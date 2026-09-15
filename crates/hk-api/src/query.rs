//! JSON for the read-only history endpoints.
//!
//! - `/api/history?f_lo&f_hi&t0&t1[&max_cells][&format][&stat]`: [`Pyramid::query`] (T-017) at the
//!   finest level whose grid over the region fits in `max_cells` cells (default
//!   [`DEFAULT_MAX_CELLS`], at most [`MAX_API_CELLS`]). Cells are row-major (time then frequency),
//!   one array per statistic; unobserved cells are `null` ("not observed" is not "quiet", C26).
//!   T-116: `floor_db`, `coverage_summary`, `scheme`/`tile_format` and richer provenance in JSON;
//!   `format=csv` (hackrf_sweep CSV) or `format=png` (waterfall) of one `stat` ([`history_export`]).
//! - `/api/floor?f_lo&f_hi&t0&t1[&max_steps]`: [`FloorProduct::floor_vs_time`] (T-021) at the
//!   finest level with at most `max_steps` time steps.
//!
//! - `/api/inventory?[f_lo&f_hi][&t0&t1][&state][&status][&tag][&scheme][&family][&cursor][&limit]`:
//!   the T-018 signal inventory ([`inventory_json`]); `state` is the T-078 lifecycle.
//!
//! `f_lo`/`f_hi` are Hz; `t0`/`t1` are Unix seconds.

use hk_model::attention::baseline::SiteKey;
use hk_model::ids::SiteId;
use hk_model::{
    AnnotationAuthor, AnnotationTarget, FreqRange, IdentityAccess, IdentityScheme, InventoryEntry,
    InventoryIdentity, InventoryQuery, KnownStatus, LifecycleAuthor, LifecycleState, RepoError,
    Repository, StatusAuthor, TimeRange, Timestamp,
};
use hk_store::history::{
    FORMAT_VERSION, FilterSummary, FrontEndState, Geometry, HistoryStat, OriginField, OriginFilter,
    waterfall_png, write_sweep_csv,
};
use hk_store::{
    FloorFlags, FloorProduct, FloorVsTime, ProvenanceSummary, Pyramid, RegionHistory, RegionQuery,
    Resolution,
};

/// Decoded query parameters, in request order.
pub type Params = [(String, String)];

fn param<'a>(q: &'a Params, key: &str) -> Option<&'a str> {
    q.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}
use serde_json::{Value, json};

/// Default recent window `/api/analysis/strongest` looks over, s.
pub const DEFAULT_STRONGEST_WINDOW_S: f64 = 5.0;
/// Largest window `/api/analysis/strongest` accepts, s.
pub const MAX_STRONGEST_WINDOW_S: f64 = 300.0;
/// Half-width of the frequency box `/api/analysis/strongest` reports around the strongest cell's
/// centre, Hz (spectrum-history cells carry no per-bin skirt to fit, unlike a live FFT row, so this
/// is fixed rather than measured).
pub const STRONGEST_BOX_HALF_HZ: f64 = 100_000.0;

/// Default cell budget of `/api/history`.
pub const DEFAULT_MAX_CELLS: usize = 100_000;
/// Largest cell budget `/api/history` accepts (bounds response size).
pub const MAX_API_CELLS: usize = 500_000;
/// Cells per line of `/api/history?format=csv` (hackrf_sweep prints ~5 MHz slices).
pub const CSV_CELLS_PER_LINE: usize = 256;
/// Default step budget of `/api/floor`.
pub const DEFAULT_MAX_STEPS: usize = 1024;
/// Largest step budget of `/api/floor`.
pub const MAX_API_STEPS: usize = 20_000;

/// An endpoint error with its HTTP status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiError {
    /// HTTP status.
    pub status: u16,
    /// Message (never echoes request values).
    pub message: String,
}

impl ApiError {
    /// An error.
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

fn bad(message: &str) -> ApiError {
    ApiError::new(400, message)
}

fn num(q: &Params, key: &'static str) -> Result<f64, ApiError> {
    param(q, key)
        .ok_or_else(|| bad(&format!("missing {key}")))?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .ok_or_else(|| bad(&format!("{key} must be a finite number")))
}

fn count(q: &Params, key: &'static str, default: usize, max: usize) -> Result<usize, ApiError> {
    match param(q, key) {
        None => Ok(default),
        Some(v) => v
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=max).contains(n))
            .ok_or_else(|| bad(&format!("{key} must be an integer in 1..={max}"))),
    }
}

/// A validated region × time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Region {
    /// Frequency extent.
    pub freq: FreqRange,
    /// Start, ns.
    pub t0_ns: i64,
    /// End, ns.
    pub t1_ns: i64,
}

/// Parses `f_lo`, `f_hi` (Hz) and `t0`, `t1` (Unix seconds).
pub fn parse_region(q: &Params) -> Result<Region, ApiError> {
    let (f_lo, f_hi) = (num(q, "f_lo")?, num(q, "f_hi")?);
    let (t0, t1) = (num(q, "t0")?, num(q, "t1")?);
    if !(f_lo >= 0.0 && f_hi > f_lo && f_hi <= 1e12) {
        return Err(bad("need 0 <= f_lo < f_hi"));
    }
    // i64 ns covers ±292 years; keep well inside it.
    if !(t1 > t0 && t0 > -4e9 && t1 < 9e9) {
        return Err(bad("need t0 < t1 (Unix seconds)"));
    }
    Ok(Region {
        freq: FreqRange::new(f_lo, f_hi),
        t0_ns: (t0 * 1e9).round() as i64,
        t1_ns: (t1 * 1e9).round() as i64,
    })
}

/// `(nt, nf)` of level `l`'s grid over `r` (the pyramid's own rule).
fn dims(geom: &Geometry, l: usize, r: &Region) -> (f64, f64) {
    let g = &geom.levels[l];
    let nf = ((r.freq.hi_hz / g.f_cell_hz).ceil() - (r.freq.lo_hz / g.f_cell_hz).floor()).max(1.0);
    let t0 = r.t0_ns.div_euclid(g.t_cell_ns);
    let t1 = (r.t1_ns.saturating_add(g.t_cell_ns - 1)).div_euclid(g.t_cell_ns);
    ((t1 - t0).max(1) as f64, nf)
}

/// The finest level whose grid satisfies `fits`, else the top level if it fits `cap` cells.
fn choose_level(
    geom: &Geometry,
    r: &Region,
    fits: impl Fn(f64, f64) -> bool,
) -> Result<u8, ApiError> {
    if let Some(l) = (0..geom.n_levels()).find(|&l| {
        let (nt, nf) = dims(geom, l, r);
        fits(nt, nf)
    }) {
        return Ok(l as u8);
    }
    let top = geom.top();
    let (nt, nf) = dims(geom, top, r);
    if nt * nf <= MAX_API_CELLS as f64 {
        Ok(top as u8)
    } else {
        Err(ApiError::new(
            400,
            "region too large for the cell budget even at the coarsest level",
        ))
    }
}

pub(crate) fn ts_s(t: Timestamp) -> f64 {
    t.as_unix_nanos() as f64 / 1e9
}

fn front_end_state_json(s: &FrontEndState) -> Value {
    json!({
        "gain": s.gain.map(|g| json!({"lna_db": g.lna_db, "vga_db": g.vga_db, "amp_on": g.amp_on})),
        "calibration": s.calibration,
        "gain_table": s.front_end.gain_table,
        "filter": s.front_end.filter.map(|t| t.to_string()),
        "spur_mask": s.front_end.spur_mask,
    })
}

/// A source key as the API spells it: 16 lowercase hex digits (T-133; JSON numbers lose `u64`
/// precision).
pub fn source_text(key: u64) -> String {
    format!("{key:016x}")
}

/// A site as the API spells it: `unassigned`, `mobile` or the site id.
pub fn site_text(site: SiteKey) -> String {
    match site {
        SiteKey::Unassigned => "unassigned".into(),
        SiteKey::Mobile => "mobile".into(),
        SiteKey::Site(id) => id.to_string(),
    }
}

/// Parses the T-133 history filters: `source` (16 hex digits, or `unknown`) and `site`
/// (`unassigned`, `mobile`, a site id, or `unknown`). Absent or empty = no filter on that field.
pub fn parse_origin_filter(q: &Params) -> Result<OriginFilter, ApiError> {
    let get = |k| param(q, k).filter(|v| !v.is_empty());
    let source = match get("source") {
        None => OriginField::Any,
        Some("unknown") => OriginField::Unknown,
        Some(v) if v.len() == 16 && v.bytes().all(|b| b.is_ascii_hexdigit()) => {
            OriginField::Is(u64::from_str_radix(v, 16).map_err(|_| bad("source"))?)
        }
        Some(_) => return Err(bad("source must be a 16-hex-digit source key or unknown")),
    };
    let site = match get("site") {
        None => OriginField::Any,
        Some("unknown") => OriginField::Unknown,
        Some("unassigned") => OriginField::Is(SiteKey::Unassigned),
        Some("mobile") => OriginField::Is(SiteKey::Mobile),
        Some(id) => id
            .parse::<SiteId>()
            .map(|id| OriginField::Is(SiteKey::Site(id)))
            .map_err(|_| bad("site must be unassigned, mobile, a site id or unknown"))?,
    };
    Ok(OriginFilter { source, site })
}

fn field_json<T: Copy>(f: OriginField<T>, text: impl Fn(T) -> String) -> Value {
    match f {
        OriginField::Any => Value::Null,
        OriginField::Unknown => json!("unknown"),
        OriginField::Is(v) => json!(text(v)),
    }
}

/// The JSON of a [`FilterSummary`] (`null` for an unfiltered query).
pub fn filter_json(f: Option<&FilterSummary>) -> Value {
    f.map_or(Value::Null, |f| {
        json!({
            "source": field_json(f.filter.source, source_text),
            "site": field_json(f.filter.site, site_text),
            "tiles_matched": f.tiles_matched,
            "tiles_mixed": f.tiles_mixed,
            "tiles_other": f.tiles_other,
            "cells_excluded": f.cells_excluded,
            "cells_from_children": f.cells_from_children,
        })
    })
}

fn provenance_json(p: &ProvenanceSummary) -> Value {
    let gains: Vec<Value> = p
        .gain_states
        .iter()
        .map(|(g, frames)| {
            json!({"lna_db": g.lna_db, "vga_db": g.vga_db, "amp_on": g.amp_on, "frames": frames})
        })
        .collect();
    let steps: Vec<Value> = p
        .steps
        .iter()
        .map(|s| {
            json!({
                "t_s": ts_s(s.t),
                "changed": s.change_names(),
                "from": front_end_state_json(&s.from),
                "to": front_end_state_json(&s.to),
            })
        })
        .collect();
    json!({
        "frames": p.frames,
        "suspect_fraction": p.suspect_fraction(),
        "dropped_samples": p.dropped_samples,
        "gain_changes": p.gain_changes,
        "gain_states": gains,
        "calibration": p.calibration,
        "calibration_mixed": p.calibration_mixed,
        "gain_table": p.gain_table,
        "gain_table_mixed": p.gain_table_mixed,
        "filter": p.filter.map(|t| t.to_string()),
        "filter_mixed": p.filter_mixed,
        "spur_mask": p.spur_mask,
        "spur_mask_mixed": p.spur_mask_mixed,
        "cell_shape": p.cell_shape,
        "cell_shape_mixed": p.cell_shape_mixed,
        "cell_shapes": p.cell_shapes.iter().map(|(shape, values, frames)| json!({
            "shape": shape,
            "values": values,
            "frames": frames,
        })).collect::<Vec<_>>(),
        "other_shape_values": p.other_shape_values,
        "steps": steps,
        "steps_dropped": p.steps_dropped,
        "first_frame_s": p.first_frame.map(ts_s),
        "last_frame_s": p.last_frame.map(ts_s),
        "origins": p.origins.iter().map(|(o, frames)| json!({
            "source": o.source.map(source_text),
            "site": o.site.map(site_text),
            "frames": frames,
        })).collect::<Vec<_>>(),
        "other_origin_frames": p.other_origin_frames,
    })
}

/// The JSON of one [`RegionHistory`].
pub fn region_history_json(h: &RegionHistory) -> Value {
    let col = |f: fn(&hk_store::CellStats) -> Value| -> Value {
        Value::Array(h.cells.iter().map(f).collect())
    };
    let cov = h.coverage_summary();
    let gaps: Vec<Value> = cov
        .gaps
        .iter()
        .map(|g| json!({"t0_s": ts_s(g.start), "t1_s": ts_s(g.end)}))
        .collect();
    json!({
        "scheme": h.scheme,
        "tile_format": FORMAT_VERSION,
        "level": h.level,
        "unit": h.unit,
        "f_cell_hz": h.f_cell_hz,
        "f_lo_hz": h.f_first_cell as f64 * h.f_cell_hz,
        "nf": h.nf,
        "t_cell_s": h.t_cell_ns as f64 / 1e9,
        "t0_s": ts_s(h.time_of(0)),
        "nt": h.nt,
        "percentiles": [h.percentiles.0, h.percentiles.1],
        "max_db": col(|c| json!(c.max_db)),
        "mean_db": col(|c| json!(c.mean_db)),
        "p_low_db": col(|c| json!(c.p_low_db)),
        "p_high_db": col(|c| json!(c.p_high_db)),
        "occupancy": col(|c| json!(c.occupancy)),
        "occupancy_max": col(|c| json!(c.occupancy_max)),
        "coverage": col(|c| json!(c.coverage)),
        "floor_db": col(|c| json!(c.floor_db)),
        "frames": col(|c| json!(c.frames)),
        "coverage_summary": {
            "cells": cov.cells,
            "observed_cells": cov.observed_cells,
            "observed_fraction": cov.observed_fraction,
            "gaps": gaps,
            "gaps_truncated": cov.gaps_truncated,
        },
        "provenance": provenance_json(&h.provenance),
        "tiles_read": h.tiles_read,
        "filter": filter_json(h.filter.as_ref()),
    })
}

/// `/api/history`.
pub fn history_json(p: &Pyramid, q: &Params) -> Result<Value, ApiError> {
    Ok(region_history_json(&region_history(p, q)?))
}

/// `/api/history?format=csv|png[&stat]` (T-116): `Ok(None)` when the format is JSON (the default),
/// else `(content type, body)`: the grid as `hackrf_sweep` CSV of `stat` (default `mean`, per-bin
/// dB, UTC, [`CSV_CELLS_PER_LINE`] cells per line) or a PNG waterfall of `stat` (default `max`;
/// grey = not observed).
pub fn history_export(
    p: &Pyramid,
    q: &Params,
) -> Result<Option<(&'static str, Vec<u8>)>, ApiError> {
    let Some(format) = history_format(q)? else {
        return Ok(None);
    };
    let stat = match param(q, "stat") {
        None if format == "csv" => HistoryStat::Mean,
        None => HistoryStat::Max,
        Some(s) => HistoryStat::parse(s)
            .ok_or_else(|| bad("stat must be max, mean, p_low, p_high or floor"))?,
    };
    let h = region_history(p, q)?;
    if format == "csv" {
        let mut out = Vec::new();
        write_sweep_csv(&h, stat, CSV_CELLS_PER_LINE, &mut out)
            .map_err(|_| ApiError::new(500, "csv export failed"))?;
        Ok(Some(("text/csv; charset=utf-8", out)))
    } else {
        Ok(Some(("image/png", waterfall_png(&h, stat, None))))
    }
}

/// The `format` of a `/api/history` request: `None` for JSON, else `csv` or `png`.
pub fn history_format(q: &Params) -> Result<Option<&'static str>, ApiError> {
    match param(q, "format") {
        None | Some("json") => Ok(None),
        Some("csv") => Ok(Some("csv")),
        Some("png") => Ok(Some("png")),
        Some(_) => Err(bad("format must be json, csv or png")),
    }
}

fn region_history(p: &Pyramid, q: &Params) -> Result<RegionHistory, ApiError> {
    let r = parse_region(q)?;
    let max_cells = count(q, "max_cells", DEFAULT_MAX_CELLS, MAX_API_CELLS)?;
    let filter = parse_origin_filter(q)?;
    let level = choose_level(p.geometry(), &r, |nt, nf| nt * nf <= max_cells as f64)?;
    let h = p
        .query_filtered(
            &RegionQuery {
                freq: r.freq,
                time: TimeRange::new(
                    Timestamp::from_unix_nanos(r.t0_ns),
                    Timestamp::from_unix_nanos(r.t1_ns),
                ),
                resolution: Resolution::Level(level),
            },
            &filter,
        )
        .map_err(|_| ApiError::new(400, "history query refused"))?;
    Ok(h)
}

const FLAG_NAMES: [(FloorFlags, &str); 14] = [
    (FloorFlags::UNCALIBRATED, "uncalibrated"),
    (FloorFlags::QUANTISATION_LIMITED, "quantisation-limited"),
    (FloorFlags::IMPULSIVE_PAUSED, "impulsive-paused"),
    (FloorFlags::GATE_RELEASED, "gate-released"),
    (FloorFlags::SEGMENT_START, "segment-start"),
    (FloorFlags::NOT_READY, "not-ready"),
    (FloorFlags::INVALID, "invalid"),
    (FloorFlags::EPISODE, "episode"),
    (FloorFlags::CAL_EDGE_HELD, "cal-edge-held"),
    (FloorFlags::GAP_BEFORE, "gap-before"),
    (FloorFlags::GAP, "gap"),
    (FloorFlags::MIXED_GAIN, "mixed-gain"),
    (FloorFlags::MIXED_CALIBRATION, "mixed-calibration"),
    (FloorFlags::PARTLY_UNCALIBRATED, "partly-uncalibrated"),
];

/// The JSON of one [`FloorVsTime`].
pub fn floor_vs_time_json(f: &FloorVsTime) -> Value {
    let steps: Vec<Value> = f
        .steps
        .iter()
        .map(|s| {
            let names: Vec<&str> = FLAG_NAMES
                .iter()
                .filter(|(flag, _)| s.flags.contains(*flag))
                .map(|(_, n)| *n)
                .collect();
            json!({
                "t_s": ts_s(s.t),
                "duration_s": s.duration_ns as f64 / 1e9,
                "unit": s.unit,
                "value_db_per_hz": s.value_db_per_hz,
                "raw_p_low_db_per_hz": s.raw_p_low_db_per_hz,
                "bias_db": s.bias_db,
                "mean_db_per_hz": s.mean_db_per_hz,
                "noise_temperature_k": s.noise_temperature_k(),
                "uncertainty_db": s.uncertainty_db,
                "model_uncertainty_db": s.model_uncertainty_db,
                "calibration_uncertainty_db": s.calibration_uncertainty_db,
                "histogram_uncertainty_db": s.histogram_uncertainty_db,
                "statistical_uncertainty_db": s.statistical_uncertainty_db,
                "cells": s.cells,
                "coverage": s.coverage,
                "level": s.level,
                "gain_states": s.gain_states,
                "flags": s.flags.bits(),
                "flag_names": names,
            })
        })
        .collect();
    json!({
        "region": {"lo_hz": f.region.lo_hz, "hi_hz": f.region.hi_hz},
        "level": f.level,
        "t_cell_s": f.t_cell_ns as f64 / 1e9,
        "shape": f.shape,
        "steps": steps,
        "calibrated_provenance": provenance_json(&f.calibrated_provenance),
        "uncalibrated_provenance": provenance_json(&f.uncalibrated_provenance),
    })
}

/// `/api/floor`.
pub fn floor_json(product: &FloorProduct, q: &Params) -> Result<Value, ApiError> {
    let r = parse_region(q)?;
    let max_steps = count(q, "max_steps", DEFAULT_MAX_STEPS, MAX_API_STEPS)?;
    let geom = product.calibrated_pyramid().geometry();
    let level = choose_level(geom, &r, |nt, nf| {
        nt <= max_steps as f64 && nt * nf <= MAX_API_CELLS as f64
    })?;
    let f = product
        .floor_vs_time(
            r.freq,
            Timestamp::from_unix_nanos(r.t0_ns),
            Timestamp::from_unix_nanos(r.t1_ns),
            Resolution::Level(level),
        )
        .map_err(|_| ApiError::new(400, "floor query refused"))?;
    Ok(floor_vs_time_json(&f))
}

/// `/api/analysis/strongest?f_lo&f_hi[&window_s]`: the strongest observed signal (max-hold dB/Hz)
/// in `[f_lo, f_hi)` over the last `window_s` seconds (default [`DEFAULT_STRONGEST_WINDOW_S`], at
/// most [`MAX_STRONGEST_WINDOW_S`]), read from the spectrum-history pyramid ([`Pyramid::query`]).
///
/// Backend replacement (T-079) for client-side peak-picking over a locally held spectrum row: the
/// UI no longer inspects raw FFT bins itself, only asks what the strongest thing in view is. Unlike
/// a live row, history cells carry no per-bin skirt to fit a box to, so the reported box is a fixed
/// [`STRONGEST_BOX_HALF_HZ`] half-width around the strongest cell's centre, clamped to
/// `[f_lo, f_hi)`. `{"found": false}` when nothing was observed in the window.
pub fn strongest_json(p: &Pyramid, q: &Params, now: Timestamp) -> Result<Value, ApiError> {
    let (f_lo, f_hi) = (num(q, "f_lo")?, num(q, "f_hi")?);
    if !(f_lo >= 0.0 && f_hi > f_lo && f_hi <= 1e12) {
        return Err(bad("need 0 <= f_lo < f_hi"));
    }
    let window_s = match param(q, "window_s") {
        None => DEFAULT_STRONGEST_WINDOW_S,
        Some(v) => v
            .parse::<f64>()
            .ok()
            .filter(|w| w.is_finite() && *w > 0.0 && *w <= MAX_STRONGEST_WINDOW_S)
            .ok_or_else(|| bad("window_s must be a finite number of seconds in (0, 300]"))?,
    };
    let t1_ns = now.as_unix_nanos();
    let t0_ns = t1_ns - (window_s * 1e9) as i64;
    let r = Region {
        freq: FreqRange::new(f_lo, f_hi),
        t0_ns,
        t1_ns,
    };
    let level = choose_level(p.geometry(), &r, |nt, nf| {
        nt * nf <= DEFAULT_MAX_CELLS as f64
    })?;
    let h = p
        .query(&RegionQuery {
            freq: r.freq,
            time: TimeRange::new(
                Timestamp::from_unix_nanos(t0_ns),
                Timestamp::from_unix_nanos(t1_ns),
            ),
            resolution: Resolution::Level(level),
        })
        .map_err(|_| ApiError::new(400, "history query refused"))?;
    let mut best: Option<(f32, usize)> = None;
    for (i, c) in h.cells.iter().enumerate() {
        if c.observed() && best.is_none_or(|(bv, _)| c.max_db > bv) {
            best = Some((c.max_db, i % h.nf));
        }
    }
    Ok(match best {
        None => json!({ "found": false }),
        Some((max_db, f)) => {
            let freq = h.freq_of(f);
            let center = 0.5 * (freq.lo_hz + freq.hi_hz);
            let lo = (center - STRONGEST_BOX_HALF_HZ).max(f_lo);
            let hi = (center + STRONGEST_BOX_HALF_HZ).min(f_hi);
            json!({
                "found": true,
                "f_center_hz": center,
                "f_lo_hz": lo,
                "f_hi_hz": hi,
                "max_db": max_db,
            })
        }
    })
}

/// Default page size of `/api/inventory`.
pub const DEFAULT_INVENTORY_LIMIT: usize = 100;
/// Largest page size `/api/inventory` accepts.
pub const MAX_API_INVENTORY_LIMIT: usize = 500;
/// Largest `/api/inventory` cursor (row offset) accepted.
pub const MAX_INVENTORY_CURSOR: u64 = 1_000_000;
/// Longest `tag` / `family` filter accepted, bytes.
const MAX_FILTER_LEN: usize = 128;

fn nonempty<'a>(q: &'a Params, key: &str) -> Option<&'a str> {
    param(q, key).filter(|v| !v.is_empty())
}

/// Both of an optional pair of numbers, or neither.
fn optional_pair(
    q: &Params,
    a: &'static str,
    b: &'static str,
) -> Result<Option<(f64, f64)>, ApiError> {
    match (nonempty(q, a), nonempty(q, b)) {
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Ok(Some((num(q, a)?, num(q, b)?))),
        _ => Err(bad(&format!("{a} and {b} must be given together"))),
    }
}

fn short_text(q: &Params, key: &'static str) -> Result<Option<String>, ApiError> {
    match nonempty(q, key) {
        Some(v) if v.len() > MAX_FILTER_LEN => Err(bad(&format!("{key} is too long"))),
        v => Ok(v.map(str::to_owned)),
    }
}

/// Parses the `/api/inventory` filters into an [`InventoryQuery`]. Identity access is always
/// [`IdentityAccess::Standard`]: no request parameter can grant the own-traffic authorisation.
pub fn parse_inventory_query(q: &Params) -> Result<InventoryQuery, ApiError> {
    let freq = match optional_pair(q, "f_lo", "f_hi")? {
        None => None,
        Some((lo, hi)) if lo >= 0.0 && hi > lo && hi <= 1e12 => Some(FreqRange::new(lo, hi)),
        Some(_) => return Err(bad("need 0 <= f_lo < f_hi")),
    };
    let time = match optional_pair(q, "t0", "t1")? {
        None => None,
        Some((t0, t1)) if t1 >= t0 && t0 > -4e9 && t1 < 9e9 => Some(TimeRange::new(
            Timestamp::from_unix_nanos((t0 * 1e9).round() as i64),
            Timestamp::from_unix_nanos((t1 * 1e9).round() as i64),
        )),
        Some(_) => return Err(bad("need t0 <= t1 (Unix seconds)")),
    };
    let mut status = Vec::new();
    for s in nonempty(q, "status").into_iter().flat_map(|v| v.split(',')) {
        let parsed: KnownStatus = serde_json::from_value(Value::String(s.trim().to_owned()))
            .map_err(|_| bad("status must be known, unexpected-here or unknown"))?;
        if !status.contains(&parsed) {
            status.push(parsed);
        }
    }
    let mut states = Vec::new();
    for s in nonempty(q, "state").into_iter().flat_map(|v| v.split(',')) {
        let parsed: LifecycleState = serde_json::from_value(Value::String(s.trim().to_owned()))
            .map_err(|_| bad("state must be candidate, confirmed or deleted"))?;
        if !states.contains(&parsed) {
            states.push(parsed);
        }
    }
    let identity_scheme = nonempty(q, "scheme")
        .map(|s| s.parse::<IdentityScheme>())
        .transpose()
        .map_err(|_| bad("unknown identity scheme"))?;
    let limit = count(q, "limit", DEFAULT_INVENTORY_LIMIT, MAX_API_INVENTORY_LIMIT)?;
    let offset = match nonempty(q, "cursor") {
        None => 0,
        Some(c) => c
            .parse::<u64>()
            .ok()
            .filter(|&o| o <= MAX_INVENTORY_CURSOR)
            .ok_or_else(|| bad("invalid cursor"))?,
    };
    Ok(InventoryQuery {
        freq,
        time,
        status,
        states,
        tag: short_text(q, "tag")?,
        identity_scheme,
        family: short_text(q, "family")?,
        limit: limit as u32,
        offset,
        access: IdentityAccess::Standard,
    })
}

/// `author_ref` of the Classifier annotations holding ranked explanations: must equal
/// `hk_pipeline::family::FAMILY_MAP_VERSION` (hk-api does not depend on hk-pipeline; the e2e
/// acceptance suite checks the two agree). Only these are served.
pub const EXPLANATIONS_AUTHOR_REF: &str = "hk-pipeline/family-map@1";

/// The latest ranked explanations stored on emitter `id`, or `[]`. Served only from Classifier
/// annotations by the family map ([`EXPLANATIONS_AUTHOR_REF`]) with no content.
fn explanations_json(repo: &Repository, id: hk_model::EmitterId) -> Result<Value, RepoError> {
    Ok(repo
        .annotations_for(&AnnotationTarget::Emitter(id))?
        .into_iter()
        .rfind(|a| {
            a.author == AnnotationAuthor::Classifier
                && a.author_ref == EXPLANATIONS_AUTHOR_REF
                && a.content.is_none()
                && a.metadata.get("explanations").is_some()
        })
        .map_or_else(|| json!([]), |a| a.metadata["explanations"].clone()))
}

/// Authors whose status reasons are computed without an identity value (priors see family and
/// frequency; the clusterer's reasons are fixed strings; classifiers see features). Reasons by
/// any other author (decoder, user, system) are withheld on rows whose identity is withheld.
fn reason_is_identity_free(author: StatusAuthor) -> bool {
    matches!(
        author,
        StatusAuthor::Prior | StatusAuthor::Clusterer | StatusAuthor::Classifier
    )
}

/// `/api/inventory`: one page of [`Repository::query_inventory`] (T-018), most recently seen
/// first.
///
/// Each entry carries id, frequency extent, first/last seen, count, current known status (with
/// its latest reason, author and prior reference), tags, family and latest classification, and the
/// identity: `identity_scheme` and `identity_class` always, `identity_value` **only** when the
/// query returned it in clear, and `withheld: true` when gating withheld it. On withheld rows a
/// status reason written by an author that may have seen the identity is withheld too
/// (`status.reason_withheld`). Tags are gated with the identity (T-036/T-038): a withheld row
/// lists only controlled-vocabulary labels (`hk_model::TAG_VOCABULARY`), `tags_withheld: true` when
/// others were removed, and a `tag` filter outside the vocabulary never matches it. No decode content, fingerprint or link is included.
/// `explanations` lists the emitter's ranked T-039 explanations (`hk_pipeline::family`), best first, or `[]`.
///
/// T-158: `snr_db` and `peak_dbfs` are the newest (highest `t_start`) detection linked to the
/// emitter's `snr_peak_db` and `peak_level_dbfs` — directly, or through one of its
/// currently-linked tracks ([`Repository::emitter_latest_measurement`]); both `null` when no
/// detection is linked yet (e.g. an emitter seen only through a decode sighting).
///
/// T-078: `state` filters by lifecycle (`candidate`, `confirmed`, `deleted`, comma-separated; by
/// default candidates and confirmed entries, never deleted ones). Each row carries `state`,
/// `lifecycle` (the latest change: state, previous, author `auto`/`user`, actor, reason, `t_s`; or
/// `null` for an untouched candidate; a user's reason is withheld on withheld-identity rows) and
/// `recurrence` (occurrences, appearances, span, on-air time, duty cycle and the
/// [`RECENT_APPEARANCES`] latest appearances).
///
/// T-171: `total` is [`Repository::count_inventory`] — the number of rows the same filters match,
/// ignoring `cursor`/`limit`, so a caller can show a count past one page. It is an efficient
/// indexed `COUNT(*)` for every filter but a `tag` outside `hk_model::TAG_VOCABULARY`, which is
/// capped (see `count_inventory`'s docs) and can then read as a lower bound.
pub fn inventory_json(repo: &Repository, q: &Params) -> Result<Value, ApiError> {
    let query = parse_inventory_query(q)?;
    let failed = |_| ApiError::new(500, "inventory query failed");
    let page = repo.query_inventory(&query).map_err(failed)?;
    let total = repo.count_inventory(&query).map_err(failed)?;
    let mut entries = Vec::with_capacity(page.entries.len());
    for entry in &page.entries {
        entries.push(inventory_entry_json(repo, entry).map_err(failed)?);
    }
    Ok(json!({
        "entries": entries,
        "next_cursor": page
            .next_offset
            .filter(|&o| o <= MAX_INVENTORY_CURSOR)
            .map(|o| o.to_string()),
        "limit": query.limit,
        "total": total,
        "identity_access": "standard",
    }))
}

/// Latest appearances listed per `/api/inventory` row.
pub const RECENT_APPEARANCES: usize = 8;

/// One `/api/inventory` row (see [`inventory_json`]).
/// T-191 `user_band` object: edges (Hz), `set_at` (Unix s), actor (token fingerprint) and the
/// user's reason, withheld (`reason: null`, `reason_withheld: true`) on a withheld-identity row
/// like any user-authored reason.
pub(crate) fn user_band_json(b: &hk_model::UserBand, withheld: bool) -> Value {
    json!({
        "f_lo": b.f_lo_hz,
        "f_hi": b.f_hi_hz,
        "set_at": ts_s(b.set_at),
        "actor": b.actor,
        "reason": if withheld { None } else { b.reason.as_deref() },
        "reason_withheld": withheld && b.reason.is_some(),
    })
}

/// T-211 inventory `classification` object: the legacy fields, then the ADR-0016 fields. `stage`
/// and `arb_rank` are stored, or derived for a pre-M3 row; `taxonomy`, `coarse`, `class`, `top`
/// (≤ 5 posterior labels, `unknown` included), `entropy_norm` and `flags` are `null` on a pre-M3
/// row. Never the input link, features or reasons (the per-emitter classification route, T-199).
fn classification_json(r: &hk_model::RecordedClassification) -> Value {
    let c = &r.classification;
    let d = r.detail.as_ref();
    json!({
        "family": c.family,
        "confidence": c.confidence,
        "open_set_score": c.open_set_score,
        "model_version": c.model_version,
        "t_s": ts_s(c.t),
        "taxonomy": r.taxonomy.as_ref().map(ToString::to_string),
        "stage": r.stage,
        "arb_rank": r.arb_rank,
        "coarse": d.map(|d| d.coarse),
        "class": d.and_then(|d| d.class.as_ref()).map(|k| json!({
            "label": k.label,
            "p": k.p,
            "stage": k.stage,
        })),
        "top": d.map(|d| d.top(5)),
        "entropy_norm": d.map(|d| d.entropy_norm),
        "flags": d.map(|d| &d.flags),
    })
}

pub fn inventory_entry_json(repo: &Repository, entry: &InventoryEntry) -> Result<Value, RepoError> {
    {
        let e = &entry.emitter;
        let (scheme, value, class, withheld) = match &entry.identity {
            InventoryIdentity::None => (None, None, None, false),
            InventoryIdentity::Clear { identity, class } => (
                Some(identity.scheme.as_string()),
                Some(identity.value.as_str()),
                Some(*class),
                false,
            ),
            InventoryIdentity::Withheld { scheme, class } => {
                (Some(scheme.as_string()), None, *class, true)
            }
        };
        let status = repo.known_status_history(e.id)?.last().map(|c| {
            let show = !withheld || reason_is_identity_free(c.author);
            json!({
                "status": c.status,
                "author": c.author,
                "t_s": ts_s(c.t),
                "reason": show.then_some(c.reason.as_str()),
                "prior_ref": if show { c.prior_ref.as_deref() } else { None },
                "reason_withheld": !show,
            })
        });
        // T-039 ranked explanations: metadata only (service labels, scores, band-plan and raster
        // evidence, flags; no identity or content), so withheld rows show them too.
        let explanations = explanations_json(repo, e.id)?;
        // T-070: the latest output-driven refinement (centre, bandwidth, mode parameters,
        // objective value, search statistics; metadata only), or null. Detected values stay in
        // `f_center_hz` / `bandwidth_hz`.
        let refined = repo.refined_tuning(e.id)?.map(|r| {
            let mut v = serde_json::to_value(&r).unwrap_or(Value::Null);
            if let Some(o) = v.as_object_mut() {
                o.insert("t_s".into(), json!(ts_s(r.t)));
            }
            v
        });
        let lifecycle = repo.emitter_lifecycle_history(e.id)?.pop().map(|c| {
            let show = !withheld || c.author == LifecycleAuthor::Auto;
            json!({
                "state": c.state,
                "previous": c.previous,
                "author": c.author,
                "actor": c.actor,
                "t_s": ts_s(c.t),
                "reason": show.then_some(c.reason.as_str()),
                "reason_withheld": !show,
            })
        });
        // T-158: the newest linked detection's peak SNR and absolute peak level, or `null` when
        // the emitter has no linked detection yet (e.g. an identity-only sighting).
        let measurement = repo.emitter_latest_measurement(e.id)?;
        // T-191: the user-adjusted band, beside (never replacing) the measured f_lo_hz/f_hi_hz.
        let user_band = repo.user_band(e.id)?.map(|b| user_band_json(&b, withheld));
        let rec = repo.emitter_recurrence(e.id, RECENT_APPEARANCES)?;
        let recurrence = json!({
            "occurrences": rec.occurrences,
            "appearances": rec.appearances,
            "span_s": rec.span_s,
            "on_air_s": rec.on_air_s,
            "duty_cycle": rec.duty_cycle,
            "recent": rec.recent.iter().map(|a| json!({
                "t_start_s": ts_s(a.time.start),
                "t_end_s": ts_s(a.time.end),
                "count": a.count,
                "duty_cycle": a.duty_cycle,
            })).collect::<Vec<_>>(),
        });
        let current = repo.current_classification(e.id)?;
        let latest_classification = match (&current, repo.latest_classification(e.id)?) {
            (Some(c), Some(l)) if *c != l => Some(classification_json(&l)),
            _ => None,
        };
        let freq = e.freq();
        let mut row = json!({
            "state": entry.lifecycle,
            "lifecycle": lifecycle,
            "recurrence": recurrence,
            "explanations": explanations,
            "refined": refined,
            "user_band": user_band,
            "id": e.id.to_string(),
            "f_center_hz": e.f_center_hz,
            "bandwidth_hz": e.bandwidth_hz,
            "f_lo_hz": freq.lo_hz,
            "f_hi_hz": freq.hi_hz,
            "first_seen_s": ts_s(e.first_seen),
            "last_seen_s": ts_s(e.last_seen),
            "count": e.count,
            "known_status": e.known_status,
            "status": status,
            "tags": e.tags,
            "tags_withheld": entry.tags_withheld,
            "family": entry.family,
            // T-211 (ADR-0016 §2): the classification that sets `family` (arbitration rank),
            // and the latest row only when a newer, lower-ranked row differs from it.
            "classification": current.as_ref().map(classification_json),
            "latest_classification": latest_classification,
            "classifications": e.classifications.len(),
            "identity_scheme": scheme,
            "identity_class": class,
            "withheld": withheld,
            "snr_db": measurement.map(|(snr, _)| snr),
            "peak_dbfs": measurement.map(|(_, peak)| peak),
        });
        if let Some(v) = value {
            row["identity_value"] = json!(v);
        }
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn getter(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// T-133: `source`/`site` history filters.
    #[test]
    fn origin_filter_parsing() {
        let f = |pairs: &[(&str, &str)]| parse_origin_filter(&getter(pairs));
        assert_eq!(f(&[]).unwrap(), OriginFilter::ANY);
        assert_eq!(
            f(&[("site", ""), ("source", "")]).unwrap(),
            OriginFilter::ANY
        );
        let id = SiteId::new();
        let got = f(&[("source", "8A1f0c3b5d2e4f60"), ("site", &id.to_string())]).unwrap();
        assert_eq!(got.source, OriginField::Is(0x8a1f_0c3b_5d2e_4f60));
        assert_eq!(got.site, OriginField::Is(SiteKey::Site(id)));
        assert_eq!(source_text(0x8a1f_0c3b_5d2e_4f60), "8a1f0c3b5d2e4f60");
        let got = f(&[("source", "unknown"), ("site", "unknown")]).unwrap();
        assert_eq!(
            (got.source, got.site),
            (OriginField::Unknown, OriginField::Unknown)
        );
        assert_eq!(
            f(&[("site", "mobile")]).unwrap().site,
            OriginField::Is(SiteKey::Mobile)
        );
        assert_eq!(
            f(&[("site", "unassigned")]).unwrap().site,
            OriginField::Is(SiteKey::Unassigned)
        );
        for bad in [
            ("source", "123"),
            ("source", "+123456789abcdef"),
            ("source", "0123456789abcdefg"),
            ("site", "nowhere"),
        ] {
            assert_eq!(f(&[bad]).unwrap_err().status, 400, "{bad:?}");
        }
    }

    #[test]
    fn region_validation() {
        let ok = [
            ("f_lo", "1e8"),
            ("f_hi", "1.01e8"),
            ("t0", "1789300800"),
            ("t1", "1789300900.5"),
        ];
        let r = parse_region(&getter(&ok)).unwrap();
        assert_eq!(r.t1_ns, 1_789_300_900_500_000_000);
        for bad_pairs in [
            &[("f_lo", "2"), ("f_hi", "1"), ("t0", "0"), ("t1", "1")][..],
            &[("f_lo", "1"), ("f_hi", "2"), ("t0", "5"), ("t1", "1")][..],
            &[("f_lo", "NaN"), ("f_hi", "2"), ("t0", "0"), ("t1", "1")][..],
            &[("f_hi", "2"), ("t0", "0"), ("t1", "1")][..],
        ] {
            assert_eq!(parse_region(&getter(bad_pairs)).unwrap_err().status, 400);
        }
        assert!(count(&getter(&[("max_cells", "0")]), "max_cells", 1, 10).is_err());
        assert_eq!(count(&getter(&[]), "max_cells", 7, 10).unwrap(), 7);
    }
}
