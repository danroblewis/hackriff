//! `hk`: hackriff control CLI.

use std::io::{self, Write as _};
use std::path::PathBuf;

use clap::{ArgGroup, Parser, Subcommand};
use hk_api::stream::StreamReader;

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
    /// Connect to a stream-output endpoint and print its header and records
    /// (docs/stream-contract.md).
    #[command(group(ArgGroup::new("endpoint").required(true).args(["uds", "tcp"])))]
    StreamTail {
        /// Unix-domain socket path.
        #[arg(long)]
        uds: Option<PathBuf>,
        /// TCP address, e.g. 127.0.0.1:7355.
        #[arg(long)]
        tcp: Option<String>,
        /// Stop after this many records.
        #[arg(long)]
        count: Option<u64>,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Replay { fixture } => print!("{}", hk_cli::replay_summary(&fixture)?),
        Command::StreamTail { uds, tcp, count } => {
            let stdout = io::stdout();
            let mut out = io::BufWriter::new(stdout.lock());
            let result = match (uds, tcp) {
                (Some(path), _) => {
                    hk_cli::stream_tail(&mut StreamReader::connect_uds(path)?, &mut out, count)
                }
                (None, Some(addr)) => {
                    hk_cli::stream_tail(&mut StreamReader::connect_tcp(addr)?, &mut out, count)
                }
                (None, None) => unreachable!("clap requires one endpoint"),
            };
            let flushed = out.flush();
            // A closed pipe (`hk stream-tail ... | head`) is a normal way to stop.
            let broken_pipe = |e: &anyhow::Error| {
                e.chain().any(|c| {
                    c.downcast_ref::<io::Error>()
                        .is_some_and(|io| io.kind() == io::ErrorKind::BrokenPipe)
                })
            };
            match result {
                Err(e) if !broken_pipe(&e) => return Err(e),
                _ => {}
            }
            if let Err(e) = flushed
                && e.kind() != io::ErrorKind::BrokenPipe
            {
                return Err(e.into());
            }
        }
    }
    Ok(())
}
