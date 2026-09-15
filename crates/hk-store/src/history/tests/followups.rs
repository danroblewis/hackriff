//! T-126 follow-ups: floor for sweep and CSV frames, per-source provenance step state across
//! restarts, cell-precise region overrides, and retention cost with many protected tiles.

use std::fmt::Write as _;

use super::*;

/// L0 1 kHz × 1 s (tile 1 min), L1 2 kHz × 1 min (tile 1 h), L2 4 kHz × 1 h.
/// A 2 s seal lag keeps every segment of a sweep (all stamped at one time) in its tile.
fn hour_cfg() -> PyramidConfig {
    let mut c = cfg(vec![level(1, 60), level(2, 60), level(2, 1)], 16);
    c.seal_lag = Duration::from_secs(2);
    c
}

/// Worst |floor_db − injected| over observed cells free of the test carrier, and their count
/// (a NaN floor counts as infinitely wrong).
fn floor_error(h: &RegionHistory, injected: f32) -> (f32, usize) {
    h.cells
        .iter()
        .filter(|c| c.observed() && c.max_db < injected + 15.0)
        .fold((0f32, 0usize), |(w, n), c| {
            let e = if c.floor_db.is_finite() {
                (c.floor_db - injected).abs()
            } else {
                f32::INFINITY
            };
            (w.max(e), n + 1)
        })
}

#[test]
fn floor_db_available_for_sweep_frames_with_estimated_shape() {
    let dir = TempDir::new("sweepfloor");
    let mut p = Pyramid::open(&dir.0, hour_cfg()).unwrap();
    let mut rng = Rng(4);
    let survey_id = SurveyId::new();
    // An hour of 1 s sweeps: 4 segments × 16 bins of 2 kHz (wider than the 1 kHz cells), 4-look
    // noise at −100 dB/Hz, a steady carrier in one bin.
    let bin_db = -100.0 + 10.0 * 2000f64.log10();
    let mut frames = Vec::new();
    for s in 0..3600i64 {
        for seg in 0..4u32 {
            let lo = 16_000.0 + f64::from(seg) * 32_000.0;
            let power = (0..16)
                .map(|bin| {
                    if seg == 1 && bin == 3 {
                        (bin_db + 30.0) as f32
                    } else {
                        (bin_db + 10.0 * rng.gamma(4).log10()) as f32
                    }
                })
                .collect();
            frames.push(SweepFrame {
                key: FrameKey { survey_id, seq: 0 },
                t: ts(T0 + s * S),
                freq: FreqRange::new(lo, lo + 32_000.0),
                bin_width_hz: 2000.0,
                unit: PowerUnit::Dbfs,
                power,
                provenance_ref: ProvenanceId::new(),
            });
        }
    }
    let mut est = NoiseShapeEstimator::new();
    for f in &frames[..64] {
        est.observe_sweep(f);
    }
    assert_eq!(est.sweeps(), 16);
    let k = est
        .bin_shape()
        .expect("16 sweeps of 64 bins estimate the shape");
    let mut scratch = DbScratch::new();
    for f in &frames {
        let mut input = scratch.sweep_frame(f, S);
        input.noise_shape = NoiseShape::BinShape(k);
        p.ingest(&input).unwrap();
    }
    p.seal_through(ts(T0 + 3600 * S)).unwrap();
    let h2 = query(
        &p,
        (16_000.0, 144_000.0),
        (T0, T0 + 3600 * S),
        Resolution::Level(2),
    );
    let (worst, n) = floor_error(&h2, -100.0);
    eprintln!(
        "T-126 sweep floor: injected k 4, estimated {k:.3}; hour cells {n}, worst |floor − injected| {worst:.2} dB"
    );
    assert!((k / 4.0 - 1.0).abs() < 0.05, "k {k}");
    assert!(n >= 31 && worst <= 0.5, "{n} cells, worst {worst}");
}

#[test]
fn floor_db_available_for_hackrf_sweep_csv_with_estimated_shape() {
    let dir = TempDir::new("csvfloor");
    let mut p = Pyramid::open(&dir.0, hour_cfg()).unwrap();
    let mut rng = Rng(2026);
    // An hour of 1 s sweeps, 4 × 16 bins of 1 kHz, single-look (the worst case) noise at
    // −100 dB/Hz, a −70 dB/Hz carrier.
    let mut csv = String::new();
    for s in 0..3600i64 {
        for seg in 0..4i64 {
            let lo = 16_000 + seg * 16_000;
            write!(
                csv,
                "2026-09-13, 12:{:02}:{:02}.000000, {lo}, {}, 1000.00, 20",
                s / 60,
                s % 60,
                lo + 16_000
            )
            .unwrap();
            for bin in 0..16 {
                let v = if seg == 0 && bin == 5 {
                    -40.0
                } else {
                    -70.0 + 10.0 * rng.gamma(1).log10()
                };
                write!(csv, ", {v:.2}").unwrap();
            }
            csv.push('\n');
        }
    }
    let r = import_sweep_csv(&mut p, csv.as_bytes(), &SweepCsvOptions::default()).unwrap();
    assert_eq!((r.frames_folded, r.revisit_ns), (14_400, S));
    let k = r.bin_shape.expect("shape estimated from the file");
    p.seal_through(ts(T0 + 3600 * S)).unwrap();
    let h2 = query(
        &p,
        (16_000.0, 80_000.0),
        (T0, T0 + 3600 * S),
        Resolution::Level(2),
    );
    let (worst, n) = floor_error(&h2, -100.0);
    eprintln!(
        "T-126 CSV floor: injected k 1, estimated {k:.3}; hour cells {n}, worst |floor − injected| {worst:.2} dB"
    );
    assert!((k - 1.0).abs() < 0.05, "k {k}");
    assert!(n >= 15 && worst <= 0.5, "{n} cells, worst {worst}");
}

fn gain(lna_db: f32) -> GainState {
    GainState {
        lna_db,
        vga_db: 20.0,
        amp_on: false,
    }
}

fn fold_gain(p: &mut Pyramid, source: u64, g: GainState, from_s: i64, to_s: i64) {
    let psd = vec![lin(-100.0); 16];
    for s in from_s..to_s {
        let mut f = frame(T0 + s * S, S, 16_000.0, 1000.0, &psd);
        f.gain = Some(g);
        f.source = source;
        p.ingest(&f).unwrap();
    }
}

fn steps(p: &Pyramid, from_s: i64, to_s: i64) -> Vec<ProvenanceStep> {
    query(
        p,
        (16_000.0, 32_000.0),
        (T0 + from_s * S, T0 + to_s * S),
        Resolution::Level(0),
    )
    .provenance
    .steps
}

#[test]
fn provenance_step_state_survives_a_store_restart() {
    let dir = TempDir::new("stepstate");
    let src = source_key("hackrf:0000000000000000457863dc2b3c");
    let mut p = Pyramid::open(&dir.0, hour_cfg()).unwrap();
    fold_gain(&mut p, src, gain(16.0), 0, 20);
    p.close().unwrap();

    // Same front end after the restart: the state came back from disk, no step.
    let mut p = Pyramid::open(&dir.0, hour_cfg()).unwrap();
    assert_eq!(
        p.source_state(src).map(|s| s.0.gain),
        Some(Some(gain(16.0)))
    );
    fold_gain(&mut p, src, gain(16.0), 20, 40);
    assert!(steps(&p, 0, 40).is_empty());
    p.close().unwrap();

    // A change made while the store was closed is still one step, at the first new frame.
    let mut p = Pyramid::open(&dir.0, hour_cfg()).unwrap();
    fold_gain(&mut p, src, gain(24.0), 40, 50);
    let st = steps(&p, 0, 50);
    assert_eq!(st.len(), 1, "{st:?}");
    assert_eq!(
        (st[0].t, st[0].changed, st[0].from.gain, st[0].to.gain),
        (
            ts(T0 + 40 * S),
            ProvenanceStep::GAIN,
            Some(gain(16.0)),
            Some(gain(24.0))
        )
    );
}

#[test]
fn interleaved_sources_do_not_show_alternating_false_steps() {
    let dir = TempDir::new("twosources");
    let (a, b) = (source_key("hackrf:a"), source_key("rtl-sdr:b"));
    let mut p = Pyramid::open(&dir.0, hour_cfg()).unwrap();
    let psd = vec![lin(-100.0); 16];
    // Two sources at different gains feed one store frame by frame; B changes gain once.
    for s in 0..60 {
        let gb = if s < 30 { gain(0.0) } else { gain(8.0) };
        for (src, g) in [(a, gain(16.0)), (b, gb)] {
            let mut f = frame(T0 + s * S, S, 16_000.0, 1000.0, &psd);
            f.gain = Some(g);
            f.source = src;
            p.ingest(&f).unwrap();
        }
    }
    let st = steps(&p, 0, 60);
    assert_eq!(st.len(), 1, "{st:?}");
    assert_eq!(
        (st[0].t, st[0].from.gain, st[0].to.gain),
        (ts(T0 + 30 * S), Some(gain(0.0)), Some(gain(8.0)))
    );
    p.close().unwrap();

    // Both resume after a restart without a step.
    let mut p = Pyramid::open(&dir.0, hour_cfg()).unwrap();
    for s in 60..70 {
        for (src, g) in [(a, gain(16.0)), (b, gain(8.0))] {
            let mut f = frame(T0 + s * S, S, 16_000.0, 1000.0, &psd);
            f.gain = Some(g);
            f.source = src;
            p.ingest(&f).unwrap();
        }
    }
    assert!(steps(&p, 60, 70).is_empty());
}

#[test]
fn narrow_override_keeps_only_overlapping_cells_past_the_level_age() {
    const DAY: i64 = 86_400;
    let dir = TempDir::new("trim");
    let a = 1_600_000.0;
    let config = || {
        let mut c = cfg(vec![level(1, 60), level(2, 60)], 16);
        c.levels[0].max_age = Some(Duration::from_secs(3600));
        // One 1 kHz cell (1.601–1.602 MHz) of the 16 kHz level-0 block.
        c.retention_overrides = vec![RetentionOverride {
            freq: FreqRange::new(1_601_000.0, 1_602_000.0),
            level: 0,
            max_age: Duration::from_secs(90 * DAY as u64),
        }];
        c
    };
    let mut p = Pyramid::open(&dir.0, config()).unwrap();
    let psd = vec![lin(-100.0); 16];
    for s in (0..7200).step_by(10) {
        p.ingest(&frame(T0 + s * S, S, a, 1000.0, &psd)).unwrap();
    }
    p.seal_through(ts(T0 + 2 * 3600 * S)).unwrap();
    let keys = p.sealed_keys(0);
    assert_eq!(keys.len(), 120, "protected tiles stay as files");
    assert_eq!((p.stats().tiles_trimmed, p.stats().tiles_expired), (60, 0));
    let old = fs::metadata(p.tile_path(keys[0])).unwrap().len();
    let new = fs::metadata(p.tile_path(keys[119])).unwrap().len();
    eprintln!(
        "T-126 trim: first-hour level-0 tile {old} B (1 of 16 cells kept), second-hour {new} B"
    );
    assert!(old < new);

    // First hour at level 0: only the overridden cell remains; the rest reads unobserved.
    let h0 = query(
        &p,
        (a, a + 16_000.0),
        (T0, T0 + 60 * S),
        Resolution::Level(0),
    );
    for t in (0..60).step_by(10) {
        for f in 0..16 {
            let c = h0.cell(t, f);
            assert_eq!(c.observed(), f == 1, "t {t} f {f}: {c:?}");
            assert_eq!(c.level, 0);
        }
    }
    // Second hour untouched; the coarser level keeps the whole first hour.
    let recent = query(
        &p,
        (a, a + 16_000.0),
        (T0 + 3600 * S, T0 + 3660 * S),
        Resolution::Level(0),
    );
    assert!((0..16).all(|f| recent.cell(0, f).observed()));
    let h1 = query(
        &p,
        (a, a + 16_000.0),
        (T0, T0 + 3600 * S),
        Resolution::Level(1),
    );
    assert!(h1.cells.iter().all(|c| c.level == 1 && c.observed()));

    // A restart re-checks the schedule without rewriting trimmed tiles; the override lapse
    // removes them.
    drop(p);
    let mut p = Pyramid::open(&dir.0, config()).unwrap();
    assert_eq!((p.stats().tiles_trimmed, p.sealed_keys(0).len()), (0, 120));
    p.seal_through(ts(T0 + 91 * DAY * S)).unwrap();
    assert!(p.sealed_keys(0).is_empty());
}

#[test]
fn retention_pass_cost_is_flat_in_the_number_of_protected_tiles() {
    // `protected` level-0 tiles past the level age inside a 90-day override (index entries only,
    // no files), then 200 seals each adding and quota-evicting one unprotected tile.
    let run = |protected: i64| -> (u64, u64) {
        let dir = TempDir::new("retcost");
        let mut c = cfg(vec![level(1, 60)], 16);
        c.levels[0].max_age = Some(Duration::from_secs(3600));
        c.levels[0].byte_quota = Some(0);
        c.retention_overrides = vec![RetentionOverride {
            freq: FreqRange::new(1_601_000.0, 1_602_000.0),
            level: 0,
            max_age: Duration::from_secs(90 * 86_400),
        }];
        let mut p = Pyramid::open(&dir.0, c).unwrap();
        let tb0 = T0 / (60 * S);
        for tb in tb0..tb0 + protected {
            p.index_fake_sealed(0, 100, tb, 1000);
        }
        let mut minute = tb0 + protected + 120;
        p.seal_through(ts(minute * 60 * S)).unwrap();
        let first = p.retention_visits;
        p.retention_visits = 0;
        for _ in 0..200 {
            p.index_fake_sealed(0, 200, minute, 1000);
            minute += 1;
            p.seal_through(ts(minute * 60 * S)).unwrap();
        }
        assert_eq!(p.sealed_keys(0).len() as i64, protected);
        assert_eq!(p.stats().tiles_evicted_quota, 200);
        (first, p.retention_visits)
    };
    let (first_small, small) = run(100);
    let (first_big, big) = run(5000);
    eprintln!(
        "T-126 retention cost: 100 protected → {small} visits over 200 seals (first pass {first_small}); \
         5000 protected → {big} visits (first pass {first_big})"
    );
    assert!(
        first_big >= 5000,
        "first pass schedules each protected tile once"
    );
    assert_eq!(
        big, small,
        "steady-state cost independent of protected tiles"
    );
    assert!(small <= 2 * 200);
}
