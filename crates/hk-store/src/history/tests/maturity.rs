//! T-116 history maturity: coverage mask, provenance steps, zstd tiles, tiered retention, burst
//! survival with the bias-corrected floor, tier consistency, hackrf_sweep CSV and PNG export.

use super::super::codec;
use super::*;

fn key_of(c: &CellStats) -> (u32, [i32; 7]) {
    let q = |v: f32| {
        if v.is_finite() {
            (v * 100.0).round() as i32
        } else {
            i32::MIN
        }
    };
    (
        c.frames,
        [
            q(c.max_db),
            q(c.mean_db),
            q(c.p_low_db),
            q(c.p_high_db),
            q(c.occupancy * 1e4),
            q(c.occupancy_max * 1e4),
            q(c.coverage * 1e4),
        ],
    )
}

#[test]
fn coverage_mask_marks_injected_gaps_at_every_tier() {
    let dir = TempDir::new("gap");
    let mut p = Pyramid::open(
        &dir.0,
        cfg(vec![level(1, 60), level(2, 60), level(2, 1)], 16),
    )
    .unwrap();
    let mut rng = Rng(11);
    let mut psd = [0f32; 16];
    // 1 h of 1-s frames over 16–32 kHz: nothing in minutes 20–25; minutes 40–45 sweep only
    // 20–32 kHz (a partial band).
    for s in 0..3600i64 {
        if (1200..1500).contains(&s) {
            continue;
        }
        for v in psd.iter_mut() {
            *v = (undb(-100.0) * rng.gamma(8)) as f32;
        }
        let (f_lo, bins) = if (2400..2700).contains(&s) {
            (20_000.0, &psd[4..])
        } else {
            (16_000.0, &psd[..])
        };
        p.ingest(&frame(T0 + s * S, S, f_lo, 1000.0, bins)).unwrap();
    }
    p.seal_through(ts(T0 + 3600 * S)).unwrap();
    let f = (16_000.0, 32_000.0);
    let hour = (T0, T0 + 3600 * S);

    let h0 = query(&p, f, hour, Resolution::Level(0));
    assert!((0..16).all(|c| !h0.cell(1300, c).observed() && h0.cell(1300, c).max_db.is_nan()));
    assert!((0..4).all(|c| !h0.cell(2500, c).observed()));
    assert!((4..16).all(|c| h0.cell(2500, c).coverage == 1.0));
    let s0 = h0.coverage_summary();
    assert_eq!(
        s0.gaps,
        vec![TimeRange::new(ts(T0 + 1200 * S), ts(T0 + 1500 * S))]
    );
    assert_eq!(s0.observed_cells, 3300 * 16 - 300 * 4);

    let h1 = query(&p, f, hour, Resolution::Level(1));
    assert_eq!((h1.nt, h1.nf), (60, 8));
    assert!((20..25).all(|t| (0..8).all(|c| !h1.cell(t, c).observed())));
    assert!((40..45).all(|t| (0..2).all(|c| !h1.cell(t, c).observed())));
    assert!((40..45).all(|t| (2..8).all(|c| h1.cell(t, c).coverage == 1.0)));
    assert_eq!(
        h1.coverage_summary().gaps,
        vec![TimeRange::new(ts(T0 + 1200 * S), ts(T0 + 1500 * S))]
    );

    let h2 = query(&p, f, hour, Resolution::Level(2));
    assert_eq!((h2.nt, h2.nf), (1, 4));
    let (low, rest) = (h2.cell(0, 0).coverage, h2.cell(0, 1).coverage);
    assert!((low - 50.0 / 60.0).abs() < 1e-3, "16–20 kHz: {low}");
    assert!((rest - 55.0 / 60.0).abs() < 1e-3, "20–24 kHz: {rest}");
    assert!(h2.coverage_summary().gaps.is_empty());
}

#[test]
fn injected_gain_step_and_front_end_changes_are_visible_in_query_provenance() {
    let dir = TempDir::new("step");
    let levels = || vec![level(1, 60), level(2, 60), level(2, 1)];
    let mut p = Pyramid::open(&dir.0, cfg(levels(), 16)).unwrap();
    let psd = vec![lin(-100.0); 16];
    let a = GainState {
        lna_db: 16.0,
        vga_db: 20.0,
        amp_on: false,
    };
    let b = GainState { lna_db: 32.0, ..a };
    let mask = hk_model::SpurMaskId::new();
    for s in 0..3600i64 {
        let mut f = frame(T0 + s * S, S, 16_000.0, 1000.0, &psd);
        // The gain step lands exactly on a level-0 tile boundary (minute 30).
        f.gain = Some(if s < 1800 { a } else { b });
        f.front_end.filter = Some(PortTag::new("fm-notch"));
        f.front_end.gain_table = Some(3);
        f.front_end.spur_mask = (s >= 2000).then_some(mask);
        p.ingest(&f).unwrap();
    }
    p.seal_through(ts(T0 + 3600 * S)).unwrap();
    let check = |p: &Pyramid| {
        let f = (16_000.0, 32_000.0);
        let h = query(p, f, (T0, T0 + 3600 * S), Resolution::Level(2));
        let pr = &h.provenance;
        assert_eq!(h.scheme, 7);
        assert_eq!(pr.steps.len(), 2, "{:?}", pr.steps);
        let g = &pr.steps[0];
        assert_eq!((g.t, g.changed), (ts(T0 + 1800 * S), ProvenanceStep::GAIN));
        assert_eq!((g.from.gain, g.to.gain), (Some(a), Some(b)));
        assert_eq!(g.change_names(), vec!["gain"]);
        let m = &pr.steps[1];
        assert_eq!(
            (m.t, m.changed),
            (ts(T0 + 2000 * S), ProvenanceStep::SPUR_MASK)
        );
        assert_eq!(
            (m.from.front_end.spur_mask, m.to.front_end.spur_mask),
            (None, Some(mask))
        );
        assert!(pr.spur_mask_mixed && !pr.filter_mixed && !pr.gain_table_mixed);
        assert_eq!(
            pr.filter.map(|t| t.to_string()).as_deref(),
            Some("fm-notch")
        );
        assert_eq!(pr.gain_table, Some(3));
        assert_eq!(pr.gain_changes, 1);
        assert_eq!(pr.gain_states, vec![(a, 1800), (b, 1800)]);
        let before = query(p, f, (T0 + 1740 * S, T0 + 1800 * S), Resolution::Level(0));
        assert!(before.provenance.steps.is_empty());
        let after = query(p, f, (T0 + 1800 * S, T0 + 1860 * S), Resolution::Level(0));
        assert_eq!(after.provenance.steps, vec![*g]);
    };
    check(&p);
    drop(p);
    check(&Pyramid::open(&dir.0, cfg(levels(), 16)).unwrap());
}

#[test]
fn zstd_tiles_compress_a_dwell_stream_and_v1_tiles_stay_readable() {
    let dir = TempDir::new("zstd");
    let config = || PyramidConfig {
        scheme: 7,
        seal_lag: Duration::ZERO,
        checkpoint_interval: None,
        byte_budget: u64::MAX,
        ..PyramidConfig::default()
    };
    let mut p = Pyramid::open(&dir.0, config()).unwrap();
    // A 20 MHz dwell: 4096 bins, 10 frames/s for 2 min; 8-look noise at −120 dBFS/Hz with a slow
    // ripple, three carriers, a bursty emitter, and a 10 s dropout.
    let (bins, fs, f_c) = (4096usize, 20e6, 433.92e6);
    let (f_lo, bw) = (f_c - fs / 2.0, fs / bins as f64);
    let mut rng = Rng(5);
    let mut psd = vec![0f32; bins];
    for k in 0..1200i64 {
        if (600..700).contains(&k) {
            continue;
        }
        for (i, v) in psd.iter_mut().enumerate() {
            let ripple = 3.0 * (i as f64 / bins as f64 * 6.0).sin();
            *v = (undb(-120.0 + ripple as f32) * rng.gamma(8)) as f32;
        }
        for c in [700usize, 2100, 3500] {
            psd[c] = lin(-80.0);
        }
        if k % 50 < 5 {
            for v in &mut psd[1200..1260] {
                *v = lin(-90.0);
            }
        }
        p.ingest(&frame(T0 + k * S / 10, S / 10, f_lo, bw, &psd))
            .unwrap();
    }
    p.seal_through(ts(T0 + 240 * S)).unwrap();
    let st = p.stats().clone();
    let ratio = st.raw_bytes_written as f64 / st.bytes_written as f64;
    let l0_mb_h = p.level_bytes(0) as f64 / 2.0 * 60.0 / 1e6;
    eprintln!(
        "T-116 zstd: {} tiles, raw {} B -> {} B, ratio {ratio:.2}; L0 {:.1} MB/h at 20 MHz",
        st.tiles_written, st.raw_bytes_written, st.bytes_written, l0_mb_h
    );
    assert!(ratio > 1.5, "compression ratio {ratio}");

    let region = (f_lo, f_lo + fs);
    let span = (T0, T0 + 120 * S);
    let before: Vec<_> = query(&p, region, span, Resolution::Level(0))
        .cells
        .iter()
        .map(key_of)
        .collect();
    // Rewrite every level-0 tile in format 1 (T-017); the store must read them unchanged.
    let g0 = p.geometry().levels[0];
    let hist = p.config().histogram;
    for key in p.sealed_keys(0) {
        let path = p.tile_path(key);
        let tile = codec::decode(&path, &g0, usize::from(hist.bins))
            .unwrap()
            .unwrap();
        let mut buf = Vec::new();
        codec::encode_v1(
            &tile,
            true,
            PowerUnit::Dbfs,
            &g0,
            &hist,
            (10.0, 90.0),
            &mut buf,
        );
        assert_eq!(u16::from_le_bytes([buf[8], buf[9]]), 1);
        fs::write(&path, &buf).unwrap();
    }
    drop(p);
    let p = Pyramid::open(&dir.0, config()).unwrap();
    assert_eq!(p.stats().files_ignored, 0);
    let after: Vec<_> = query(&p, region, span, Resolution::Level(0))
        .cells
        .iter()
        .map(key_of)
        .collect();
    assert_eq!(before, after);
}

/// One frame every 10 s on each band, interleaved in time order.
fn sparse_bands(p: &mut Pyramid, bands: &[f64], from_s: i64, to_s: i64) {
    let psd = vec![lin(-100.0); 16];
    for s in (from_s..to_s).step_by(10) {
        for &f_lo in bands {
            p.ingest(&frame(T0 + s * S, S, f_lo, 1000.0, &psd)).unwrap();
        }
    }
}

fn blocks(p: &Pyramid, level: usize) -> Vec<i64> {
    let mut v: Vec<i64> = p.sealed_keys(level).iter().map(|k| k.f_block).collect();
    v.dedup();
    v.sort_unstable();
    v.dedup();
    v
}

#[test]
fn retention_ages_tiers_and_honours_region_overrides_on_a_fast_forwarded_clock() {
    const HOUR: u64 = 3600;
    const DAY: u64 = 86_400;
    let dir = TempDir::new("retain");
    // A = 1.600–1.616 MHz (L0 block 100), B = 3.200–3.216 MHz (L0 block 200).
    let (a, b) = (1_600_000.0, 3_200_000.0);
    let mut c = cfg(vec![level(1, 60), level(2, 60), level(2, 24)], 16);
    c.levels[0].max_age = Some(Duration::from_secs(HOUR));
    c.levels[1].max_age = Some(Duration::from_secs(DAY));
    c.retention_overrides = vec![RetentionOverride {
        freq: FreqRange::new(1_601_000.0, 1_602_000.0),
        level: 0,
        max_age: Duration::from_secs(90 * DAY),
    }];
    let mut p = Pyramid::open(&dir.0, c).unwrap();
    sparse_bands(&mut p, &[a, b], 0, 7200);
    p.seal_through(ts(T0 + 2 * 3600 * S)).unwrap();
    let count = |p: &Pyramid, level: usize, fb: i64| {
        p.sealed_keys(level)
            .iter()
            .filter(|k| k.f_block == fb)
            .count()
    };
    // L0 older than 1 h expired outside the override; A kept at full resolution.
    assert_eq!((count(&p, 0, 100), count(&p, 0, 200)), (120, 60));
    assert_eq!(p.stats().tiles_expired, 60);

    // Fast-forward two days: B's level 0 is gone, its level-1 hours expired once the day tile
    // covered them; A's level-1 hours stay while their protected children do (children first).
    p.seal_through(ts(T0 + 2 * 86_400 * S)).unwrap();
    assert_eq!((count(&p, 0, 100), count(&p, 0, 200)), (120, 0));
    assert_eq!((count(&p, 1, 50), count(&p, 1, 100)), (2, 0));
    assert_eq!(blocks(&p, 2), vec![25, 50]);
    let old_b = query(
        &p,
        (b, b + 16_000.0),
        (T0, T0 + 60 * S),
        Resolution::Level(0),
    );
    assert!(old_b.cells.iter().all(|c| c.level == 2 && c.observed()));

    // Past 90 days the override lapses: A's level 0 expires, then its now childless level-1
    // tiles; the untimed top level keeps the summary.
    p.seal_through(ts(T0 + 91 * 86_400 * S)).unwrap();
    assert!(p.sealed_keys(0).is_empty() && p.sealed_keys(1).is_empty());
    assert_eq!(blocks(&p, 2), vec![25, 50]);
    assert_eq!(p.stats().tiles_expired, 240 + 4);
}

#[test]
fn level_quota_spares_protected_tiles_and_budget_takes_them_last() {
    let dir = TempDir::new("quota");
    let (a, b) = (1_600_000.0, 3_200_000.0);
    let config = |budget: u64| {
        let mut c = cfg(vec![level(1, 60), level(2, 60)], 16);
        c.levels[0].byte_quota = Some(0);
        c.byte_budget = budget;
        c.retention_overrides = vec![RetentionOverride {
            freq: FreqRange::new(a, a + 1.0),
            level: 0,
            max_age: Duration::from_secs(90 * 86_400),
        }];
        c
    };
    let mut p = Pyramid::open(&dir.0, config(u64::MAX)).unwrap();
    sparse_bands(&mut p, &[a, b], 0, 600);
    p.seal_through(ts(T0 + 3600 * S)).unwrap();
    // Quota 0 at level 0: every covered unprotected tile went; protected ones stay, flagged.
    assert_eq!(blocks(&p, 0), vec![100]);
    assert_eq!(p.sealed_keys(0).len(), 10);
    assert_eq!(p.stats().tiles_evicted_quota, 10);
    assert!(p.stats().over_quota);
    let first_a = p.sealed_keys(0)[0];
    let disk = p.disk_bytes();
    drop(p);

    // One byte over budget: B's level-1 hour (childless, unprotected) goes first.
    let p = Pyramid::open(&dir.0, config(disk - 1)).unwrap();
    assert_eq!(blocks(&p, 1), vec![50]);
    assert_eq!(p.sealed_keys(0).len(), 10);
    let disk = p.disk_bytes();
    drop(p);

    // Over again: only protected tiles and their parent remain, so the oldest protected level-0
    // tile goes before A's level-1 hour (children first).
    let p = Pyramid::open(&dir.0, config(disk - 1)).unwrap();
    assert_eq!(p.sealed_keys(0).len(), 9);
    assert!(!p.sealed_keys(0).contains(&first_a));
    assert_eq!(blocks(&p, 1), vec![50]);
    assert!(!p.stats().over_budget);
}

#[test]
fn burst_every_five_minutes_survives_an_hour_and_floor_matches_injected_noise() {
    let dir = TempDir::new("burst5");
    let mut p = Pyramid::open(
        &dir.0,
        cfg(vec![level(1, 60), level(2, 60), level(2, 1)], 16),
    )
    .unwrap();
    let mut rng = Rng(99);
    let mut psd = vec![0f32; 16];
    // 8-look noise at −100 dB/Hz on 1 kHz bins aligned with the 1 kHz cells: cell shape = 8.
    // An emitter at 21 kHz, −70 dB/Hz, on for 10 s every 5 min.
    for k in 0..36_000i64 {
        for v in psd.iter_mut() {
            *v = (undb(-100.0) * rng.gamma(8)) as f32;
        }
        if k % 3000 < 100 {
            psd[5] = lin(-70.0);
        }
        let mut f = frame(T0 + k * S / 10, S / 10, 16_000.0, 1000.0, &psd);
        f.noise_shape = NoiseShape::CellShape(8.0);
        p.ingest(&f).unwrap();
    }
    p.seal_through(ts(T0 + 3600 * S)).unwrap();
    let f = (16_000.0, 32_000.0);
    let hour = (T0, T0 + 3600 * S);
    let h2 = query(&p, f, hour, Resolution::Level(2));
    let h1 = query(&p, f, hour, Resolution::Level(1));
    let h0 = query(&p, f, hour, Resolution::Level(0));

    let burst = h2.cell(0, 1);
    assert!(
        burst.max_db >= -70.5,
        "max tier keeps the burst: {}",
        burst.max_db
    );
    assert!(burst.occupancy_max >= 0.99);
    assert!(
        (burst.occupancy - 120.0 / 3600.0).abs() < 0.01,
        "{}",
        burst.occupancy
    );
    for c in 0..4 {
        let cell = h2.cell(0, c);
        eprintln!(
            "T-116 floor L2 cell {c}: raw p10 {:.2} dB, corrected {:.2} dB (injected −100)",
            cell.p_low_db, cell.floor_db
        );
        assert!(
            cell.p_low_db < -101.5,
            "raw p10 is biased low: {}",
            cell.p_low_db
        );
        assert!((cell.floor_db + 100.0).abs() <= 0.5, "{cell:?}");
    }
    let mut worst = 0f32;
    for t in 0..60 {
        for c in 0..8 {
            let cell = h1.cell(t, c);
            worst = worst.max((cell.floor_db + 100.0).abs());
            let busy = c == 2 && t % 5 == 0;
            assert_eq!(
                cell.max_db >= -70.5,
                busy,
                "minute {t} col {c}: {}",
                cell.max_db
            );
        }
    }
    eprintln!("T-116 floor L1: worst |corrected − injected| = {worst:.2} dB");
    assert!(worst <= 0.5);

    // Tier consistency: max-of-max, frames and power mean agree across boundaries.
    for c in 0..4 {
        let kids: Vec<_> = (0..60)
            .flat_map(|t| [h1.cell(t, 2 * c), h1.cell(t, 2 * c + 1)])
            .collect();
        let cell = h2.cell(0, c);
        let max = kids
            .iter()
            .map(|k| k.max_db)
            .fold(f32::NEG_INFINITY, f32::max);
        assert_eq!(cell.max_db, max);
        assert_eq!(cell.frames, kids.iter().map(|k| k.frames).sum::<u32>());
    }
    for t in 0..60 {
        for c in 0..8 {
            let kids: Vec<_> = (t * 60..(t + 1) * 60)
                .flat_map(|s| [h0.cell(s, 2 * c), h0.cell(s, 2 * c + 1)])
                .collect();
            let cell = h1.cell(t, c);
            let max = kids
                .iter()
                .map(|k| k.max_db)
                .fold(f32::NEG_INFINITY, f32::max);
            assert_eq!(cell.max_db, max, "minute {t} col {c}");
            let n: u32 = kids.iter().map(|k| k.frames).sum();
            assert_eq!(cell.frames, n);
            let lin_sum: f64 = kids
                .iter()
                .map(|k| undb(k.mean_db) * f64::from(k.frames))
                .sum();
            assert!((db(lin_sum / f64::from(n)) - cell.mean_db).abs() <= 0.02);
            assert_eq!(cell.coverage, 1.0);
        }
    }
}

fn png_pixels(png: &[u8]) -> (usize, usize, Vec<u8>) {
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    let (mut at, mut w, mut h, mut idat) = (8, 0, 0, Vec::new());
    while at < png.len() {
        let len = u32::from_be_bytes(png[at..at + 4].try_into().unwrap()) as usize;
        let kind = &png[at + 4..at + 8];
        let data = &png[at + 8..at + 8 + len];
        let crc = u32::from_be_bytes(png[at + 8 + len..at + 12 + len].try_into().unwrap());
        assert_eq!(codec::crc32(&png[at + 4..at + 8 + len]), crc);
        match kind {
            b"IHDR" => {
                w = u32::from_be_bytes(data[..4].try_into().unwrap()) as usize;
                h = u32::from_be_bytes(data[4..8].try_into().unwrap()) as usize;
            }
            b"IDAT" => idat.extend_from_slice(data),
            _ => {}
        }
        at += 12 + len;
    }
    let (mut p, mut raw) = (2, Vec::new());
    loop {
        let last = idat[p] & 1 == 1;
        let len = u16::from_le_bytes([idat[p + 1], idat[p + 2]]) as usize;
        raw.extend_from_slice(&idat[p + 5..p + 5 + len]);
        p += 5 + len;
        if last {
            break;
        }
    }
    assert_eq!(raw.len(), h * (w + 1));
    let px = raw.chunks(w + 1).flat_map(|r| r[1..].to_vec()).collect();
    (w, h, px)
}

#[test]
fn hackrf_sweep_csv_imports_and_exports_with_png_waterfall() {
    use std::fmt::Write as _;
    let dir = TempDir::new("csv");
    let open = |dir: &TempDir| {
        let mut c = cfg(vec![level(1, 10), level(2, 6)], 16);
        c.f_cell_hz = 100_000.0;
        c.seal_lag = Duration::from_secs(2);
        Pyramid::open(&dir.0, c).unwrap()
    };
    let mut p = open(&dir);
    // Six sweeps 0.5 s apart; two 8-bin 100 kHz slices over 400.0–401.6 MHz, upper slice
    // first (hackrf_sweep interleaves); a −30 dBFS tone at 400.9 MHz from the third sweep.
    let mut csv = String::new();
    for k in 0..6u32 {
        for slice in [1u32, 0] {
            let lo = 400_000_000 + slice * 800_000;
            write!(
                csv,
                "2026-09-13, 12:00:{:02}.{:06}, {lo}, {}, 100000.00, 20",
                k / 2,
                k % 2 * 500_000,
                lo + 800_000
            )
            .unwrap();
            for bin in 0..8 {
                let v = if slice == 1 && bin == 1 && k >= 2 {
                    -30.0
                } else {
                    -80.0 - bin as f32 * 0.25
                };
                write!(csv, ", {v:.2}").unwrap();
            }
            csv.push('\n');
        }
    }
    csv.push_str("not, a, sweep line\n");
    let r = import_sweep_csv(&mut p, csv.as_bytes(), &SweepCsvOptions::default()).unwrap();
    assert_eq!(
        (r.rows, r.rows_rejected, r.frames_folded, r.frames_late),
        (13, 1, 12, 0)
    );
    assert_eq!(r.revisit_ns, S / 2);
    assert_eq!(r.first, Some(ts(T0)));
    p.seal_through(ts(T0 + 10 * S)).unwrap();

    let h = query(&p, (400e6, 401.6e6), (T0, T0 + 3 * S), Resolution::Level(0));
    assert_eq!((h.nt, h.nf), (3, 16));
    assert_eq!(h.cell(1, 9).mean_db, -80.0, "−30 dB per 100 kHz bin");
    assert_eq!(h.cell(0, 9).mean_db, -130.25);
    assert_eq!(h.cell(1, 9).coverage, 1.0);

    let mut out = Vec::new();
    write_sweep_csv(&h, HistoryStat::Mean, 8, &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 6, "3 rows × 2 slices: {text}");
    assert!(
        lines[0].starts_with(
            "2026-09-13, 12:00:00.000000, 400000000, 400800000, 100000.00, 2, -80.00, -80.25"
        ),
        "{}",
        lines[0]
    );
    assert!(lines[3].starts_with(
        "2026-09-13, 12:00:01.000000, 400800000, 401600000, 100000.00, 2, -80.00, -30.00"
    ));

    // Round trip: the exported CSV imports to the same means.
    let dir2 = TempDir::new("csv2");
    let mut p2 = open(&dir2);
    let r2 = import_sweep_csv(
        &mut p2,
        text.as_bytes(),
        &SweepCsvOptions {
            revisit: Some(Duration::from_secs(1)),
            ..SweepCsvOptions::default()
        },
    )
    .unwrap();
    assert_eq!((r2.frames_folded, r2.rows_rejected), (6, 0));
    p2.seal_through(ts(T0 + 10 * S)).unwrap();
    let h2 = query(
        &p2,
        (400e6, 401.6e6),
        (T0, T0 + 3 * S),
        Resolution::Level(0),
    );
    for (a, b) in h.cells.iter().zip(&h2.cells) {
        assert_eq!(a.observed(), b.observed());
        if a.observed() {
            assert_eq!(a.mean_db, b.mean_db);
        }
    }

    // Unobserved cells: `nan` in a partly observed slice, fully unobserved slices omitted.
    let wide = query(&p, (400e6, 402.4e6), (T0, T0 + 2 * S), Resolution::Level(0));
    let mut out = Vec::new();
    write_sweep_csv(&wide, HistoryStat::Max, 10, &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert_eq!(text.lines().count(), 4);
    assert!(
        text.lines()
            .all(|l| (l.split(", ").nth(2) == Some("401000000")) == l.contains("nan"))
    );

    let (w, hgt, px) = png_pixels(&waterfall_png(&wide, HistoryStat::Max, None));
    assert_eq!((w, hgt), (24, 2));
    assert_eq!(px[w + 9], 255, "tone at the top of the range");
    assert_eq!(px[20], 0, "unobserved is palette index 0 (grey)");
    assert!((1..255).contains(&px[0]));
}
