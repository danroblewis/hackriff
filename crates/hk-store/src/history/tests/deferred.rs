//! **T-901: a deferred seal does no file work, and nothing about the store changes because of it.**
//!
//! The defect was a lock held across a seal: `/ws/tiles/rows` stalled for 1.0-4.5 s every 64
//! rows because the view writer's `ingest` encoded, wrote and fsync'd ~24 tiles while holding
//! the mutex every row push takes. The fix moves that work out of `ingest`
//! ([`Pyramid::set_deferred_writes`]), so the structural property to pin is exactly that: **with
//! deferred writes on, `ingest` creates no file at all** — then that deferring changes nothing a
//! reader or the disk can see, compared cell for cell and byte for byte with the inline store fed
//! the same frames. No wall-clock bound: the timing claim lives in the `timing` tier
//! (`hk-cli/tests/row_push_cadence.rs`).
//!
//! Each test runs the **inline store as its control** over the same frames, and asserts the
//! control shows what the fix removes (tile files created during `ingest`) before judging the
//! deferred one — so none of this can pass by judging nothing.

use super::*;

fn lat() -> ViewLattice {
    ViewLattice {
        scheme: 11,
        f_cell_hz: 1000.0,
        t_cell: Duration::from_secs(1),
        cells_per_block: 16,
        f_levels: 3,
        t_levels: 3,
    }
}

fn cfg() -> PyramidConfig {
    PyramidConfig {
        histogram: HistogramConfig {
            lo_db: -130.0,
            step_db: 5.0,
            bins: 30,
        },
        seal_lag: Duration::ZERO,
        checkpoint_interval: Some(Duration::from_secs(7)),
        byte_budget: u64::MAX,
        ..PyramidConfig::view_lattice(lat())
    }
}

const N_BINS: usize = 16 * 4;

/// Every `*.tile` file under `root`, as (path relative to `root`, bytes).
fn tile_files(root: &std::path::Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, root, out);
            } else if p.extension().is_some_and(|x| x == "tile") {
                out.push((
                    p.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(&p).unwrap(),
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// Feeds `secs` one-second frames, calling `each` after every one.
fn feed(p: &mut Pyramid, secs: i64, mut each: impl FnMut(&mut Pyramid)) {
    for s in 0..secs {
        let mut rng = Rng(0xDEF0_0901 ^ s as u64);
        let psd: Vec<f32> = (0..N_BINS)
            .map(|b| {
                let floor = -100.0 + 10.0 * rng.gamma(4).log10() as f32;
                lin(floor + if b % 11 == 3 { 35.0 } else { 0.0 })
            })
            .collect();
        p.ingest(&frame(T0 + s * S, S, 0.0, 1000.0, &psd)).unwrap();
        each(p);
    }
}

/// Every level's overview over the whole run, as text (an unobserved cell is `NaN`, which `==`
/// cannot compare).
fn overviews(p: &Pyramid, secs: i64) -> Vec<String> {
    let freq = FreqRange::new(0.0, N_BINS as f64 * 1000.0);
    let time = TimeRange::new(ts(T0), ts(T0 + secs * S));
    (0..p.geometry().n_levels())
        .map(|l| {
            let h = p
                .query(&RegionQuery {
                    freq,
                    time,
                    resolution: Resolution::Level(l as u8),
                })
                .unwrap();
            format!("L{l}: {:?}", h.overview(time, freq, 32, 16).cells)
        })
        .collect()
}

/// Seconds of capture: several level-0 blocks (16 s) and one of the coarsest time level's, so
/// seals happen at every level and at least one checkpoint lands between them.
const SECS: i64 = 16 * 4 + 5;

#[test]
fn a_deferred_seal_creates_no_file_and_reads_exactly_as_the_inline_store() {
    let (dir_i, dir_d) = (TempDir::new("t901-inline"), TempDir::new("t901-deferred"));
    let mut inline = Pyramid::open(&dir_i.0, cfg()).unwrap();
    let mut deferred = Pyramid::open(&dir_d.0, cfg()).unwrap();
    deferred.set_deferred_writes(true).unwrap();
    let root_i = dir_i.0.join("history").join("s11");
    let root_d = dir_d.0.join("history").join("s11");

    // The control: the inline store writes tile files INSIDE `ingest` — the work the view writer
    // used to do while holding the lock every row push takes.
    let mut files_during_ingest = 0usize;
    feed(&mut inline, SECS, |p| {
        files_during_ingest = files_during_ingest.max(tile_files(&root_i).len());
        let _ = p;
    });
    assert!(
        files_during_ingest > 0 && inline.stats().tiles_written > 0,
        "the control never sealed a tile during ingest, so this test judges nothing"
    );

    // The deferred store: the same seals happen (the index says so), and not one file exists.
    feed(&mut deferred, SECS, |p| {
        assert!(
            tile_files(&root_d).is_empty(),
            "a deferred ingest created a tile file: the seal's file work ran inside ingest"
        );
        let _ = p;
    });
    assert_eq!(
        deferred.stats().tiles_written,
        inline.stats().tiles_written,
        "the deferred store sealed a different number of tiles"
    );
    assert!(deferred.queued_writes() > 0 && deferred.unwritten_tiles() > 0);

    // Before a single file lands, every read answers exactly as the inline store does: a sealed
    // tile is never unreadable between its seal and its write.
    assert_eq!(overviews(&deferred, SECS), overviews(&inline, SECS));

    // Performed with no pyramid in reach, then booked.
    let pending = deferred.take_writes();
    assert!(!pending.is_empty());
    let done = pending.perform();
    deferred.land_writes(done).unwrap();
    assert_eq!(deferred.unwritten_tiles(), 0);
    assert_eq!(deferred.queued_writes(), 0);

    // Byte for byte the files the inline path wrote, and the same budget accounting.
    assert_eq!(tile_files(&root_d), tile_files(&root_i));
    assert_eq!(deferred.disk_bytes(), inline.disk_bytes());
    assert_eq!(deferred.stats().bytes_written, inline.stats().bytes_written);
    assert_eq!(overviews(&deferred, SECS), overviews(&inline, SECS));
}

#[test]
fn a_second_take_waits_for_the_first_batch_to_land() {
    let dir = TempDir::new("t901-order");
    let mut p = Pyramid::open(&dir.0, cfg()).unwrap();
    p.set_deferred_writes(true).unwrap();
    feed(&mut p, 16 + 1, |_| {});
    let first = p.take_writes();
    assert!(
        !first.is_empty(),
        "the first level-0 block should have sealed"
    );
    // More seals while the first batch is out.
    for s in 17..SECS {
        let psd = vec![lin(-100.0); N_BINS];
        p.ingest(&frame(T0 + s * S, S, 0.0, 1000.0, &psd)).unwrap();
    }
    assert!(p.queued_writes() > 0);
    assert!(
        p.take_writes().is_empty(),
        "a second flusher was handed writes while the first batch was out: two threads could \
         then write one tile out of order, or share its temp file"
    );
    p.land_writes(first.perform()).unwrap();
    let second = p.take_writes();
    assert!(
        !second.is_empty(),
        "the queued writes were not released by the landing"
    );
    p.land_writes(second.perform()).unwrap();
    assert_eq!(p.unwritten_tiles(), 0);
}

#[test]
fn dropping_a_deferred_store_writes_what_it_queued() {
    // Both stores are DROPPED, not closed: a drop loses at most one checkpoint interval of level 0
    // either way (`Pyramid::close`'s contract), so the comparison is like for like, and what it
    // judges is only that the queued seals were not lost with the pyramid.
    let (dir_i, dir_d) = (TempDir::new("t901-drop-i"), TempDir::new("t901-drop-d"));
    {
        let mut p = Pyramid::open(&dir_i.0, cfg()).unwrap();
        feed(&mut p, SECS, |_| {});
    }
    {
        let mut p = Pyramid::open(&dir_d.0, cfg()).unwrap();
        p.set_deferred_writes(true).unwrap();
        feed(&mut p, SECS, |_| {});
        assert!(p.queued_writes() > 0 && p.unwritten_tiles() > 0);
    }
    let root = |d: &TempDir| d.0.join("history").join("s11");
    assert!(!tile_files(&root(&dir_d)).is_empty());
    assert_eq!(tile_files(&root(&dir_d)), tile_files(&root(&dir_i)));
    let reopened = Pyramid::open(&dir_d.0, cfg()).unwrap();
    let inline = Pyramid::open(&dir_i.0, cfg()).unwrap();
    assert_eq!(overviews(&reopened, SECS), overviews(&inline, SECS));
}

/// **The seal no longer folds a backlog.** With `seal_lag` held back, the last `seal_lag` rows of
/// a level-0 tile whose clock had moved on waited for its SEAL and went up the whole cascade in
/// one `ingest` — measured at 0.8-1.8 s of a view-lattice seal, under the lock every row push
/// takes. They now fold the frame after the clock leaves them, so no single `ingest` does more
/// than a small multiple of a typical frame's fold work, seal or not. A count, not a clock.
#[test]
fn a_seal_folds_no_backlog_of_held_back_rows() {
    let lag_rows = 6;
    let cfg = PyramidConfig {
        seal_lag: Duration::from_secs(lag_rows),
        checkpoint_interval: None,
        ..cfg()
    };
    let dir = TempDir::new("t901-backlog");
    let mut p = Pyramid::open(&dir.0, cfg).unwrap();
    let mut prev = 0u64;
    let mut per_frame: Vec<u64> = Vec::new();
    let mut seal_frames = 0;
    let mut last_written = 0;
    feed(&mut p, SECS, |p| {
        let now = p.stats().coarse_cells_folded;
        per_frame.push(now - prev);
        prev = now;
        seal_frames += usize::from(p.stats().tiles_written != last_written);
        last_written = p.stats().tiles_written;
    });
    assert!(
        seal_frames >= 3,
        "only {seal_frames} seals: this judges nothing"
    );
    // One level-0 row's fold, and its cascade: the typical frame once the lag has filled.
    let mut sorted = per_frame[(lag_rows as usize + 2)..].to_vec();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2].max(1);
    let max = *sorted.last().unwrap();
    assert!(
        max <= 3 * median,
        "one ingest folded {max} coarse cells against a typical {median}: a seal folded the \
         {lag_rows} held-back rows of the tile it closed in one go (per frame: {per_frame:?})"
    );
}
