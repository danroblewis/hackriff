//! Unit tests: frame folding, regridding, rollup, byte budget, crash safety, scheme, provenance.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hk_model::{
    FrameKey, FreqRange, PowerUnit, ProvenanceId, SurveyId, SweepFrame, TimeRange, Timestamp,
};

use super::stats::{db, undb};
use super::*;

const S: i64 = 1_000_000_000;
/// 2026-09-13T12:00:00Z (aligned to the hour).
const T0: i64 = 1_789_300_800 * S;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "hk-store-unit-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Rng(u64);

impl Rng {
    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Mean-1 gamma(k): a k-look averaged periodogram bin.
    fn gamma(&mut self, k: u32) -> f64 {
        (0..k).map(|_| -self.unit().max(1e-12).ln()).sum::<f64>() / f64::from(k)
    }
}

fn level(f_factor: u32, t_cells_per_block: u32) -> LevelConfig {
    LevelConfig {
        f_factor,
        t_cells_per_block,
        max_age: None,
    }
}

/// 1 kHz × 1 s level-0 cells, `nf` cells per tile, no lag, no checkpoints, no budget.
fn cfg(levels: Vec<LevelConfig>, nf: u32) -> PyramidConfig {
    PyramidConfig {
        scheme: 7,
        f_cell_hz: 1000.0,
        t_cell: Duration::from_secs(1),
        f_cells_per_block: nf,
        levels,
        histogram: HistogramConfig {
            lo_db: -130.0,
            step_db: 0.5,
            bins: 240,
        },
        seal_lag: Duration::ZERO,
        checkpoint_interval: None,
        byte_budget: u64::MAX,
        ..PyramidConfig::default()
    }
}

fn ts(ns: i64) -> Timestamp {
    Timestamp::from_unix_nanos(ns)
}

fn lin(v: f32) -> f32 {
    undb(v) as f32
}

fn frame(t: i64, dur: i64, f_lo: f64, bw: f64, psd: &[f32]) -> FrameInput<'_> {
    FrameInput::new(ts(t), dur, f_lo, bw, PowerUnit::Dbfs, psd)
}

fn query(p: &Pyramid, f: (f64, f64), t: (i64, i64), resolution: Resolution) -> RegionHistory {
    p.query(&RegionQuery {
        freq: FreqRange::new(f.0, f.1),
        time: TimeRange::new(ts(t.0), ts(t.1)),
        resolution,
    })
    .unwrap()
}

#[test]
fn frame_folding_exact_statistics() {
    // One 1-s cell gets 10 frames with values −99.75 … −90.75 dB; the rest of the block −100 dB.
    for caller_floor in [true, false] {
        let dir = TempDir::new("fold");
        let mut p = Pyramid::open(&dir.0, cfg(vec![level(1, 10)], 16)).unwrap();
        let floor = vec![-100f32; 16];
        let mut vals = Vec::new();
        for k in 0..10 {
            let mut psd = vec![lin(-100.0); 16];
            let v = -99.75 + k as f32;
            psd[3] = lin(v);
            vals.push(v);
            let mut f = frame(T0 + k * S / 10, S / 10, 16_000.0, 1000.0, &psd);
            if caller_floor {
                f.floor_db = Some(&floor);
            }
            assert_eq!(p.ingest(&f).unwrap(), IngestOutcome::Folded);
        }
        p.seal_through(ts(T0 + 10 * S)).unwrap();
        let h = query(&p, (16_000.0, 32_000.0), (T0, T0 + S), Resolution::Level(0));
        assert_eq!((h.nt, h.nf, h.level), (1, 16, 0));
        let c = h.cell(0, 3);
        assert_eq!(c.frames, 10);
        assert_eq!(c.max_db, -90.75);
        let mean = db(vals.iter().map(|&v| undb(v)).sum::<f64>() / 10.0);
        assert!((c.mean_db - mean).abs() <= 0.006, "{} vs {mean}", c.mean_db);
        // numpy-style p10 = v0 + 0.9 · 1 dB, p90 = v0 + 8.1 dB.
        assert!((c.p_low_db - -98.85).abs() < 0.006, "{}", c.p_low_db);
        assert!((c.p_high_db - -91.65).abs() < 0.006, "{}", c.p_high_db);
        // Threshold −100 + 6 = −94 dB: values −93.75 … −90.75 (4 of 10) are occupied.
        assert!((c.occupancy - 0.4).abs() < 1e-4, "{}", c.occupancy);
        assert!((c.occupancy_max - 0.4).abs() < 1e-4);
        assert_eq!(c.coverage, 1.0);
        let quiet = h.cell(0, 0);
        assert_eq!(
            (quiet.occupancy, quiet.p_low_db, quiet.max_db),
            (0.0, -100.0, -100.0)
        );
        // A time cell with no frames is not observed (not "quiet").
        let h2 = query(
            &p,
            (16_000.0, 32_000.0),
            (T0, T0 + 2 * S),
            Resolution::Level(0),
        );
        assert!(!h2.cell(1, 3).observed());
        assert_eq!(h2.cell(1, 3).level, 0);
    }
}

#[test]
fn canonical_grid_resampling_preserves_tone_max() {
    for (bw, f_lo) in [
        (700.0, 16_150.0),
        (3000.0, 16_000.0),
        (1000.0, 16_500.0),
        (125.0, 16_020.0),
    ] {
        let dir = TempDir::new("regrid");
        let mut p = Pyramid::open(&dir.0, cfg(vec![level(1, 10)], 16)).unwrap();
        let n = (15_000.0 / bw) as usize;
        let tone_bin = n / 2;
        let mut psd = vec![lin(-110.0); n];
        psd[tone_bin] = lin(-60.0);
        for k in 0..5 {
            p.ingest(&frame(T0 + k * S / 5, S / 5, f_lo, bw, &psd))
                .unwrap();
        }
        p.seal_through(ts(T0 + 10 * S)).unwrap();
        let h = query(&p, (16_000.0, 32_000.0), (T0, T0 + S), Resolution::Level(0));
        let tone_f = f_lo + (tone_bin as f64 + 0.5) * bw;
        let col = ((tone_f / 1000.0).floor() as i64 - h.f_first_cell) as usize;
        assert_eq!(
            h.cell(0, col).max_db,
            -60.0,
            "bw {bw}: tone max at its cell"
        );
        let region_max = h
            .row(0)
            .iter()
            .filter(|c| c.observed())
            .map(|c| c.max_db)
            .fold(f32::NEG_INFINITY, f32::max);
        assert_eq!(region_max, -60.0);
        // Power is conserved: Σ mean density × cell width over the cells fully inside the frame
        // equals Σ psd × bw over the same span (only the tone and floor contribute).
        let (a, b) = (f_lo, f_lo + n as f64 * bw);
        let inside: Vec<usize> = (0..h.nf)
            .filter(|&f| {
                let r = h.freq_of(f);
                r.lo_hz >= a && r.hi_hz <= b
            })
            .collect();
        let tone_inside = inside.iter().any(|&f| {
            let r = h.freq_of(f);
            tone_f - bw / 2.0 >= r.lo_hz && tone_f + bw / 2.0 <= r.hi_hz
        });
        if tone_inside {
            let got: f64 = inside
                .iter()
                .map(|&f| undb(h.cell(0, f).mean_db) * 1000.0)
                .sum();
            let span = inside.len() as f64 * 1000.0;
            let want = undb(-110.0) * (span - bw) + undb(-60.0) * bw;
            assert!(
                (db(got) - db(want)).abs() < 0.02,
                "bw {bw}: {got} vs {want}"
            );
        }
    }
}

#[test]
fn rollup_preserves_max_conserves_power_and_merges_percentiles() {
    // L0: 1 kHz × 1 s, 10-s tiles; L1: 2 kHz × 10 s, 1-min tiles; L2: 4 kHz × 1 min, 10-min tiles.
    let dir = TempDir::new("rollup");
    let mut p = Pyramid::open(
        &dir.0,
        cfg(vec![level(1, 10), level(2, 6), level(2, 10)], 16),
    )
    .unwrap();
    let mut rng = Rng(42);
    let mut on = [false; 16];
    let mut pooled1: HashMap<(i64, i64), Vec<f32>> = HashMap::new();
    let mut pooled2: HashMap<(i64, i64), Vec<f32>> = HashMap::new();
    let mut psd = vec![0f32; 16];
    for k in 0..6000i64 {
        let t = T0 + k * S / 10;
        for (b, v) in psd.iter_mut().enumerate() {
            if on[b] {
                on[b] = rng.unit() > 0.02;
            } else {
                on[b] = rng.unit() < 0.002;
            }
            let level = if on[b] { -75.0 } else { -100.0 };
            *v = (undb(level) * rng.gamma(4)) as f32;
            let vdb = db(f64::from(*v));
            let mid_s = (t + S / 20).div_euclid(S);
            let fc = 16 + b as i64;
            pooled1
                .entry((mid_s.div_euclid(10), fc / 2))
                .or_default()
                .push(vdb);
            pooled2
                .entry((mid_s.div_euclid(60), fc / 4))
                .or_default()
                .push(vdb);
        }
        p.ingest(&frame(t, S / 10, 16_000.0, 1000.0, &psd)).unwrap();
    }
    p.seal_through(ts(T0 + 600 * S)).unwrap();
    assert!(!p.sealed_keys(2).is_empty(), "the 10-min L2 tile sealed");
    let span = (T0, T0 + 600 * S);
    let f = (16_000.0, 32_000.0);
    let h0 = query(&p, f, span, Resolution::Level(0));
    let h1 = query(&p, f, span, Resolution::Level(1));
    let h2 = query(&p, f, span, Resolution::Level(2));
    assert_eq!(
        (h0.nt, h0.nf, h1.nt, h1.nf, h2.nt, h2.nf),
        (600, 16, 60, 8, 10, 4)
    );

    let check = |fine: &RegionHistory,
                 coarse: &RegionHistory,
                 tf: usize,
                 pooled: &HashMap<(i64, i64), Vec<f32>>| {
        let mut max_step = 0f32;
        for tc in 0..coarse.nt {
            for fc in 0..coarse.nf {
                let c = coarse.cell(tc, fc);
                assert!(c.observed());
                let (mut max, mut frames, mut lin_sum, mut occ_max) =
                    (f32::NEG_INFINITY, 0u64, 0.0, 0f32);
                let mut best_occ = 0f64;
                for ff in 2 * fc..2 * fc + 2 {
                    let (mut e, mut o) = (0.0f64, 0.0f64);
                    for tfine in tc * tf..(tc + 1) * tf {
                        let x = fine.cell(tfine, ff);
                        max = max.max(x.max_db);
                        frames += u64::from(x.frames);
                        lin_sum += undb(x.mean_db) * f64::from(x.frames);
                        occ_max = occ_max.max(x.occupancy_max);
                        e += f64::from(x.coverage);
                        o += f64::from(x.occupancy) * f64::from(x.coverage);
                    }
                    best_occ = best_occ.max(o / e);
                }
                assert_eq!(c.max_db, max, "max-of-max is exact");
                assert_eq!(u64::from(c.frames), frames);
                let mean = db(lin_sum / frames as f64);
                assert!(
                    (c.mean_db - mean).abs() <= 0.02,
                    "power mean {} vs {mean}",
                    c.mean_db
                );
                assert!(
                    (f64::from(c.occupancy) - best_occ).abs() < 5e-4,
                    "occupancy {} vs {best_occ}",
                    c.occupancy
                );
                assert_eq!(c.occupancy_max, occ_max);
                let key = (
                    coarse.t_first_cell + tc as i64,
                    coarse.f_first_cell + fc as i64,
                );
                let mut vals = pooled[&key].clone();
                vals.sort_by(f32::total_cmp);
                for (q, got) in [(10.0f32, c.p_low_db), (90.0, c.p_high_db)] {
                    // The stated bound: within one step of the pooled order statistic x_(⌈q·n⌉).
                    let rank = ((f64::from(q) / 100.0 * vals.len() as f64).ceil() as usize)
                        .clamp(1, vals.len());
                    let order_stat = vals[rank - 1];
                    let err = (got - order_stat).abs();
                    max_step = max_step.max(err);
                    assert!(
                        err <= 0.51,
                        "p{q}: merged {got} vs pooled x_({rank}) {order_stat}"
                    );
                }
            }
        }
        max_step
    };
    let e1 = check(&h0, &h1, 10, &pooled1);
    let e2 = check(&h1, &h2, 6, &pooled2);
    eprintln!("percentile merge error: L1 {e1:.3} dB, L2 {e2:.3} dB (bound 0.5 dB)");
}

#[test]
fn one_second_burst_in_an_hour_survives_rollup() {
    // L0 1 s (1-min tiles) → L1 1 min (1-h tiles) → L2 1 h.
    let dir = TempDir::new("burst");
    let mut p = Pyramid::open(
        &dir.0,
        cfg(vec![level(1, 60), level(2, 60), level(2, 1)], 16),
    )
    .unwrap();
    let mut rng = Rng(7);
    let mut psd = vec![0f32; 16];
    for k in 0..36_000i64 {
        let t = T0 + k * S / 10;
        for v in psd.iter_mut() {
            // 32 looks: noise false alarms at p10 + 6 dB are negligible, so the mean occupancy
            // reflects the burst alone.
            *v = (undb(-100.0) * rng.gamma(32)) as f32;
        }
        if (12_340..12_350).contains(&k) {
            psd[5] = lin(-70.0);
        }
        p.ingest(&frame(t, S / 10, 16_000.0, 1000.0, &psd)).unwrap();
    }
    p.seal_through(ts(T0 + 3600 * S)).unwrap();
    let f = (16_000.0, 32_000.0);
    let h2 = query(&p, f, (T0, T0 + 3600 * S), Resolution::Level(2));
    assert_eq!((h2.nt, h2.nf), (1, 4));
    let burst = h2.cell(0, 1); // 20–24 kHz holds 21 kHz (bin 5)
    assert!(
        burst.max_db >= -70.5,
        "coarse max shows the burst: {}",
        burst.max_db
    );
    assert!(burst.max_db - burst.p_low_db > 25.0);
    assert!(
        burst.occupancy_max >= 0.99,
        "max-occupancy keeps it: {}",
        burst.occupancy_max
    );
    assert!(
        burst.occupancy < 0.001,
        "mean occupancy is ~1/3600: {}",
        burst.occupancy
    );
    for fc in [0, 2, 3] {
        let quiet = h2.cell(0, fc);
        assert!(
            quiet.max_db < -80.0 && quiet.occupancy_max <= 0.2,
            "{quiet:?}"
        );
    }
    let h1 = query(&p, f, (T0, T0 + 3600 * S), Resolution::Level(1));
    let minute = h1.cell(20, 2); // 1234 s → minute 20; 20–22 kHz
    assert!(
        (minute.occupancy - 1.0 / 60.0).abs() < 0.005,
        "{}",
        minute.occupancy
    );
    assert!(minute.occupancy_max >= 0.99);
    assert_eq!(h1.cell(20, 2).max_db, burst.max_db);
}

fn noise_frames(p: &mut Pyramid, rng: &mut Rng, from_s: i64, to_s: i64, nbins: usize, f_lo: f64) {
    let mut psd = vec![0f32; nbins];
    for s in from_s..to_s {
        for v in psd.iter_mut() {
            *v = (undb(-100.0) * rng.gamma(8)) as f32;
        }
        p.ingest(&frame(T0 + s * S, S, f_lo, 1000.0, &psd)).unwrap();
    }
}

#[test]
fn byte_budget_respected_with_coarse_coverage_kept() {
    // L0 1 s (1-min tiles) → L1 1 min (1-h tiles) → L2 1 h (1-day tiles); 64 cells per tile.
    let dir = TempDir::new("budget");
    let levels = || vec![level(1, 60), level(2, 60), level(2, 24)];
    let mut rng = Rng(99);
    // First hour unbudgeted, to measure tile sizes.
    let mut p = Pyramid::open(&dir.0, cfg(levels(), 64)).unwrap();
    noise_frames(&mut p, &mut rng, 0, 3600, 64, 64_000.0);
    p.seal_through(ts(T0 + 3600 * S)).unwrap();
    let l0_tile = p.level_bytes(0) / p.sealed_keys(0).len() as u64;
    let l1_tile = p.level_bytes(1) / p.sealed_keys(1).len() as u64;
    p.close().unwrap();
    // Room for 70 L0 tiles (more than one uncovered hour) plus 40 L1 tiles.
    let budget = 70 * l0_tile + 40 * l1_tile;
    let mut c = cfg(levels(), 64);
    c.byte_budget = budget;
    let mut p = Pyramid::open(&dir.0, c).unwrap();
    assert!(p.disk_bytes() <= budget);
    let hours = 30;
    for h in 1..hours {
        noise_frames(&mut p, &mut rng, h * 3600, (h + 1) * 3600, 64, 64_000.0);
        assert!(
            p.disk_bytes() <= budget,
            "hour {h}: {} > {budget}",
            p.disk_bytes()
        );
        assert!(!p.stats().over_budget, "hour {h}");
    }
    let end = T0 + hours * 3600 * S;
    p.seal_through(ts(end)).unwrap();
    assert!(p.disk_bytes() <= budget);
    assert!(p.stats().tiles_evicted > 1000, "{:?}", p.stats());
    let oldest_l0 = p.sealed_keys(0)[0].t_block * 60 * S;
    assert!(oldest_l0 >= end - 3 * 3600 * S, "old level-0 tiles evicted");
    assert_eq!(
        p.sealed_keys(1).len() as i64,
        hours,
        "all hourly L1 tiles kept"
    );
    // Every minute of every hour still has coverage at a coarse level.
    let f = (64_000.0, 128_000.0);
    let h1 = query(&p, f, (T0, end), Resolution::Level(1));
    for t in 0..h1.nt {
        assert!(
            h1.row(t).iter().any(|c| c.observed() && c.level == 1),
            "minute {t}"
        );
    }
    // A level-0 query over the first hour falls back to level 1.
    let h0 = query(&p, f, (T0, T0 + 3600 * S), Resolution::Level(0));
    assert!(h0.cells.iter().all(|c| c.observed() && c.level == 1));
    // Recent level 0 is still fine-grained.
    let recent = query(&p, f, (end - 60 * S, end), Resolution::Level(0));
    assert!(recent.cells.iter().all(|c| c.observed() && c.level == 0));
}

#[test]
fn byte_budget_never_evicts_uncovered_tiles() {
    // Three levels: L1 hour tiles are covered only once the (open) L2 day tile seals.
    let dir = TempDir::new("budget-tiny");
    let mut c = cfg(vec![level(1, 60), level(2, 60), level(2, 24)], 16);
    c.byte_budget = 1024;
    let mut p = Pyramid::open(&dir.0, c).unwrap();
    let mut rng = Rng(3);
    noise_frames(&mut p, &mut rng, 0, 5400, 16, 16_000.0);
    assert!(p.stats().over_budget);
    // Hour 0's level-0 tiles are covered by the sealed L1 tile and go; the 30 minutes after it
    // are level-0-only and are kept despite the budget, as is the uncovered L1 tile.
    assert_eq!(p.sealed_keys(0).len(), 30);
    assert_eq!(p.sealed_keys(1).len(), 1);
    let h = query(
        &p,
        (16_000.0, 32_000.0),
        (T0, T0 + 5400 * S),
        Resolution::Level(0),
    );
    for t in 0..h.nt {
        let want_level = if t < 3600 { 1 } else { 0 };
        assert!(
            h.row(t)
                .iter()
                .all(|c| c.observed() && c.level == want_level),
            "row {t}"
        );
    }

    // With two levels, L1 is the top: nothing is coarser, so its oldest tile expires by quota.
    let dir = TempDir::new("budget-top");
    let mut c = cfg(vec![level(1, 60), level(2, 60)], 16);
    c.byte_budget = 1024;
    let mut p = Pyramid::open(&dir.0, c).unwrap();
    noise_frames(&mut p, &mut rng, 0, 5400, 16, 16_000.0);
    assert!(p.stats().over_budget);
    assert_eq!(p.sealed_keys(0).len(), 30, "uncovered level 0 still kept");
    assert!(p.sealed_keys(1).is_empty(), "top-level tile expired");
}

#[test]
fn crash_safety_ignores_partial_tiles_and_restores_checkpoints() {
    let dir = TempDir::new("crash");
    let c = || cfg(vec![level(1, 10), level(2, 6)], 16);
    let mut p = Pyramid::open(&dir.0, c()).unwrap();
    let psd = vec![lin(-100.0); 16];
    for k in 0..350 {
        p.ingest(&frame(T0 + k * S / 10, S / 10, 16_000.0, 1000.0, &psd))
            .unwrap();
    }
    p.checkpoint().unwrap();
    for k in 360..380 {
        p.ingest(&frame(T0 + k * S / 10, S / 10, 16_000.0, 1000.0, &psd))
            .unwrap();
    }
    let tb = T0 / (10 * S);
    let key = |t_block| hk_model::TileKey {
        scheme: 7,
        level: 0,
        f_block: 1,
        t_block,
    };
    let good = p.tile_path(key(tb));
    let corrupt = p.tile_path(key(tb + 1));
    let partial = p.tile_path(key(tb + 100));
    let tmp = good.with_file_name(format!("t{}.tile.tmp999", tb + 101));
    assert_eq!(p.sealed_keys(0).len(), 3);
    drop(p); // crash: frames after the checkpoint are lost
    let bytes = fs::read(&good).unwrap();
    fs::write(&partial, &bytes[..bytes.len() / 2]).unwrap();
    fs::write(&tmp, b"torn").unwrap();
    let mut flipped = fs::read(&corrupt).unwrap();
    *flipped.last_mut().unwrap() ^= 0xff;
    fs::write(&corrupt, flipped).unwrap();

    let mut p = Pyramid::open(&dir.0, c()).unwrap();
    assert!(
        !partial.exists() && !tmp.exists(),
        "partial and temp files removed"
    );
    assert!(p.stats().files_ignored >= 3, "{:?}", p.stats());
    assert!(p.open_keys(0).contains(&key(tb + 3)), "checkpoint restored");
    let h = query(
        &p,
        (16_000.0, 32_000.0),
        (T0, T0 + 40 * S),
        Resolution::Level(0),
    );
    for t in 0..10 {
        assert!(
            h.row(t).iter().all(|c| c.observed() && c.level == 0),
            "row {t}"
        );
    }
    for t in 10..20 {
        assert!(
            h.row(t).iter().all(|c| !(c.observed() && c.level == 0)),
            "corrupt tile ignored"
        );
    }
    for t in 30..35 {
        assert!(
            h.row(t).iter().all(|c| c.observed() && c.frames == 10),
            "checkpointed row {t}"
        );
    }
    for t in 36..38 {
        assert!(
            h.row(t).iter().all(|c| !c.observed()),
            "post-checkpoint row {t} lost"
        );
    }
    assert_eq!(
        p.ingest(&frame(T0 + 36 * S, S / 10, 16_000.0, 1000.0, &psd))
            .unwrap(),
        IngestOutcome::Folded
    );
    assert_eq!(
        p.ingest(&frame(T0 + 5 * S, S / 10, 16_000.0, 1000.0, &psd))
            .unwrap(),
        IngestOutcome::Late
    );
}

#[test]
fn scheme_id_is_honoured() {
    let dir = TempDir::new("scheme");
    let mut p = Pyramid::open(&dir.0, cfg(vec![level(1, 10)], 16)).unwrap();
    let psd = vec![lin(-100.0); 16];
    p.ingest(&frame(T0, S, 16_000.0, 1000.0, &psd)).unwrap();
    p.seal_through(ts(T0 + 10 * S)).unwrap();
    assert_eq!(p.sealed_keys(0)[0].scheme, 7);
    drop(p);
    let mut changed = cfg(vec![level(1, 10)], 16);
    changed.f_cell_hz = 2000.0;
    assert!(matches!(
        Pyramid::open(&dir.0, changed.clone()),
        Err(StoreError::SchemeMismatch { .. })
    ));
    changed.scheme = 8;
    let p = Pyramid::open(&dir.0, changed).unwrap();
    assert!(
        p.sealed_keys(0).is_empty(),
        "a new scheme id starts a separate pyramid"
    );
}

#[test]
fn provenance_summary_records_gain_steps_and_suspect_frames() {
    let dir = TempDir::new("prov");
    let mut p = Pyramid::open(&dir.0, cfg(vec![level(1, 10), level(2, 6)], 16)).unwrap();
    let psd = vec![lin(-100.0); 16];
    let cal = hk_model::CalibrationStateId::new();
    let (a, b) = (
        GainState {
            lna_db: 16.0,
            vga_db: 20.0,
            amp_on: false,
        },
        GainState {
            lna_db: 32.0,
            vga_db: 20.0,
            amp_on: false,
        },
    );
    for k in 0..100 {
        let mut f = frame(T0 + k * S / 10, S / 10, 16_000.0, 1000.0, &psd);
        f.gain = Some(if k < 50 { a } else { b });
        f.suspect = (60..70).contains(&k);
        f.dropped_samples = if k == 80 { 100 } else { 0 };
        f.calibration = Some(cal);
        p.ingest(&f).unwrap();
    }
    p.seal_through(ts(T0 + 60 * S)).unwrap();
    let check = |p: &Pyramid| {
        let h = query(
            p,
            (16_000.0, 32_000.0),
            (T0, T0 + 60 * S),
            Resolution::Level(1),
        );
        let pr = &h.provenance;
        assert_eq!(pr.frames, 100);
        assert_eq!(pr.gain_states, vec![(a, 50), (b, 50)]);
        assert_eq!(pr.gain_changes, 1);
        assert!((pr.suspect_fraction() - 0.1).abs() < 1e-12);
        assert_eq!(pr.dropped_samples, 100);
        assert_eq!(pr.calibration, Some(cal));
        assert!(!pr.calibration_mixed);
        assert_eq!(pr.first_frame, Some(ts(T0)));
    };
    check(&p);
    drop(p);
    check(&Pyramid::open(&dir.0, cfg(vec![level(1, 10), level(2, 6)], 16)).unwrap());
}

#[test]
fn open_column_is_visible_to_queries() {
    let dir = TempDir::new("preview");
    let mut p = Pyramid::open(&dir.0, cfg(vec![level(1, 10)], 16)).unwrap();
    let mut psd = vec![lin(-100.0); 16];
    for k in 0..5 {
        psd[2] = lin(if k < 2 { -80.0 } else { -100.0 });
        p.ingest(&frame(T0 + k * S / 10, S / 10, 16_000.0, 1000.0, &psd))
            .unwrap();
    }
    let h = query(&p, (16_000.0, 32_000.0), (T0, T0 + S), Resolution::Level(0));
    let c = h.cell(0, 2);
    assert_eq!((c.frames, c.max_db, c.p_low_db), (5, -80.0, -100.0));
    assert!((c.occupancy - 0.4).abs() < 1e-4, "{}", c.occupancy);
    assert_eq!(p.open_keys(0).len(), 1);
    assert!(p.sealed_keys(0).is_empty());
}

#[test]
fn sweep_frame_converter_regrids_density() {
    let dir = TempDir::new("sweep");
    let mut c = cfg(vec![level(1, 10)], 16);
    c.f_cell_hz = 25_000.0;
    let mut p = Pyramid::open(&dir.0, c).unwrap();
    let sweep = SweepFrame {
        key: FrameKey {
            survey_id: SurveyId::new(),
            seq: 0,
        },
        t: ts(T0),
        freq: FreqRange::new(400e6, 400.4e6),
        bin_width_hz: 100_000.0,
        unit: PowerUnit::Dbfs,
        power: vec![-50.0, -50.0, -30.0, -50.0],
        provenance_ref: ProvenanceId::new(),
    };
    let mut scratch = DbScratch::new();
    p.ingest(&scratch.sweep_frame(&sweep, 750_000_000)).unwrap();
    let h = query(&p, (400e6, 400.4e6), (T0, T0 + S), Resolution::Level(0));
    assert_eq!(h.nf, 16);
    // −50 dB per 100 kHz bin = −100 dB/Hz; the −30 dB bin spreads over cells 8..12.
    for f in 0..16 {
        let want = if (8..12).contains(&f) { -80.0 } else { -100.0 };
        assert_eq!(h.cell(0, f).mean_db, want, "cell {f}");
        assert!((h.cell(0, f).coverage - 0.75).abs() < 1e-4);
    }
}
