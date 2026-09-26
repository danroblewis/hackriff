//! T-1058: the **last-known ledger** — the fog-of-war's source, kept as rows arrive rather than
//! searched for per tile. See [`super::super::ledger`].
//!
//! What is held to: a departed band's newest value is answered **exactly** however far away the
//! question is (no budget, nothing searched); the value is the store's own cell's for the last row
//! at the asked time level (the T-911 colour); a never-observed column is `Never`; the ledger
//! survives a restart and the removal of every tile it was folded beside; a column looked at again
//! after the asked instant is `Later`, never a value from its future.

use super::*;

/// 1 kHz × 1 s level 0 (10 s tiles), 2 kHz × 10 s level 1 (60 s tiles), 4 kHz × 60 s level 2.
fn ladder() -> PyramidConfig {
    cfg(vec![level(1, 10), level(2, 6), level(2, 10)], 16)
}

const BAND: (f64, f64) = (16_000.0, 32_000.0);

/// Band A = 16–20 kHz for `T0 .. T0+5 s` (values −60 … −56 dB rising, with −40 dB in the second
/// frame); band B = 24–28 kHz at −70 dB for `T0+100 .. T0+103 s`. 20–24 and 28–32 kHz never.
fn swept(p: &mut Pyramid) {
    for k in 0..5i64 {
        let v = if k == 1 { -40.0 } else { -60.0 + k as f32 };
        let a = vec![lin(v); 4];
        p.ingest(&frame(T0 + k * S, S, 16_000.0, 1000.0, &a))
            .unwrap();
    }
    let b = vec![lin(-70.0); 4];
    for k in 100..103 {
        p.ingest(&frame(T0 + k * S, S, 24_000.0, 1000.0, &b))
            .unwrap();
    }
}

fn ask(p: &Pyramid, t_cell_s: i64, before_s: Option<i64>) -> LedgerAnswer {
    p.last_known_ledger(
        None,
        FreqRange::new(BAND.0, BAND.1),
        16,
        t_cell_s * S,
        before_s.map(|b| ts(T0 + b * S)),
    )
}

fn known(c: &LedgerColumn) -> LedgerValue {
    match c {
        LedgerColumn::Known(v) => *v,
        other => panic!("expected a known value, got {other:?}"),
    }
}

#[test]
fn a_departed_band_is_known_exactly_at_any_distance_and_a_never_observed_one_is_never() {
    let dir = TempDir::new("ledger-departed");
    let mut p = Pyramid::open(&dir.0, ladder()).unwrap();
    swept(&mut p);
    // Seconds, a day, a year after: the same exact answer — no budget, no reach.
    for before in [200, 86_400, 365 * 86_400] {
        let a = ask(&p, 1, Some(before));
        assert!(a.complete);
        for f in 0..4 {
            let v = known(&a.columns[f]);
            assert_eq!(v.last_ns, T0 + 5 * S, "A's last row ends at T0+5 s");
            assert_eq!(v.first_ns, T0, "A's first row");
            assert!(
                (v.max_db - (-56.0)).abs() < 1e-3,
                "the last row's value: {v:?}"
            );
        }
        for f in 8..12 {
            let v = known(&a.columns[f]);
            assert_eq!(v.last_ns, T0 + 103 * S);
            assert!((v.max_db - (-70.0)).abs() < 1e-3);
        }
        for f in (4..8).chain(12..16) {
            assert_eq!(a.columns[f], LedgerColumn::Never, "column {f}");
        }
    }
    // Before B arrived: B has nothing before, A is known.
    let mid = ask(&p, 1, Some(50));
    assert_eq!(known(&mid.columns[0]).last_ns, T0 + 5 * S);
    assert_eq!(
        mid.columns[8],
        LedgerColumn::NothingBefore {
            first_ns: T0 + 100 * S
        }
    );
    // Inside A's dwell: A was seen before T0+3 s and after it — the ledger holds only the newest,
    // so it says so rather than answering with a value from the question's future.
    let inside = ask(&p, 1, Some(3));
    assert_eq!(
        inside.columns[0],
        LedgerColumn::Later {
            newest_ns: T0 + 5 * S
        }
    );
    // No instant: the newest, whenever.
    assert_eq!(known(&ask(&p, 1, None).columns[9]).last_ns, T0 + 103 * S);
}

/// The max-hold is over the store's cell **at the asked time level** that holds the last row — so
/// the shadow has exactly the colour the store's own cell at that zoom had (T-911), checked against
/// the store's own query of that cell.
#[test]
fn the_value_is_the_stores_own_cell_for_the_last_row_at_every_time_level() {
    let dir = TempDir::new("ledger-levels");
    let mut p = Pyramid::open(&dir.0, ladder()).unwrap();
    swept(&mut p);
    p.seal_through(ts(T0 + 2000 * S)).unwrap();
    for (t_cell_s, level_f_cell, expect) in
        [(1, 1000.0, -56.0), (10, 2000.0, -40.0), (60, 4000.0, -40.0)]
    {
        // At level 0 the last 1-s row is −56 dB; the 10-s and 60-s cells that hold it also hold the
        // −40 dB frame, and their max says so.
        let a = p.last_known_ledger(
            None,
            FreqRange::new(16_000.0, 20_000.0),
            (4000.0 / level_f_cell) as usize,
            t_cell_s * S,
            Some(ts(T0 + 1000 * S)),
        );
        let v = known(&a.columns[0]);
        assert_eq!(a.t_cell_ns, t_cell_s * S);
        assert!((v.max_db - expect).abs() < 1e-3, "{t_cell_s} s: {v:?}");
        // The store's own cell of that level, over the row holding A's last frame.
        let row0 = (T0 + 4 * S).div_euclid(t_cell_s * S) * t_cell_s * S;
        let q = query(
            &p,
            (16_000.0, 16_000.0 + level_f_cell),
            (row0, row0 + t_cell_s * S),
            Resolution::Level(
                p.geometry()
                    .levels
                    .iter()
                    .position(|g| g.t_cell_ns == t_cell_s * S)
                    .unwrap() as u8,
            ),
        );
        let own = q
            .cells
            .iter()
            .filter(|c| c.frames > 0)
            .map(|c| c.max_db)
            .fold(f32::NEG_INFINITY, f32::max);
        assert!(
            (own - v.max_db).abs() <= 0.006,
            "{t_cell_s} s: the store's own cell {own} vs the ledger {}",
            v.max_db
        );
    }
}

/// The ledger survives a restart, and is independent of the tiles: with every tile file gone (the
/// byte budget's worst case) the departed band still has its last-known value.
#[test]
fn the_ledger_persists_across_a_restart_and_outlives_its_tiles() {
    let dir = TempDir::new("ledger-restart");
    let before = {
        let mut p = Pyramid::open(&dir.0, ladder()).unwrap();
        swept(&mut p);
        let a = ask(&p, 1, Some(500));
        p.close().unwrap();
        a
    };
    let p = Pyramid::open(&dir.0, ladder()).unwrap();
    let st = p.ledger_stats();
    assert!(st.loaded && st.complete, "{st:?}");
    assert_eq!(st.cells, 8, "{st:?}");
    let after = ask(&p, 1, Some(500));
    for f in 0..16 {
        match (before.columns[f], after.columns[f]) {
            (LedgerColumn::Known(a), LedgerColumn::Known(b)) => {
                assert_eq!(
                    (a.last_ns, a.first_ns, a.epoch),
                    (b.last_ns, b.first_ns, b.epoch)
                );
                // Persisted at the tiles' own 0.01 dB.
                assert!((a.max_db - b.max_db).abs() <= 0.005, "{a:?} {b:?}");
            }
            (a, b) => assert_eq!(a, b, "column {f}"),
        }
    }
    drop(p);
    // Every tile removed: the history is gone, the last-known value is not.
    let root = dir.0.join("history").join("s7");
    for e in fs::read_dir(&root).unwrap() {
        let path = e.unwrap().path();
        if path.is_dir() {
            fs::remove_dir_all(&path).unwrap();
        }
    }
    let p = Pyramid::open(&dir.0, ladder()).unwrap();
    assert_eq!(known(&ask(&p, 1, Some(500)).columns[0]).last_ns, T0 + 5 * S);
}

/// A store that already held history when its ledger began (written before T-1058, or its file
/// lost) is **incomplete**: its "never" is not proof, and it says so.
#[test]
fn a_ledger_begun_on_existing_history_is_incomplete() {
    let dir = TempDir::new("ledger-incomplete");
    {
        let mut p = Pyramid::open(&dir.0, ladder()).unwrap();
        swept(&mut p);
        p.close().unwrap();
    }
    fs::remove_file(dir.0.join("history").join("s7").join("last_known.ledger")).unwrap();
    let mut p = Pyramid::open(&dir.0, ladder()).unwrap();
    let a = ask(&p, 1, Some(500));
    assert!(!a.complete);
    assert_eq!(a.columns[0], LedgerColumn::Never, "not seen by THIS ledger");
    // What it sees from here on it knows, and the flag persists.
    p.ingest(&frame(T0 + 200 * S, S, 16_000.0, 1000.0, &[lin(-50.0); 4]))
        .unwrap();
    assert_eq!(
        known(&ask(&p, 1, Some(500)).columns[0]).last_ns,
        T0 + 201 * S
    );
    p.close().unwrap();
    let p = Pyramid::open(&dir.0, ladder()).unwrap();
    assert!(!p.ledger_stats().complete);
}

/// A corrupt file is discarded, never half-read.
#[test]
fn a_corrupt_ledger_file_is_discarded() {
    let dir = TempDir::new("ledger-corrupt");
    {
        let mut p = Pyramid::open(&dir.0, ladder()).unwrap();
        swept(&mut p);
        p.close().unwrap();
    }
    let path = dir.0.join("history").join("s7").join("last_known.ledger");
    let mut b = fs::read(&path).unwrap();
    let n = b.len();
    b[n - 3] ^= 0x5a;
    fs::write(&path, b).unwrap();
    let p = Pyramid::open(&dir.0, ladder()).unwrap();
    let st = p.ledger_stats();
    assert!(!st.loaded && !st.complete && st.cells == 0, "{st:?}");
}

/// Per source (a front end): scoped answers, the newest across sources when unscoped, and a tune
/// epoch that advances on a retune.
#[test]
fn sources_are_kept_apart_and_a_retune_opens_a_new_epoch() {
    let dir = TempDir::new("ledger-sources");
    let mut p = Pyramid::open(&dir.0, ladder()).unwrap();
    let (a, b) = (source_key("hackrf-a"), source_key("hackrf-b"));
    let org = |source| FrameOrigin { source, site: None };
    let v = [lin(-60.0); 4];
    p.ingest(&frame(T0, S, 16_000.0, 1000.0, &v).with_origin(org(a)))
        .unwrap();
    p.ingest(&frame(T0 + 10 * S, S, 24_000.0, 1000.0, &v).with_origin(org(a)))
        .unwrap();
    p.ingest(&frame(T0 + 20 * S, S, 16_000.0, 1000.0, &v).with_origin(org(b)))
        .unwrap();
    let col = |src, f: usize| {
        p.last_known_ledger(src, FreqRange::new(BAND.0, BAND.1), 16, S, None)
            .columns[f]
    };
    assert_eq!(known(&col(Some(a), 0)).last_ns, T0 + S);
    assert_eq!(known(&col(Some(a), 0)).epoch, 0);
    assert_eq!(
        known(&col(Some(a), 8)).epoch,
        1,
        "the retune opened epoch 1"
    );
    assert_eq!(known(&col(Some(b), 0)).last_ns, T0 + 21 * S);
    assert_eq!(col(Some(b), 8), LedgerColumn::Never);
    let any = known(&col(None, 0));
    assert_eq!(
        (any.last_ns, any.source),
        (T0 + 21 * S, b),
        "the newest source wins"
    );
    assert_eq!(p.ledger_stats().sources, 2);
}

/// A frame arriving out of order never moves a cell's newest time back; it raises only the
/// max-holds of the time cells it shares with the newest.
#[test]
fn a_late_frame_never_moves_the_newest_back() {
    let dir = TempDir::new("ledger-late");
    let mut p = Pyramid::open(&dir.0, cfg(vec![level(1, 100), level(2, 6)], 16)).unwrap();
    p.ingest(&frame(T0 + 5 * S, S, 16_000.0, 1000.0, &[lin(-60.0); 4]))
        .unwrap();
    p.ingest(&frame(T0 + 3 * S, S, 16_000.0, 1000.0, &[lin(-30.0); 4]))
        .unwrap();
    let a = p.last_known_ledger(None, FreqRange::new(16_000.0, 20_000.0), 4, S, None);
    let v = known(&a.columns[0]);
    assert_eq!((v.last_ns, v.first_ns), (T0 + 6 * S, T0 + 3 * S));
    assert!(
        (v.max_db + 60.0).abs() < 1e-3,
        "the 1 s cell is the newest's: {v:?}"
    );
    let coarse = p.last_known_ledger(None, FreqRange::new(16_000.0, 20_000.0), 2, 100 * S, None);
    assert!(
        (known(&coarse.columns[0]).max_db + 30.0).abs() < 1e-3,
        "the 100 s cell holds both frames"
    );
}

/// **T-453: the per-row cost on the capture thread, measured.** A 20 MHz row on the shipped
/// spectrum-history scheme (6.25 kHz × 5 time levels: 3200 cells) and on the shipped view lattice
/// (8 time levels), timed over whole [`Pyramid::ingest`] calls with the ledger's share isolated by
/// timing [`Ledger::note_row`] alone on the same rows. Printed, never asserted (a timing bound is
/// the `timing` tier's, docs/10 §3.6); `cargo test --release -p hk-store ledger_cost -- --nocapture`.
#[test]
fn ledger_cost_per_row_is_measured() {
    for (name, cfg) in [
        ("spectrum-history", PyramidConfig::default()),
        (
            "view-lattice",
            PyramidConfig::view_lattice(ViewLattice {
                f_cell_hz: 6250.0,
                t_cell: Duration::from_millis(40),
                ..ViewLattice::default()
            }),
        ),
    ] {
        let g = cfg.geometry().unwrap();
        let mut l = super::super::ledger::Ledger::new(&g);
        let f0 = g.levels[0].f_cell_hz;
        let t0 = g.levels[0].t_cell_ns;
        let c0 = (100e6 / f0) as i64;
        let cells: Vec<(i64, f32)> = (0..3200)
            .map(|i| (c0 + i, -80.0 + (i % 7) as f32))
            .collect();
        let rows = 2000i64;
        let tc0 = (T0 / t0) + 1;
        let start = std::time::Instant::now();
        for r in 0..rows {
            l.note_row(1, tc0 + r, (c0, c0 + 3200), &cells);
        }
        let per_row_us = start.elapsed().as_secs_f64() * 1e6 / rows as f64;
        let st = l.stats();
        let q = std::time::Instant::now();
        let a = l.columns(None, FreqRange::new(90e6, 130e6), 256, t0, i64::MAX);
        let q_us = q.elapsed().as_secs_f64() * 1e6;
        eprintln!(
            "T-1058 ledger {name}: note_row {per_row_us:.1} us/row over 3200 cells x {} time \
             levels ({:.1} ns/cell); {} cells resident {} B; one 256-column 40 MHz read {q_us:.0} \
             us over {} cells, {} known",
            g.t_axis().len(),
            per_row_us * 1e3 / 3200.0,
            st.cells,
            st.resident_bytes,
            a.cells_read,
            a.known(),
        );
        assert_eq!(st.cells, 3200);
        assert!(a.known() > 0);
        // The whole ingest of the same row, ledger included, for scale.
        let dir = TempDir::new("ledger-cost");
        let mut p = Pyramid::open(
            &dir.0,
            PyramidConfig {
                checkpoint_interval: None,
                ..cfg
            },
        )
        .unwrap();
        let psd: Vec<f32> = (0..3200).map(|i| lin(-80.0 + (i % 7) as f32)).collect();
        let t_start = (T0 / t0 + 1) * t0;
        let n = 500i64;
        let start = std::time::Instant::now();
        for r in 0..n {
            p.ingest(&frame(t_start + r * t0, t0, c0 as f64 * f0, f0, &psd))
                .unwrap();
        }
        let ingest_us = start.elapsed().as_secs_f64() * 1e6 / n as f64;
        eprintln!(
            "T-1058 ledger {name}: whole Pyramid::ingest {ingest_us:.1} us/row (ledger share {:.0} %)",
            100.0 * per_row_us / ingest_us
        );
    }
}
