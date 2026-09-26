//! **T-1024: what a tile cell costs on disk, plane by plane.**
//!
//! The user's storage ruling (2026-09-25) makes the pyramid rolling *history*, not a cache: bytes
//! per cell decide how many hours of one band the device keeps. Staging measured one 2.4 MHz band
//! at 0.04 s rows growing `history/` at 1.1 MB/s (~3.9 GB/h) at ~1.8 B/cell, so 8 GiB holds about
//! **2 h**.
//!
//! # What the measurement found (level 0, a 2.4 MHz mock live run, zstd-3)
//!
//! | plane | raw B/cell | marginal B/cell | share of the tile |
//! |---|---|---|---|
//! | `max_db` | 2.000 | 1.276 | 45 % |
//! | `mean_db` / `p_low_db` / `p_high_db` | 6.000 | 1.469 *(as a group)* | 52 % |
//! | `occupancy` + `occupancy_max` + `coverage` | 6.000 | **0.040** *(as a group)* | 1.4 % |
//! | `frames` | 1.000 | 0.000 | 0 % |
//! | histogram + observed bitmap | 0.257 | 0.036 | 1.3 % |
//! | **total** | **15.257** | | **2.827 B/cell** |
//!
//! So **97.5 % of a tile is the four dB order statistics**, each sitting at the entropy of a
//! single-look spectrum estimate quantised to 0.01 dB, and the planes this ticket set out to stop
//! storing — `coverage` above all, at **0.001 B/cell** — are already free. Three reductions were
//! implemented and priced against that:
//!
//! * **Stop storing the derivable planes.** A format-7 payload with a per-column tag (constant /
//!   duplicate-of / delta-of) halves the *raw* payload and buys **0.5 %** of the compressed
//!   level-0 tile and **−3.9 %** (worse) on a coarse one, where eliding an all-unknown percentile
//!   column costs zstd more context than the column costs bytes. Guaranteeing it never grew a
//!   tile needed a second encode-and-compress per seal: **+100 % seal cost on the capture
//!   thread** (T-453) for 0.5 % of disk. Not shipped; the code is in this history and the
//!   measurement is [`plane_bytes_per_cell_and_what_a_reduction_would_buy`].
//! * **A higher zstd level.** 3 → 19 buys 5 % for 18× the compression time. Not shipped.
//! * **A coarser dB grid.** The only real lever, and a lossy one: 0.1 dB is **−30 %** and 0.25 dB
//!   **−42 %**, both still 20× finer than the estimator's own standard error. It discards measured
//!   values, so it is the user's ruling to make and not the codec's; the sweep is printed.
//!
//! Measured again on a **real pipeline store** — 37 tiles from an `hk-pipeline` mock-SDR run,
//! scheme 1 and the live view lattice, `HK_TILE_BYTES_DIR` on a read-only copy — the picture is
//! the same at every level: **2.07-2.15 B/cell**, `max_db` and `mean_db` ~1.0 B/cell each, and
//! `coverage`, `occupancy`, `occupancy_max` and `frames` all at **<= 0.002 B/cell**.
//!
//! Run the table with
//!
//! ```text
//! cargo nextest run -p hk-store -E 'test(plane_bytes)' --no-capture
//! ```
//!
//! and point it at a real store directory (a read-only copy) with `HK_TILE_BYTES_DIR=<dir>`.

use std::fmt::Write as _;

use super::*;
use crate::history::codec;
use crate::history::tile::Tile;

/// A staging-shaped live lattice: 2.34 kHz × 40 ms level-0 cells (a 2.4 MHz band at a 1024-bin
/// FFT and 25 rows/s), 1024 frequency cells and 64 time cells per tile, with one coarse level.
fn live_cfg() -> PyramidConfig {
    PyramidConfig {
        scheme: 91,
        f_cell_hz: 2_343.75,
        t_cell: Duration::from_millis(40),
        f_cells_per_block: 1024,
        levels: vec![
            LevelConfig {
                f_factor: 1,
                t_cells_per_block: 64,
                ..LevelConfig::default()
            },
            LevelConfig {
                f_factor: 2,
                t_cells_per_block: 64,
                t_factor: Some(2),
                ..LevelConfig::default()
            },
        ],
        histogram: HistogramConfig {
            lo_db: -200.0,
            step_db: 5.0,
            bins: 44,
        },
        seal_lag: Duration::ZERO,
        checkpoint_interval: None,
        byte_budget: u64::MAX,
        ..PyramidConfig::default()
    }
}

/// Realistic PSD: a −110 dBFS/Hz K = 4 noise floor with a handful of steady carriers and one
/// burst that comes and goes, over `bins` bins.
fn psd(rng: &mut Rng, k: i64, bins: usize) -> Vec<f32> {
    let mut v = vec![0f32; bins];
    for (b, x) in v.iter_mut().enumerate() {
        let level = if b % 137 == 11 {
            -70.0
        } else if (300..320).contains(&b) && (k / 25) % 3 == 0 {
            -60.0
        } else {
            -110.0
        };
        *x = (undb(level) * rng.gamma(4)) as f32;
    }
    v
}

/// Ingests `secs` seconds of 25-rows/s frames into a fresh pyramid and returns it with its dir.
fn live_run(tag: &str, secs: i64) -> (TempDir, Pyramid) {
    let dir = TempDir::new(tag);
    let mut p = Pyramid::open(&dir.0, live_cfg()).unwrap();
    let mut rng = Rng(0x5eed_1024);
    let bins = 1024;
    let f_lo = 433.92e6 - 1.2e6;
    let bw = 2.4e6 / bins as f64;
    let dur = 40 * S / 1000;
    for k in 0..secs * 25 {
        let v = psd(&mut rng, k, bins);
        p.ingest(&frame(T0 + k * dur, dur, f_lo, bw, &v)).unwrap();
    }
    p.seal_through(ts(T0 + secs * 25 * dur + 10 * S)).unwrap();
    (dir, p)
}

/// Prices every plane of `tile` and appends the table to `out`. Returns
/// `(observed cells, raw payload bytes, compressed payload bytes)`.
fn plane_table(label: &str, tile: &Tile, out: &mut String) -> (usize, usize, usize) {
    let (cells, raw, z, planes) = codec::plane_bytes(tile, 3);
    let per = |b: usize| b as f64 / cells.max(1) as f64;
    writeln!(
        out,
        "\n{label}: {cells} observed cells of {} ({} x {})\n\
         {:<16} {:>12} {:>12} {:>12}\n{:-<56}",
        tile.nf * tile.nt,
        tile.nf,
        tile.nt,
        "plane",
        "raw B/cell",
        "alone B/cell",
        "marg B/cell",
        ""
    )
    .unwrap();
    for p in &planes {
        writeln!(
            out,
            "{:<16} {:>12.3} {:>12.3} {:>12.3}",
            p.name,
            per(p.raw),
            per(p.alone),
            p.marginal as f64 / cells.max(1) as f64
        )
        .unwrap();
    }
    writeln!(
        out,
        "{:<16} {:>12.3} {:>12.3}   payload {z} B (raw {raw} B)",
        "TOTAL",
        per(raw),
        per(z)
    )
    .unwrap();
    // Groups, because planes that duplicate one another cannot be priced one at a time.
    for (name, skip) in [
        ("mean+p_lo+p_hi", 0b000_1110u16),
        ("all four dB", 0b000_1111),
        ("occupancy+occ_max+coverage", 0b111_0000),
        ("frames", 1 << 7),
        ("histogram", 1 << 8),
    ] {
        let (_, without) = codec::group_bytes(tile, skip, 3);
        writeln!(
            out,
            "  the group {name:<28} costs {:>6.3} B/cell ({:.1}% of the tile)",
            per(z) - per(without),
            100.0 * (z as f64 - without as f64) / z as f64
        )
        .unwrap();
    }
    for (l, bytes, enc, dec) in codec::payload_sweep(tile, &[1, 3, 6, 9, 12, 19]) {
        writeln!(
            out,
            "  zstd-{l:<2}: {:>7.3} B/cell  compress {:>7.2} ms  decompress {:>6.2} ms",
            per(bytes),
            enc * 1e3,
            dec * 1e3
        )
        .unwrap();
    }
    for (step, bytes) in codec::quant_sweep(tile, &[0.01, 0.05, 0.1, 0.25, 0.5, 1.0], 3) {
        writeln!(
            out,
            "  dB grid {step:>5} dB: {:>7.3} B/cell ({:+.0}% vs the stored 0.01 dB grid)",
            per(bytes),
            100.0 * (bytes as f64 - z as f64) / z as f64
        )
        .unwrap();
    }
    (cells, raw, z)
}

/// The widest sealed tile of `level`, with a label naming its file.
fn widest_sealed(p: &Pyramid, level: usize) -> (Tile, String) {
    let mut best: Option<(usize, Tile, String)> = None;
    for key in p.sealed_keys(level) {
        let path = p.tile_path(key);
        let Some((_, tile, _)) = codec::read_tile_standalone(&path).unwrap() else {
            continue;
        };
        let cells = (0..tile.nf * tile.nt)
            .filter(|&i| tile.count[i] > 0)
            .count();
        if best.as_ref().is_none_or(|b| cells > b.0) {
            best = Some((cells, tile, format!("L{level} {}", path.display())));
        }
    }
    let (_, tile, label) = best.expect("a decodable sealed tile");
    (tile, label)
}

/// **The table the ticket asks for**, on a mock live run: bytes per cell per plane at level 0 and
/// at a coarse level, raw and compressed, plus what each candidate reduction would buy.
///
/// The assertions are the findings, so they cannot rot silently: `coverage` is not where the disk
/// goes, and the four dB order statistics are.
#[test]
fn plane_bytes_per_cell_and_what_a_reduction_would_buy() {
    let (_dir, p) = live_run("planes", 120);
    let mut out = String::from("\nT-1024 tile bytes per cell, by plane (zstd-3)\n");
    let mut l0 = (0usize, 0usize);
    for level in 0..2 {
        let (tile, label) = widest_sealed(&p, level);
        let (cells, _, z) = plane_table(&label, &tile, &mut out);
        if level == 0 {
            let (_, _, _, planes) = codec::plane_bytes(&tile, 3);
            let per = |b: i64| b as f64 / cells as f64;
            // The ticket's premise, measured: coverage is derivable *and* free, so not storing it
            // buys nothing. (It is also not derivable from the observation record — see
            // `coverage_is_delivered_frame_seconds_not_tuned_seconds`.)
            assert!(
                per(planes[6].marginal).abs() < 0.05,
                "coverage costs {} B/cell",
                per(planes[6].marginal)
            );
            // ... and where the disk actually goes.
            let (_, without_db) = codec::group_bytes(&tile, 0b000_1111, 3);
            let db_share = (z as f64 - without_db as f64) / z as f64;
            assert!(
                db_share > 0.8,
                "the four dB planes are {:.0}% of the tile",
                db_share * 100.0
            );
            l0 = (cells, z);
        }
    }
    // Hours of one 2.4 MHz band per 8 GiB. Staging measured 1.1 MB/s at ~1.8 B/cell, so its cell
    // rate across every level is ~611 k cells/s; hours scale inversely with bytes per cell.
    let (cells, z) = l0;
    let b = z as f64 / cells as f64;
    let hours = |bpc: f64| (8.0 * 1024.0f64.powi(3)) / (611_000.0 * bpc) / 3600.0;
    writeln!(
        out,
        "\nHours of one 2.4 MHz band per 8 GiB, at staging's 611 k cells/s:\n  \
         staging's measured 1.800 B/cell -> {:.2} h\n  this fixture's {b:.3} B/cell -> {:.2} h\n  \
         the same fixture on a 0.1 dB grid (-30%) -> {:.2} h;  on 0.25 dB (-42%) -> {:.2} h",
        hours(1.8),
        hours(b),
        hours(b * 0.70),
        hours(b * 0.58)
    )
    .unwrap();
    println!("{out}");
}

/// **Round trip.** Every plane a sealed tile stores reads back the value it was written with, and
/// re-encoding the decoded tile is byte-identical — the fixed point the byte table is measured on.
#[test]
fn plane_bytes_round_trip_every_stored_plane() {
    let (_dir, p) = live_run("rt", 40);
    let hist = live_cfg().histogram;
    let bins = usize::from(hist.bins);
    let mut checked = 0;
    for level in 0..2 {
        for key in p.sealed_keys(level) {
            let path = p.tile_path(key);
            let bytes = fs::read(&path).unwrap();
            let (h, tile, _) = codec::read_tile_standalone(&path)
                .unwrap()
                .expect("decodes");
            let g = LevelGeometry {
                f_cell_hz: h.f_cell_hz,
                t_cell_ns: h.t_cell_ns,
                nt: tile.nt,
                f_factor: 1,
                t_factor: 1,
                from: None,
            };
            let (mut buf, mut scratch) = (Vec::new(), Vec::new());
            codec::encode_format(
                codec::FORMAT_VERSION,
                &tile,
                true,
                PowerUnit::Dbfs,
                &g,
                &hist,
                (10.0, 90.0),
                Some(3),
                &mut buf,
                &mut scratch,
            );
            assert_eq!(
                buf,
                bytes,
                "{}: re-encode is byte-identical",
                path.display()
            );
            let back = codec::decode_bytes(&buf, &g, bins).expect("decodes");
            assert_eq!(back.count, tile.count, "frames");
            same(&back.max, &tile.max, "max_db");
            same64(&back.sum_lin, &tile.sum_lin, "mean_db");
            same(&back.p_lo, &tile.p_lo, "p_low_db");
            same(&back.p_hi, &tile.p_hi, "p_high_db");
            same(&back.occ_max, &tile.occ_max, "occupancy_max");
            same64(&back.obs_s, &tile.obs_s, "coverage");
            same64(&back.occ_s, &tile.occ_s, "occupancy");
            assert_eq!(back.hist, tile.hist, "histogram");
            checked += 1;
        }
    }
    assert!(checked >= 2, "checked {checked} tiles");
}

/// **Why `coverage` is not read from the observation record.** The ticket's hypothesis was that a
/// per-cell coverage is derivable from the tune history, so the plane need not be stored. It is
/// not: the stored plane is the **delivered frame-seconds** of the cell, and the observation
/// record knows only what the front end was *tuned* for. Here the radio is tuned continuously and
/// delivers a frame for half of every cell — the stored coverage is 0.5, the record would say 1.0,
/// and reporting 1.0 is exactly the "we didn't measure it but say we did" the grey rule forbids.
#[test]
fn plane_bytes_coverage_is_delivered_frame_seconds_not_tuned_seconds() {
    let dir = TempDir::new("cov");
    let mut p = Pyramid::open(&dir.0, live_cfg()).unwrap();
    let bins = 64;
    let bw = 2.4e6 / 1024.0;
    let f_lo = 400e6;
    let dur = 20 * S / 1000; // half a 40 ms cell, every cell, continuously tuned
    let v = vec![1e-11f32; bins];
    for k in 0..25 * 64 {
        p.ingest(&frame(T0 + k * 2 * dur, dur, f_lo, bw, &v))
            .unwrap();
    }
    p.seal_through(ts(T0 + 25 * 64 * 2 * dur + 10 * S)).unwrap();
    let (tile, _) = widest_sealed(&p, 0);
    let mut seen = 0;
    for i in 0..tile.nf * tile.nt {
        if tile.count[i] == 0 {
            continue;
        }
        let (obs, _) = tile.cell_obs(i);
        let coverage = obs / tile.t_cell_s;
        assert!(
            (coverage - 0.5).abs() < 1e-4,
            "cell {i}: coverage {coverage}, not the tuned 1.0"
        );
        seen += 1;
    }
    assert!(seen > 1000, "only {seen} observed cells");
}

/// Bit-identical, with NaN (an unknown percentile) equal to NaN rather than unequal to itself.
fn same(a: &[f32], b: &[f32], what: &str) {
    assert_eq!(a.len(), b.len(), "{what}: length");
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        assert!(
            x == y || (x.is_nan() && y.is_nan()),
            "{what}: cell {i}: {x} vs {y}"
        );
    }
}

fn same64(a: &[f64], b: &[f64], what: &str) {
    assert_eq!(a.len(), b.len(), "{what}: length");
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        assert!(
            x == y || (x.is_nan() && y.is_nan()),
            "{what}: cell {i}: {x} vs {y}"
        );
    }
}

/// Optional: the same table over a real store directory (`HK_TILE_BYTES_DIR`, a read-only copy),
/// aggregated per level. Skipped when the variable is unset, which is every CI run.
#[test]
fn plane_bytes_over_a_store_directory() {
    let Ok(root) = std::env::var("HK_TILE_BYTES_DIR") else {
        return;
    };
    let mut files = Vec::new();
    let mut stack = vec![PathBuf::from(&root)];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "tile") {
                files.push(p);
            }
        }
    }
    files.sort();
    assert!(!files.is_empty(), "no tiles under {root}");
    /// Per level: observed cells, compressed bytes, and per plane (raw, marginal).
    type Level = (usize, usize, Vec<(usize, i64)>);
    let mut per_level: HashMap<u8, Level> = HashMap::new();
    for path in &files {
        let Some((h, tile, _)) = codec::read_tile_standalone(path).unwrap() else {
            continue;
        };
        let (cells, _, z, planes) = codec::plane_bytes(&tile, 3);
        let e = per_level
            .entry(h.level)
            .or_insert_with(|| (0, 0, vec![(0, 0); codec::PLANE_NAMES.len()]));
        e.0 += cells;
        e.1 += z;
        for (i, a) in planes.iter().enumerate() {
            e.2[i].0 += a.raw;
            e.2[i].1 += a.marginal;
        }
    }
    let mut levels: Vec<_> = per_level.into_iter().collect();
    levels.sort_by_key(|(l, _)| *l);
    println!("\nT-1024: {} tiles under {root}", files.len());
    for (level, (cells, z, planes)) in levels {
        println!(
            "\nL{level}: {cells} observed cells, {:.3} B/cell",
            z as f64 / cells.max(1) as f64
        );
        for (i, (raw, marginal)) in planes.iter().enumerate() {
            println!(
                "  {:<16} raw {:>7.3}  marginal {:>7.3} B/cell",
                codec::PLANE_NAMES[i],
                *raw as f64 / cells.max(1) as f64,
                *marginal as f64 / cells.max(1) as f64
            );
        }
    }
}
