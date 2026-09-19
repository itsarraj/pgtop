use std::io::{self, Write};
use std::thread::sleep;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use pgtop::{db, render};

#[derive(Parser)]
#[command(
    name = "pgtop",
    version,
    about = "Live view of a running Postgres: active queries by duration, who is blocking whom, and idle-in-transaction connections"
)]
struct Cli {
    /// Connection string — `postgres://user@host:5432/dbname` or libpq
    /// keyword form (`host=... port=... dbname=...`). Required on purpose:
    /// a monitoring tool should never guess which server you meant.
    conninfo: String,

    /// Seconds between redraws.
    #[arg(short, long, default_value_t = 2.0)]
    interval: f64,

    /// Take this many snapshots and exit (default: run until interrupted).
    #[arg(short = 'n', long)]
    count: Option<u64>,

    /// One snapshot, then exit. Same as `--count 1`.
    #[arg(long)]
    once: bool,

    /// Hide active queries younger than this many seconds.
    #[arg(long, default_value_t = 0.0)]
    min_duration: f64,

    /// Characters of query text shown per row.
    #[arg(long, default_value_t = 90)]
    query_width: usize,

    /// Max rows in the ACTIVE section (0 = all).
    #[arg(long, default_value_t = 20)]
    limit: usize,

    /// Include pgtop's own backend, which is always running pgtop's query.
    #[arg(long)]
    include_self: bool,

    /// Don't clear the screen between redraws — use when piping to a file
    /// or `tee`, so successive snapshots append instead of overwriting.
    #[arg(long)]
    no_clear: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let opts = render::Options {
        query_width: cli.query_width,
        min_duration: cli.min_duration,
        limit: cli.limit,
    };
    let total = if cli.once { Some(1) } else { cli.count };
    let clear = !cli.no_clear && total != Some(1);
    let interval = Duration::from_secs_f64(cli.interval.max(0.1));

    let mut client = db::connect(&cli.conninfo)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();

    let mut taken = 0u64;
    loop {
        match db::snapshot(&mut client, cli.include_self) {
            Ok(snap) => {
                let text = render::snapshot(&snap, &opts);
                if clear {
                    // Clear + home. Cheaper and less surprising than taking
                    // over the terminal with an alternate screen buffer:
                    // Ctrl-C leaves the last snapshot on screen.
                    write!(out, "\x1b[2J\x1b[H")?;
                }
                write!(out, "{text}")?;
                out.flush()?;
            }
            Err(e) => {
                // A monitoring tool that dies when the server restarts is
                // the wrong shape: report, then try to pick it back up.
                eprintln!("pgtop: {e:#}");
                if total.is_none() || taken + 1 < total.unwrap_or(0) {
                    match db::connect(&cli.conninfo) {
                        Ok(c) => {
                            client = c;
                            eprintln!("pgtop: reconnected");
                        }
                        Err(e) => eprintln!("pgtop: reconnect failed: {e:#}"),
                    }
                } else {
                    return Err(e);
                }
            }
        }

        taken += 1;
        if let Some(t) = total {
            if taken >= t {
                break;
            }
        }
        sleep(interval);
    }

    Ok(())
}
