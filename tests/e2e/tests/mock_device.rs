//! T-049 / T-057: the whole pipeline driven **through the mock SDR device**, blind, on the real
//! `fm_100p8M_2p4M_l32g30a1_t1p5_5s` fixture (annotations stripped with
//! `hk_e2e::blind::strip_truth`; only the test holds the truth).
//!
//! - **Blind FM through the device (T-049):** the mock at its power-on tuning (the recording's
//!   centre, rate and gains) feeds `Pipeline::start` like the radio would. The class is derived from
//!   the tuned window as for a live device there. The 101.3 MHz station is detected and every
//!   emitter matching it carries FM broadcast among its ranked explanations.
//! - **Scheduled replay stays truthful (T-057):** hackriffd's path (`open_mock_replay` with the
//!   attention scheduler driving it). A wrapper checks, block by block, that the station's energy
//!   sits where the block's provenance centre says it does, whatever the scheduler tunes to. The
//!   station's detections and emitter land at 101.3 MHz, and nothing FM-wide is detected in the
//!   noise-filled spectrum outside the recording.

#[path = "acceptance/common.rs"]
mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use hk_core::{
    BlockHeader, Coverage, MockEnd, MockOptions, MockSdrDriver, Pacing, Source, SourceCapabilities,
    SourceControl, SourceError,
};
use hk_e2e::blind::{matching, strip_truth};
use hk_e2e::{Fixture, TruthItem};
use hk_model::attention::occupancy::OccupancySubject;
use hk_model::{ContentClass, FreqRange, InventoryQuery, LinkTarget, Region};
use hk_pipeline::class::band_class;
use hk_pipeline::{
    Pipeline, PipelineConfig, PipelineHandle, RunSummary, SourceInfo, TrackInventory, explanations,
    open_mock_replay, replay_block_len, replay_plan,
};
use hk_store::occupancy::{
    OccupancyQuery, OccupancyStore, OccupancyStoreConfig, SeriesInterval, SubjectKind,
};
use num_complex::{Complex, Complex32};

const FM_FIXTURE: &str = "fm_100p8M_2p4M_l32g30a1_t1p5_5s";
const CENTER_TOL_HZ: f64 = 50e3;
const RUN_LIMIT: Duration = Duration::from_secs(1800);

fn station(fx: &Fixture) -> TruthItem {
    fx.of_kind("wfm-broadcast")
        .first()
        .map(|t| (*t).clone())
        .expect("fixture truth has a wfm-broadcast station")
}

/// Waits for a run within [`RUN_LIMIT`] and requires no thread errors.
fn bounded_finish(handle: PipelineHandle) -> RunSummary {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(finish(handle));
    });
    rx.recv_timeout(RUN_LIMIT)
        .expect("the run finished within the time limit")
}

/// Matched detections and FM-explained emitters of a finished run.
fn station_hits(dir: &TempDir, truth: &TruthItem) -> (usize, Vec<(f64, bool)>) {
    let repo = repo(&dir.0);
    let detections = repo
        .detections_in_region(&Region::new(FreqRange::new(0.0, 7.0e9), ever()))
        .unwrap();
    let dets = matching(
        truth,
        0.0,
        &detections,
        |d| (d.f_center_hz, d.obw_hz),
        CENTER_TOL_HZ,
    )
    .len();
    let all = inventory(&repo, InventoryQuery::default());
    let emitters = matching(
        truth,
        0.0,
        &all,
        |e| (e.emitter.f_center_hz, e.emitter.bandwidth_hz),
        CENTER_TOL_HZ,
    )
    .into_iter()
    .map(|e| {
        let x = explanations(&repo, e.emitter.id).unwrap();
        eprintln!(
            "emitter {:.4} MHz bw {:.0} Hz top-k {:?}",
            e.emitter.f_center_hz / 1e6,
            e.emitter.bandwidth_hz,
            x.iter().map(|x| x.service.as_str()).collect::<Vec<_>>()
        );
        (
            e.emitter.f_center_hz,
            x.iter().any(|x| x.service == "fm-broadcast"),
        )
    })
    .collect();
    eprintln!(
        "{dets} of {} detections and {} of {} emitters match the private truth",
        detections.len(),
        all.len(),
        all.len()
    );
    (dets, emitters)
}

#[test]
fn blind_fm_through_the_mock_device_detects_the_station_with_fm_broadcast_in_top_k() {
    let Some(meta) = real_fixture(FM_FIXTURE) else {
        return;
    };
    let truth = station(&Fixture::load(&meta).unwrap());
    let src = TempDir::new("mockfm-src");
    let blind = strip_truth(&meta, &src.0, "blind", 0.0).unwrap();
    let fs = hk_model::sigmf::SigmfMeta::read(&blind)
        .unwrap()
        .global
        .sample_rate
        .unwrap();
    let driver = MockSdrDriver::new(
        &blind,
        MockOptions {
            block_len: replay_block_len(fs),
            ..MockOptions::default()
        },
    )
    .unwrap();
    let source = driver.open_mock(&driver.default_request()).unwrap();
    let control = source.mock_control();
    let device = source.control().device_info().unwrap();
    let info = SourceInfo {
        sample_rate_hz: source.recording().sample_rate_hz,
        center_hz: source.recording().center_hz,
        start_time: source.start_time(),
    };
    // Content class (informational) exactly as a live device tuned there would get.
    let class = band_class(&[info.center_hz], info.sample_rate_hz);
    assert_eq!(
        class,
        ContentClass::Unrestricted,
        "FM broadcast band window"
    );

    let dir = TempDir::new("mockfm");
    let plan = replay_plan(info.center_hz, info.sample_rate_hz, info.start_time);
    let mut cfg = PipelineConfig::new(&dir.0, plan).unwrap();
    cfg.source_class = class;
    cfg.lossless = source.pausable();
    cfg.device_id = device.device_id.clone();
    cfg.device_hw = Some(device.hw.clone());
    let handle = Pipeline::start(
        cfg,
        Box::new(source),
        info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let summary = bounded_finish(handle);

    let stats = control.mock_stats();
    eprintln!("mock device: {stats:?}");
    assert_eq!(
        summary.counter("/source/samples"),
        12_000_000,
        "every sample served"
    );
    assert_eq!(
        (
            stats.source.overruns,
            stats.source.dropped_samples,
            stats.source.discarded_samples
        ),
        (0, 0, 0),
        "no control change, no loss"
    );
    assert_eq!(stats.uncovered_samples, 0, "tuned to the recording");
    assert_eq!(summary.always_on_lost_samples, 0);

    let (dets, emitters) = station_hits(&dir, &truth);
    assert!(
        dets > 0,
        "the 101.3 MHz station was not detected blind through the mock"
    );
    assert!(!emitters.is_empty(), "no emitter at the station");
    for (f, fm) in &emitters {
        assert!(*fm, "emitter at {f} Hz lacks FM broadcast in its top-k");
    }
}

/// Station energy check on the IQ a block actually carries.
#[derive(Debug, Default)]
struct Truthfulness {
    centres: Vec<f64>,
    checked: u64,
    failures: Vec<String>,
    partial_or_noise_blocks: u64,
}

/// Wraps the mock: every 16th block whose window holds the station, the station band must stand
/// clear of the window's median floor at the offset the provenance centre implies.
struct TruthfulIq {
    inner: Box<dyn Source>,
    station_hz: f64,
    blocks: u64,
    seen: Arc<Mutex<Truthfulness>>,
}

/// Mean power of `x` around `f` Hz through a 3-stage moving-average low-pass whose first null is
/// at ±250 kHz (−3 dB near ±65 kHz, first sidelobe −39 dB): one FM broadcast channel.
fn band_power(x: &[Complex<i8>], f: f64, fs: f64) -> f64 {
    let len = ((fs / 250e3).round() as usize).max(1);
    let mut y: Vec<num_complex::Complex64> = x
        .iter()
        .enumerate()
        .map(|(i, z)| {
            num_complex::Complex64::new(f64::from(z.re), f64::from(z.im))
                * num_complex::Complex64::from_polar(
                    1.0,
                    -std::f64::consts::TAU * f * i as f64 / fs,
                )
        })
        .collect();
    for _ in 0..3 {
        y = y
            .windows(len)
            .map(|w| w.iter().sum::<num_complex::Complex64>() / len as f64)
            .collect();
    }
    y.iter().map(|z| z.norm_sqr()).sum::<f64>() / y.len() as f64
}

impl TruthfulIq {
    fn check(&mut self, h: &BlockHeader, x: &[Complex<i8>]) {
        let mut seen = self.seen.lock().unwrap();
        let c = h.center_hz();
        if seen.centres.last() != Some(&c) {
            seen.centres.push(c);
        }
        if Coverage::from_provenance(&h.provenance) != Some(Coverage::Recorded) {
            seen.partial_or_noise_blocks += 1;
        }
        self.blocks += 1;
        let fs = h.sample_rate_hz();
        let off = self.station_hz - c;
        if self.blocks % 16 != 0 || off.abs() > fs / 2.0 - 200e3 || x.len() < 4096 {
            return;
        }
        // T-175: band power over the station's channel, not 7 narrow (≈ 250 Hz) spectral samples
        // of it. Which 5 ms of FM programme a checked block holds depends on where the scheduler's
        // retune landed in stream time (thread timing in an unpaced replay), and a block whose
        // instantaneous deviation sat between the samples read as 3× floor while its neighbours
        // read 10–200×. The floor is measured through the same filter.
        let band = band_power(x, off, fs);
        let mut floor: Vec<f64> = (0..64)
            .map(|k| band_power(x, -0.4 * fs + 0.8 * fs * f64::from(k) / 63.0, fs))
            .collect();
        floor.sort_by(f64::total_cmp);
        let median = floor[32];
        seen.checked += 1;
        if band < 4.0 * median {
            seen.failures.push(format!(
                "block {} at centre {c} Hz, rate {fs} Hz: station band {band:.3e} vs floor \
                 {median:.3e} at offset {off} Hz",
                self.blocks
            ));
        }
    }
}

impl Source for TruthfulIq {
    fn capabilities(&self) -> &SourceCapabilities {
        self.inner.capabilities()
    }

    fn control(&self) -> Arc<dyn SourceControl> {
        self.inner.control()
    }

    fn pausable(&self) -> bool {
        self.inner.pausable()
    }

    fn read_block(
        &mut self,
        samples: &mut Vec<Complex32>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        self.inner.read_block(samples)
    }

    fn read_block_ci8(
        &mut self,
        samples: &mut Vec<Complex<i8>>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        let h = self.inner.read_block_ci8(samples)?;
        if let Some(h) = &h {
            self.check(h, samples);
        }
        Ok(h)
    }
}

#[test]
fn t057_scheduled_replay_keeps_every_frequency_truthful() {
    let Some(meta) = real_fixture(FM_FIXTURE) else {
        return;
    };
    let truth = station(&Fixture::load(&meta).unwrap());
    let station_hz = 0.5 * (truth.f_lo_hz + truth.f_hi_hz);
    let src = TempDir::new("t057-src");
    let blind = strip_truth(&meta, &src.0, "blind", 0.0).unwrap();
    // hackriffd's path for `--source sigmf:<file>` (scheduler-driven, unpaced for the test).
    let replay = open_mock_replay(&blind, Pacing::Unpaced, MockEnd::Stop).unwrap();
    let control = replay.source.mock_control();
    let (lo, hi) = {
        let b = replay.source.recording().band();
        (b.min_hz, b.max_hz)
    };
    let dir = TempDir::new("t057");
    let plan = replay_plan(
        replay.info.center_hz,
        replay.info.sample_rate_hz,
        replay.info.start_time,
    );
    let mut cfg = PipelineConfig::new(&dir.0, plan).unwrap();
    cfg.source_class = replay.class;
    cfg.lossless = true;
    cfg.drive_scheduler = true;
    cfg.device_id = replay.device.device_id.clone();
    let seen = Arc::new(Mutex::new(Truthfulness::default()));
    let source = TruthfulIq {
        inner: Box::new(replay.source),
        station_hz,
        blocks: 0,
        seen: Arc::clone(&seen),
    };
    let handle = Pipeline::start(
        cfg,
        Box::new(source),
        replay.info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let summary = bounded_finish(handle);
    let stats = control.mock_stats();
    let seen = seen.lock().unwrap();
    eprintln!(
        "scheduler steps {}, mock {stats:?}, centres {:?}, {} blocks checked, {} partial/noise blocks",
        summary.counter("/scheduler/steps"),
        seen.centres,
        seen.checked,
        seen.partial_or_noise_blocks
    );
    assert!(
        summary.counter("/scheduler/steps") > 1,
        "the scheduler drove the device"
    );
    assert!(
        seen.centres.len() > 1 && stats.control_changes > 0,
        "the scheduler retuned the mock device: {:?}",
        seen.centres
    );
    assert!(seen.checked > 0, "no block held the station");
    assert!(
        seen.failures.is_empty(),
        "headers disagree with the IQ served: {:?}",
        &seen.failures[..seen.failures.len().min(5)]
    );

    let (dets, emitters) = station_hits(&dir, &truth);
    assert!(dets > 0, "the station was not detected at 101.3 MHz");
    assert!(!emitters.is_empty(), "no emitter at the station");
    for (f, fm) in &emitters {
        assert!(
            (f - station_hz).abs() <= CENTER_TOL_HZ,
            "emitter misplaced at {f} Hz"
        );
        assert!(*fm, "emitter at {f} Hz lacks FM broadcast in its top-k");
    }
    // Nothing FM-wide in the noise-filled spectrum outside the recording: the mock serves no
    // recorded content beyond its coverage. T-175: boxes from windows served quantisation-limited
    // (provenance `quantisation_limited`, floor within 3 dB of the ADC rounding, docs/07) cannot
    // speak to that. At LNA 24 / VGA 20, 29 dB under the recording's gain, the IQ is ≈ 85 % zero
    // codes; one 128-bin frame's power then spreads far wider than the detector's Gaussian floor
    // model, so single-frame whole-window boxes appear on pure noise fill, and the detector marks
    // every box of such a segment `marginal`. The check stays on every window the scheduler served
    // with a measurable floor (the recording's own gain here), where it caught the edge folding.
    // The skip is guarded below: the skipped boxes back no inventory emitter or candidate, and no
    // occupancy row outside the recording holds occupied time (a failure there is a product bug).
    let repo = repo(&dir.0);
    let mut skipped: Vec<hk_model::DetectionId> = Vec::new();
    let phantoms: Vec<(f64, f64)> = repo
        .detections_in_region(&Region::new(FreqRange::new(0.0, 7.0e9), ever()))
        .unwrap()
        .into_iter()
        .filter(|d| (d.f_center_hz < lo || d.f_center_hz > hi) && d.obw_hz > 20e3)
        .filter(|d| {
            let ql = repo
                .provenance(d.provenance_ref)
                .unwrap()
                .quantisation_limited;
            if ql {
                skipped.push(d.id);
            }
            !ql
        })
        .map(|d| (d.f_center_hz, d.obw_hz))
        .collect();
    // The skip is only sound while the product does not learn from those boxes: none of them backs
    // an inventory emitter (confirmed or candidate), through a track or directly.
    let mut learned = Vec::new();
    for e in inventory(&repo, InventoryQuery::default()) {
        for link in repo.emitter_links(e.emitter.id).unwrap() {
            let dets = match link.target {
                LinkTarget::Track(t) => repo.track_detections(t).unwrap(),
                LinkTarget::Detection(d) => vec![d],
                _ => Vec::new(),
            };
            if dets.iter().any(|d| skipped.contains(d)) {
                learned.push((e.emitter.f_center_hz, e.emitter.bandwidth_hz));
            }
        }
    }
    // Nor does noise fill outside the recording add occupied time to any occupancy row (FCO counts
    // level crossings over the floor; detections only mark crossings suspect).
    let store =
        OccupancyStore::open(dir.0.join("occupancy"), OccupancyStoreConfig::default()).unwrap();
    let mut rows = 0usize;
    let mut occupied = Vec::new();
    for interval in [SeriesInterval::Min15, SeriesInterval::Hour1] {
        let found = store
            .query(&OccupancyQuery {
                freq: FreqRange::new(0.0, 7.0e9),
                span: ever(),
                interval,
                subject: Some(SubjectKind::Band),
                f_cell_hz: 1_000.0,
                limit: 100_000,
            })
            .unwrap();
        for r in found.rows {
            rows += 1;
            if let OccupancySubject::Band { freq } = r.subject
                && (freq.hi_hz <= lo || freq.lo_hz >= hi)
                && r.n_occupied > 0
            {
                occupied.push((freq.lo_hz, freq.hi_hz, r.n_occupied, r.n_suspect));
            }
        }
    }
    eprintln!(
        "{} out-of-band boxes from quantisation-limited windows; {rows} occupancy band rows",
        skipped.len()
    );
    assert!(
        learned.is_empty(),
        "quantisation-limited out-of-band boxes back inventory emitters: {learned:?}"
    );
    assert!(
        occupied.is_empty(),
        "occupied time outside the recorded band (lo, hi, n_occupied, n_suspect): {occupied:?}"
    );
    assert!(
        phantoms.is_empty(),
        "detections outside the recorded band {lo}..{hi} Hz: {phantoms:?}"
    );
}
