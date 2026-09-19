# pgtop

A live view into a running Postgres: what is executing right now and for
how long, who is stuck behind whom, and which connections are sitting
`idle in transaction` holding locks they've forgotten about. Everyone
reaches for `pg_activity` for this — which is Python, ncurses, and a pip
install on a box you may not want to pip install on. There is no small
Rust equivalent. This is one: a single static binary, a connection
string, and a snapshot every couple of seconds.

## Usage

```bash
pgtop "postgres://postgres@127.0.0.1:5432/mydb"            # redraw every 2s until Ctrl-C
pgtop "postgres://postgres@db.internal:5432/mydb" -i 5     # every 5 seconds
pgtop "host=db.internal port=5432 dbname=mydb" --once      # one snapshot and exit

pgtop "$DATABASE_URL" --min-duration 10                    # only queries older than 10s
pgtop "$DATABASE_URL" --count 30 --no-clear > incident.log # 30 snapshots, appended to a file
```

The connection string is a required argument on purpose — a tool that
reaches into a production server should never guess which one you meant
from the environment.

`--once`/`--count N` print and exit; the default watch mode clears the
screen between redraws, which `--no-clear` turns off so successive
snapshots append (that's the mode you want when piping to `tee` during an
incident). Ctrl-C leaves the last snapshot on your screen — it doesn't
take over an alternate screen buffer and wipe it on exit.

## What it shows

Three sections, from one poll of `pg_stat_activity` plus a `pg_locks`
lookup:

```
pgtop  pgtop_test@127.0.0.1:55471  PostgreSQL 18.6  server time 2026-09-19 17:44:36
5 client connections: 4 active, 0 idle, 1 idle in transaction, 2 blocked

ACTIVE QUERIES (4, longest first)
     PID  DURATION  WAIT                             USER/DB              APP           QUERY
  105738     30.0s  Timeout:PgSleep                  postgres/pgtop_test  report-job    SELECT pg_sleep(300)
  106112     25.0s  Timeout:PgSleep                  postgres/pgtop_test  nightly-etl   SELECT pg_sleep(300), count(*) FROM accounts
  107115     15.0s  transactionid ShareLock          postgres/pgtop_test  checkout-api  UPDATE accounts SET balance = balance + 5 WHERE id = 1
  108354     10.0s  tuple ExclusiveLock on accounts  postgres/pgtop_test  batch-settle  UPDATE accounts SET balance = balance + 1 WHERE id = 1

BLOCKED (2)
  pid 107115  postgres/pgtop_test (checkout-api)  waiting 15.0s on transactionid ShareLock
      query: UPDATE accounts SET balance = balance + 5 WHERE id = 1
      blocked by pid 106973  postgres/pgtop_test (invoice-worker)  idle in transaction 20.0s  <- kill this one
          last query: UPDATE accounts SET balance = balance - 10 WHERE id = 1;
  pid 108354  postgres/pgtop_test (batch-settle)  waiting 10.0s on tuple ExclusiveLock on accounts
      query: UPDATE accounts SET balance = balance + 1 WHERE id = 1
      blocked by pid 107115  postgres/pgtop_test (checkout-api)  active 15.0s
          last query: UPDATE accounts SET balance = balance + 5 WHERE id = 1
          blocked by pid 106973  postgres/pgtop_test (invoice-worker)  idle in transaction 20.0s  <- kill this one
              last query: UPDATE accounts SET balance = balance - 10 WHERE id = 1;

IDLE IN TRANSACTION (1)
     PID  IDLE FOR  XACT AGE  USER/DB              APP             LAST QUERY
  106973     20.0s     20.0s  postgres/pgtop_test  invoice-worker  UPDATE accounts SET balance = balance - 10 WHERE id = 1;
```

**ACTIVE QUERIES** is sorted by duration, longest first, with the wait
event beside it — so the query that has been running for nine minutes is
the first thing on screen, not somewhere in a scroll. Durations are
computed server-side from `now() - query_start`, so a clock skew on the
machine running `pgtop` can't invent or hide a slow query.

**BLOCKED** is the part a plain `pg_stat_activity` query won't give you.
Each stuck backend is followed up the chain to the backend at the root of
it, which is tagged `<- kill this one` — in the example above, `pid
108354` is behind `107115`, which is behind `106973`, and only `106973`
is worth a `pg_terminate_backend()`. The edges come from
`pg_blocking_pids()` (the catalog function that does the
conflicting-lock-mode analysis properly — the hand-written `pg_locks`
self-join everyone pastes from Stack Overflow happily reports
*non-conflicting* lock holders as blockers); the `on transactionid
ShareLock` / `on tuple ExclusiveLock on accounts` detail comes from a
`LATERAL` join back into `pg_locks` for the backend's ungranted lock row.
The chain walk is cycle-guarded, so a momentary mutual wait prints a
`!! wait cycle` note instead of hanging the tool.

**IDLE IN TRANSACTION** is the footgun: a connection that ran a statement,
never committed, and is now holding its locks and pinning `xmin` (blocking
vacuum) while your pooler happily counts it as busy. `IDLE FOR` is time in
that state, `XACT AGE` is how long the whole transaction has been open.
`idle in transaction (aborted)` counts here too — it holds the same locks.

`--min-duration` filters the ACTIVE section only; a blocked backend shows
up in BLOCKED no matter how young it is, because a two-second block is
still a bug.

Notes on access: `pgtop` only reads catalogs — it never writes, never
`SET`s anything, and takes no locks of its own. A non-superuser will see
its own queries but `NULL`-blanked query text for other users' backends
unless it's a member of `pg_read_all_stats` (that's Postgres's rule, not
this tool's). `pgtop` hides its own backend by default, since it would
otherwise always be the `active` query running `pgtop`'s own poll;
`--include-self` shows it.

## Status: built and verified live against a real Postgres, with real blocked sessions

- **32 unit tests** (`cargo test --lib`), all on pure logic, no database
  involved: duration formatting across the unit switches (`0.4s`,
  `59.9s`, `1m00s`, `3m04s`, `1h15m`) and the clamp that keeps a negative
  or NaN interval from printing as `-0.0s`; query-text flattening (a
  four-line ORM query collapsing to one row) and **char-safe** truncation
  (a query containing a non-ASCII character truncated to an exact width
  without panicking on a byte boundary); sorting of active queries
  longest-first with ties broken by pid so a redraw doesn't reshuffle
  rows; `--min-duration` filtering; `idle in transaction (aborted)`
  counted as idle-in-transaction; the wait-description fallback chain
  (concrete `pg_locks` entry → `wait_event_type:wait_event` → `-`); and
  the blocking-chain builder — a one-hop chain, a three-deep chain
  resolving to the right root, multiple immediate blockers all kept
  rather than only the first, **a two-node wait cycle and a self-loop
  both terminating instead of looping forever**, and a blocker that has
  vanished between the two catalog reads being reported as `(gone)`
  rather than silently dropped. The renderer is pure (snapshot in,
  `String` out), so several of those tests assert on the actual rendered
  screen, including that the query column starts at the same offset on a
  row with a 1-digit pid and a 6-digit one.
- **Live run against a throwaway local Postgres 18.6** (`initdb`/`pg_ctl`
  on port 55471, a scratch data directory, deleted afterwards — *not*
  this machine's real server). Five concurrent `psql` sessions were
  started at deliberately staggered times — T+0 `SELECT pg_sleep(300)`,
  T+5 a second sleep, T+10 a session that ran `BEGIN; UPDATE accounts SET
  balance = balance - 10 WHERE id = 1;` and then sat there, T+15 a second
  session trying to update **the same row**, T+20 a third one queueing up
  behind that. A snapshot taken at T+30.0 read back durations of
  **30.0s / 25.0s / 15.0s / 10.0s** — each matching its session's real
  start offset to the tenth of a second — with the holder shown as
  **idle in transaction, IDLE FOR 20.0s, XACT AGE 20.0s**. The block was
  reported as it genuinely was: pid 107115 *waiting 15.0s on transactionid
  ShareLock*, blocked by pid 106973 (`invoice-worker`, idle in
  transaction 20.0s); and pid 108354 *waiting 10.0s on tuple
  ExclusiveLock on accounts*, blocked by 107115, itself blocked by
  106973 — a genuine three-deep chain, with only 106973 tagged
  `<- kill this one`. A second snapshot 20s later showed every duration
  advanced by exactly that (50.1s / 45.1s / 35.0s / 30.0s, holder idle
  40.0s). Then **pgtop's advice was acted on**: running
  `pg_terminate_backend(106973)` on the pid it fingered cleared the jam —
  three seconds later `BLOCKED (0)`, `IDLE IN TRANSACTION (0)`, and the
  row read `balance = 106` (100, plus the two queued updates that finally
  went through, with the killed transaction's `-10` rolled back), which
  is only possible if the backend `pgtop` named really was the one
  holding the lock.
- **Watch mode verified live**: `--count 3 --interval 2 --no-clear` on a
  running `pg_sleep(60)` produced three frames whose server timestamps
  were 17:49:50 / 17:49:52 / 17:49:54 and whose duration column read
  **20.0s → 22.0s → 24.0s** — real polling, not a cached first read.
  `--min-duration 10` on a server with a 20.0s query and a 5s query
  showed `2 client connections` in the header and exactly one row in
  ACTIVE.

**Not done / deliberately deferred**:
- **A real TUI.** This is a polling redraw, not `ratatui` — no keyboard
  interaction, no sorting by another column at runtime, no
  kill-this-backend key. `pg_activity` has all of that; this doesn't.
- **TLS.** The connection is `NoTls`, so a server that requires SSL
  (most managed Postgres — RDS, Supabase, Neon) can't be reached yet.
  That's the first thing to fix for it to be useful beyond a VPC or an
  SSH tunnel.
- **Per-backend CPU/memory/IO** (`pg_activity` reads those from `/proc`
  for local backends), and anything from `pg_stat_statements` — this is
  strictly a "what is happening right now" view, not a workload profile.
- **Older Postgres versions.** Verified only against 18.6.
  `pg_blocking_pids()` needs 9.6+ and the `backend_type` filter needs
  10+, so 9.6–17 should work, but that is reasoning, not a test result.
  Nothing here was run against a replica, either.

## Local testing without Docker

```bash
initdb -D /tmp/pgtop-pgdata -U postgres --auth=trust
pg_ctl -D /tmp/pgtop-pgdata -o "-h 127.0.0.1 -p 55471" -l /tmp/pgtop.log start
psql -h 127.0.0.1 -p 55471 -U postgres -c "CREATE DATABASE pgtop_test;"

# session 1: something slow
psql -h 127.0.0.1 -p 55471 -U postgres -d pgtop_test -c "SELECT pg_sleep(120)" &
# session 2: hold a row lock open, then try to touch it from a third session
psql -h 127.0.0.1 -p 55471 -U postgres -d pgtop_test   # BEGIN; UPDATE ...; (leave it)

cargo run -- "postgres://postgres@127.0.0.1:55471/pgtop_test"
pg_ctl -D /tmp/pgtop-pgdata stop && rm -rf /tmp/pgtop-pgdata
```

Pick a port that is definitely not your real server's.
