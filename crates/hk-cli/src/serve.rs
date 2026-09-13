//! `hk serve`: a thin demo composer for the web UI (T-022a). Full pipeline assembly is T-027.
//!
//! ```text
//! SigMF replay (real-time pacing) ─► STFT ─► dB rows ─► spectrum Publisher ─► WebSocket bridge
//!                                       └─► (bounded, drop on full) NoiseFloorTracker ─► FloorProduct
//!                                            (T-017 pyramids + T-021 floor) ─► /api/history, /api/floor
//! ```
//!
//! # The spectrum stream
//! - `stream_id` [`STREAM_ID`], kind `spectrum`, `datatype` `rf32_le`: each row is `fft_size`
//!   little-endian f32 values of PSD in **dBFS/Hz** (hk-dsp `PowerUnit::DbfsPerHz`), ascending
//!   frequency over `center_hz ± bandwidth_hz/2`. The v1.0 header has no units field; this is
//!   the convention of this producer.
//! - `sample_rate_hz` is the declared row rate, `fft_size` and `datatype` are always declared, and
//!   rows carry the IQ time and sample index of their first sample.
//! - **Class** ([`fixture_class`]): the fixture's `hackriff:content_class` when present (parsed
//!   failing closed); otherwise `unrestricted` only when the whole capture lies inside the FM
//!   broadcast band (a band prior, positively chosen); otherwise the fail-closed `metadata-only`,
//!   where the contract gates spectrum at ≤ 50 rows/s. Under a gated class the declared rate is
//!   the actual row rate plus 10 % margin, capped at 50.
//! - The replay is not rewound in place (a gated publisher refuses `t` going backwards): with
//!   `--loop` each pass opens a new publisher under the same id, and browsers reconnect.
//!
//! # History
//! With `--history-dir`, the first pass's frames go to a [`FloorProduct`] through a bounded
//! channel that drops on full, so a slow history query never stalls the stream. Looped passes are
//! not ingested (their times repeat).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Context as _;
use hk_api::stream::{
    BinaryRecord, GATED_SPECTRUM_MAX_ROW_RATE_HZ, Publisher, PublisherConfig, RecordFlags,
    StreamError, StreamHeader, StreamKind,
};
use hk_api::{ApiState, Server, ServerConfig, StreamRegistry, Token};
use hk_core::{Discontinuity, Pacing, ReplayOptions, SigmfReplaySource, Source};
use hk_dsp::floor::{FloorConfig, NoiseFloorTracker};
use hk_dsp::radiometry::PowerCalibrations;
use hk_dsp::{InputInfo, PowerUnit, SpectrumFrame, StftConfig, StftProcessor, WelchConfig};
use hk_model::sigmf::SigmfMeta;
use hk_model::{ContentClass, Repository};
use hk_store::{FloorProduct, FloorProductConfig};
use num_complex::Complex32;

/// Stream id of the replayed spectrum.
pub const STREAM_ID: &str = "spectrum/replay";
/// Datatype of the spectrum rows.
pub const SPECTRUM_DATATYPE: &str = "rf32_le";
/// FM broadcast band used as the `unrestricted` band prior, Hz.
pub const FM_BROADCAST_HZ: (f64, f64) = (87.5e6, 108.0e6);

/// `hk serve` settings.
#[derive(Clone, Debug)]
pub struct ServeOptions {
    /// SigMF recording to replay.
    pub replay: PathBuf,
    /// Floor product / history directory.
    pub history_dir: Option<PathBuf>,
    /// Signal-inventory database (hk-model SQLite) served read-only at `/api/inventory`. The
    /// replay does not write it yet (pipeline composition is T-027).
    pub inventory_db: Option<PathBuf>,
    /// Listen address.
    pub bind: SocketAddr,
    /// Built UI directory.
    pub ui_dist: Option<PathBuf>,
    /// FFT length (bins per row).
    pub fft_len: usize,
    /// Target row rate, rows/s.
    pub rows_per_s: f64,
    /// Replay again after the end.
    pub loop_replay: bool,
    /// Real-time pacing (tests may replay unpaced).
    pub realtime: bool,
}

/// The stream class for a recording (see the [module docs](self)).
pub fn fixture_class(meta: &SigmfMeta) -> ContentClass {
    if let Some(v) = meta.global.extra.get("hackriff:content_class") {
        return ContentClass::parse_fail_closed(v.as_str());
    }
    let fs = meta.global.sample_rate.unwrap_or(f64::INFINITY);
    let centres: Vec<f64> = meta
        .captures
        .iter()
        .filter_map(|c| c.frequency)
        .chain(meta.global.provenance.as_ref().map(|p| p.tune.center_hz))
        .collect();
    let inside =
        |fc: &f64| fc - fs / 2.0 >= FM_BROADCAST_HZ.0 && fc + fs / 2.0 <= FM_BROADCAST_HZ.1;
    if !centres.is_empty() && centres.iter().all(inside) {
        ContentClass::Unrestricted
    } else {
        ContentClass::FAIL_CLOSED
    }
}

/// STFT settings and row rates for a recording rate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowPlan {
    /// STFT settings.
    pub stft: StftConfig,
    /// Actual row rate, rows/s.
    pub row_rate_hz: f64,
    /// Declared row rate (`sample_rate_hz` of the header).
    pub declared_hz: f64,
}

/// Chooses `K` so rows come at most at `rows_per_s` (and within the gated cap when `class`
/// forbids content).
pub fn row_plan(fs: f64, fft_len: usize, rows_per_s: f64, class: ContentClass) -> RowPlan {
    let mut welch = WelchConfig::new(fft_len);
    welch.spectral_kurtosis = false;
    let hop = welch.hop() as f64;
    let gated = !class.permits_content();
    let target = if gated {
        rows_per_s.min(GATED_SPECTRUM_MAX_ROW_RATE_HZ / 1.1)
    } else {
        rows_per_s
    };
    let k = ((fs / (hop * target)).ceil() as usize).max(1);
    let row_rate_hz = fs / (k as f64 * hop);
    let declared_hz = if gated {
        (row_rate_hz * 1.1).min(GATED_SPECTRUM_MAX_ROW_RATE_HZ)
    } else {
        row_rate_hz
    };
    RowPlan {
        stft: StftConfig::new(welch, k),
        row_rate_hz,
        declared_hz,
    }
}

/// The spectrum stream header.
pub fn spectrum_header(
    class: ContentClass,
    plan: &RowPlan,
    center_hz: f64,
    fs: f64,
) -> StreamHeader {
    let bins = plan.stft.welch.fft_len;
    let mut h = StreamHeader::new(STREAM_ID, StreamKind::Spectrum, class, "hk-cli:serve");
    h.datatype = Some(SPECTRUM_DATATYPE.into());
    h.fft_size = Some(bins as u32);
    h.sample_rate_hz = Some(plan.declared_hz);
    h.center_hz = Some(center_hz);
    h.bandwidth_hz = Some(fs);
    h.max_frame_len = (hk_api::stream::BINARY_RECORD_HEADER_LEN + 4 * bins) as u32;
    h
}

/// What one replay pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PassStats {
    /// Rows published (delivered or gated).
    pub rows: u64,
    /// Rows withheld by the gated-spectrum cap.
    pub rows_gated: u64,
    /// Frames not handed to history (channel full).
    pub history_dropped: u64,
}

/// Replays the recording once into a new publisher registered in `registry`.
pub fn replay_pass(
    opts: &ServeOptions,
    registry: &StreamRegistry,
    history: Option<&SyncSender<SpectrumFrame>>,
) -> anyhow::Result<PassStats> {
    let pacing = if opts.realtime {
        Pacing::RealTime { speed: 1.0 }
    } else {
        Pacing::Unpaced
    };
    let mut src = SigmfReplaySource::open(
        &opts.replay,
        ReplayOptions {
            block_len: 16_384,
            pacing,
        },
    )
    .with_context(|| format!("opening {}", opts.replay.display()))?;
    let class = fixture_class(src.meta());
    let fs = src.sample_rate_hz();
    let mut samples: Vec<Complex32> = Vec::new();
    let Some(mut block) = src.read_block(&mut samples)? else {
        return Ok(PassStats::default());
    };
    let plan = row_plan(fs, opts.fft_len, opts.rows_per_s, class);
    let header = spectrum_header(class, &plan, block.center_hz(), fs);
    let bins = plan.stft.welch.fft_len;
    let config = PublisherConfig {
        queue_bytes: (1 << 20).max(64 * (32 + 4 * bins)),
        ..PublisherConfig::default()
    };
    let mut publisher = Publisher::new(header.clone(), config)?;
    registry.register(&header, publisher.handle());
    let mut stft = StftProcessor::new(plan.stft).map_err(|e| anyhow::anyhow!("STFT: {e:?}"))?;
    let mut db = vec![0f32; bins];
    let mut bytes = vec![0u8; 4 * bins];
    let mut stats = PassStats::default();
    let mut failure: Option<StreamError> = None;
    loop {
        stft.push(InputInfo::from(&block), &samples, |frame| {
            let s = &frame.spectrum;
            if s.bins() != bins {
                return;
            }
            s.write_db(&s.psd, PowerUnit::DbfsPerHz, &mut db);
            for (chunk, v) in bytes.chunks_exact_mut(4).zip(&db) {
                chunk.copy_from_slice(&v.to_le_bytes());
            }
            let mut flags = RecordFlags::empty();
            if frame.provenance.get().overload {
                flags = flags.with(RecordFlags::OVERLOAD);
            }
            if frame.discontinuity.bits() & !Discontinuity::STREAM_START.bits() != 0 {
                flags = flags.with(RecordFlags::DISCONTINUITY);
            }
            stats.rows += 1;
            match publisher.publish_binary(BinaryRecord {
                t: frame.t.host_time,
                sample_index: frame.t.sample_index,
                flags,
                payload: &bytes,
            }) {
                Ok(_) => {}
                Err(StreamError::SpectrumGated { .. }) => stats.rows_gated += 1,
                Err(e) => failure = Some(e),
            }
            if let Some(tx) = history {
                if let Err(TrySendError::Full(_)) = tx.try_send(frame.clone()) {
                    stats.history_dropped += 1;
                }
            }
        });
        if let Some(e) = failure.take() {
            return Err(e.into());
        }
        match src.read_block(&mut samples)? {
            Some(next) => block = next,
            None => break,
        }
    }
    publisher.finish();
    Ok(stats)
}

/// Folds frames into the floor product until the channel closes.
pub fn history_worker(rx: Receiver<SpectrumFrame>, product: Arc<Mutex<FloorProduct>>) {
    let Ok(mut tracker) = NoiseFloorTracker::new(FloorConfig::default()) else {
        eprintln!("hk serve: noise-floor tracker config refused; history disabled");
        return;
    };
    let mut errors = 0u64;
    for frame in rx {
        let floor = tracker.update(&frame, |_| {});
        let mut p = product.lock().unwrap_or_else(|e| e.into_inner());
        if p.ingest(&frame, floor).is_err() {
            errors += 1;
            if errors == 1 {
                eprintln!("hk serve: history ingest refused a frame (further refusals counted)");
            }
        }
    }
    let mut p = product.lock().unwrap_or_else(|e| e.into_inner());
    if let Err(e) = p.checkpoint() {
        eprintln!("hk serve: history checkpoint failed: {e}");
    }
    if errors > 0 {
        eprintln!("hk serve: {errors} frames refused by history ingest");
    }
}

/// Runs the server until the process is stopped.
pub fn run(opts: ServeOptions) -> anyhow::Result<()> {
    let token = match std::env::var("HK_TOKEN") {
        Ok(t) => Token::from_config(&t).map_err(|e| anyhow::anyhow!("HK_TOKEN: {e}"))?,
        Err(_) => Token::generate().context("generating the API token")?,
    };
    let floor = match &opts.history_dir {
        Some(dir) => Some(Arc::new(Mutex::new(
            FloorProduct::open(dir, FloorProductConfig::default(), PowerCalibrations::new())
                .with_context(|| format!("opening history in {}", dir.display()))?,
        ))),
        None => None,
    };
    let inventory = match &opts.inventory_db {
        Some(path) => Some(Arc::new(Mutex::new(Repository::open(path).with_context(
            || format!("opening the inventory database {}", path.display()),
        )?))),
        None => None,
    };
    let registry = StreamRegistry::new();
    let state = ApiState {
        streams: registry.clone(),
        history: None,
        floor: floor.clone(),
        inventory,
    };
    let mut config = ServerConfig::new(opts.bind, token.clone());
    config.ui_dist = opts.ui_dist.clone();
    let server = Server::start(config, state).context("starting the HTTP server")?;
    let addr = server.local_addr();
    eprintln!("hk serve: listening on {addr}");
    if !addr.ip().is_loopback() {
        eprintln!(
            "hk serve: WARNING: bound to a non-loopback address; anyone on the network who \
             learns the token (sent in cleartext, no TLS) can read the API"
        );
    }
    let host = if addr.ip().is_unspecified() {
        SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), addr.port())
    } else {
        addr
    };
    // The token rides in the fragment: never sent to the server or logged, stripped by the page.
    println!("open http://{host}/#token={}", token.expose());

    let (tx, worker) = match &floor {
        Some(product) => {
            let (tx, rx) = sync_channel::<SpectrumFrame>(256);
            let product = Arc::clone(product);
            let worker = thread::Builder::new()
                .name("hk-serve-history".into())
                .spawn(move || history_worker(rx, product))?;
            (Some(tx), Some(worker))
        }
        None => (None, None),
    };
    let mut pass = 0u64;
    let mut tx = tx;
    loop {
        let stats = replay_pass(&opts, &registry, tx.as_ref())?;
        pass += 1;
        eprintln!(
            "hk serve: pass {pass}: {} rows ({} gated), {} history frames dropped",
            stats.rows, stats.rows_gated, stats.history_dropped
        );
        tx = None; // only the first pass goes to history
        if !opts.loop_replay {
            break;
        }
    }
    drop(tx);
    if let Some(w) = worker {
        let _ = w.join();
    }
    eprintln!("hk serve: replay finished; still serving the API and history (Ctrl-C to stop)");
    let _server = server; // runs until the process is stopped
    loop {
        thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
