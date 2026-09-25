//! T-439 — **"live" is the finest-level growing edge of the view lattice, not a separate mode.**
//!
//! `docs/16` §8.1's leap, made testable: the live view is a *viewport* onto one surface, so the
//! live chain must write the finest node of the de-welded view scheme as capture proceeds. T-438
//! left the addressing without a store behind it — `PyramidConfig::view_lattice` existed and
//! nothing in `hk_pipeline` opened one, so the coarse addresses folded out of scheme 1's welded
//! ladder, whose off-diagonal nodes do not exist at all. This drives a scripted receiver behind the
//! generic device contract and asserts the other half:
//!
//! 1. **The lattice is open and de-welded.** Node (0, 0) is the **display plan's own bin and row**
//!    (T-484's `view_geometry`; T-437's finding F1 was that `docs/16` §6.2's 128 s floor swallowed
//!    the whole 120 s IQ retention in one cell and re-welded the axis the lattice exists to
//!    separate, and this is that argument taken to its end — the floor is the cadence of the
//!    measurements, not a number chosen for the axis) — and `(level_f 0, level_t 3)` is a real node,
//!    which is precisely the address scheme 1 must answer `404` for.
//! 2. **The live chain writes the growing edge**, scoped to the **capture time** of the phase that
//!    must have recorded it (T-446's trap: the defect it fixed was an hour wide in stream time, and
//!    a scripted radio read on demand outruns an hour in ~88 s of wall clock, so a test that asks
//!    "has anything landed yet" passes on a broken build).
//! 3. **It never blocks the live path.** A competing thread holds the view pyramid the way a tile
//!    fan-out does, and capture, the ring and the detect reader are asserted to keep advancing
//!    through it — the always-on invariant, at the one place T-439 could have broken it.
//! 4. **Observed-but-not-yet-measured survives.** At a growing edge the newest cells are routinely
//!    sampled-but-unwritten; that is the normal state, not an error, and it is neither grey nor
//!    T-423's `"unknown"`. The test asserts the state *exists* (the pyramid is behind capture) and
//!    that the pyramid does not paper over it (nothing is written for the gap).
//! 5. **The off-diagonal nodes hold the live edge's own measurements** once the finest tiles seal,
//!    which is what "the coarsest tiles no longer read from scheme 1's top level" means in data.
//! 6. **T-484's two costs, measured, not assumed** — the resident accumulator at the shipped floor
//!    (it falls, because the tuned span is one frequency block at any rate) and the bytes the finest
//!    node writes per second of capture (it rises with the cell rate, and the byte budget's
//!    retention is what turns that into an honest live-detail horizon).
//! 7. **The floor and the depth are one decision.** A viewport's demanded level is set by the
//!    floor's absolute cell size while the work budget bounds level *indices*, so a finer node
//!    (0, 0) raises every demand and the DEPTH has to follow, per axis. Stated in tiles per
//!    viewport, because since T-482 the client clamps to the declared ceiling rather than being
//!    refused: the cost of too little reach is fan-out, not an error.
//!
//! Run against the mock/scripted device. The mechanism is in the pipeline's own reader plumbing and
//! is independent of the front end.

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use hk_model::{FreqRange, TimeRange, Timestamp};
use hk_pipeline::class::window_class;
use hk_pipeline::history::{
    VIEW_F_CELLS_PER_BLOCK, VIEW_F_LEVELS, VIEW_SCHEME, VIEW_T_CELLS_PER_BLOCK, VIEW_T_LEVELS,
};
use hk_pipeline::{Pipeline, PipelineConfig, SourceInfo, TrackInventory, replay_plan};
use hk_store::{Pyramid, RegionQuery, Resolution};
use serde_json::json;

const CENTER: f64 = 100.8e6;
const FS: f64 = 500e3;
const OFFSET_HZ: f64 = 80e3;
const BLOCK: usize = 16_384;
const RING_S: f64 = 0.5;
/// 4 s of stream time at 500 kS/s — ~40 history frames at the default 10 rows/s, far more than the
/// handful of level-0 cells any assertion here needs.
const PHASE_SAMPLES: u64 = 2_000_000;
/// Capture-time width of every pyramid query: twice a phase, so a phase's frames land inside it
/// with slack and nothing much later does. **This is the load-bearing part** (see T-446).
const WINDOW_NS: i64 = 2 * (PHASE_SAMPLES as i64) * 1_000_000_000 / (FS as i64);
const LIMIT: Duration = Duration::from_secs(180);

/// Flattened level index of view-lattice node `(level_f, level_t)`.
fn node(level_f: usize, level_t: usize) -> u8 {
    (level_f * VIEW_T_LEVELS + level_t) as u8
}

/// Observed cells the view pyramid holds at `level` for `center` over the **capture-time** window
/// `[from_ns, from_ns + WINDOW_NS)`.
///
/// **T-453: a coarse node is built when a read asks for it** (`docs/16` §5.2), so this asks — in
/// the same lock hold as the query, which is what `/api/tiles` does per output-row chunk and for
/// the same reason: a live-edge summary is dropped the moment a frame lands under it.
fn observed_at(view: &Arc<Mutex<Pyramid>>, level: u8, center: f64, from_ns: i64) -> usize {
    let mut p = view.lock().unwrap();
    let freq = FreqRange::centered(center, 0.5 * FS);
    let time = TimeRange::new(
        Timestamp::from_unix_nanos(from_ns),
        Timestamp::from_unix_nanos(from_ns + WINDOW_NS),
    );
    p.materialize(usize::from(level), freq, time)
        .expect("the view pyramid built the node");
    let h = p
        .query(&RegionQuery {
            freq: FreqRange::centered(center, 0.5 * FS),
            time: TimeRange::new(
                Timestamp::from_unix_nanos(from_ns),
                Timestamp::from_unix_nanos(from_ns + WINDOW_NS),
            ),
            resolution: Resolution::Level(level),
        })
        .expect("the view pyramid answered the query");
    h.cells.iter().filter(|c| c.observed()).count()
}

#[test]
fn the_live_chain_grows_the_view_lattices_finest_node_without_blocking_capture() {
    let dir = TempDir::new("live-edge-tiles");
    let (rx, ctl) = radio::Radio::new(CENTER, FS, BLOCK, radio::tone(|_| OFFSET_HZ));
    let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
    let mut plan = replay_plan(CENTER, FS, t0);
    plan.extra = json!({ "pipeline": { "ring_s": RING_S } });
    let mut cfg = PipelineConfig::new(&dir.0, plan).unwrap();
    cfg.source_class = window_class(CENTER, FS);
    cfg.live_window_class = true;
    cfg.lossless = true;
    cfg.settings.chains = Some(Vec::new());
    assert!(
        cfg.settings.view_history,
        "the view lattice is on by default: T-439 is not an opt-in surface"
    );
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
    let view = handle
        .view_history()
        .expect("the pipeline opens a view-scheme pyramid beside scheme 1");

    // ---- 1. the lattice is open, de-welded, and floored at the DISPLAY ROW ----
    {
        let p = view.lock().unwrap();
        assert_eq!(p.config().scheme, VIEW_SCHEME, "its own scheme root");
        let g = p.geometry();
        // T-484: node (0, 0) is the display STFT's own bin and row, so the canvas's finest tier
        // reproduces the rows the spectrum stream publishes. T-437's F1 argument against docs/16
        // §6.2's 128 s floor still holds and is now made by a stronger rule: the floor is not a
        // number chosen for the axis, it is the cadence of the measurements themselves.
        let (want_f, want_t) = hk_pipeline::history::view_geometry(
            FS,
            &PipelineConfig::new(&dir.0, replay_plan(CENTER, FS, t0))
                .unwrap()
                .settings,
            window_class(CENTER, FS),
        );
        assert_eq!(
            (g.levels[0].f_cell_hz, g.levels[0].t_cell_ns),
            (want_f, i64::try_from(want_t.as_nanos()).unwrap()),
            "node (0, 0) must be the display plan's own bin and row"
        );
        assert!(
            g.levels[0].t_cell_ns < 1_000_000_000,
            "and that is finer than the 1 s cell T-483 measured keeping 3 % of a station's level \
             variation, got {} ns",
            g.levels[0].t_cell_ns
        );
        assert_eq!(g.n_levels(), VIEW_F_LEVELS * VIEW_T_LEVELS);
        assert_eq!(g.f_axis().len(), VIEW_F_LEVELS);
        assert_eq!(g.t_axis().len(), VIEW_T_LEVELS);
        // The address scheme 1 CANNOT express: fine frequency, coarse time. A welded ladder is the
        // diagonal of its own lattice, so this is the whole reason a second scheme exists.
        assert_eq!(
            g.level_at(0, 3),
            Some(node(0, 3) as usize),
            "a fine-frequency / coarse-time node must be real here: {:?}",
            g.t_axis()
        );
        assert_eq!(g.level_at(3, 0), Some(node(3, 0) as usize));
        assert_eq!(
            g.levels[node(0, 0) as usize].nt,
            VIEW_T_CELLS_PER_BLOCK as usize
        );
    }

    let stream_now = || {
        counters
            .stream_time_ns
            .load(Ordering::Relaxed)
            .max(radio::T0_NS)
    };

    // ---- 2 + 3. the growing edge, written while readers fight for the pyramid ----
    //
    // The competing thread is a tile fan-out in miniature: `/api/tiles` takes this same lock per
    // output-row chunk (T-438), and `ViewIngest` only ever *tries* it, so the history reader can
    // never be parked behind a reader. Running the contention DURING the phase is what makes the
    // always-on assertion below mean something.
    let mark = stream_now();
    let before = (
        counters.detect_reader.samples.load(Ordering::Relaxed),
        counters.history.view_frames.load(Ordering::Relaxed),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let hog = {
        let (view, stop) = (Arc::clone(&view), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut holds = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let p = view.lock().unwrap();
                std::thread::sleep(Duration::from_millis(15));
                drop(p);
                holds += 1;
                std::thread::sleep(Duration::from_millis(1));
            }
            holds
        })
    };
    assert!(
        ctl.wait_emitted(ctl.emitted() + PHASE_SAMPLES, LIMIT),
        "the run stopped delivering samples while a reader held the view pyramid — the live path \
         was blocked by a tile read, which is the one thing T-439 must not do"
    );
    // The growing edge, inside the capture window of the phase that must have recorded it.
    let deadline = Instant::now() + LIMIT;
    let finest = loop {
        let n = observed_at(&view, node(0, 0), CENTER, mark);
        if n > 0 {
            break n;
        }
        assert!(
            Instant::now() < deadline,
            "no view-lattice cells for {:.3} MHz over the {:.0} s of capture from {mark} \
             (view frames {}, late {}, rejected {}, dropped {})",
            CENTER / 1e6,
            WINDOW_NS as f64 / 1e9,
            counters.history.view_frames.load(Ordering::Relaxed),
            counters.history.view_late.load(Ordering::Relaxed),
            counters.history.view_rejected.load(Ordering::Relaxed),
            counters.history.view_dropped.load(Ordering::Relaxed),
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    stop.store(true, Ordering::Relaxed);
    let holds = hog.join().expect("the contending reader finished");
    assert!(
        holds > 0,
        "the contention thread never took the lock, so nothing was contended"
    );

    // The always-on invariant, measured rather than assumed: capture advanced by the phase it was
    // asked for, the detect reader kept draining the ring, and the view lattice kept being written
    // — all while a reader was repeatedly holding the pyramid.
    let advanced_ns = stream_now() - mark;
    let want_ns = PHASE_SAMPLES as i64 * 1_000_000_000 / FS as i64;
    assert!(
        advanced_ns >= want_ns,
        "capture stalled under reader contention: {advanced_ns} ns of stream time against the \
         {want_ns} ns the radio delivered"
    );
    assert!(
        counters.detect_reader.samples.load(Ordering::Relaxed) > before.0,
        "detection stopped advancing while view tiles were written"
    );
    let folded = counters.history.view_frames.load(Ordering::Relaxed);
    assert!(folded > before.1, "no frames reached the view lattice");
    assert!(finest > 0);
    // The writer being behind is expected and fine. DROPPED frames are not: the queue is a minute
    // deep at 10 rows/s, and losing the growing edge would be the same failure as blocking for it,
    // spelled differently.
    assert_eq!(
        counters.history.view_dropped.load(Ordering::Relaxed),
        0,
        "the growing edge lost frames to a reader"
    );
    // T-446's reading, on the second pyramid: a frame folded behind the watermark means a segment
    // end sealed a run that continues.
    assert_eq!(
        counters.history.view_late.load(Ordering::Relaxed),
        0,
        "view-lattice frames were folded behind the watermark"
    );
    assert_eq!(counters.history.view_rejected.load(Ordering::Relaxed), 0);

    // ---- 4. observed-but-not-yet-measured is a real state, and nothing invents over it ----
    //
    // The radio is demonstrably sampling (the ring's stream time is ahead) and the pyramid has not
    // written that far yet. On a live edge that is the NORMAL state of the newest cells. It is not
    // grey (the coverage plane says observed) and it is not T-423's "unknown" (we know perfectly
    // well that we looked). T-441 owns its colour; this asserts it still exists to be coloured.
    // Produced on demand rather than waited for: holding this lock DEFERS the reader's folds
    // (`ViewIngest` only tries it) while capture carries on, so the newest folded frame is pinned
    // and the sampled-but-unwritten window is guaranteed to open — which is also a second reading
    // of the always-on invariant, from inside a reader's lock hold.
    {
        let p = view.lock().unwrap();
        let latest = p
            .latest_frame_end()
            .expect("the live chain has folded frames")
            .as_unix_nanos();
        // First time cell no fold can have reached: a frame is assigned to the cell holding its
        // midpoint, and no frame has ended past `latest`, so every written cell index is at most
        // the one holding `latest`.
        let cell_ns = 1_000_000_000i64;
        let unwritten_from = latest.div_euclid(cell_ns) * cell_ns + cell_ns;
        let deadline = Instant::now() + LIMIT;
        while stream_now() <= unwritten_from + cell_ns {
            assert!(
                Instant::now() < deadline,
                "capture stalled while a reader held the view pyramid: stream time {} has not \
                 passed {unwritten_from}",
                stream_now()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let now = stream_now();
        assert!(
            latest < now,
            "the pyramid claims to have measured every sampled nanosecond; a growing edge is \
             always behind capture, and erasing that state erases the distinction T-441 must draw"
        );
        let unmeasured = p
            .query(&RegionQuery {
                freq: FreqRange::centered(CENTER, 0.5 * FS),
                time: TimeRange::new(
                    Timestamp::from_unix_nanos(unwritten_from),
                    Timestamp::from_unix_nanos(now),
                ),
                resolution: Resolution::Level(node(0, 0)),
            })
            .expect("the view pyramid answered")
            .cells
            .iter()
            .filter(|c| c.observed())
            .count();
        assert_eq!(
            unmeasured, 0,
            "the pyramid wrote cells past the newest frame it folded: observed-but-not-yet-measured \
             is the normal state of a growing edge's newest cells, and it must stay EMPTY here so \
             that the coverage plane's \"observed\" is what distinguishes it from grey"
        );
    }

    // ---- 5. the off-diagonal nodes carry the live edge's own measurements ----
    //
    // T-453: they carry them **when asked**. `observed_at` materialises before it queries, which is
    // the whole of the change — capture writes node (0, 0) and no other, and a coarse node is
    // folded out of it by the reader that wants it. A node whose own time block has elapsed is
    // sealed on the way, so the second reader of the same address pays nothing; one at the live
    // edge is folded transiently and thrown away when the next frame lands.
    //
    // This still waits for a finest tile to complete in capture time rather than forcing it, so
    // that the *sealed* path is the one exercised: the run-end seal goes through the last frame and
    // no further, deliberately (see `history::view_writer` — the hour of slack scheme 1 uses writes
    // 273 files to persist 0.1 MB on a 64-node lattice). A node (0, 0) tile is
    // `VIEW_T_CELLS_PER_BLOCK` seconds, so one completes on its own shortly after that much stream
    // time, and (0, 1) and (1, 0) can then be folded out of sealed tiles.
    let tile_ns = VIEW_T_CELLS_PER_BLOCK as i64 * {
        let p = view.lock().unwrap();
        p.geometry().levels[0].t_cell_ns
    };
    let want = 3 * tile_ns;
    let deadline = Instant::now() + LIMIT;
    while stream_now() - mark < want {
        assert!(
            ctl.wait_emitted(ctl.emitted() + PHASE_SAMPLES, LIMIT),
            "the run stopped delivering samples while waiting for a finest tile to complete"
        );
        assert!(Instant::now() < deadline, "capture never reached {want} ns");
    }
    for (lf, lt) in [(0usize, 1usize), (1, 0), (1, 1)] {
        let deadline = Instant::now() + LIMIT;
        loop {
            if observed_at(&view, node(lf, lt), CENTER, mark) > 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "view-lattice node ({lf}, {lt}) holds nothing for the band the run was tuned to \
                 after {:.0} s of capture: the coarse end is still folding out of scheme 1's \
                 ladder, which is the gap T-438 named",
                (stream_now() - mark) as f64 / 1e9
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    // And the finest node still holds what it held: folding up never consumed it.
    assert!(observed_at(&view, node(0, 0), CENTER, mark) > 0);

    ctl.finish();
    let (summary, fired) = wait_guarded(handle, LIMIT);
    assert!(!fired, "the run had to be stopped by the watchdog");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);

    // Tiles really reached disk under the view scheme's own root.
    assert!(
        counters.history.view_tiles_written.load(Ordering::Relaxed) > 0,
        "no view-lattice tiles were written"
    );
    assert!(
        dir.0.join("history").join("view").join("history").is_dir(),
        "the view scheme has its own store root beside calibrated/ and uncalibrated/"
    );
}

/// **What the view lattice's floor costs in RAM, measured** — the sizing figure the settings doc
/// quotes, and the standing measurement of every residency decision made about the lattice.
///
/// | | resident floor | what a read of a coarse tile costs |
/// |---|---|---|
/// | T-439 eager-at-seal | 2.28 MB/MHz | 1 tile, but up to a whole producer tile stale at the edge |
/// | T-453 on demand | **0.91 MB/MHz** | up to 1024 producer tiles, ~500 ms, folded in the request |
/// | T-571 live | 73 MB measured / 93.5 MB bound at 20 MHz (3.65 / 4.67 MB/MHz) | 1 tile, ≤1 producer *cell* stale |
/// | **T-585 live, rows encoded** | **measured below; bound in the tens of MB** | 1 tile, ≤1 producer *cell* stale |
///
/// T-571 stated its cost rather than hiding it: 16 nodes, 16 open accumulators (25 at the
/// measured peak, mid-seal), because the tile is the write unit and a committed row had to live
/// somewhere until the seal. **T-585 holds a coarse node's committed rows in the codec's stored
/// form and keeps only the in-progress row as accumulator**, so the coarse nodes' residency is
/// one row each plus an encoded remainder, and the figure is measured here from the buffers
/// themselves ([`hk_store::Pyramid::resident_bytes`]) rather than from an open-tile count times
/// an assumed size.
///
/// The bound is arithmetic over the RAW stored form (no credit for zstd), and every assertion is
/// a byte or event count — no wall clock. The other lever the ticket named,
/// [`VIEW_T_CELLS_PER_BLOCK`] (a shorter tile is resident for less of its life), is measured
/// alongside at 16 rows and printed, so the two can be compared on the same frames.
#[test]
fn the_view_lattices_floor_costs_what_the_settings_doc_says_it_costs() {
    use hk_model::PowerUnit;
    use hk_store::{FrameInput, ResidentBytes, ViewLattice};

    /// Bytes of per-cell accumulator in one full `hk_store` tile cell: `count` u32, `max`/
    /// `occ_max`/`p_lo`/`p_hi` f32, `sum_lin`/`obs_s`/`occ_s` f64. T-571's unit of residency.
    const BYTES_PER_CELL: usize = 4 + 4 * 4 + 3 * 8;
    /// Stored-form bytes one committed cell can cost before compression (`max`/`mean` i16,
    /// three u16 fractions, a varint count of up to 3 B, its bitmap bit).
    const ENC_RAW_MAX: usize = 2 + 2 + 2 + 2 + 2 + 3 + 1;
    /// Per-segment bookkeeping, generously; a row commits in at most two footprints.
    const SEGMENT_OVERHEAD: usize = 64;
    /// Block-aligned, so the coefficient is not inflated by a straddled boundary.
    const F_LO: f64 = 100.0e6;
    /// Stream seconds to run. Enough for the finest four time levels to open, seal and fold, which
    /// is what exercises the mechanism; the bound below is arithmetic over all sixteen nodes, and
    /// the peak measured here is asserted against it.
    const SECS: i64 = 700;
    /// Seconds between frames.
    ///
    /// **It has to put more than one row in a level-0 block, and that is not a free parameter.**
    /// Residency is a function of which `(node, frequency block)` the watermark sits in and not of
    /// how densely those cells were filled — the bound below is unchanged by this number — but the
    /// `ever_live` assertion is about the *fold*, and the fold only happens *inside* a block if
    /// two of its rows close while it is open. At the shipped floor a level-0 tile is
    /// `VIEW_T_CELLS_PER_BLOCK` x 40 ms = 2.56 s, so the old 4 s stride gave every block exactly
    /// one row: its single fold coincided with its own seal, and the three frequency-coarser /
    /// time-finest nodes — whose blocks end at the same instant as their producer's — were opened
    /// and sealed inside one `ingest`, never resident between frames. That read as "only 12 of 15
    /// coarse nodes ever held an open accumulator" while the lattice was in fact being filled row
    /// by row, and until T-584 it was masked by `Pyramid::checkpoint` folding its still-OPEN
    /// column upward every 60 s — which T-584 deliberately stopped doing, because that column is
    /// not a finished row and late frames were being lost from every coarse node. So the stride is
    /// the thing that was wrong: one second puts two or three rows in each level-0 block, which is
    /// the regime a 25 rows/s live edge is actually in.
    const STRIDE: i64 = 1;
    /// The span a HackRF's widest practical live window covers.
    const LIVE_EDGE_HZ: f64 = 20.0e6;

    // **T-484: measured at the SHIPPED floor, derived rather than named.** Node (0, 0) is the
    // display plan's own bin and row, so `f_cell = fs / spectrum_fft_len` and one level-0 frequency
    // block (`VIEW_F_CELLS_PER_BLOCK` = 1024 = the display FFT size) is **exactly the tuned span**,
    // at any rate: a 20 MHz live edge holds ONE block, so measuring at the live edge itself is the
    // honest case and needs no extrapolation.
    let (f_cell_hz, t_cell) = hk_pipeline::history::view_geometry(
        LIVE_EDGE_HZ,
        &hk_pipeline::PipelineSettings::default(),
        hk_model::ContentClass::Unrestricted,
    );
    let cfg = hk_pipeline::history::view_config(f_cell_hz, t_cell);
    let span_hz = f_cell_hz * f64::from(VIEW_F_CELLS_PER_BLOCK);
    let (f_cell, bins) = (cfg.f_cell_hz, usize::from(cfg.histogram.bins));
    let nf = cfg.f_cells_per_block as usize;
    assert_eq!(nf, VIEW_F_CELLS_PER_BLOCK as usize);
    let n = (span_hz / f_cell) as usize;
    let psd = vec![1e-9f32; n];
    let t0 = radio::T0_NS;

    /// One run: the peak resident set (by total), what T-571's one-full-tile-per-open-tile rule
    /// would have held at that same sample, the most coarse tiles ever open, and the worst
    /// accumulator rows per live tile.
    struct Run {
        peak: ResidentBytes,
        peak_t571_coarse: usize,
        ever_live: usize,
        worst_rows_per_tile: f64,
        per_tile: usize,
        tile_rows: usize,
    }
    let measure = |tag: &str, cfg: hk_store::PyramidConfig| -> Run {
        let dir = TempDir::new(tag);
        let mut p = Pyramid::open(&dir.0, cfg).unwrap();
        let g = p.geometry().clone();
        let tile_rows = g.levels[0].nt;
        let per_tile = nf * tile_rows * BYTES_PER_CELL + nf * bins * 4;
        let mut r = Run {
            peak: ResidentBytes::default(),
            peak_t571_coarse: 0,
            ever_live: 0,
            worst_rows_per_tile: 0.0,
            per_tile,
            tile_rows,
        };
        for s in (0..SECS).step_by(STRIDE as usize) {
            p.ingest(&FrameInput::new(
                Timestamp::from_unix_nanos(t0 + s * 1_000_000_000),
                1_000_000_000,
                F_LO,
                f_cell,
                PowerUnit::Dbfs,
                &psd,
            ))
            .unwrap();
            let now = p.resident_bytes();
            r.ever_live = r.ever_live.max(now.live_tiles);
            if now.live_tiles > 0 {
                r.worst_rows_per_tile = r
                    .worst_rows_per_tile
                    .max(now.live_acc_rows as f64 / now.live_tiles as f64);
            }
            if now.total() > r.peak.total() {
                r.peak = now;
                r.peak_t571_coarse = (1..g.n_levels())
                    .map(|l| p.open_keys(l).len() * per_tile)
                    .sum();
            }
        }
        r
    };

    let g = cfg.geometry().unwrap();
    let shipped = measure("view-lattice-cost", cfg.clone());
    // The other lever, on the same frames: 16-row tiles (4x the tile files, a scheme bump).
    let mut cfg16 = cfg.clone();
    cfg16.scheme = cfg.scheme + 100;
    cfg16.levels = ViewLattice {
        cells_per_block: 16,
        ..hk_pipeline::history::view_lattice(f_cell_hz, t_cell)
    }
    .levels();
    let short = measure("view-lattice-cost-16", cfg16);

    // Blocks the span covers, counted the way the store tiles them — per node, because a
    // frequency-coarser node's block is 2x as wide and it needs half as many of them.
    let level_blocks = |l: usize| -> f64 {
        let bw = g.levels[l].f_cell_hz * f64::from(VIEW_F_CELLS_PER_BLOCK);
        ((F_LO + span_hz) / bw).ceil() - (F_LO / bw).floor()
    };
    let blocks = level_blocks(0);
    // **The bound, per node, with a seal-lag overlap on every one of them** (T-571 measured 25
    // open tiles over 16 nodes at this floor: one level-0 column close cascades through every
    // level, so several nodes hold an outgoing tile and its successor at once). Level 0 is a
    // full accumulator; every coarse node is at most three rows of accumulator (one in progress,
    // one just completed by a gap flush, one spare) plus its rows in RAW stored form.
    let nt = shipped.tile_rows;
    let level0_bound = 2.0 * blocks * shipped.per_tile as f64;
    let coarse_bound: f64 = (1..g.n_levels())
        .map(|l| {
            2.0 * level_blocks(l)
                * (3 * nf * Pyramid::ROW_ACC_BYTES_PER_CELL
                    + nt * nf * ENC_RAW_MAX
                    + 2 * nt * SEGMENT_OVERHEAD) as f64
        })
        .sum();
    let bound = level0_bound + coarse_bound;
    let mb = |b: f64| b / (1 << 20) as f64;
    let per_mhz = |b: f64| mb(b) / (span_hz / 1e6);
    let pk = &shipped.peak;
    eprintln!(
        "T-585 view-lattice floor at T-501's shipped geometry ({:.2} kHz x {:.1} ms, \
         {nf}x{nt} cells/block, {} nodes), {:.1} MHz tuned = the live edge:\n  measured peak \
         {:.1} MB resident = {} full tiles {:.1} MB + {} live coarse tiles holding {} accumulator \
         rows {:.1} MB and {} encoded segments {:.1} MB; {:.2} MB/MHz\n  T-571 held {:.1} MB of \
         full accumulators for those same coarse tiles ({:.1}%); T-571's bound was 93.5 MB\n  \
         bound (level 0 {:.1} MB + coarse nodes at three rows of accumulator and RAW stored rows \
         {:.1} MB, seal-lag overlap on every node): {:.1} MB, {:.2} MB/MHz\n  the other lever, \
         VIEW_T_CELLS_PER_BLOCK 64 -> 16 on the same frames: peak {:.1} MB ({} full tiles \
         {:.1} MB, coarse {:.1} MB)",
        f_cell / 1e3,
        t_cell.as_nanos() as f64 / 1e6,
        g.n_levels(),
        span_hz / 1e6,
        mb(pk.total() as f64),
        pk.full_tiles,
        mb(pk.full_bytes as f64),
        pk.live_tiles,
        pk.live_acc_rows,
        mb(pk.live_acc_bytes as f64),
        pk.live_segments,
        mb(pk.live_encoded_bytes as f64),
        per_mhz(pk.total() as f64),
        mb(shipped.peak_t571_coarse as f64),
        100.0 * pk.live_bytes() as f64 / shipped.peak_t571_coarse.max(1) as f64,
        mb(level0_bound),
        mb(coarse_bound),
        mb(bound),
        per_mhz(bound),
        mb(short.peak.total() as f64),
        short.peak.full_tiles,
        mb(short.peak.full_bytes as f64),
        mb(short.peak.live_bytes() as f64),
    );

    // The residency figure, against docs/16 §6.2's 9.1 h. T-484 shortens it further — a tile is
    // `VIEW_T_CELLS_PER_BLOCK` display rows rather than 64 s.
    assert!(
        g.levels[0].t_cell_ns * g.levels[0].nt as i64 <= 64 * 1_000_000_000,
        "the finest node's tile must be no longer than the 64 s T-439 shipped, and nothing like \
         docs/16 §6.2's 9.1 h, got {} ns",
        g.levels[0].t_cell_ns * g.levels[0].nt as i64
    );
    assert_eq!(
        blocks, 1.0,
        "T-484's sizing consequence: at `f_cell = fs / spectrum_fft_len` the tuned span is exactly \
         ONE level-0 frequency block, so open-block count stops scaling with the span"
    );
    assert!(
        (2_500_000..3_500_000).contains(&shipped.per_tile),
        "~2.9 MB per full tile — scheme 1's own level-0 tile shape, got {}",
        shipped.per_tile
    );
    assert_eq!(nt, VIEW_T_CELLS_PER_BLOCK as usize);
    // The mechanism, over the WHOLE run: coarse nodes ARE live-maintained (the inverse of what
    // T-453 asserted here) ...
    assert!(
        shipped.ever_live >= g.n_levels() - 1,
        "only {} of {} coarse nodes ever held an open tile: a live lattice fills every node as \
         rows close",
        shipped.ever_live,
        g.n_levels() - 1
    );
    // ... and what each holds is one row of accumulator, never the tile (T-585).
    assert!(
        shipped.worst_rows_per_tile <= 2.0,
        "{:.2} accumulator rows per live coarse tile: committed rows are staying resident as \
         accumulator",
        shipped.worst_rows_per_tile
    );
    // The peak sits inside the bound, and the finest node's full tile is inside the peak.
    assert!(
        pk.total() as f64 <= bound,
        "residency exceeded its bound: {} > {bound:.0}",
        pk.total()
    );
    assert!(
        pk.full_bytes >= shipped.per_tile,
        "the finest node's tile should be resident at the peak ({} full tiles, {} B)",
        pk.full_tiles,
        pk.full_bytes
    );
    // **The order that matters, and this ticket moved it back.** T-571's coarse nodes held full
    // accumulators; the same tiles now hold under a third of that even in RAW stored form (the
    // bound: 3 rows x 36 B + 64 rows x 14 B against 64 rows x 44 B + histograms), and less with
    // zstd. Asserted at a half so the figure cannot creep back without this going red.
    assert!(
        pk.live_bytes() * 2 < shipped.peak_t571_coarse,
        "coarse residency {} B is not under half of T-571's {} B for the same open tiles",
        pk.live_bytes(),
        shipped.peak_t571_coarse
    );
    // And the figure that actually sizes a device: a 20 MHz live edge stays in the TENS of MB.
    // T-571 accepted 94 MB (asserted 40..160); this is the number that replaces it.
    assert!(
        (5.0..45.0).contains(&mb(bound)),
        "{:.0} MB at a {:.0} MHz live edge is not what T-585 decided",
        mb(bound),
        LIVE_EDGE_HZ / 1e6
    );
    assert!(
        mb(pk.total() as f64) < 45.0,
        "{:.1} MB measured at a {:.0} MHz live edge",
        mb(pk.total() as f64),
        LIVE_EDGE_HZ / 1e6
    );
}

/// **T-484 — the floor and the depth are one decision, and zooming out is what couples them.**
///
/// A viewport's demanded level is `ceil(log2(hzPerPx / f_cell₀))` and `ceil(log2(nsPerPx / t_cell₀))`
/// (`ui/src/surface/lattice.ts::levelsFor`), so it moves with the **floor**; the reach the store can
/// actually serve is bounded by `level_f + level_t` — a bound on **level indices**, blind to how big
/// a cell is. Making node (0, 0) 2.67× finer in frequency and 25× finer in time therefore raised
/// every demand by about six levels while leaving the reach where it was. Nothing in the fidelity
/// tests could see that: they only ever look at the finest node.
///
/// **What this measures is what the CLIENT pays, not whether a demand is refused.** Since T-482 the
/// client clamps its demand to the declared readable ceiling and never asks past it, so an
/// unaffordable demand is not a `400` — it is *more tiles*, at a coarser level than the viewport
/// wanted. That is the honest cost of a finer floor, and it is the number to hold down.
#[test]
fn the_lattices_depth_keeps_the_canvass_zoom_out_affordable() {
    use hk_api::tiles::{TILE_CELLS, TileLattice, readable_ceiling};
    use hk_pipeline::history::{view_config, view_geometry};

    /// A canvas pane, in CSS pixels.
    const PX: (f64, f64) = (1600.0, 800.0);
    /// The rate the floor is derived at. The demand arithmetic is a ratio, so the conclusion does
    /// not depend on it; this is simply a live edge the user actually tunes.
    const RATE_HZ: f64 = 2.4e6;
    /// Tiles a pane may need at the declared ceiling, per viewport — **pinned at what the shipped
    /// geometry costs today, which is NOT what it should cost.** T-439's coarse floor cost 2, 21
    /// and 24 for these three; T-484's fine floor costs 8, 150 and 600 at the same 4 × 4 depth, and
    /// **4 × 6 would cost 8, 20 and 316** — better than the coarse floor on the band sweep. That
    /// depth is not shipped because it makes T-482's declared ceiling false at its own corner (see
    /// `VIEW_T_LEVELS`), so these numbers are the stated price of the fidelity fix and the thing the
    /// follow-up has to move. Pinned so neither the cost nor its repair can change unnoticed.
    const MAX_TILES: [f64; 3] = [8.0, 160.0, 640.0];

    let dir = TempDir::new("view-zoom-out");
    let shipped = view_geometry(
        RATE_HZ,
        &hk_pipeline::PipelineSettings::default(),
        hk_model::ContentClass::Unrestricted,
    );
    // CONTROL: T-439's floor, measured through the identical arithmetic, so "unchanged" is a
    // comparison rather than a claim.
    for (tag, (f0, t0)) in [
        (
            "T-439 floor 6250 Hz x 1 s ",
            (6250.0, Duration::from_secs(1)),
        ),
        ("T-484 floor             ", shipped),
    ] {
        let d = TempDir::new(&format!("zoom-ctl-{}", tag.trim().replace(' ', "-")));
        let pc = Pyramid::open(&d.0, view_config(f0, t0)).unwrap();
        let lat = TileLattice::view(pc.geometry());
        let (mf, mt) = readable_ceiling(&pc, &lat);
        let t0ns = t0.as_nanos() as f64;
        let mut line = format!("T-484 zoom-out CONTROL {tag} ceiling ({mf},{mt}):");
        for (what, span_hz, span_s) in [
            ("tuned 2.4MHz/20s", 2.4e6, 20.0),
            ("band 20MHz/10min", 20.0e6, 600.0),
            ("device 6GHz/10min", 6.0e9, 600.0),
        ] {
            let lf = (((span_hz / PX.0) / f0).log2().ceil().max(0.0) as usize).min(mf);
            let lt = (((span_s * 1e9 / PX.1) / t0ns).log2().ceil().max(0.0) as usize).min(mt);
            let tf = (span_hz / (lat.f_cells_hz[lf] * TILE_CELLS as f64)).ceil();
            let tt = (span_s * 1e9 / (lat.t_cells_ns[lt] as f64 * TILE_CELLS as f64)).ceil();
            line += &format!(" | {what} ({lf},{lt}) {:.0} tiles", tf * tt);
        }
        eprintln!("{line}");
    }
    let (f_cell_hz, t_cell) = shipped;
    let p = Pyramid::open(&dir.0, view_config(f_cell_hz, t_cell)).unwrap();
    let lattice = TileLattice::view(p.geometry());
    let (max_f, max_t) = readable_ceiling(&p, &lattice);
    let t_cell_ns = t_cell.as_nanos() as f64;
    eprintln!(
        "T-484 zoom-out: floor {f_cell_hz:.1} Hz x {:.1} ms, {}x{} axes over a {}x{} store, \
         declared readable ceiling ({max_f}, {max_t}) = {:.3} MHz x {:.1} s per tile",
        t_cell_ns / 1e6,
        lattice.f_cells_hz.len(),
        lattice.t_cells_ns.len(),
        VIEW_F_LEVELS,
        VIEW_T_LEVELS,
        lattice.f_cells_hz[max_f] * TILE_CELLS as f64 / 1e6,
        lattice.t_cells_ns[max_t] as f64 * TILE_CELLS as f64 / 1e9,
    );

    for (i, (what, span_hz, span_s)) in [
        ("the tuned window, 2.4 MHz x 20 s", 2.4e6, 20.0),
        ("a band sweep, 20 MHz x 10 min", 20.0e6, 600.0),
        ("the whole device, 6 GHz x 10 min", 6.0e9, 600.0),
    ]
    .into_iter()
    .enumerate()
    {
        // `levelsFor`: demand per axis, independently, then clamped to the declared ceiling.
        let lf = (((span_hz / PX.0) / f_cell_hz).log2().ceil().max(0.0) as usize).min(max_f);
        let lt = (((span_s * 1e9 / PX.1) / t_cell_ns).log2().ceil().max(0.0) as usize).min(max_t);
        let tiles_f = (span_hz / (lattice.f_cells_hz[lf] * TILE_CELLS as f64)).ceil();
        let tiles_t = (span_s * 1e9 / (lattice.t_cells_ns[lt] as f64 * TILE_CELLS as f64)).ceil();
        eprintln!(
            "  {what:<34} -> (level_f {lf}, level_t {lt}) after clamping, \
             {tiles_f:.0} x {tiles_t:.0} = {:.0} tiles",
            tiles_f * tiles_t
        );
        assert!(
            tiles_f * tiles_t <= MAX_TILES[i],
            "{what} costs {:.0} tiles at the declared ceiling ({max_f}, {max_t}), over the {:.0} \
             this viewport is held to. The floor and \
             the depth are ONE decision: a finer node (0, 0) raises every demand, the reach is a \
             bound on level INDICES, and the client — which clamps rather than refusing — pays the \
             difference in fan-out.",
            tiles_f * tiles_t,
            MAX_TILES[i],
        );
    }
}

/// **T-484 — what the finer floor costs to WRITE, measured against T-453's budget.**
///
/// T-453's constraint is that work on the capture thread is paid whether or not anyone looks, and
/// that residency must not grow back. Residency is measured above (it falls). This measures the
/// other half — bytes and tile writes per second of capture — because the finer floor multiplies
/// the finest level's cell rate, and that is the one number this ticket could plausibly have made
/// worse without noticing.
///
/// Both geometries are driven at **their own cadence**, which is the whole point: the old floor
/// folded `crate::history`'s frames (512 bins at 10 rows/s at 2.4 Msps), the new one folds
/// `crate::spectrum`'s (1024 bins at ~24.9 rows/s). So the comparison is not "the same frames on a
/// finer grid" — it is what each chain actually writes. Compression is left on, because bytes on
/// disk is the quantity, and the fold runs on [`hk_pipeline::history::ViewWriter`]'s own thread in
/// the product either way, so none of it lands on the thread that gates the ring.
///
/// The assertion is deliberately loose and one-sided: this is a **measurement with a ceiling**, not
/// a pinned number. What it forbids is the failure that would matter — the finest tier quietly
/// becoming an order of magnitude more expensive than the arithmetic predicts.
#[test]
fn the_finer_floor_costs_what_the_cell_rate_says_it_costs() {
    use std::time::Duration;

    use hk_model::PowerUnit;
    use hk_store::FrameInput;

    /// Capture seconds to drive.
    const SECS: f64 = 120.0;
    /// The rate both chains run at here.
    const RATE_HZ: f64 = 2.4e6;
    /// Tuned span: the whole window at this rate.
    const SPAN_HZ: f64 = RATE_HZ;

    let run = |tag: &str, f_cell: f64, t_cell: Duration, bins: usize, rows_per_s: f64| {
        let dir = TempDir::new(&format!("view-write-cost-{tag}"));
        let cfg = hk_pipeline::history::view_config(f_cell, t_cell);
        let mut p = Pyramid::open(&dir.0, cfg).unwrap();
        let bin_hz = SPAN_HZ / bins as f64;
        let dur_ns = (1e9 / rows_per_s) as i64;
        let n = (SECS * rows_per_s) as i64;
        // **Noisy, structured frames, because bytes-on-disk is a property of the DATA.** A constant
        // PSD is the adjacent question: zstd crushes it to 0.01 B/cell and both geometries report
        // ~nothing, which would let a real 30× go unmeasured. This is a deterministic LCG for the
        // per-bin noise (≈5 dB of scatter, an exponential's shape) with a broadcast-width plateau
        // 20 dB up — the shape of the fixture T-483 measures on, and the thing the store actually
        // has to encode. Deterministic so the number is comparable between the two runs and across
        // machines.
        let mut seed = 0x243f_6a88_85a3_08d3u64;
        let mut psd = vec![0f32; bins];
        for i in 0..n {
            for (b, v) in psd.iter_mut().enumerate() {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                let u = ((seed >> 33) as f64 / (1u64 << 31) as f64).max(1e-6);
                let station = (b as f64 / bins as f64 - 0.7).abs() < 0.04;
                *v = (1e-9 * -u.ln() * if station { 100.0 } else { 1.0 }) as f32;
            }
            p.ingest(&FrameInput::new(
                Timestamp::from_unix_nanos(radio::T0_NS + i * dur_ns),
                dur_ns,
                100.0e6,
                bin_hz,
                PowerUnit::Dbfs,
                &psd,
            ))
            .unwrap();
        }
        p.seal_through(Timestamp::from_unix_nanos(
            radio::T0_NS + n * dur_ns + 3_600_000_000_000,
        ))
        .unwrap();
        p.checkpoint().unwrap();
        let s = p.stats();
        let cells_per_s = (SPAN_HZ / f_cell) * (1e9 / t_cell.as_nanos() as f64);
        let kb_s = s.bytes_written as f64 / 1024.0 / SECS;
        eprintln!(
            "T-484 write cost, {tag:<26} {:.0} Hz x {:.1} ms cells, {bins} bins @ {rows_per_s:.1} \
             rows/s\n  level-0 cells/s {cells_per_s:8.0} | {:5} tiles and {:7.1} kB written per \
             {SECS:.0} s of capture = {:.2} tiles/s, {kb_s:.1} kB/s, {:.2} B/cell",
            f_cell,
            t_cell.as_nanos() as f64 / 1e6,
            s.tiles_written,
            s.bytes_written as f64 / 1024.0,
            s.tiles_written as f64 / SECS,
            s.bytes_written as f64 / (cells_per_s * SECS),
        );
        (cells_per_s, kb_s)
    };

    let (old_cells, old_kb) = run(
        "T-439 floor (history)",
        6250.0,
        Duration::from_secs(1),
        512,
        10.0,
    );
    let (f_cell, t_cell) = hk_pipeline::history::view_geometry(
        RATE_HZ,
        &hk_pipeline::PipelineSettings::default(),
        hk_model::ContentClass::Unrestricted,
    );
    let (new_cells, new_kb) = run(
        "T-484 floor (display)",
        f_cell,
        t_cell,
        1024,
        1e9 / t_cell.as_nanos() as f64,
    );

    eprintln!(
        "T-484 write cost: cells/s x{:.1}, bytes/s x{:.1} (at {:.1} MHz; the cell rate is \
         fft_len x rows_per_s and so is INDEPENDENT of the span, while the old floor's was \
         span / f_cell — the ratio falls to ~8x at a 20 MHz edge)",
        new_cells / old_cells,
        new_kb / old_kb,
        SPAN_HZ / 1e6,
    );
    // **The ceiling is re-derived against T-571, not inherited from T-501.** T-501 measured this
    // under `coarse_on_demand`, where capture wrote node (0, 0) and nothing else, and set the
    // ceiling at 200 kB/s. T-571 made every node live, so capture now writes the WHOLE lattice —
    // its own measurement is 4.80x the finest level in bytes, stated and accepted in its notes as
    // the trade for deleting the read-time fold. 200 kB/s is therefore a bound on a code path that
    // no longer exists; keeping it would fail this test for the feature it is measuring. Measured
    // here: 310.9 kB/s at 2.4 MHz, which is 65 kB/s of finest level times T-571's 4.8x. The
    // ceiling below is that with headroom, and it still forbids the failure this test is for — an
    // order of magnitude more than the arithmetic predicts.
    assert!(
        new_kb < 500.0,
        "the finest tier writes {new_kb:.1} kB per second of capture, which is not a live edge's \
         worth of detail but a leak: the arithmetic is {new_cells:.0} cells/s at a couple of bytes \
         each, times T-571's whole-lattice write"
    );
    assert!(
        new_kb / old_kb <= 1.5 * (new_cells / old_cells),
        "bytes grew {:.1}x against a cell rate of {:.1}x: the extra is not resolution, it is \
         per-tile overhead from sealing {:.0}x more often",
        new_kb / old_kb,
        new_cells / old_cells,
        (1_000_000_000.0 / t_cell.as_nanos() as f64),
    );
}

/// **T-501 — the client's pinned tile budget is measured against THIS floor, not a hand-written
/// one.**
///
/// T-505 pinned the two-tier budget in `ui/test/surface-lattice.test.ts` against a `Lattice`
/// literal — `f0Hz: 585.9375, t0Ns: 40_106_667, levelsF: 16, levelsT: 19, maxLevelF: 9,
/// maxLevelT: 1` — written down from T-501's measurement while the fidelity floor was reverted off
/// main. Relanding the floor makes that literal a claim about the shipped server, and a claim no
/// TypeScript test can check: the numbers come out of [`hk_pipeline::history::view_geometry`] and
/// [`hk_api::tiles::TileLattice`], on this side of the wire.
///
/// So this asserts the whole descriptor, field for field, for **both tiers**. If any of it moves,
/// the counts the client test pins move with it and that test is measuring a lattice the server no
/// longer serves — which is exactly the failure mode T-484 shipped: every suite green about a
/// system nobody was running.
///
/// The rate is fixed at 2.4 Msps because the floor is `fs / fft_len` and the descriptor is
/// therefore rate-dependent; 2.4 Msps is the live edge the demo and the client test both assume.
#[test]
fn the_shipped_floor_is_the_lattice_the_client_test_pins() {
    use hk_api::tiles::{TILE_CELLS, TileLattice, readable_ceiling};
    use hk_pipeline::history::{view_config, view_geometry};

    const RATE_HZ: f64 = 2.4e6;

    let (f0, t0) = view_geometry(
        RATE_HZ,
        &hk_pipeline::PipelineSettings::default(),
        hk_model::ContentClass::Unrestricted,
    );
    assert_eq!(
        (f0, t0.as_nanos() as i64),
        (2343.75, 40_106_667),
        "the display STFT's own bin and row at 2.4 Msps"
    );

    // The DETAIL tier: the view pyramid at that floor.
    let dir = TempDir::new("t501-detail");
    let p = Pyramid::open(&dir.0, view_config(f0, t0)).unwrap();
    let detail = TileLattice::view(p.geometry());
    let (mf, mt) = readable_ceiling(&p, &detail);
    eprintln!(
        "T-501 detail tier: f0 {:.4} Hz, t0 {} ns, levels ({}, {}), ceiling ({mf}, {mt}) \
         = {:.1} MHz x {:.1} s per tile",
        detail.f_cells_hz[0],
        detail.t_cells_ns[0],
        detail.f_cells_hz.len(),
        detail.t_cells_ns.len(),
        detail.f_cells_hz[mf] * TILE_CELLS as f64 / 1e6,
        detail.t_cells_ns[mt] as f64 * TILE_CELLS as f64 / 1e9,
    );
    assert_eq!(detail.f_cells_hz[0], 2343.75);
    assert_eq!(detail.t_cells_ns[0], 40_106_667);
    assert_eq!((detail.f_cells_hz.len(), detail.t_cells_ns.len()), (14, 19));
    assert_eq!((mf, mt), (9, 1));

    // The OVERVIEW tier: T-505's lattice, anchored on scheme 1, whose cells do not move when the
    // display's do. Opened with the shipped spectrum-history config, exactly as `/api/tiles` does.
    let odir = TempDir::new("t501-overview");
    let main = Pyramid::open(&odir.0, hk_store::PyramidConfig::default()).unwrap();
    let over = TileLattice::overview(main.geometry());
    let (of, ot) = readable_ceiling(&main, &over);
    eprintln!(
        "T-501 overview tier: f0 {:.1} Hz, t0 {} ns, levels ({}, {}), ceiling ({of}, {ot}) \
         = {:.1} MHz x {:.2} days per tile",
        over.f_cells_hz[0],
        over.t_cells_ns[0],
        over.f_cells_hz.len(),
        over.t_cells_ns.len(),
        over.f_cells_hz[of] * TILE_CELLS as f64 / 1e6,
        over.t_cells_ns[ot] as f64 * TILE_CELLS as f64 / 86_400e9,
    );
    assert_eq!(
        (over.f_cells_hz[0], over.t_cells_ns[0]),
        (6250.0, 1e9 as i64)
    );
    assert_eq!((of, ot), (11, 14));

    // The counts the client test pins, computed here from the descriptors above by the same
    // arithmetic `levelsFor`/`tilesFor` use — so the two sides cannot drift apart silently.
    // 1600 x 800 for a pane, 1600 x 120 for the minimap, at a THIRTY-MINUTE horizon.
    let tiles =
        |lat: &TileLattice, max: (usize, usize), span_hz: f64, span_s: f64, px: (f64, f64)| {
            let f0 = lat.f_cells_hz[0];
            let t0 = lat.t_cells_ns[0] as f64;
            let lf = (((span_hz / px.0) / f0).log2().ceil().max(0.0) as usize).min(max.0);
            let lt = (((span_s * 1e9 / px.1) / t0).log2().ceil().max(0.0) as usize).min(max.1);
            let tf = (span_hz / (lat.f_cells_hz[lf] * TILE_CELLS as f64)).ceil();
            let tt = (span_s * 1e9 / (lat.t_cells_ns[lt] as f64 * TILE_CELLS as f64)).ceil();
            (tf * tt) as usize
        };
    for (what, span_hz, px) in [
        ("pane 2.4 MHz", 2.4e6, (1600.0, 800.0)),
        ("minimap 6 GHz", 6.0e9, (1600.0, 120.0)),
    ] {
        let d = tiles(&detail, (mf, mt), span_hz, 1800.0, px);
        let o = tiles(&over, (of, ot), span_hz, 1800.0, px);
        eprintln!("T-501 30 min {what}: detail {d} tiles, overview {o} tiles");
        assert!(
            o <= 100,
            "{what} at a thirty-minute horizon costs {o} tiles on the OVERVIEW tier, over the \
             100-tile budget the client picks tiers by. The floor cannot land: this is T-484's \
             dark map again, one tier up."
        );
    }
}

/// **T-1018: a coarse tile is read from its OWN node, never folded out of level 0 at read time.**
///
/// The user's tile-latency review (2026-09-25): `/api/tiles` sorted its candidates finest first and
/// read the first that held anything, so a `(3, 1)` tile read 16 × 65 536 level-0 cells in four
/// history-lock holds while T-585's live-maintained node `(3, 1)` sat unread, and a `(6, 1)` tile —
/// past the lattice's `level_f` ≤ 3 — was folded from the finest affordable level instead of the
/// cheapest one that answers it. Max-hold composes, so the node gives the same grid.
///
/// Driven end to end: the scripted receiver behind the generic device contract, the live chain
/// writing the view lattice, and the tile route reading the pipeline's own pyramid. Every
/// assertion is a count or a value — no wall clock (`cost.build_ms` is printed, never asserted).
///
/// 1. An **on-node** coarse tile answers from its exact node: `exact_node == true`, and
///    `cost.source_cells == cells²` in ONE lock hold.
/// 2. Its pixels equal the **level-0 fold** of the same data (the answer the finest-first walk
///    served), cell for cell: the same cells observed and the same max-hold value.
/// 3. An **off-lattice** tile answers from the candidate with the FEWEST source cells that still
///    only folds — node `(3, 1)` for `(6, 1)` — never a replicating level, and states it.
#[test]
fn a_coarse_tile_reads_its_own_node_and_an_off_lattice_one_the_cheapest_that_folds() {
    use hk_api::http::ApiState;
    use hk_api::tiles::{TILE_CELLS, tiles_json};

    let dir = TempDir::new("tile-read-level");
    let (rx, ctl) = radio::Radio::new(CENTER, FS, BLOCK, radio::tone(|_| OFFSET_HZ));
    let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
    let mut plan = replay_plan(CENTER, FS, t0);
    plan.extra = json!({ "pipeline": { "ring_s": RING_S } });
    let mut cfg = PipelineConfig::new(&dir.0, plan).unwrap();
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
    let view = handle
        .view_history()
        .expect("the pipeline opens a view-scheme pyramid");
    // 5 phases = 20 s of capture time: enough rows for node (3, 1) (8 bins × 2 rows) to hold
    // committed rows of its own, and a finished run, so the store is still while it is compared.
    for _ in 0..5 {
        assert!(
            ctl.wait_emitted(ctl.emitted() + PHASE_SAMPLES, LIMIT),
            "the run stopped delivering samples"
        );
    }
    ctl.finish();
    let (summary, fired) = wait_guarded(handle, LIMIT);
    assert!(!fired, "the run had to be stopped by the watchdog");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);

    let (f0, t0_ns, n3_1) = {
        let p = view.lock().unwrap();
        assert!(
            p.config().coarse_live,
            "the view lattice's coarse nodes are live (T-571)"
        );
        let g = p.geometry();
        (
            g.levels[0].f_cell_hz,
            g.levels[0].t_cell_ns,
            g.level_at(3, 1).expect("node (3, 1) is on the lattice"),
        )
    };
    assert_eq!(n3_1, node(3, 1) as usize);
    let state = ApiState {
        view_history: Some(Arc::clone(&view)),
        ..ApiState::default()
    };
    let cells = TILE_CELLS;
    // The tile holding the tone, 5 s into the run, at `(level_f, level_t)` of the view lattice.
    let address = |lf: usize, lt: usize| {
        let f_span = f0 * 2f64.powi(lf as i32) * cells as f64;
        let t_span = t0_ns * (1i64 << lt) * cells as i64;
        let f_index = ((CENTER + OFFSET_HZ) / f_span).floor() as i64;
        let t_index = (radio::T0_NS + 5_000_000_000).div_euclid(t_span);
        (f_index, t_index, f_span, t_span)
    };
    let tile = |lf: usize, lt: usize| {
        let (f_index, t_index, ..) = address(lf, lt);
        let q: Vec<(String, String)> = [
            ("scheme", "view".to_string()),
            ("level_f", lf.to_string()),
            ("level_t", lt.to_string()),
            ("f_index", f_index.to_string()),
            ("t_index", t_index.to_string()),
            ("cells", cells.to_string()),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b))
        .collect();
        let v = tiles_json(&state, &q).expect("the tile is servable");
        eprintln!(
            "tile ({lf}, {lt}): answered level {} exact_node {} source_cells {} chunks {} \
             build_ms {} of which shadow search {} (printed, never asserted) candidates {} \
             tried {}",
            v["resolution"]["answered"]["level"],
            v["resolution"]["answered"]["exact_node"],
            v["cost"]["source_cells"],
            v["cost"]["chunks"],
            v["cost"]["build_ms"],
            v["shadow"]["search"]["build_ms"],
            v["resolution"]["candidates"],
            v["resolution"]["tried"],
        );
        v
    };

    // ---- 1. on-node: the exact node answers, cells² source cells, one lock hold ----
    let on = tile(3, 1);
    // Both reads before any assertion, so a red run still prints what each one cost.
    let off = tile(6, 1);
    let observed = on["grid"]["observed_cells"].as_u64().unwrap();
    assert!(
        observed > 0,
        "the tone's tile must hold data, or nothing is judged: {}",
        on["grid"]["observed_cells"]
    );
    assert_eq!(
        on["axes"]["store_node"],
        json!(n3_1),
        "(3, 1) is a real node"
    );
    assert_eq!(
        on["resolution"]["answered"]["exact_node"],
        json!(true),
        "node (3, 1) exists, is live-maintained, and was not read: level {} answered from {} \
         source cells",
        on["resolution"]["answered"]["level"],
        on["cost"]["source_cells"]
    );
    assert_eq!(on["cost"]["source_cells"], json!(cells * cells));
    assert_eq!(on["cost"]["chunks"], json!(1), "one history lock hold");
    for axis in ["frequency", "time"] {
        assert_eq!(
            on["resolution"]["fold"][axis]["direction"],
            json!("exact"),
            "{axis}"
        );
    }

    // ---- 2. the same pixels as the level-0 fold, cell for cell ----
    let (f_index, t_index, f_span, t_span) = address(3, 1);
    let freq = FreqRange::new(f_index as f64 * f_span, (f_index + 1) as f64 * f_span);
    let time = TimeRange::new(
        Timestamp::from_unix_nanos(t_index * t_span),
        Timestamp::from_unix_nanos((t_index + 1) * t_span),
    );
    let fine = {
        let p = view.lock().unwrap();
        p.query(&RegionQuery {
            freq,
            time,
            resolution: Resolution::Level(0),
        })
        .expect("level 0 answers")
        .overview(time, freq, cells, cells)
    };
    let served = on["grid"]["max_db"].as_array().expect("json planes");
    assert_eq!(served.len(), fine.cells.len());
    assert_eq!(
        fine.observed_cells as u64, observed,
        "the same cells are observed"
    );
    let mut worst = 0f64;
    for (i, (s, f)) in served.iter().zip(&fine.cells).enumerate() {
        match (s.as_f64(), f.sources > 0) {
            (None, false) => {}
            (Some(s), true) => worst = worst.max((s - f64::from(f.max_db)).abs()),
            (s, f) => panic!("cell {i}: served {s:?}, level-0 fold observed {f}"),
        }
    }
    eprintln!(
        "node (3, 1) against the level-0 fold: max |Δ max_db| = {worst} dB over {observed} cells"
    );
    // Max-hold composes exactly; the only slack is the store's 0.01 dB stored form (codec).
    assert!(
        worst <= 0.011,
        "the node's max-hold differs from level 0's by {worst} dB"
    );

    // ---- 3. off-lattice: the cheapest candidate that folds, never the finest ----
    assert!(
        off["grid"]["observed_cells"].as_u64().unwrap() > 0,
        "{}",
        off["grid"]["observed_cells"]
    );
    assert_eq!(
        off["axes"]["store_node"],
        json!(null),
        "(6, 1) is past the lattice"
    );
    assert_eq!(
        off["resolution"]["answered"]["level"],
        json!(n3_1),
        "node (3, 1) is the candidate with the fewest source cells that still folds onto (6, 1); \
         candidates {} tried {}",
        off["resolution"]["candidates"],
        off["resolution"]["tried"]
    );
    for axis in ["frequency", "time"] {
        assert_ne!(
            off["resolution"]["fold"][axis]["direction"],
            json!("replicated"),
            "a replicating level was chosen over one that folds ({axis})"
        );
    }
    // (6, 1) over node (3, 1): 8× the frequency cells, the time cells one for one.
    assert_eq!(off["cost"]["source_cells"], json!(8 * cells * cells));
    // Every other folding candidate costs more to read — which is the whole rule.
    let p = view.lock().unwrap();
    let g = p.geometry();
    let src = |l: usize| {
        let c = &g.levels[l];
        ((f_span * 8.0) / c.f_cell_hz).ceil() * ((t_span as f64) / c.t_cell_ns as f64).ceil()
    };
    let chosen = src(n3_1);
    for c in off["resolution"]["candidates"].as_array().unwrap() {
        let l = c.as_u64().unwrap() as usize;
        let lv = &g.levels[l];
        let folds = lv.f_cell_hz <= f0 * 64.0 && lv.t_cell_ns <= t0_ns * 2;
        if folds && l != n3_1 {
            assert!(
                src(l) > chosen,
                "level {l} folds from {} cells, fewer than {chosen}",
                src(l)
            );
        }
    }
}
