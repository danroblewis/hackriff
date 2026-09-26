//! T-965: **a survey pass must characterise what it finds.**
//!
//! The explorer ran the shipped `Scan everything (fast)` one-click against the live HackRF in San
//! Francisco (418 steps × 0.3 s at 19.2 Msps) and got an inventory that cannot be read: **zero**
//! rows across 88–108 MHz, where 19 FM stations are the loudest thing on the air, and 146 rows
//! elsewhere with **every one** of them carrying `snr_db: null` and `family: null`. A row with no
//! level is not a measurement — it is a box the user cannot rank, explain or act on, and the whole
//! point of the survey is to say *what is out there and how strong it was*.
//!
//! This test is that claim, through the SDR device interface, over a synthetic scene the pipeline
//! has never been told about:
//!
//! 1. **Every truth emitter in the scene gets a row.** The scene's tones are its private truth
//!    (the blind rule): the scan walks the band, detection finds energy, and each tone must end up
//!    inside some row's band. Nothing here looks a frequency up.
//! 2. **Every row carries a measured SNR.** `Repository::emitter_latest_measurement` is exactly
//!    what `/api/inventory`'s `snr_db`/`peak_dbfs`/`measured` read (T-158/T-350), so asserting it
//!    here asserts what the user sees. A row with none is the defect.
//!
//! Why a level can be missing at all, and why that is a bug and not a cost of a short dwell: the
//! SNR of a row is a **detection's** number, and a detection is one frame's measurement. It needs
//! no chain, no demodulation and no second visit. A dwell too short for the characterisation
//! chains may honestly leave `family` null (the survey says so in the row's `resolution`), but it
//! can never excuse a row with no level: the energy that created the row was measured to create it.

mod common;

use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::*;
use hk_core::{MockEnd, Pacing};
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use hk_model::{FreqRange, InventoryQuery, TimeRange, Timestamp};
use hk_pipeline::{Pipeline, PipelineConfig, TrackInventory, open_mock_replay};

/// The scene's recorded rate and centre: 4 MHz of band around 100 MHz, so one recording covers
/// every step of the walk and the mock shifts/resamples each window out of it (`hk_core::source::
/// mock::dsp` is an honest radio: absolute frequencies are preserved).
const REC_FS: f64 = 2e6;
const REC_CENTER_HZ: f64 = 100e6;
/// Seconds of scene. Looped by the mock, so the pass sees it again on every step.
const REC_SECS: f64 = 1.0;

/// The scan's own window rate: 1 MHz, so a step covers 750 kHz of usable span and the walk below
/// is several steps wide — the shape of a real pass, at a size a test can afford.
const SCAN_FS: f64 = 1e6;
/// The band the scan walks (inside the recording, with a window's margin either side).
const LO_HZ: f64 = 99.1e6;
const HI_HZ: f64 = 100.9e6;

/// The **fast-scan dwell** the shipped one-click uses (`ui/src/controls/model.ts`
/// `FAST_SCAN_DWELL_S`). This is the dwell the defect was measured at, so it is the dwell the test
/// runs: a fix that only works at 15 s would not fix what the user pressed.
const DWELL_S: f64 = 0.3;

/// One emitter of the scene: what it is, and what it must be measured as.
struct Emission {
    /// Centre frequency, Hz.
    f_hz: f64,
    /// Peak deviation, Hz — `0.0` for an unmodulated carrier.
    deviation_hz: f64,
    /// Modulating tone, Hz (ignored when `deviation_hz` is zero).
    modulator_hz: f64,
}

impl Emission {
    /// Carson's-rule bandwidth: what a detector measuring this emission should report, Hz.
    /// `2 × (deviation + modulator)` for an FM carrier; one bin's worth for a bare tone.
    fn bandwidth_hz(&self) -> f64 {
        if self.deviation_hz <= 0.0 {
            0.0
        } else {
            2.0 * (self.deviation_hz + self.modulator_hz)
        }
    }
}

/// The scene's private truth. Two bare carriers and **two wide FM emissions**, because the wide
/// ones are the case the live pass lost: 88-108 MHz is 19 broadcast stations ~180 kHz wide and it
/// produced **zero** rows, while the 146 rows the pass did produce elsewhere were "mostly 8-35 kHz
/// slivers". A bare tone *is* a sliver, so a scene of tones alone cannot tell a working survey
/// from the one the explorer measured; a 152 kHz FM carrier can.
const TRUTH: [Emission; 4] = [
    Emission {
        f_hz: 99.28e6,
        deviation_hz: 0.0,
        modulator_hz: 0.0,
    },
    Emission {
        f_hz: 99.68e6,
        deviation_hz: 75e3,
        modulator_hz: 1e3,
    },
    Emission {
        f_hz: 100.08e6,
        deviation_hz: 0.0,
        modulator_hz: 0.0,
    },
    Emission {
        f_hz: 100.45e6,
        deviation_hz: 75e3,
        modulator_hz: 1e3,
    },
];

/// Amplitude of each emission in ci8 counts (the noise is +/-6 counts peak), so every one of them
/// is tens of dB over the floor: the FM-band case, where "the strongest thing on the air" produced
/// no rows at all.
const TONE_AMPLITUDE: f64 = 34.0;

/// Writes the multi-band scene as a ci8 SigMF recording: [`TRUTH_HZ`] as continuous tones in
/// noise, at [`REC_FS`] around [`REC_CENTER_HZ`].
fn scene_recording(dir: &Path, name: &str) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let n = (REC_SECS * REC_FS) as usize;
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut noise = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        ((state >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 12.0
    };
    let mut data = Vec::with_capacity(2 * n);
    for i in 0..n {
        let t = i as f64 / REC_FS;
        let (mut re, mut im) = (noise(), noise());
        for (k, e) in TRUTH.iter().enumerate() {
            let off = e.f_hz - REC_CENTER_HZ;
            // A per-emission phase so the sum never adds coherently at t = 0 and clips.
            let mut ph = std::f64::consts::TAU * (off * t) + k as f64 * 0.7;
            if e.deviation_hz > 0.0 {
                // Sinusoidal FM: peak deviation `deviation_hz` at `modulator_hz`, so the emission
                // occupies Carson's 2 x (deviation + modulator) rather than one bin.
                ph += (e.deviation_hz / e.modulator_hz)
                    * (std::f64::consts::TAU * e.modulator_hz * t).sin();
            }
            re += TONE_AMPLITUDE * ph.cos();
            im += TONE_AMPLITUDE * ph.sin();
        }
        data.push(re.round().clamp(-128.0, 127.0) as i8 as u8);
        data.push(im.round().clamp(-128.0, 127.0) as i8 as u8);
    }
    std::fs::write(dir.join(format!("{name}.sigmf-data")), data).unwrap();
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(REC_FS);
    meta.captures.push(Capture {
        sample_start: 0,
        frequency: Some(REC_CENTER_HZ),
        datetime: Some("2026-09-25T12:00:00Z".into()),
        provenance: None,
        clip_count: None,
        extra: serde_json::Map::new(),
    });
    let path = dir.join(format!("{name}.sigmf-meta"));
    meta.write(&path).unwrap();
    path
}

#[test]
fn a_scan_pass_finds_every_truth_emitter_and_measures_its_snr() {
    let dir = TempDir::new("t965-scan");
    let rec = scene_recording(&dir.0.join("src"), "scene");
    let replay = open_mock_replay(&rec, Pacing::Unpaced, MockEnd::Loop).unwrap();

    let scan = hk_core::scheduler::IterativeScan::from_seconds(DWELL_S).unwrap();
    let mut plan = scan.plan_over(
        "iterative scan",
        FreqRange::new(LO_HZ, HI_HZ),
        replay.info.start_time,
    );
    plan.extra["scheduler"]["sweep_rate_hz"] = serde_json::json!(SCAN_FS);

    let data_dir = dir.0.join("data");
    let mut cfg = PipelineConfig::new(&data_dir, plan).unwrap();
    cfg.source_class = replay.class;
    cfg.drive_scheduler = true;
    cfg.device_id = replay.device.device_id.clone();
    let handle = Pipeline::start(
        cfg,
        Box::new(replay.source),
        replay.info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let counters = handle.counters();

    // Three whole passes of stream time: every step in the range gets several turns.
    let steps_per_pass = ((HI_HZ - LO_HZ) / (SCAN_FS * 0.75)).ceil() as i64;
    let want_ns = 3 * steps_per_pass * (DWELL_S * 1e9) as i64;
    let deadline = Instant::now() + Duration::from_secs(180);
    let tick = |what: &str, f: &dyn Fn() -> bool| {
        while !f() {
            assert!(Instant::now() < deadline, "timed out: {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    tick("the capture thread starts", &|| {
        counters.stream_time_ns.load(Ordering::Relaxed) > 0
    });
    let s0 = counters.stream_time_ns.load(Ordering::Relaxed);
    tick("three passes of stream time", &|| {
        counters.stream_time_ns.load(Ordering::Relaxed) >= s0 + want_ns
    });
    let reached = Instant::now();
    handle.stop();
    // The bound is a DEADLOCK GUARD, not a latency assertion: it exists so a run that wedges on
    // stop (the T-941 class of defect, which `!stopped` below still catches) fails the test instead
    // of hanging the suite. It is deliberately generous because the number is not what is under
    // test — measured alone this run stops in 1.5-3 s, and under a load average of 33 on this box
    // it took 33 s, so a 90 s bound was inside the range a busy box produces (the sibling
    // `iterative_scan_device`'s 60 s bound fails the same way under the same load, on main).
    let (summary, stopped) = wait_guarded(handle, Duration::from_secs(240));
    eprintln!(
        "[T-965] {:.1} s to three passes, {:.1} s to stop{}",
        (reached - (deadline - Duration::from_secs(180))).as_secs_f64(),
        reached.elapsed().as_secs_f64(),
        if stopped { " (WATCHDOG)" } else { "" },
    );
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);

    let repo = repo(&data_dir);
    let rows = inventory(&repo, InventoryQuery::default());
    eprintln!(
        "[T-965] {} inventory rows from {} steps, {} detections, {} tracks opened",
        rows.len(),
        summary.counter("/scheduler/steps"),
        summary.counter("/detect/detections_written"),
        summary.counter("/detect/tracks_opened"),
    );
    for r in &rows {
        let m = repo.emitter_latest_measurement(r.emitter.id).unwrap();
        eprintln!(
            "[T-965]   {:.4} MHz bw {:.1} kHz state {:?} family {:?} snr {:?}",
            r.emitter.f_center_hz / 1e6,
            r.emitter.bandwidth_hz / 1e3,
            r.lifecycle,
            r.family,
            m.map(|m| (m.snr_peak_db * 10.0).round() / 10.0),
        );
    }

    // 1. Every truth emitter has a row whose band contains it — the blind rule: the scene's
    //    frequencies are its private truth and nothing in the run was told them.
    let row_of = |e: &Emission| {
        rows.iter().find(|r| {
            let half = (r.emitter.bandwidth_hz.max(0.0) / 2.0).max(e.bandwidth_hz() / 2.0 + 30e3);
            (r.emitter.f_center_hz - e.f_hz).abs() <= half
        })
    };
    let missing: Vec<f64> = TRUTH
        .iter()
        .filter(|e| row_of(e).is_none())
        .map(|e| e.f_hz / 1e6)
        .collect();
    assert!(
        missing.is_empty(),
        "the pass found no row for {:?} MHz of {} truth emitters ({} rows listed)",
        missing,
        TRUTH.len(),
        rows.len(),
    );

    // 1b. And the row measures the emission's WIDTH, not a sliver of it. A 152 kHz FM carrier
    //     reported as a 10 kHz row is the "8-35 kHz slivers" the live pass filled up with: the
    //     bandwidth is half of what a detection is for (centre AND width, ADR-0017), and a survey
    //     that reports a fragment of every wide emission cannot rank, explain or decode it.
    let narrow: Vec<String> = TRUTH
        .iter()
        .filter(|e| e.bandwidth_hz() > 0.0)
        .filter_map(|e| {
            let r = row_of(e)?;
            // Half the truth width is the floor: generous (a detector measures a threshold
            // crossing, not Carson's rule) and still an order of magnitude above a sliver.
            (r.emitter.bandwidth_hz < 0.5 * e.bandwidth_hz()).then(|| {
                format!(
                    "{:.3} MHz measured {:.1} kHz wide, emission is {:.1} kHz",
                    e.f_hz / 1e6,
                    r.emitter.bandwidth_hz / 1e3,
                    e.bandwidth_hz() / 1e3,
                )
            })
        })
        .collect();
    assert!(
        narrow.is_empty(),
        "the pass reported a sliver instead of the emission: {narrow:?}",
    );

    // 2. Every row carries a measured SNR — the number `/api/inventory` serves as `snr_db`.
    let unmeasured: Vec<f64> = rows
        .iter()
        .filter(|r| {
            repo.emitter_latest_measurement(r.emitter.id)
                .unwrap()
                .is_none()
        })
        .map(|r| r.emitter.f_center_hz / 1e6)
        .collect();
    assert!(
        unmeasured.is_empty(),
        "{} of {} rows carry no measured SNR (snr_db: null at {:?} MHz)",
        unmeasured.len(),
        rows.len(),
        unmeasured,
    );

    // 3. Each step's own record says how much of its planned dwell it actually heard, so a pass can
    //    be priced against measurement rather than intention (T-965's budget half; the per-step
    //    retune cost itself is wall-clock and is measured in `hk-api`). A step that recorded only
    //    its *planned* length would leave nothing to compare a stated budget against.
    {
        use hk_core::scheduler::is_scan_step;
        use hk_model::attention::observation::ObservationRecord;
        use hk_store::observation::{
            MAX_RECORD_LIMIT, ObservationLogConfig, ObservationStore, RecordQuery,
        };
        let store =
            ObservationStore::open(ObservationLogConfig::new(data_dir.join("observations")))
                .unwrap();
        let page = store.query(&RecordQuery {
            freq: FreqRange::new(LO_HZ - 2.0 * REC_FS, HI_HZ + 2.0 * REC_FS),
            span: TimeRange::new(
                Timestamp::from_unix_nanos(0),
                Timestamp::from_unix_nanos(i64::MAX / 2),
            ),
            tier: None,
            cursor: 0,
            limit: MAX_RECORD_LIMIT,
        });
        let steps: Vec<_> = page
            .records
            .iter()
            .filter_map(|r| match r {
                ObservationRecord::Dwell(d) if is_scan_step(d) => Some(d),
                _ => None,
            })
            .collect();
        assert!(!steps.is_empty(), "the pass wrote no scan-step records");
        for d in &steps {
            let planned = d.planned.duration_ns();
            let observed = d.observed.duration_ns();
            assert!(
                observed > 0 && observed <= planned,
                "a step's realised dwell must be a measurement inside its plan: seq {} \
                 planned {planned} ns observed {observed} ns",
                d.seq,
            );
        }
        let unheard: i64 = steps
            .iter()
            .map(|d| d.planned.duration_ns() - d.observed.duration_ns())
            .sum();
        eprintln!(
            "[T-965] {} steps: {:.3} s planned, {:.3} s heard ({:.3} s of settle the dwells do \
             not pay for)",
            steps.len(),
            steps.iter().map(|d| d.planned.duration_ns()).sum::<i64>() as f64 * 1e-9,
            steps.iter().map(|d| d.observed.duration_ns()).sum::<i64>() as f64 * 1e-9,
            unheard as f64 * 1e-9,
        );
    }

    assert!(!stopped, "the run stopped when asked");
}
