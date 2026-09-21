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
