//! T-419: the coverage fold, proved **against level 0** at every depth.
//!
//! `Tile::fold_child` used to roll coverage up as `best_obs = max(best_obs, obs_f)` — the
//! best-observed child frequency cell — so a parent whose left half was observed for the full
//! minute and whose right half was **never observed** read *fully covered*, and at `f_factor = 2`
//! over four folds a level-4 cell could claim full coverage on one sixteenth of its frequency
//! extent. The correct rule sums observed seconds over the children and divides by the parent's
//! **own** extent: `observed_s` is foldable, `duty` is not.
//!
//! # Why every assertion here reaches back to level 0
//!
//! T-397's lesson, one axis over. There, every fold on the way out *was* a max and the wire
//! declared `"fold": "max-hold"` correctly — what was wrong was the **input**, so a parent-vs-child
//! test could never have seen it. Here the input is honest and the **fold** rounds up, and a
//! parent-vs-child test is blind again, for a reason [`a_parent_vs_child_assertion_would_not_have_caught_this`]
//! makes explicit: the buggy answer satisfies *"a parent is the max of its children"* at **every**
//! level while being wrong by `f_factor` at every level. The only assertion that catches it is the
//! coarse cell's value against the **finest available** cells in its box, computed independently.
//!
//! The standing pair of rules, which is why one of these is not the other: **folding must never
//! lower a measurement** (that is what makes a peak survive downsampling) and **folding must never
//! raise coverage** (that is what keeps grey meaning genuinely unobserved). Getting one right does
//! not give you the other.

use super::*;

/// 1 kHz × 1 s level-0 cells, 16 per tile, then four ×2 frequency folds: L1 2 kHz, L2 4 kHz,
/// L3 8 kHz, L4 16 kHz — the four folds of the brief. Time cells double with them (L0 1 s tiles of
/// 4 s → L4 32 s cells in 64 s tiles), so both axes are exercised by the same ladder.
fn ladder() -> PyramidConfig {
    cfg(
        vec![
            level(1, 4),
            level(2, 2),
            level(2, 2),
            level(2, 2),
            level(2, 2),
        ],
        16,
    )
}

/// **The control.** The observed fraction of coarse cell `(tc, fc)`'s own time–frequency extent,
/// computed from the **level-0** grid alone: every level-0 cell inside the box contributes
/// `coverage × its own duration`, and the divisor is the box's whole extent — `f_children` cells
/// wide by the coarse cell's duration. Nothing here reads the coarse cell it is checking, and
/// nothing reads an intermediate level.
fn truth_from_level_0(h0: &RegionHistory, coarse: &RegionHistory, tc: usize, fc: usize) -> f64 {
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

/// A pyramid holding `secs` seconds of frames, each covering `[f_lo, f_lo + bins·1 kHz)` of the
/// 0–16 kHz band, ingested only on the seconds `keep` accepts.
fn build(dir: &TempDir, secs: i64, f_lo: f64, bins: usize, keep: impl Fn(i64) -> bool) -> Pyramid {
    let mut p = Pyramid::open(&dir.0, ladder()).unwrap();
    let psd = vec![lin(-80.0); bins];
    for s in 0..secs {
        if keep(s) {
            p.ingest(&frame(T0 + s * S, S, f_lo, 1000.0, &psd)).unwrap();
        }
    }
    p.seal_through(ts(T0 + secs * S)).unwrap();
    p
}

fn grids(p: &Pyramid, secs: i64) -> Vec<RegionHistory> {
    (0..5)
        .map(|l| {
            query(
                p,
                (0.0, 16_000.0),
                (T0, T0 + secs * S),
                Resolution::Level(l as u8),
            )
        })
        .collect()
}

#[test]
fn a_half_observed_parent_reports_half_covered_not_fully_covered() {
    // THE DEFECT, measured. One kilohertz of a sixteen-kilohertz band is observed, continuously,
    // for the whole span. Level 0 sees one covered cell beside fifteen unobserved ones. Every
    // coarser level pools that covered cell with `f_factor − 1` never-observed siblings, so its
    // coverage must **halve at every fold**: 1 → 1/2 → 1/4 → 1/8 → 1/16.
    //
    // Under the old max-of-children rule every one of these read 1.0 — a level-4 cell claiming
    // full coverage on one sixteenth of its frequency extent, which is a pyramid that paints the
    // spectrum as scanned.
    let dir = TempDir::new("cov-half");
    let secs = 128;
    let p = build(&dir, secs, 0.0, 1, |_| true);
    let h = grids(&p, secs);

    for (l, want) in [
        (0usize, 1.0f64),
        (1, 0.5),
        (2, 0.25),
        (3, 0.125),
        (4, 0.0625),
    ] {
        let g = &h[l];
        let c = g.cell(0, 0);
        assert!(c.observed(), "level {l}: the covered cell exists");
        assert_eq!(c.level, l as u8, "level {l}: answered by its own tier");
        // Against level 0, independently — not against the level above.
        let truth = truth_from_level_0(&h[0], g, 0, 0);
        assert!(
            (truth - want).abs() < 1e-9,
            "level {l}: level-0 truth {truth} should be {want}"
        );
        assert!(
            (f64::from(c.coverage) - want).abs() < 1e-3,
            "level {l}: coverage {} should be {want} (the old max rule said 1.0)",
            c.coverage
        );
        // And the cells the radio never pointed at stay unobserved at every level — a fold that
        // rounds coverage up is one step from a fold that invents observation.
        let last = g.nf - 1;
        if last > 0 {
            assert!(
                !g.cell(0, last).observed(),
                "level {l}: the far end of the band was never observed"
            );
        }
    }
}

#[test]
fn a_parent_vs_child_assertion_would_not_have_caught_this() {
    // THE GUARD, stated as a test rather than as a comment. Take the same half-observed ladder and
    // ask the question a parent-vs-child test asks: *is the parent the max of its children?*
    //
    // The answer is YES at every level, for the **correct** values — because the correct parent
    // (the mean over `f_factor` children, one of which is covered) and the buggy parent (the max
    // over them) differ by exactly `f_factor`, and both are consistent with *some* child. So the
    // parent-vs-child form cannot separate the right answer from an answer `2×`, `4×`, `16×` too
    // large; it only ever proves the fold is *a* fold. That is T-397's blind spot exactly, and it
    // is how this survived. Only `truth_from_level_0` — the finest available cells in the box,
    // computed independently — pins the value.
    let dir = TempDir::new("cov-guard");
    let secs = 128;
    let p = build(&dir, secs, 0.0, 1, |_| true);
    let h = grids(&p, secs);

    for l in 1..5 {
        let parent = f64::from(h[l].cell(0, 0).coverage);
        // The immediate children of parent cell (0, 0): the level-(l−1) cells inside its box.
        let fr = h[l].freq_of(0);
        let t_start = h[l].time_of(0).as_unix_nanos();
        let t_end = t_start + h[l].t_cell_ns;
        let parent_s = h[l].t_cell_ns as f64 * 1e-9;
        let child = &h[l - 1];
        let child_s = child.t_cell_ns as f64 * 1e-9;
        // Per child FREQUENCY cell, the observed seconds its time rows hold.
        let mut per_f = vec![0.0f64; child.nf];
        for t in 0..child.nt {
            let ts0 = child.time_of(t).as_unix_nanos();
            if ts0 < t_start || ts0 >= t_end {
                continue;
            }
            for (f, acc) in per_f.iter_mut().enumerate() {
                let c0 = child.freq_of(f);
                if c0.lo_hz < fr.lo_hz || c0.hi_hz > fr.hi_hz || !child.cell(t, f).observed() {
                    continue;
                }
                *acc += f64::from(child.cell(t, f).coverage) * child_s;
            }
        }
        // The OLD rule, recomputed: the best-observed child frequency cell, as a fraction of the
        // parent's duration. And the new rule: their sum over the parent's own extent.
        let best = per_f.iter().fold(0.0f64, |m, &s| m.max(s)) / parent_s;
        let summed = per_f.iter().sum::<f64>() / (2.0 * parent_s);

        // The blind assertion: "the parent never exceeds the best child" — true of the fixed value
        // AND true of the value twice as large that the bug produced.
        assert!(
            best >= parent - 1e-6,
            "level {l}: parent {parent} > best child {best}"
        );
        assert!(
            (best - 2.0 * parent).abs() < 1e-3,
            "level {l}: the max-of-children answer {best} is exactly f_factor × the truth {parent} \
             — which is why that assertion proves nothing"
        );
        // The parent IS the children's summed seconds over its own extent.
        assert!(
            (summed - parent).abs() < 1e-3,
            "level {l}: parent {parent} should be the children's summed seconds {summed}"
        );
        // And the only assertion that would have failed under the bug.
        let truth = truth_from_level_0(&h[0], &h[l], 0, 0);
        assert!(
            (parent - truth).abs() < 1e-3,
            "level {l}: parent {parent} vs level-0 truth {truth}"
        );
    }
}

#[test]
fn a_half_observed_parent_in_time_also_reports_half_covered() {
    // The other axis, so the sum rule is not accidentally a frequency-only fix. The whole 16 kHz
    // band is observed, but only on every other second: every level's coverage must read 0.5, and
    // must equal the level-0 truth rather than the best second inside it (which is 1.0).
    let dir = TempDir::new("cov-time");
    let secs = 128;
    let p = build(&dir, secs, 0.0, 16, |s| s % 2 == 0);
    let h = grids(&p, secs);

    for l in 1..5 {
        let g = &h[l];
        for f in 0..g.nf {
            let c = g.cell(0, f);
            assert!(c.observed(), "level {l} cell {f}");
            let truth = truth_from_level_0(&h[0], g, 0, f);
            assert!(
                (truth - 0.5).abs() < 1e-9,
                "level {l} cell {f}: level-0 truth {truth}"
            );
            assert!(
                (f64::from(c.coverage) - 0.5).abs() < 1e-3,
                "level {l} cell {f}: coverage {} should be 0.5",
                c.coverage
            );
        }
    }
}

#[test]
fn a_fully_observed_parent_still_reports_fully_covered() {
    // The fix must not cost the honest case anything: a band observed everywhere, always, reads 1.0
    // at every level. A coverage fold that under-reports is a different lie with the same shape.
    let dir = TempDir::new("cov-full");
    let secs = 128;
    let p = build(&dir, secs, 0.0, 16, |_| true);
    let h = grids(&p, secs);

    for l in 0..5 {
        let g = &h[l];
        for t in 0..g.nt {
            for f in 0..g.nf {
                let c = g.cell(t, f);
                assert!(c.observed(), "level {l} cell ({t}, {f})");
                assert!(
                    (f64::from(c.coverage) - 1.0).abs() < 1e-3,
                    "level {l} cell ({t}, {f}): coverage {}",
                    c.coverage
                );
                let truth = truth_from_level_0(&h[0], g, t, f);
                assert!((truth - 1.0).abs() < 1e-9, "level {l}: truth {truth}");
            }
        }
    }
}

#[test]
fn occupancy_is_a_fraction_of_observed_time_and_does_not_move_with_coverage() {
    // Coverage says how much of the cell was **looked at**; occupancy says what was found **while
    // looking**. Halving a parent's coverage because its sibling frequency cells were never
    // observed must not halve (or inflate) the occupancy of the part that was: `occ_s` is
    // re-derived as `ratio × obs_s` against the new `obs_s`, so the ratio the query reports is
    // unchanged. Without that, the same one-line fix would have quietly doubled occupancy.
    let dir = TempDir::new("cov-occ");
    let secs = 128;
    let mut p = Pyramid::open(&dir.0, ladder()).unwrap();
    let floor = vec![-100f32; 1];
    for s in 0..secs {
        // Loud for one second in four, quiet otherwise: occupancy 0.25 of observed time.
        let psd = vec![lin(if s % 4 == 0 { -40.0 } else { -100.0 })];
        let mut f = frame(T0 + s * S, S, 0.0, 1000.0, &psd);
        f.floor_db = Some(&floor);
        p.ingest(&f).unwrap();
    }
    p.seal_through(ts(T0 + secs * S)).unwrap();
    let h = grids(&p, secs);

    for (l, g) in h.iter().enumerate().take(5).skip(1) {
        let c = g.cell(0, 0);
        assert!(
            (f64::from(c.coverage) - 0.5f64.powi(l as i32)).abs() < 1e-3,
            "level {l}: coverage {}",
            c.coverage
        );
        assert!(
            (c.occupancy - 0.25).abs() < 0.02,
            "level {l}: occupancy {} should stay 0.25 of OBSERVED time",
            c.occupancy
        );
    }
}
