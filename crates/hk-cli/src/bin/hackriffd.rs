//! `hackriffd`: the hackriff daemon. It owns the source, the pipeline and the stores, drives the
//! attention scheduler, and serves the control API and streams (T-027, T-037a; see
//! `hk_cli::pipeline`). Ctrl-C (or SIGTERM) stops it gracefully.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use hk_cli::pipeline::LiveArgs;

#[derive(Parser)]
#[command(name = "hackriffd", version, about = "hackriff daemon")]
struct Args {
    /// Sample source: `hackrf` (the live HackRF One, receive only; needs `--features hackrf`),
    /// `hackrf:<serial>`, `mock:<file.sigmf-meta>` (the mock SDR device: the recording as live air,
    /// retuned by the scheduler), or `sigmf:<file.sigmf-meta>` (replayed in real time; the
    /// scheduler's retunes go through the mock device too, so frequencies stay truthful).
    #[arg(long, default_value = "hackrf")]
    source: String,
    #[command(flatten)]
    live: LiveArgs,
    /// Replay the recording again when it ends; the stream continues (recordings only).
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
    /// Replay unpaced and lossless instead of in real time (recordings only).
    #[arg(long)]
    unpaced: bool,
    /// Offline feed cache directory for anomaly correlation.
    #[arg(long)]
    feeds: Option<PathBuf>,
    /// T-021 CalibrationState JSON (a file or a directory) for calibrated floors.
    #[arg(long)]
    calibration: Option<PathBuf>,
    #[command(flatten)]
    listen: hk_cli::pipeline::ListenArgs,
    #[command(flatten)]
    compute: hk_cli::pipeline::ComputeArgs,
    #[command(flatten)]
    iq_buffer: hk_cli::pipeline::IqBufferArgs,
}

fn main() -> anyhow::Result<()> {
    let a = Args::parse();
    hk_cli::signal::install()?;
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
        live: a.live,
        loop_replay: a.loop_replay,
        data_dir: a.data_dir,
        plan: a.plan,
        bind: a.bind,
        ui_dist,
        unpaced: a.unpaced,
        feeds: a.feeds,
        token: None,
        calibration: a.calibration,
        listen: a.listen,
        compute: a.compute,
        iq_buffer: a.iq_buffer,
    })
}
