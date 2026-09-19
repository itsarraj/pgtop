//! Snapshot -> the text you see. Kept entirely pure (a `Snapshot` in, a
//! `String` out) so the whole screen, not just its pieces, is unit-testable
//! without a server anywhere near it.

use crate::blocking;
use crate::format::{duration, query_cell};
use crate::model::Snapshot;

#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Characters of query text per row before `...`.
    pub query_width: usize,
    /// Hide active queries faster than this many seconds.
    pub min_duration: f64,
    /// Cap on rows in the ACTIVE section (0 = no cap).
    pub limit: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            query_width: 90,
            min_duration: 0.0,
            limit: 20,
        }
    }
}

/// Render an aligned table. Columns are sized to their widest cell, and the
/// final column is never padded, so a long query can run to the edge of the
/// terminal without dragging trailing spaces behind it.
fn table(headers: &[&str], rows: &[Vec<String>], right: &[bool]) -> String {
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(cols) {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }

    let mut out = String::new();
    let render_row = |out: &mut String, cells: &[String]| {
        out.push_str("  ");
        for (i, cell) in cells.iter().enumerate().take(cols) {
            let pad = widths[i].saturating_sub(cell.chars().count());
            if i == cols - 1 {
                out.push_str(cell);
            } else if right.get(i).copied().unwrap_or(false) {
                out.push_str(&" ".repeat(pad));
                out.push_str(cell);
                out.push_str("  ");
            } else {
                out.push_str(cell);
                out.push_str(&" ".repeat(pad));
                out.push_str("  ");
            }
        }
        out.push('\n');
    };

    let header_cells: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    render_row(&mut out, &header_cells);
    for row in rows {
        render_row(&mut out, row);
    }
    out
}

fn header(snapshot: &Snapshot) -> String {
    let s = &snapshot.server;
    let c = snapshot.counts();
    let mut out = format!(
        "pgtop  {}@{}:{}  {}  server time {}\n",
        s.database, s.host, s.port, s.version, s.now
    );
    out.push_str(&format!(
        "{} client connections: {} active, {} idle, {} idle in transaction, {} blocked\n",
        c.total, c.active, c.idle, c.idle_in_transaction, c.blocked
    ));
    out
}

fn active_section(snapshot: &Snapshot, opts: &Options) -> String {
    let all = snapshot.active_by_duration(opts.min_duration);
    let shown: Vec<_> = if opts.limit > 0 && all.len() > opts.limit {
        all.iter().take(opts.limit).collect()
    } else {
        all.iter().collect()
    };

    let mut out = format!("\nACTIVE QUERIES ({}, longest first)\n", all.len());
    if all.is_empty() {
        out.push_str("  (none)\n");
        return out;
    }
    let rows: Vec<Vec<String>> = shown
        .iter()
        .map(|b| {
            vec![
                b.pid.to_string(),
                duration(b.query_secs),
                b.wait_cell(),
                b.who(),
                b.origin(),
                query_cell(&b.query, opts.query_width),
            ]
        })
        .collect();
    out.push_str(&table(
        &["PID", "DURATION", "WAIT", "USER/DB", "APP", "QUERY"],
        &rows,
        &[true, true, false, false, false, false],
    ));
    if shown.len() < all.len() {
        out.push_str(&format!(
            "  ... {} more (raise --limit)\n",
            all.len() - shown.len()
        ));
    }
    out
}

fn blocked_section(snapshot: &Snapshot, opts: &Options) -> String {
    let waits = blocking::waits(snapshot);
    let mut out = format!("\nBLOCKED ({})\n", waits.len());
    if waits.is_empty() {
        out.push_str("  (none)\n");
        return out;
    }

    for w in &waits {
        let Some(b) = snapshot.by_pid(w.pid) else {
            continue;
        };
        out.push_str(&format!(
            "  pid {}  {}  waiting {} on {}\n",
            b.pid,
            b.who_full(),
            duration(b.query_secs),
            b.wait_cell()
        ));
        out.push_str(&format!(
            "      query: {}\n",
            query_cell(&b.query, opts.query_width)
        ));

        if w.blockers.len() > 1 {
            let others: Vec<String> = w.blockers[1..].iter().map(|p| p.to_string()).collect();
            out.push_str(&format!(
                "      (also behind pid {})\n",
                others.join(", pid ")
            ));
        }

        let mut indent = String::from("      ");
        for (i, blocker_pid) in w.chain.iter().enumerate() {
            match snapshot.by_pid(*blocker_pid) {
                Some(bl) => {
                    let mark = if !w.cycle && i == w.chain.len() - 1 {
                        "  <- kill this one"
                    } else {
                        ""
                    };
                    out.push_str(&format!(
                        "{}blocked by pid {}  {}  {} {}{}\n",
                        indent,
                        bl.pid,
                        bl.who_full(),
                        bl.state,
                        duration(bl.state_secs),
                        mark
                    ));
                    out.push_str(&format!(
                        "{}    last query: {}\n",
                        indent,
                        query_cell(&bl.query, opts.query_width)
                    ));
                }
                None => {
                    // Blocker vanished (or is a backend we filtered out)
                    // between the two reads — say so rather than dropping it.
                    out.push_str(&format!("{indent}blocked by pid {blocker_pid}  (gone)\n"));
                }
            }
            indent.push_str("    ");
        }
        if w.cycle {
            out.push_str("      !! wait cycle — these backends are blocking each other\n");
        }
    }
    out
}

fn idle_in_transaction_section(snapshot: &Snapshot, opts: &Options) -> String {
    let rows_src = snapshot.idle_in_transaction();
    let mut out = format!("\nIDLE IN TRANSACTION ({})\n", rows_src.len());
    if rows_src.is_empty() {
        out.push_str("  (none)\n");
        return out;
    }
    let rows: Vec<Vec<String>> = rows_src
        .iter()
        .map(|b| {
            vec![
                b.pid.to_string(),
                duration(b.state_secs),
                duration(b.xact_secs),
                b.who(),
                b.origin(),
                query_cell(&b.query, opts.query_width),
            ]
        })
        .collect();
    out.push_str(&table(
        &[
            "PID",
            "IDLE FOR",
            "XACT AGE",
            "USER/DB",
            "APP",
            "LAST QUERY",
        ],
        &rows,
        &[true, true, true, false, false, false],
    ));
    out
}

/// The whole screen.
pub fn snapshot(snapshot: &Snapshot, opts: &Options) -> String {
    let mut out = header(snapshot);
    out.push_str(&active_section(snapshot, opts));
    out.push_str(&blocked_section(snapshot, opts));
    out.push_str(&idle_in_transaction_section(snapshot, opts));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Backend, ServerInfo};

    fn server() -> ServerInfo {
        ServerInfo {
            database: "shop".into(),
            host: "local".into(),
            port: 55471,
            version: "PostgreSQL 18.6".into(),
            now: "2026-09-19 17:33:02".into(),
        }
    }

    fn b(pid: i32, state: &str, query: &str) -> Backend {
        Backend {
            pid,
            user: "app".into(),
            database: "shop".into(),
            state: state.into(),
            query: query.into(),
            ..Default::default()
        }
    }

    #[test]
    fn an_idle_server_renders_all_three_sections_as_empty() {
        let snap = Snapshot {
            server: server(),
            backends: vec![b(1, "idle", "select 1")],
        };
        let out = snapshot(&snap, &Options::default());
        assert!(out.contains("ACTIVE QUERIES (0, longest first)"));
        assert!(out.contains("BLOCKED (0)"));
        assert!(out.contains("IDLE IN TRANSACTION (0)"));
        assert_eq!(out.matches("(none)").count(), 3);
        assert!(out.contains("shop@local:55471"));
        assert!(out.contains("1 client connections: 0 active, 1 idle"));
    }

    #[test]
    fn active_rows_come_out_longest_first_with_formatted_durations() {
        let mut fast = b(11, "active", "select 1");
        fast.query_secs = 0.4;
        let mut slow = b(12, "active", "select pg_sleep(600)");
        slow.query_secs = 184.6;
        let snap = Snapshot {
            server: server(),
            backends: vec![fast, slow],
        };
        let out = snapshot(&snap, &Options::default());
        let body: Vec<&str> = out.lines().collect();
        let i_slow = body.iter().position(|l| l.contains("pg_sleep")).unwrap();
        let i_fast = body.iter().position(|l| l.contains("select 1")).unwrap();
        assert!(i_slow < i_fast, "longest-running query must be on top");
        assert!(out.contains("3m04s"));
        assert!(out.contains("0.4s"));
    }

    #[test]
    fn limit_caps_rows_and_says_how_many_were_hidden() {
        let backends: Vec<Backend> = (1..=5)
            .map(|i| {
                let mut x = b(i, "active", "select 1");
                x.query_secs = i as f64;
                x
            })
            .collect();
        let snap = Snapshot {
            server: server(),
            backends,
        };
        let out = snapshot(
            &snap,
            &Options {
                limit: 2,
                ..Default::default()
            },
        );
        assert!(out.contains("ACTIVE QUERIES (5, longest first)"));
        assert!(out.contains("... 3 more (raise --limit)"));
    }

    #[test]
    fn a_blocking_pair_names_the_blocker_and_its_open_transaction() {
        let mut blocked = b(41, "active", "UPDATE accounts SET balance = 1 WHERE id = 1");
        blocked.query_secs = 4.1;
        blocked.blocked_by = vec![37];
        blocked.lock_type = "transactionid".into();
        blocked.lock_mode = "ShareLock".into();

        let mut holder = b(
            37,
            "idle in transaction",
            "UPDATE accounts SET balance = 2 WHERE id = 1",
        );
        holder.state_secs = 9.0;
        holder.xact_secs = 12.0;

        let snap = Snapshot {
            server: server(),
            backends: vec![blocked, holder],
        };
        let out = snapshot(&snap, &Options::default());
        assert!(out.contains("BLOCKED (1)"));
        assert!(out.contains("pid 41  app/shop  waiting 4.1s on transactionid ShareLock"));
        assert!(out.contains("blocked by pid 37  app/shop  idle in transaction 9.0s"));
        assert!(out.contains("<- kill this one"));
        assert!(out.contains("IDLE IN TRANSACTION (1)"));
    }

    #[test]
    fn the_app_name_identifies_which_service_is_holding_the_lock() {
        let mut blocked = b(41, "active", "UPDATE accounts SET balance = 1");
        blocked.query_secs = 4.1;
        blocked.blocked_by = vec![37];
        blocked.application_name = "checkout-api".into();
        let mut holder = b(37, "idle in transaction", "UPDATE accounts SET balance = 2");
        holder.application_name = "nightly-etl".into();
        let snap = Snapshot {
            server: server(),
            backends: vec![blocked, holder],
        };
        let out = snapshot(&snap, &Options::default());
        assert!(out.contains("pid 41  app/shop (checkout-api)  waiting 4.1s"));
        assert!(out.contains("blocked by pid 37  app/shop (nightly-etl)"));
        assert!(out.contains("APP"), "ACTIVE table gains an APP column");
    }

    #[test]
    fn a_three_deep_chain_is_indented_down_to_the_root() {
        let mut a = b(41, "active", "update t set x = 1");
        a.blocked_by = vec![39];
        a.query_secs = 2.0;
        let mut mid = b(39, "active", "update t set x = 2");
        mid.blocked_by = vec![37];
        mid.query_secs = 9.5;
        let mut root = b(37, "idle in transaction", "update t set x = 3");
        root.state_secs = 30.0;

        let snap = Snapshot {
            server: server(),
            backends: vec![a, mid, root],
        };
        let out = snapshot(&snap, &Options::default());
        assert!(out.contains("BLOCKED (2)"));
        // The 41 -> 39 -> 37 walk must print both hops, root marked once.
        let for_41 = out.split("pid 41  app/shop  waiting").nth(1).unwrap();
        assert!(for_41.contains("blocked by pid 39"));
        assert!(for_41.contains("blocked by pid 37"));
        assert_eq!(out.matches("<- kill this one").count(), 2);
    }

    #[test]
    fn a_wait_cycle_is_called_out_rather_than_pointing_at_a_root() {
        let mut x = b(41, "active", "update t set x = 1");
        x.blocked_by = vec![39];
        let mut y = b(39, "active", "update t set x = 2");
        y.blocked_by = vec![41];
        let snap = Snapshot {
            server: server(),
            backends: vec![x, y],
        };
        let out = snapshot(&snap, &Options::default());
        assert!(out.contains("wait cycle"));
        assert!(!out.contains("<- kill this one"));
    }

    #[test]
    fn a_blocker_that_vanished_is_reported_not_silently_dropped() {
        let mut blocked = b(41, "active", "update t set x = 1");
        blocked.blocked_by = vec![999];
        let snap = Snapshot {
            server: server(),
            backends: vec![blocked],
        };
        let out = snapshot(&snap, &Options::default());
        assert!(out.contains("blocked by pid 999  (gone)"));
    }

    #[test]
    fn long_queries_are_truncated_to_the_requested_width() {
        let mut long = b(11, "active", &format!("select {}", "x".repeat(200)));
        long.query_secs = 1.0;
        let snap = Snapshot {
            server: server(),
            backends: vec![long],
        };
        let out = snapshot(
            &snap,
            &Options {
                query_width: 30,
                ..Default::default()
            },
        );
        assert!(out.contains("..."));
        for line in out.lines() {
            assert!(line.chars().count() < 120, "row too wide: {line}");
        }
    }

    #[test]
    fn columns_line_up_when_pids_have_different_widths() {
        let mut short = b(7, "active", "select 1");
        short.query_secs = 1.0;
        let mut wide = b(123456, "active", "select 2");
        wide.query_secs = 2.0;
        let snap = Snapshot {
            server: server(),
            backends: vec![short, wide],
        };
        let out = snapshot(&snap, &Options::default());
        let rows: Vec<&str> = out
            .lines()
            .filter(|l| l.contains("select 1") || l.contains("select 2"))
            .collect();
        assert_eq!(rows.len(), 2);
        let col = |l: &str| l.find("select").unwrap();
        assert_eq!(
            col(rows[0]),
            col(rows[1]),
            "query column must start at the same offset on both rows"
        );
    }
}
