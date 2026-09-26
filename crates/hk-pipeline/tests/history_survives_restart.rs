//! T-942 — **restarting on an existing data dir must keep recording spectrum history.**
//!
//! Found on staging by the explorer, 2026-09-25, on live HackRF in SF: after a restart on the
//! same `--data-dir`, `/api/status` read `history.frames_ingested 0` and `frames_late 2024` —
//! every spectrum-history frame of the new run refused as arriving behind the watermark — and
//! `/api/history` answered `observed_cells 0` for the live window. The view lattice was healthy
//! (`view_frames 5050`), so nothing in the capture path was wrong: the run was looking, the ring
//! held the samples, and the measurements were thrown away. The same store served
//! `/api/navigation` `time.latest_s` = an exact hour boundary **34 minutes in the future**, which
//! is why `/api/analysis/strongest` answered `found: false` for every range and the pager pane
//! drew 689.6 s short of the top: one root cause, two surfaces.
//!
//! **The mechanism** was the run-end seal reaching past the data. `hk_pipeline::history` sealed
//! through *the last frame plus an hour* to force partially-filled tiles shut; scheme 1's level 2
//! is a one-hour tile, so that wrote the tile of the hour the run died in, and left its block end
//! — the next hour boundary — on disk as the newest sealed time. `Pyramid::recover` then resumed
//! the watermark from the newest sealed block end *of every level*, and `Pyramid::ingest` answers
//! `Late` for every frame whose level-0 block ends at or before the watermark. The unit half of
//! this is `hk_store`'s `history::tests::restart`; this is the whole thing, driven through the
//! device contract, because the defect is a **handshake between two runs** and neither half of it
//! is visible in one.
//!
//! What it asserts, with a scripted receiver behind the generic device contract
//! (`tests/support/radio.rs`):
//!
//! 1. run 1, on an empty dir, records history for its centre (the control);
//! 2. run 2, on **the same dir**, resuming later in capture time as a restart does, records
//!    history for the same centre in *its own* capture window — the property the defect broke;
//! 3. run 2 counts **no** late frames — the mechanism itself, and the counter the explorer read;
//! 4. what the store says its newest recorded row is never runs ahead of the data, at the start
//!    of run 2 (before a frame of it has landed) or at the end — the future-timestamp half.
//!
//! Each history query is scoped to the capture time of the run that must have written it: the
//! defect is an hour wide in capture time and a scripted radio outruns an hour of stream time in
//! about a minute of wall clock, which is exactly how T-446's first draft passed against a broken
//! build.

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use hk_model::{FreqRange, TimeRange, Timestamp};
use hk_pipeline::class::window_class;
use hk_pipeline::{Pipeline, PipelineConfig, SourceInfo, TrackInventory, replay_plan};
use hk_store::{FloorProduct, RegionQuery, Resolution};
use serde_json::json;

const CENTER: f64 = 100.8e6;
const FS: f64 = 500e3;
const OFFSET_HZ: f64 = 80e3;
const BLOCK: usize = 16_384;
const RING_S: f64 = 0.5;
/// Samples of stream time each run delivers: 4 s at 500 kS/s, ~40 history frames at the default
/// 10 rows/s.
const RUN_SAMPLES: u64 = 2_000_000;
const RUN_NS: i64 = (RUN_SAMPLES as i64) * 1_000_000_000 / (FS as i64);
/// The radio is off between the runs. Long enough to be a real restart, short enough that run 2
/// stays inside the hour the old code poisoned.
const GAP_NS: i64 = 3_000_000_000;
const LIMIT: Duration = Duration::from_secs(180);

/// Observed level-0 cells the uncalibrated pyramid holds for `CENTER` over the capture-time
/// window `[from_ns, from_ns + RUN_NS)`. Level 0 deliberately: the defect is that nothing is
/// *written*, and a coarser tier could answer from an earlier run's fold.
fn observed_cells(product: &Arc<Mutex<FloorProduct>>, from_ns: i64) -> usize {
    let p = product.lock().unwrap();
    p.uncalibrated_pyramid()
        .query(&RegionQuery {
            freq: FreqRange::centered(CENTER, 0.5 * FS),
            time: TimeRange::new(
                Timestamp::from_unix_nanos(from_ns),
                Timestamp::from_unix_nanos(from_ns + RUN_NS),
            ),
            resolution: Resolution::Level(0),
        })
        .expect("the pyramid answered the query")
        .cells
        .iter()
        .filter(|c| c.observed())
        .count()
}

/// The newest frame end the uncalibrated pyramid claims, ns (`i64::MIN`: none).
fn latest_ns(product: &Arc<Mutex<FloorProduct>>) -> i64 {
    product
        .lock()
        .unwrap()
        .uncalibrated_pyramid()
        .latest_frame_end()
        .map_or(i64::MIN, |t| t.as_unix_nanos())
}

/// One run's readings.
struct Run {
    /// Observed level-0 cells in the run's own capture window.
    cells: usize,
    /// History frames the pyramid refused as late.
    late: u64,
    /// The newest frame end the store claimed **when this run opened it** — before this run had
    /// folded anything, so it is entirely the previous run's legacy.
    opened_latest: i64,
    /// The newest frame end the store claims at the end of the run.
    end_latest: i64,
    /// Capture time the radio actually reached (the run keeps streaming until it is told to
    /// stop, so this is not `t0 + RUN_NS`).
    end_capture: i64,
}

/// Runs one pipeline on `dir` from capture time `t0` until it has delivered [`RUN_SAMPLES`].
fn run(dir: &std::path::Path, t0_ns: i64, what: &str) -> Run {
    let (rx, ctl) = radio::Radio::new(CENTER, FS, BLOCK, radio::tone(|_| OFFSET_HZ));
    let rx = rx.starting_at(t0_ns);
    let t0 = Timestamp::from_unix_nanos(t0_ns);
    let mut plan = replay_plan(CENTER, FS, t0);
    plan.extra = json!({ "pipeline": { "ring_s": RING_S } });
    let mut cfg = PipelineConfig::new(dir, plan).unwrap();
    cfg.source_class = window_class(CENTER, FS);
    cfg.live_window_class = true;
    cfg.lossless = true;
    cfg.settings.chains = Some(Vec::new());
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
    let product = handle.floor_product();
    let opened_latest = latest_ns(&product);

    assert!(
        ctl.wait_emitted(RUN_SAMPLES, LIMIT),
        "{what} stopped delivering samples"
    );
    let deadline = Instant::now() + LIMIT;
    let cells = loop {
        let n = observed_cells(&product, t0_ns);
        if n > 0 {
            break n;
        }
        if Instant::now() >= deadline {
            break 0;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let late = counters.history.frames_late.load(Ordering::Relaxed);
    let ingested = counters.history.frames_ingested.load(Ordering::Relaxed);
    let end_latest = latest_ns(&product);
    assert!(
        cells > 0,
        "no spectrum history for {what} over its own {:.0} s of capture from {t0_ns} \
         (ingested {ingested}, LATE {late})",
        RUN_NS as f64 / 1e9,
    );

    ctl.finish();
    let (summary, fired) = wait_guarded(handle, LIMIT);
    assert!(!fired, "{what} had to be stopped by the watchdog");
    assert!(summary.errors.is_empty(), "{what}: {:?}", summary.errors);
    let end_capture = t0_ns + (ctl.emitted() as f64 / FS * 1e9) as i64;
    Run {
        cells,
        late,
        opened_latest,
        end_latest: end_latest.max(latest_ns(&product)),
        end_capture,
    }
}

#[test]
fn spectrum_history_keeps_recording_across_a_restart() {
    let dir = TempDir::new("history-survives-restart");
    let t0 = radio::T0_NS;

    // ---- 1. the control: a first run on an empty dir records at all ----
    let r1 = run(&dir.0, t0, "the first run");
    assert_eq!(r1.late, 0, "the first run counted {} late frames", r1.late);
    assert!(
        r1.end_latest <= r1.end_capture,
        "the first run's store claims a newest row ({}) past the data it folded (capture reached          {})",
        r1.end_latest,
        r1.end_capture
    );

    // ---- 2. the restart, on the same data dir, resuming later in capture time ----
    let t1 = r1.end_capture + GAP_NS;
    let r2 = run(&dir.0, t1, "the run AFTER a restart");

    // 4. what the store says it holds, before the restarted run has folded anything: never ahead
    //    of the data. The old code answered an exact hour boundary up to an hour in the future,
    //    and `/api/navigation` and `/api/analysis/strongest` served exactly that.
    assert!(
        r2.opened_latest <= r1.end_capture,
        "on reopening, the store claimed a newest recorded row at {} — {:.1} s PAST the last \
         frame the previous run could have folded ({})",
        r2.opened_latest,
        (r2.opened_latest - r1.end_capture) as f64 / 1e9,
        r1.end_capture,
    );
    assert!(
        r2.end_latest <= r2.end_capture,
        "the restarted run's store claims a newest row ({}) past its own data",
        r2.end_latest
    );

    // 3. the mechanism: a restarted run has no reason to reject a single frame.
    assert_eq!(
        r2.late, 0,
        "{} history frames of the restarted run were folded behind a watermark left by the run \
         before it",
        r2.late
    );
    assert!(
        r2.cells > 0 && r1.cells > 0,
        "both runs recorded ({} then {} level-0 cells)",
        r1.cells,
        r2.cells
    );
}
