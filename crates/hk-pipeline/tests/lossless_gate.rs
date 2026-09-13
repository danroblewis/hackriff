//! Lossless flow control (T-027 review fixes). Every run has a watchdog: a deadlocked capture
//! thread fails the test instead of hanging it.
//!
//! - A recording whose first sample index (`core:global_index`) is far beyond the ring's slack,
//!   replayed unpaced with a small ring, completes with no loss (the always-on readers' cursors
//!   start at 0).
//! - A lossless analog chain whose recording trigger lies more than `slack − post_s` (1.75 s with
//!   the default 4 s ring) into its window records and demodulates without stalling the capture
//!   thread.
//! - Lossless mode is off by default and refused for a source that cannot pause.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use hk_core::{BlockHeader, Pacing, Source, SourceCapabilities, SourceControl, SourceError};
use hk_pipeline::{
    Candidate, Pipeline, PipelineConfig, TrackInventory, builtin_chains, open_replay, replay_plan,
};
use num_complex::{Complex, Complex32};
use serde_json::json;

#[test]
fn unpaced_replay_with_a_huge_global_index_and_a_small_ring_completes_losslessly() {
    let src = TempDir::new("gate-src");
    let fs = 250e3;
    let secs = 2.0;
    let meta = tone_recording(&src.0, "huge-index", fs, secs, 433.5e6, Some(5_000_000_000));
    let dir = TempDir::new("gate");
    let (cfg, replay) = replay_config(
        &dir.0,
        &meta,
        json!({ "pipeline": { "ring_s": 0.5 } }),
        Pacing::Unpaced,
    );
    assert!(cfg.lossless);
    let (s, fired) = wait_guarded(start(cfg, replay), Duration::from_secs(180));
    eprintln!("{}", s.to_text());
    assert!(
        !fired,
        "the capture thread deadlocked; the watchdog stopped the run"
    );
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    let n = (secs * fs) as u64;
    assert_eq!(s.counter("/source/samples"), n);
    assert_eq!(s.counter("/source/ring_errors"), 0);
    assert_eq!(s.always_on_lost_samples, 0);
    for r in ["detect", "history", "spectrum"] {
        assert_eq!(s.counter(&format!("/readers/{r}/samples")), n, "{r}");
        assert_eq!(s.counter(&format!("/readers/{r}/lost_samples")), 0, "{r}");
    }
}

#[test]
fn lossless_analog_chain_with_a_late_trigger_records_without_stalling_capture() {
    let Some(meta) = real_fixture("fm_100p8M_2p4M_l32g30a1_t1p5_5s") else {
        return;
    };
    let dir = TempDir::new("gate-analog");
    // No registry chains: only the manually attached one runs.
    let (cfg, replay) = replay_config(
        &dir.0,
        &meta,
        json!({ "pipeline": { "chains": [] } }),
        Pacing::Unpaced,
    );
    assert!(cfg.lossless);
    let fs = replay.info.sample_rate_hz;
    let first = replay
        .meta
        .captures
        .first()
        .and_then(|c| c.extra.get("core:global_index"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let handle = start(cfg, replay);
    let spec = builtin_chains()
        .into_iter()
        .find(|s| s.id == "wfm-rds")
        .unwrap();
    // 2.5 s into the window: beyond slack (2 s) − post (0.25 s) behind the probe end.
    let trigger = first + (2.5 * fs) as u64;
    handle.attach_chain(
        spec,
        Candidate {
            track: None,
            detection: None,
            f_lo_hz: 101.2e6,
            f_hi_hz: 101.4e6,
            first_sample: first,
            trigger_sample: trigger,
            bursty: Some(false),
        },
    );
    let (s, fired) = wait_guarded(handle, Duration::from_secs(900));
    eprintln!("{}", s.to_text());
    assert!(
        !fired,
        "the capture thread deadlocked; the watchdog stopped the run"
    );
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    assert_eq!(s.always_on_lost_samples, 0);
    assert_eq!(s.counter("/chains/attached"), 1);
    assert_eq!(s.counter("/chains/mode_rejected"), 0, "101.3 MHz is WFM");
    assert_eq!(
        s.counter("/chains/recordings"),
        1,
        "the late-trigger recording"
    );
    assert_eq!(s.counter("/chains/recordings_incomplete"), 0);
    assert!(s.counter("/chains/demodulations") >= 1);
    assert_eq!(s.counter("/chains/lost_samples"), 0);
}

/// A replay that claims it cannot pause, like a live radio.
struct Unpausable(hk_core::SigmfReplaySource);

impl Source for Unpausable {
    fn capabilities(&self) -> &SourceCapabilities {
        self.0.capabilities()
    }

    fn control(&self) -> Arc<dyn SourceControl> {
        self.0.control()
    }

    fn read_block(
        &mut self,
        samples: &mut Vec<Complex32>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        self.0.read_block(samples)
    }

    fn read_block_ci8(
        &mut self,
        samples: &mut Vec<Complex<i8>>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        self.0.read_block_ci8(samples)
    }
}

#[test]
fn lossless_is_off_by_default_and_refused_for_a_source_that_cannot_pause() {
    let src = TempDir::new("gate-live-src");
    let meta = tone_recording(&src.0, "live", 250e3, 0.2, 433.5e6, None);
    let dir = TempDir::new("gate-live");
    let replay = open_replay(&meta, Pacing::Unpaced, false).unwrap();
    assert!(replay.source.pausable(), "a recording can pause");
    let info = replay.info;
    let plan = replay_plan(info.center_hz, info.sample_rate_hz, info.start_time);
    let cfg = PipelineConfig::new(&dir.0, plan).unwrap();
    assert!(!cfg.lossless, "lossless is opt-in");

    let mut lossless = cfg.clone();
    lossless.lossless = true;
    let err = Pipeline::start(
        lossless,
        Box::new(Unpausable(replay.source)),
        info,
        None,
        Box::new(TrackInventory::default()),
    )
    .err()
    .expect("a source that cannot pause is refused in lossless mode");
    assert!(err.to_string().contains("pause"), "{err}");

    // The same source runs with lossless off.
    let replay = open_replay(&meta, Pacing::Unpaced, false).unwrap();
    let handle = Pipeline::start(
        cfg,
        Box::new(Unpausable(replay.source)),
        info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let (s, fired) = wait_guarded(handle, Duration::from_secs(60));
    assert!(!fired, "the run did not finish; the watchdog stopped it");
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    assert_eq!(s.counter("/source/gate_waits"), 0);
    assert!(s.counter("/source/samples") > 0);
}
