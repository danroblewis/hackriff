//! `hk serve`: the web UI over the **whole pipeline** (T-042). The source is the live HackRF One
//! (the default, receive only) or an explicit `--replay` recording; spectrum, detection, tracking,
//! inventory and history all come from that run ([`crate::pipeline`]).
//!
//! ```text
//! HackRF One (or --replay) ─► hk-pipeline (detect → track → inventory, history, spectrum)
//!                                   └─► hk-api: /ws/spectrum/live, /api/inventory, /api/history,
//!                                               /api/floor, /api/status, /api/control/*,
//!                                               /api/bookmarks (T-050)
//! ```
//!
//! - **No demo data.** `/api/inventory` reads the run's own database only; a fresh data directory
//!   starts empty. There is no seed and no external inventory database.
//! - **Spectrum stream** `spectrum/live`, kind `spectrum`, `rf32_le` PSD rows in dBFS/Hz over
//!   `center_hz ± bandwidth_hz/2` (`hk_pipeline::spectrum`); a retune re-offers the stream with
//!   the new header.
//! - **Class** ([`fixture_class`] for recordings; band-derived from the window for the live
//!   radio): a class that forbids content gates spectrum at ≤ 50 rows/s. A live retune or rate
//!   change into a window of another class re-plumbs the run with that class (T-050); a replayed
//!   recording refuses device settings (409 `not_live`) and accepts display settings.
//! - **Token** (T-050): `HK_TOKEN`, else the 0600 token file (`hk_api::default_token_path`),
//!   printed as `http://<addr>/#token=…` at start.
//! - Ctrl-C stops the run gracefully ([`crate::signal`]).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use hk_api::stream::StreamHeader;
use hk_api::{LiveControl, Server, StreamRegistry};
use hk_core::{Pacing, Source, SourceControl};
use hk_model::ContentClass;
use hk_model::sigmf::SigmfMeta;
use hk_pipeline::{PipelineHandle, SourceFactory, TrackInventory, open_replay};

use crate::pipeline::{
    LiveArgs, LiveOptions, config_for, load_plan, serve_api, start_live, temp_data_dir, token,
};
use crate::signal;

/// Stream id of the spectrum.
pub const STREAM_ID: &str = "spectrum/live";
pub use hk_pipeline::class::{FM_BROADCAST_HZ, RowPlan, SPECTRUM_DATATYPE, row_plan};

/// Where `hk serve` gets samples.
#[derive(Clone, Debug)]
pub enum ServeSource {
    /// A device: the live HackRF One (`hackrf` or `hackrf:<serial>`) or the mock SDR
    /// (`mock:<file.sigmf-meta>`, `hk serve --device`), driven through the same live path.
    HackRf {
        /// Source spec.
        spec: String,
        /// Tuning and gains.
        live: LiveArgs,
    },
    /// A SigMF recording.
    Replay {
        /// `.sigmf-meta` path.
        path: PathBuf,
        /// Replay again at the end (the stream continues).
        loop_replay: bool,
        /// Real-time pacing (tests may replay unpaced, which is then lossless).
        realtime: bool,
    },
}

/// `hk serve` settings.
#[derive(Clone, Debug)]
pub struct ServeOptions {
    /// Source.
    pub source: ServeSource,
    /// Data directory (default: a fresh temp directory).
    pub data_dir: Option<PathBuf>,
    /// Listen address.
    pub bind: SocketAddr,
    /// Built UI directory.
    pub ui_dist: Option<PathBuf>,
    /// Spectrum FFT length (bins per row).
    pub fft_len: usize,
    /// Target spectrum row rate, rows/s.
    pub rows_per_s: f64,
    /// T-021 calibration JSON (file or directory).
    pub calibration: Option<PathBuf>,
    /// API token (default: `HK_TOKEN`, else generated).
    pub token: Option<String>,
    /// Listen limits (T-066).
    pub listen: crate::pipeline::ListenArgs,
    /// Compute provider (T-056).
    pub compute: crate::pipeline::ComputeArgs,
    /// Rolling IQ capture buffer retention and cap (T-157).
    pub iq_buffer: crate::pipeline::IqBufferArgs,
    /// Test-only override of the ring's allocation (T-217); `None` uses the real allocator.
    pub iq_buffer_hooks: Option<crate::pipeline::IqBufferHooksOverride>,
}

/// The stream class for a recording (`hk_pipeline::class`).
pub fn fixture_class(meta: &SigmfMeta) -> ContentClass {
    hk_pipeline::class::source_class(meta)
}

/// A spectrum stream header for this producer.
pub fn spectrum_header(
    class: ContentClass,
    plan: &RowPlan,
    center_hz: f64,
    fs: f64,
) -> StreamHeader {
    hk_pipeline::class::spectrum_header(STREAM_ID, "hk-cli:serve", class, plan, center_hz, fs)
}

/// A running `hk serve`.
pub struct Serving {
    /// The API server.
    pub server: Server,
    /// The pipeline.
    pub handle: PipelineHandle,
    /// Live control (live source only).
    pub live_control: Option<Arc<dyn LiveControl>>,
    /// The live source's control handle (stats, identity); `None` for recordings.
    pub source_control: Option<Arc<dyn SourceControl>>,
    /// The run's data directory.
    pub data_dir: PathBuf,
    /// The streams this server offers (T-531: withdrawn on shutdown, see [`run`]).
    pub streams: StreamRegistry,
}

/// Starts the pipeline over the source and the API server.
pub fn start(opts: &ServeOptions) -> anyhow::Result<Serving> {
    let data_dir = opts.data_dir.clone().unwrap_or_else(temp_data_dir);
    let registry = StreamRegistry::new();
    let token = token(opts.token.as_deref())?;
    opts.listen.validate()?;
    match &opts.source {
        ServeSource::HackRf { spec, live } => {
            let lp = start_live(
                &LiveOptions {
                    source: spec.clone(),
                    live: live.clone(),
                    data_dir: data_dir.clone(),
                    plan: None,
                    // `hk serve` is interactive tuning: it never drives the scheduler, so a
                    // `--survey-dwell` plan would have nothing to step it (T-406).
                    //
                    // T-452 KEPT THIS, DELIBERATELY, and put the in-app survey sweep somewhere
                    // else. The scheduler is chosen when a segment is composed, so turning it on
                    // here would be a second composition path — and the interactive run already
                    // writes what a sweep needs: `hk_pipeline`'s interactive observer closes one
                    // dwell record per steady tune, with that tune's own window and interval,
                    // which is exactly T-406's "one record per step with its true band and
                    // interval". So the sweep is a driver over the interactive retune path
                    // (`hk_api::scan`), issuing the same gated `DeviceAction::Retune` a user's
                    // explicit tune issues, and its coverage lands in the same plane with no
                    // second accumulator. `hk run`/`hackriffd` keep `--survey-dwell`.
                    survey_dwell_s: None,
                    schedule: false,
                    feeds: None,
                    calibration: opts.calibration.clone(),
                    spectrum_fft_len: Some(opts.fft_len),
                    spectrum_rows_per_s: Some(opts.rows_per_s),
                    compute: opts.compute.clone(),
                    iq_buffer: opts.iq_buffer.clone(),
                    iq_buffer_hooks: opts.iq_buffer_hooks.clone(),
                },
                &registry,
            )?;
            opts.listen.apply(&lp.handle);
            let server = serve_api(
                opts.bind,
                opts.ui_dist.clone(),
                &registry,
                &lp.handle,
                token,
                "hk serve",
                lp.live_control.clone(),
            )?;
            Ok(Serving {
                server,
                handle: lp.handle,
                live_control: lp.live_control,
                source_control: Some(lp.control),
                data_dir,
                streams: registry,
            })
        }
        ServeSource::Replay {
            path,
            loop_replay,
            realtime,
        } => {
            let pacing = if *realtime {
                Pacing::RealTime { speed: 1.0 }
            } else {
                Pacing::Unpaced
            };
            let replay = open_replay(path, pacing, false)?;
            let plan = load_plan(None, &replay.info)?;
            let mut cfg = config_for(
                data_dir.clone(),
                plan,
                &registry,
                None,
                opts.calibration.as_deref(),
            )?;
            cfg.settings.spectrum_fft_len = opts.fft_len;
            cfg.settings.spectrum_rows_per_s = opts.rows_per_s;
            opts.compute.apply(&mut cfg.settings);
            opts.iq_buffer
                .apply(&mut cfg.iq_buffer, Some(replay.info.sample_rate_hz));
            if let Some(h) = &opts.iq_buffer_hooks {
                cfg.iq_buffer_hooks = Some(Arc::clone(&h.0));
            }
            cfg.source_class = replay.class;
            cfg.lossless = !*realtime;
            if let Some(hw) = &replay.meta.global.hw {
                cfg.device_id = format!("sigmf:{hw}");
            }
            cfg.device_hw = replay.meta.global.hw.clone();
            let reopen: Option<SourceFactory> = loop_replay.then(|| {
                let p = path.clone();
                Box::new(move || -> anyhow::Result<Box<dyn Source>> {
                    Ok(Box::new(open_replay(&p, pacing, false)?.source))
                }) as SourceFactory
            });
            let handle = hk_pipeline::Pipeline::start(
                cfg,
                Box::new(replay.source),
                replay.info,
                reopen,
                Box::new(TrackInventory::default()),
            )?;
            opts.listen.apply(&handle);
            let server = serve_api(
                opts.bind,
                opts.ui_dist.clone(),
                &registry,
                &handle,
                token,
                "hk serve",
                None,
            )?;
            Ok(Serving {
                server,
                handle,
                live_control: None,
                source_control: None,
                data_dir,
                streams: registry,
            })
        }
    }
}

/// Runs the server until Ctrl-C (it keeps serving after a recording ends).
pub fn run(opts: ServeOptions) -> anyhow::Result<()> {
    let Serving {
        server,
        handle,
        source_control,
        streams,
        ..
    } = start(&opts)?;
    // T-531: withdraw the offered streams as soon as a shutdown signal arrives, on a thread of its
    // own because `handle.wait()` below is what the drain happens inside.
    //
    // Without it, a stop leaves every bridged connection waiting on nothing: the run stops, every
    // publisher finishes, and each connection settles into the `CARRY_OVER_GRACE` wait for the
    // next offer (T-417/T-425) — a wait that is right between windows and wrong for ever when the
    // producer is finished for good. Withdrawing says "this stream is over", which
    // `bridge::watch_peer` already answers by ending the connection at once.
    //
    // **Reasoning, not a measurement.** T-525's 78-second SIGTERM was on a live 40-minute run with
    // the demo UI open, and 60 s of grace plus a drain is close enough to 78 s to be worth saying;
    // but it did not reproduce on the mock (0.86–1.91 s there, before and after alike), so this is
    // a wait that should not exist being removed, not a demonstrated cure. What actually bounds
    // shutdown is `signal::SHUTDOWN_BOUND`.
    {
        let streams = streams.clone();
        let _ = std::thread::Builder::new()
            .name("hk-serve-withdraw".into())
            .spawn(move || {
                signal::wait_for_signal();
                streams.clear();
            });
    }
    let watch = signal::stop_on_signal(handle.stopper());
    let summary = handle.wait()?;
    drop(watch);
    print!("{}", summary.to_text());
    if let Some(stats) = source_control.and_then(|c| c.stats()) {
        eprintln!("source: {stats:?}");
    }
    if !signal::requested() {
        eprintln!("hk serve: source finished; still serving the API and history (Ctrl-C to stop)");
        signal::wait_for_signal();
    }
    drop(server);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::path::Path;
    use std::time::Duration;

    use super::*;
    use crate::pipeline::TempDataDirGuard;
    use hk_api::stream::{Publisher, PublisherConfig};
    use hk_core::{HackRfDriver, SourceDriver};

    const TOKEN: &str = "t042-serve-empty-inventory-token-0123";

    #[test]
    fn spectrum_plan_respects_the_row_rate_and_the_publisher_accepts_it() {
        for (class, rows) in [
            (ContentClass::MetadataOnly, 200.0),
            (ContentClass::Unrestricted, 60.0),
            (ContentClass::RestrictedPaging, 25.0),
        ] {
            let plan = row_plan(2.4e6, 4096, rows, class, hk_dsp::WindowKind::default());
            assert!(plan.row_rate_hz <= rows + 1e-9);
            let header = spectrum_header(class, &plan, 100e6, 2.4e6);
            Publisher::new(header, PublisherConfig::default()).expect("contract accepts header");
        }
    }

    fn get(addr: SocketAddr, path: &str) -> (u16, String) {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        write!(
            s,
            "GET {path} HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut raw = String::new();
        s.read_to_string(&mut raw).unwrap();
        let status = raw[9..12].parse().unwrap();
        let body = raw.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_owned();
        (status, body)
    }

    /// A recording with no samples (no source input) at 100.8 MHz.
    fn empty_recording(dir: &Path) -> PathBuf {
        use hk_model::sigmf::{Capture, Datatype};
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("empty.sigmf-data"), []).unwrap();
        let mut meta = SigmfMeta::new(Datatype::Ci8);
        meta.global.sample_rate = Some(2.4e6);
        meta.captures.push(Capture {
            sample_start: 0,
            frequency: Some(100.8e6),
            datetime: Some("2026-09-13T12:00:00Z".into()),
            provenance: None,
            clip_count: None,
            extra: serde_json::Map::new(),
        });
        let path = dir.join("empty.sigmf-meta");
        meta.write(&path).unwrap();
        path
    }

    /// T-042: `hk serve` shows only what its pipeline detected. A freshly started server with no
    /// source input answers an empty inventory (no demo seed anywhere in serving).
    #[test]
    fn a_fresh_server_with_no_source_input_has_an_empty_inventory() {
        let dir = temp_data_dir();
        let _guard = TempDataDirGuard::new(dir.clone());
        let path = empty_recording(&dir.join("src"));
        let Serving {
            server,
            handle,
            live_control,
            data_dir,
            ..
        } = start(&ServeOptions {
            source: ServeSource::Replay {
                path,
                loop_replay: false,
                realtime: false,
            },
            data_dir: Some(dir.join("data")),
            bind: "127.0.0.1:0".parse().unwrap(),
            ui_dist: None,
            fft_len: 1024,
            rows_per_s: 25.0,
            calibration: None,
            token: Some(TOKEN.into()),
            listen: Default::default(),
            compute: Default::default(),
            iq_buffer: Default::default(),
            iq_buffer_hooks: None,
        })
        .unwrap();
        assert!(live_control.is_none(), "no live control over a recording");
        assert_eq!(data_dir, dir.join("data"));
        let addr = server.local_addr();
        let check_empty = || {
            let (status, body) = get(addr, "/api/inventory");
            assert_eq!(status, 200, "{body}");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["entries"], serde_json::json!([]), "{v}");
        };
        check_empty();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(handle.wait().map_err(|e| format!("{e:#}")));
        });
        let summary = rx
            .recv_timeout(Duration::from_secs(60))
            .expect("the run over an empty recording finishes")
            .unwrap();
        assert!(summary.errors.is_empty(), "{:?}", summary.errors);
        assert_eq!(summary.counter("/source/samples"), 0);
        check_empty();
        drop(server);
    }

    /// Sends a request and drops the connection without reading the response, leaving its handler
    /// thread running (and still touching the run's data directory) when the server is stopped.
    fn fire_and_forget(addr: SocketAddr, path: &str) {
        let mut s = TcpStream::connect(addr).unwrap();
        write!(
            s,
            "GET {path} HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let _ = s.flush();
    }

    /// T-236: `hk_api::http::Server::shutdown` waits for its in-flight connection threads, so the
    /// moment it returns nothing under the run's data directory is still open or being written —
    /// `remove_dir_all` succeeds on the **first** attempt, with no retry. This is the root-cause
    /// closure of the leak T-232 measured (every server-starting test left an orphan under load,
    /// because a detached connection thread was still writing the audit log, the database's WAL or
    /// the observation log while the guard walked the directory). Repeated, with unread requests
    /// in flight and one read *after* the pipeline stopped (the shape of T-232's one residual).
    #[test]
    fn shutdown_releases_the_data_dir_on_the_first_removal_attempt() {
        for i in 0..5 {
            let dir = temp_data_dir();
            let guard = TempDataDirGuard::new(dir.clone());
            let path = empty_recording(&dir.join("src"));
            let Serving {
                mut server, handle, ..
            } = start(&ServeOptions {
                source: ServeSource::Replay {
                    path,
                    loop_replay: false,
                    realtime: false,
                },
                data_dir: Some(dir.join("data")),
                bind: "127.0.0.1:0".parse().unwrap(),
                ui_dist: None,
                fft_len: 1024,
                rows_per_s: 25.0,
                calibration: None,
                token: Some(TOKEN.into()),
                listen: Default::default(),
                compute: Default::default(),
                iq_buffer: Default::default(),
                iq_buffer_hooks: None,
            })
            .unwrap();
            let addr = server.local_addr();
            let (status, body) = get(addr, "/api/inventory");
            assert_eq!(status, 200, "{body}");

            let (tx, rx) = std::sync::mpsc::channel();
            handle.stop();
            let waiter = std::thread::spawn(move || {
                let _ = tx.send(handle.wait());
            });
            rx.recv_timeout(Duration::from_secs(60))
                .expect("the run over an empty recording finishes")
                .expect("it finishes cleanly");
            // The waiter drops the handle after sending, tearing the pipeline's stores down on
            // that thread; join it so only the server's own threads are under test here.
            let _ = waiter.join();

            // Requests whose responses nobody reads: their handlers are mid-flight when shutdown
            // runs, which is exactly the race that leaked directories.
            for _ in 0..8 {
                fire_and_forget(addr, "/api/inventory");
            }
            let (status, body) = get(addr, "/api/status");
            assert_eq!(status, 200, "{body}");
            server.shutdown();

            assert_eq!(server.abandoned_connections(), 0, "iteration {i}");
            std::fs::remove_dir_all(&dir).unwrap_or_else(|e| {
                panic!("iteration {i}: the data dir was still busy on the first attempt: {e}")
            });
            assert!(!dir.exists());
            drop(guard);
        }
    }

    #[test]
    fn the_live_source_without_the_driver_is_reported() {
        if HackRfDriver.available() {
            return;
        }
        let dir = temp_data_dir();
        let _guard = TempDataDirGuard::new(dir.clone());
        let err = start(&ServeOptions {
            source: ServeSource::HackRf {
                spec: "hackrf".into(),
                live: LiveArgs::default(),
            },
            data_dir: Some(dir),
            bind: "127.0.0.1:0".parse().unwrap(),
            ui_dist: None,
            fft_len: 1024,
            rows_per_s: 25.0,
            calibration: None,
            token: Some(TOKEN.into()),
            listen: Default::default(),
            compute: Default::default(),
            iq_buffer: Default::default(),
            iq_buffer_hooks: None,
        })
        .err()
        .expect("no driver in this build");
        assert!(format!("{err:#}").contains("hackrf"), "{err:#}");
    }
}
