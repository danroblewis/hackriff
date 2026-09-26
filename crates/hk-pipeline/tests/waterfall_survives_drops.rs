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
//! **T-1071 goes one step further: the gap no longer ends the row at all.** T-939's partial row
//! still needed `K / 10` unbroken segments, so a stream broken more often than THAT (a coarse scan
//! step at 19.2 Msps on a front end delivering a fraction of it) folded nothing again. The history
//! STFT now bridges a pure gap: the row averages the segments on both sides of it and closes at
//! its row period. So this stream folds one row per row period — not one per drop interval — and
//! resets only at its start.
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

/// What one lossy run folded.
struct Run {
    h: serde_json::Value,
    display: serde_json::Value,
    history: serde_json::Value,
    total: u64,
    dropped: u64,
    row_samples: u64,
    min_samples: u64,
    between_drops: u64,
}

/// Streams `BLOCKS` blocks of a held tune through the pipeline, the front end dropping
/// `DROP_SAMPLES` every `drop_every` blocks.
fn run_with_drops(name: &str, drop_every: u64) -> Run {
    let dir = TempDir::new(name);
    let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
    let mut plan = replay_plan(CENTER, FS, t0);
    plan.extra = json!({ "pipeline": { "ring_s": 1.0 } });
    let mut cfg = PipelineConfig::new(&dir.0, plan).unwrap();
    cfg.source_class = window_class(CENTER, FS);
    cfg.live_window_class = true;
    cfg.lossless = true;
    cfg.settings.chains = Some(Vec::new());

    let fft = hk_pipeline::config::detection_resolution(FS, &cfg.settings).0;
    let scfg = history_stft_config(FS, fft, cfg.settings.history_rows_per_s);
    let row_samples = (scfg.averages * scfg.welch.hop()) as u64;
    let min_samples =
        (hk_pipeline::history::partial_min_segments(scfg.averages) * scfg.welch.hop()) as u64;

    let (rx, ctl) = radio::Radio::new(CENTER, FS, BLOCK, radio::tone(|_| 300e3));
    ctl.drop_every(drop_every, DROP_SAMPLES);
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
    Run {
        h: summary.counters["readers"]["history"].clone(),
        display: summary.counters["readers"]["spectrum"].clone(),
        history: summary.counters["history"].clone(),
        total,
        dropped: counters.source.source_dropped.load(Ordering::Relaxed),
        row_samples,
        min_samples,
        between_drops: drop_every * BLOCK as u64,
    }
}

#[test]
fn the_waterfall_still_folds_rows_when_the_front_end_drops_samples() {
    let run = run_with_drops("waterfall-drops", DROP_EVERY);
    let h = &run.h;
    let (total, dropped, row_samples) = (run.total, run.dropped, run.row_samples);
    let (min_samples, between_drops) = (run.min_samples, run.between_drops);
    // The geometry this test depends on, stated rather than assumed: a drop interval that sits
    // between the partial-row minimum and a full row. Outside that window the test would prove
    // nothing (too short: T-939's partial row is not owed, which is the next test's case; too
    // long: full rows complete anyway).
    assert!(
        min_samples < between_drops && between_drops < row_samples,
        "drops every {between_drops} samples must fall between the partial minimum \
         ({min_samples}) and a full row ({row_samples})"
    );
    let frames = h["frames"].as_u64().unwrap();
    let partial = h["partial_frames"].as_u64().unwrap();
    let resets = h["stft_resets"].as_u64().unwrap();
    let lost = h["lost_samples"].as_u64().unwrap();
    let gaps = h["gap_samples"].as_u64().unwrap();

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
    let bridged = h["gaps_bridged"].as_u64().unwrap();
    assert!(
        bridged >= BLOCKS / DROP_EVERY - 2,
        "every gap on the held tune is bridged (T-1071): {h}"
    );
    assert!(
        resets <= 1,
        "a gap on a held tune no longer resets the averaging (T-1071), only the stream start: {h}"
    );

    // The fix: a row per row period of capture — the drops inside it cost segments, not the row.
    // The stream index runs over the drops (they are gaps in it), so this is the capture time.
    let capture = total;
    let want = capture / row_samples - 2;
    assert!(
        frames >= want,
        "the waterfall went empty: {frames} rows folded from {BLOCKS} blocks broken by a gap \
         every {DROP_EVERY}, where {capture} samples of capture hold {} row periods of \
         {row_samples}. A row needs {row_samples} unbroken samples and the stream never offers \
         that many, so without a gap-tolerant row this is 0 (T-939) — and the surface is blank \
         over spectrum the radio sampled. {h}",
        capture / row_samples
    );
    assert_eq!(
        partial, frames,
        "every row here closes short of K at its row period (each holds a gap): {h}"
    );
    // And what it does NOT do: a stream this broken must not be reported as if it were whole, nor
    // chopped into a row per gap. Each row carries the segments it averaged (`n_avg`), which is
    // what keeps the pyramid's noise shape honest; its span is one row period.
    assert!(
        frames <= capture / row_samples + 2,
        "a row per row period, not a row per gap: {h}"
    );
}

/// **T-1071: a front end that drops more often than even a partial row still fills the history.**
///
/// The case T-939's partial row could not reach: a gap every block (16 384 samples, 8 ms at
/// 2 Msps), shorter than the `K / 10` segments a partial row needs. It is the geometry of a coarse
/// scan step — `Scan everything (fast)` runs at 19.2 Msps, and on the mock the front end delivered
/// ~20 % of that in ~17 k-sample pieces — where the history reader folded **no row at all**, so the
/// spectrum-history pyramid (the survey-overview tier) held nothing of the swept range, and the
/// display stream froze with it (`view_frames` flat for the whole step). On the code before the
/// fix both `frames` below are 0.
#[test]
fn rows_still_reach_the_pyramid_when_gaps_come_faster_than_a_partial_row() {
    let run = run_with_drops("waterfall-drops-fast", 1);
    assert!(
        run.between_drops < run.min_samples,
        "drops every {} samples must come faster than a partial row's minimum ({})",
        run.between_drops,
        run.min_samples
    );
    let (h, d) = (&run.h, &run.display);
    assert_eq!(h["lost_samples"].as_u64().unwrap(), 0, "{h}");
    // One drop per block delivered, over a stream index that runs through them.
    let drops = run.dropped / DROP_SAMPLES;
    assert!(
        drops >= run.total / (BLOCK as u64 + DROP_SAMPLES) - 2,
        "the front end dropped {} samples: {h}",
        run.dropped
    );
    let capture = run.total;
    let periods = capture / run.row_samples;
    let frames = h["frames"].as_u64().unwrap();
    assert!(
        frames >= periods - 2 && frames <= periods + 2,
        "history rows: {frames} over {periods} row periods of {} samples, a gap every {} \
         (before T-1071: 0). {h}",
        run.row_samples,
        run.between_drops
    );
    assert!(h["gaps_bridged"].as_u64().unwrap() >= drops - 2, "{h}");
    // ... and they are IN the pyramid, not only counted by the reader.
    let ingested = run.history["frames_ingested"].as_u64().unwrap();
    assert!(
        ingested >= periods - 2,
        "the spectrum-history pyramid ingested {ingested} rows: {}",
        run.history
    );
    for k in [
        "frames_late",
        "frames_rejected",
        "view_late",
        "view_rejected",
    ] {
        assert_eq!(run.history[k].as_u64(), Some(0), "{k}: {}", run.history);
    }
    // The display stream (the live edge and the view lattice's finest node) keeps drawing too.
    let shown = d["frames"].as_u64().unwrap();
    assert!(
        shown >= 2 * periods,
        "display rows: {shown} over {periods} history row periods (the display's rows are \
         shorter): {d}"
    );
    assert!(d["gaps_bridged"].as_u64().unwrap() >= drops - 2, "{d}");
}
