//! Turning `pg_blocking_pids()` edges into the thing you actually want to
//! read: "pid 41 is stuck behind pid 39, which is stuck behind pid 37,
//! which is idle in transaction — go kill 37."
//!
//! A flat list of `blocked_by` arrays does not say that. Walking them does,
//! and walking them naively hangs the moment Postgres reports a cycle
//! (mutual waits do briefly exist before the deadlock detector fires, and a
//! monitoring tool that spins forever on one is worse than useless), so the
//! walk is explicitly cycle-guarded and that guard is tested.

use std::collections::{HashMap, HashSet};

use crate::model::{Backend, Snapshot};

/// pid -> the pids it is waiting behind, as `pg_blocking_pids()` reports them.
pub type Edges = HashMap<i32, Vec<i32>>;

pub fn edges(backends: &[Backend]) -> Edges {
    backends
        .iter()
        .filter(|b| b.is_blocked())
        .map(|b| (b.pid, b.blocked_by.clone()))
        .collect()
}

/// One blocked backend and everything standing in front of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wait {
    /// The backend that is stuck.
    pub pid: i32,
    /// Its *immediate* blockers — `pg_blocking_pids()` can name more than
    /// one (several holders of a shared lock, say), and hiding the extras
    /// would point you at the wrong victim.
    pub blockers: Vec<i32>,
    /// The transitive chain from the first immediate blocker to the root,
    /// e.g. `[39, 37]` meaning "blocked by 39, which is blocked by 37".
    /// The last entry is the one worth killing.
    pub chain: Vec<i32>,
    /// Set when the walk came back around to a pid already on the chain —
    /// a genuine wait cycle. The chain is truncated at that point.
    pub cycle: bool,
}

impl Wait {
    /// The pid at the head of the chain: the backend that is blocking
    /// everyone and is not itself blocked. `None` only for a cycle, where
    /// by definition there is no root.
    pub fn root(&self) -> Option<i32> {
        if self.cycle {
            return None;
        }
        self.chain.last().copied()
    }
}

/// Follow the first blocker of `pid` up to whoever is not blocked.
pub fn chain(pid: i32, edges: &Edges) -> (Vec<i32>, bool) {
    let mut out = Vec::new();
    let mut seen: HashSet<i32> = HashSet::from([pid]);
    let mut cur = pid;
    loop {
        let Some(next) = edges.get(&cur).and_then(|v| v.first().copied()) else {
            return (out, false);
        };
        if !seen.insert(next) {
            // Already on this chain — a cycle. Stop here rather than looping.
            return (out, true);
        }
        out.push(next);
        cur = next;
    }
}

/// Every blocked backend in the snapshot, longest-waiting first.
///
/// A blocked backend's `query_start` is when the statement that is stuck
/// began, so its query duration *is* its wait time — which makes the
/// ordering here "who has been suffering longest".
pub fn waits(snapshot: &Snapshot) -> Vec<Wait> {
    let edges = edges(&snapshot.backends);
    let mut blocked: Vec<&Backend> = snapshot
        .backends
        .iter()
        .filter(|b| b.is_blocked())
        .collect();
    blocked.sort_by(|a, b| {
        b.query_secs
            .partial_cmp(&a.query_secs)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.pid.cmp(&b.pid))
    });
    blocked
        .into_iter()
        .map(|b| {
            let (chain, cycle) = chain(b.pid, &edges);
            Wait {
                pid: b.pid,
                blockers: b.blocked_by.clone(),
                chain,
                cycle,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(pid: i32, blocked_by: Vec<i32>, query_secs: f64) -> Backend {
        Backend {
            pid,
            user: "app".into(),
            database: "shop".into(),
            state: "active".into(),
            query_secs,
            blocked_by,
            query: "update accounts set balance = 0".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_single_hop_chain_names_the_holder() {
        let e: Edges = HashMap::from([(41, vec![37])]);
        assert_eq!(chain(41, &e), (vec![37], false));
    }

    #[test]
    fn a_three_deep_chain_walks_to_the_root() {
        let e: Edges = HashMap::from([(41, vec![39]), (39, vec![37])]);
        let (path, cycle) = chain(41, &e);
        assert_eq!(path, vec![39, 37]);
        assert!(!cycle);
        let w = Wait {
            pid: 41,
            blockers: vec![39],
            chain: path,
            cycle,
        };
        assert_eq!(w.root(), Some(37), "the root is the one to kill");
    }

    #[test]
    fn a_wait_cycle_terminates_instead_of_looping_forever() {
        // 41 waits on 39, 39 waits on 41 — briefly real before the deadlock
        // detector fires. A naive walk never returns from this.
        let e: Edges = HashMap::from([(41, vec![39]), (39, vec![41])]);
        let (path, cycle) = chain(41, &e);
        assert!(cycle);
        assert_eq!(path, vec![39]);
        let w = Wait {
            pid: 41,
            blockers: vec![39],
            chain: path,
            cycle,
        };
        assert_eq!(w.root(), None, "a cycle has no root to point at");
    }

    #[test]
    fn a_self_loop_does_not_hang_either() {
        let e: Edges = HashMap::from([(41, vec![41])]);
        assert_eq!(chain(41, &e), (vec![], true));
    }

    #[test]
    fn an_unblocked_pid_has_an_empty_chain() {
        let e: Edges = HashMap::from([(41, vec![37])]);
        assert_eq!(chain(37, &e), (vec![], false));
    }

    #[test]
    fn multiple_immediate_blockers_are_all_kept() {
        let snap = Snapshot {
            backends: vec![
                backend(41, vec![37, 38], 3.0),
                backend(37, vec![], 100.0),
                backend(38, vec![], 100.0),
            ],
            ..Default::default()
        };
        let w = waits(&snap);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].blockers, vec![37, 38]);
        assert_eq!(w[0].chain, vec![37]);
    }

    #[test]
    fn waits_are_ordered_longest_stuck_first_and_skip_the_unblocked() {
        let snap = Snapshot {
            backends: vec![
                backend(41, vec![39], 2.0),
                backend(39, vec![37], 9.5),
                backend(37, vec![], 30.0),
                backend(50, vec![], 4.0),
            ],
            ..Default::default()
        };
        let w = waits(&snap);
        assert_eq!(
            w.iter().map(|x| x.pid).collect::<Vec<_>>(),
            vec![39, 41],
            "only blocked backends, longest wait first"
        );
        assert_eq!(w[1].chain, vec![39, 37]);
        assert_eq!(w[1].root(), Some(37));
    }

    #[test]
    fn edges_only_contains_blocked_backends() {
        let e = edges(&[backend(41, vec![37], 1.0), backend(37, vec![], 1.0)]);
        assert_eq!(e.len(), 1);
        assert!(e.contains_key(&41));
    }
}
