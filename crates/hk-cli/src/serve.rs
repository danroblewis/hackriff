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
                    schedule: false,
                    feeds: None,
                    calibration: opts.calibration.clone(),
                    spectrum_fft_len: Some(opts.fft_len),
                    spectrum_rows_per_s: Some(opts.rows_per_s),
                    compute: opts.compute.clone(),
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
        ..
    } = start(&opts)?;
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
    use hk_api::stream::{GATED_SPECTRUM_MAX_ROW_RATE_HZ, Publisher, PublisherConfig};
    use hk_core::{HackRfDriver, SourceDriver};

    const TOKEN: &str = "t042-serve-empty-inventory-token-0123";

    #[test]
    fn fm_fixture_is_unrestricted_and_others_fail_closed() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
        let fm = SigmfMeta::read(
            root.join("hackrf/2026-09-13/fm_100p8M_2p4M_l32g30a1_t1p5_5s.sigmf-meta"),
        )
        .unwrap();
        assert_eq!(fixture_class(&fm), ContentClass::Unrestricted);
        let ism = SigmfMeta::read(
            root.join("hackrf/2026-09-13/ism_433p62M_2M_l24g30a1_t162p0_6s.sigmf-meta"),
        )
        .unwrap();
        assert_eq!(fixture_class(&ism), ContentClass::MetadataOnly);
        let mut tagged = fm.clone();
        tagged.global.extra.insert(
            "hackriff:content_class".into(),
            serde_json::Value::String("restricted-paging".into()),
        );
        assert_eq!(fixture_class(&tagged), ContentClass::RestrictedPaging);
        tagged.global.extra.insert(
            "hackriff:content_class".into(),
            serde_json::Value::String("bogus".into()),
        );
        assert_eq!(fixture_class(&tagged), ContentClass::MetadataOnly);
    }

    #[test]
    fn gated_plan_declares_within_the_cap_and_the_publisher_accepts_it() {
        for (class, rows) in [
            (ContentClass::MetadataOnly, 200.0),
            (ContentClass::Unrestricted, 60.0),
            (ContentClass::RestrictedPaging, 25.0),
        ] {
            let plan = row_plan(2.4e6, 4096, rows, class);
            assert!(plan.row_rate_hz <= rows + 1e-9);
            if !class.permits_content() {
                assert!(plan.declared_hz <= GATED_SPECTRUM_MAX_ROW_RATE_HZ);
                assert!(plan.declared_hz >= plan.row_rate_hz * 1.09);
            }
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
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_live_source_without_the_driver_is_reported() {
        if HackRfDriver.available() {
            return;
        }
        let err = start(&ServeOptions {
            source: ServeSource::HackRf {
                spec: "hackrf".into(),
                live: LiveArgs::default(),
            },
            data_dir: Some(temp_data_dir()),
            bind: "127.0.0.1:0".parse().unwrap(),
            ui_dist: None,
            fft_len: 1024,
            rows_per_s: 25.0,
            calibration: None,
            token: Some(TOKEN.into()),
            listen: Default::default(),
            compute: Default::default(),
        })
        .err()
        .expect("no driver in this build");
        assert!(format!("{err:#}").contains("hackrf"), "{err:#}");
    }
}
