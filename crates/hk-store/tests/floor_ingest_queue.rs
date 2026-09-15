//! T-037b: in live mode `/api/history` and `/api/floor` hold the floor product's lock for a whole
//! query. The history reader's ingest goes through `FloorIngestQueue`, which must never wait for
//! such a query: frames are queued while the lock is held and folded, in order, once it is free.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::floor::{FloorConfig, FloorFrame, NoiseFloorTracker};
use hk_dsp::radiometry::PowerCalibrations;
use hk_dsp::synth::{Rng, complex_noise};
use hk_dsp::{InputInfo, SpectrumFrame, StftConfig, StftProcessor, WelchConfig};
use hk_model::{Provenance, SampleTime, Timestamp};
use hk_store::{FloorIngestQueue, FloorProduct, FloorProductConfig};

const FS: f64 = 1e6;
/// 2026-09-13T12:00:00Z.
const T0: i64 = 1_789_300_800_000_000_000;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("hk-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `seconds` of noise → STFT → floor tracker: the frame pairs the history reader ingests.
fn frames(seconds: f64) -> Vec<(SpectrumFrame, FloorFrame)> {
    frames_k(seconds, 8, 0)
}

/// [`frames`] averaging `k` segments per frame, starting `offset_ns` after `T0`.
fn frames_k(seconds: f64, k: usize, offset_ns: i64) -> Vec<(SpectrumFrame, FloorFrame)> {
    let prov: Provenance = serde_json::from_value(serde_json::json!({
        "device_id": "synthetic:t-037b",
        "tune": {"center_hz": 100e6, "sample_rate_hz": FS, "lna_db": 24.0, "vga_db": 20.0,
                 "amp_on": false, "bandwidth_hz": 0.75 * FS},
        "overload": false, "quantisation_limited": false, "clock_source": "internal",
        "clock_locked": true, "timestamp_method": "synthetic", "timestamp_error_budget_ns": 0,
    }))
    .unwrap();
    let prov = ProvenanceHandle::new(prov);
    let mut stft = StftProcessor::new(StftConfig::new(WelchConfig::new(1024), k)).unwrap();
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let mut rng = Rng::new(0x037b);
    let mut out = Vec::new();
    let mut disc = Discontinuity::STREAM_START;
    let (mut index, end) = (0u64, (seconds * FS) as u64);
    while index < end {
        let n = 10_000.min(end - index);
        let samples = complex_noise(&mut rng, n as usize, 1e-3);
        let info = InputInfo {
            time: SampleTime {
                sample_index: index,
                host_time: Timestamp::from_unix_nanos(
                    T0 + offset_ns + (index as f64 * 1e9 / FS) as i64,
                ),
            },
            discontinuity: std::mem::replace(&mut disc, Discontinuity::NONE),
            dropped_before: 0,
            provenance: &prov,
        };
        stft.push(info, &samples, |frame| {
            let floor = tracker.update(frame, |_| {});
            out.push((frame.clone(), floor.clone()));
        });
        index += n;
    }
    out
}

/// Holds the product's lock on another thread (a slow query) until the returned sender is
/// dropped or sent to; the lock is held when this returns.
fn hold(product: &Arc<Mutex<FloorProduct>>) -> (mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let (held_tx, held_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let p = Arc::clone(product);
    let join = std::thread::spawn(move || {
        let _query = p.lock().unwrap();
        held_tx.send(()).unwrap();
        let _ = release_rx.recv_timeout(Duration::from_secs(10));
    });
    held_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    (release_tx, join)
}

#[test]
fn ingest_never_waits_for_a_query_holding_the_floor_product() {
    let dir = TempDir::new("ingest-queue");
    let product = Arc::new(Mutex::new(
        FloorProduct::open(
            &dir.0,
            FloorProductConfig::default(),
            PowerCalibrations::new(),
        )
        .unwrap(),
    ));
    let pairs = frames(1.0);
    let held = 30;
    assert!(pairs.len() > held + 8, "{} frames", pairs.len());
    let mut queue = FloorIngestQueue::new(1000);

    let r = queue.ingest(&product, &pairs[0].0, &pairs[0].1);
    assert!(!r.deferred && r.folded.len() == 1 && r.folded[0].is_ok());

    let (release, query) = hold(&product);
    for (s, f) in &pairs[1..=held] {
        let t = Instant::now();
        let r = queue.ingest(&product, s, f);
        assert!(
            t.elapsed() < Duration::from_millis(100),
            "ingest waited {:?} for the query",
            t.elapsed()
        );
        assert!(r.deferred && r.folded.is_empty() && r.dropped == 0);
    }
    assert_eq!(queue.pending(), held);
    release.send(()).unwrap();
    query.join().unwrap();

    // The next frame folds the queued ones first, in order, then itself.
    let next = held + 1;
    let r = queue.ingest(&product, &pairs[next].0, &pairs[next].1);
    assert!(!r.deferred);
    assert_eq!(r.folded.len(), held + 1);
    assert!(r.folded.iter().all(Result::is_ok), "{:?}", r.folded);
    assert_eq!(queue.pending(), 0);
    let st = queue.stats();
    assert_eq!(
        (st.deferred, st.dropped, st.folded_late),
        (held as u64, 0, held as u64)
    );
    assert_eq!(
        product.lock().unwrap().stats().uncalibrated_frames,
        next as u64 + 1,
        "every frame folded exactly once"
    );

    // Beyond the capacity the oldest queued frames are dropped and counted; `drain` folds the
    // rest under the caller's lock.
    let mut small = FloorIngestQueue::new(4);
    let (release, query) = hold(&product);
    let dropped: u64 = pairs[next + 1..next + 7]
        .iter()
        .map(|(s, f)| small.ingest(&product, s, f).dropped)
        .sum();
    release.send(()).unwrap();
    query.join().unwrap();
    assert_eq!((dropped, small.pending(), small.stats().dropped), (2, 4, 2));
    let folded = small.drain(&mut product.lock().unwrap());
    assert_eq!(folded.len(), 4);
    assert!(folded.iter().all(Result::is_ok));
}

/// T-139: frames averaging fewer segments (another cell shape) are rejected by default and folded
/// and counted with `mixed_shapes`; the floor then still answers from the cells' own shapes.
#[test]
fn mixed_shapes_fold_short_rows_instead_of_rejecting_them() {
    let full = frames_k(2.0, 8, 0);
    // K = 3: a partial row's geometry, 120 s later (its own 60-s level-0 tile, uniform shape).
    let short = frames_k(2.0, 3, 120_000_000_000);
    // And the same geometry inside the full rows' tile (30 s in): that tile's shape is mixed.
    let shared = frames_k(2.0, 3, 30_000_000_000);
    let open = |tag: &str, mixed: bool| {
        let dir = TempDir::new(tag);
        let p = FloorProduct::open(
            &dir.0,
            FloorProductConfig {
                mixed_shapes: mixed,
                ..FloorProductConfig::default()
            },
            PowerCalibrations::new(),
        )
        .unwrap();
        (dir, p)
    };
    let ingest = |p: &mut FloorProduct| {
        for (s, f) in full.iter().chain(&short) {
            let _ = p.ingest(s, f);
        }
    };

    let (_d0, mut strict) = open("shape-strict", false);
    ingest(&mut strict);
    let st = strict.stats();
    assert_eq!(st.rejected_frames, short.len() as u64, "{st:?}");
    assert_eq!(st.mixed_shape_frames, 0);

    let (_d1, mut mixed) = open("shape-mixed", true);
    ingest(&mut mixed);
    let st = mixed.stats();
    assert_eq!(st.rejected_frames, 0, "{st:?}");
    assert_eq!(st.mixed_shape_frames, short.len() as u64);
    assert_eq!(st.uncalibrated_frames, (full.len() + short.len()) as u64);
    mixed
        .seal_through(Timestamp::from_unix_nanos(T0 + 3_600_000_000_000))
        .unwrap();
    let floor = |p: &FloorProduct, s0: i64, s1: i64| -> Vec<f64> {
        p.floor_vs_time(
            hk_model::FreqRange::centered(100e6, 0.5 * FS),
            Timestamp::from_unix_nanos(T0 + s0 * 1_000_000_000),
            Timestamp::from_unix_nanos(T0 + s1 * 1_000_000_000),
            hk_store::Resolution::Level(0),
        )
        .unwrap()
        .steps
        .iter()
        .filter_map(|s| s.value_db_per_hz)
        .collect()
    };
    let (v_full, v_short) = (floor(&mixed, 0, 3), floor(&mixed, 120, 123));
    assert!(
        !v_full.is_empty() && !v_short.is_empty(),
        "each uniform tile has a floor: {v_full:?} {v_short:?}"
    );
    let values: Vec<f64> = v_full.iter().chain(&v_short).copied().collect();
    // Both geometries measure the same noise (same seed): bias-corrected floors agree within 2 dB
    // (0.5 dB histogram step; the Gamma bias model is looser at K = 3, measured 1.6 dB worst cell).
    let (lo, hi) = values
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |a, v| {
            (a.0.min(*v), a.1.max(*v))
        });
    assert!(hi - lo < 2.0, "floors {values:?}");

    // A tile holding both geometries has no single shape: no bias-corrected floor, never a
    // wrong one.
    let (_d2, mut both) = open("shape-both", true);
    for (s, f) in full.iter().chain(&shared) {
        both.ingest(s, f).unwrap();
    }
    both.seal_through(Timestamp::from_unix_nanos(T0 + 3_600_000_000_000))
        .unwrap();
    assert!(both.stats().mixed_shape_frames > 0);
    assert!(
        floor(&both, 0, 60).is_empty(),
        "a mixed-shape tile reports no floor"
    );
}
