//! T-434: the two axes are independent coordinates, and each level is proved against **level 0**.
//!
//! `docs/16` §5.2 correction 2 measured the defect: across scheme 1's ladder frequency coarsens
//! ×16 and time ×86 400, because `t_cell` of level *n+1* was *forced* to one whole tile of level
//! *n*. That is a ladder for one band over a long time, not a map. [`LevelConfig::t_factor`]
//! removes the weld, so a scheme can fold **frequency alone**, **time alone**, or both, and a
//! lattice of `(level_f, level_t)` nodes becomes expressible.
//!
//! # Why every assertion reaches back to level 0
//!
//! T-419's discipline, unchanged, and it bites harder here. The buggy max-of-children coverage
//! fold satisfied *"a parent never exceeds its best child"* at **every** level while being wrong by
//! `f_factor` at every level. A de-welded ladder adds a second way to be wrong at every level —
//! a time fold that writes one parent column instead of `nt / t_factor` of them would still
//! produce a parent that is a plausible max of its children, in the wrong place. Only the coarse
//! cell against the finest cells in its own box, computed independently, sees either.

use super::*;

/// 1 kHz × 1 s cells, 4 per tile on both axes, and a 3 × 3 lattice: frequency ×2 rightwards, time
/// ×2 downwards, independently. Deliberately tiny so the level-0 control can be computed by hand.
///
/// | | t0 (1 s) | t1 (2 s) | t2 (4 s) |
/// |---|---|---|---|
/// | **f0** (1 kHz) | 0 | 1 | 2 |
/// | **f1** (2 kHz) | 3 | 4 | 5 |
/// | **f2** (4 kHz) | 6 | 7 | 8 |
fn lattice() -> ViewLattice {
    ViewLattice {
        scheme: 11,
        f_cell_hz: 1000.0,
        t_cell: Duration::from_secs(1),
        cells_per_block: 4,
        f_levels: 3,
        t_levels: 3,
    }
}

fn lattice_cfg() -> PyramidConfig {
    PyramidConfig {
        histogram: HistogramConfig {
            lo_db: -130.0,
            step_db: 0.5,
            bins: 240,
        },
        seal_lag: Duration::ZERO,
        checkpoint_interval: None,
        byte_budget: u64::MAX,
        ..PyramidConfig::view_lattice(lattice())
    }
}

/// T-571: the shipped lattice is maintained **live**, so a read is a read. The `materialize` call
/// is left in deliberately — it is a no-op for a live scheme and asserts so: if it ever folded
/// anything here the counter check in `live_coarse.rs` would be the thing that caught it, and this
/// helper would be quietly hiding the read-time fold behind every assertion in the file.
fn read(p: &mut Pyramid, level: usize, secs: i64) -> RegionHistory {
    let built = p
        .materialize(
            level,
            FreqRange::new(0.0, 4000.0),
            TimeRange::new(ts(T0), ts(T0 + secs * S)),
        )
        .unwrap();
    assert_eq!(built, 0, "a live lattice builds nothing at read time");
    query(
        p,
        (0.0, 4000.0),
        (T0, T0 + secs * S),
        Resolution::Level(level as u8),
    )
}

/// **The control.** Max-hold of coarse cell `(tc, fc)` computed from the **level-0** grid alone:
/// the greatest level-0 max inside the coarse cell's own time–frequency box, reading no
/// intermediate level and not the cell it checks. `None` when no level-0 cell in the box was
/// observed.
fn truth_max_from_level_0(
    h0: &RegionHistory,
    coarse: &RegionHistory,
    tc: usize,
    fc: usize,
) -> Option<f32> {
    let fr = coarse.freq_of(fc);
    let t_start = coarse.time_of(tc).as_unix_nanos();
    let t_end = t_start + coarse.t_cell_ns;
    let mut best: Option<f32> = None;
    for t in 0..h0.nt {
        let ts0 = h0.time_of(t).as_unix_nanos();
        if ts0 < t_start || ts0 >= t_end {
            continue;
        }
        for f in 0..h0.nf {
            let c0 = h0.freq_of(f);
            if c0.lo_hz < fr.lo_hz || c0.hi_hz > fr.hi_hz {
                continue;
            }
            let c = h0.cell(t, f);
            if c.observed() {
                best = Some(best.map_or(c.max_db, |b: f32| b.max(c.max_db)));
            }
        }
    }
    best
}

/// **The control, coverage.** T-421/T-419's rule: observed seconds **sum** over the level-0 cells
/// in the box, divided by the box's own whole extent.
fn truth_coverage_from_level_0(
    h0: &RegionHistory,
    coarse: &RegionHistory,
    tc: usize,
    fc: usize,
) -> f64 {
    let fr = coarse.freq_of(fc);
    let t_start = coarse.time_of(tc).as_unix_nanos();
    let t_end = t_start + coarse.t_cell_ns;
    let t0_cell_s = h0.t_cell_ns as f64 * 1e-9;
    let mut observed_s = 0.0;
    for t in 0..h0.nt {
        let ts0 = h0.time_of(t).as_unix_nanos();
        if ts0 < t_start || ts0 >= t_end {
            continue;
        }
        for f in 0..h0.nf {
            let c0 = h0.freq_of(f);
            if c0.lo_hz < fr.lo_hz || c0.hi_hz > fr.hi_hz {
                continue;
            }
            let c = h0.cell(t, f);
            if c.observed() {
                observed_s += f64::from(c.coverage) * t0_cell_s;
            }
        }
    }
    let n_f = coarse.f_cell_hz / h0.f_cell_hz;
    observed_s / (n_f * coarse.t_cell_ns as f64 * 1e-9)
}

/// 16 s of 1 s frames over 0–4 kHz, with `at(second, bin)` giving each cell's dB.
fn build(dir: &TempDir, secs: i64, at: impl Fn(i64, usize) -> f32) -> Pyramid {
    let mut p = Pyramid::open(&dir.0, lattice_cfg()).unwrap();
    for s in 0..secs {
        let psd: Vec<f32> = (0..4).map(|b| lin(at(s, b))).collect();
        p.ingest(&frame(T0 + s * S, S, 0.0, 1000.0, &psd)).unwrap();
    }
    p.seal_through(ts(T0 + secs * S)).unwrap();
    p
}

/// A single 1 s × 1 kHz spike must survive every fold, and must land in the **right** cell on
/// whichever axis that node coarsens.
#[test]
fn a_spike_survives_each_axis_independently_and_lands_where_level_0_puts_it() {
    let dir = TempDir::new("lat-spike");
    let sh = lattice();
    let secs = 16;
    // One loud cell at t = 5 s, bin 3 (3–4 kHz); everything else at the floor.
    let mut p = build(
        &dir,
        secs,
        |s, b| {
            if s == 5 && b == 3 { -20.0 } else { -90.0 }
        },
    );
    let h0 = read(&mut p, 0, secs);

    for i in 0..sh.f_levels {
        for j in 0..sh.t_levels {
            let l = sh.index(i, j);
            let h = read(&mut p, l, secs);
            // Each axis has coarsened on its own, by its own factor.
            assert_eq!(h.f_cell_hz, 1000.0 * f64::from(1u32 << i), "({i},{j}) f");
            assert_eq!(h.t_cell_ns, S * i64::from(1u32 << j), "({i},{j}) t");
            let mut found = 0;
            for t in 0..h.nt {
                for f in 0..h.nf {
                    let c = h.cell(t, f);
                    let truth = truth_max_from_level_0(&h0, &h, t, f);
                    match truth {
                        Some(want) => {
                            assert!(c.observed(), "({i},{j}) cell ({t},{f}) must be observed");
                            assert!(
                                (c.max_db - want).abs() < 0.05,
                                "({i},{j}) cell ({t},{f}): max {} vs level-0 truth {want}",
                                c.max_db
                            );
                            if want > -50.0 {
                                found += 1;
                            }
                        }
                        None => assert!(
                            !c.observed(),
                            "({i},{j}) cell ({t},{f}) claims observation level 0 does not have"
                        ),
                    }
                }
            }
            // Exactly one cell holds the spike at every node — it is never smeared and never lost.
            assert_eq!(found, 1, "({i},{j}) spike cells");
        }
    }
}

/// Coverage still folds as a **sum over the parent's own extent**, on the time axis too.
///
/// Half the seconds are skipped, so every level-0 cell that exists is fully covered and every
/// other second is genuinely unobserved. A ×2 time fold must therefore read 0.5, a ×4 fold 0.5,
/// and so on — the *frequency* axis is fully covered throughout, so this isolates the time fold
/// that the weld made impossible to test at all.
#[test]
fn a_time_fold_sums_coverage_over_the_parent_extent() {
    let dir = TempDir::new("lat-cov");
    let sh = lattice();
    let secs = 16;
    let mut p = Pyramid::open(&dir.0, lattice_cfg()).unwrap();
    let psd = vec![lin(-80.0); 4];
    for s in 0..secs {
        if s % 2 == 0 {
            p.ingest(&frame(T0 + s * S, S, 0.0, 1000.0, &psd)).unwrap();
        }
    }
    p.seal_through(ts(T0 + secs * S)).unwrap();
    let h0 = read(&mut p, 0, secs);

    for j in 1..sh.t_levels {
        let h = read(&mut p, sh.index(0, j), secs);
        for t in 0..h.nt {
            for f in 0..h.nf {
                let c = h.cell(t, f);
                let truth = truth_coverage_from_level_0(&h0, &h, t, f);
                assert!(c.observed(), "(0,{j}) cell ({t},{f})");
                assert!(
                    (f64::from(c.coverage) - truth).abs() < 1e-3,
                    "(0,{j}) cell ({t},{f}): coverage {} vs level-0 truth {truth}",
                    c.coverage
                );
                // Half the extent was looked at, and the fold must say so rather than rounding to
                // the best-observed child.
                assert!(
                    (f64::from(c.coverage) - 0.5).abs() < 1e-3,
                    "(0,{j}) cell ({t},{f}): {} should be half-covered",
                    c.coverage
                );
            }
        }
    }
}

/// A parent-vs-child assertion is blind to a mis-placed time fold; the level-0 control is not.
///
/// Every node's cells here are a correct max of the node it was folded from — that holds whether
/// the fold writes the spike into the right parent column or shifts it. The control catches the
/// shift because it recomputes the box from level 0.
#[test]
fn a_parent_vs_child_assertion_would_not_have_caught_a_shifted_time_fold() {
    let dir = TempDir::new("lat-shift");
    let sh = lattice();
    let secs = 16;
    let mut p = build(
        &dir,
        secs,
        |s, b| {
            if s == 5 && b == 3 { -20.0 } else { -90.0 }
        },
    );
    let child = read(&mut p, sh.index(0, 0), secs);
    let parent = read(&mut p, sh.index(0, 1), secs);

    // The parent-vs-child form: every parent cell is the max of the child cells under it. True
    // whichever column the fold wrote to, as long as it wrote *some* column of the right tile.
    for t in 0..parent.nt {
        for f in 0..parent.nf {
            let c = parent.cell(t, f);
            if !c.observed() {
                continue;
            }
            let mut best = f32::NEG_INFINITY;
            for ct in 0..child.nt {
                let ts0 = child.time_of(ct).as_unix_nanos();
                if ts0 < parent.time_of(t).as_unix_nanos()
                    || ts0 >= parent.time_of(t).as_unix_nanos() + parent.t_cell_ns
                {
                    continue;
                }
                let cc = child.cell(ct, f);
                if cc.observed() {
                    best = best.max(cc.max_db);
                }
            }
            assert!((c.max_db - best).abs() < 0.05);
        }
    }

    // The control: the spike is in the parent cell holding second 5, and in no other.
    let spike_t = (0..parent.nt)
        .find(|&t| {
            let t0 = parent.time_of(t).as_unix_nanos();
            T0 + 5 * S >= t0 && T0 + 5 * S < t0 + parent.t_cell_ns
        })
        .expect("a parent row holds second 5");
    assert_eq!(spike_t, 2, "1 s cells folded ×2: second 5 is row 2");
    assert!(parent.cell(spike_t, 3).max_db > -25.0);
    for t in 0..parent.nt {
        if t != spike_t {
            assert!(parent.cell(t, 3).max_db < -80.0, "row {t} must not hold it");
        }
    }
}

/// **The price of de-welding, stated as a test.** A tile keeps one histogram per frequency cell
/// over the whole tile, which is the parent cell's histogram only when a child tile is exactly one
/// parent time cell. De-weld and the fold has nothing to compute a percentile from — so it says
/// **unknown** rather than inventing one, at every level above the finest.
///
/// This is the same refusal as `Coverage::of` returning `Unobserved` rather than a zeroed
/// `Sampled`, and as `bias_tee: "unknown"` not being `"off"`: nothing said is never permissive.
#[test]
fn a_de_welded_fold_leaves_the_percentiles_unknown_rather_than_inventing_them() {
    let dir = TempDir::new("lat-pct");
    let sh = lattice();
    let secs = 16;
    let mut p = build(&dir, secs, |s, b| -90.0 + (s % 4) as f32 + b as f32);

    // Level 0 is fed by ingest and closes its columns exactly, so it has percentiles.
    let h0 = read(&mut p, 0, secs);
    assert!(
        h0.cell(0, 0).p_low_db.is_finite(),
        "level 0 computes percentiles from the column itself"
    );

    // Every node above it folds across more than one parent time cell, so none does.
    for i in 0..sh.f_levels {
        for j in 0..sh.t_levels {
            if i == 0 && j == 0 {
                continue;
            }
            let h = read(&mut p, sh.index(i, j), secs);
            for t in 0..h.nt {
                for f in 0..h.nf {
                    let c = h.cell(t, f);
                    if !c.observed() || c.level as usize != sh.index(i, j) {
                        continue;
                    }
                    // The measurement survives; the distribution does not claim to.
                    assert!(c.max_db.is_finite(), "({i},{j}) keeps the max-hold");
                    assert!(
                        !c.p_low_db.is_finite() && !c.p_high_db.is_finite(),
                        "({i},{j}) cell ({t},{f}) must not report a percentile it cannot see: \
                         {} / {}",
                        c.p_low_db,
                        c.p_high_db
                    );
                }
            }
        }
    }

    // And the welded ladder is untouched: scheme 1 still carries percentiles all the way up.
    let dir2 = TempDir::new("lat-pct-ladder");
    let mut q = Pyramid::open(&dir2.0, cfg(vec![level(1, 4), level(2, 2)], 4)).unwrap();
    for s in 0..secs {
        let psd: Vec<f32> = (0..4)
            .map(|b| lin(-90.0 + (s % 4) as f32 + b as f32))
            .collect();
        q.ingest(&frame(T0 + s * S, S, 0.0, 1000.0, &psd)).unwrap();
    }
    q.seal_through(ts(T0 + secs * S)).unwrap();
    let h1 = query(&q, (0.0, 4000.0), (T0, T0 + secs * S), Resolution::Level(1));
    assert!(
        h1.cell(0, 0).p_low_db.is_finite(),
        "the weld is what buys the percentiles, and scheme 1 keeps it"
    );
}

/// The lattice survives a restart: coarse nodes are rebuilt from their sealed producers through
/// the same seal path, with no second producer and no double-counting.
#[test]
fn a_lattice_recovers_every_node_from_its_one_producer() {
    let dir = TempDir::new("lat-recover");
    let sh = lattice();
    let secs = 16;
    let before = {
        let mut p = build(
            &dir,
            secs,
            |s, b| {
                if s == 5 && b == 3 { -20.0 } else { -90.0 }
            },
        );
        (0..sh.f_levels * sh.t_levels)
            .map(|l| read(&mut p, l, secs))
            .collect::<Vec<_>>()
    };
    let mut p = Pyramid::open(&dir.0, lattice_cfg()).unwrap();
    for (l, was) in before.iter().enumerate() {
        let after = read(&mut p, l, secs);
        let (i, j) = sh.coords(l);
        assert_eq!(after.nt, was.nt, "({i},{j})");
        for t in 0..after.nt {
            for f in 0..after.nf {
                let (a, b) = (after.cell(t, f), was.cell(t, f));
                assert_eq!(a.observed(), b.observed(), "({i},{j}) cell ({t},{f})");
                if a.observed() {
                    assert!(
                        (a.max_db - b.max_db).abs() < 0.05,
                        "({i},{j}) cell ({t},{f})"
                    );
                    assert!(
                        (a.coverage - b.coverage).abs() < 1e-3,
                        "({i},{j}) cell ({t},{f}): coverage {} vs {}",
                        a.coverage,
                        b.coverage
                    );
                }
            }
        }
    }
}

/// **T-571: the live edge is MAINTAINED, not folded on request, and still never sealed early.**
///
/// This was T-453's test that a coarse node at the live edge is folded when a read asks for it.
/// T-571 inverts the first half and keeps the second: the node is built as producer rows close,
/// so a read folds **nothing**, and a node whose own time block has not ended must still not be
/// written — a tile sealed early is a tile that says *this is all there was*.
#[test]
fn a_live_edge_coarse_node_is_maintained_as_rows_close_and_never_sealed_early() {
    let dir = TempDir::new("lat-edge");
    let sh = lattice();
    // Node (0, 0)'s tile is 4 x 1 s, node (0, 1)'s is 4 x 2 s. Six seconds therefore seals the
    // first of the former and leaves the latter open, which is exactly the two cases.
    let secs = 6;
    let mut p = Pyramid::open(&dir.0, lattice_cfg()).unwrap();
    for s in 0..secs {
        let psd: Vec<f32> = (0..4)
            .map(|b| lin(if b == 3 { -20.0 } else { -90.0 }))
            .collect();
        p.ingest(&frame(T0 + s * S, S, 0.0, 1000.0, &psd)).unwrap();
    }
    let edge = sh.index(0, 1);
    assert!(
        !p.sealed_keys(0).is_empty(),
        "capture writes its own product, always"
    );
    assert!(
        p.sealed_keys(edge).is_empty(),
        "a node whose own time block has not ended must not be sealed: it would claim to be the \
         whole of a block that is still filling"
    );
    // It is nonetheless already there, open, with the rows its producer has closed.
    assert!(
        !p.open_keys(edge).is_empty(),
        "the coarse node must be maintained as rows close, not waiting for a reader"
    );

    // Read it. Nothing is folded to answer.
    let before = p.stats().producer_tiles_folded;
    let h = read(&mut p, edge, secs);
    assert_eq!(
        p.stats().producer_tiles_folded,
        before,
        "a read of a live-edge coarse node must fold no producer tiles"
    );
    let observed = h.cells.iter().filter(|c| c.observed()).count();
    assert!(
        observed > 0,
        "a live-edge coarse node must answer from what it holds, not read grey over data the \
         store holds"
    );
    assert!(
        p.sealed_keys(edge).is_empty(),
        "reading it must not seal it either"
    );

    // It agrees with level 0 over every producer row that has CLOSED. The newest level-0 time
    // cell is still open — its own column has not ended — so a coarse node lags it by at most one
    // producer cell, which is what "commits every N rows" means and is the only honest answer a
    // streaming fold can give. (Node (0, 0) itself is unaffected: a direct level-0 read stands in
    // for its open column with `column_preview`.)
    let psd: Vec<f32> = (0..4).map(|_| lin(-90.0)).collect();
    p.ingest(&frame(T0 + secs * S, S, 0.0, 1000.0, &psd))
        .unwrap();
    let again = read(&mut p, edge, secs + 1);
    let h0 = read(&mut p, sh.index(0, 0), secs + 1);
    // The producer cell still open, in this coarse node's own row index.
    let open_row = (secs as usize) / 2;
    let mut checked = 0;
    for t in 0..again.nt {
        if t == open_row {
            continue;
        }
        for f in 0..again.nf {
            let truth = truth_max_from_level_0(&h0, &again, t, f);
            match truth {
                Some(want) => {
                    checked += 1;
                    assert!(
                        (again.cell(t, f).max_db - want).abs() < 0.05,
                        "live-edge cell ({t},{f}): {} vs level-0 truth {want}",
                        again.cell(t, f).max_db
                    );
                }
                None => assert!(!again.cell(t, f).observed(), "cell ({t},{f})"),
            }
        }
    }
    assert!(checked > 0, "the agreement check judged nothing");

    // Once its block ends it is written, exactly as it stood — not rebuilt.
    let folded = p.stats().producer_tiles_folded;
    p.seal_through(ts(T0 + 8 * S)).unwrap();
    read(&mut p, edge, 8);
    assert_eq!(
        p.sealed_keys(edge).len(),
        1,
        "a coarse node whose block has elapsed is sealed on the way"
    );
    assert_eq!(
        p.stats().producer_tiles_folded,
        folded,
        "and sealing it folds nothing either: the rows were already in it"
    );
}

/// **T-453: retention is the one place laziness cannot be honest on its own.**
///
/// The byte budget evicts the finest tiles first and may only evict a tile a coarser level covers.
/// Under a lazy lattice that summary does not exist until someone asks — so the budget pass must
/// build it for exactly the tile it is about to drop, or either lose the measurement outright or
/// stall with `over_budget` set while the finest level grows without bound.
#[test]
fn the_byte_budget_builds_the_summary_of_the_tile_it_is_about_to_evict() {
    let dir = TempDir::new("lat-budget");
    let sh = lattice();
    let cfg = PyramidConfig {
        // Well under the ~20 kB the run writes at level 0, so the budget really binds.
        byte_budget: 6 << 10,
        ..lattice_cfg()
    };
    let mut p = Pyramid::open(&dir.0, cfg).unwrap();
    let secs = 256;
    for s in 0..secs {
        let psd: Vec<f32> = (0..4)
            .map(|b| lin(-95.0 + (s % 7) as f32 + b as f32))
            .collect();
        p.ingest(&frame(T0 + s * S, S, 0.0, 1000.0, &psd)).unwrap();
    }
    p.seal_through(ts(T0 + secs * S)).unwrap();
    let st = p.stats().clone();
    assert!(
        st.tiles_evicted > 0,
        "the budget should have bound: {} B written",
        st.bytes_written
    );
    assert!(
        !st.over_budget,
        "the budget stalled: with lazy coarse nodes the pass has to build the summary of the tile \
         it is evicting, and `covered` is false until it does"
    );
    // T-571: retention no longer has to build anything. Under the lazy lattice this was the one
    // place the fold could not be deferred — the budget was about to drop tiles whose summary did
    // not exist — and live maintenance removes the case entirely: the summary was built as the
    // rows closed, so the pass finds `covered` already true.
    assert_eq!(
        st.tiles_materialized, 0,
        "a live lattice's summaries exist before retention looks for them"
    );
    assert_eq!(
        st.producer_tiles_folded, 0,
        "and nothing re-folded them from their producers"
    );
    // Nothing was thrown away without a summary: the oldest time is still answerable, coarsely.
    let coarse = read(&mut p, sh.index(sh.f_levels - 1, sh.t_levels - 1), secs);
    assert!(
        coarse.cells.iter().any(|c| c.observed()),
        "the coarse end holds nothing, so the evicted fine tiles were lost rather than summarised"
    );
}
