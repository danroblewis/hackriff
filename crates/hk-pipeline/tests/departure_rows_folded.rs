//! T-915 — **a band's rows run all the way to the retune that left it.**
//!
//! Found by T-911 on the unified canvas: tune away from a band and the top of its waterfall — the
//! time just before the retune — read as T-441's dotted `AWAITING` ("observed, nothing folded")
//! for ever, never filling in. The coverage map was right that the radio had looked there; the
//! IQ ring held the samples; the view lattice's finest node held no row for them. Two defects,
//! both at the end of a stream, and a retune ends one:
//!
//! 1. **The averaging in progress was discarded.** The display STFT (T-484: the view lattice's
//!    node (0, 0)) emits a row every `K` segments. A re-plumbing retune ends the segment's ring
//!    and the reader's stream with it; `flush` published only rows already complete, so the
//!    samples since the last full row — up to a whole row period — never became a row. A retune
//!    *in place* resets the STFT instead, with the same loss. Now the stream's end emits that
//!    averaging as a partial row ([`hk_dsp::StftProcessor::finish`]) and a retune in place emits it
//!    as T-139's partial frame, both with their true `n_avg`.
//! 2. **Rows pushed after the view writer ended were lost.** The history reader ends the view
//!    writer when *its* ring read closes; the spectrum reader, which produces the rows, drains the
//!    same ring at its own pace and could still be pushing. A lagging producer's backlog — the
//!    departed band's newest rows — went into a queue nobody read again. The writer now waits for
//!    its producer ([`hk_pipeline`]'s `ViewProducer`).
//!
//! **What is asserted**, through a scripted receiver behind the generic device contract (the retune
//! is a real device action): the radio delivers band A for **30¾ display rows** and holds, then
//! retunes. Every row those samples make — 30 full rows and the ¾ one — is in the view lattice's
//! finest node over band A, counted in folded frames per cell ([`hk_store::CellStats::frames`],
//! the number `/api/tiles` serves as `grid.frames` and the client reads `AWAITING` from). On the
//! code before the fix the count is 30. Both retune kinds are covered: a content-class change,
//! which re-plumbs the run, and a retune within the class, which tunes in place.
//!
//! Cost (T-453): nothing per arriving row. Each fix is one frame per stream end or retune, on the
//! reader's own thread, and one condition-variable wait at a segment end.

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use hk_model::{FreqRange, TimeRange, Timestamp};
use hk_pipeline::class::{row_plan, window_class};
use hk_pipeline::config::DisplaySettings;
use hk_pipeline::{Pipeline, PipelineConfig, SourceInfo, TrackInventory, replay_plan};
use hk_store::{Pyramid, RegionQuery, Resolution};
use serde_json::json;

/// FM broadcast: `unrestricted`.
const CENTER_A: f64 = 100.8e6;
/// 433 MHz ISM: `metadata-only`, so a retune here changes the class and re-plumbs the run.
const CENTER_REPLUMB: f64 = 433.92e6;
/// Still FM broadcast, and clear of band A's window: a retune here is tuned in place.
const CENTER_IN_PLACE: f64 = 101.6e6;
const FS: f64 = 500e3;
const OFFSET_HZ: f64 = 80e3;
const BLOCK: usize = 16_384;
const RING_S: f64 = 0.5;
/// Full display rows band A is recorded for before the retune.
const FULL_ROWS: u64 = 30;
const LIMIT: Duration = Duration::from_secs(120);

/// Frames the view lattice's finest node holds in one frequency cell of band A, over the capture
/// time `[T0, until_ns)`, and the newest cell holding any.
///
/// One cell of frequency is enough: every display row folds into every cell of its span, so the
/// count per column is the number of rows. It is read clear of the tone and of the DC notch.
fn band_a_rows(view: &Arc<Mutex<Pyramid>>, until_ns: i64) -> (u64, Option<i64>) {
    let p = view.lock().unwrap();
    let h = p
        .query(&RegionQuery {
            freq: FreqRange::centered(CENTER_A - 100e3, 1.0),
            time: TimeRange::new(
                Timestamp::from_unix_nanos(radio::T0_NS),
                Timestamp::from_unix_nanos(until_ns),
            ),
            resolution: Resolution::Level(0),
        })
        .expect("the view pyramid answered the query");
    let mut rows = 0u64;
    let mut newest = None;
    for t in 0..h.nt {
        let n: u32 = h.row(t).iter().map(|c| c.frames).max().unwrap_or(0);
        if n > 0 {
            rows += u64::from(n);
            newest = Some((h.t_first_cell + t as i64 + 1) * h.t_cell_ns);
        }
    }
    (rows, newest)
}

fn departure_folds_every_row(to: f64, expect_replumb: bool) {
    let dir = TempDir::new(if expect_replumb {
        "departure-rows-replumb"
    } else {
        "departure-rows-in-place"
    });
    let class = window_class(CENTER_A, FS);
    let mut plan = replay_plan(CENTER_A, FS, Timestamp::from_unix_nanos(radio::T0_NS));
    plan.extra = json!({ "pipeline": { "ring_s": RING_S } });
    let mut cfg = PipelineConfig::new(&dir.0, plan).unwrap();
    cfg.source_class = class;
    cfg.live_window_class = true;
    cfg.lossless = true;
    cfg.settings.chains = Some(Vec::new());
    assert!(
        cfg.settings.view_history,
        "the view lattice is on by default"
    );

    // The display plan the run will use, so the ¾ row is ¾ of ITS row.
    let d = DisplaySettings::from_settings(&cfg.settings);
    let rp = row_plan(FS, d.fft_size, d.rows_per_s, class, d.window);
    let (k, hop, n) = (rp.stft.averages, rp.stft.welch.hop(), rp.stft.welch.fft_len);
    let row = (k * hop) as u64;
    let tail = 3 * row / 4;
    let on_a = FULL_ROWS * row + tail;
    // The ¾ row averages this many segments, which must clear T-139's minimum for the fix to owe
    // a row at all — otherwise this test would prove nothing.
    let tail_segments = (tail as usize - n) / hop + 1;
    assert!(
        tail_segments >= k.div_ceil(10) && tail_segments < k,
        "the tail is a partial row: {tail_segments} of {k} segments"
    );

    let (rx, ctl) = radio::Radio::new(CENTER_A, FS, BLOCK, radio::tone(|_| OFFSET_HZ));
    ctl.hold_at(on_a);
    let handle = Pipeline::start(
        cfg,
        Box::new(rx),
        SourceInfo {
            sample_rate_hz: FS,
            center_hz: CENTER_A,
            start_time: Timestamp::from_unix_nanos(radio::T0_NS),
        },
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let controller = handle.controller();
    let counters = handle.counters();
    let view = handle
        .view_history()
        .expect("the pipeline opens the view lattice");

    // ---- band A, exactly 30¾ rows of it, then the retune ----
    assert!(
        ctl.wait_emitted(on_a, LIMIT),
        "the radio never delivered band A"
    );
    let a_end_ns = radio::T0_NS + (on_a as f64 * 1e9 / FS).round() as i64;
    let out = controller
        .retune(to, FS)
        .expect("the retune is a device action the run accepts");
    assert_eq!(
        out.replumbed,
        expect_replumb,
        "this retune must {} (else it exercises the other path)",
        if expect_replumb {
            "re-plumb"
        } else {
            "tune in place"
        }
    );
    // Band B runs a while, so the run has plainly moved on past the departure — and, for the
    // in-place retune, so the first block of B (whose RETUNE flag ends A's averaging) arrives.
    ctl.hold_at(on_a + 40 * row);
    assert!(
        ctl.wait_emitted(on_a + 40 * row, LIMIT),
        "the radio never delivered band B"
    );

    // ---- every row band A's samples make is folded, the ¾ one included ----
    //
    // Ingest is asynchronous, so wait (in wall clock) for the count to arrive; on the defect it
    // never does, and the wait ends at the deadline with the count it stopped at.
    let want = FULL_ROWS + 1;
    let until = a_end_ns + 5 * 1_000_000_000;
    let deadline = Instant::now() + Duration::from_secs(30);
    let (rows, newest) = loop {
        let (rows, newest) = band_a_rows(&view, until);
        if rows >= want || Instant::now() > deadline {
            break (rows, newest);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let h = &counters.history;
    assert_eq!(
        rows,
        want,
        "band A was recorded for {FULL_ROWS}¾ display rows ({on_a} samples, ending at {a_end_ns}) \
         and the view lattice's finest node holds {rows} of its {want} rows: the samples since \
         the last full row before the retune were never folded (view frames {}, dropped {}, late \
         {}, rejected {})",
        h.view_frames.load(Ordering::Relaxed),
        h.view_dropped.load(Ordering::Relaxed),
        h.view_late.load(Ordering::Relaxed),
        h.view_rejected.load(Ordering::Relaxed),
    );
    // The newest of them is the departure's own row: it ends within one row of the last sample.
    let t_cell_ns = (1e9 / rp.row_rate_hz).round() as i64;
    let newest = newest.expect("band A has rows");
    assert!(
        newest > a_end_ns - t_cell_ns / 2 && newest <= a_end_ns + t_cell_ns,
        "band A's newest row ends at {newest}, the band's last sample at {a_end_ns} (cell {t_cell_ns} ns)"
    );
    assert_eq!(
        h.view_dropped.load(Ordering::Relaxed),
        0,
        "no view row may be dropped"
    );

    ctl.finish();
    let (summary, fired) = wait_guarded(handle, LIMIT);
    assert!(!fired, "the run had to be stopped by the watchdog");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
}

/// A retune that changes the content class re-plumbs the run: band A's segment, its ring and
/// every reader's stream end at the departure.
#[test]
fn a_replumbing_retune_folds_the_departed_bands_last_row() {
    departure_folds_every_row(CENTER_REPLUMB, true);
}

/// A retune within the class is applied in place: the stream continues, and the RETUNE flag on
/// band B's first block resets the display STFT mid-row.
#[test]
fn an_in_place_retune_folds_the_departed_bands_last_row() {
    departure_folds_every_row(CENTER_IN_PLACE, false);
}
