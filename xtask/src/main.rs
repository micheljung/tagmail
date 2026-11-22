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
    }
    Ok(())
}
