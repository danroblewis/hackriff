//! T-304: history frames take their source key from each block's own provenance, not once per
//! run from `PipelineConfig::device_id`.
//!
//! Two halves, because either alone is satisfiable by a broken implementation:
//! - **Property:** a replay whose segments carry different `hackriff:provenance.device_id`
//!   values lands its frames under two distinct history source keys, end to end through the
//!   feeder (`crates/hk-pipeline/src/history.rs`) — not just in the store, which T-259 already
//!   found correct and tested (`hk-store/src/history/tests/followups.rs:206`).
//! - **Control:** a single-device replay still produces exactly one source key, unchanged from
//!   today. Without this half, "every block gets its own key regardless of the run" would also
//!   pass the property half.
//!
//! Blind: this test asserts on provenance/source bookkeeping only, never on detected content.

mod common;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use common::*;
use hk_core::Pacing;
use hk_dsp::radiometry::PowerCalibrations;
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use hk_model::{ClockSource, FreqRange, Provenance, TimeRange, TimestampMethod, Tune};
use hk_store::history::{Origin, source_key};
use hk_store::{FloorProduct, FloorProductConfig, RegionQuery, Resolution};

const FS: f64 = 500e3;
const SECS_PER_SEGMENT: f64 = 3.0;

fn provenance_for(device_id: &str, center_hz: f64) -> Provenance {
    Provenance {
        device_id: device_id.into(),
        tune: Tune {
            center_hz,
            sample_rate_hz: FS,
            lna_db: 24.0,
            vga_db: 20.0,
            amp_on: false,
            bandwidth_hz: FS,
        },
        overload: false,
        quantisation_limited: false,
        temperature_c: None,
        antenna_port: None,
        bias_tee: hk_model::BiasTee::Unknown,
        clock_source: ClockSource::Internal,
        clock_locked: true,
        calibration_state_ref: None,
        spur_mask_ref: None,
        timestamp_method: TimestampMethod::Synthetic,
        timestamp_error_budget_ns: None,
    }
}

/// Writes a `.sigmf-meta`/`.sigmf-data` pair whose captures are `segments` back to back
/// (`(center_hz, device_id)` per segment, each `SECS_PER_SEGMENT` long), each with its own
/// explicit `hackriff:provenance` so the replay source never falls back to a synthesised one.
fn multi_device_recording(dir: &Path, name: &str, segments: &[(f64, &str)]) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let n_per = (SECS_PER_SEGMENT * FS) as usize;
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut noise = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        ((state >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 12.0
    };
    let mut data = Vec::with_capacity(2 * n_per * segments.len());
    for _ in 0..(n_per * segments.len()) {
        let re = noise().round().clamp(-128.0, 127.0) as i8;
        let im = noise().round().clamp(-128.0, 127.0) as i8;
        data.push(re as u8);
        data.push(im as u8);
    }
    std::fs::write(dir.join(format!("{name}.sigmf-data")), data).unwrap();
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(FS);
    for (i, &(center_hz, device_id)) in segments.iter().enumerate() {
        meta.captures.push(Capture {
            sample_start: (i * n_per) as u64,
            frequency: Some(center_hz),
            datetime: Some(format!(
                "2026-09-16T12:00:{:02}Z",
                (i as f64 * SECS_PER_SEGMENT) as u64
            )),
            provenance: Some(provenance_for(device_id, center_hz)),
            clip_count: None,
            extra: Default::default(),
        });
    }
    let path = dir.join(format!("{name}.sigmf-meta"));
    meta.write(&path).unwrap();
    path
}

/// Runs `meta` once (unpaced, no scheduler) and returns the distinct history source keys seen
/// (device -> frame count) across the whole recorded region, from the uncalibrated pyramid the
/// feeder folds into (no calibration state is configured in this test).
fn history_sources(
    dir: &Path,
    meta: &Path,
    lo_hz: f64,
    hi_hz: f64,
    n_segments: usize,
) -> (Vec<(Origin, u64)>, u64) {
    let (cfg, replay) = replay_config(dir, meta, serde_json::json!({}), Pacing::Unpaced);
    let start_time = replay.info.start_time;
    let (summary, stopped) = wait_guarded(start(cfg, replay), std::time::Duration::from_secs(120));
    assert!(!stopped, "the run finished on its own");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);

    let product = FloorProduct::open(
        dir.join("history"),
        FloorProductConfig {
            mixed_shapes: true,
            ..FloorProductConfig::default()
        },
        PowerCalibrations::new(),
    )
    .unwrap();
    let span_ns = (n_segments as f64 * SECS_PER_SEGMENT * 1e9) as i64 + 1_000_000_000;
    let h = product
        .uncalibrated_pyramid()
        .query(&RegionQuery {
            freq: FreqRange::new(lo_hz, hi_hz),
            time: TimeRange::new(
                start_time.saturating_add_nanos(-1_000_000_000),
                start_time.saturating_add_nanos(span_ns),
            ),
            resolution: Resolution::Level(0),
        })
        .unwrap();
    (h.provenance.origins, h.provenance.other_origin_frames)
}

/// Property: two segments of the same replay, carrying different `hackriff:provenance.device_id`
/// (and a retune between them, so the STFT resets cleanly at the boundary instead of blending),
/// land under two distinct history source keys — derived from each frame's own provenance, not
/// the run's single `PipelineConfig::device_id` (which here is `PipelineConfig::new`'s default
/// `"sigmf-replay"`, matching neither device).
#[test]
fn blocks_from_different_provenance_devices_land_under_different_source_keys() {
    const DEV_A: &str = "hackrf:0000000000000000a06063c8234e925f";
    const DEV_B: &str = "rtl-sdr:00000001";
    const C1: f64 = 433.0e6;
    const C2: f64 = 441.0e6;

    let src = TempDir::new("hist-src-two");
    let meta = multi_device_recording(&src.0, "two-device", &[(C1, DEV_A), (C2, DEV_B)]);
    let dir = TempDir::new("hist-two");
    let (origins, other) =
        history_sources(&dir.0, &meta, C1 - FS / 2.0 - 1e3, C2 + FS / 2.0 + 1e3, 2);
    eprintln!("origins: {origins:?}, other_origin_frames: {other}");

    let sources: HashSet<u64> = origins.iter().filter_map(|(o, _)| o.source).collect();
    let key_a = source_key(DEV_A);
    let key_b = source_key(DEV_B);
    assert_ne!(key_a, key_b, "distinct devices must hash to distinct keys");
    assert_eq!(
        sources,
        HashSet::from([key_a, key_b]),
        "both devices' blocks must be found under their own source key"
    );
    for (want, tag) in [(key_a, "A"), (key_b, "B")] {
        let frames: u64 = origins
            .iter()
            .filter(|(o, _)| o.source == Some(want))
            .map(|(_, n)| n)
            .sum();
        assert!(frames > 0, "device {tag} contributed no folded frames");
    }
    assert_eq!(other, 0, "only two origins were ever folded, no overflow");
}

/// Control: a single-device replay (one provenance device_id throughout, even though it still
/// differs from `PipelineConfig::device_id`) still yields exactly one source key — the fix must
/// not turn "one source" into "one key per block" regardless of device.
#[test]
fn a_single_device_replay_still_yields_exactly_one_source_key() {
    const DEV: &str = "hackrf:0000000000000000a06063c8234e925f";
    const C1: f64 = 433.0e6;

    let src = TempDir::new("hist-src-one");
    // Two back-to-back segments, same device and same tuning throughout: a realistic single-
    // source recording, not a single capture (so the control exercises the same segment-boundary
    // machinery as the property test, just with one device on both sides of it).
    let meta = multi_device_recording(&src.0, "one-device", &[(C1, DEV), (C1, DEV)]);
    let dir = TempDir::new("hist-one");
    let (origins, other) =
        history_sources(&dir.0, &meta, C1 - FS / 2.0 - 1e3, C1 + FS / 2.0 + 1e3, 2);
    eprintln!("origins: {origins:?}, other_origin_frames: {other}");

    let sources: HashSet<u64> = origins.iter().filter_map(|(o, _)| o.source).collect();
    assert_eq!(
        sources,
        HashSet::from([source_key(DEV)]),
        "a single-device run must fold under exactly one source key"
    );
    assert_eq!(other, 0);
}
