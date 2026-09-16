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
fn coverage_is_the_mean_observed_fraction_of_the_folded_cells() {
    // Source cells are equal-duration, so the mean of their coverage is the output cell's observed
    // fraction — a fold that is exact rather than a chosen representative.
    let mut h = grid(4, 1, |_, _| Some(-90.0));
    h.cells[0].coverage = 0.5;
    h.cells[1].coverage = 1.0;
    let o = h.overview(window(0, 4), FreqRange::new(0.0, 1000.0), 2, 1);
    assert!((o.cells[0].coverage - 0.75).abs() < 1e-6);
    assert!((o.cells[1].coverage - 1.0).abs() < 1e-6);
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
