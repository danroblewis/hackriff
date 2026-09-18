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
/// # What T-439 measured, and what T-453 changed
///
/// T-439 measured, and did not predict, that **only the `level_f = 0` column is ever resident**: a
/// frequency-coarser node has the *same* time cell as its producer, so the fold that fills it runs
/// inside the producer's seal — after the watermark has already passed that tile's end — and it is
/// sealed in the same pass instead of being left open. Residency was one tile row per **time**
/// level, not per node: an eighth of the obvious estimate. The peak came out **one row above** that,
/// because inside `seal_lag` a node's outgoing tile is still open while its successor has been
/// created.
///
/// **T-453 collapses that to one level.** `docs/16` §5.2 decided the coarse nodes are built on
/// demand, so capture opens an accumulator for node (0, 0) and for nothing else, whichever axis a
/// node coarsens: the *whole lattice's* floor is now one node's, and it does not move when the
/// lattice grows. That is the residency half of making the node count a reach decision — the write
/// half is in [`VIEW_F_CELLS_PER_BLOCK`]. The seal-lag row survives, and is why the bound is two
/// rows rather than one: it is a property of sealing, not of the lattice.
#[test]
fn the_view_lattices_floor_costs_what_the_settings_doc_says_it_costs() {
    use hk_model::PowerUnit;
    use hk_store::FrameInput;

    /// Bytes of per-cell accumulator in one `hk_store` tile cell: `count` u32, `max`/`occ_max`/
    /// `p_lo`/`p_hi` f32, `sum_lin`/`obs_s`/`occ_s` f64.
    const BYTES_PER_CELL: usize = 4 + 4 * 4 + 3 * 8;
    /// Block-aligned, so the coefficient is not inflated by a straddled boundary. Misalignment
    /// costs up to one extra block per node and is a real cost of wide blocks — it is just not the
    /// thing this measures.
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
    // **T-484: measured at the SHIPPED floor, derived rather than named.** Node (0, 0) is now the
    // display plan's own bin and row, so `f_cell = fs / spectrum_fft_len` and one level-0 frequency
    // block (`VIEW_F_CELLS_PER_BLOCK` = 1024 = the display FFT size) is **exactly the tuned span**,
    // at any rate. That is the load-bearing consequence for sizing: the number of open blocks stops
    // being a function of the span, so a 20 MHz live edge holds ONE block where 6.25 kHz cells held
    // four. Measuring at the live edge itself is therefore the honest case and needs no
    // extrapolation.
    let (f_cell_hz, t_cell) = hk_pipeline::history::view_geometry(
        LIVE_EDGE_HZ,
        &hk_pipeline::PipelineSettings::default(),
        // The class only enters the row plan above ~45 rows/s (`GATED_SPECTRUM_MAX_ROW_RATE_HZ`),
        // so at the shipped 25 it is the same geometry either way; the ungated one is the bound.
        hk_model::ContentClass::Unrestricted,
    );
    let mut cfg = hk_pipeline::history::view_config(f_cell_hz, t_cell);
    let span_hz = f_cell_hz * f64::from(VIEW_F_CELLS_PER_BLOCK);
    // Uncompressed payloads: this measures RESIDENT accumulator, and zstd on the sealed tiles is a
    // large share of the run time without touching the number being measured.
    cfg.compression_level = None;
    let (f_cell, bins) = (cfg.f_cell_hz, usize::from(cfg.histogram.bins));
    let nf = cfg.f_cells_per_block as usize;
    let mut p = Pyramid::open(&dir.0, cfg).unwrap();
    let n = (span_hz / f_cell) as usize;
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
            // T-453: ANY node above (0, 0), on either axis. T-439 could only count the frequency
            // column, because the time column was resident by construction.
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
    // epoch of frequency, not to the span's own lower edge).
    let bw = f_cell * f64::from(VIEW_F_CELLS_PER_BLOCK);
    let blocks = ((F_LO + span_hz) / bw).ceil() - (F_LO / bw).floor();
    // **T-453: one tile row, plus one.** Capture opens node (0, 0) and nothing else, so the steady
    // set is one row however many nodes the lattice has; inside `seal_lag` that row's outgoing tile
    // is still open while its successor has been created, which is the second. T-439 measured
    // `VIEW_T_LEVELS + 1` rows here, one per TIME level plus the seal lag — the difference between
    // the two numbers is exactly the eager fold this ticket removed, and the remaining term is a
    // property of sealing rather than of the lattice. Measured, not assumed.
    const RESIDENT_ROWS: f64 = 2.0;
    let bound = RESIDENT_ROWS * blocks * per_tile as f64;
    let mb = |b: f64| b / (1 << 20) as f64;
    let per_mhz = |b: f64| mb(b) / (span_hz / 1e6);
    eprintln!(
        "T-453/T-484 view-lattice floor ({:.2} kHz x {:.1} ms, {nf}x{} cells/block, {} nodes), \
         {:.1} MHz tuned:\n  measured peak {peak_tiles} tiles, {:.1} MB resident, {:.0} KB/tile, \
         {:.2} MB/MHz\n  bound (node (0, 0)'s tile row, plus a seal-lag overlap; {} nodes, and \
         the count does not enter): {:.1} MB, \
         {:.2} MB/MHz -> {:.0} MB at a {:.0} MHz live edge",
        f_cell / 1e3,
        t_cell.as_nanos() as f64 / 1e6,
        VIEW_T_CELLS_PER_BLOCK,
        g.n_levels(),
        span_hz / 1e6,
        mb(peak_bytes as f64),
        per_tile as f64 / 1024.0,
        per_mhz(peak_bytes as f64),
        VIEW_F_LEVELS * VIEW_T_LEVELS,
        mb(bound),
        per_mhz(bound),
        per_mhz(bound) * LIVE_EDGE_HZ / 1e6,
        LIVE_EDGE_HZ / 1e6,
    );

    // The residency figure, against docs/16 §6.2's 9.1 h. T-484 shortens it further — a tile is
    // `VIEW_T_CELLS_PER_BLOCK` display rows rather than 64 s — which is the write-frequency half of
    // the trade this ticket made, and the half that does NOT touch the number measured here.
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
        (2_500_000..3_500_000).contains(&per_tile),
        "~2.9 MB per tile — scheme 1's own level-0 tile shape, got {per_tile}"
    );
    // The mechanism, over the WHOLE run rather than at its end: no coarse node — on either axis —
    // ever holds an open accumulator, because capture never folds one. This is the assertion that
    // makes the floor independent of the node count, so growing the lattice cannot move it.
    assert_eq!(
        ever_coarse, 0,
        "a coarse node held an open tile at some point: with docs/16 §5.2's on-demand folding, \
         capture opens node (0, 0) and nothing else, and the sizing bound assumes it"
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
    // **The order that matters, and T-484 changed its shape.** T-439/T-453 quoted a per-MHz
    // coefficient because open blocks scaled with the span: 0.91 MB/MHz, ~18 MB at a 20 MHz live
    // edge. With `f_cell = fs / spectrum_fft_len` the span is one block whatever the rate, so the
    // floor is an ABSOLUTE few MB and the per-MHz number is a derived quantity that falls as the
    // edge widens. Both are asserted, so neither can drift unnoticed: single-digit MB total, and
    // strictly under the figure the settings doc used to quote at this edge.
    assert!(
        (2.0..8.0).contains(&mb(bound)),
        "{:.1} MB is outside the few-MB floor T-484 measured at a {:.0} MHz edge",
        mb(bound),
        LIVE_EDGE_HZ / 1e6,
    );
    assert!(
        per_mhz(bound) < 0.91,
        "{:.2} MB/MHz must be below T-453's 0.91 MB/MHz at this edge: one open block instead of \
         four is the whole sizing consequence",
        per_mhz(bound)
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
    assert!(
        new_kb < 200.0,
        "the finest tier writes {new_kb:.1} kB per second of capture, which is not a live edge's \
         worth of detail but a leak: the arithmetic is {new_cells:.0} cells/s at a couple of bytes \
         each"
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
