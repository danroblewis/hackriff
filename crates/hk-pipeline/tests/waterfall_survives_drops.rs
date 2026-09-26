//! T-939 — **a front end that drops samples must not empty the waterfall.**
//!
//! Reported from a live 20 Msps run: the history reader lost **nothing** of its own
//! (`readers.history.lost_samples` 0) and the waterfall still showed mostly empty rows. The two
//! facts only fit together one way. A history row is `K` segments of *unbroken* samples — 0.1 s at
//! the default 10 rows/s — and every gap resets the averaging; the front end was dropping USB
//! transfers more often than that (54 M samples in four minutes, in bursts), so the reset arrived
//! before `K` segments ever completed and this reader folded **no row at all**. Nothing was lost by
//! the pyramid, the ring or the reader: the rows were simply never made, over spectrum the radio
//! really did sample. "We have it but didn't render it" — in its purest form, because the samples
//! existed at every stage and the product of them did not.
//!
//! T-139 already had the right idea for a different cause (a scheduler hop shorter than a row):
//! emit the averaging in progress as a **partial** row, with its own `n_avg` and `sample_count`, so
//! the row is honest about how much it averaged. It was armed on `RETUNE`/`RATE_CHANGE` only.
//! T-939 arms it on `GAP` as well, which is the same argument T-915 makes for the last segments of
//! a stream: samples that were captured and whose time is observed must not leave the surface
//! blank.
//!
//! **Deterministic, and in the gate.** The drops fall on a block count, not on the wall clock
//! ([`radio::RadioControl::drop_every`]), and the run is a lossless replay, so the frame counts
//! here are a function of the stream alone. On the code before the fix this test reads
//! `frames == 0`.

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use hk_model::Timestamp;
use hk_pipeline::class::window_class;
use hk_pipeline::history::history_stft_config;
use hk_pipeline::{Pipeline, PipelineConfig, SourceInfo, TrackInventory, replay_plan};
use serde_json::json;

const CENTER: f64 = 100.8e6;
const FS: f64 = 2e6;
const BLOCK: usize = 16_384;
/// Blocks between drops: 4 × 16 384 = 65 536 samples, 33 ms at 2 Msps — far more often than the
/// 0.1 s a full row needs, and far less often than the ~10 ms a partial row needs.
const DROP_EVERY: u64 = 4;
/// Samples the front end throws away each time (a quarter of a transfer).
const DROP_SAMPLES: u64 = 4_096;
/// Blocks of stream: ~0.8 s, eight row periods.
const BLOCKS: u64 = 100;
const LIMIT: Duration = Duration::from_secs(120);

#[test]
fn the_waterfall_still_folds_rows_when_the_front_end_drops_samples() {
    let dir = TempDir::new("waterfall-drops");
    let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
    let mut plan = replay_plan(CENTER, FS, t0);
    plan.extra = json!({ "pipeline": { "ring_s": 1.0 } });
    let mut cfg = PipelineConfig::new(&dir.0, plan).unwrap();
    cfg.source_class = window_class(CENTER, FS);
    cfg.live_window_class = true;
    cfg.lossless = true;
    cfg.settings.chains = Some(Vec::new());

    // The geometry this test depends on, stated rather than assumed: a drop interval that sits
    // between the partial-row minimum and a full row. Outside that window the test would prove
    // nothing (too short: no row at all is owed; too long: full rows complete anyway).
    let fft = hk_pipeline::config::detection_resolution(FS, &cfg.settings).0;
    let scfg = history_stft_config(FS, fft, cfg.settings.history_rows_per_s);
    let row_samples = (scfg.averages * scfg.welch.hop()) as u64;
    let min_samples =
        (hk_pipeline::history::partial_min_segments(scfg.averages) * scfg.welch.hop()) as u64;
    let between_drops = DROP_EVERY * BLOCK as u64;
    assert!(
        min_samples < between_drops && between_drops < row_samples,
        "drops every {between_drops} samples must fall between the partial minimum \
         ({min_samples}) and a full row ({row_samples})"
    );

    let (rx, ctl) = radio::Radio::new(CENTER, FS, BLOCK, radio::tone(|_| 300e3));
    ctl.drop_every(DROP_EVERY, DROP_SAMPLES);
    let total = BLOCKS * BLOCK as u64;
    ctl.hold_at(total);
    let handle = Pipeline::start(
        cfg,
        Box::new(rx),
        SourceInfo {
            sample_rate_hz: FS,
            center_hz: CENTER,
            start_time: t0,
        },
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let counters = handle.counters();
    assert!(
        ctl.wait_emitted(total, LIMIT),
        "the radio never delivered the stream"
    );
    handle.stop();
    let (summary, watchdog) = wait_guarded(handle, LIMIT);
    assert!(!watchdog, "the run had to be stopped by the watchdog");

    let h = &summary.counters["readers"]["history"];
    let frames = h["frames"].as_u64().unwrap();
    let partial = h["partial_frames"].as_u64().unwrap();
    let resets = h["stft_resets"].as_u64().unwrap();
    let lost = h["lost_samples"].as_u64().unwrap();
    let gaps = h["gap_samples"].as_u64().unwrap();
    let dropped = counters.source.source_dropped.load(Ordering::Relaxed);

    // The premise: the front end really did drop, the reader really did see the gaps, and it
    // really did lose nothing of its own.
    assert!(
        dropped >= (BLOCKS / DROP_EVERY - 2) * DROP_SAMPLES,
        "the front end was supposed to drop {DROP_SAMPLES} samples every {DROP_EVERY} blocks: \
         {dropped} dropped, {h}"
    );
    assert_eq!(lost, 0, "the history reader lost ring samples: {h}");
    assert_eq!(
        gaps, dropped,
        "every dropped sample is a gap this reader passed: {h}"
    );
    assert!(resets >= BLOCKS / DROP_EVERY - 2, "one reset per gap: {h}");

    // The fix: a row per drop interval instead of none at all.
    let want = BLOCKS / DROP_EVERY - 3;
    assert!(
        frames >= want,
        "the waterfall went empty: {frames} rows folded from {BLOCKS} blocks broken by a gap \
         every {DROP_EVERY}. A row needs {row_samples} unbroken samples and the stream never \
         offers that many, so without T-939's GAP-armed partial row this is 0 — and the surface \
         is blank over spectrum the radio sampled. {h}"
    );
    assert_eq!(
        partial, frames,
        "every row here is a partial one (no {row_samples}-sample stretch is unbroken): {h}"
    );
    // And what it does NOT do: a stream this broken must not be reported as if it were whole.
    // Each row carries the segments it averaged, which is what keeps the pyramid's noise shape
    // honest; the count above is the claim, `n_avg` on the frame is the qualification.
    assert!(
        frames < BLOCKS * BLOCK as u64 / row_samples + BLOCKS,
        "a partial row per gap, not a row per block: {h}"
    );
}
