//! **T-571: coarse view-lattice nodes are maintained LIVE and INCREMENTALLY, not folded at read
//! time.**
//!
//! CLAUDE.md, "Live rendering, tile maintenance and playback": *"Tiles are maintained LIVE and
//! INCREMENTALLY, per downsample interval… A level that downsamples every N rows adjusts its
//! in-progress top row as rows arrive and COMMITS EVERY N — the same rule at 2, 4, 8, 16, 32.
//! Coarse levels are incrementally maintained structures, not batch materialize-on-demand work."*
//!
//! What the code did instead was fold a coarse tile out of up to [`MAX_MATERIALIZE_TILES`] = 1024
//! producer tiles **inside the request**, ~500 ms a tile and 135–290 tiles a screen: 50–110 s to
//! fill a cold screen, with the live edge gated on that batch work.
//!
//! # How this file asserts it, and why there is not a single wall-clock bound in it
//!
//! Direct instruction from the user (2026-09-21): **count assertions and a response-size cap,
//! never a wall-clock budget.** A wall-clock bound measures the machine, not the code — T-537 is
//! the precedent, where a 700 ms budget bought exactly one frame under load. Every assertion here
//! is a count that holds identically on an idle machine and on one running 28 test binaries.
//!
//! # Why none of it can go vacuously green
//!
//! Each test carries the **broken design as a control**, in the same run, over the same frames: a
//! second store configured [`PyramidConfig::coarse_on_demand`] instead of
//! [`PyramidConfig::coarse_live`]. The control is asserted to show the defect — a fold of many
//! producer tiles, generation runs on the read path — before the live store is asserted to show
//! none of it. A test that judged nothing would fail on the control first, and the pair is also
//! the standing "prove it goes red without the fix" evidence, permanently, rather than once.
//!
//! **T-585** then took back the residency T-571 paid for it: a coarse node holds its committed
//! rows in the codec's stored form and only its in-progress row as accumulator
//! (`history::live`). `a_coarse_node_holds_its_committed_rows_encoded_and_one_row_of_accumulator`
//! measures that from the buffers themselves against the arithmetic of T-571's one-full-tile-per-
//! node rule, and checks every node cell for cell against the on-demand control; the restart
//! test now runs an ODD number of rows, because replaying a node's pending row at reopen (the
//! defect T-585 found on the way) only shows with one in flight.

use super::*;
use crate::history::store::OpenTile;

/// A lattice big enough for the fold-from-nothing cost to be real: node (3, 3) sits eight folds
/// above node (0, 0). 16 cells per tile keeps the run short.
fn lat(scheme: u16) -> ViewLattice {
    ViewLattice {
        scheme,
        f_cell_hz: 1000.0,
        t_cell: Duration::from_secs(1),
        cells_per_block: 16,
        f_levels: 4,
        t_levels: 4,
    }
}

fn cfg_for(shape: ViewLattice) -> PyramidConfig {
    PyramidConfig {
        histogram: HistogramConfig {
            lo_db: -130.0,
            step_db: 5.0,
            bins: 30,
        },
        seal_lag: Duration::ZERO,
        checkpoint_interval: None,
        byte_budget: u64::MAX,
        ..PyramidConfig::view_lattice(shape)
    }
}

/// The same lattice, folded the old way: coarse nodes produced when a read asks for them.
fn on_demand(shape: ViewLattice) -> PyramidConfig {
    PyramidConfig {
        coarse_live: false,
        coarse_on_demand: true,
        ..cfg_for(shape)
    }
}

/// Frequency cells across the store, chosen so the coarsest frequency node is several blocks wide.
const N_BINS: usize = 16 * 8;
const BW: f64 = 1000.0;

fn run(p: &mut Pyramid, secs: i64) {
    run_each(p, secs, |_| {});
}

/// [`run`], calling `each` after every frame (T-585: residency is sampled per frame).
fn run_each(p: &mut Pyramid, secs: i64, mut each: impl FnMut(&Pyramid)) {
    let mut rng = Rng(0xC0FF_EE00_1234_5678);
    for s in 0..secs {
        let psd: Vec<f32> = (0..N_BINS)
            .map(|b| {
                let floor = -100.0 + 10.0 * rng.gamma(4).log10() as f32;
                lin(floor + if b % 23 == 5 { 40.0 } else { 0.0 })
            })
            .collect();
        p.ingest(&frame(T0 + s * S, S, 0.0, BW, &psd)).unwrap();
        each(p);
    }
}

/// The frequency and time extent of exactly **one** tile of `level`, at the origin of the run.
fn one_tile(p: &Pyramid, level: usize) -> (FreqRange, TimeRange) {
    let g = p.geometry().levels[level];
    let nf = p.geometry().nf as f64;
    let tb = T0.div_euclid(g.t_block_ns());
    (
        FreqRange::new(0.0, g.f_cell_hz * nf),
        TimeRange::new(ts(tb * g.t_block_ns()), ts((tb + 1) * g.t_block_ns())),
    )
}

/// **The assertion the ticket is for: serving a coarse tile reads ONE tile, not up to 1024.**
///
/// The control is the same address on an on-demand store: it folds a large number of producer
/// tiles for the same one answer, which is both the defect and the proof that this test judges
/// something.
#[test]
fn a_coarse_tile_is_served_from_one_committed_tile_not_a_thousand_producers() {
    let secs = 600;
    let shape = lat(41);
    let coarsest = shape.index(shape.f_levels - 1, shape.t_levels - 1);

    // --- the control: the read-time fold, measured ---
    let ctl_dir = TempDir::new("live-coarse-ctl");
    let mut ctl = Pyramid::open(&ctl_dir.0, on_demand(shape)).unwrap();
    run(&mut ctl, secs);
    ctl.seal_through(ts(T0 + secs * S)).unwrap();
    let (freq, time) = one_tile(&ctl, coarsest);
    ctl.materialize(coarsest, freq, time).unwrap();
    let ctl_folded = ctl.stats().producer_tiles_folded;
    println!("  control (on demand): one coarse tile folded {ctl_folded} producer tiles");
    assert!(
        ctl_folded > 64,
        "the control must show the defect, or this test proves nothing: {ctl_folded} producer \
         tiles folded for one address"
    );

    // --- live maintenance ---
    let dir = TempDir::new("live-coarse");
    let mut p = Pyramid::open(&dir.0, cfg_for(shape)).unwrap();
    run(&mut p, secs);
    p.seal_through(ts(T0 + secs * S)).unwrap();
    assert_eq!(
        p.stats().producer_tiles_folded,
        0,
        "capture must not have folded anything from producer TILES; it folds rows"
    );

    let (freq, time) = one_tile(&p, coarsest);
    p.reset_source_tiles_read();
    let built = p.materialize(coarsest, freq, time).unwrap();
    let h = query(
        &p,
        (freq.lo_hz, freq.hi_hz),
        (time.start.as_unix_nanos(), time.end.as_unix_nanos()),
        Resolution::Level(coarsest as u8),
    );
    let read_tiles = p.source_tiles_read();
    println!(
        "  live: one coarse tile built {built} tiles, folded {} producer tiles, read \
         {read_tiles} source tiles",
        p.stats().producer_tiles_folded
    );
    assert!(
        h.cells.iter().any(|c| c.observed()),
        "the coarse tile must actually hold the run, or the counts below judge an empty answer"
    );
    assert_eq!(built, 0, "a read must build nothing");
    assert_eq!(
        p.stats().producer_tiles_folded,
        0,
        "a read must fold no producer tiles"
    );
    assert_eq!(
        read_tiles, 1,
        "one coarse tile is ONE tile read, not up to {MAX_MATERIALIZE_TILES}"
    );
}

/// **No generation on the read path** — the review's R1 guard, over a whole viewport rather than
/// one address, and over the live edge as well as elapsed time.
#[test]
fn a_viewport_read_triggers_no_tile_generation_at_any_level() {
    let secs = 600;
    let shape = lat(42);
    let n_levels = shape.f_levels * shape.t_levels;
    let whole = FreqRange::new(0.0, N_BINS as f64 * BW);

    // The control first: the same viewport, folded on demand.
    let ctl_dir = TempDir::new("live-viewport-ctl");
    let mut ctl = Pyramid::open(&ctl_dir.0, on_demand(shape)).unwrap();
    run(&mut ctl, secs);
    // NOT sealed: this is the live edge, the case `docs/16` §5.2 said could only be folded per read.
    let span = TimeRange::new(ts(T0), ts(T0 + secs * S));
    for l in 0..n_levels {
        let _ = ctl.materialize(l, whole, span);
    }
    let ctl_built = ctl.stats().tiles_materialized;
    println!("  control (on demand): a viewport of every node ran {ctl_built} generation passes");
    assert!(
        ctl_built > 0,
        "the control must generate, or the assertion below is vacuous"
    );

    let dir = TempDir::new("live-viewport");
    let mut p = Pyramid::open(&dir.0, cfg_for(shape)).unwrap();
    run(&mut p, secs);
    let mut observed_levels = 0;
    for l in 0..n_levels {
        assert_eq!(
            p.materialize(l, whole, span).unwrap(),
            0,
            "level {l} was generated on the read path"
        );
        let h = query(
            &p,
            (0.0, N_BINS as f64 * BW),
            (T0, T0 + secs * S),
            Resolution::Level(l as u8),
        );
        observed_levels += usize::from(h.cells.iter().any(|c| c.observed()));
    }
    println!(
        "  live: {observed_levels} of {n_levels} nodes answered from data, 0 generation passes"
    );
    assert_eq!(
        observed_levels, n_levels,
        "every node must answer from what it holds — a node reading grey over data the store \
         holds is the display invariant's own failure case"
    );
    assert_eq!(
        p.stats().tiles_materialized,
        0,
        "generation ran on the read path"
    );
    assert_eq!(p.stats().producer_tiles_folded, 0);
}

/// **Per-arriving-row fold work is bounded per level and does not grow with node count.**
///
/// T-453's constraint, asserted the way the ticket demands: in counted work, not in seconds. The
/// footprint a row is folded over **halves** at every frequency step and is visited once per
/// `t_factor` rows at every time step, so both series converge — quadrupling the node count can
/// only move the total by the tail of a geometric series, never by 4×.
#[test]
fn per_arriving_row_fold_work_is_bounded_and_does_not_grow_with_node_count() {
    let secs = 1200;
    let measure = |shape: ViewLattice, tag: &str| -> (f64, u64, f64) {
        let dir = TempDir::new(tag);
        let mut p = Pyramid::open(&dir.0, cfg_for(shape)).unwrap();
        run(&mut p, secs);
        p.seal_through(ts(T0 + secs * S)).unwrap();
        let st = p.stats().clone();
        // Level-0 rows that closed: one per time cell per frequency block of the run.
        let blocks = N_BINS as u64 / u64::from(shape.cells_per_block);
        let rows = secs as u64 * blocks;
        let per_row = st.coarse_cells_folded as f64 / rows as f64;
        // T-585: cells COMMITTED (encoded) per arriving row, the same series one step up.
        let committed_per_row = st.coarse_cells_committed as f64 / rows as f64;
        println!(
            "  {tag}: {} nodes, {rows} level-0 rows closed, {} producer rows folded, \
             {} producer cells folded = {per_row:.1} cells per arriving row \
             ({:.2}x one row of {} cells); {} footprints committed, {} cells encoded = \
             {committed_per_row:.1} cells encoded per arriving row",
            shape.f_levels * shape.t_levels,
            st.coarse_rows_folded,
            st.coarse_cells_folded,
            per_row / f64::from(shape.cells_per_block),
            shape.cells_per_block,
            st.coarse_rows_committed,
            st.coarse_cells_committed,
        );
        assert!(
            st.coarse_cells_committed <= st.coarse_cells_folded,
            "a consumer cell is committed at most once per producer cell folded into it"
        );
        (per_row, st.coarse_rows_folded, committed_per_row)
    };

    let small = lat(43);
    let (small_per_row, small_rows, small_committed) = measure(small, "4x4");
    let big = ViewLattice {
        scheme: 44,
        f_levels: 8,
        t_levels: 8,
        ..lat(44)
    };
    let (big_per_row, big_rows, big_committed) = measure(big, "8x8");
    // T-585: the encode work per arriving row converges the same way the fold work does — a
    // committed footprint halves at every frequency step and a time step commits once per
    // `t_factor` rows — so quadrupling the node count cannot 4x it either.
    assert!(
        big_committed < 1.5 * small_committed,
        "64 nodes encode {big_committed:.1} cells per row against 16 nodes' {small_committed:.1}: \
         the per-row commit cost is growing with the node count"
    );

    assert!(small_rows > 0 && big_rows > 0, "no rows were folded at all");
    // Bounded per level: the whole cascade costs a small multiple of ONE producer row, whatever
    // the depth. 2 for the frequency arm (1 + 1/2 + 1/4 + …) and 1 for the time fold, halved
    // again at each time level: the closed form is under 6.
    let nf = f64::from(small.cells_per_block);
    assert!(
        big_per_row < 8.0 * nf,
        "{big_per_row:.1} cells per arriving row is more than 8 rows' worth ({nf} cells a row)"
    );
    // And quadrupling the node count (16 -> 64) moves it by the tail of the series, not by 4x.
    assert!(
        big_per_row < 1.5 * small_per_row,
        "64 nodes cost {big_per_row:.1} cells per row against 16 nodes' {small_per_row:.1}: the \
         per-row cost is growing with the node count"
    );
    assert!(
        big_per_row >= small_per_row,
        "the deeper lattice folded LESS ({big_per_row:.1} < {small_per_row:.1}) — something is \
         not being maintained"
    );
}

/// **Commits every N are counted in WRITES, not guessed** (T-453's own regression was file writes,
/// 5.1× wall for identical CPU and RSS — the runs were waiting, not computing).
///
/// A row commit must cost **no write at all**: a tile is written when it seals, exactly as before,
/// so the number of writes over a run equals the number of tiles that sealed and is a tiny
/// fraction of the number of rows committed.
#[test]
fn committing_a_row_writes_nothing_and_a_tile_is_still_written_once() {
    let secs = 1200;
    let shape = lat(45);
    let dir = TempDir::new("live-writes");
    let mut p = Pyramid::open(&dir.0, cfg_for(shape)).unwrap();
    run(&mut p, secs);
    p.seal_through(ts(T0 + secs * S)).unwrap();
    let st = p.stats().clone();

    // Every tile file on disk, counted by reading the directory back.
    let root = dir.0.join("history").join(format!("s{}", shape.scheme));
    let mut files = 0u64;
    for l in 0..shape.f_levels * shape.t_levels {
        let dir_l = root.join(format!("L{l}"));
        if let Ok(fs) = std::fs::read_dir(&dir_l) {
            for f in fs.flatten() {
                if let Ok(ts) = std::fs::read_dir(f.path()) {
                    files += ts
                        .flatten()
                        .filter(|t| t.path().extension().is_some_and(|e| e == "tile"))
                        .count() as u64;
                }
            }
        }
    }
    println!(
        "  {} rows committed into coarse nodes, {} tile writes, {files} tile files on disk",
        st.coarse_rows_folded, st.tiles_written
    );
    assert!(
        st.coarse_rows_folded > 1000,
        "the run was too short to judge"
    );
    assert_eq!(
        st.tiles_written, files,
        "a tile was written more than once: a row commit must not write"
    );
    assert!(
        st.tiles_written * 8 < st.coarse_rows_folded,
        "{} writes for {} committed rows — commits are writing",
        st.tiles_written,
        st.coarse_rows_folded
    );
    assert_eq!(
        st.checkpoints_written, 0,
        "no checkpointing is configured here"
    );
}

/// Total frames each node holds over the whole run, per node — the quantity that says whether a
/// row reached a coarse level at all.
fn frames_per_node(p: &Pyramid, shape: ViewLattice, secs: i64) -> Vec<u64> {
    (0..shape.f_levels * shape.t_levels)
        .map(|l| {
            query(
                p,
                (0.0, N_BINS as f64 * BW),
                (T0, T0 + secs * S),
                Resolution::Level(l as u8),
            )
            .cells
            .iter()
            .map(|c| u64::from(c.frames))
            .sum()
        })
        .collect()
}

/// **A checkpoint closes a time column, and that column is a finished row.**
///
/// It was dropped: under `coarse_live` the only two paths into the cascade are the column advance
/// in `ingest` and the pre-seal flush, and a column closed by `checkpoint` reaches neither — the
/// next frame finds `col_t` at `None`, and the seal's own `close_column` returns `None`. The
/// shipped `view_config` inherits a 60 s checkpoint interval over a 1 s time cell, so it fired
/// every sixtieth row in production, and the loss was on disk and permanent: a burst inside a
/// dropped second is there at the finest zoom and **gone when you zoom out**.
///
/// Every other test in this file sets `checkpoint_interval: None`, which is exactly why none of
/// them saw it. This one turns checkpointing on and compares every node against the on-demand
/// control, which folds open producers and so never had the hole.
#[test]
fn a_checkpoint_does_not_swallow_the_row_it_closes() {
    let secs = 600;
    let shape = lat(46);
    // Every 60 rows, as the shipped config does.
    let every = Duration::from_secs(60);

    let ctl_dir = TempDir::new("live-cp-ctl");
    let mut ctl = Pyramid::open(
        &ctl_dir.0,
        PyramidConfig {
            checkpoint_interval: Some(every),
            ..on_demand(shape)
        },
    )
    .unwrap();
    run(&mut ctl, secs);
    // **Both stores are sealed past EVERY coarse block before they are compared**, and that is
    // load-bearing rather than tidiness. An unsealed live store's ACCUMULATORS are
    // legitimately behind at each time level — the cascade commits every N, so a chain of
    // in-progress rows lags the edge by up to 2^j finest rows (measured: 0, 2, 4 and 8 at time
    // levels 0 to 3) — while the on-demand control folds open producers and lags by nothing. That
    // is the same shape as the defect and would mask it. A read undoes the lag (T-583), but this
    // test compares node against node, and both sides here are read the same way. Sealing well past the run ends every
    // coarse tile's block, which flushes every pending row.
    let flush = ts(T0 + 2048 * S);
    ctl.seal_through(flush).unwrap();
    let whole = FreqRange::new(0.0, N_BINS as f64 * BW);
    let span = TimeRange::new(ts(T0), ts(T0 + secs * S));
    for l in 0..shape.f_levels * shape.t_levels {
        ctl.materialize(l, whole, span).unwrap();
    }
    let want = frames_per_node(&ctl, shape, secs);

    let dir = TempDir::new("live-cp");
    let mut p = Pyramid::open(
        &dir.0,
        PyramidConfig {
            checkpoint_interval: Some(every),
            ..cfg_for(shape)
        },
    )
    .unwrap();
    run(&mut p, secs);
    assert!(
        p.stats().checkpoints_written > 0,
        "no checkpoint fired, so this test judged nothing"
    );
    p.seal_through(flush).unwrap();
    let got = frames_per_node(&p, shape, secs);

    println!(
        "  {} checkpoints over {secs} rows; frames per node, control vs live:",
        p.stats().checkpoints_written
    );
    let mut short = Vec::new();
    for (l, (&w, &g)) in want.iter().zip(&got).enumerate() {
        let (i, j) = shape.coords(l);
        if w != g {
            short.push(format!("({i},{j}) {g} vs {w} (short by {})", w - g));
        }
    }
    println!(
        "    node (0,0) {} / {}, node (0,1) {} / {}, nodes short: {}",
        got[shape.index(0, 0)],
        want[shape.index(0, 0)],
        got[shape.index(0, 1)],
        want[shape.index(0, 1)],
        if short.is_empty() {
            "none".to_string()
        } else {
            short.join(", ")
        }
    );
    assert!(
        want.iter().all(|&w| w > 0),
        "the control holds nothing, so an equality below would be vacuous"
    );
    assert_eq!(
        got, want,
        "a coarse node is short of the control: a checkpoint's closed column was dropped"
    );
}

/// **A restart must not lose the open level-0 tile from every coarse node.**
///
/// The reopen rebuild re-folds *sealed* children, which is everything a seal-time scheme is fed
/// by. A live scheme is also fed by the rows of the currently open level-0 tile, and that tile
/// comes back from its checkpoint still open. Without the refold its closed rows reach level 0
/// and no coarser node, and the coarse tiles covering them are never written at all — at the
/// shipped 64 x 1 s level-0 tile, up to 64 consecutive seconds of **grey over time that was
/// observed**, which the display invariant calls a bug twice over.
#[test]
fn a_restart_keeps_the_open_tiles_rows_in_every_coarse_node() {
    // **Odd, deliberately (T-585).** 601 closed rows leaves node (0, 1) with a PENDING row at the
    // restart (300 complete pairs and one half); 600 left none, and the reopen replay of a
    // pending row was never exercised. Replaying it as though committed folded its partial
    // contents into the level above, and its completion then folded them again: measured on
    // T-571's code, every node above (0, 1) came back with MORE frames than the control —
    // (3, 3) 77568 against 76928. The replay now walks a live node's committed footprints only.
    let secs = 601;
    let shape = lat(47);
    let cfg = || PyramidConfig {
        checkpoint_interval: Some(Duration::from_secs(60)),
        ..cfg_for(shape)
    };

    // The control is the same store WITHOUT the restart: the run is identical, so any node that
    // differs differs because of the reopen. Both are sealed past every coarse block first, so
    // neither has rows in flight, which T-583's read-time preview would otherwise fold into one
    // side's answer and not the other's.
    let flush = ts(T0 + 2048 * S);
    let ctl_dir = TempDir::new("live-restart-ctl");
    let mut ctl = Pyramid::open(&ctl_dir.0, cfg()).unwrap();
    run(&mut ctl, secs);
    ctl.seal_through(flush).unwrap();
    let want = frames_per_node(&ctl, shape, secs);

    let dir = TempDir::new("live-restart");
    let mut p = Pyramid::open(&dir.0, cfg()).unwrap();
    run(&mut p, secs);
    p.close().unwrap();
    let mut p = Pyramid::open(&dir.0, cfg()).unwrap();
    assert!(
        !p.open_keys(0).is_empty(),
        "no open level-0 tile came back from its checkpoint, so this test judged nothing"
    );
    p.seal_through(flush).unwrap();
    let got = frames_per_node(&p, shape, secs);

    let mut short = Vec::new();
    for (l, (&w, &g)) in want.iter().zip(&got).enumerate() {
        let (i, j) = shape.coords(l);
        if w != g {
            short.push(format!("({i},{j}) {g} vs {w}"));
        }
    }
    println!(
        "  after restart: node (0,1) {} / {}, node ({},{}) {} / {}, nodes short: {}",
        got[shape.index(0, 1)],
        want[shape.index(0, 1)],
        shape.f_levels - 1,
        shape.t_levels - 1,
        got[shape.index(shape.f_levels - 1, shape.t_levels - 1)],
        want[shape.index(shape.f_levels - 1, shape.t_levels - 1)],
        if short.is_empty() {
            "none".to_string()
        } else {
            short.join(", ")
        }
    );
    assert!(
        want.iter().all(|&w| w > 0),
        "the control holds nothing, so an equality below would be vacuous"
    );
    assert_eq!(
        got, want,
        "a coarse node lost rows across the restart: the reloaded open level-0 tile was not \
         re-folded"
    );
}

/// A run of `per_cell` frames per time cell. `disorder` swaps the arrival order of the two frames
/// either side of every `disorder`-th cell boundary, so the earlier of the two arrives after its
/// own time cell has already closed. Frame content is a function of the frame's own time, so a
/// swap changes arrival order and nothing else.
fn run_frames(p: &mut Pyramid, secs: i64, per_cell: i64, disorder: i64) {
    let dur = S / per_cell;
    let n = secs * per_cell;
    let mut times: Vec<i64> = (0..n).map(|k| T0 + k * dur).collect();
    let mut c = disorder;
    while disorder > 0 && c * per_cell < n {
        let i = (c * per_cell) as usize;
        times.swap(i - 1, i);
        c += disorder;
    }
    for &t in &times {
        let mut rng = Rng((t as u64) ^ 0x9E37_79B9_7F4A_7C15);
        let psd: Vec<f32> = (0..N_BINS)
            .map(|b| {
                let floor = -100.0 + 10.0 * rng.gamma(4).log10() as f32;
                lin(floor + if b % 23 == 5 { 40.0 } else { 0.0 })
            })
            .collect();
        p.ingest(&frame(t, dur, 0.0, BW, &psd)).unwrap();
    }
}

/// The frames every node holds over a run of `secs`, live against the on-demand control, and the
/// live store's own out-of-order count: `(live, control, frames_out_of_order)`.
fn live_vs_control(
    shape: ViewLattice,
    secs: i64,
    lag: Duration,
    checkpoint: Option<Duration>,
    disorder: i64,
) -> (Vec<u64>, Vec<u64>, u64) {
    let flush = ts(T0 + 4096 * S);
    let whole = FreqRange::new(0.0, N_BINS as f64 * BW);
    let span = TimeRange::new(ts(T0), ts(T0 + secs * S));

    // The control re-folds the open producer on every read, so it never had the hole — and it
    // runs the same frames under the same lag, so it accepts and refuses exactly what live does.
    let ctl_dir = TempDir::new("t584-ctl");
    let mut ctl = Pyramid::open(
        &ctl_dir.0,
        PyramidConfig {
            seal_lag: lag,
            checkpoint_interval: checkpoint,
            ..on_demand(shape)
        },
    )
    .unwrap();
    run_frames(&mut ctl, secs, 4, disorder);
    ctl.seal_through(flush).unwrap();
    for l in 0..shape.f_levels * shape.t_levels {
        ctl.materialize(l, whole, span).unwrap();
    }
    let want = frames_per_node(&ctl, shape, secs);

    let dir = TempDir::new("t584-live");
    let mut p = Pyramid::open(
        &dir.0,
        PyramidConfig {
            seal_lag: lag,
            checkpoint_interval: checkpoint,
            ..cfg_for(shape)
        },
    )
    .unwrap();
    run_frames(&mut p, secs, 4, disorder);
    // Sealed past every coarse block, so neither store carries the in-progress-row lag (T-583)
    // — nor, for the live one, the `seal_lag` a finished row now waits out (T-584).
    p.seal_through(flush).unwrap();
    assert_eq!(p.stats().frames_late, ctl.stats().frames_late);
    (
        frames_per_node(&p, shape, secs),
        want,
        p.stats().frames_out_of_order,
    )
}

/// Nodes whose frame count is short of the control, as `name lo/hi -x%`.
fn shortfall(shape: ViewLattice, got: &[u64], want: &[u64]) -> Vec<String> {
    got.iter()
        .zip(want)
        .enumerate()
        .filter(|(_, (g, w))| g != w)
        .map(|(l, (g, w))| {
            let (i, j) = shape.coords(l);
            format!(
                "({i},{j}) {g}/{w} -{:.2}%",
                100.0 * (*w - *g) as f64 / *w as f64
            )
        })
        .collect()
}

/// **T-584: a frame whose time cell has already closed reaches every coarse node, not just
/// level 0.**
///
/// A late frame is folded into its level-0 cell **in place** — [`Tile::add_value`] writes the
/// count, the max, the power sum and the observed seconds, and [`Tile::add_late_occupancy`] the
/// occupancy. T-571's cascade folded a row upwards the instant its **column closed**, which is
/// earlier: the row had gone up before the frame arrived, so the value sat on disk at level 0 and
/// was absent from every zoom above it — the same "present at the finest zoom, gone when you zoom
/// out" shape T-571 fixed for checkpoints, and for the same reason.
///
/// Two ordinary windows produce it, and this test drives both at once: mild frame disorder inside
/// a cell, and every frame arriving in the cell a checkpoint has just closed (at the shipped 60 s
/// interval over a 1 s cell, one cell in sixty for the rest of its second).
///
/// # What the fix is, and what the two pairs here measure
///
/// A row is folded upwards when the ingest clock has left its cell by `seal_lag` — the store's
/// own declared tolerance for frames arriving out of order, the number that already decides when
/// a *tile* may seal, applied one level down. So:
///
/// - **`seal_lag` zero** — no tolerance declared, which is what every other test in this file
///   configures — is T-571's behaviour exactly, and it is the first pair here: the loss is real
///   and is printed as a percentage. That pair is this test's control against going vacuously
///   green, in the same run, over the same frames.
/// - **`seal_lag` at the shipped 2 s** is the second pair, and it must match the on-demand
///   control node for node.
#[test]
fn a_frame_arriving_after_its_cell_closed_reaches_every_coarse_node() {
    let secs = 240;
    // Every tenth cell boundary, and a checkpoint every ten seconds: the shipped 60:1
    // checkpoint-to-cell ratio compressed, so a short run still crosses the window many times.
    let (disorder, checkpoint) = (10, Some(Duration::from_secs(10)));

    // --- the control: no disorder tolerance declared, which is what T-571 shipped ---
    let none = lat(48);
    let (got, want, late) = live_vs_control(none, secs, Duration::ZERO, checkpoint, disorder);
    let short = shortfall(none, &got, &want);
    println!(
        "  seal_lag 0 (T-571's rule): {late} frames arrived after their cell closed; nodes \
         short: {}",
        if short.is_empty() {
            "none".to_string()
        } else {
            short.join(", ")
        }
    );
    assert!(
        late > 0,
        "no frame arrived out of order, so this test judged nothing"
    );
    assert_eq!(
        got[none.index(0, 0)],
        want[none.index(0, 0)],
        "node (0, 0) must hold every frame either way — the defect is above it, not at it"
    );
    assert!(
        !short.is_empty(),
        "with no tolerance declared the late frames must be missing from the coarse nodes, or \
         the equality below proves nothing"
    );

    // --- the shipped 2 s: every late frame reaches every node ---
    let shipped = lat(49);
    let (got, want, late) =
        live_vs_control(shipped, secs, Duration::from_secs(2), checkpoint, disorder);
    let short = shortfall(shipped, &got, &want);
    println!(
        "  seal_lag 2 s: {late} frames arrived after their cell closed; nodes short: {}",
        if short.is_empty() {
            "none".to_string()
        } else {
            short.join(", ")
        }
    );
    assert!(late > 0, "the same frames must still be arriving late");
    assert!(
        want.iter().all(|&w| w > 0),
        "the control holds nothing, so an equality below would be vacuous"
    );
    assert_eq!(
        got, want,
        "a coarse node is short of the control: a frame that arrived after its time cell closed \
         reached level 0 and no zoom above it"
    );
}

/// The newest row of `level` that holds anything, read **straight out of the accumulators** — so
/// it is what the live cascade has propagated, before any read-time preview. Absolute ns.
fn newest_committed_row_ns(p: &Pyramid, level: usize) -> Option<i64> {
    let g = p.geometry().levels[level];
    p.open[level]
        .iter()
        .filter_map(|(&(_, tb), t)| {
            // T-585: a coarse node of a live lattice holds its committed rows encoded and its
            // in-progress row as accumulator; both are what the cascade has put there.
            let (nt, newest) = match t {
                OpenTile::Full(t) => (
                    t.nt,
                    (0..t.nt)
                        .rev()
                        .find(|&r| (0..t.nf).any(|f| t.count[r * t.nf + f] > 0)),
                ),
                OpenTile::Live(t) => (t.nt, t.newest_row()),
            };
            newest.map(|r| (tb * nt as i64 + r as i64) * g.t_cell_ns)
        })
        .max()
}

/// The newest row a **read** of `level` answers as observed, as the absolute ns its cell starts at.
fn newest_read_row_ns(p: &Pyramid, level: usize, secs: i64) -> Option<i64> {
    let h = query(
        p,
        (0.0, N_BINS as f64 * BW),
        (T0, T0 + secs * S),
        Resolution::Level(level as u8),
    );
    (0..h.nt)
        .rev()
        .find(|&t| (0..h.nf).any(|f| h.cell(t, f).observed()))
        .map(|t| h.time_of(t).as_unix_nanos())
}

/// **T-583: a zoomed-out node shows its IN-PROGRESS row instead of trailing the live edge.**
///
/// T-571 measured the trail and left the product call open. It is answered *show it*, for the
/// reason CLAUDE.md gives twice: *"whenever data exists for that window it must be shown"*, and
/// *"a level that downsamples every N rows adjusts its in-progress top row as rows arrive"*. The
/// rows exist, recorded and held; a node that waits for its own commit is "we have it but didn't
/// render it", and at four time levels over a 1 s cell it was up to 7 s of it.
///
/// The trail is **still there in the accumulators** — the cascade must keep propagating on commit,
/// which is what makes it exactly-once — so this test measures it there first, in rows, and that
/// measurement is the proof the assertion below judges something. The fix is on the read.
#[test]
fn a_coarse_node_shows_its_in_progress_row_instead_of_trailing_the_live_edge() {
    // 98 rows leaves the finest level closed through row 96 with row 97 still buffering, which is
    // the *worst* phase for the cascade: the 2 s node has not committed (96, 97), so neither the
    // 4 s nor the 8 s node has heard of second 96 at all.
    let secs = 98;
    let shape = lat(48);
    let n_levels = shape.f_levels * shape.t_levels;
    let dir = TempDir::new("live-inprogress");
    let mut p = Pyramid::open(&dir.0, cfg_for(shape)).unwrap();
    run(&mut p, secs);
    // NOT sealed: this is the live edge, which is the only place the trail exists.

    // --- the trail, measured in the accumulators, per node ---
    let closed_ns = p.open[0]
        .iter()
        .filter_map(|(&(_, tb), t)| match t {
            OpenTile::Full(t) => t.col_done.map(|c| (tb * t.nt as i64 + c as i64) * S),
            OpenTile::Live(_) => None,
        })
        .max()
        .expect("the finest level has closed a row");
    let mut trail = vec![0i64; n_levels];
    for (l, trail) in trail.iter_mut().enumerate() {
        let cell = p.geometry().levels[l].t_cell_ns;
        // The row of this node that COVERS the finest closed row is the newest it could hold.
        let want = closed_ns.div_euclid(cell) * cell;
        // No open tile at all is the extreme of the same trail: the node's last block sealed and
        // its next has not been opened, because nothing has committed into it yet.
        let got = newest_committed_row_ns(&p, l).unwrap_or(want - cell);
        *trail = (want - got) / S;
    }
    println!("  the cascade's own trail, in finest rows, per node:");
    for j in 0..shape.t_levels {
        let row: Vec<String> = (0..shape.f_levels)
            .map(|i| format!("({i},{j}) {}", trail[shape.index(i, j)]))
            .collect();
        println!("    {}", row.join("  "));
    }
    assert!(
        trail.iter().any(|&t| t > 0),
        "no node is behind, so the read assertions below are vacuous — the phase of this run no \
         longer exercises the trail"
    );

    // --- what a READ answers: no node trails, at any zoom ---
    let base = newest_read_row_ns(&p, 0, secs).expect("the finest level answers a row");
    assert_eq!(
        base,
        closed_ns + S,
        "the finest level must answer its own open column (T-453's `column_preview`), or the \
         comparison below is against the wrong edge"
    );
    for l in 0..n_levels {
        let (i, j) = shape.coords(l);
        let cell = p.geometry().levels[l].t_cell_ns;
        let got = newest_read_row_ns(&p, l, secs)
            .unwrap_or_else(|| panic!("node ({i},{j}) answered no row at all"));
        assert!(
            got <= base && base < got + cell,
            "node ({i},{j}) trails the live edge: its newest row starts at {}, which does not \
             cover the finest level's newest row at {}",
            (got - T0) / S,
            (base - T0) / S
        );
    }

    // --- and it is the same data, folded once: every node holds every frame, none twice ---
    let got = frames_per_node(&p, shape, secs);
    // The control folds open producers on every read, so it trails by nothing by construction —
    // the value this read must now match, from a scheme that never had the defect.
    let ctl_dir = TempDir::new("live-inprogress-ctl");
    let mut ctl = Pyramid::open(&ctl_dir.0, on_demand(shape)).unwrap();
    run(&mut ctl, secs);
    let whole = FreqRange::new(0.0, N_BINS as f64 * BW);
    let span = TimeRange::new(ts(T0), ts(T0 + secs * S));
    for l in 0..n_levels {
        let _ = ctl.materialize(l, whole, span);
    }
    let want = frames_per_node(&ctl, shape, secs);
    println!(
        "  frames per node at the live edge: node (0,0) {}, coarsest {} (control {})",
        got[0],
        got[shape.index(shape.f_levels - 1, shape.t_levels - 1)],
        want[shape.index(shape.f_levels - 1, shape.t_levels - 1)]
    );
    assert!(
        want.iter().all(|&w| w > 0),
        "the control holds nothing, so the equality below would be vacuous"
    );
    assert_eq!(
        got, want,
        "a node is short of (or ahead of) the whole run: the preview lost a row, or folded one \
         twice"
    );
    assert!(
        p.preview_rows_folded() > 0,
        "no row was folded on the read path, so nothing was previewed"
    );
}

/// **The preview costs a read of elapsed time nothing, and capture nothing at all.**
///
/// T-453's constraint is that work on the capture thread is paid whether or not anyone looks. The
/// preview is therefore on the read, like `column_preview`: it runs only where rows are in flight
/// *below the address being read*, which is the live edge and nowhere else.
#[test]
fn previewing_the_in_progress_row_costs_capture_nothing_and_elapsed_reads_nothing() {
    let secs = 200;
    let shape = lat(49);
    let n_levels = shape.f_levels * shape.t_levels;
    let dir = TempDir::new("live-preview-cost");
    let mut p = Pyramid::open(&dir.0, cfg_for(shape)).unwrap();
    run(&mut p, secs);
    assert_eq!(
        p.preview_rows_folded(),
        0,
        "capture folded a preview row: the read path's work must not run on the capture thread"
    );

    // An elapsed window, well behind the edge: nothing is in flight under it.
    let past = TimeRange::new(ts(T0), ts(T0 + 32 * S));
    for l in 0..n_levels {
        let h = query(
            &p,
            (0.0, N_BINS as f64 * BW),
            (past.start.as_unix_nanos(), past.end.as_unix_nanos()),
            Resolution::Level(l as u8),
        );
        assert!(h.cells.iter().any(|c| c.observed()), "level {l} read grey");
    }
    assert_eq!(
        p.preview_rows_folded(),
        0,
        "a read of elapsed time previewed rows: only the live edge has any in flight"
    );

    // The live edge does preview, and pays a bounded number of rows for it: at most one per level
    // of a producer chain per open tile it consults, never a tile fold.
    let before = p.stats().producer_tiles_folded;
    let mut folded = 0;
    for l in 0..n_levels {
        let _ = query(
            &p,
            (0.0, N_BINS as f64 * BW),
            (T0, T0 + secs * S),
            Resolution::Level(l as u8),
        );
        folded = p.preview_rows_folded();
    }
    println!("  {folded} rows previewed for a read of every node at the live edge");
    assert!(folded > 0, "the live edge previewed nothing");
    assert_eq!(
        p.stats().producer_tiles_folded,
        before,
        "the preview folded a producer TILE: it folds rows into clones, nothing else"
    );
    assert_eq!(
        p.stats().tiles_materialized,
        0,
        "generation ran on the read path"
    );
}

/// **T-585: a coarse node holds its committed rows ENCODED, and only one row as accumulator.**
///
/// T-571 stated its residency decision rather than hiding it: every node held a whole open
/// accumulator for the tile it was filling, because the tile is the write unit and a committed
/// row had to live somewhere until the seal. That is the defect this test is for, and it no
/// longer exists in the code to run as a control — so the control is its **arithmetic**: at the
/// same sample, what every open coarse tile would hold as a full [`Tile`], taken from that type's
/// own buffers. The on-demand store is still the control for **correctness**: every node's sealed
/// cells must agree with it, so the encode → hold → decode → seal path is proven to lose nothing
/// beyond the quantisation the sealed file already applies.
///
/// Every assertion is a measured byte count or an event count. No wall clock.
#[test]
fn a_coarse_node_holds_its_committed_rows_encoded_and_one_row_of_accumulator() {
    use super::super::tile::Tile;
    use hk_model::TileKey;

    let secs = 600;
    let shape = lat(48);
    let n_levels = shape.f_levels * shape.t_levels;
    let whole = FreqRange::new(0.0, N_BINS as f64 * BW);
    let span = TimeRange::new(ts(T0), ts(T0 + secs * S));
    // Sealed past EVERY coarse block before the comparison, for the reason
    // `a_checkpoint_does_not_swallow_the_row_it_closes` gives: an unsealed live store lags at
    // each time level by design (T-583), and that lag has the same shape as a loss.
    let flush = ts(T0 + 2048 * S);

    // --- the correctness control: the on-demand fold ---
    let ctl_dir = TempDir::new("live-enc-ctl");
    let mut ctl = Pyramid::open(&ctl_dir.0, on_demand(shape)).unwrap();
    run(&mut ctl, secs);
    ctl.seal_through(flush).unwrap();
    for l in 0..n_levels {
        ctl.materialize(l, whole, span).unwrap();
    }

    // --- live, residency sampled after every frame ---
    let dir = TempDir::new("live-enc");
    let mut p = Pyramid::open(&dir.0, cfg_for(shape)).unwrap();
    let g = p.geometry().clone();
    let bins = usize::from(p.config().histogram.bins);
    let nt = shape.cells_per_block as usize;
    // What ONE open tile of each level costs as a full accumulator: T-571's unit of residency.
    let full_tile: Vec<usize> = (0..n_levels)
        .map(|l| {
            let key = TileKey {
                scheme: shape.scheme,
                level: l as u8,
                f_block: 0,
                t_block: 0,
            };
            Tile::new(key, g.nf, &g.levels[l], bins).resident_bytes()
        })
        .collect();
    /// Stored-form bytes one committed cell can cost before compression: `max`/`mean` i16,
    /// `occupancy`/`occupancy_max`/`coverage` u16, `frames` varint (≤ 3 B at these counts),
    /// plus its bit of the observed bitmap. The bound is over the RAW form so it does not depend
    /// on how well zstd did.
    const ENC_RAW_MAX: usize = 2 + 2 + 2 + 2 + 2 + 3 + 1;
    /// Generous per-segment bookkeeping (the `Segment` struct itself), and a row is committed in
    /// at most two footprints here (a frequency-coarser node's row is filled by two producer
    /// tiles), so at most `2 * nt` segments per tile.
    const SEGMENT_OVERHEAD: usize = 64;

    let mut peak = ResidentBytes::default();
    let mut peak_t571 = 0usize;
    let mut ever_live = 0usize;
    let mut worst_rows_per_tile = 0.0f64;
    run_each(&mut p, secs, |p| {
        let r = p.resident_bytes();
        ever_live = ever_live.max(r.live_tiles);
        if r.live_tiles > 0 {
            worst_rows_per_tile =
                worst_rows_per_tile.max(r.live_acc_rows as f64 / r.live_tiles as f64);
        }
        if r.live_bytes() > peak.live_bytes() {
            peak = r;
            peak_t571 = (1..n_levels)
                .map(|l| p.open_keys(l).len() * full_tile[l])
                .sum();
        }
    });
    let st = p.stats().clone();
    let bound = peak.live_tiles
        * (3 * g.nf * Pyramid::ROW_ACC_BYTES_PER_CELL
            + nt * g.nf * ENC_RAW_MAX
            + 2 * nt * SEGMENT_OVERHEAD);
    let kb = |b: usize| b as f64 / 1024.0;
    println!(
        "  T-585: {n_levels} nodes; at the coarse-residency peak {} live tiles held {} \
         accumulator rows ({:.1} KB) and {} encoded segments ({:.1} KB) = {:.1} KB, where T-571 \
         held {:.1} KB of full accumulators ({:.1}%); raw bound {:.1} KB. Worst rows per tile \
         {worst_rows_per_tile:.2}. {} footprints committed: {} cells at {:.2} B/cell stored \
         ({:.2} B/cell raw, zstd {:.2}x)",
        peak.live_tiles,
        peak.live_acc_rows,
        kb(peak.live_acc_bytes),
        peak.live_segments,
        kb(peak.live_encoded_bytes),
        kb(peak.live_bytes()),
        kb(peak_t571),
        100.0 * peak.live_bytes() as f64 / peak_t571.max(1) as f64,
        kb(bound),
        st.coarse_rows_committed,
        st.coarse_cells_committed,
        st.coarse_row_bytes_stored as f64 / st.coarse_cells_committed.max(1) as f64,
        st.coarse_row_bytes_raw as f64 / st.coarse_cells_committed.max(1) as f64,
        st.coarse_row_bytes_raw as f64 / st.coarse_row_bytes_stored.max(1) as f64,
    );

    // The mechanism: every coarse node is live-maintained (the inverse of T-453's zero) ...
    assert!(
        ever_live >= n_levels - 1,
        "only {ever_live} of {} coarse nodes ever held an open tile",
        n_levels - 1
    );
    assert!(
        st.coarse_rows_committed > 1000,
        "the run was too short to judge"
    );
    // ... and what each one holds is ONE row of accumulator (two while a gap flush's row waits
    // for its consumers), never the tile.
    assert!(
        worst_rows_per_tile <= 2.0,
        "{worst_rows_per_tile:.2} accumulator rows per live tile: committed rows are staying \
         resident as accumulator"
    );
    assert!(
        peak.live_acc_rows <= 2 * peak.live_tiles,
        "{} accumulator rows over {} live tiles",
        peak.live_acc_rows,
        peak.live_tiles
    );
    // The bytes, against a bound that is arithmetic over the raw stored form.
    assert!(
        peak.live_bytes() <= bound,
        "coarse residency {} B exceeds its raw bound {bound} B",
        peak.live_bytes()
    );
    // And against what T-571 held at the same instant. At this lattice's 16-row tiles the raw
    // form alone is about half a full accumulator; the shipped 64-row tile is where it pays
    // (`hk-pipeline/tests/live_edge_tiles.rs::the_view_lattices_floor_costs_what_the_settings_\
    // doc_says_it_costs` measures that one).
    assert!(
        peak.live_bytes() * 4 < peak_t571 * 3,
        "coarse residency {} B is not under 75% of T-571's {peak_t571} B of full accumulators",
        peak.live_bytes()
    );
    // The stored form is the codec's: well under a full cell's 44 B even before compression.
    assert!(
        st.coarse_row_bytes_raw <= st.coarse_cells_committed * ENC_RAW_MAX as u64,
        "{} raw bytes for {} committed cells",
        st.coarse_row_bytes_raw,
        st.coarse_cells_committed
    );

    // --- correctness: every node, cell for cell, against the on-demand control ---
    p.seal_through(flush).unwrap();
    let mut compared = 0usize;
    for l in 0..n_levels {
        let want = query(
            &ctl,
            (0.0, N_BINS as f64 * BW),
            (T0, T0 + secs * S),
            Resolution::Level(l as u8),
        );
        let got = query(
            &p,
            (0.0, N_BINS as f64 * BW),
            (T0, T0 + secs * S),
            Resolution::Level(l as u8),
        );
        assert_eq!(want.cells.len(), got.cells.len(), "level {l}: grid shape");
        let (i, j) = shape.coords(l);
        for (k, (a, b)) in got.cells.iter().zip(&want.cells).enumerate() {
            assert_eq!(a.observed(), b.observed(), "({i},{j}) cell {k}: observed");
            if !a.observed() {
                continue;
            }
            compared += 1;
            assert_eq!(a.frames, b.frames, "({i},{j}) cell {k}: frames");
            assert!(
                (a.max_db - b.max_db).abs() <= 0.05,
                "({i},{j}) cell {k}: max {} vs {}",
                a.max_db,
                b.max_db
            );
            // One centi-dB re-quantisation per level above the first, at most.
            assert!(
                (a.mean_db - b.mean_db).abs() <= 0.1,
                "({i},{j}) cell {k}: mean {} vs {}",
                a.mean_db,
                b.mean_db
            );
            assert!(
                (a.coverage - b.coverage).abs() <= 2e-3,
                "({i},{j}) cell {k}: coverage {} vs {}",
                a.coverage,
                b.coverage
            );
            assert!(
                (a.occupancy - b.occupancy).abs() <= 2e-3,
                "({i},{j}) cell {k}: occupancy {} vs {}",
                a.occupancy,
                b.occupancy
            );
            assert!(
                (a.occupancy_max - b.occupancy_max).abs() <= 2e-3,
                "({i},{j}) cell {k}: occupancy_max {} vs {}",
                a.occupancy_max,
                b.occupancy_max
            );
        }
    }
    println!("  {compared} observed cells over {n_levels} nodes agree with the on-demand control");
    assert!(compared > 0, "nothing was compared");
}
