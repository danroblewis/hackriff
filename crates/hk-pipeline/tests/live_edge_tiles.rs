//! T-439 — **"live" is the finest-level growing edge of the view lattice, not a separate mode.**
//!
//! `docs/16` §8.1's leap, made testable: the live view is a *viewport* onto one surface, so the
//! live chain must write the finest node of the de-welded view scheme as capture proceeds. T-438
//! left the addressing without a store behind it — `PyramidConfig::view_lattice` existed and
//! nothing in `hk_pipeline` opened one, so the coarse addresses folded out of scheme 1's welded
//! ladder, whose off-diagonal nodes do not exist at all. This drives a scripted receiver behind the
//! generic device contract and asserts the other half:
//!
//! 1. **The lattice is open and de-welded.** Node (0, 0)'s time cell is **1 s** — T-437's finding
//!    F1, where `docs/16` §6.2's 128 s floor swallowed the whole 120 s IQ retention in one cell and
//!    re-welded the axis the lattice exists to separate — and `(level_f 0, level_t 3)` is a real
//!    node, which is precisely the address scheme 1 must answer `404` for.
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
    VIEW_F_CELLS_PER_BLOCK, VIEW_LEVELS, VIEW_SCHEME, VIEW_T_CELLS_PER_BLOCK,
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
    (level_f * VIEW_LEVELS + level_t) as u8
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

    // ---- 1. the lattice is open, de-welded, and floored at 1 s ----
    {
        let p = view.lock().unwrap();
        assert_eq!(p.config().scheme, VIEW_SCHEME, "its own scheme root");
        let g = p.geometry();
        assert_eq!(
            g.levels[0].t_cell_ns, 1_000_000_000,
            "F1: node (0, 0) is one second, not docs/16 §6.2's 128 — a 128 s floor puts the whole \
             120 s IQ retention inside one cell and re-welds the axis"
        );
        assert_eq!(g.n_levels(), VIEW_LEVELS * VIEW_LEVELS);
        assert_eq!(g.f_axis().len(), VIEW_LEVELS);
        assert_eq!(g.t_axis().len(), VIEW_LEVELS);
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
    let tile_ns = VIEW_T_CELLS_PER_BLOCK as i64 * 1_000_000_000;
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

/// **What the floor costs, measured** — T-434's RAM caveat, answered.
///
/// `docs/16` §6.2's V(0, 0) keeps a level-0 tile open for **9.1 h per 25.6 MHz block** at ~3 MB of
/// accumulator: fine for a survey, wrong for a growing edge, because a level-0 tile is an
/// in-memory accumulator for the whole of its duration. The shipped floor keeps the 1 s time cell
/// (F1) and shrinks the block's **height** to 64 time cells, so the finest node's tile spans
/// **64 s** rather than 9.1 h — 512× less residency. Its *width* stays scheme 1's 1024 cells,
/// because that axis buys file count rather than memory (see [`VIEW_F_CELLS_PER_BLOCK`]).
///
/// What a device must be sized for is the whole lattice, though, and this measures it through a
/// real pyramid. It tracks the **peak** rather than a final snapshot deliberately: which nodes hold
/// an open tile depends on where the watermark sits modulo each node's tile duration, so one
/// snapshot varies by 2× and is not a bound.
///
/// # What T-439 measured, what T-453 changed, and **what T-571 pays for live tiles**
///
/// T-439 measured, and did not predict, that **only the `level_f = 0` column is ever resident**: a
/// frequency-coarser node has the *same* time cell as its producer, so the fold that fills it runs
/// inside the producer's seal — after the watermark has already passed that tile's end — and it is
/// sealed in the same pass instead of being left open. Residency was one tile row per **time**
/// level, not per node: an eighth of the obvious estimate. The peak came out **one row above** that,
/// because inside `seal_lag` a node's outgoing tile is still open while its successor has been
/// created. Measured: **2.28 MB/MHz peak against a 3.65 MB/MHz bound**.
///
/// **T-453 collapsed that to one level** — coarse nodes built on demand, so capture opened an
/// accumulator for node (0, 0) and for nothing else: **0.91 MB/MHz**, and it did not move when the
/// lattice grew. It bought that by folding a coarse tile out of up to 1024 producer tiles **while
/// a reader waited**, which is the defect T-571 is about.
///
/// **T-571 is the explicit residency decision the ticket asked for, and this test is the number.**
/// Live maintenance means every node holds an open accumulator for the tile it is filling, because
/// **the tile is the write unit**: a row committed into a coarse tile has to live somewhere between
/// its commit and that tile's seal, and the only two places are RAM and a row-granular tile file
/// that `history::codec` does not have. So the floor goes back up, past T-439's, and the honest
/// statement is:
///
/// | | resident floor | what a read of a coarse tile costs |
/// |---|---|---|
/// | T-439 eager-at-seal | 2.28 MB/MHz | 1 tile, but up to a whole producer tile stale at the edge |
/// | T-453 on demand | **0.91 MB/MHz** | up to 1024 producer tiles, ~500 ms, folded in the request |
/// | T-571 live | ~7.8 MB/MHz at one block, **~4.9 MB/MHz at 20 MHz** | 1 tile, ≤1 producer *cell* stale |
///
/// The per-MHz coefficient **falls** as the span widens, because a frequency-coarser node covers
/// 2× the spectrum per tile: at one 6.4 MHz block every node needs one block, at 20 MHz the four
/// frequency levels need 4, 2, 1 and 1. The bound below is computed per node from its own block
/// width rather than extrapolated, for exactly that reason.
///
/// **It does grow with node count, and that is the cost this ticket accepted**, stated rather than
/// hidden: 16 nodes, 16 open accumulators. The knob that would take it back is the one
/// [`VIEW_T_CELLS_PER_BLOCK`] already names as the **memory** decision — a shorter tile seals
/// sooner and is resident for less of its life — and the structural fix is holding a coarse tile's
/// already-committed rows in their encoded form, leaving only the in-progress row as an
/// accumulator. Both are stored-format changes; neither is this ticket.
#[test]
fn the_view_lattices_floor_costs_what_the_settings_doc_says_it_costs() {
    use hk_model::PowerUnit;
    use hk_store::FrameInput;

    /// Bytes of per-cell accumulator in one `hk_store` tile cell: `count` u32, `max`/`occ_max`/
    /// `p_lo`/`p_hi` f32, `sum_lin`/`obs_s`/`occ_s` f64.
    const BYTES_PER_CELL: usize = 4 + 4 * 4 + 3 * 8;
    /// Tuned span to measure at: **exactly one level-0 frequency block**, so no edge rounding is
    /// folded into the coefficient. Cost is linear in the span — one more block per node per
    /// 6.4 MHz — so the per-MHz figure is what extrapolates to a live edge.
    const SPAN_HZ: f64 = 6.4e6;
    /// Block-aligned, so the coefficient is not inflated by a straddled boundary. Misalignment
    /// costs up to one extra block per node and is a real cost of wide blocks — it is just not the
    /// thing this measures.
    const F_LO: f64 = 102.4e6;
    /// Stream seconds to run. Enough for the finest four time levels to open, seal and fold, which
    /// is what exercises the mechanism; the bound below is arithmetic over all eight, and the peak
    /// measured here is asserted against it.
    const SECS: i64 = 700;
    /// Seconds between frames. Residency is a function of which `(node, frequency block)` the
    /// watermark sits in, not of how densely those cells were filled, so a stride costs nothing.
    const STRIDE: i64 = 4;
    /// The span a HackRF's widest practical live window covers, for the extrapolation.
    const LIVE_EDGE_HZ: f64 = 20.0e6;

    let dir = TempDir::new("view-lattice-cost");
    let mut cfg = hk_pipeline::history::view_config(6250.0);
    // Uncompressed payloads: this measures RESIDENT accumulator, and zstd on the sealed tiles is a
    // large share of the run time without touching the number being measured.
    cfg.compression_level = None;
    let (f_cell, bins) = (cfg.f_cell_hz, usize::from(cfg.histogram.bins));
    let nf = cfg.f_cells_per_block as usize;
    let mut p = Pyramid::open(&dir.0, cfg).unwrap();
    let n = (SPAN_HZ / f_cell) as usize;
    let psd = vec![1e-9f32; n];
    let t0 = radio::T0_NS;
    assert_eq!(nf, VIEW_F_CELLS_PER_BLOCK as usize);
    let per_tile = nf * VIEW_T_CELLS_PER_BLOCK as usize * BYTES_PER_CELL + nf * bins * 4;

    let resident = |p: &Pyramid| -> (usize, usize, usize) {
        let g = p.geometry();
        let (mut tiles, mut bytes, mut coarse) = (0usize, 0usize, 0usize);
        for level in 0..g.n_levels() {
            let open = p.open_keys(level).len();
            tiles += open;
            bytes += open * (nf * g.levels[level].nt * BYTES_PER_CELL + nf * bins * 4);
            // ANY node above (0, 0), on either axis. T-439 could only count the frequency column,
            // because the time column was resident by construction.
            if level > 0 {
                coarse += open;
            }
        }
        (tiles, bytes, coarse)
    };

    let (mut peak_tiles, mut peak_bytes, mut ever_coarse) = (0usize, 0usize, 0usize);
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
        let (tiles, bytes, coarse) = resident(&p);
        ever_coarse = ever_coarse.max(coarse);
        if bytes > peak_bytes {
            peak_bytes = bytes;
            peak_tiles = tiles;
        }
    }

    let g = p.geometry().clone();
    // Blocks the span actually covers, counted the way the store tiles them (aligned to the
    // epoch of frequency, not to the span's own lower edge) — **per node**, because a
    // frequency-coarser node's block is 2× as wide and it needs half as many of them.
    let level_blocks = |l: usize| -> f64 {
        let bw = g.levels[l].f_cell_hz * f64::from(VIEW_F_CELLS_PER_BLOCK);
        ((F_LO + SPAN_HZ) / bw).ceil() - (F_LO / bw).floor()
    };
    let bw = f_cell * f64::from(VIEW_F_CELLS_PER_BLOCK);
    let blocks = ((F_LO + SPAN_HZ) / bw).ceil() - (F_LO / bw).floor();
    // **T-571: one open tile per node, plus a seal-lag overlap.** Every node is filled as rows
    // close, so every node holds the tile it is filling; inside `seal_lag` a node's outgoing tile
    // is still open while its successor has been created, which is the `+ blocks` term (a whole
    // extra row of the finest node is the worst that overlap can be).
    let per_node: f64 = (0..g.n_levels())
        .map(|l| level_blocks(l) * (nf * g.levels[l].nt * BYTES_PER_CELL + nf * bins * 4) as f64)
        .sum();
    let bound = per_node + blocks * per_tile as f64;
    let mb = |b: f64| b / (1 << 20) as f64;
    let per_mhz = |b: f64| mb(b) / (SPAN_HZ / 1e6);
    // What the same bound says at a real live edge, where the coarse nodes need fewer blocks.
    let edge_bound: f64 = (0..g.n_levels())
        .map(|l| {
            let bw = g.levels[l].f_cell_hz * f64::from(VIEW_F_CELLS_PER_BLOCK);
            (LIVE_EDGE_HZ / bw).ceil() * (nf * g.levels[l].nt * BYTES_PER_CELL + nf * bins * 4) as f64
        })
        .sum();
    eprintln!(
        "T-571 view-lattice floor ({:.2} kHz x 1 s, {nf}x{} cells/block, {} nodes), {:.1} MHz \
         tuned:\n  measured peak {peak_tiles} tiles, {:.1} MB resident, {:.0} KB/tile, \
         {:.2} MB/MHz\n  bound (one open tile per node, plus a seal-lag overlap; {} nodes, and \
         the count DOES enter — T-571's stated cost): {:.1} MB, \
         {:.2} MB/MHz -> {:.0} MB at a {:.0} MHz live edge",
        f_cell / 1e3,
        VIEW_T_CELLS_PER_BLOCK,
        g.n_levels(),
        SPAN_HZ / 1e6,
        mb(peak_bytes as f64),
        per_tile as f64 / 1024.0,
        per_mhz(peak_bytes as f64),
        VIEW_LEVELS * VIEW_LEVELS,
        mb(bound),
        per_mhz(bound),
        mb(edge_bound),
        LIVE_EDGE_HZ / 1e6,
    );

    // The residency figure, against docs/16 §6.2's 9.1 h.
    assert_eq!(
        g.levels[0].t_cell_ns * g.levels[0].nt as i64,
        64 * 1_000_000_000,
        "the finest node's tile spans 64 s, not docs/16 §6.2's 9.1 h"
    );
    assert!(
        (2_500_000..3_500_000).contains(&per_tile),
        "~2.9 MB per tile — scheme 1's own level-0 tile shape, got {per_tile}"
    );
    // The mechanism, over the WHOLE run rather than at its end, and the exact inverse of what
    // T-453 asserted here: coarse nodes DO hold open accumulators, because they are being filled
    // row by row. If this were 0 again the lattice would be back to folding at read time.
    assert!(
        ever_coarse >= g.n_levels() - 1,
        "only {ever_coarse} of {} coarse nodes ever held an open accumulator: a live lattice \
         fills every node as rows close",
        g.n_levels() - 1
    );
    // The peak must sit inside the bound, and near enough to it that the bound is not vacuous.
    assert!(
        peak_bytes as f64 <= bound,
        "residency exceeded node (0, 0)'s tile row plus a seal-lag overlap: {peak_bytes} > \
         {bound:.0}"
    );
    assert!(
        peak_bytes as f64 >= blocks * per_tile as f64,
        "the finest node's row should be resident at the peak, got {peak_tiles} tiles \
         ({peak_bytes} B)"
    );
    // The order that matters: single MB per MHz, so a 20 MHz live edge is tens of MB — not the
    // hundreds the naive per-NODE estimate gives, and not the tens of KB that would mean nothing
    // had opened.
    assert!(
        (0.5..12.0).contains(&per_mhz(bound)),
        "{:.2} MB/MHz is outside the range the settings doc quotes",
        per_mhz(bound)
    );
    // And the figure that actually sizes a device: a 20 MHz live edge stays in the HUNDREDS of MB,
    // not GB. This is the number T-571 accepted; if it moves, the ticket's decision has moved.
    assert!(
        (40.0..160.0).contains(&mb(edge_bound)),
        "{:.0} MB at a {:.0} MHz live edge is not what T-571 decided",
        mb(edge_bound),
        LIVE_EDGE_HZ / 1e6
    );
}
