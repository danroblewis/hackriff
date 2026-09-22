//! T-510 — one `Pipeline`, N front ends (milestone MSDR, use case AWARE-011).
//!
//! **The invariant under test is attribution.** Two front ends run concurrently in one run,
//! writing the same stores. A measurement from device A must never be attributed to device B:
//! - every `Detection`'s `provenance_ref -> Provenance.device_id` names the radio whose tuned
//!   window actually contains it (and `Provenance.tune.center_hz` is that radio's centre — the
//!   field T-598's retune classifier reads);
//! - the spectrum history records each front end's frames under **its own** source key, in its
//!   own band;
//! - the coverage record — the thing that decides what the canvas greys — says each device
//!   observed only the band it was tuned to, because "grey = genuinely unobserved" stops being
//!   true the moment one radio's coverage answers for another's.
//!
//! Everything is asserted with **counts over a stated number of records**, never wall-clock, so
//! the test cannot pass on zero.
//!
//! Blind: the recordings' content is irrelevant here; this asserts on provenance and bookkeeping,
//! never on what was decoded or explained.
//!
//! Measurement harness (ignored; run alone, one configuration per process, so `ru_maxrss` is that
//! configuration's peak):
//!
//! ```text
//! HK_T510_N=1 cargo test -p hk-pipeline --test multi_source -- --ignored --nocapture measure
//! HK_T510_N=2 cargo test -p hk-pipeline --test multi_source -- --ignored --nocapture measure
//! ```

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::*;
use hk_core::{MockEnd, Pacing, Source};
use hk_dsp::radiometry::PowerCalibrations;
use hk_model::sigmf::{Capture, SigmfMeta};
use hk_model::{
    ClockSource, FreqRange, Provenance, Region, Repository, TimeRange, Timestamp, TimestampMethod,
    Tune,
};
use hk_pipeline::{
    ExtraSource, Pipeline, PipelineConfig, TrackInventory, open_mock_replay, replay_plan,
};
use hk_store::history::source_key;
use hk_store::{FloorProduct, FloorProductConfig, RegionQuery, Resolution};

/// Device A (the primary): a HackRF-like window on FM broadcast.
const DEVICE_A: &str = "unit-a";
const CENTER_A: f64 = 100.8e6;
const FS_A: f64 = 2.0e6;
const SECS_A: f64 = 4.0;

/// Device B (the further front end): an RTL-like rate on the 433 MHz ISM band.
const DEVICE_B: &str = "unit-b";
const CENTER_B: f64 = 433.92e6;
const FS_B: f64 = 2.4e6;
const SECS_B: f64 = 3.0;

/// The mock names itself after the recording's provenance device.
fn mock_id(device: &str) -> String {
    format!("mock:{device}")
}

/// Fewest history rows each front end must produce for the run to count as "both radios ran".
/// Four seconds at ~10 rows/s is ~40; 20 leaves room for a loaded machine without letting a
/// front end that produced nothing pass.
const MIN_ROWS_PER_DEVICE: u64 = 20;

/// Fewest detections the two front ends must produce between them. A strong tone sits in each
/// recording, so zero would mean the detector never ran — and an attribution assertion over an
/// empty set proves nothing.
const MIN_DETECTIONS: usize = 4;

fn provenance_for(device_id: &str, center_hz: f64, fs: f64) -> Provenance {
    Provenance {
        device_id: device_id.into(),
        tune: Tune {
            center_hz,
            sample_rate_hz: fs,
            lna_db: 24.0,
            vga_db: 20.0,
            amp_on: false,
            bandwidth_hz: fs,
        },
        overload: false,
        quantisation_limited: false,
        noise_sigma_lsb: None,
        temperature_c: None,
        antenna_port: None,
        bias_tee: hk_model::BiasTee::Unknown,
        clock_source: ClockSource::Internal,
        clock_locked: true,
        calibration_state_ref: None,
        spur_mask_ref: None,
        timestamp_method: TimestampMethod::Synthetic,
        timestamp_error_budget_ns: None,
        capture_artefacts: Vec::new(),
    }
}

/// A tone-in-noise recording at `center`/`fs` whose capture carries an explicit
/// `hackriff:provenance` naming `device_id`, so the mock replaying it calls itself
/// `mock:<device_id>`. Both recordings start at the same instant, so the two front ends share a
/// clock (`hk_pipeline::MAX_START_SKEW`).
fn unit_recording(dir: &Path, device_id: &str, center: f64, fs: f64, secs: f64) -> PathBuf {
    let path = tone_recording(dir, device_id, fs, secs, center, None);
    let mut meta = SigmfMeta::read(&path).unwrap();
    meta.global.hw = Some(format!("unit {device_id}"));
    meta.captures = vec![Capture {
        sample_start: 0,
        frequency: Some(center),
        datetime: Some("2026-09-21T12:00:00Z".into()),
        provenance: Some(provenance_for(device_id, center, fs)),
        clip_count: None,
        extra: Default::default(),
    }];
    meta.write(&path).unwrap();
    path
}

fn config(dir: &Path, center: f64, fs: f64, start: Timestamp) -> PipelineConfig {
    let mut cfg = PipelineConfig::new(dir, replay_plan(center, fs, start)).unwrap();
    // Unpaced + lossless: each front end's own flow gate holds its own capture thread for its own
    // readers, so the counts below are the counts the run produced, not a race with the clock.
    cfg.lossless = true;
    cfg.settings.chains = Some(Vec::new());
    // The always-on IQ ring, with a small quota so the test does not preallocate GBs. It is here
    // because a **per-device ring directory** is part of what keeps coverage per device.
    cfg.iq_buffer.enabled = Some(true);
    cfg.iq_buffer.retention_s = 2.0;
    cfg.iq_buffer.max_bytes = Some(64 << 20);
    // The primary is live-shaped (the mock is controllable), so it logs where it looked exactly as
    // `hk serve` does — which is what makes the coverage assertion below symmetric between the two
    // front ends instead of only testing the one this ticket added.
    cfg.live_window_class = true;
    cfg
}

/// The window every store query below uses: the recordings' own instant, generously bracketed.
/// (A query spanning the epoch to the far future is refused by the pyramid — rightly: it would be
/// billions of cells.)
fn window(start: Timestamp) -> TimeRange {
    TimeRange::new(
        start.saturating_add_nanos(-60_000_000_000),
        start.saturating_add_nanos(600_000_000_000),
    )
}

/// Which front end's tuned window contains `f_hz`, or `None` when neither does. The windows are
/// 300 MHz apart, so this is unambiguous by construction.
fn owner_of(f_hz: f64) -> Option<&'static str> {
    if (f_hz - CENTER_A).abs() <= FS_A / 2.0 {
        Some(DEVICE_A)
    } else if (f_hz - CENTER_B).abs() <= FS_B / 2.0 {
        Some(DEVICE_B)
    } else {
        None
    }
}

/// Runs A (primary) and B (further front end) concurrently in one `Pipeline`, and returns the
/// data directory and the run's start instant so the stores can be read back.
fn run_two() -> (TempDir, Timestamp) {
    let rec = TempDir::new("t510-recordings");
    let dir = TempDir::new("t510-two");
    let a = unit_recording(&rec.0, DEVICE_A, CENTER_A, FS_A, SECS_A);
    let b = unit_recording(&rec.0, DEVICE_B, CENTER_B, FS_B, SECS_B);

    let primary = open_mock_replay(&a, Pacing::Unpaced, MockEnd::Stop).unwrap();
    let second = open_mock_replay(&b, Pacing::Unpaced, MockEnd::Stop).unwrap();
    assert_eq!(primary.device.device_id, mock_id(DEVICE_A));
    assert_eq!(second.device.device_id, mock_id(DEVICE_B));

    let start = primary.info.start_time;
    let mut cfg = config(&dir.0, CENTER_A, FS_A, start);
    cfg.source_class = primary.class;
    cfg.device_id = primary.device.device_id.clone();

    let handle = Pipeline::start_multi(
        cfg,
        Box::new(primary.source) as Box<dyn Source>,
        primary.info,
        None,
        Box::new(TrackInventory::default()),
        vec![ExtraSource {
            source: Box::new(second.source) as Box<dyn Source>,
            info: second.info,
        }],
    )
    .unwrap();

    // The run lists both front ends, the primary first.
    let devices = handle.devices();
    assert_eq!(devices.len(), 2, "one Pipeline, two front ends");
    assert!(devices[0].primary && !devices[1].primary);
    assert_eq!(
        devices[0].device_id.as_deref(),
        Some(mock_id(DEVICE_A)).as_deref()
    );
    assert_eq!(
        devices[1].device_id.as_deref(),
        Some(mock_id(DEVICE_B)).as_deref()
    );
    // Counters are per front end: `Counters` is one object per radio, not one per run.
    assert!(
        !std::sync::Arc::ptr_eq(&devices[0].counters, &devices[1].counters),
        "each front end needs its own counters, or the second overwrites the first's live state"
    );
    drop(devices);

    let (summary, fired) = wait_guarded(handle, Duration::from_secs(300));
    assert!(!fired, "the run did not finish on its own");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);

    // Separate IQ rings on disk: a shared ring would let one radio's segments — which is what the
    // coverage map reads — answer for another's.
    assert!(dir.0.join("iqbuffer").is_dir(), "the primary's IQ ring");
    let aux_rings: Vec<_> = std::fs::read_dir(dir.0.join("iqbuffer-devices"))
        .expect("the further front end's IQ ring directory")
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(
        aux_rings.len(),
        1,
        "one further front end, one further ring: {aux_rings:?}"
    );
    (dir, start)
}

/// **Both front ends produce rows, and neither starves the other.**
///
/// Counts only: each device's frames are recorded in the shared pyramid under its **own** source
/// key and only in its **own** band, at least [`MIN_ROWS_PER_DEVICE`] of them.
#[test]
fn two_mock_front_ends_each_produce_rows_under_their_own_source_key() {
    let (dir, start) = run_two();
    let product = FloorProduct::open(
        dir.0.join("history"),
        FloorProductConfig {
            mixed_shapes: true,
            ..FloorProductConfig::default()
        },
        PowerCalibrations::new(),
    )
    .unwrap();

    let mut rows: BTreeMap<String, u64> = BTreeMap::new();
    for (device, center, fs) in [(DEVICE_A, CENTER_A, FS_A), (DEVICE_B, CENTER_B, FS_B)] {
        let (other, other_center, other_fs) = if device == DEVICE_A {
            (DEVICE_B, CENTER_B, FS_B)
        } else {
            (DEVICE_A, CENTER_A, FS_A)
        };
        let key = source_key(&mock_id(device));
        rows.insert(mock_id(device), frames_of(&product, key, center, fs, start));
        let intruding = frames_of(&product, key, other_center, other_fs, start);
        assert_eq!(
            intruding,
            0,
            "{} has {intruding} history rows in {}'s band: one radio's spectrum history is \
             answering for another's",
            mock_id(device),
            mock_id(other)
        );
    }
    for (id, n) in &rows {
        assert!(
            *n >= MIN_ROWS_PER_DEVICE,
            "{id} folded {n} history rows, wanted at least {MIN_ROWS_PER_DEVICE}: {rows:?}"
        );
    }
    eprintln!("T-510 history rows per front end: {rows:?}");
}

/// Frames the pyramid holds under source `key` over `center ± fs/2`, across the whole run.
fn frames_of(product: &FloorProduct, key: u64, center: f64, fs: f64, start: Timestamp) -> u64 {
    let h = product
        .uncalibrated_pyramid()
        .query(&RegionQuery {
            freq: FreqRange::centered(center, fs),
            time: window(start),
            resolution: Resolution::Level(0),
        })
        .unwrap();
    h.provenance
        .origins
        .iter()
        .filter(|(o, _)| o.source == Some(key))
        .map(|(_, n)| *n)
        .sum()
}

/// **The attribution invariant: a detection from device A is never attributed to device B.**
///
/// Every detection the run wrote is checked against the only front end whose tuned window
/// contains it: its provenance must name that radio, and carry that radio's centre (the field
/// T-598's retune classifier reads). The number of detections judged is asserted and printed, so
/// this cannot pass vacuously.
#[test]
fn every_detection_carries_the_provenance_of_the_front_end_that_saw_it() {
    let (dir, start) = run_two();
    let repo = Repository::open(dir.0.join("hackriff.db")).unwrap();
    let all = repo
        .detections_in_region(&Region::new(FreqRange::new(1e6, 6e9), window(start)))
        .unwrap();

    let mut judged: BTreeMap<String, usize> = BTreeMap::new();
    for d in &all {
        let Some(owner) = owner_of(d.f_center_hz) else {
            panic!(
                "a detection at {:.3} MHz is in neither front end's window",
                d.f_center_hz / 1e6
            );
        };
        let p = repo.provenance(d.provenance_ref).unwrap();
        assert_eq!(
            p.device_id,
            mock_id(owner),
            "a detection at {:.3} MHz (only {} was tuned there) is attributed to {}",
            d.f_center_hz / 1e6,
            mock_id(owner),
            p.device_id
        );
        let want_center = if owner == DEVICE_A {
            CENTER_A
        } else {
            CENTER_B
        };
        assert!(
            (p.tune.center_hz - want_center).abs() < 1.0,
            "{}'s detection carries tune.center_hz {} , wanted {want_center}",
            p.device_id,
            p.tune.center_hz
        );
        *judged.entry(p.device_id.clone()).or_default() += 1;
    }

    eprintln!("T-510 detections judged per front end: {judged:?}");
    assert!(
        all.len() >= MIN_DETECTIONS,
        "only {} detections were judged (wanted at least {MIN_DETECTIONS}): an attribution \
         assertion over an empty set proves nothing",
        all.len()
    );
    assert_eq!(
        judged.len(),
        2,
        "both front ends must have contributed detections, got {judged:?}"
    );
}

/// **The coverage map stays per device.**
///
/// This is the invariant that makes grey honest: the canvas computes "did *this* radio look here"
/// per device from the observation records, so one front end's coverage must never answer for
/// another's. Asserted on [`hk_store::coverage`]'s own grids — each device observed in its own
/// band and **unobserved in the other's** — plus the records themselves.
#[test]
fn the_coverage_map_is_per_front_end() {
    let (dir, start) = run_two();
    let store = hk_store::observation::ObservationStore::open(
        hk_store::observation::ObservationLogConfig::new(dir.0.join("observations")),
    )
    .unwrap();
    let band = FreqRange::new(1e6, 6e9);
    let page = store.query(&hk_store::observation::RecordQuery {
        freq: band,
        span: window(start),
        tier: None,
        cursor: 0,
        limit: hk_store::observation::MAX_RECORD_LIMIT,
    });

    // Every record names a front end, and covers only that front end's own band.
    let mut per_device: BTreeMap<String, usize> = BTreeMap::new();
    for rec in &page.records {
        let hk_model::attention::observation::ObservationRecord::Dwell(d) = rec else {
            continue;
        };
        let Some(device_id) = d.device_id.clone() else {
            panic!("a dwell record names no front end: its coverage could answer for any radio");
        };
        let owner = owner_of(d.window.center_hz).map(mock_id);
        assert_eq!(
            owner.as_deref(),
            Some(device_id.as_str()),
            "a dwell record by {device_id} covers {:.3} MHz, which only {owner:?} was tuned to",
            d.window.center_hz / 1e6
        );
        *per_device.entry(device_id).or_default() += 1;
    }
    eprintln!("T-510 dwell records per front end: {per_device:?}");
    assert_eq!(
        per_device.len(),
        2,
        "both front ends must have recorded where they looked, got {per_device:?}"
    );

    // And the grids the canvas actually greys from: observed in mine, unobserved in yours.
    let spans = hk_store::coverage::spans_from_records(&page.records, &page.geometries, band);
    assert_eq!(
        spans.named,
        spans.spans.len(),
        "every coverage span must name the radio that produced it"
    );
    for (device, center, fs) in [(DEVICE_A, CENTER_A, FS_A), (DEVICE_B, CENTER_B, FS_B)] {
        let (other, other_center, other_fs) = if device == DEVICE_A {
            (DEVICE_B, CENTER_B, FS_B)
        } else {
            (DEVICE_A, CENTER_A, FS_A)
        };
        let me = hk_store::coverage::Device::Id(mock_id(device));
        let mine = hk_store::coverage::grid(
            &spans.spans,
            &me,
            FreqRange::centered(center, fs),
            window(start),
            8,
        );
        assert!(
            mine.observed_cells() > 0,
            "{} must be observed in its own band",
            mock_id(device)
        );
        let theirs = hk_store::coverage::grid(
            &spans.spans,
            &me,
            FreqRange::centered(other_center, other_fs),
            window(start),
            8,
        );
        assert_eq!(
            theirs.observed_cells(),
            0,
            "{} is marked observed over {}'s band: one radio's coverage is answering for \
             another's, and grey stops being honest",
            mock_id(device),
            mock_id(other)
        );
    }
}

/// **Two front ends cannot share an identity.** Refused before anything opens: two radios under
/// one id would pool their floors, detections and coverage, which is precisely what the
/// per-device stores exist to prevent.
#[test]
fn two_front_ends_under_one_device_id_are_refused() {
    let rec = TempDir::new("t510-same-id-recordings");
    let dir = TempDir::new("t510-same-id");
    let a = unit_recording(&rec.0, DEVICE_A, CENTER_A, FS_A, 1.0);
    let b = unit_recording(&rec.0, DEVICE_A, CENTER_B, FS_B, 1.0);
    let primary = open_mock_replay(&a, Pacing::Unpaced, MockEnd::Stop).unwrap();
    let second = open_mock_replay(&b, Pacing::Unpaced, MockEnd::Stop).unwrap();
    let mut cfg = config(&dir.0, CENTER_A, FS_A, primary.info.start_time);
    cfg.source_class = primary.class;
    let err = Pipeline::start_multi(
        cfg,
        Box::new(primary.source) as Box<dyn Source>,
        primary.info,
        None,
        Box::new(TrackInventory::default()),
        vec![ExtraSource {
            source: Box::new(second.source) as Box<dyn Source>,
            info: second.info,
        }],
    );

    let err = match err {
        Ok(_) => panic!("two front ends under one device id must be refused"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("already part of this run"),
        "{err:#}"
    );
    // Nothing was opened: the refusal happens before the stores are created.
    assert!(!dir.0.join("hackriff.db").exists());
}

fn max_rss_bytes() -> u64 {
    // SAFETY: `ru` is a valid, writable rusage; the call only writes it.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut ru) };
    // macOS reports bytes, Linux KiB.
    if cfg!(target_os = "macos") {
        ru.ru_maxrss as u64
    } else {
        ru.ru_maxrss as u64 * 1024
    }
}

/// **What N costs.** The per-arriving-row work on the capture thread is paid whether or not
/// anyone looks, and that thread gates the ring (T-453), so the cost of composing a second front
/// end has to be measured rather than assumed. Prints capture CPU per Msample and history CPU per
/// row **for each front end**, plus the process peak RSS, for N=1 and N=2.
#[test]
#[ignore = "measurement harness (T-510); run alone with --ignored --nocapture"]
fn measure_capture_cost() {
    let n: usize = std::env::var("HK_T510_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let secs: f64 = std::env::var("HK_T510_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10.0);
    let rec = TempDir::new("t510-measure-recordings");
    let dir = TempDir::new("t510-measure");
    let a = unit_recording(&rec.0, DEVICE_A, CENTER_A, FS_A, secs);
    let primary = open_mock_replay(&a, Pacing::Unpaced, MockEnd::Stop).unwrap();
    let mut cfg = config(&dir.0, CENTER_A, FS_A, primary.info.start_time);
    cfg.source_class = primary.class;
    cfg.device_id = primary.device.device_id.clone();
    let extra = (n > 1)
        .then(|| {
            let b = unit_recording(&rec.0, DEVICE_B, CENTER_B, FS_B, secs);
            let second = open_mock_replay(&b, Pacing::Unpaced, MockEnd::Stop).unwrap();
            ExtraSource {
                source: Box::new(second.source) as Box<dyn Source>,
                info: second.info,
            }
        })
        .into_iter()
        .collect();

    let started = Instant::now();
    let handle = Pipeline::start_multi(
        cfg,
        Box::new(primary.source) as Box<dyn Source>,
        primary.info,
        None,
        Box::new(TrackInventory::default()),
        extra,
    )
    .unwrap();
    let devices = handle.devices();
    let counters: Vec<_> = devices
        .iter()
        .map(|d| (d.device_id.clone().unwrap_or_default(), d.counters.clone()))
        .collect();
    drop(devices);
    let (summary, fired) = wait_guarded(handle, Duration::from_secs(600));
    assert!(!fired);
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);

    let wall = started.elapsed().as_secs_f64();
    for (id, c) in &counters {
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let (blocks, samples, cap) = (
            l(&c.source.blocks),
            l(&c.source.samples),
            l(&c.source.cpu_ns),
        );
        let (rows, hist) = (l(&c.history_reader.frames), l(&c.history_reader.cpu_ns));
        println!(
            "T510 N={n} wall={wall:.1}s dev={id} blocks={blocks} samples={samples} \
             capture_cpu_ms={:.1} capture_ns_per_block={} capture_ms_per_msample={:.2} \
             rows={rows} history_cpu_ms={:.1} history_ms_per_row={:.2} lost={}",
            cap as f64 / 1e6,
            cap / blocks.max(1),
            cap as f64 / 1e3 / (samples.max(1) as f64 / 1e6) / 1e3,
            hist as f64 / 1e6,
            hist as f64 / 1e6 / rows.max(1) as f64,
            l(&c.history_reader.lost_samples),
        );
    }
    println!(
        "T510 N={n} maxrss_mb={:.1}",
        max_rss_bytes() as f64 / (1 << 20) as f64
    );
}
