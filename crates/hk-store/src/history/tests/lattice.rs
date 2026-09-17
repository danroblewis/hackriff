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

fn read(p: &Pyramid, level: usize, secs: i64) -> RegionHistory {
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
    let p = build(
        &dir,
        secs,
        |s, b| {
            if s == 5 && b == 3 { -20.0 } else { -90.0 }
        },
    );
    let h0 = read(&p, 0, secs);

    for i in 0..sh.f_levels {
        for j in 0..sh.t_levels {
            let l = sh.index(i, j);
            let h = read(&p, l, secs);
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
    let h0 = read(&p, 0, secs);

    for j in 1..sh.t_levels {
        let h = read(&p, sh.index(0, j), secs);
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
    let p = build(
        &dir,
        secs,
        |s, b| {
            if s == 5 && b == 3 { -20.0 } else { -90.0 }
        },
    );
    let child = read(&p, sh.index(0, 0), secs);
    let parent = read(&p, sh.index(0, 1), secs);

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
    let p = build(&dir, secs, |s, b| -90.0 + (s % 4) as f32 + b as f32);

    // Level 0 is fed by ingest and closes its columns exactly, so it has percentiles.
    let h0 = read(&p, 0, secs);
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
            let h = read(&p, sh.index(i, j), secs);
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
        let p = build(
            &dir,
            secs,
            |s, b| {
                if s == 5 && b == 3 { -20.0 } else { -90.0 }
            },
        );
        (0..sh.f_levels * sh.t_levels)
            .map(|l| read(&p, l, secs))
            .collect::<Vec<_>>()
    };
    let p = Pyramid::open(&dir.0, lattice_cfg()).unwrap();
    for (l, was) in before.iter().enumerate() {
        let after = read(&p, l, secs);
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
