mod bench_rss;
mod bench_search;
mod harness;
mod seed;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

/// Development tasks for the Tagmail workspace.
#[derive(Parser)]
#[command(name = "cargo xtask", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a deterministic Maildir++ fixture
    Seed {
        #[arg(long, default_value_t = 1_000)]
        count: u64,
        /// Average message size in KiB
        #[arg(long, default_value_t = 8)]
        avg_kb: u64,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Output directory; must not exist or be empty
        #[arg(long)]
        out: PathBuf,
    },
    /// Manage the Dovecot test servers (test/dovecot)
    Dovecot {
        #[command(subcommand)]
        action: DovecotAction,
    },
    /// Manage the Stalwart test server (test/stalwart)
    Stalwart {
        #[command(subcommand)]
        action: ServerAction,
    },
    /// Idle RSS gate (AC 9): sync the seeded `user1` with tagmail-server,
    /// idle, and fail if VmRSS over the last window exceeds the limit
    BenchRss {
        /// Prebuilt tagmail-server; built in release mode when absent
        #[arg(long)]
        server: Option<PathBuf>,
        #[arg(long, default_value = "127.0.0.1:10143")]
        imap: SocketAddr,
        #[arg(long, default_value = "user1")]
        user: String,
        #[arg(long, default_value = "pass1")]
        password: String,
        /// Idle period after the sync
        #[arg(long, default_value_t = 300)]
        idle_secs: u64,
        /// Gated tail of the idle period
        #[arg(long, default_value_t = 60)]
        window_secs: u64,
        /// Gate in decimal megabytes (64 MB = 61.04 MiB)
        #[arg(long, default_value_t = 64)]
        limit_mb: u64,
        /// Give up if the sync and text pass take longer
        #[arg(long, default_value_t = 3600)]
        sync_timeout_secs: u64,
        /// JSON result
        #[arg(long, default_value = "target/bench-rss.json")]
        out: PathBuf,
        /// Run the server inside the Dovecot container's network namespace
        /// (for Docker daemons whose published ports are unreachable)
        #[arg(long)]
        in_dovecot_network: bool,
    },
    /// Search gate (AC 11): sync the seeded accounts with tagmail-server,
    /// run the named queries through the API and fail if the warm p95
    /// latency reaches the limit
    BenchSearch {
        /// Prebuilt tagmail-server; built in release mode when absent
        #[arg(long)]
        server: Option<PathBuf>,
        #[arg(long, default_value = "127.0.0.1:10143")]
        imap: SocketAddr,
        /// `user:password`, once per account
        #[arg(long = "account", default_values = ["user1:pass1", "user2:pass2"])]
        accounts: Vec<String>,
        /// Warm runs of every query after the cold run
        #[arg(long, default_value_t = 5)]
        warm_runs: usize,
        #[arg(long, default_value_t = 500)]
        limit_ms: u64,
        /// Give up if the sync and text pass take longer
        #[arg(long, default_value_t = 3600)]
        sync_timeout_secs: u64,
        /// JSON result
        #[arg(long, default_value = "target/bench-search.json")]
        out: PathBuf,
        /// Run the server inside the Dovecot container's network namespace
        #[arg(long)]
        in_dovecot_network: bool,
    },
    /// Regenerate the TypeScript API types in packages/api-types
    GenTsTypes {
        /// Fail if the committed bindings are stale instead of writing
        #[arg(long)]
        check: bool,
    },
}

#[derive(Subcommand)]
enum DovecotAction {
    /// Start Dovecot and the Mailpit SMTP sink
    Up {
        /// Also start the Gmail-mode variant with a virtual \All mailbox
        #[arg(long)]
        gmail: bool,
    },
    /// Stop Dovecot and delete its mail
    Down,
    /// Copy a seeded Maildir into a test user's mailbox
    Load {
        #[arg(long)]
        from: PathBuf,
        #[arg(long, default_value = "user1")]
        user: String,
        /// Load into the Gmail-mode variant instead
        #[arg(long)]
        gmail: bool,
    },
}

#[derive(Subcommand)]
enum ServerAction {
    /// Start the server and the Mailpit SMTP sink
    Up,
    /// Stop the server and delete its mail
    Down,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Seed {
            count,
            avg_kb,
            seed,
            out,
        } => {
            let started = std::time::Instant::now();
            let summary = seed::generate(&seed::Options {
                count,
                avg_kb,
                seed,
                out: out.clone(),
            })?;
            println!(
                "seeded {} messages, {:.1} MiB, into {} in {:.1?}",
                summary.count,
                summary.bytes as f64 / (1024.0 * 1024.0),
                out.display(),
                started.elapsed()
            );
        }
        Command::Dovecot { action } => match action {
            DovecotAction::Up { gmail } => {
                let mut services = vec![harness::DOVECOT];
                if gmail {
                    services.push(harness::DOVECOT_GMAIL);
                }
                harness::up(&services)?;
            }
            DovecotAction::Down => harness::down(&[harness::DOVECOT, harness::DOVECOT_GMAIL])?,
            DovecotAction::Load { from, user, gmail } => {
                let service = if gmail {
                    harness::DOVECOT_GMAIL
                } else {
                    harness::DOVECOT
                };
                harness::load_maildir(service, &from, &user)?;
            }
        },
        Command::Stalwart { action } => match action {
            ServerAction::Up => harness::up(&[harness::STALWART])?,
            ServerAction::Down => harness::down(&[harness::STALWART])?,
        },
        Command::BenchRss {
            server,
            imap,
            user,
            password,
            idle_secs,
            window_secs,
            limit_mb,
            sync_timeout_secs,
            out,
            in_dovecot_network,
        } => {
            if window_secs == 0 || window_secs > idle_secs {
                bail!("--window-secs must be between 1 and --idle-secs");
            }
            let opts = bench_rss::Options {
                server,
                imap,
                user,
                password,
                idle: Duration::from_secs(idle_secs),
                window: Duration::from_secs(window_secs),
                limit_mb,
                sync_timeout: Duration::from_secs(sync_timeout_secs),
                out,
            };
            if in_dovecot_network {
                bench_rss::run_in_dovecot_network(&opts)?;
            } else {
                bench_rss::run_local(&opts)?;
            }
        }
        Command::BenchSearch {
            server,
            imap,
            accounts,
            warm_runs,
            limit_ms,
            sync_timeout_secs,
            out,
            in_dovecot_network,
        } => {
            let accounts = accounts
                .iter()
                .map(|a| match a.split_once(':') {
                    Some((user, password)) => Ok((user.to_owned(), password.to_owned())),
                    None => bail!("--account {a}: expected user:password"),
                })
                .collect::<Result<Vec<_>>>()?;
            if warm_runs == 0 {
                bail!("--warm-runs must be at least 1");
            }
            let opts = bench_search::Options {
                server,
                imap,
                accounts,
                warm_runs,
                limit_ms,
                sync_timeout: Duration::from_secs(sync_timeout_secs),
                out,
            };
            if in_dovecot_network {
                bench_search::run_in_dovecot_network(&opts)?;
            } else {
                bench_search::run_local(&opts)?;
            }
        }
        Command::GenTsTypes { check } => {
            let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join(tagmail_api_core::ts::OUT_DIR);
            if check {
                let drifted = tagmail_api_core::ts::drift(&dir)?;
                if !drifted.is_empty() {
                    bail!("stale TypeScript bindings {drifted:?}: run `cargo xtask gen-ts-types`");
                }
            } else {
                tagmail_api_core::ts::write(&dir)?;
                println!("wrote {}", dir.display());
            }
        }
    }
    Ok(())
}
