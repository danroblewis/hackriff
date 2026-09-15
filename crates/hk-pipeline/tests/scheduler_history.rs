//! T-139 (AWARE-042, AWARE-044, AWARE-027) through the mock SDR: a scheduler-driven run with the
//! **default scheduler settings** (sweep hops of 50 ms, bandit on as T-124 runs it) over a
//! time-compressed occupancy scene folds history rows from its short steps, so the attention and
//! memory chain is fed: OccupancyStat rows, baseline folds and novelty-alarm inputs. Before T-139
//! the same run folded no history frame at all (every hop was shorter than one history row and
//! each retune discarded the partial row).
//!
//! The fixed-tune companion replays the same scene without the scheduler: it never retunes, so the
//! history reader emits no partial row (its frames, and hence its tiles, are the pre-T-139 ones;
//! the frame-level bit identity is `hk-dsp` `partial_frames_never_armed_are_bit_identical`).
//!
//! Blind: the scene's truth is stripped and never read.

mod common;

use common::*;
use hk_core::{MockEnd, Pacing};
use hk_e2e::{SynthOutput, SynthRequest};
use hk_model::ScanPolicy;
use hk_pipeline::{Pipeline, PipelineConfig, TrackInventory, open_mock_replay, replay_plan};
use serde_json::{Value, json};

const T139: &str = "T-139";
const SAMPLE_RATE: f64 = 500e3;
/// Simulated span, h: eight 15-min occupancy intervals.
const SPAN_H: f64 = 2.0;
const REVISIT_MEAN_GAP_S: f64 = 60.0;
/// T-124's window: 65 536 samples (0.13 s), a little over one history row at the scene's rate.
const WINDOW_S: f64 = (4 * 16_384) as f64 / SAMPLE_RATE;

fn scene() -> Option<SynthOutput> {
    let request = SynthRequest::new("occupancy_markov_scene")
        .seed(7)
        .param("span_hours", SPAN_H)
        .param("sample_rate", SAMPLE_RATE)
        .param("iq_windows_at_revisits", "true")
        .param("revisit_mean_gap_s", REVISIT_MEAN_GAP_S)
        .param("window_duration_s", WINDOW_S);
    match request.generate() {
        Ok(out) => Some(out),
        Err(err) if err.is_unavailable() && !hk_e2e::synth::require_synth() => {
            eprintln!("SKIP {}: {err}", module_path!());
            None
        }
        Err(err) => panic!("synthetic scenario generation failed: {err}"),
    }
}

struct Outcome {
    counters: Value,
    rows_written: u64,
    intervals_closed: u64,
    alarms: Value,
}

fn run_scene(out: &SynthOutput, scheduler: bool) -> Outcome {
    let dir = TempDir::new(if scheduler {
        "t139-sched"
    } else {
        "t139-fixed"
    });
    let src = TempDir::new("t139-src");
    let rec = hk_e2e::scene::join_scene_windows(out, &src.0, "scene").unwrap();
    let blind = blind_meta(&rec.meta, &src.0);
    let replay = open_mock_replay(&blind, Pacing::Unpaced, MockEnd::Stop).unwrap();
    let mut plan = replay_plan(
        replay.info.center_hz,
        replay.info.sample_rate_hz,
        replay.info.start_time,
    );
    plan.policy = ScanPolicy::SweepThenDwell;
    // T-124's run: scheduler defaults (no `extra.scheduler`), the bandit on.
    plan.extra = json!({
        "bandit": { "min_dwell_s": 0.5, "max_dwell_s": 2.0, "sweep_floor_window_s": 10.0 }
    });
    let mut cfg = PipelineConfig::new(dir.0.join("data"), plan).unwrap();
    cfg.source_class = replay.class;
    cfg.lossless = true;
    cfg.drive_scheduler = scheduler;
    cfg.device_id = replay.device.device_id.clone();
    let handle = Pipeline::start(
        cfg,
        Box::new(replay.source),
        replay.info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let occ = handle.occupancy();
    let alarms = handle.alarms().expect("the run's alarm service");
    let summary = handle.wait().unwrap();
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    let stats = occ.stats();
    let alarms = alarms.status_json();
    let tag = if scheduler { "scheduler" } else { "fixed" };
    eprintln!(
        "[{T139}] {tag}: history reader {}; history {}; scheduler {}; attention {}; occupancy \
         {stats:?}; alarms {alarms}",
        summary.counters["readers"]["history"],
        summary.counters["history"],
        summary.counters["scheduler"],
        summary.counters["attention"],
    );
    Outcome {
        counters: summary.counters,
        rows_written: stats.rows_written,
        intervals_closed: stats.intervals_closed,
        alarms,
    }
}

fn n(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

#[test]
fn scheduler_default_settings_feed_history_occupancy_baselines_and_alarms() {
    let Some(out) = scene() else { return };
    let r = run_scene(&out, true);
    let c = &r.counters;
    let reader = &c["readers"]["history"];
    assert!(
        n(&c["scheduler"]["sweep_steps"]) > 0,
        "[{T139}] the scheduler swept"
    );
    assert!(
        n(&reader["partial_frames"]) > 0 && n(&reader["frames"]) >= n(&reader["partial_frames"]),
        "[{T139}] scheduler steps leave history rows: {reader}"
    );
    assert!(
        n(&c["history"]["frames_ingested"]) > 0 && n(&c["history"]["tiles_written"]) > 0,
        "[{T139}] rows fold into tiles: {}",
        c["history"]
    );
    assert!(
        r.intervals_closed > 0 && r.rows_written > 0,
        "[{T139}] OccupancyStat rows: {} intervals, {} rows",
        r.intervals_closed,
        r.rows_written
    );
    assert!(
        n(&c["attention"]["folds"]) > 0,
        "[{T139}] baselines folded: {}",
        c["attention"]
    );
    assert!(
        n(&r.alarms["inputs_observed"]) > 0,
        "[{T139}] alarm inputs: {}",
        r.alarms
    );
}

#[test]
fn scheduler_history_fixed_tune_emits_no_partial_rows() {
    let Some(out) = scene() else { return };
    let r = run_scene(&out, false);
    let reader = &r.counters["readers"]["history"];
    assert!(n(&reader["frames"]) > 0, "[{T139}] {reader}");
    assert_eq!(
        n(&reader["partial_frames"]),
        0,
        "[{T139}] a stream that never retunes folds whole rows only: {reader}"
    );
}
