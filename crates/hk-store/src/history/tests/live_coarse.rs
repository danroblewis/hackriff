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

use super::*;

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
    let mut rng = Rng(0xC0FF_EE00_1234_5678);
    for s in 0..secs {
        let psd: Vec<f32> = (0..N_BINS)
            .map(|b| {
                let floor = -100.0 + 10.0 * rng.gamma(4).log10() as f32;
                lin(floor + if b % 23 == 5 { 40.0 } else { 0.0 })
            })
            .collect();
        p.ingest(&frame(T0 + s * S, S, 0.0, BW, &psd)).unwrap();
    }
}

/// The frequency and time extent of exactly **one** tile of `level`, at the origin of the run.
fn one_tile(p: &Pyramid, level: usize) -> (FreqRange, TimeRange) {
    let g = p.geometry().levels[level];
    let nf = p.geometry().nf as f64;
    let tb = T0.div_euclid(g.t_block_ns());
    (
        FreqRange::new(0.0, g.f_cell_hz * nf),
        TimeRange::new(
            ts(tb * g.t_block_ns()),
            ts((tb + 1) * g.t_block_ns()),
        ),
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
    let measure = |shape: ViewLattice, tag: &str| -> (f64, u64, u64) {
        let dir = TempDir::new(tag);
        let mut p = Pyramid::open(&dir.0, cfg_for(shape)).unwrap();
        run(&mut p, secs);
        p.seal_through(ts(T0 + secs * S)).unwrap();
        let st = p.stats().clone();
        // Level-0 rows that closed: one per time cell per frequency block of the run.
        let blocks = N_BINS as u64 / u64::from(shape.cells_per_block);
        let rows = secs as u64 * blocks;
        let per_row = st.coarse_cells_folded as f64 / rows as f64;
        println!(
            "  {tag}: {} nodes, {rows} level-0 rows closed, {} producer rows folded, \
             {} producer cells folded = {per_row:.1} cells per arriving row \
             ({:.2}x one row of {} cells)",
            shape.f_levels * shape.t_levels,
            st.coarse_rows_folded,
            st.coarse_cells_folded,
            per_row / f64::from(shape.cells_per_block),
            shape.cells_per_block,
        );
        (per_row, st.coarse_rows_folded, st.tiles_written)
    };

    let small = lat(43);
    let (small_per_row, small_rows, _) = measure(small, "4x4");
    let big = ViewLattice {
        scheme: 44,
        f_levels: 8,
        t_levels: 8,
        ..lat(44)
    };
    let (big_per_row, big_rows, _) = measure(big, "8x8");

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
    assert!(st.coarse_rows_folded > 1000, "the run was too short to judge");
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
    assert_eq!(st.checkpoints_written, 0, "no checkpointing is configured here");
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
    // load-bearing rather than tidiness. An unsealed live store is legitimately behind at each
    // time level — the cascade commits every N, so a chain of in-progress rows lags the edge by
    // 2^j − 1 finest rows (T-583) — while the on-demand control folds open producers and lags by
    // nothing. Measured, that lag alone is 1, 2 and 4 rows at time levels 1, 2 and 3, which is
    // the same shape as the defect and would mask it. Sealing well past the run ends every
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
    let secs = 600;
    let shape = lat(47);
    let cfg = || PyramidConfig {
        checkpoint_interval: Some(Duration::from_secs(60)),
        ..cfg_for(shape)
    };

    // The control is the same store WITHOUT the restart: the run is identical, so any node that
    // differs differs because of the reopen. Both are sealed past every coarse block first, so
    // neither carries the in-progress-row lag (T-583) that would otherwise be read as a loss.
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
