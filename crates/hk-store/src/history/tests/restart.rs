//! T-942 — **a restart on an existing data dir must keep recording history.**
//!
//! Found on staging by the explorer, 2026-09-25: after restarting the app on the same
//! `--data-dir`, `/api/status` read `history.frames_ingested 0` and `frames_late 2024` — *every*
//! spectrum-history frame of the new run was refused as arriving behind the watermark — while
//! `/api/history` answered `observed_cells 0` for the live window. The view lattice was
//! unaffected (`view_frames 5050`), which is the clue: it is the only pyramid whose end-of-run
//! seal did not reach past its data.
//!
//! **The mechanism**, in two halves, one per test below.
//!
//! 1. The run-end seal went `seal_through(last_frame + 1 h)`, to force every partially-filled tile
//!    shut. It also sealed tiles of time blocks that **had not happened yet**.
//! 2. [`Pyramid::recover`] then resumed the watermark from the newest sealed block end **of every
//!    level**. Scheme 1's level 2 is a one-hour tile, so a store last written at 04:26 came back
//!    claiming 05:00 — and `ingest` answers [`IngestOutcome::Late`] for every frame whose level-0
//!    block ends at or before the watermark. 34 minutes of the next run, refused. The same value
//!    is `latest_frame_end`, so the store also reported its newest recorded row as an exact hour
//!    boundary in the future, which is what `/api/navigation` and `/api/analysis/strongest` then
//!    served (the explorer's second finding, same root cause).
//!
//! Both halves are fixed here: the seal stops at the data (`hk_pipeline::history`), and recovery
//! takes the persisted last frame end, else the newest sealed **level-0** block — never a coarse
//! level's rounding-up of it.

use super::*;

/// 1 kHz × 1 s cells; level 0 is a 10 s tile, level 1 a 60 s tile. The 6× gap between them is
/// scheme 1's hour-against-a-minute in miniature: it is the coarse level that used to decide the
/// resumed watermark.
fn restart_cfg() -> PyramidConfig {
    // No checkpoint interval: `close` checkpoints the open tiles anyway, and that is the
    // gesture a restart actually depends on.
    cfg(vec![level(1, 10), level(2, 6)], 16)
}

/// Folds `n` one-second frames from `t`, and returns the end of the last one.
fn run(p: &mut Pyramid, t: i64, n: i64) -> i64 {
    for k in 0..n {
        let psd = vec![lin(-90.0); 16];
        let f = frame(t + k * S, S, 16_000.0, 1000.0, &psd);
        assert_eq!(p.ingest(&f).unwrap(), IngestOutcome::Folded, "frame {k}");
    }
    t + n * S
}

#[test]
fn t942_a_restart_on_an_existing_data_dir_records_history() {
    let dir = TempDir::new("t942-restart");
    // ---- run 1: 25 s of frames, then the run-end gesture (seal through the data, checkpoint) ----
    let mut p = Pyramid::open(&dir.0, restart_cfg()).unwrap();
    let end1 = run(&mut p, T0, 25);
    p.seal_all(ts(end1)).unwrap();
    p.close().unwrap();

    // ---- run 2, on the same directory ----
    let mut p = Pyramid::open(&dir.0, restart_cfg()).unwrap();
    // The newest recorded row is never in the future: it is the last frame the store folded, to
    // the nanosecond, not the end of whatever tile happened to contain it.
    assert_eq!(
        p.latest_frame_end(),
        Some(ts(end1)),
        "the resumed latest frame end must be the data's own edge"
    );
    assert!(
        p.watermark().as_unix_nanos() <= end1,
        "the resumed watermark ({}) ran past the last frame ({end1})",
        p.watermark().as_unix_nanos()
    );

    // A restart is a gap of a few seconds, not a new epoch: the next frames land in the tile the
    // last run was in the middle of — forced shut by the run-end seal and reopened here — and in
    // the ones after it.
    assert!(
        p.stats().tiles_reopened > 0,
        "the tile the last run died in was left sealed over time it had not recorded"
    );
    let end2 = run(&mut p, end1 + 2 * S, 20);
    assert_eq!(
        p.stats().frames_late,
        0,
        "frames of a restarted run are late"
    );

    // Both runs' measurements are readable, through one query, with no gap but the 2 s the radio
    // was off.
    p.seal_through(ts(end2)).unwrap();
    let h = query(&p, (16_000.0, 17_000.0), (T0, end2), Resolution::Level(0));
    let observed = h.cells.iter().filter(|c| c.observed()).count();
    assert_eq!(
        observed, 45,
        "25 s before the restart and 20 s after it, one 1 s cell each"
    );
}

#[test]
fn t942_a_store_sealed_past_its_data_resumes_at_the_level_0_edge() {
    // The legacy case, and the one the explorer hit: a data dir written by the old run-end seal,
    // which reached an hour past the last frame and so sealed level 1's whole 60 s tile (scheme
    // 1's whole HOUR) over 25 s of data. Recovery must still not claim time that no tile of the
    // ingest level has closed: the bound is level 0's block end (30 s here), never level 1's
    // (60 s), and a frame after it folds.
    let dir = TempDir::new("t942-sealed-ahead");
    let mut p = Pyramid::open(&dir.0, restart_cfg()).unwrap();
    let end1 = run(&mut p, T0, 25);
    p.seal_through(ts(end1 + 3600 * S)).unwrap();
    assert!(
        end1 < T0 + 30 * S,
        "the last frame is inside the open level-0 tile"
    );
    p.close().unwrap();
    // The old code never wrote the edge file: drop it, so this really is a legacy dir.
    let removed = fs::remove_file(dir.0.join("history").join("s7").join("edge"));
    assert!(
        removed.is_ok(),
        "the edge file is written where this expects it"
    );

    let mut p = Pyramid::open(&dir.0, restart_cfg()).unwrap();
    assert_eq!(
        p.watermark().as_unix_nanos(),
        T0 + 30 * S,
        "the resumed watermark must be the newest sealed LEVEL-0 block end"
    );
    let end2 = run(&mut p, T0 + 35 * S, 10);
    assert_eq!(p.stats().frames_late, 0);
    p.seal_through(ts(end2)).unwrap();
    let h = query(
        &p,
        (16_000.0, 17_000.0),
        (T0 + 35 * S, end2),
        Resolution::Level(0),
    );
    assert_eq!(h.cells.iter().filter(|c| c.observed()).count(), 10);
}
