//! T-519: [`Pyramid::last_known`], the carry-forward fold behind the canvas's **shadow** tier
//! (fog-of-war), and [`LastKnown::carry_forward`], which lays it down a tile's rows.
//!
//! The semantics under test are the user's: a band swept and then departed carries its
//! most-recent-known value; a band never swept carries **nothing**; a cell observed now is its own
//! value and never a shadow.
//!
//! **The search never reads backward; the carry does, exactly once** (T-527). `last_known` still
//! refuses any value from after the instant it was asked about — that is what
//! [`a_value_from_after_the_instant_is_never_carried_backward`] holds it to. The single backward
//! read in the system is [`LastKnown::carry_forward`]'s, over the tile's **own** grid: the stretch
//! before a column's first-ever sample takes that sample, so a column observed only in the middle
//! of a view has no grey gap above it either. It is marked [`ShadowFill::Backward`] wherever it
//! goes.

use super::*;

/// 1 kHz × 1 s level 0 (10 s tiles), 2 kHz × 10 s level 1 (60 s tiles), 4 kHz × 60 s level 2
/// (600 s tiles): a welded ladder, so the search's stages are 1 s, 10 s and 60 s rows.
fn ladder() -> PyramidConfig {
    cfg(vec![level(1, 10), level(2, 6), level(2, 10)], 16)
}

/// Band A = 16–20 kHz at −60 dB for `T0 .. T0+5 s`; band B = 24–28 kHz at −70 dB for
/// `T0+100 .. T0+103 s`. 20–24 kHz and 28–32 kHz are never observed.
fn swept(dir: &TempDir) -> Pyramid {
    let mut p = Pyramid::open(&dir.0, ladder()).unwrap();
    let a = vec![lin(-60.0); 4];
    let b = vec![lin(-70.0); 4];
    for k in 0..5 {
        p.ingest(&frame(T0 + k * S, S, 16_000.0, 1000.0, &a))
            .unwrap();
    }
    for k in 100..103 {
        p.ingest(&frame(T0 + k * S, S, 24_000.0, 1000.0, &b))
            .unwrap();
    }
    p.seal_through(ts(T0 + 2000 * S)).unwrap();
    p
}

const BAND: (f64, f64) = (16_000.0, 32_000.0);

fn search(p: &Pyramid, before_s: i64, guard: Option<StraddleGuard>, total: usize) -> LastKnown {
    p.last_known(
        FreqRange::new(BAND.0, BAND.1),
        ts(T0 + before_s * S),
        16,
        guard,
        1_000_000,
        total,
    )
    .unwrap()
}

#[test]
fn a_departed_band_carries_its_newest_value_and_a_never_observed_one_carries_nothing() {
    let dir = TempDir::new("lastknown-semantics");
    let p = swept(&dir);
    let k = search(&p, 300, None, usize::MAX);
    for f in 0..4 {
        let c = k.cells[f];
        assert_eq!(c.max_db, -60.0, "A col {f}: {c:?}");
        // Found at level 2 (60 s cells), whose cell [T0, T0+60 s) is the newest holding A.
        assert_eq!((c.level, c.t_ns), (2, T0 + 60 * S), "A col {f}");
    }
    for f in 8..12 {
        let c = k.cells[f];
        assert_eq!(
            (c.max_db, c.level, c.t_ns),
            (-70.0, 2, T0 + 120 * S),
            "B col {f}"
        );
    }
    for f in (4..8).chain(12..16) {
        assert!(
            !k.cells[f].found(),
            "never observed, so NO value: {:?}",
            k.cells[f]
        );
        assert!(k.cells[f].max_db.is_nan());
    }
    assert_eq!(k.found(), 8);
    assert!(k.searched_from_ns <= T0, "the search reached back past A");
    assert!(k.source_cells > 0);
}

#[test]
fn the_newest_value_wins_and_is_read_at_the_finest_level_that_holds_it() {
    let dir = TempDir::new("lastknown-newest");
    let p = swept(&dir);
    // Just after B: B is found by the 1-s stage, A by the 60-s one.
    let k = search(&p, 103, None, usize::MAX);
    assert_eq!(
        (k.cells[8].max_db, k.cells[8].level, k.cells[8].t_ns),
        (-70.0, 0, T0 + 103 * S)
    );
    assert_eq!((k.cells[0].max_db, k.cells[0].level), (-60.0, 2));
    // The stages are newest-first and contiguous: [100, 103) at 1 s, [60, 100) at 10 s, then 60 s.
    let read: Vec<(u8, i64, i64)> = k
        .stages
        .iter()
        .filter(|s| !s.skipped)
        .map(|s| (s.level, (s.from_ns - T0) / S, (s.to_ns - T0) / S))
        .collect();
    assert_eq!(read[0], (0, 100, 103), "{read:?}");
    assert_eq!(read[1], (1, 60, 100), "{read:?}");
    assert_eq!((read[2].0, read[2].2), (2, 60), "{read:?}");
}

#[test]
fn a_value_from_after_the_instant_is_never_carried_backward() {
    let dir = TempDir::new("lastknown-backward");
    let p = swept(&dir);
    // Before B ever happened: B has no value, even though the level-2 cell [60, 120) that holds it
    // straddles T0+65 s.
    let k = search(&p, 65, None, usize::MAX);
    for f in 8..12 {
        assert!(
            !k.cells[f].found(),
            "B is from the future: {:?}",
            k.cells[f]
        );
    }
    // The fold itself refuses a cell straddling the instant with no guard: level 2's [60, 120)
    // holds B, and read directly it is still not a value at T0+65 s.
    let h = query(&p, BAND, (T0 + 60 * S, T0 + 120 * S), Resolution::Level(2));
    assert!(h.cells.iter().any(|c| c.observed()), "the cell does hold B");
    let mut out = LastKnown {
        before_ns: T0 + 65 * S,
        f_lo_hz: BAND.0,
        f_cell_hz: 1000.0,
        nf: 16,
        cells: vec![LastKnownCell::NONE; 16],
        stages: Vec::new(),
        searched_from_ns: T0,
        source_cells: 0,
    };
    let t_cells: Vec<i64> = p.geometry().levels.iter().map(|g| g.t_cell_ns).collect();
    assert_eq!(h.fold_newest(&mut out, &t_cells, None, &[]), 0);
    assert_eq!(out.found(), 0);

    // And in the middle of A: A's value is the one seen up to that instant.
    let k = search(&p, 2, None, usize::MAX);
    assert_eq!((k.cells[0].level, k.cells[0].t_ns), (0, T0 + 2 * S));
    assert!(!k.cells[8].found());
}

#[test]
fn a_straddling_coarse_cell_is_used_only_where_the_guard_proves_its_after_part_empty() {
    let dir = TempDir::new("lastknown-guard");
    let p = swept(&dir);
    // Budget 40 cells: the 1-s stage over [100, 103) costs 3 × 16 = 48, so it is merged into the
    // 10-s stage, which then reads [60, 110) — its top cell [100, 110) straddles T0+103 s.
    let all_clear = StraddleGuard {
        first_after: vec![i64::MAX; 16],
        known_until_ns: i64::MAX,
    };
    let k = search(&p, 103, Some(all_clear), 40);
    let b = k.cells[8];
    assert_eq!((b.max_db, b.level), (-70.0, 1), "{:?}", k.stages);
    assert_eq!(b.t_ns, T0 + 103 * S, "clamped to the instant: never later");
    assert!(k.source_cells <= 40, "{}", k.source_cells);

    // The tile shows B again at T0+105 s — before the straddling cell ends — so the cell may hold
    // frames from after the instant, and is refused for B.
    let mut first_after = vec![i64::MAX; 16];
    for f in first_after.iter_mut().skip(8).take(4) {
        *f = T0 + 105 * S;
    }
    let seen = StraddleGuard {
        first_after,
        known_until_ns: i64::MAX,
    };
    let k = search(&p, 103, Some(seen), 40);
    assert!(!k.cells[8].found(), "{:?}", k.cells[8]);

    // The guard knows only up to T0+108 s < the cell's end: unprovable, and REPORTED unsearched.
    let short = StraddleGuard {
        first_after: vec![i64::MAX; 16],
        known_until_ns: T0 + 108 * S,
    };
    let k = search(&p, 103, Some(short), 40);
    assert!(!k.cells[8].found());
    assert!(
        k.stages
            .iter()
            .any(|s| s.skipped && s.from_ns == T0 + 100 * S && s.to_ns == T0 + 103 * S),
        "the unprovable part-cell is stated, not read as empty: {:?}",
        k.stages
    );
}

#[test]
fn without_a_guard_a_stage_that_does_not_fit_is_reported_unsearched_never_empty() {
    let dir = TempDir::new("lastknown-skip");
    let p = swept(&dir);
    let k = search(&p, 103, None, 40);
    assert!(!k.cells[8].found(), "B lives only in the skipped window");
    assert!(
        k.stages
            .iter()
            .any(|s| s.skipped && s.from_ns == T0 + 100 * S && s.to_ns == T0 + 103 * S),
        "{:?}",
        k.stages
    );
}

fn overview(nt: usize, nf: usize, t0: i64, fill: impl Fn(usize, usize) -> Option<f32>) -> Overview {
    let mut cells = Vec::new();
    for t in 0..nt {
        for f in 0..nf {
            cells.push(match fill(t, f) {
                Some(db) => OverviewCell {
                    max_db: db,
                    occupancy_max: 0.0,
                    coverage: 1.0,
                    frames: 1,
                    sources: 1,
                },
                None => OverviewCell::UNOBSERVED,
            });
        }
    }
    Overview {
        unit: PowerUnit::Dbfs,
        nt,
        nf,
        t0_ns: t0,
        t_cell_ns: S as f64,
        f_lo_hz: 0.0,
        f_cell_hz: 1000.0,
        observed_cells: cells.iter().filter(|c| c.observed()).count(),
        cells,
        src_nt: nt,
        src_nf: nf,
        range_db: None,
    }
}

/// `(f, row, rows, max_db, t_ns relative to `before` in seconds, level, fill)` per run.
type Run = (usize, usize, usize, f32, i64, Option<u8>, ShadowFill);

fn runs_of(k: &LastKnown, runs: &[ShadowRun]) -> Vec<Run> {
    runs.iter()
        .map(|r| {
            (
                r.f,
                r.row,
                r.rows,
                r.max_db,
                (r.t_ns - k.before_ns) / S,
                r.level,
                r.fill,
            )
        })
        .collect()
}

#[test]
fn the_carry_runs_down_each_column_yield_to_the_grid_and_stop_at_the_data_edge() {
    let before = T0 + 1000 * S;
    let seed = |db: f32, age_s: i64| LastKnownCell {
        max_db: db,
        t_ns: before - age_s * S,
        level: 3,
    };
    let k = LastKnown {
        before_ns: before,
        f_lo_hz: 0.0,
        f_cell_hz: 1000.0,
        nf: 3,
        cells: vec![seed(-50.0, 10), LastKnownCell::NONE, seed(-40.0, 99)],
        stages: Vec::new(),
        searched_from_ns: T0,
        source_cells: 0,
    };
    // Column 1 is observed at row 2 only; column 2 at rows 0–1; the data edge is row 5's start.
    let g = overview(6, 3, before, |t, f| match (t, f) {
        (2, 1) => Some(-70.0),
        (0 | 1, 2) => Some(-45.0),
        _ => None,
    });
    let runs = k.carry_forward(Some(&g), S as f64, 6, Some(before + 5 * S));
    assert_eq!(
        runs_of(&k, &runs),
        vec![
            // Departed before the tile: the seed, down to the data edge and no further.
            (0, 0, 5, -50.0, -10, Some(3), ShadowFill::Forward),
            // T-527: nothing older than row 2 exists for this column, so the rows ABOVE its
            // first-ever sample take that sample — the one backward read — timestamped at the
            // start of the cell it came from, which is where the unknown stretch ends.
            (1, 0, 2, -70.0, 2, None, ShadowFill::Backward),
            // …and below it the same value carries forward, seen until row 2's end.
            (1, 3, 2, -70.0, 3, None, ShadowFill::Forward),
            // Observed at rows 0–1: those rows carry no shadow (a measurement is not replaced),
            // and the carry that follows is the tile's own value, not the older seed. Its head
            // needs no fill of either kind, because the grid measures row 0.
            (2, 2, 3, -45.0, 2, None, ShadowFill::Forward),
        ]
    );
}

/// **T-527, the fill rule and its one exception.** Every gap in a column that was ever observed is
/// filled; a column never observed at all carries **no run**, which is what keeps grey meaning
/// *we never looked*.
#[test]
fn every_gap_in_an_observed_column_is_filled_and_a_never_observed_one_carries_no_run() {
    let before = T0;
    let k = LastKnown {
        before_ns: before,
        f_lo_hz: 0.0,
        f_cell_hz: 1000.0,
        nf: 4,
        cells: vec![
            LastKnownCell::NONE,
            LastKnownCell::NONE,
            LastKnownCell::NONE,
            // Column 3 has an older value, so it has no "first-ever" sample in this grid.
            LastKnownCell {
                max_db: -30.0,
                t_ns: before - 7 * S,
                level: 2,
            },
        ],
        stages: Vec::new(),
        searched_from_ns: before - 100 * S,
        source_cells: 0,
    };
    // Column 0: observed in the MIDDLE only (rows 3–4) — the user's case. Column 1: observed at
    // row 0, so no head gap at all. Column 2: never observed, and nothing older. Column 3:
    // observed at row 5, with an older value above it.
    let g = overview(8, 4, before, |t, f| match (t, f) {
        (3 | 4, 0) => Some(-60.0),
        (0, 1) => Some(-65.0),
        (5, 3) => Some(-20.0),
        _ => None,
    });
    let runs = k.carry_forward(Some(&g), S as f64, 8, None);
    assert_eq!(
        runs_of(&k, &runs),
        vec![
            // The gap ABOVE the first-ever sample: that sample, read backward, and nothing else in
            // this plane ever reads backward.
            (0, 0, 3, -60.0, 3, None, ShadowFill::Backward),
            // The gap BELOW it: the nearest past sample, carried forward, to the end of the grid.
            (0, 5, 3, -60.0, 5, None, ShadowFill::Forward),
            // Observed at row 0: one forward carry and no backward fill — there is no gap above a
            // sample in row 0 to fill.
            (1, 1, 7, -65.0, 1, None, ShadowFill::Forward),
            // Column 3's head is the OLDER value carried FORWARD, not its row-5 sample read
            // backward: a backward fill happens only where nothing older exists.
            (3, 0, 5, -30.0, -7, Some(2), ShadowFill::Forward),
            (3, 6, 2, -20.0, 6, None, ShadowFill::Forward),
        ]
    );
    // The invariant the wire rests on: NO run touches column 2.
    assert!(
        !runs.iter().any(|r| r.f == 2),
        "a column never observed and with nothing older carries no run: {runs:?}"
    );
    // And every row of every column that WAS observed is either measured or covered exactly once.
    for f in [0usize, 1, 3] {
        for r in 0..8 {
            let measured = g.cells[r * 4 + f].observed();
            let covered = runs
                .iter()
                .filter(|x| x.f == f && x.row <= r && r < x.row + x.rows)
                .count();
            assert_eq!(
                usize::from(!measured),
                covered,
                "column {f} row {r}: measured {measured}, covered {covered}"
            );
        }
    }
}

/// A backward fill is read **only** from the column's own first sample, and never from a cell
/// belonging to some other column or to the future beyond it: the rows it covers end exactly where
/// that sample begins.
#[test]
fn a_backward_fill_reaches_no_further_than_the_first_ever_sample_it_reads() {
    let before = T0;
    let k = LastKnown {
        before_ns: before,
        f_lo_hz: 0.0,
        f_cell_hz: 1000.0,
        nf: 1,
        cells: vec![LastKnownCell::NONE],
        stages: Vec::new(),
        searched_from_ns: before,
        source_cells: 0,
    };
    let g = overview(4, 1, before, |t, _| (t == 2).then_some(-77.0));
    let runs = k.carry_forward(Some(&g), S as f64, 4, None);
    let back = runs
        .iter()
        .find(|r| r.fill == ShadowFill::Backward)
        .expect("the head takes the first-ever sample");
    assert_eq!((back.row, back.rows), (0, 2), "stops at the sample's row");
    assert_eq!(back.max_db, -77.0);
    // First seen at the START of the source cell: the instant the unknown stretch above it ends,
    // and the smallest age any row in the run can claim.
    assert_eq!(back.t_ns, before + 2 * S);
    assert!(
        back.t_ns > before + S,
        "a backward run's instant lies AFTER its own rows: that is what makes it backward"
    );
    // A grid with nothing in it (the coverage short-circuit's tile) has no first-ever sample, so
    // the backward fill cannot fire there at all — the path T-523 budgeted pays nothing for it.
    assert!(
        k.carry_forward(None, S as f64, 4, None).is_empty(),
        "no grid, no sample, no run"
    );
}

#[test]
fn a_tile_with_no_grid_carries_the_seed_over_every_row_up_to_the_edge() {
    let before = T0;
    let k = LastKnown {
        before_ns: before,
        f_lo_hz: 0.0,
        f_cell_hz: 1000.0,
        nf: 2,
        cells: vec![
            LastKnownCell {
                max_db: -80.0,
                t_ns: before - S,
                level: 0,
            },
            LastKnownCell::NONE,
        ],
        stages: Vec::new(),
        searched_from_ns: before,
        source_cells: 0,
    };
    let runs = k.carry_forward(None, S as f64, 4, None);
    assert_eq!(runs.len(), 1);
    assert_eq!((runs[0].f, runs[0].row, runs[0].rows), (0, 0, 4));
}

/// Cost of the search on scheme 1's real ladder, a tuned viewport against a device-wide one.
/// `cargo test --release -p hk-store last_known_cost -- --nocapture` prints the measurement.
#[test]
fn last_known_cost_is_bounded_on_a_tuned_and_a_device_wide_viewport() {
    let dir = TempDir::new("lastknown-cost");
    let mut p = Pyramid::open(
        &dir.0,
        PyramidConfig {
            checkpoint_interval: None,
            seal_lag: Duration::ZERO,
            byte_budget: u64::MAX,
            ..PyramidConfig::default()
        },
    )
    .unwrap();
    // 20 MHz at 6.25 kHz: 3200 bins. Five minutes on 100–120 MHz, then five on 400–420 MHz.
    let psd = vec![lin(-90.0); 3200];
    let t0 = T0;
    // And a long, sparse past: 390–410 MHz seen once an hour for two days, ending two days
    // earlier — so the search has to reach the hour and day levels to find it.
    let old = vec![lin(-95.0); 3200];
    let start = t0 - 4 * 86_400 * S;
    for h in 0..48 {
        p.ingest(&frame(start + h * 3600 * S, S, 390e6, 6250.0, &old))
            .unwrap();
    }
    for k in 0..600 {
        let lo = if k < 300 { 100e6 } else { 400e6 };
        p.ingest(&frame(t0 + k * S, S, lo, 6250.0, &psd)).unwrap();
    }
    let now = t0 + 600 * S;
    p.seal_through(ts(now)).unwrap();
    let guard = |nf: usize| StraddleGuard {
        first_after: vec![i64::MAX; nf],
        known_until_ns: i64::MAX,
    };
    let (per_step, total) = (500_000, 2_000_000);
    for (name, lo, hi) in [
        ("tuned 1.6 MHz", 100e6, 101.6e6),
        ("12.8 MHz", 96e6, 108.8e6),
        ("51.2 MHz over the old band", 380e6, 431.2e6),
        ("819 MHz (the widest tile /api/tiles serves)", 0.0, 819.2e6),
        ("one 1 MHz-6 GHz region", 1e6, 6e9),
    ] {
        let started = std::time::Instant::now();
        let k = p
            .last_known(
                FreqRange::new(lo, hi),
                ts(now),
                256,
                Some(guard(256)),
                per_step,
                total,
            )
            .unwrap();
        let ms = started.elapsed().as_secs_f64() * 1e3;
        eprintln!(
            "T-519 last_known {name}: {ms:.1} ms, {} source cells, {} of 256 columns found, \
             stages (level, window_s, cells, skipped) {:?}",
            k.source_cells,
            k.found(),
            k.stages
                .iter()
                .map(|s| (
                    s.level,
                    (s.to_ns - s.from_ns) / S,
                    s.source_cells,
                    s.skipped
                ))
                .collect::<Vec<_>>()
        );
        assert!(k.source_cells <= total, "{name}: {}", k.source_cells);
        let col_of = |f: f64| ((f - lo) / ((hi - lo) / 256.0)) as usize;
        let wide = hi - lo > 819.2e6;
        if lo < 110e6 && hi > 110e6 {
            let c = k.cells[col_of((lo.max(100e6) + hi.min(120e6)) / 2.0)];
            if !wide {
                // 100-120 MHz was observed and departed: every viewport a tile can be finds it.
                assert_eq!(
                    c.max_db, -90.0,
                    "{name}: the departed band has a value: {c:?}"
                );
            } else if !c.found() {
                // A single 6 GHz region cannot afford the fine stages; what it could not read is
                // STATED as unsearched, never answered as "nothing there".
                assert!(k.stages.iter().any(|s| s.skipped), "{name}: {:?}", k.stages);
            }
        }
        if lo <= 390e6 && hi >= 400e6 && !wide {
            // 390-400 MHz was last seen two days ago, at the hour and day levels only.
            let c = k.cells[col_of(395e6)];
            assert_eq!(c.max_db, -95.0, "{name}: {c:?}");
            assert!(c.t_ns < t0 - 86_400 * S, "{name}");
            assert!(c.level >= 3, "{name}: {c:?}");
        }
    }
}
