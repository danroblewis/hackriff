//! Report exports (§6.4), rendered in the backend: CSV (channel occupancy rows, then coverage gaps,
//! with a header comment carrying the coverage statement) and PNG (occupancy heatmap over the
//! history grid, unobserved cells hatched).

use std::fmt::Write as _;

use hk_model::attention::occupancy::{OccupancyStat, OccupancySubject};
use hk_model::attention::report::{ComparisonStatus, SurveyReport};
use hk_model::{FreqRange, Timestamp};
use hk_store::RegionHistory;
use hk_store::history::PNG_UNOBSERVED_RGB;

fn ts(t: Timestamp) -> f64 {
    t.as_unix_nanos() as f64 / 1e9
}

fn opt(v: Option<f64>) -> String {
    v.map_or_else(String::new, |v| format!("{v:.6}"))
}

fn stat_row(out: &mut String, row: &str, freq: FreqRange, s: &OccupancyStat) {
    let _ = writeln!(
        out,
        "{row},{:.0},{:.0},{:.3},{:.3},{},{},{},{},{},{},{:.3},{}",
        freq.lo_hz,
        freq.hi_hz,
        ts(s.interval.start),
        ts(s.interval.end),
        opt(s.fco),
        opt(s.fco_all_visits),
        opt(s.fbo),
        s.n_revisits,
        s.n_occupied,
        s.n_revisits_all,
        s.observed_s,
        s.revisit_biased
    );
}

/// The CSV export. Channel extents come from their keys on the level-0 grid (`level0_f_cell_hz`).
/// Every line starting `#` is a comment; the first comments carry the coverage statement, which
/// always says unobserved is not quiet.
pub fn report_csv(r: &SurveyReport, level0_f_cell_hz: f64) -> String {
    let mut out = String::new();
    let c = &r.coverage;
    let _ = writeln!(out, "# hackriff survey report (schema {})", r.schema);
    let _ = writeln!(
        out,
        "# region_hz {:.0}..{:.0}, span_s {:.3}..{:.3}",
        r.region.lo_hz,
        r.region.hi_hz,
        ts(r.span.start),
        ts(r.span.end)
    );
    let _ = writeln!(out, "# coverage: {}", c.statement.replace('\n', " "));
    let _ = writeln!(
        out,
        "# observed_fraction {:.6}, observed_s {:.3}, gaps {}{}, never_observed {}",
        c.observed_fraction,
        c.observed_s,
        c.gaps.len(),
        if c.gaps_truncated { " (truncated)" } else { "" },
        c.never_observed.len()
    );
    let poi: Vec<String> = c
        .poi
        .iter()
        .map(|p| format!("tau {} s p_poi {:.6}", p.tau_s, p.p_poi))
        .collect();
    let _ = writeln!(out, "# poi: {}", poi.join("; "));
    let status = match r.change_vs_baseline.status {
        ComparisonStatus::Available => "available",
        ComparisonStatus::Immature => "immature",
        ComparisonStatus::NoBaseline => "no-baseline",
        ComparisonStatus::Unavailable => "unavailable",
    };
    let _ = writeln!(out, "# change_vs_baseline: {status}");
    let _ = writeln!(
        out,
        "# fco is activity-independent (empty when unavailable, never substituted); \
         fco_all_visits includes activity-driven dwells"
    );
    let _ = writeln!(
        out,
        "row,f_lo_hz,f_hi_hz,t0_s,t1_s,fco,fco_all_visits,fbo,n_revisits,n_occupied,\
         n_revisits_all,observed_s,revisit_biased"
    );
    for s in &r.occupancy.bands {
        let freq = match s.subject {
            OccupancySubject::Band { freq } => freq,
            OccupancySubject::Channel { key } => key.freq(level0_f_cell_hz),
        };
        stat_row(&mut out, "band", freq, s);
    }
    for s in &r.occupancy.channels {
        let freq = match s.subject {
            OccupancySubject::Channel { key } => key.freq(level0_f_cell_hz),
            OccupancySubject::Band { freq } => freq,
        };
        stat_row(&mut out, "channel", freq, s);
    }
    for g in &c.gaps {
        let _ = writeln!(
            out,
            "gap,{:.0},{:.0},{:.3},{:.3},,,,,,,,",
            g.freq.lo_hz,
            g.freq.hi_hz,
            ts(g.time.start),
            ts(g.time.end)
        );
    }
    for f in &c.never_observed {
        let _ = writeln!(
            out,
            "never_observed,{:.0},{:.0},{:.3},{:.3},,,,,,,,",
            f.lo_hz,
            f.hi_hz,
            ts(r.span.start),
            ts(r.span.end)
        );
    }
    out
}

/// Palette index of an unobserved cell on a hatch line.
const HATCH: u8 = 1;
const HATCH_RGB: [u8; 3] = [64, 64, 64];

fn occupancy_rgb(x: f32) -> [u8; 3] {
    let x = x.clamp(0.0, 1.0);
    let lerp = |a: f32, b: f32| (a + (b - a) * x).round() as u8;
    [lerp(16.0, 250.0), lerp(24.0, 214.0), lerp(72.0, 40.0)]
}

/// The PNG export: one pixel per grid cell (time down, frequency right), colour = tile occupancy
/// (dark = 0, yellow = 1); unobserved cells are grey with a diagonal hatch, so not observed never
/// reads as quiet.
pub fn report_png(grid: &RegionHistory) -> Vec<u8> {
    let (w, h) = (grid.nf.max(1), grid.nt.max(1));
    let mut raw = Vec::with_capacity(h * (w + 1));
    for t in 0..h {
        raw.push(0);
        for f in 0..w {
            let c = (t < grid.nt && f < grid.nf).then(|| grid.cell(t, f));
            let idx = match c {
                Some(c) if c.observed() && c.occupancy.is_finite() => {
                    2 + (c.occupancy.clamp(0.0, 1.0) * 253.0).round() as u8
                }
                _ if (t + f) % 4 == 0 => HATCH,
                _ => 0,
            };
            raw.push(idx);
        }
    }
    let mut plte = PNG_UNOBSERVED_RGB.to_vec();
    plte.extend_from_slice(&HATCH_RGB);
    for i in 0..254u32 {
        plte.extend_from_slice(&occupancy_rgb(i as f32 / 253.0));
    }
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 3, 0, 0, 0]);
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"PLTE", &plte);
    chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

fn zlib_stored(raw: &[u8]) -> Vec<u8> {
    let mut z = vec![0x78, 0x01];
    if raw.is_empty() {
        z.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    let mut blocks = raw.chunks(65_535).peekable();
    while let Some(b) = blocks.next() {
        z.push(u8::from(blocks.peek().is_none()));
        let len = b.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(b);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in raw {
        a = (a + u32::from(x)) % 65_521;
        b = (b + a) % 65_521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    z
}

fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &p in parts {
        for &byte in p {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
    }
    !crc
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&[kind, data]).to_be_bytes());
}
