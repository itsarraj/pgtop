//! The only part that touches a server. One connection, two read-only
//! catalog queries per poll, no temp tables, no `SET`, nothing written.

use anyhow::{Context, Result};
use postgres::{Client, NoTls, Row};

use crate::model::{Backend, ServerInfo, Snapshot};

/// Everything the dashboard needs about one backend.
///
/// The lock columns come from a `LATERAL` join back into `pg_locks` for the
/// backend's *ungranted* lock row — a backend waits on at most one lock at a
/// time, so that row (when it exists) is exactly "what this pid is stuck
/// on". `pg_blocking_pids()` supplies who holds it: it is the catalog
/// function that already does the conflicting-lock-mode analysis correctly,
/// which a hand-rolled `pg_locks` self-join notoriously gets subtly wrong
/// (it happily reports non-conflicting lock holders as "blockers").
const ACTIVITY_SQL: &str = "
SELECT
    a.pid,
    COALESCE(a.usename::text, '')                                     AS usename,
    COALESCE(a.datname::text, '')                                     AS datname,
    COALESCE(a.application_name, '')                                  AS application_name,
    COALESCE(host(a.client_addr), '')                                 AS client,
    COALESCE(a.state, '')                                             AS state,
    COALESCE(a.wait_event_type, '')                                   AS wait_event_type,
    COALESCE(a.wait_event, '')                                        AS wait_event,
    COALESCE(EXTRACT(EPOCH FROM (now() - a.query_start)), 0)::float8  AS query_secs,
    COALESCE(EXTRACT(EPOCH FROM (now() - a.xact_start)), 0)::float8   AS xact_secs,
    COALESCE(EXTRACT(EPOCH FROM (now() - a.state_change)), 0)::float8 AS state_secs,
    COALESCE(a.query, '')                                             AS query,
    pg_blocking_pids(a.pid)                                           AS blocked_by,
    COALESCE(l.locktype, '')                                          AS lock_type,
    COALESCE(l.mode, '')                                              AS lock_mode,
    COALESCE(l.relname, '')                                           AS lock_relation
FROM pg_stat_activity a
LEFT JOIN LATERAL (
    SELECT
        w.locktype,
        w.mode,
        CASE
            WHEN w.relation IS NOT NULL
             AND w.database IN (0, (SELECT oid FROM pg_database WHERE datname = current_database()))
            THEN w.relation::regclass::text
        END AS relname
    FROM pg_locks w
    WHERE w.pid = a.pid AND NOT w.granted
    ORDER BY w.locktype
    LIMIT 1
) l ON true
WHERE a.backend_type = 'client backend'
";

const SERVER_SQL: &str = "
SELECT
    current_database()::text,
    COALESCE(host(inet_server_addr()), 'local')::text,
    COALESCE(inet_server_port(), 0)::int4,
    ('PostgreSQL ' || current_setting('server_version'))::text,
    to_char(now(), 'YYYY-MM-DD HH24:MI:SS')::text
";

pub fn connect(conninfo: &str) -> Result<Client> {
    Client::connect(conninfo, NoTls).context("could not connect to Postgres")
}

fn row_to_backend(row: &Row) -> Backend {
    Backend {
        pid: row.get("pid"),
        user: row.get("usename"),
        database: row.get("datname"),
        application_name: row.get("application_name"),
        client: row.get("client"),
        state: row.get("state"),
        wait_event_type: row.get("wait_event_type"),
        wait_event: row.get("wait_event"),
        query_secs: row.get("query_secs"),
        xact_secs: row.get("xact_secs"),
        state_secs: row.get("state_secs"),
        query: row.get("query"),
        blocked_by: row.get("blocked_by"),
        lock_type: row.get("lock_type"),
        lock_mode: row.get("lock_mode"),
        lock_relation: row.get("lock_relation"),
    }
}

/// One poll: server identity + every client backend.
///
/// `include_self` keeps `pgtop`'s own connection in the list. It is hidden
/// by default because it is always `active` running this very query, so it
/// would otherwise sit in the ACTIVE table on every single redraw.
pub fn snapshot(client: &mut Client, include_self: bool) -> Result<Snapshot> {
    let row = client
        .query_one(SERVER_SQL, &[])
        .context("failed to read server identity")?;
    let server = ServerInfo {
        database: row.get(0),
        host: row.get(1),
        port: row.get(2),
        version: row.get(3),
        now: row.get(4),
    };

    let sql = if include_self {
        ACTIVITY_SQL.to_string()
    } else {
        format!("{ACTIVITY_SQL}  AND a.pid <> pg_backend_pid()")
    };
    let backends = client
        .query(&sql, &[])
        .context("failed to read pg_stat_activity (needs pg_read_all_stats or superuser to see other users' queries)")?
        .iter()
        .map(row_to_backend)
        .collect();

    Ok(Snapshot { server, backends })
}
