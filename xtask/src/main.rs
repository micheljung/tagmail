use clap::Parser;

/// Development tasks for the Tagmail workspace.
#[derive(Parser)]
#[command(name = "cargo xtask", version)]
struct Cli {}

fn main() {
    Cli::parse();
}
