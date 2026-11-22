mod harness;
mod seed;

use std::path::PathBuf;

use anyhow::Result;
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
    }
    Ok(())
}
