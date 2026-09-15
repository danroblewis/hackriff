//! T-130: the detection `clipped` flag through the device and ring path. The mock SDR serves a
//! 433.92 MHz tone recording (peak ≈ 46 of 127, no `hackriff:provenance`, so its capture gains
//! are unknown) and the pipeline detects blind; the flag is asserted on the stored detections.
//!
//! - **Working gain (the scheduler's default, LNA 24 / VGA 20):** no sample reaches full scale, so
//!   no detection is clipped. Before T-130 the unknown capture gain read as 0 dB, the working gain
//!   scaled the IQ ×158 and every detection was flagged.
//! - **+12 dB (≈ 4× amplitude, LNA 32 / VGA 24):** the tone saturates the 8-bit output, the
//!   device reports clipped components and overload, and every detection at the tone is clipped.

mod common;

use common::*;
use hk_core::{Gains, MockOptions, MockSdrDriver, Pacing, SourceControl};
use hk_model::{Detection, FreqRange, Region, TimeRange, Timestamp};
use hk_pipeline::{Pipeline, PipelineConfig, SourceInfo, TrackInventory, open_mock_replay};

const FS: f64 = 2e6;
const CENTER: f64 = 433.92e6;
/// `tone_recording`'s tone offset.
const EMITTER: f64 = CENTER + 50e3;

fn detections(dir: &std::path::Path) -> Vec<Detection> {
    let ever = TimeRange::new(
        Timestamp::from_unix_nanos(0),
        Timestamp::from_unix_nanos(i64::MAX / 2),
    );
    repo(dir)
        .detections_in_region(&Region::new(FreqRange::new(0.0, 7e9), ever))
        .unwrap()
}

/// Replays the tone through the mock at `gains` (applied through the device control, as the
/// scheduler does); returns the stored detections and the device's clipped component count.
fn run_at(tag: &str, gains: Gains) -> (Vec<Detection>, u64, TempDir) {
    let dir = TempDir::new(tag);
    let rec = tone_recording(&dir.0.join("src"), "tone", FS, 3.0, CENTER, None);
    // The class, rate and start come from the pipeline's own mock opener.
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
    let control = driver.last_control().unwrap();
    control.set_gains(&gains).unwrap();
    let info = SourceInfo {
        sample_rate_hz: FS,
        center_hz: CENTER,
        start_time: source.start_time(),
    };
    let plan = hk_pipeline::replay_plan(CENTER, FS, info.start_time);
    let mut cfg = PipelineConfig::new(dir.0.join("data"), plan).unwrap();
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
    let (summary, stopped) = wait_guarded(handle, std::time::Duration::from_secs(120));
    assert!(!stopped, "the run finished on its own");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    let clipped = control.mock_stats().clipped_components;
    let dets = detections(&dir.0.join("data"));
    (dets, clipped, dir)
}

fn at_tone(d: &[Detection]) -> Vec<&Detection> {
    d.iter()
        .filter(|d| (d.f_center_hz - EMITTER).abs() < 20e3)
        .collect()
}

#[test]
fn working_gain_on_an_unrecorded_gain_tone_is_unclipped_and_4x_overdrive_is_clipped() {
    let (dets, clipped, _dir) = run_at(
        "clip-working",
        Gains {
            lna_db: 24.0,
            vga_db: 20.0,
            amp_on: false,
        },
    );
    let tone = at_tone(&dets);
    let flagged = dets.iter().filter(|d| d.flags.clipped).count();
    eprintln!(
        "working gain: {} detections ({} at the tone), {flagged} clipped, {clipped} clipped \
         components",
        dets.len(),
        tone.len()
    );
    assert_eq!(
        clipped, 0,
        "no sample reaches full scale at the working gain"
    );
    assert!(!tone.is_empty(), "the tone is detected blind");
    assert_eq!(flagged, 0, "an unclipped replay has no clipped detection");

    let (dets, clipped, _dir) = run_at(
        "clip-overdrive",
        Gains {
            lna_db: 32.0,
            vga_db: 24.0,
            amp_on: false,
        },
    );
    let tone = at_tone(&dets);
    let flagged = tone.iter().filter(|d| d.flags.clipped).count();
    eprintln!(
        "+12 dB: {} detections ({} at the tone), {flagged} at the tone clipped, {clipped} clipped \
         components",
        dets.len(),
        tone.len()
    );
    assert!(clipped > 0, "the ≈4× tone saturates the 8-bit output");
    assert!(!tone.is_empty(), "the overdriven tone is detected");
    assert_eq!(
        flagged,
        tone.len(),
        "every detection of the saturated tone is clipped"
    );
}
