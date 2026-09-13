//! `hk`: hackriff control CLI.

use std::io::{self, Write as _};
use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{ArgGroup, Parser, Subcommand};
use hk_api::stream::StreamReader;
use hk_cli::pipeline::LiveArgs;

#[derive(Parser)]
#[command(name = "hk", version, about = "hackriff control CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a SigMF recording once through the whole pipeline (detections, tracks, runtime
    /// demod/decode chains, history, streams) and print the run summary.
    Replay {
        /// Path to a `.sigmf-meta` file. The `.sigmf-data` file sits beside it.
        fixture: PathBuf,
        /// Data directory (database, history tiles, recordings). Default: a fresh temp directory.
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// ScanPlan JSON (settings under `extra.pipeline` / `extra.scheduler`).
        #[arg(long)]
        plan: Option<PathBuf>,
        /// Serve the API (streams, history, floor, status) during the run and afterwards.
        #[arg(long)]
        serve: Option<SocketAddr>,
        /// Replay in real time (lossy, like a live source) instead of unpaced and lossless.
        #[arg(long)]
        paced: bool,
        /// Drive the attention scheduler over the replay (virtual tuning).
        #[arg(long)]
        schedule: bool,
        /// Offline feed cache directory for anomaly correlation.
        #[arg(long)]
        feeds: Option<PathBuf>,
        /// T-021 CalibrationState JSON (a file or a directory) for calibrated floors.
        #[arg(long)]
        calibration: Option<PathBuf>,
        /// Only print the recording's metadata summary.
        #[arg(long)]
        info: bool,
        /// Print the summary as JSON.
        #[arg(long)]
        json: bool,
        /// Built UI directory (with --serve; default: ui/dist).
        #[arg(long)]
        ui_dist: Option<PathBuf>,
    },
    /// Run the whole pipeline over the live HackRF One (receive only) until --duration or Ctrl-C,
    /// then print the run summary. Needs a build with `--features hackrf`.
    Run {
        /// Live source: `hackrf` (first device), `hackrf:<serial>`, or `mock:<file.sigmf-meta>`
        /// (the mock SDR device replaying a recording as live air).
        #[arg(long, default_value = "hackrf")]
        source: String,
        #[command(flatten)]
        live: LiveArgs,
        /// Stop after this many seconds (default: until Ctrl-C).
        #[arg(long)]
        duration: Option<f64>,
        /// Data directory. Default: a fresh temp directory.
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// ScanPlan JSON.
        #[arg(long)]
        plan: Option<PathBuf>,
        /// Serve the API during the run and afterwards.
        #[arg(long)]
        serve: Option<SocketAddr>,
        /// Drive the attention scheduler (it retunes the radio over the plan).
        #[arg(long)]
        schedule: bool,
        /// Offline feed cache directory for anomaly correlation.
        #[arg(long)]
        feeds: Option<PathBuf>,
        /// T-021 CalibrationState JSON (a file or a directory).
        #[arg(long)]
        calibration: Option<PathBuf>,
        /// Print the summary as JSON.
        #[arg(long)]
        json: bool,
        /// Built UI directory (with --serve; default: ui/dist).
        #[arg(long)]
        ui_dist: Option<PathBuf>,
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
    /// Serve the web UI over the whole pipeline: the live HackRF One by default (receive only;
    /// needs `--features hackrf`), or `--replay` a recording. The inventory, history and status
    /// show only what this run detected. Prints the URL with the API token (HK_TOKEN fixes it).
    Serve {
        /// Live HackRF One (the default source); `--hackrf SERIAL` picks a device.
        #[arg(long, num_args = 0..=1, default_missing_value = "", conflicts_with = "replay")]
        hackrf: Option<String>,
        /// Device spec instead of `--hackrf`: `hackrf`, `hackrf:<serial>`, or
        /// `mock:<file.sigmf-meta>` (the mock SDR: the recording as live air, in real time and
        /// looping, retunable through the live controls). Tuning and gains not given explicitly
        /// take the recording's own.
        #[arg(long, conflicts_with_all = ["replay", "hackrf"])]
        device: Option<String>,
        #[command(flatten)]
        live: LiveArgs,
        /// Replay this `.sigmf-meta` recording in real time instead of the live radio.
        #[arg(long)]
        replay: Option<PathBuf>,
        /// Replay again when the recording ends (the stream continues).
        #[arg(long = "loop", requires = "replay")]
        loop_replay: bool,
        /// Data directory (database, history, recordings). Default: a fresh temp directory.
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// T-021 CalibrationState JSON (a file or a directory).
        #[arg(long)]
        calibration: Option<PathBuf>,
        /// Listen address. 0.0.0.0 exposes the API to the whole network (token only, no TLS).
        #[arg(long, default_value = "127.0.0.1:8787")]
        bind: SocketAddr,
        /// Built UI directory (default: ui/dist).
        #[arg(long)]
        ui_dist: Option<PathBuf>,
        /// Spectrum FFT length (bins per row).
        #[arg(long, default_value_t = 4096)]
        fft: usize,
        /// Target spectrum rows per second.
        #[arg(long, default_value_t = 25.0)]
        rows_per_s: f64,
    },
}

fn default_ui_dist(ui_dist: Option<PathBuf>) -> Option<PathBuf> {
    ui_dist.or_else(|| {
        [
            PathBuf::from("ui/dist"),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/dist"),
        ]
        .into_iter()
        .find(|p| p.is_dir())
    })
}

fn print_summary(summary: &hk_pipeline::RunSummary, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(summary)?);
    } else {
        print!("{}", summary.to_text());
    }
    if !summary.errors.is_empty() {
        anyhow::bail!("the run reported {} error(s)", summary.errors.len());
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Replay {
            fixture,
            data_dir,
            plan,
            serve,
            paced,
            schedule,
            feeds,
            calibration,
            info,
            json,
            ui_dist,
        } => {
            if info {
                print!("{}", hk_cli::replay_summary(&fixture)?);
                return Ok(());
            }
            hk_cli::signal::install()?;
            let summary = hk_cli::pipeline::run_replay(&hk_cli::pipeline::ReplayArgs {
                fixture,
                data_dir,
                plan,
                serve,
                paced,
                schedule,
                feeds,
                ui_dist: default_ui_dist(ui_dist),
                calibration,
            })?;
            print_summary(&summary, json)?;
        }
        Command::Run {
            source,
            live,
            duration,
            data_dir,
            plan,
            serve,
            schedule,
            feeds,
            calibration,
            json,
            ui_dist,
        } => {
            hk_cli::signal::install()?;
            let summary = hk_cli::pipeline::run_live(&hk_cli::pipeline::RunArgs {
                source,
                live,
                duration_s: duration,
                data_dir,
                plan,
                serve,
                schedule,
                feeds,
                ui_dist: default_ui_dist(ui_dist),
                calibration,
            })?;
            print_summary(&summary, json)?;
        }
        Command::Serve {
            hackrf,
            device,
            live,
            replay,
            loop_replay,
            data_dir,
            calibration,
            bind,
            ui_dist,
            fft,
            rows_per_s,
        } => {
            hk_cli::signal::install()?;
            let source = match (replay, device) {
                (Some(path), _) => hk_cli::serve::ServeSource::Replay {
                    path,
                    loop_replay,
                    realtime: true,
                },
                (None, Some(spec)) => hk_cli::serve::ServeSource::HackRf { spec, live },
                (None, None) => hk_cli::serve::ServeSource::HackRf {
                    spec: match hackrf.as_deref() {
                        None | Some("") => "hackrf".into(),
                        Some(serial) => format!("hackrf:{serial}"),
                    },
                    live,
                },
            };
            hk_cli::serve::run(hk_cli::serve::ServeOptions {
                source,
                data_dir,
                bind,
                ui_dist: default_ui_dist(ui_dist),
                fft_len: fft,
                rows_per_s,
                calibration,
                token: None,
            })?;
        }
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
