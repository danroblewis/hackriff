//! `hackrf_sweep` CSV import/export and PNG waterfall export (T-116; C26 "Export to PNG, or to CSV
//! in hackrf_sweep format").
//!
//! # The CSV format
//!
//! One line per sweep slice, exactly as `hackrf_sweep` prints it (docs/01 §1.6):
//!
//! ```text
//! 2026-09-13, 12:00:00.250000, 88000000, 93000000, 1000000.00, 20, -70.12, -71.30, …
//! date,       time,            hz_low,   hz_high,  bin_width,  num_samples, dB per bin…
//! ```
//!
//! The dB values are **power per bin** (dBFS for `hackrf_sweep`); history stores densities, so
//! import divides by the bin width (`10^(dB/10)/bin_width`, as [`super::DbScratch::sweep_frame`])
//! and export multiplies back. `hackrf_sweep` prints local time: [`SweepCsvOptions::utc_offset_s`]
//! says which zone the file is in (default UTC). Export always writes UTC.
//!
//! **Import.** Each line becomes one [`FrameInput`] of duration = the sweep revisit time
//! ([`SweepCsvOptions::revisit`], or estimated from the file: the time between the first line and
//! the next line with the same `hz_low`). Lines stream straight into the pyramid, so a file must be
//! in time order (as `hackrf_sweep` writes it) and its revisit should not exceed the pyramid's
//! `seal_lag`, or lines for already-sealed tiles are counted late. Malformed lines are counted and
//! skipped.
//!
//! **Export.** One line per time row of a [`RegionHistory`], split into slices of at most
//! `cells_per_line` cells; `num_samples` carries the largest frame count in the slice. Unobserved
//! cells are written `nan` and fully unobserved slices are omitted: not observed is not quiet.
//!
//! # The PNG waterfall
//!
//! One pixel per cell, frequency left→right, time top (earliest) → bottom; 8-bit palette. Index 0
//! is **mid grey = not observed**; indices 1–255 map the chosen statistic linearly over the range
//! (default: 2nd percentile to maximum of the observed values) onto a dark-purple→yellow ramp.
//! Encoded with stored (uncompressed) deflate blocks: no compression dependency, and the size is
//! bounded by the query's cell limit.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::time::Duration;

use hk_model::{PowerUnit, Timestamp};

use super::StoreError;
use super::codec::crc32;
use super::frame::{FrameInput, GainState};
use super::query::{CellStats, RegionHistory};
use super::stats::undb;
use super::store::{IngestOutcome, Pyramid};

const NS: i64 = 1_000_000_000;
const DAY_S: i64 = 86_400;

/// Which cell statistic an export shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryStat {
    /// Max-hold.
    Max,
    /// Power mean.
    Mean,
    /// Low percentile (raw).
    PLow,
    /// High percentile.
    PHigh,
    /// Bias-corrected floor ([`CellStats::floor_db`]).
    Floor,
}

impl HistoryStat {
    /// The statistic of a cell, dB/Hz (NaN when unobserved or unavailable).
    pub fn of(self, c: &CellStats) -> f32 {
        if !c.observed() {
            return f32::NAN;
        }
        match self {
            Self::Max => c.max_db,
            Self::Mean => c.mean_db,
            Self::PLow => c.p_low_db,
            Self::PHigh => c.p_high_db,
            Self::Floor => c.floor_db,
        }
    }

    /// Parses `max`, `mean`, `p_low`, `p_high` or `floor`.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "max" => Self::Max,
            "mean" => Self::Mean,
            "p_low" => Self::PLow,
            "p_high" => Self::PHigh,
            "floor" => Self::Floor,
            _ => return None,
        })
    }
}

use super::frame::NoiseShape;

/// How to read a `hackrf_sweep` CSV.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SweepCsvOptions {
    /// Observation time each line represents; `None` estimates it from the file.
    pub revisit: Option<Duration>,
    /// Offset of the file's timestamps from UTC, seconds (`hackrf_sweep` prints local time).
    pub utc_offset_s: i32,
    /// Unit of the dB values (`hackrf_sweep`: dBFS).
    pub unit: PowerUnit,
    /// Gain state the capture used, if known (provenance).
    pub gain: Option<GainState>,
    /// Gamma shape of each bin value when known (T-126); `None` estimates it from the first
    /// [`SHAPE_ESTIMATE_SWEEPS`] sweeps ([`super::shape`]) so `floor_db` is available.
    pub bin_shape: Option<f32>,
    /// Source key of the capture ([`FrameInput::source`]; default `source_key("hackrf_sweep")`).
    pub source: u64,
}

impl Default for SweepCsvOptions {
    fn default() -> Self {
        Self {
            revisit: None,
            utc_offset_s: 0,
            unit: PowerUnit::Dbfs,
            gain: None,
            bin_shape: None,
            source: super::frame::source_key("hackrf_sweep"),
        }
    }
}

/// Sweeps [`import_sweep_csv`] buffers to estimate the bin noise shape before folding (T-126).
pub const SHAPE_ESTIMATE_SWEEPS: u32 = 16;
/// Most dB values buffered for the estimate (the estimate is made early past this).
const SHAPE_ESTIMATE_MAX_VALUES: usize = 1 << 23;

/// What [`import_sweep_csv`] did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SweepCsvImport {
    /// Non-empty lines read.
    pub rows: u64,
    /// Lines that did not parse or were rejected by the pyramid.
    pub rows_rejected: u64,
    /// Lines folded into history.
    pub frames_folded: u64,
    /// Lines whose tiles had already sealed.
    pub frames_late: u64,
    /// Revisit time used as each line's duration, ns.
    pub revisit_ns: i64,
    /// Earliest line time.
    pub first: Option<Timestamp>,
    /// Latest line time.
    pub last: Option<Timestamp>,
    /// Bin noise shape used (given or estimated, T-126); `None` when too few rows or noise bins
    /// were available, and `floor_db` reads NaN.
    pub bin_shape: Option<f32>,
}

struct Row {
    t_ns: i64,
    hz_low: f64,
    bin_width: f64,
    db: Vec<f32>,
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `(year, month, day)` of days since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if m <= 2 {
            yoe + era * 400 + 1
        } else {
            yoe + era * 400
        },
        m,
        d,
    )
}

fn parse_time_ns(date: &str, time: &str) -> Option<i64> {
    let mut d = date.split('-');
    let (y, mo, day) = (
        d.next()?.parse::<i64>().ok()?,
        d.next()?.parse::<i64>().ok()?,
        d.next()?.parse::<i64>().ok()?,
    );
    if d.next().is_some() || !(1..=12).contains(&mo) || !(1..=31).contains(&day) {
        return None;
    }
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut t = hms.split(':');
    let (h, mi, s) = (
        t.next()?.parse::<i64>().ok()?,
        t.next()?.parse::<i64>().ok()?,
        t.next()?.parse::<i64>().ok()?,
    );
    if t.next().is_some() || h > 23 || mi > 59 || s > 60 || h < 0 || mi < 0 || s < 0 {
        return None;
    }
    if frac.len() > 9 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let frac_ns = if frac.is_empty() {
        0
    } else {
        frac.parse::<i64>().ok()? * 10i64.pow(9 - frac.len() as u32)
    };
    let secs = days_from_civil(y, mo, day) * DAY_S + h * 3600 + mi * 60 + s;
    secs.checked_mul(NS)?.checked_add(frac_ns)
}

fn parse_row(line: &str, utc_offset_s: i32) -> Option<Row> {
    let mut it = line.split(',').map(str::trim);
    let t_ns = parse_time_ns(it.next()?, it.next()?)? - i64::from(utc_offset_s) * NS;
    let hz_low: f64 = it.next()?.parse().ok()?;
    let hz_high: f64 = it.next()?.parse().ok()?;
    let bin_width: f64 = it.next()?.parse().ok()?;
    let _num_samples: f64 = it.next()?.parse().ok()?;
    let db: Vec<f32> = it
        .map(|v| v.parse::<f32>())
        .collect::<Result<_, _>>()
        .ok()?;
    let ok = !db.is_empty()
        && hz_low.is_finite()
        && hz_high > hz_low
        && bin_width.is_finite()
        && bin_width > 0.0;
    ok.then_some(Row {
        t_ns,
        hz_low,
        bin_width,
        db,
    })
}

/// Revisit and noise shape of an import, decided from the buffered first rows.
fn import_plan(
    pending: &[Row],
    est: &super::shape::NoiseShapeEstimator,
    opts: &SweepCsvOptions,
) -> (i64, NoiseShape) {
    let rev = opts
        .revisit
        .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX).max(1))
        .or_else(|| {
            let (lo, t0) = pending.first().map(|r| (r.hz_low, r.t_ns))?;
            pending
                .iter()
                .find(|r| r.hz_low == lo && r.t_ns > t0)
                .map(|r| r.t_ns - t0)
        })
        .unwrap_or_else(|| {
            // A single sweep: its own span, at least a second.
            let span = pending.iter().map(|r| r.t_ns).max().unwrap_or(0)
                - pending.iter().map(|r| r.t_ns).min().unwrap_or(0);
            span.max(NS)
        });
    let shape = opts
        .bin_shape
        .filter(|k| k.is_finite() && *k > 0.0)
        .or_else(|| est.bin_shape())
        .map_or(NoiseShape::Unknown, NoiseShape::BinShape);
    (rev, shape)
}

fn emit_row(
    p: &mut Pyramid,
    opts: &SweepCsvOptions,
    psd: &mut Vec<f32>,
    row: &Row,
    (rev, shape): (i64, NoiseShape),
    out: &mut SweepCsvImport,
) -> Result<(), StoreError> {
    psd.clear();
    psd.extend(row.db.iter().map(|&d| (undb(d) / row.bin_width) as f32));
    let t = Timestamp::from_unix_nanos(row.t_ns);
    let mut f = FrameInput::new(t, rev, row.hz_low, row.bin_width, opts.unit, psd);
    f.gain = opts.gain;
    f.noise_shape = shape;
    f.source = opts.source;
    match p.ingest(&f) {
        Ok(IngestOutcome::Folded) => out.frames_folded += 1,
        Ok(IngestOutcome::Late) => out.frames_late += 1,
        Err(StoreError::BadFrame(_)) => out.rows_rejected += 1,
        Err(e) => return Err(e),
    }
    out.first = Some(out.first.map_or(t, |x| x.min(t)));
    out.last = Some(out.last.map_or(t, |x| x.max(t)));
    Ok(())
}

/// Folds a `hackrf_sweep` CSV into `p` (see the [module docs](self)). Seal with
/// [`Pyramid::seal_through`] afterwards to roll the last tiles up.
///
/// The first rows are buffered until the revisit time is known and, unless
/// [`SweepCsvOptions::bin_shape`] is given, [`SHAPE_ESTIMATE_SWEEPS`] sweeps have been seen, so
/// every row folds with the same estimated [`NoiseShape::BinShape`] (T-126).
pub fn import_sweep_csv<R: BufRead>(
    p: &mut Pyramid,
    reader: R,
    opts: &SweepCsvOptions,
) -> Result<SweepCsvImport, StoreError> {
    let mut out = SweepCsvImport::default();
    let mut est = super::shape::NoiseShapeEstimator::new();
    let mut pending: Vec<Row> = Vec::new();
    let (mut sweeps, mut buffered) = (0u32, 0usize);
    let mut plan: Option<(i64, NoiseShape)> = None;
    let mut psd: Vec<f32> = Vec::new();
    let need = match (opts.bin_shape.is_some(), opts.revisit.is_some()) {
        (true, true) => 1,
        (true, false) => 2,
        (false, _) => SHAPE_ESTIMATE_SWEEPS,
    };
    for line in reader.lines() {
        let line = line.map_err(|source| StoreError::Io {
            path: PathBuf::from("<hackrf_sweep csv>"),
            source,
        })?;
        if line.trim().is_empty() {
            continue;
        }
        out.rows += 1;
        let Some(row) = parse_row(&line, opts.utc_offset_s) else {
            out.rows_rejected += 1;
            continue;
        };
        if let Some(pl) = plan {
            emit_row(p, opts, &mut psd, &row, pl, &mut out)?;
            continue;
        }
        if opts.bin_shape.is_none() {
            est.observe(row.hz_low, &row.db);
        }
        if pending.first().is_none_or(|r| r.hz_low == row.hz_low) {
            sweeps += 1;
        }
        buffered += row.db.len();
        pending.push(row);
        if sweeps >= need || buffered >= SHAPE_ESTIMATE_MAX_VALUES {
            let pl = import_plan(&pending, &est, opts);
            for r in pending.drain(..) {
                emit_row(p, opts, &mut psd, &r, pl, &mut out)?;
            }
            plan = Some(pl);
        }
    }
    let pl = plan.unwrap_or_else(|| import_plan(&pending, &est, opts));
    for r in pending.drain(..) {
        emit_row(p, opts, &mut psd, &r, pl, &mut out)?;
    }
    out.revisit_ns = pl.0;
    out.bin_shape = match pl.1 {
        NoiseShape::BinShape(k) => Some(k),
        _ => None,
    };
    Ok(out)
}

fn write_time(w: &mut impl Write, ns: i64) -> io::Result<()> {
    let secs = ns.div_euclid(NS);
    let usec = ns.rem_euclid(NS) / 1000;
    let (y, m, d) = civil_from_days(secs.div_euclid(DAY_S));
    let sod = secs.rem_euclid(DAY_S);
    write!(
        w,
        "{y:04}-{m:02}-{d:02}, {:02}:{:02}:{:02}.{usec:06}",
        sod / 3600,
        sod / 60 % 60,
        sod % 60
    )
}

/// Writes `h` as `hackrf_sweep` CSV (see the [module docs](self)): per-bin dB of `stat`, UTC.
pub fn write_sweep_csv<W: Write>(
    h: &RegionHistory,
    stat: HistoryStat,
    cells_per_line: usize,
    mut w: W,
) -> io::Result<()> {
    let per_line = cells_per_line.max(1);
    let bin_db = 10.0 * h.f_cell_hz.log10();
    for t in 0..h.nt {
        let row = h.row(t);
        for start in (0..h.nf).step_by(per_line) {
            let chunk = &row[start..(start + per_line).min(h.nf)];
            if !chunk.iter().any(CellStats::observed) {
                continue;
            }
            let hz_low = h.freq_of(start).lo_hz;
            write_time(&mut w, h.time_of(t).as_unix_nanos())?;
            let samples = chunk.iter().map(|c| c.frames).max().unwrap_or(0);
            write!(
                w,
                ", {:.0}, {:.0}, {:.2}, {samples}",
                hz_low,
                hz_low + chunk.len() as f64 * h.f_cell_hz,
                h.f_cell_hz
            )?;
            for c in chunk {
                let v = stat.of(c);
                if v.is_finite() {
                    write!(w, ", {:.2}", f64::from(v) + bin_db)?;
                } else {
                    w.write_all(b", nan")?;
                }
            }
            w.write_all(b"\n")?;
        }
    }
    w.flush()
}

/// The palette index 0 colour: not observed.
pub const PNG_UNOBSERVED_RGB: [u8; 3] = [128, 128, 128];

fn ramp(x: f32) -> [u8; 3] {
    const STOPS: [[f32; 3]; 5] = [
        [0.0, 0.0, 4.0],
        [87.0, 16.0, 110.0],
        [188.0, 55.0, 84.0],
        [249.0, 142.0, 9.0],
        [252.0, 255.0, 164.0],
    ];
    let pos = x.clamp(0.0, 1.0) * 4.0;
    let i = (pos.floor() as usize).min(3);
    let f = pos - i as f32;
    let mut c = [0u8; 3];
    for (k, v) in c.iter_mut().enumerate() {
        *v = (STOPS[i][k] + (STOPS[i + 1][k] - STOPS[i][k]) * f).round() as u8;
    }
    c
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let at = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[at..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// The palette index of a value: 0 unobserved, else 1–255 over `range`.
pub fn waterfall_index(v: f32, range: (f32, f32)) -> u8 {
    if !v.is_finite() {
        return 0;
    }
    let x = (v - range.0) / (range.1 - range.0);
    1 + (x.clamp(0.0, 1.0) * 254.0).round() as u8
}

/// The default colour range of `stat` over `h`: 2nd percentile to maximum of the observed values
/// (`None` when nothing was observed). At least 1 dB wide.
pub fn waterfall_range(h: &RegionHistory, stat: HistoryStat) -> Option<(f32, f32)> {
    let mut v: Vec<f32> = h
        .cells
        .iter()
        .map(|c| stat.of(c))
        .filter(|x| x.is_finite())
        .collect();
    if v.is_empty() {
        return None;
    }
    let hi = v.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let lo = super::stats::exact_percentile(&mut v, 2.0);
    Some((lo, hi.max(lo + 1.0)))
}

/// Encodes `h` as a PNG waterfall of `stat` (see the [module docs](self)); `range` overrides the
/// colour range, dB/Hz.
pub fn waterfall_png(h: &RegionHistory, stat: HistoryStat, range: Option<(f32, f32)>) -> Vec<u8> {
    let (width, height) = (h.nf.max(1), h.nt.max(1));
    let range = range
        .or_else(|| waterfall_range(h, stat))
        .unwrap_or((0.0, 1.0));
    let mut raw = Vec::with_capacity(height * (width + 1));
    for t in 0..height {
        raw.push(0); // filter: none
        for f in 0..width {
            let v = if t < h.nt && f < h.nf {
                stat.of(h.cell(t, f))
            } else {
                f32::NAN
            };
            raw.push(waterfall_index(v, range));
        }
    }
    // zlib stream of stored deflate blocks.
    let mut z = vec![0x78, 0x01];
    let mut chunks = raw.chunks(65_535).peekable();
    if raw.is_empty() {
        z.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(block) = chunks.next() {
        z.push(u8::from(chunks.peek().is_none()));
        let len = block.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + u32::from(x)) % 65_521;
        b = (b + a) % 65_521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 3, 0, 0, 0]); // 8-bit, palette, deflate, adaptive, no interlace
    png_chunk(&mut out, b"IHDR", &ihdr);
    let mut plte = PNG_UNOBSERVED_RGB.to_vec();
    for i in 1..=255u32 {
        plte.extend_from_slice(&ramp((i - 1) as f32 / 254.0));
    }
    png_chunk(&mut out, b"PLTE", &plte);
    png_chunk(&mut out, b"IDAT", &z);
    png_chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_round_trip() {
        for days in [-1, 0, 59, 365, 11_016, 20_709, 100_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        // 2026-09-13T12:00:00Z
        assert_eq!(
            parse_time_ns("2026-09-13", "12:00:00.250000"),
            Some(1_789_300_800 * NS + 250_000_000)
        );
        let mut s = Vec::new();
        write_time(&mut s, 1_789_300_800 * NS + 250_000_000).unwrap();
        assert_eq!(String::from_utf8(s).unwrap(), "2026-09-13, 12:00:00.250000");
    }

    #[test]
    fn parses_the_hackrf_sweep_line() {
        let r = parse_row(
            "2026-09-13, 12:00:01.5, 88000000, 93000000, 1000000.00, 20, -70.12, -71.3",
            3600,
        )
        .unwrap();
        assert_eq!(r.t_ns, (1_789_300_800 - 3599) * NS + 500_000_000);
        assert_eq!((r.hz_low, r.bin_width, r.db.len()), (88e6, 1e6, 2));
        assert!(parse_row("2026-09-13, 12:00:01, 1, 0, 1, 1, -3", 0).is_none());
        assert!(parse_row("garbage", 0).is_none());
    }
}
