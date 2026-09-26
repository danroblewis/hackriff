//! T-948: **the receiver's own DC point is never an inventory candidate, and a steady line beside
//! it still is.**
//!
//! The explorer (2026-09-25, live HackRF at 162.2 MHz / 2.4 Msps) found a candidate at the exact
//! tuned centre — 162.1998 MHz, 8.5 kHz wide — on *every* tune it made (144.600, 930.800,
//! 434.200 too), while the steady line it could see 216 kHz below the centre never became a
//! detection or an explanation. One scene, through the mock SDR, pins both halves:
//!
//! - **A DC term** (LO leakage / ADC offset: a constant added to every sample) at the tuned
//!   centre. It is the receiver's own artefact, the coverage map declares its notch *excluded from
//!   analysis* (T-595), and the detector's rule 2 says so on every detection there
//!   (`SpurReason::Dc`). It must therefore get **no inventory entry** — not a suppressed one, not a
//!   low-ranked one: a receiver artefact listed as a signal is a signal the operator has to
//!   disprove, and the explorer's DC point came with a band-plan explanation attached.
//! - **A steady CW line 200 kHz above the centre**, near enough to be the same kind of narrow,
//!   always-on line and far outside the notch. It is an emission: it **must** be listed. This is
//!   the guard on the rule above — the refusal is bounded by the artefact verdict, never by
//!   "narrow and always on", which is what a real CW beacon also looks like.
//!
//! **Nothing is dropped.** The DC detections are still stored, with their reason, so the line is
//! explained rather than silently missing: asserted below on the detection rows.
//!
//! Blind: the pipeline sees only the device. The two truth frequencies are the test's own.

mod common;

use common::*;
use hk_core::{MockOptions, MockSdrDriver, Pacing};
use hk_model::detection::SpurReason;
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use hk_model::{Detection, FreqRange, InventoryQuery, Region, TimeRange, Timestamp};
use hk_pipeline::{Pipeline, PipelineConfig, SourceInfo, TrackInventory, open_mock_replay};
use std::path::{Path, PathBuf};

const FS: f64 = 2e6;
const CENTER: f64 = 162.2e6;
/// The steady line beside the notch: the explorer's was 216 kHz below its centre.
const LINE_OFFSET_HZ: f64 = 200e3;
/// Truth: the emission's frequency.
const LINE_HZ: f64 = CENTER + LINE_OFFSET_HZ;
/// Around the tuned centre: the detector's DC tolerance (15 kHz) with room for the measured width.
const NOTCH_HALF_HZ: f64 = 30e3;
const SECONDS: f64 = 3.0;

/// A ci8 recording of a DC term plus one steady CW line at `+LINE_OFFSET_HZ`, in noise.
///
/// The DC term is what a real front end does: a constant complex offset, so its energy lands in
/// the centre bin of every frame whatever the tuning. The line is a pure tone, i.e. exactly the
/// CW shape a receiver line has too — which is the point of the pair.
fn dc_and_line(dir: &Path, name: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let n = (SECONDS * FS) as usize;
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut noise = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        ((state >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 12.0
    };
    let mut data = Vec::with_capacity(2 * n);
    for i in 0..n {
        let ph = 2.0 * std::f64::consts::PI * LINE_OFFSET_HZ * i as f64 / FS;
        let re = (30.0 + 40.0 * ph.cos() + noise())
            .round()
            .clamp(-128.0, 127.0) as i8;
        let im = (30.0 + 40.0 * ph.sin() + noise())
            .round()
            .clamp(-128.0, 127.0) as i8;
        data.push(re as u8);
        data.push(im as u8);
    }
    std::fs::write(dir.join(format!("{name}.sigmf-data")), data).unwrap();
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(FS);
    meta.captures.push(Capture {
        sample_start: 0,
        frequency: Some(CENTER),
        datetime: Some("2026-09-25T04:00:00Z".into()),
        provenance: None,
        clip_count: None,
        extra: serde_json::Map::new(),
    });
    let path = dir.join(format!("{name}.sigmf-meta"));
    meta.write(&path).unwrap();
    path
}

fn detections(dir: &Path) -> Vec<Detection> {
    let ever = TimeRange::new(
        Timestamp::from_unix_nanos(0),
        Timestamp::from_unix_nanos(i64::MAX / 2),
    );
    repo(dir)
        .detections_in_region(&Region::new(FreqRange::new(0.0, 7e9), ever))
        .unwrap()
}

#[test]
fn t948_the_dc_point_is_not_a_candidate_and_the_line_beside_it_is() {
    let dir = TempDir::new("t948-dc");
    let rec = dc_and_line(&dir.0.join("src"), "dc-and-line");
    let reference = open_mock_replay(&rec, Pacing::Unpaced, hk_core::MockEnd::Stop).unwrap();
    let driver = MockSdrDriver::new(
        &rec,
        MockOptions {
            block_len: hk_pipeline::replay_block_len(FS),
            pacing: Pacing::Unpaced,
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
    let plan = hk_pipeline::replay_plan(CENTER, FS, info.start_time);
    let data = dir.0.join("data");
    let mut cfg = PipelineConfig::new(&data, plan).unwrap();
    cfg.source_class = reference.class;
    cfg.lossless = true;
    drop(reference);
    let handle = Pipeline::start(
        cfg,
        Box::new(source),
        info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let (summary, stopped) = wait_guarded(handle, std::time::Duration::from_secs(180));
    assert!(!stopped, "the run finished on its own");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);

    // The honest record: the DC point *was* measured and *was* attributed to the receiver.
    let dets = detections(&data);
    let at_dc: Vec<&Detection> = dets
        .iter()
        .filter(|d| (d.f_center_hz - CENTER).abs() <= NOTCH_HALF_HZ)
        .collect();
    assert!(
        !at_dc.is_empty(),
        "the DC term produced no detection at all; the scene, not the rule, is wrong"
    );
    assert!(
        at_dc
            .iter()
            .all(|d| d.flags.spur_reason == Some(SpurReason::Dc)),
        "every detection at the tuned centre must carry the DC reason, so the line is explained \
         rather than silently missing: {:?}",
        at_dc
            .iter()
            .map(|d| (d.f_center_hz, d.flags.spur_reason))
            .collect::<Vec<_>>()
    );

    // The rule: no candidate, no confirmed entry, nothing at all inside the notch.
    let rows = inventory(&repo(&data), InventoryQuery::default());
    let listed = |lo: f64, hi: f64| -> Vec<(f64, f64)> {
        rows.iter()
            .map(|e| (e.emitter.f_center_hz, e.emitter.bandwidth_hz))
            .filter(|(f, _)| *f >= lo && *f <= hi)
            .collect()
    };
    let at_centre = listed(CENTER - NOTCH_HALF_HZ, CENTER + NOTCH_HALF_HZ);
    assert!(
        at_centre.is_empty(),
        "the DC point is listed as a signal at the tuned centre: {at_centre:?} (all rows: {:?})",
        rows.iter()
            .map(|e| (e.emitter.f_center_hz, e.emitter.bandwidth_hz))
            .collect::<Vec<_>>()
    );

    // The guard: the steady line beside the notch is an emission and is listed.
    let at_line = listed(LINE_HZ - 40e3, LINE_HZ + 40e3);
    assert!(
        !at_line.is_empty(),
        "the steady line at {:.3} MHz is an emission and must be listed; inventory: {:?}",
        LINE_HZ / 1e6,
        rows.iter()
            .map(|e| (e.emitter.f_center_hz, e.emitter.bandwidth_hz))
            .collect::<Vec<_>>()
    );
}
