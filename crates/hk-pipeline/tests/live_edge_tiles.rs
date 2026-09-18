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
use hk_pipeline::history::{VIEW_CELLS_PER_BLOCK, VIEW_LEVELS, VIEW_SCHEME};
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
fn observed_at(view: &Arc<Mutex<Pyramid>>, level: u8, center: f64, from_ns: i64) -> usize {
    let p = view.lock().unwrap();
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
            VIEW_CELLS_PER_BLOCK as usize
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
    // Deferred frames are expected and fine — that is the try-lock working. DROPPED frames are
    // not: the queue is a minute deep at 10 rows/s, and losing the growing edge to a reader would
    // be the same failure as blocking it, spelled differently.
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
    // Coarser nodes are fed by folding SEALED finer tiles, and the end of a run that does not
    // continue seals through the last frame. So finishing the run is what fills them — and it is
    // also the assertion that T-439's segment-end path reaches the view pyramid at all.
    ctl.finish();
    let (summary, fired) = wait_guarded(handle, LIMIT);
    assert!(!fired, "the run had to be stopped by the watchdog");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);

    for (lf, lt) in [(0usize, 1usize), (1, 0), (1, 1)] {
        let n = observed_at(&view, node(lf, lt), CENTER, mark);
        assert!(
            n > 0,
            "view-lattice node ({lf}, {lt}) holds nothing for the band the run was tuned to: the \
             coarse end is still folding out of scheme 1's ladder, which is the gap T-438 named"
        );
    }
    // And the finest node still holds what it held: folding up never consumed it.
    assert!(observed_at(&view, node(0, 0), CENTER, mark) > 0);

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
/// (F1) and shrinks the *block* to 64 × 64, so the finest node's tile spans **64 s** at ~187 KB —
/// 512× less residency, 17× less memory per tile.
///
/// What a device must be sized for is the whole lattice, though, and this measures it through a
/// real pyramid. It tracks the **peak** rather than a final snapshot deliberately: which nodes hold
/// an open tile depends on where the watermark sits modulo each node's tile duration, so one
/// snapshot varies by 2× and is not a bound.
///
/// The measurement found something the arithmetic did not predict. **Only the `level_f = 0` column
/// is ever resident.** A frequency-coarser node has the *same* time cell as its producer, so the
/// fold that fills it runs inside the producer's seal — after the watermark has already passed that
/// tile's end — and it is sealed in the same pass instead of being left open. Residency is
/// therefore one tile row per **time** level, not per node: an eighth of the obvious estimate, and
/// the bound the settings doc quotes.
#[test]
fn the_view_lattices_floor_costs_what_the_settings_doc_says_it_costs() {
    use hk_model::PowerUnit;
    use hk_store::FrameInput;

    /// Bytes of per-cell accumulator in one `hk_store` tile cell: `count` u32, `max`/`occ_max`/
    /// `p_lo`/`p_hi` f32, `sum_lin`/`obs_s`/`occ_s` f64.
    const BYTES_PER_CELL: usize = 4 + 4 * 4 + 3 * 8;
    /// Tuned span to measure at. Cost is linear in it — one more block per node per 400 kHz — so
    /// the per-MHz coefficient is what extrapolates to a live edge.
    const SPAN_HZ: f64 = 800.0e3;
    const F_LO: f64 = 100.0e6;
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
    let per_tile = nf * VIEW_CELLS_PER_BLOCK as usize * BYTES_PER_CELL + nf * bins * 4;

    let resident = |p: &Pyramid| -> (usize, usize, usize) {
        let g = p.geometry();
        let (mut tiles, mut bytes, mut coarse_f) = (0usize, 0usize, 0usize);
        for level in 0..g.n_levels() {
            let open = p.open_keys(level).len();
            tiles += open;
            bytes += open * (nf * g.levels[level].nt * BYTES_PER_CELL + nf * bins * 4);
            if level / VIEW_LEVELS > 0 {
                coarse_f += open;
            }
        }
        (tiles, bytes, coarse_f)
    };

    let (mut peak_tiles, mut peak_bytes, mut ever_coarse_f) = (0usize, 0usize, 0usize);
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
        let (tiles, bytes, coarse_f) = resident(&p);
        ever_coarse_f = ever_coarse_f.max(coarse_f);
        if bytes > peak_bytes {
            peak_bytes = bytes;
            peak_tiles = tiles;
        }
    }

    let g = p.geometry().clone();
    let blocks = (SPAN_HZ / (f_cell * f64::from(VIEW_CELLS_PER_BLOCK))).ceil();
    let bound = VIEW_LEVELS as f64 * blocks * per_tile as f64;
    let mb = |b: f64| b / (1 << 20) as f64;
    let per_mhz = |b: f64| mb(b) / (SPAN_HZ / 1e6);
    eprintln!(
        "T-439 view-lattice floor ({:.2} kHz x 1 s, {nf}x{} cells/block, {} nodes), {:.1} MHz \
         tuned:\n  measured peak {peak_tiles} tiles, {:.1} MB resident, {:.0} KB/tile, \
         {:.2} MB/MHz\n  bound (one tile row per TIME level, all {} of them): {:.1} MB, \
         {:.2} MB/MHz -> {:.0} MB at a {:.0} MHz live edge",
        f_cell / 1e3,
        VIEW_CELLS_PER_BLOCK,
        g.n_levels(),
        SPAN_HZ / 1e6,
        mb(peak_bytes as f64),
        per_tile as f64 / 1024.0,
        per_mhz(peak_bytes as f64),
        VIEW_LEVELS,
        mb(bound),
        per_mhz(bound),
        per_mhz(bound) * LIVE_EDGE_HZ / 1e6,
        LIVE_EDGE_HZ / 1e6,
    );

    // The residency figure, against docs/16 §6.2's 9.1 h.
    assert_eq!(
        g.levels[0].t_cell_ns * g.levels[0].nt as i64,
        64 * 1_000_000_000,
        "the finest node's tile spans 64 s, not docs/16 §6.2's 9.1 h"
    );
    assert!(
        (150_000..250_000).contains(&per_tile),
        "~187 KB per tile, got {per_tile}"
    );
    // The mechanism, over the WHOLE run rather than at its end: a frequency-coarser node is
    // written and sealed inside its producer's seal, so it never holds an open accumulator. If this
    // changes, the bound below is wrong by about 2× and so is the number the settings doc quotes.
    assert_eq!(
        ever_coarse_f, 0,
        "a level_f > 0 node held an open tile at some point: residency is one tile row per TIME \
         level, and the sizing bound assumes it"
    );
    // The peak must sit inside the bound, and near enough to it that the bound is not vacuous.
    assert!(
        peak_bytes as f64 <= bound,
        "residency exceeded one tile row per time level: {peak_bytes} > {bound:.0}"
    );
    assert!(
        peak_bytes as f64 >= 4.0 * blocks * per_tile as f64,
        "the four finest time levels should be resident together at the peak, got {peak_tiles} \
         tiles ({peak_bytes} B)"
    );
    // The order that matters: single MB per MHz, so a 20 MHz live edge is tens of MB — not the
    // hundreds the naive per-NODE estimate gives, and not the tens of KB that would mean nothing
    // had opened.
    assert!(
        (0.5..5.0).contains(&per_mhz(bound)),
        "{:.2} MB/MHz is outside the range the settings doc quotes",
        per_mhz(bound)
    );
}
