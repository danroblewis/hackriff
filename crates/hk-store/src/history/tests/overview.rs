//! T-338: [`RegionHistory::overview`], the backend fold behind the capture timeline's compressed
//! "sideways" overview waterfall.
//!
//! The reduction exists because the pyramid's ladder couples its axes: a level with cells coarse
//! enough in frequency for a thin strip's rows has one-day time cells. Every assertion here is
//! about the fold being a *measurement that folds exactly* — max of maxes, mean coverage, summed
//! frames, replication rather than interpolation — and about the output grid being the requested
//! window rather than the pyramid's outward-snapped one.

use hk_model::{FreqRange, PowerUnit, TimeRange, Timestamp};

use super::super::query::{CellStats, Overview, RegionHistory};
use super::super::tile::ProvenanceSummary;

const S: i64 = 1_000_000_000;
const T0: i64 = 1_789_300_800 * S;

/// A grid of `nt × nf` cells, one second and 1 kHz each, starting at `T0` / 0 Hz. `fill` gives each
/// `(t, f)` a max-hold, or `None` for an unobserved cell.
fn grid(nt: usize, nf: usize, fill: impl Fn(usize, usize) -> Option<f32>) -> RegionHistory {
    let mut cells = Vec::with_capacity(nt * nf);
    for t in 0..nt {
        for f in 0..nf {
            cells.push(match fill(t, f) {
                None => CellStats::NONE,
                Some(db) => CellStats {
                    max_db: db,
                    mean_db: db,
                    p_low_db: db,
                    p_high_db: db,
                    occupancy: 0.25,
                    occupancy_max: 0.5,
                    coverage: 1.0,
                    floor_db: db,
                    frames: 2,
                    level: 0,
                },
            });
        }
    }
    RegionHistory {
        scheme: 1,
        level: 0,
        unit: PowerUnit::Dbfs,
        f_cell_hz: 1000.0,
        f_first_cell: 0,
        nf,
        t_cell_ns: S,
        t_first_cell: T0 / S,
        nt,
        percentiles: (0.1, 0.9),
        cells,
        provenance: ProvenanceSummary::default(),
        tiles_read: 0,
        filter: None,
    }
}

fn window(from_s: i64, to_s: i64) -> TimeRange {
    TimeRange::new(
        Timestamp::from_unix_nanos(T0 + from_s * S),
        Timestamp::from_unix_nanos(T0 + to_s * S),
    )
}

fn at(o: &Overview, t: usize, f: usize) -> f32 {
    o.cells[t * o.nf + f].max_db
}

#[test]
fn the_overview_grid_is_the_requested_window_not_the_pyramids() {
    // The source grid is 1 s × 1 kHz; the overview is asked for 4 columns over 10 s and 2 rows
    // over 8 kHz. Its cells are 2.5 s and 4 kHz — sizes no pyramid level has — and cell 0 starts
    // at the window's own start.
    let h = grid(10, 8, |_, _| Some(-100.0));
    let o = h.overview(window(0, 10), FreqRange::new(0.0, 8000.0), 4, 2);
    assert_eq!((o.nt, o.nf), (4, 2));
    assert_eq!(o.t0_ns, T0);
    assert_eq!(o.t_cell_ns, 2.5 * S as f64);
    assert_eq!(o.f_cell_hz, 4000.0);
    assert_eq!(o.f_lo_hz, 0.0);
    assert_eq!((o.src_nt, o.src_nf), (10, 8));
}

#[test]
fn folding_takes_the_max_of_the_max_holds() {
    // The fold has to be exact, not approximate: the max of max-holds *is* the max-hold of the
    // union. One hot cell in a bucket therefore survives compression — which is the whole point of
    // an overview that shows where activity is worth scrubbing to.
    let h = grid(4, 2, |t, f| {
        Some(if t == 2 && f == 1 { -40.0 } else { -110.0 })
    });
    let o = h.overview(window(0, 4), FreqRange::new(0.0, 2000.0), 2, 1);
    assert_eq!(at(&o, 0, 0), -110.0);
    assert_eq!(at(&o, 1, 0), -40.0);
    // Peak occupancy folds by max too, and frames sum over every folded cell.
    assert_eq!(o.cells[1].occupancy_max, 0.5);
    // Four source cells (2 s × 2 kHz) at two frames each.
    assert_eq!(o.cells[1].frames, 8);
    assert_eq!(o.cells[1].sources, 4);
}

#[test]
fn an_unobserved_bucket_stays_unobserved_never_quiet() {
    // C26: a cell nothing was folded into is NaN with `observed() == false`, so the client can draw
    // "not observed" rather than a floor value that reads as a measured quiet band.
    let h = grid(4, 1, |t, _| (t < 2).then_some(-90.0));
    let o = h.overview(window(0, 4), FreqRange::new(0.0, 1000.0), 2, 1);
    assert!(o.cells[0].observed());
    assert!(!o.cells[1].observed());
    assert!(o.cells[1].max_db.is_nan());
    assert_eq!(o.cells[1].coverage, 0.0);
    assert_eq!(o.observed_cells, 1);
}

#[test]
fn coverage_is_the_observed_fraction_of_the_output_cells_own_extent() {
    // The exact case, and why it stayed hidden: when the output grid is a whole coarsening of the
    // source grid, every source cell is equal-duration *and* wholly inside one output cell, so the
    // extent-weighted sum and the old mean-over-sources agree exactly. 0.5 and 1.0 over two equal
    // halves is 0.75 either way.
    let mut h = grid(4, 1, |_, _| Some(-90.0));
    h.cells[0].coverage = 0.5;
    h.cells[1].coverage = 1.0;
    let o = h.overview(window(0, 4), FreqRange::new(0.0, 1000.0), 2, 1);
    assert!((o.cells[0].coverage - 0.75).abs() < 1e-6);
    assert!((o.cells[1].coverage - 1.0).abs() < 1e-6);
}

#[test]
fn one_observed_second_does_not_report_a_collapsed_column_fully_covered() {
    // T-419, THE DEFECT on this axis, and the case the survey bar and the left time navigator
    // actually ask for: `nt = 1` collapses the whole window into one column. One of ten seconds was
    // observed, the other nine never were. The old rule accumulated `coverage` and divided by
    // `sources` — one source, coverage 1.0 — so the column reported itself **fully covered** on a
    // tenth of its own extent. Weighted by extent it reads 0.1: the nine unobserved seconds are
    // part of the cell, not absent from the divisor.
    //
    // The cell is still `observed()` — something was measured here, and `max_db` is a real
    // max-hold — which is precisely why coverage has to carry the *how much*: "observed" and "fully
    // observed" are different claims, and only one of them was true.
    let h = grid(10, 1, |t, _| (t == 0).then_some(-70.0));
    let o = h.overview(window(0, 10), FreqRange::new(0.0, 1000.0), 1, 1);
    assert!(o.cells[0].observed());
    assert_eq!(o.cells[0].max_db, -70.0);
    assert!(
        (o.cells[0].coverage - 0.1).abs() < 1e-6,
        "one observed second of ten: {}",
        o.cells[0].coverage
    );

    // The same on the frequency axis: one observed kilohertz of eight, collapsed to one row.
    let h = grid(1, 8, |_, f| (f == 3).then_some(-70.0));
    let o = h.overview(window(0, 1), FreqRange::new(0.0, 8000.0), 1, 1);
    assert!(
        (o.cells[0].coverage - 0.125).abs() < 1e-6,
        "{:?}",
        o.cells[0]
    );

    // And both at once: one cell of a 4 × 4 grid.
    let h = grid(4, 4, |t, f| (t == 1 && f == 2).then_some(-70.0));
    let o = h.overview(window(0, 4), FreqRange::new(0.0, 4000.0), 1, 1);
    assert!(
        (o.cells[0].coverage - 1.0 / 16.0).abs() < 1e-6,
        "{:?}",
        o.cells[0]
    );
}

#[test]
fn a_fractional_grid_weights_a_source_cell_by_how_much_of_the_cell_it_covers() {
    // `overview` lays a **fractional** grid on the requested window (`t_cell = (t1 − t0)/nt`) and
    // folds every source cell that *overlaps* an output cell, so source cells straddle output
    // boundaries. Under the old mean a source cell overlapping by 1/3 counted as much as one
    // overlapping wholly. Here three 1 s source cells are asked for in two 1.5 s columns:
    //
    //   column 0 = [0, 1.5) = all of cell 0 (w = 2/3) + half of cell 1 (w = 1/3)
    //   column 1 = [1.5, 3)  = half of cell 1 (w = 1/3) + all of cell 2 (w = 2/3)
    //
    // With coverages 1.0, 0.25, 1.0 that is 0.75 and 0.75; the old mean gave 0.625 in both.
    let mut h = grid(3, 1, |_, _| Some(-90.0));
    h.cells[0].coverage = 1.0;
    h.cells[1].coverage = 0.25;
    h.cells[2].coverage = 1.0;
    let o = h.overview(window(0, 3), FreqRange::new(0.0, 1000.0), 2, 1);
    for (i, c) in o.cells.iter().enumerate() {
        assert!(
            (c.coverage - 0.75).abs() < 1e-6,
            "column {i}: {} (the unweighted mean would say 0.625)",
            c.coverage
        );
    }
}

#[test]
fn a_fully_covered_window_stays_fully_covered_at_any_output_shape() {
    // The honest case must cost nothing, on grids that do and do not divide evenly. A coverage fold
    // that *under*-reports is a different lie with the same shape — it would paint scanned spectrum
    // as unexplored, and the weights, being an exact partition of the output cell, must sum to 1.
    let h = grid(7, 5, |_, _| Some(-80.0));
    for (nt, nf) in [(1, 1), (2, 2), (3, 2), (7, 5), (13, 9), (64, 3)] {
        let o = h.overview(window(0, 7), FreqRange::new(0.0, 5000.0), nt, nf);
        for (i, c) in o.cells.iter().enumerate() {
            assert!(
                (c.coverage - 1.0).abs() < 1e-5,
                "{nt}×{nf} cell {i}: coverage {}",
                c.coverage
            );
        }
    }
}

#[test]
fn a_source_cell_coarser_than_the_output_gives_each_output_cell_its_own_coverage() {
    // The replication direction (T-334): one source cell covering several output cells hands each
    // of them its coverage whole, because it covers each of them whole. Weighting by extent must
    // not turn a covering source cell into a fractional one — the weight is the fraction of the
    // OUTPUT cell covered, never of the source cell.
    let mut h = grid(2, 1, |_, _| Some(-90.0));
    h.cells[0].coverage = 0.5;
    h.cells[1].coverage = 1.0;
    let o = h.overview(window(0, 2), FreqRange::new(0.0, 1000.0), 8, 1);
    for t in 0..4 {
        assert!((o.cells[t].coverage - 0.5).abs() < 1e-6, "column {t}");
    }
    for t in 4..8 {
        assert!((o.cells[t].coverage - 1.0).abs() < 1e-6, "column {t}");
    }
}

#[test]
fn a_source_cell_coarser_than_an_output_cell_replicates_rather_than_interpolates() {
    // T-334's safe direction. Asking for more columns than the source has time cells must repeat a
    // measured value across them, never invent the values between.
    let h = grid(2, 1, |t, _| Some(if t == 0 { -100.0 } else { -60.0 }));
    let o = h.overview(window(0, 2), FreqRange::new(0.0, 1000.0), 8, 1);
    assert_eq!(o.nt, 8);
    for t in 0..4 {
        assert_eq!(at(&o, t, 0), -100.0, "column {t}");
    }
    for t in 4..8 {
        assert_eq!(at(&o, t, 0), -60.0, "column {t}");
    }
}

#[test]
fn source_cells_outside_the_window_are_not_folded_in() {
    // The pyramid snaps a region outward to its cell boundaries, so the grid it returns is wider
    // than the window asked for. Those extra cells belong to times the timeline does not span, and
    // folding them would put energy at an instant it was not measured at.
    let h = grid(6, 1, |t, _| {
        Some(if (2..4).contains(&t) { -100.0 } else { -30.0 })
    });
    let o = h.overview(window(2, 4), FreqRange::new(0.0, 1000.0), 2, 1);
    assert_eq!(at(&o, 0, 0), -100.0);
    assert_eq!(at(&o, 1, 0), -100.0);
    assert_eq!(o.range_db, Some((-100.0, -100.0)));
}

#[test]
fn the_dynamic_range_served_is_the_grids_own_observed_range() {
    // The client used to pick a colour scale from whatever numbers it happened to hold; the range
    // is a measurement, so it is reported. With nothing observed there is no range at all — `None`,
    // not a default pair that would render as a scale.
    let h = grid(4, 1, |t, _| Some(-100.0 + t as f32 * 10.0));
    let o = h.overview(window(0, 4), FreqRange::new(0.0, 1000.0), 4, 1);
    assert_eq!(o.range_db, Some((-100.0, -70.0)));

    let empty = grid(4, 1, |_, _| None);
    let o = empty.overview(window(0, 4), FreqRange::new(0.0, 1000.0), 4, 1);
    assert_eq!(o.range_db, None);
    assert_eq!(o.observed_cells, 0);
}

// ---------------------------------------------------------------------------
// T-342: the band-collapsed series, and the same fold on the other axis.

#[test]
fn collapsing_a_band_to_one_column_is_the_max_over_its_rows() {
    // THE MEASUREMENT, asserted on values. `nf = 1` is the band-collapsed activity-vs-time series
    // the client used to reduce for itself: one value per time step over a whole region. Here it is
    // proved to be exactly the max over the rows of the same window at a finer `nf` — the reduction
    // that used to live in `ui/src`, now made once, where the levels are known.
    //
    // A mean would fail this: rows -110/-40 mean to -75, and the band would report a quiet step
    // where a strong emission was.
    let h = grid(4, 4, |t, f| Some(-110.0 + (t * 4 + f) as f32 * 5.0));
    let rows = h.overview(window(0, 4), FreqRange::new(0.0, 4000.0), 4, 4);
    let band = h.overview(window(0, 4), FreqRange::new(0.0, 4000.0), 4, 1);
    assert_eq!(band.nf, 1);
    for t in 0..4 {
        let want = (0..4).fold(f32::NEG_INFINITY, |m, f| m.max(at(&rows, t, f)));
        assert_eq!(at(&band, t, 0), want, "step {t}");
    }
    // And the whole series' peak is the grid's peak: folding further never loses the strongest thing.
    assert_eq!(band.range_db.unwrap().1, rows.range_db.unwrap().1);
}

#[test]
fn collapsing_a_window_to_one_row_is_the_max_over_its_columns() {
    // The same fold on the other axis (`nt = 1`): one max-hold per frequency cell over the whole
    // window — the survey strip's column, and `/api/coverage`'s shade. Many time cells collapse into
    // one, and a brief emission must survive that, which is exactly why the fold is a max.
    let h = grid(6, 3, |t, f| {
        Some(if t == 4 && f == 2 { -30.0 } else { -105.0 })
    });
    let strip = h.overview(window(0, 6), FreqRange::new(0.0, 3000.0), 1, 3);
    assert_eq!(strip.nt, 1);
    assert_eq!(at(&strip, 0, 0), -105.0);
    assert_eq!(at(&strip, 0, 1), -105.0);
    assert_eq!(
        at(&strip, 0, 2),
        -30.0,
        "a one-cell burst must light its column"
    );
}

#[test]
fn a_collapsed_step_with_no_coverage_is_unobserved_not_quiet() {
    // THE CONTROL. Three steps: one with energy, one **observed and quiet**, one never observed at
    // all. The band-collapsed series must keep the last two apart — the max of nothing is unknown,
    // not the bottom of the scale — and the difference is structural (`sources == 0`, `max_db` NaN),
    // not a small number. Emitting a floor value for step 2 turns "the receiver was not listening"
    // into "the band was quiet", and this assertion is what fails when it does.
    let h = grid(3, 2, |t, _| match t {
        0 => Some(-40.0),
        1 => Some(-120.0), // observed, and it was quiet: a finding
        _ => None,         // never observed: no claim either way
    });
    let band = h.overview(window(0, 3), FreqRange::new(0.0, 2000.0), 3, 1);

    assert!(band.cells[0].observed() && at(&band, 0, 0) == -40.0);
    // Observed and quiet: a real measurement, at the bottom of the grid's own range.
    assert!(band.cells[1].observed());
    assert_eq!(at(&band, 1, 0), -120.0);
    assert_eq!(band.cells[1].frames, 4);
    // Never observed: no value at all, and no frames to suggest one was taken.
    assert!(!band.cells[2].observed());
    assert!(band.cells[2].max_db.is_nan());
    assert!(band.cells[2].occupancy_max.is_nan());
    assert_eq!(band.cells[2].coverage, 0.0);
    assert_eq!(band.cells[2].frames, 0);
    // And the unobserved step is not the range's floor: the quiet step is.
    assert_eq!(band.range_db, Some((-120.0, -40.0)));
    assert_eq!(band.observed_cells, 2);
}

#[test]
fn the_fold_carries_the_scale_of_the_values_it_folded() {
    // A folded number whose scale is not carried with it is one a consumer must guess at, and a
    // guess is how "max-hold in dBFS/Hz" becomes "a level". The unit is the source grid's, never
    // assumed: calibrate the history and the overview says so.
    let mut h = grid(2, 1, |_, _| Some(-90.0));
    assert_eq!(
        h.overview(window(0, 2), FreqRange::new(0.0, 1000.0), 2, 1)
            .unit,
        PowerUnit::Dbfs
    );
    h.unit = PowerUnit::Dbm;
    assert_eq!(
        h.overview(window(0, 2), FreqRange::new(0.0, 1000.0), 2, 1)
            .unit,
        PowerUnit::Dbm
    );
}

#[test]
fn every_output_cell_of_a_fully_observed_window_is_observed() {
    // No hole may be opened by the index arithmetic itself: a window whose source cells are all
    // observed must compress to a grid with no unobserved cell, at any output shape.
    let h = grid(7, 5, |_, _| Some(-80.0));
    for (nt, nf) in [(1, 1), (3, 2), (7, 5), (13, 9), (64, 3)] {
        let o = h.overview(window(0, 7), FreqRange::new(0.0, 5000.0), nt, nf);
        assert_eq!(o.observed_cells, nt * nf, "{nt}×{nf}");
    }
}
