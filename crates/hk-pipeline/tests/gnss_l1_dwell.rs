//! T-322: the C36 L1 dwell has a caller, and it reaches C30 as evidence rather than as a
//! detection (ADR-0018; SIGNAL-030, AWARE-002, AWARE-006).
//!
//! # The falsifiable pair
//!
//! T-274 built GPS L1 acquisition and deliberately left it unwired. A guard that would pass with
//! or without the wiring proves nothing — several of this project's earlier no-caller vacuums were
//! found precisely that way — so these two tests fail for **different** removals:
//!
//! - [`t322_c04_grants_an_l1_dwell_and_acquisition_consumes_it`] fails if the **wiring** is
//!   removed. Delete `SchedState::request_gnss_dwell` and `requested` stays 0; delete the `arm`
//!   in `SchedState::tick` and `granted` stays 0; delete the `hk-gnss` reader thread and
//!   `acquisitions` stays 0. It also pins the cadence: never more acquisitions than dwells C04
//!   granted.
//! - [`t322_a_degraded_dwell_reaches_c30_as_evidence_not_as_an_inventory_row`] fails if the
//!   **acquisition** is removed or stubbed. It asserts the planted satellites come back by PRN
//!   from a signal buried under the noise, which no stub returning "0 acquired" and no
//!   energy-domain shortcut can satisfy — L1 here is below the floor by construction. It then
//!   asserts the result lands on a **blindly opened** anomaly as `Cause::OwnHistory` evidence, and
//!   that it names neither a `Detection` nor an `Emitter`.
//!
//! # The scene
//!
//! One mock-SDR recording at 1575.42 MHz, 4 Msps, in two halves:
//!
//! - **4 s quiet:** three GPS C/A satellites (PRN 7, 11, 19) at about −20 dB SNR in the 4 MHz
//!   window — invisible to any energy detector, recoverable only by despreading.
//! - **4 s jammed:** the same satellites under a broadband floor 18 dB higher. The blind path sees
//!   the floor step (that is what a jammer looks like: *above* the floor); the GNSS dwell sees the
//!   constellation go.
//!
//! Everything runs through the mock SDR device, per the standing rule that end-to-end tests drive
//! the system through the device interface rather than feeding files into the pipeline.

mod common;

use std::collections::BTreeSet;
use std::sync::OnceLock;

use common::*;
use hk_core::{MockOptions, MockSdrDriver, Pacing};
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use hk_model::{AnomalyKind, Cause, Evidence, FreqRange, Region, TimeRange, Timestamp};
use hk_pipeline::{
    GnssCounts, GnssL1Measurement, Pipeline, PipelineConfig, SourceInfo, TrackInventory,
};

/// Recording centre: L1 itself, so the mock's retunes stay inside the recorded band.
const CENTER: f64 = 1_575_420_000.0;
const FS: f64 = 4.0e6;
/// Seconds of each half.
const HALF_S: f64 = 4.0;
/// The satellites planted in the scene. **Truth, hidden from the pipeline** — nothing in the
/// recording's metadata names them, and the acquisition searches the whole 32-PRN codebook.
const PLANTED: [u8; 3] = [7, 11, 19];
/// Their Doppler, Hz. Whole kilohertz so one code period tiles exactly.
const DOPPLER_HZ: [f64; 3] = [1_000.0, -2_000.0, 3_000.0];
/// Uniform noise half-width in the quiet half, LSB, and in the jammed half (+18 dB).
const QUIET_NOISE: f64 = 12.0;
const JAMMED_NOISE: f64 = 96.0;
/// Per-satellite amplitude, LSB.
const SV_AMPLITUDE: f64 = 1.0;

/// Writes the two-half L1 scene as `<dir>/l1.sigmf-{meta,data}` and returns the meta path.
///
/// The satellite sum is periodic over one 1 ms code period (every carrier offset is a whole
/// kilohertz), so one period is built and tiled; only the noise is drawn per sample.
fn l1_recording(dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let per_ms = (FS / 1000.0) as usize;
    let codes: Vec<hk_gnss::CaCode> = PLANTED
        .iter()
        .map(|p| hk_gnss::CaCode::new(*p).unwrap())
        .collect();
    // One code period of the satellite sum, at the recording's centre.
    let mut period = vec![(0.0f64, 0.0f64); per_ms];
    for (code, fd) in codes.iter().zip(DOPPLER_HZ) {
        let chips = code.chips();
        for (i, slot) in period.iter_mut().enumerate() {
            let t = i as f64 / FS;
            let chip = chips[((t * hk_gnss::CHIP_RATE_HZ) as usize) % hk_gnss::CODE_LENGTH];
            let ph = std::f64::consts::TAU * fd * t;
            let a = SV_AMPLITUDE * f64::from(chip);
            slot.0 += a * ph.cos();
            slot.1 += a * ph.sin();
        }
    }

    let n = (2.0 * HALF_S * FS) as usize;
    let switch = (HALF_S * FS) as usize;
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut uniform = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 40) as f64 / (1u64 << 24) as f64 - 0.5
    };
    let mut data = Vec::with_capacity(2 * n);
    for i in 0..n {
        let sigma = if i < switch {
            QUIET_NOISE
        } else {
            JAMMED_NOISE
        };
        let (sr, si) = period[i % per_ms];
        let re = (sr + 2.0 * sigma * uniform()).round().clamp(-128.0, 127.0) as i8;
        let im = (si + 2.0 * sigma * uniform()).round().clamp(-128.0, 127.0) as i8;
        data.push(re as u8);
        data.push(im as u8);
    }
    std::fs::write(dir.join("l1.sigmf-data"), data).unwrap();
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(FS);
    meta.captures.push(Capture {
        sample_start: 0,
        frequency: Some(CENTER),
        datetime: Some("2026-09-16T12:00:00Z".into()),
        provenance: None,
        clip_count: None,
        extra: serde_json::Map::new(),
    });
    let path = dir.join("l1.sigmf-meta");
    meta.write(&path).unwrap();
    path
}

struct Outcome {
    counts: GnssCounts,
    last: Option<GnssL1Measurement>,
    /// Every acquisition's PRN set, best-first, in order.
    acquired_prns: Vec<Vec<u8>>,
    /// Anomalies the **blind** path opened anywhere in L-band, with their explanations.
    anomalies: Vec<(AnomalyKind, Vec<hk_model::Explanation>)>,
    _dir: TempDir,
}

fn outcome() -> &'static Outcome {
    static ONCE: OnceLock<Outcome> = OnceLock::new();
    ONCE.get_or_init(run_scene)
}

fn run_scene() -> Outcome {
    let dir = TempDir::new("t322-gnss-l1");
    let rec = l1_recording(&dir.0.join("src"));
    let driver = MockSdrDriver::new(
        &rec,
        MockOptions {
            block_len: hk_pipeline::replay_block_len(FS),
            pacing: Pacing::Unpaced,
            end: hk_core::MockEnd::Stop,
            ..MockOptions::default()
        },
    )
    .unwrap();
    let source = driver.open_mock(&driver.default_request()).unwrap();
    let info = SourceInfo {
        sample_rate_hz: FS,
        center_hz: CENTER,
        start_time: source.start_time(),
    };
    let mut plan = hk_pipeline::replay_plan(CENTER, FS, info.start_time);
    // A dwell a second, long enough for the reader to take its window out of the ring. The
    // production defaults (one 0.5 s dwell a minute) would give one dwell in an 8 s scene.
    plan.extra = serde_json::json!({
        "gnss": {"every_s": 1.0, "dwell_s": 1.0, "window_s": 0.05}
    });
    let data = dir.0.join("data");
    let mut cfg = PipelineConfig::new(&data, plan).unwrap();
    cfg.lossless = true;
    // C04 only asks for the dwell when it is actually driving the radio.
    cfg.drive_scheduler = true;
    let handle = Pipeline::start(
        cfg,
        Box::new(source),
        info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let gnss = handle.gnss();
    let acquired_prns = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
    // Sample the service while the run proceeds: the last measurement alone would hide the quiet
    // half, and the quiet half is where the satellites are acquirable.
    let watcher = {
        let (g, seen, stop) = (
            std::sync::Arc::clone(&gnss),
            std::sync::Arc::clone(&acquired_prns),
            handle.stopper(),
        );
        std::thread::spawn(move || {
            let mut n = 0u64;
            while !stop.is_stopped() {
                let c = g.counts();
                if c.acquisitions > n {
                    n = c.acquisitions;
                    if let Some(m) = g.last() {
                        seen.lock().unwrap().push(m.acquired_prns);
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        })
    };
    let (summary, stopped) = wait_guarded(handle, std::time::Duration::from_secs(300));
    let _ = watcher.join();
    assert!(!stopped, "the run finished on its own");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);

    let counts = gnss.counts();
    let last = gnss.last();
    let repo = repo(&data);
    let ever = TimeRange::new(
        Timestamp::from_unix_nanos(0),
        Timestamp::from_unix_nanos(i64::MAX / 2),
    );
    let anomalies = repo
        .anomalies_in_region(&Region::new(FreqRange::new(1.5e9, 1.7e9), ever))
        .unwrap()
        .into_iter()
        .map(|a| {
            let e = repo.explanations_for_anomaly(a.id).unwrap();
            (a.kind, e)
        })
        .collect();
    let acquired_prns = acquired_prns.lock().unwrap().clone();
    eprintln!("T-322 counts: {counts:?}\nlast: {last:?}\nacquired: {acquired_prns:?}");
    Outcome {
        counts,
        last,
        acquired_prns,
        anomalies,
        _dir: dir,
    }
}

/// **Half one of the pair: the wiring.** C04 accepts a scheduled L1 dwell, grants it, and the
/// acquisition path consumes what it granted — nothing more often than that.
#[test]
fn t322_c04_grants_an_l1_dwell_and_acquisition_consumes_it() {
    let o = outcome();
    assert_eq!(
        o.counts.requested, 1,
        "C04 must accept exactly one standing L1 dwell request for a plan covering L1"
    );
    assert!(
        o.counts.granted >= 1,
        "the scheduler must actually issue L1 steps: {:?}",
        o.counts
    );
    assert!(
        o.counts.acquisitions >= 1,
        "an acquisition must run on a granted dwell: {:?}",
        o.counts
    );
    // The cadence control. An acquisition is reachable only through a dwell C04 granted, so this
    // can never track blocks, frames or detections however many of those the run produced.
    assert!(
        o.counts.acquisitions + o.counts.abstained <= o.counts.granted,
        "acquisition ran more often than C04 granted a dwell: {:?}",
        o.counts
    );
}

/// **Half two of the pair: the acquisition, and what it is allowed to become.**
///
/// The planted satellites sit below the noise floor, so recovering them by PRN is only possible
/// by despreading against the published codes — the ADR-0018 exception actually running. The
/// result then has to arrive at C30 as evidence on an anomaly the blind path opened, and must not
/// name a detection or an emitter.
#[test]
fn t322_a_degraded_dwell_reaches_c30_as_evidence_not_as_an_inventory_row() {
    let o = outcome();
    let m = o.last.as_ref().expect("a measurement");
    assert_eq!(m.svs_searched, 32, "the whole GPS L1 codebook is searched");
    assert_eq!(
        m.codebook, "gps-l1-ca@is-gps-200",
        "every known-code-led result records the code set that led it (ADR-0018)"
    );

    // The satellites come back, by PRN, from IQ in which no energy detector could find them.
    let planted: BTreeSet<u8> = PLANTED.into_iter().collect();
    let best = o
        .acquired_prns
        .iter()
        .max_by_key(|p| p.iter().filter(|x| planted.contains(x)).count())
        .expect("at least one acquisition");
    let found: BTreeSet<u8> = best.iter().copied().collect();
    assert!(
        planted.is_subset(&found),
        "acquisition must recover the planted satellites {planted:?}, got {found:?} \
         (all acquisitions: {:?})",
        o.acquired_prns
    );

    // The blind path found the jammed half on its own: a floor rise, which is what a jammer is.
    let rises: Vec<_> = o
        .anomalies
        .iter()
        .filter(|(k, _)| *k == AnomalyKind::NoiseFloorRise)
        .collect();
    assert!(
        !rises.is_empty(),
        "the blind floor tracker must open an L-band anomaly for the +18 dB step"
    );

    // ...and the GNSS measurement reached C30 as evidence on it.
    let ours: Vec<&hk_model::Explanation> = rises
        .iter()
        .flat_map(|(_, e)| e.iter())
        .filter(|e| e.rule_version == hk_context::gnss_service::RULE_VERSION)
        .collect();
    assert!(
        !ours.is_empty(),
        "the L1 dwell's verdict must reach C30 as an explanation of the blind anomaly; \
         counts {:?}, explanations on L-band anomalies: {:?}",
        o.counts,
        o.anomalies
    );
    let e = ours[0];
    assert!(
        matches!(e.cause, Cause::OwnHistory { .. }),
        "the device's own GNSS measurement is own-history, not an external event: {:?}",
        e.cause
    );
    let values: Vec<(&str, f64)> = e
        .evidence
        .iter()
        .filter_map(|x| match x {
            Evidence::Value { name, value } => Some((name.as_str(), *value)),
            _ => None,
        })
        .collect();
    assert!(
        values.iter().any(|(n, _)| *n == "gnss.l1.svs_acquired"),
        "the measurement itself must be carried as evidence: {values:?}"
    );
    assert!(
        values
            .iter()
            .any(|(n, v)| *n == "gnss.l1.power_rise_db" && *v >= 6.0),
        "the in-band power rise is what separates jamming from a blocked antenna: {values:?}"
    );

    // **The leak, closed at the second door.** A Gold code correlating may never produce an
    // inventory row, and the two shapes that would let it are these.
    for e in &ours {
        assert!(
            !matches!(e.cause, Cause::Emitter { .. }),
            "a known-code acquisition may never name an emitter as a cause"
        );
        assert!(
            !e.evidence
                .iter()
                .any(|x| matches!(x, Evidence::Detection { .. })),
            "a known-code acquisition may never be linked as a blind detection"
        );
    }
}
