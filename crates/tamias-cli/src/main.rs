//! tamias — the CLI. P2a skeleton (identity + a `version`/`manifest` shell);
//! the verb surface lands in P2b.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "tamias",
    version,
    about = "Package & content manager — fetch, store, cache, dispense"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print the tamias version.
    Version,
    /// Print the machine-readable capability manifest (stub).
    Manifest,
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Some(Command::Manifest) => {
            println!(
                "{{\"name\":\"tamias\",\"version\":\"{}\"}}",
                env!("CARGO_PKG_VERSION")
            );
            ExitCode::SUCCESS
        }
        Some(Command::Version) | None => {
            println!("tamias {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
    }
}
