//! `hk`: hackriff control CLI.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "hk", version, about = "hackriff control CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Replay a SigMF recording through the pipeline. For now this prints a metadata summary.
    Replay {
        /// Path to a `.sigmf-meta` file. The `.sigmf-data` file sits beside it.
        fixture: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Replay { fixture } => print!("{}", hk_cli::replay_summary(&fixture)?),
    }
    Ok(())
}
