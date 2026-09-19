//! The plain-data shapes `pgtop` renders. Nothing in here talks to a
//! database — `db.rs` fills these in from a real server, and every test in
//! this crate builds them by hand, so the formatting/sorting/blocking-chain
//! logic is exercised without needing Postgres running.

/// One client backend as `pg_stat_activity` (plus a `pg_locks` lookup for
/// the lock it is waiting on, if any) reports it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Backend {
    pub pid: i32,
    pub user: String,
    pub database: String,
    pub application_name: String,
    pub client: String,
    /// Raw `pg_stat_activity.state`: `active`, `idle`,
    /// `idle in transaction`, `idle in transaction (aborted)`, ...
    pub state: String,
    pub wait_event_type: String,
    pub wait_event: String,
    /// Seconds since `query_start` (0 when there is no current query).
    pub query_secs: f64,
    /// Seconds since `xact_start` (0 when not in a transaction).
    pub xact_secs: f64,
    /// Seconds since `state_change` — how long it has sat in `state`.
    pub state_secs: f64,
    pub query: String,
    /// `pg_blocking_pids(pid)` — the backends this one is waiting behind.
    pub blocked_by: Vec<i32>,
    /// From the backend's ungranted `pg_locks` row, when it has one.
    pub lock_type: String,
    pub lock_mode: String,
    pub lock_relation: String,
}

impl Backend {
    pub fn is_active(&self) -> bool {
        self.state == "active"
    }

    /// True for both `idle in transaction` and
    /// `idle in transaction (aborted)` — the aborted flavour holds the same
    /// locks and pins `xmin` exactly the same way, so it is the same footgun.
    pub fn is_idle_in_transaction(&self) -> bool {
        self.state.starts_with("idle in transaction")
    }

    pub fn is_blocked(&self) -> bool {
        !self.blocked_by.is_empty()
    }

    /// `user/db`, the identity column used everywhere in the output.
    pub fn who(&self) -> String {
        format!("{}/{}", self.user, self.database)
    }

    /// Which client this is: the `application_name` the app set if it set
    /// one (the useful answer — `checkout-api`), otherwise the client IP,
    /// otherwise `-`. Worth a column of its own: `user/db` is usually the
    /// same for every connection in a pool, so it can't tell you *which*
    /// service is holding the lock.
    pub fn origin(&self) -> String {
        if !self.application_name.is_empty() {
            self.application_name.clone()
        } else if !self.client.is_empty() {
            self.client.clone()
        } else {
            "-".to_string()
        }
    }

    /// `user/db (app)` — the one-line identity used in the BLOCKED section,
    /// where there is no table to hang columns off.
    pub fn who_full(&self) -> String {
        let origin = self.origin();
        if origin == "-" {
            self.who()
        } else {
            format!("{} ({})", self.who(), origin)
        }
    }

    /// What the backend is waiting on, as one short cell. Prefers the
    /// concrete lock from `pg_locks` (`transactionid ShareLock`) and falls
    /// back to the coarser `wait_event_type:wait_event` from
    /// `pg_stat_activity` (`IO:DataFileRead`), or `-` when it is running.
    pub fn wait_cell(&self) -> String {
        if !self.lock_type.is_empty() {
            let mut s = self.lock_type.clone();
            if !self.lock_mode.is_empty() {
                s.push(' ');
                s.push_str(&self.lock_mode);
            }
            if !self.lock_relation.is_empty() {
                s.push_str(&format!(" on {}", self.lock_relation));
            }
            return s;
        }
        match (self.wait_event_type.as_str(), self.wait_event.as_str()) {
            ("", "") => "-".to_string(),
            (t, "") => t.to_string(),
            ("", e) => e.to_string(),
            (t, e) => format!("{t}:{e}"),
        }
    }
}

/// Identifying detail about the server, shown in the header so a snapshot
/// pasted into a ticket says which server it came from.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ServerInfo {
    pub database: String,
    pub host: String,
    pub port: i32,
    pub version: String,
    /// Server-side `now()`, preformatted — durations below are relative to
    /// this, not to the clock on whatever machine is running `pgtop`.
    pub now: String,
}

/// One poll of the server.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Snapshot {
    pub server: ServerInfo,
    pub backends: Vec<Backend>,
}

/// The one-line connection census under the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    pub total: usize,
    pub active: usize,
    pub idle: usize,
    pub idle_in_transaction: usize,
    pub blocked: usize,
}

impl Snapshot {
    pub fn counts(&self) -> Counts {
        let mut c = Counts {
            total: self.backends.len(),
            ..Default::default()
        };
        for b in &self.backends {
            if b.is_active() {
                c.active += 1;
            }
            if b.state == "idle" {
                c.idle += 1;
            }
            if b.is_idle_in_transaction() {
                c.idle_in_transaction += 1;
            }
            if b.is_blocked() {
                c.blocked += 1;
            }
        }
        c
    }

    /// Active queries, longest-running first — the top-of-screen list.
    /// `min_secs` drops the short ones (`--min-duration`), which on a busy
    /// server is the difference between a readable screen and a firehose.
    pub fn active_by_duration(&self, min_secs: f64) -> Vec<&Backend> {
        let mut rows: Vec<&Backend> = self
            .backends
            .iter()
            .filter(|b| b.is_active() && b.query_secs >= min_secs)
            .collect();
        // Longest first; ties broken by pid so repeated redraws of an idle-ish
        // server don't shuffle rows around.
        rows.sort_by(|a, b| {
            b.query_secs
                .partial_cmp(&a.query_secs)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.pid.cmp(&b.pid))
        });
        rows
    }

    /// Idle-in-transaction connections, longest-idle first.
    pub fn idle_in_transaction(&self) -> Vec<&Backend> {
        let mut rows: Vec<&Backend> = self
            .backends
            .iter()
            .filter(|b| b.is_idle_in_transaction())
            .collect();
        rows.sort_by(|a, b| {
            b.state_secs
                .partial_cmp(&a.state_secs)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.pid.cmp(&b.pid))
        });
        rows
    }

    pub fn by_pid(&self, pid: i32) -> Option<&Backend> {
        self.backends.iter().find(|b| b.pid == pid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(pid: i32, state: &str, query_secs: f64) -> Backend {
        Backend {
            pid,
            user: "app".into(),
            database: "shop".into(),
            state: state.into(),
            query_secs,
            state_secs: query_secs,
            query: "select 1".into(),
            ..Default::default()
        }
    }

    #[test]
    fn active_queries_are_sorted_longest_first() {
        let snap = Snapshot {
            backends: vec![
                backend(1, "active", 0.5),
                backend(2, "active", 91.2),
                backend(3, "idle", 0.0),
                backend(4, "active", 12.0),
            ],
            ..Default::default()
        };
        let pids: Vec<i32> = snap.active_by_duration(0.0).iter().map(|b| b.pid).collect();
        assert_eq!(pids, vec![2, 4, 1], "idle backends must not appear at all");
    }

    #[test]
    fn min_duration_filters_the_short_ones() {
        let snap = Snapshot {
            backends: vec![
                backend(1, "active", 0.5),
                backend(2, "active", 91.2),
                backend(4, "active", 12.0),
            ],
            ..Default::default()
        };
        let pids: Vec<i32> = snap.active_by_duration(5.0).iter().map(|b| b.pid).collect();
        assert_eq!(pids, vec![2, 4]);
    }

    #[test]
    fn equal_durations_break_ties_by_pid_so_redraws_dont_shuffle() {
        let snap = Snapshot {
            backends: vec![
                backend(9, "active", 3.0),
                backend(2, "active", 3.0),
                backend(5, "active", 3.0),
            ],
            ..Default::default()
        };
        let pids: Vec<i32> = snap.active_by_duration(0.0).iter().map(|b| b.pid).collect();
        assert_eq!(pids, vec![2, 5, 9]);
    }

    #[test]
    fn aborted_idle_in_transaction_still_counts() {
        let mut aborted = backend(7, "idle in transaction (aborted)", 0.0);
        aborted.state_secs = 30.0;
        let snap = Snapshot {
            backends: vec![backend(6, "idle in transaction", 5.0), aborted],
            ..Default::default()
        };
        let pids: Vec<i32> = snap.idle_in_transaction().iter().map(|b| b.pid).collect();
        assert_eq!(pids, vec![7, 6], "longest-idle first, aborted included");
        assert_eq!(snap.counts().idle_in_transaction, 2);
    }

    #[test]
    fn counts_census_adds_up() {
        let mut blocked = backend(4, "active", 2.0);
        blocked.blocked_by = vec![6];
        let snap = Snapshot {
            backends: vec![
                backend(1, "active", 1.0),
                backend(2, "idle", 0.0),
                backend(3, "idle in transaction", 0.0),
                blocked,
            ],
            ..Default::default()
        };
        let c = snap.counts();
        assert_eq!(c.total, 4);
        assert_eq!(c.active, 2);
        assert_eq!(c.idle, 1);
        assert_eq!(c.idle_in_transaction, 1);
        assert_eq!(c.blocked, 1);
    }

    #[test]
    fn origin_prefers_application_name_then_client_ip_then_dash() {
        let mut b = backend(1, "active", 1.0);
        assert_eq!(b.origin(), "-");
        assert_eq!(b.who_full(), "app/shop", "no app name, no parens");

        b.client = "10.0.0.7".into();
        assert_eq!(b.origin(), "10.0.0.7");

        b.application_name = "checkout-api".into();
        assert_eq!(b.origin(), "checkout-api");
        assert_eq!(b.who_full(), "app/shop (checkout-api)");
    }

    #[test]
    fn wait_cell_prefers_the_concrete_lock_then_falls_back() {
        let mut b = backend(1, "active", 1.0);
        assert_eq!(b.wait_cell(), "-");

        b.wait_event_type = "IO".into();
        b.wait_event = "DataFileRead".into();
        assert_eq!(b.wait_cell(), "IO:DataFileRead");

        b.lock_type = "transactionid".into();
        b.lock_mode = "ShareLock".into();
        assert_eq!(b.wait_cell(), "transactionid ShareLock");

        b.lock_type = "tuple".into();
        b.lock_relation = "accounts".into();
        assert_eq!(b.wait_cell(), "tuple ShareLock on accounts");
    }
}
