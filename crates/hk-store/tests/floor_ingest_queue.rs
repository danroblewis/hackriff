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
    let prov: Provenance = serde_json::from_value(serde_json::json!({
        "device_id": "synthetic:t-037b",
        "tune": {"center_hz": 100e6, "sample_rate_hz": FS, "lna_db": 24.0, "vga_db": 20.0,
                 "amp_on": false, "bandwidth_hz": 0.75 * FS},
        "overload": false, "quantisation_limited": false, "clock_source": "internal",
        "clock_locked": true, "timestamp_method": "synthetic", "timestamp_error_budget_ns": 0,
    }))
    .unwrap();
    let prov = ProvenanceHandle::new(prov);
    let mut stft = StftProcessor::new(StftConfig::new(WelchConfig::new(1024), 8)).unwrap();
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
                host_time: Timestamp::from_unix_nanos(T0 + (index as f64 * 1e9 / FS) as i64),
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
