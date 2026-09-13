//! `hackriffd`: the hackriff daemon. It owns the source, the pipeline and the stores, drives the
//! attention scheduler, and serves the control API and streams (T-027; see
//! `hk_cli::pipeline`).

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
#[command(name = "hackriffd", version, about = "hackriff daemon")]
struct Args {
    /// Sample source: `sigmf:<file.sigmf-meta>` (replayed in real time). A live HackRF source is
    /// not implemented yet.
    #[arg(long)]
    source: String,
    /// Replay the recording again when it ends; the stream continues.
    #[arg(long = "loop")]
    loop_replay: bool,
    /// Data directory (database, history tiles, recordings).
    #[arg(long)]
    data_dir: PathBuf,
    /// ScanPlan JSON (default: one region over the source window).
    #[arg(long)]
    plan: Option<PathBuf>,
    /// API listen address. 0.0.0.0 exposes the API to the whole network (token only, no TLS).
    #[arg(long, default_value = "127.0.0.1:8787")]
    bind: SocketAddr,
    /// Built UI directory (default: ui/dist when present).
    #[arg(long)]
    ui_dist: Option<PathBuf>,
    /// Replay unpaced and lossless instead of in real time.
    #[arg(long)]
    unpaced: bool,
    /// Offline feed cache directory for anomaly correlation.
    #[arg(long)]
    feeds: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let a = Args::parse();
    let ui_dist = a.ui_dist.or_else(|| {
        [
            PathBuf::from("ui/dist"),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/dist"),
        ]
        .into_iter()
        .find(|p| p.is_dir())
    });
    hk_cli::pipeline::run_daemon(&hk_cli::pipeline::DaemonArgs {
        source: a.source,
        loop_replay: a.loop_replay,
        data_dir: a.data_dir,
        plan: a.plan,
        bind: a.bind,
        ui_dist,
        unpaced: a.unpaced,
        feeds: a.feeds,
        token: None,
    })
}
