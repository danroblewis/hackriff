//! JSON for the read-only history endpoints.
//!
//! - `/api/history?f_lo&f_hi&t0&t1[&max_cells][&max_t][&max_f][&format][&stat]`:
//!   [`Pyramid::query`] (T-017) at the finest level whose grid over the region fits in `max_cells`
//!   cells (default [`DEFAULT_MAX_CELLS`], at most [`MAX_API_CELLS`]) and, when given, `max_t`
//!   time cells and `max_f` frequency cells — the view's own rows and columns (T-334). The whole
//!   requested span is always covered: zooming re-scales the grid, it never truncates the range.
//!   The `resolution` block reports what was actually served ([`ResolutionRequest`]). Cells are
//!   row-major (time then frequency),
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
    AnnotationAuthor, AnnotationTarget, ArtifactKind, Demodulation, FreqRange, IdentityAccess,
    IdentityScheme, InventoryEntry, InventoryIdentity, InventoryQuery, KnownStatus,
    LifecycleAuthor, LifecycleState, Presence, RelationVisibility, RepoError, Repository,
    StatusAuthor, TimeRange, Timestamp, cluster_label,
};
use hk_store::history::{
    CoverageSummary, FORMAT_VERSION, FilterSummary, FrontEndState, Geometry, HistoryStat,
    OriginField, OriginFilter, waterfall_png, write_sweep_csv,
};
use hk_store::{
    FloorFlags, FloorProduct, FloorVsTime, ProvenanceSummary, Pyramid, RegionHistory, RegionQuery,
    Resolution,
};

use crate::coverage::ObservedCoverage;

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

pub(crate) fn bad(message: &str) -> ApiError {
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

pub(crate) fn count(
    q: &Params,
    key: &'static str,
    default: usize,
    max: usize,
) -> Result<usize, ApiError> {
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

/// Parses `f_lo`/`f_hi` (Hz) alone, for a route whose **time** extent is not the caller's to give
/// (T-338). `None` when neither is present; an error when only one is, or when they do not order.
pub(crate) fn parse_freq_only(q: &Params) -> Result<Option<FreqRange>, ApiError> {
    match (param(q, "f_lo"), param(q, "f_hi")) {
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => Err(bad("give f_lo and f_hi together, or neither")),
        (Some(_), Some(_)) => {
            let (f_lo, f_hi) = (num(q, "f_lo")?, num(q, "f_hi")?);
            if !(f_lo >= 0.0 && f_hi > f_lo && f_hi <= 1e12) {
                return Err(bad("need 0 <= f_lo < f_hi"));
            }
            Ok(Some(FreqRange::new(f_lo, f_hi)))
        }
    }
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

/// The resolution budgets one `/api/history` request asked for (T-334).
///
/// `max_cells` bounds the response's size (the product); `max_t`/`max_f` are the **view's own**
/// budgets — the rows and columns it will draw — so a span can be asked for in the terms the view
/// has rather than as a product that a wrongly-shaped grid can satisfy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolutionRequest {
    /// `max_cells`: cells in the whole grid.
    pub max_cells: usize,
    /// `max_t`: time cells (the view's rows), when asked for.
    pub max_t: Option<usize>,
    /// `max_f`: frequency cells (the view's columns), when asked for.
    pub max_f: Option<usize>,
}

/// Largest per-axis budget `/api/history` accepts. A view cannot draw more rows or columns than
/// this, and a budget above it would only ever be met by [`MAX_API_CELLS`] instead.
pub const MAX_API_AXIS_CELLS: usize = MAX_API_CELLS;

/// The `resolution` block of `/api/history` (T-334): what was asked for, what was served, and —
/// when they differ — on which axis, so a client never has to infer it from the grid.
///
/// The ladder is discrete (scheme 1: 6.25 kHz × 1 s at level 0, each step ×2 in frequency and
/// ×60/×15/×4/×24/×7 in time), so an exact match is not generally reachable. The rule is **the
/// finest level that fits every budget**, which errs *coarser* than the view, never finer. That
/// direction is deliberate: a coarser cell drawn across several pixels repeats one measured value
/// (honest, if blocky), whereas a finer grid reduced in the client invents the value a pixel
/// stands for — a measurement made with no knowledge of the floor or of what a peak means.
/// `over_resolved` names any budget the served grid still exceeds, which is the only case in which
/// the client holds more cells than it can draw one-to-one.
///
/// # `source`: live-IQ detail or overview (T-341)
///
/// T-334 shipped `source` as the constant `"spectrum-history"`, documented as the home for "which
/// tier answered". T-341 makes it a [`DetailSource`], because the user's navigation invariant needs
/// the view to tell **live-IQ-backed detail from survey/spectrum-history overview**.
///
/// `/api/history` reads the pyramid and only the pyramid, so it never claims `live-iq`. What it can
/// say is whether the grid it served could have come from **one capture window** at all: a span
/// wider than the widest instantaneous bandwidth (`max_live_span_hz`) was necessarily stitched from
/// separate dwells, so it is `survey-overview`. The span compared is the one actually served
/// (`nf · f_cell_hz`), not the one requested — the claim is about the picture drawn.
///
/// With no live front end to ask (`max_live_span_hz` is `None`) the answer is `survey-overview`:
/// the weaker claim, because not knowing the window is not evidence that the span fits inside it.
fn resolution_json(
    h: &RegionHistory,
    req: &ResolutionRequest,
    levels: usize,
    max_live_span_hz: Option<f64>,
) -> Value {
    let cells = h.nt.saturating_mul(h.nf);
    let mut over: Vec<&str> = Vec::new();
    if cells > req.max_cells {
        over.push("max_cells");
    }
    if req.max_t.is_some_and(|t| h.nt > t) {
        over.push("max_t");
    }
    if req.max_f.is_some_and(|f| h.nf > f) {
        over.push("max_f");
    }
    // Which tier answered, as a detail claim (T-334's field, T-341's enum). Never `live-iq` here:
    // this reads the pyramid. `survey-overview` when the served span could not have fitted one
    // capture window.
    let served_span_hz = h.nf as f64 * h.f_cell_hz;
    let source = match crate::navigation::live_window_verdict(served_span_hz, max_live_span_hz) {
        crate::navigation::DetailSource::LiveIq => crate::navigation::DetailSource::SpectrumHistory,
        other => other,
    };
    json!({
        "source": source.as_str(),
        "live": source.is_live(),
        "statement": source.statement(),
        "served_span_hz": served_span_hz,
        "max_live_span_hz": max_live_span_hz,
        "level": h.level,
        "levels": levels,
        "t_cell_s": h.t_cell_ns as f64 / 1e9,
        "f_cell_hz": h.f_cell_hz,
        "requested": {
            "max_cells": req.max_cells,
            "max_t": req.max_t,
            "max_f": req.max_f,
        },
        "served": { "nt": h.nt, "nf": h.nf, "cells": cells },
        "matched": over.is_empty(),
        "over_resolved": over,
    })
}

/// `/api/history`.
///
/// `max_live_span_hz` is the widest instantaneous bandwidth the run's front end can produce
/// ([`hk_core::SourceCapabilities::max_live_span_hz`]), or `None` when there is no live front end
/// to ask. It decides `resolution.source` only (T-341): a grid wider than one capture window is
/// `survey-overview`, and without the figure the weaker claim is made rather than the stronger.
pub fn history_json(
    p: &Pyramid,
    q: &Params,
    max_live_span_hz: Option<f64>,
) -> Result<Value, ApiError> {
    let (h, req) = region_history(p, q)?;
    let mut v = region_history_json(&h);
    let levels = p.geometry().n_levels();
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "resolution".into(),
            resolution_json(&h, &req, levels, max_live_span_hz),
        );
    }
    Ok(v)
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
    let (h, _) = region_history(p, q)?;
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

fn region_history(p: &Pyramid, q: &Params) -> Result<(RegionHistory, ResolutionRequest), ApiError> {
    let r = parse_region(q)?;
    let max_cells = count(q, "max_cells", DEFAULT_MAX_CELLS, MAX_API_CELLS)?;
    // T-334: per-axis budgets, in the terms a view has (its rows and its columns). Absent, the
    // product budget alone chooses the level exactly as before.
    let axis = |key: &'static str| -> Result<Option<usize>, ApiError> {
        match param(q, key) {
            None => Ok(None),
            Some(_) => count(q, key, 1, MAX_API_AXIS_CELLS).map(Some),
        }
    };
    let (max_t, max_f) = (axis("max_t")?, axis("max_f")?);
    let req = ResolutionRequest {
        max_cells,
        max_t,
        max_f,
    };
    let filter = parse_origin_filter(q)?;
    let level = choose_level(p.geometry(), &r, |nt, nf| {
        nt * nf <= max_cells as f64
            && max_t.is_none_or(|t| nt <= t as f64)
            && max_f.is_none_or(|f| nf <= f as f64)
    })?;
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
    Ok((h, req))
}

/// One timeline overview read (T-338): the compressed grid, and the pyramid tier it came from.
pub(crate) struct OverviewRead {
    /// The grid, laid on the requested window rather than on the tier's cell boundaries.
    pub grid: hk_store::Overview,
    /// Pyramid level read.
    pub level: u8,
    /// That level's time cell, s.
    pub src_t_cell_s: f64,
    /// That level's frequency cell, Hz.
    pub src_f_cell_hz: f64,
}

/// The fold every band-collapsed series on this API is made with, stated once and served verbatim
/// (T-342).
///
/// The rule is not "a max happens to be taken". It is the pair of claims a consumer would otherwise
/// have to guess at: **which statistic** a drawn cell is, and **what happens to a cell nothing was
/// folded into**. The second half is the one that makes a display lie quietly — a fold that emitted
/// its floor for an empty cell would render *never looked* as *looked and it was quiet*, which is
/// the distinction [`hk_store::Coverage::of`] exists to make unspellable.
pub(crate) const MAX_HOLD_RULE: &str = "max-hold: a cell is the maximum of the source cells folded \
     into it, so folding further never lowers a value and a brief emission survives the collapse; \
     the max of nothing is unobserved, not zero";

/// What a cell nothing was folded into looks like on the wire, said in the response rather than
/// left to a reader of this source (T-342, the `grey_rule` precedent from T-368).
pub(crate) const UNOBSERVED_RULE: &str = "a cell nothing was folded into is null in `max_db` and \
     `occupancy_max`, and 0 in `coverage` and `frames`: null is never observed, never quiet, and is \
     not the bottom of the scale";

/// The unit of a folded dB series, in the vocabulary [`/api/floor`](floor_vs_time_json) already
/// uses: densities per Hz, calibrated to the antenna port or relative to ADC full scale.
pub(crate) fn scale_str(unit: hk_model::PowerUnit) -> &'static str {
    match unit {
        hk_model::PowerUnit::Dbm => "dbm-per-hz",
        hk_model::PowerUnit::Dbfs => "dbfs-per-hz",
    }
}

/// What one served overview grid **means**: the fold, the scale, and the unobserved rule (T-342).
///
/// Served beside the numbers so that nothing downstream has to infer whether a value is a max or a
/// mean, or what it is relative to. A series whose statistic is unstated is one a client will
/// eventually re-reduce for itself, which is how the measurement walked into `ui/src` the first
/// time.
pub(crate) fn overview_semantics_json(o: &hk_store::Overview) -> Value {
    let scale = scale_str(o.unit);
    json!({
        "fold": "max-hold",
        "rule": MAX_HOLD_RULE,
        "unobserved_rule": UNOBSERVED_RULE,
        // Per series, because they are not all max-holds: three of the four fold by a different
        // exact rule, and a single "max-hold" label over the grid would misstate them.
        "series": {
            "max_db":        { "statistic": "max-hold", "scale": scale,      "unobserved": "null" },
            "occupancy_max": { "statistic": "max",      "scale": "fraction", "unobserved": "null" },
            "coverage":      { "statistic": "mean",     "scale": "fraction", "unobserved": "0" },
            "frames":        { "statistic": "sum",      "scale": "count",    "unobserved": "0" },
        },
        "range_db": "the observed minimum and maximum of `max_db` over this grid, in the same \
            scale, measured here so a client shades against a scale it was given rather than one \
            it decided from the values it happened to receive",
    })
}

/// The pyramid tier that answers a timeline overview (T-338).
///
/// The rule is the **coarsest tier whose time cells are no larger than one drawn column**: reading
/// finer than the picture buys nothing and costs cells, and reading coarser while a finer tier
/// would fit throws away detail the timeline is there to show. When no tier is fine enough (a
/// window of seconds asked for in ~100 columns is finer than the 1 s floor of the ladder), the
/// finest tier that fits the budget answers and its cells **replicate** across columns — T-334's
/// safe direction, and `resolution.t_cell_s` says so rather than hiding it.
fn overview_level(geom: &Geometry, r: &Region, columns: usize) -> Result<u8, ApiError> {
    let out_t_ns = ((r.t1_ns - r.t0_ns) as f64 / columns.max(1) as f64).max(1.0);
    let fits = |l: usize| {
        let (nt, nf) = dims(geom, l, r);
        nt * nf <= MAX_API_CELLS as f64
    };
    // Levels run finest first, so iterate in reverse to take the coarsest adequate tier.
    if let Some(l) = (0..geom.n_levels())
        .rev()
        .find(|&l| (geom.levels[l].t_cell_ns as f64) <= out_t_ns && fits(l))
    {
        return Ok(l as u8);
    }
    (0..geom.n_levels())
        .find(|&l| fits(l))
        .map(|l| l as u8)
        .ok_or_else(|| {
            ApiError::new(
                400,
                "capture window too large for the cell budget even at the coarsest level",
            )
        })
}

/// Reads the compressed overview of one window (T-338): the pyramid at [`overview_level`], folded
/// by [`hk_store::RegionHistory::overview`] onto exactly `columns × rows` cells over the window.
///
/// The fold is here rather than in the client because choosing which measured value stands for a
/// drawn cell is a measurement (T-334), and because no single pyramid tier can be fine in time and
/// coarse in frequency at once — the ladder couples its axes, and a thin sideways strip needs
/// exactly that combination.
pub(crate) fn overview_read(
    p: &Pyramid,
    r: &Region,
    columns: usize,
    rows: usize,
) -> Result<OverviewRead, ApiError> {
    let level = overview_level(p.geometry(), r, columns)?;
    let time = TimeRange::new(
        Timestamp::from_unix_nanos(r.t0_ns),
        Timestamp::from_unix_nanos(r.t1_ns),
    );
    let h = p
        .query(&RegionQuery {
            freq: r.freq,
            time,
            resolution: Resolution::Level(level),
        })
        .map_err(|_| ApiError::new(400, "history query refused"))?;
    Ok(OverviewRead {
        grid: h.overview(time, r.freq, columns, rows),
        level: h.level,
        src_t_cell_s: h.t_cell_ns as f64 / 1e9,
        src_f_cell_hz: h.f_cell_hz,
    })
}

/// What the spectrum history observed over one region (T-264, ADR-0017 TM-8): the coverage mask
/// behind `/api/events`' `coverage` block.
///
/// Read at the finest level fitting [`DEFAULT_MAX_CELLS`], because only the mask is wanted, not the
/// statistics — a coarser grid would hide short unobserved stretches, and a gap that is not
/// reported is a gap that reads as a quiet band (C26).
pub(crate) fn region_coverage(p: &Pyramid, r: &Region) -> Result<CoverageSummary, ApiError> {
    let level = choose_level(p.geometry(), r, |nt, nf| {
        nt * nf <= DEFAULT_MAX_CELLS as f64
    })?;
    let h = p
        .query(&RegionQuery {
            freq: r.freq,
            time: TimeRange::new(
                Timestamp::from_unix_nanos(r.t0_ns),
                Timestamp::from_unix_nanos(r.t1_ns),
            ),
            resolution: Resolution::Level(level),
        })
        .map_err(|_| ApiError::new(400, "history query refused"))?;
    Ok(h.coverage_summary())
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
///
/// **Every answer carries absolute capture time** (T-337, the user's "one shared time axis"
/// invariant: every time-varying record the backend serves carries the time the client must place
/// it at, so nothing is inferred from a request parameter or from when the response arrived).
/// `window: {t0_s, t1_s}` is the window actually searched — present on `found: false` too, so
/// "nothing in the last 5 s" and "nothing in the last 300 s" are distinguishable — and a found box
/// carries its own `t_start_s`/`t_end_s`/`duration_s`: the time extent of the pyramid cell the peak
/// was measured in, which is the box's *time* exactly as `f_lo_hz`/`f_hi_hz` are its frequency.
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
    let mut best: Option<(f32, usize, usize)> = None;
    for (i, c) in h.cells.iter().enumerate() {
        if c.observed() && best.is_none_or(|(bv, _, _)| c.max_db > bv) {
            best = Some((c.max_db, i / h.nf, i % h.nf));
        }
    }
    // The window searched, always reported: a client places the answer from the response, never
    // from its own request or from when the reply arrived (T-337).
    let window = json!({"t0_s": ts_s(Timestamp::from_unix_nanos(t0_ns)), "t1_s": ts_s(Timestamp::from_unix_nanos(t1_ns))});
    Ok(match best {
        None => json!({ "found": false, "window": window }),
        Some((max_db, t, f)) => {
            let freq = h.freq_of(f);
            let center = 0.5 * (freq.lo_hz + freq.hi_hz);
            let lo = (center - STRONGEST_BOX_HALF_HZ).max(f_lo);
            let hi = (center + STRONGEST_BOX_HALF_HZ).min(f_hi);
            // The box's time extent is the cell the peak was measured in, not the whole window:
            // `t0_s + k·t_cell_s` is the pyramid's own grid contract (T-334, docs/07 §4.1).
            let t_cell_s = h.t_cell_ns as f64 / 1e9;
            let t_start_s = ts_s(h.time_of(0)) + t as f64 * t_cell_s;
            json!({
                "found": true,
                "f_center_hz": center,
                "f_lo_hz": lo,
                "f_hi_hz": hi,
                "max_db": max_db,
                "t_start_s": t_start_s,
                "t_end_s": t_start_s + t_cell_s,
                "duration_s": t_cell_s,
                "t_cell_s": t_cell_s,
                "window": window,
                // The same statement the band-collapsed series carries (T-342): this route is
                // "strongest in a band", which is a max-hold over a window, and a consumer must
                // not have to read the docs to learn that or to learn what `max_db` is relative
                // to. `unit` is the source grid's, densities per Hz.
                "semantics": {
                    "statistic": "max-hold",
                    "rule": MAX_HOLD_RULE,
                    "scale": scale_str(h.unit),
                },
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

pub(crate) fn nonempty<'a>(q: &'a Params, key: &str) -> Option<&'a str> {
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

/// The `t0`/`t1` view window, in Unix seconds on the **capture clock**, or `None` when the caller
/// named none (T-384).
///
/// One parser for every windowed route, so `/api/inventory`, `/api/inventory/{id}/decode` and
/// anything added later cannot disagree about what a window is, what its bounds mean, or which
/// half of a half-given pair is an error. The UI sends one window to every surface
/// (`ui/src/app/explore/inventory.ts`'s `viewWindow`), and a route that parsed it differently
/// would silently answer about a different range than the waterfall beside it is showing.
///
/// Both ends are required together — a lone `t0` is a caller bug, not "from then on", because a
/// window with an invented end is exactly the *plausible query* the whole-UI window rule forbids.
pub fn parse_time_window(q: &Params) -> Result<Option<TimeRange>, ApiError> {
    match optional_pair(q, "t0", "t1")? {
        None => Ok(None),
        Some((t0, t1)) if t1 >= t0 && t0 > -4e9 && t1 < 9e9 => Ok(Some(TimeRange::new(
            Timestamp::from_unix_nanos((t0 * 1e9).round() as i64),
            Timestamp::from_unix_nanos((t1 * 1e9).round() as i64),
        ))),
        Some(_) => Err(bad("need t0 <= t1 (Unix seconds)")),
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
    let time = parse_time_window(q)?;
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
    // T-219: rows that currently defer to another row (suppressed by a Confirmed entry, the weaker
    // of a duplicate group, an attributed receiver artifact) are hidden unless asked for. Their
    // rows, detections, tracks and history are kept and still reachable by id.
    let relations = match nonempty(q, "relations") {
        None | Some("shown") => RelationVisibility::Shown,
        Some("all") => RelationVisibility::All,
        Some(_) => return Err(bad("relations must be shown or all")),
    };
    Ok(InventoryQuery {
        freq,
        time,
        status,
        states,
        tag: short_text(q, "tag")?,
        identity_scheme,
        family: short_text(q, "family")?,
        relations,
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
pub(crate) fn explanations_json(
    repo: &Repository,
    id: hk_model::EmitterId,
) -> Result<Value, RepoError> {
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
/// T-163 (ADR-0013 gap 7a): `estimated_params` is the emitter's latest [`hk_model::EstimatedParams`]
/// (symbol rate, modulation, deviation, CFO, bandwidth), from its latest demodulation session
/// ([`Repository::latest_demodulation_for_emitter`]); `null` when none has run yet, and on a
/// withheld-identity row, always — see [`estimated_params_json`].
///
/// T-078: `state` filters by lifecycle (`candidate`, `confirmed`, `deleted`, comma-separated; by
/// default candidates and confirmed entries, never deleted ones). Each row carries `state`,
/// `lifecycle` (the latest change: state, previous, author `auto`/`user`, actor, reason, `t_s`; or
/// `null` for an untouched candidate; a user's reason is withheld on withheld-identity rows) and
/// `recurrence` (occurrences, appearances, span, on-air time, duty cycle and the
/// [`RECENT_APPEARANCES`] latest appearances).
///
/// T-320: `cluster_group` is the row's `cluster_id` served as grouping data — the id, a short
/// stable `label`, and `rows_in_view`, the number of rows **on this page** sharing it. See
/// [`cluster_group_json`]: it makes duplication visible and de-duplicates nothing.
///
/// T-171: `total` is [`Repository::count_inventory`] — the number of rows the same filters match,
/// ignoring `cursor`/`limit`, so a caller can show a count past one page. It is an efficient
/// indexed `COUNT(*)` for every filter but a `tag` outside `hk_model::TAG_VOCABULARY`, which is
/// capped (see `count_inventory`'s docs) and can then read as a lower bound.
pub fn inventory_json(
    state: &crate::http::ApiState,
    repo: &Repository,
    q: &Params,
) -> Result<Value, ApiError> {
    let query = parse_inventory_query(q)?;
    // T-263 (ADR-0017 TM-7): an unwindowed caller's own live edge, for a scrubbed-back view.
    let at = parse_presence_at(q, query.time)?;
    let failed = |_| ApiError::new(500, "inventory query failed");
    let page = repo.query_inventory(&query).map_err(failed)?;
    let total = repo.count_inventory(&query).map_err(failed)?;
    // T-410 (ADR-0019 §3): the tune history behind every row's idle gap, read once for the whole
    // page and then asked per band. It is the projection window, not the selection window, that
    // matters — an unwindowed Confirmed list still renders liveness, against all of time up to its
    // live edge — so it is built from the same `presence_window` those rows are projected through.
    let coverage = ObservedCoverage::of(state, presence_window(query.time, at).0);
    let mut entries = Vec::with_capacity(page.entries.len());
    for entry in &page.entries {
        // ADR-0017 TM-2: the query's own window scopes each row's `presence` and
        // `family_in_window`. The rows themselves are already selected by the same window, in the
        // same predicate (`InventoryQuery::time`, interval overlap), so the list and the liveness
        // it renders can never disagree.
        entries
            .push(inventory_entry_json_at(repo, entry, query.time, at, &coverage).map_err(failed)?);
    }
    // T-320: the grouping, computed here because only the list knows what is in view. A client
    // must not derive it — the UI is a thin client over this contract (CLAUDE.md) — and only the
    // server can scope the count honestly to the page it actually served.
    let mut per_cluster: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for e in &entries {
        if let Some(id) = e["cluster_id"].as_str() {
            *per_cluster.entry(id.to_owned()).or_default() += 1;
        }
    }
    for e in &mut entries {
        let id = e["cluster_id"].as_str().map(ToOwned::to_owned);
        let n = id.as_deref().and_then(|i| per_cluster.get(i)).copied();
        e["cluster_group"] = cluster_group_json(id.as_deref(), n.unwrap_or(1));
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

/// T-320 `cluster_group`: the row's C18 cluster membership served as **grouping data**, so a list
/// can show *which* rows measure alike rather than only that each has been seen before.
///
/// `null` exactly when `cluster_id` is — no cluster, one that is not yet visible, or a
/// withheld-identity row — so it can never reveal a membership `cluster_id` withholds.
///
/// - `cluster_id`: the same id as the row's own field, repeated so the object stands alone.
/// - `label`: [`hk_model::cluster_label`], a short stable form of that id. Backend-owned because
///   the grouping is the backend's claim, not a client's arithmetic over a page it happens to hold.
/// - `rows_in_view`: how many rows **in this response** carry the same id. It is scoped to what
///   was served — never a cluster's total membership, which `/api/clusters/{id}` answers — so a
///   client can say "11 of the rows you are looking at measure alike" and nothing stronger.
///
/// **It groups; it does not merge.** Rows sharing a label stay separate inventory rows with their
/// own ids, counts, detections and history: a cluster is a *type* and an emitter an *instance*
/// (ADR-0016 §5), and clustering writes nothing on an emitter — pinned by
/// `a_cluster_never_changes_anything_about_the_emitter`. Duplicate rows are minted upstream by
/// entity resolution; this field makes such duplication **visible**, and de-duplicates nothing.
/// The grouping is derived from the cluster id alone, so it cannot vary with which front end
/// reported a row (T-259/T-305: identity and clustering never read the device).
fn cluster_group_json(cluster_id: Option<&str>, rows_in_view: usize) -> Value {
    cluster_id.map_or(Value::Null, |id| {
        json!({
            "cluster_id": id,
            "label": cluster_label(id),
            "rows_in_view": rows_in_view,
        })
    })
}

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

/// T-163 (ADR-0013 gap 7a) `estimated_params` object: one field per [`hk_model::EstimatedParams`]
/// measurement, plus `modulation` (the demodulation's `mode`, e.g. `wfm`, `2fsk`) and provenance.
/// A field the estimator never measured for this signal (e.g. `symbol_rate_hz` on an analog FM
/// station) is `null`, exactly like the stored [`hk_model::EstimatedParams`] — never a fabricated
/// default. `t_s` and `source_session` match `/api/inventory/{id}/decode`'s `at`/`source_session`
/// convention: the session's end time, and the demodulation's own id.
fn estimated_params_json(d: &Demodulation) -> Value {
    let p = &d.params;
    json!({
        "modulation": d.mode,
        "symbol_rate_hz": p.symbol_rate_hz,
        "mod_order": p.mod_order,
        "deviation_hz": p.deviation_hz,
        "cfo_hz": p.cfo_hz,
        "bandwidth_hz": p.bandwidth_hz,
        "roll_off": p.roll_off,
        "pilot_hz": p.pilot_hz,
        "t_s": ts_s(d.time.end),
        "source_session": d.id.to_string(),
        "source_recording": d.recording_ref.map(|r| r.to_string()),
    })
}

/// Stand-in for "since the beginning" when a request gave no `t0`. Half of `i64::MIN` (about 146
/// years before the epoch) so no arithmetic on the bound can overflow.
const ALL_TIME_START_NS: i64 = i64::MIN / 2;

/// The window an inventory row's `presence` and `family_in_window` are derived over, and the live
/// edge `open` is read against (ADR-0017 TM-2).
///
/// - **A caller that gave `t0`/`t1` asked about that window, and its `t1` is its own live edge** —
///   TM-3's Explore sends `t1 = now`. Closure is therefore derived against `t1` rather than the
///   wall clock, which is what makes scrubbing re-derive exactly the liveness a row had at that
///   time (ADR-0017 §2.4) instead of marking every past window `ended`.
/// - **A caller that gave no window asked about all of time up to now.** That is Explore's
///   Confirmed list, which is deliberately *not* time-filtered (§2.2, T-260) and still has to
///   render liveness, so `presence` is served there too — against the wall clock, or against `at`
///   when the caller named its own live edge (T-263, below).
///
/// **`at` (T-263, ADR-0017 TM-7).** An unwindowed caller's own live edge, for a scrubbed-back
/// view. Explore's Confirmed list must stay *listed* while scrubbed — that is the §2.2 safety
/// valve, and sending `t0`/`t1` there would filter quiet catalogue entries out — but its rows
/// would then read the liveness they have *now*, disagreeing with the past window every other
/// surface is showing. `at` separates the two questions the window parameter had fused: `t0`/`t1`
/// **select** rows and scope their projections; `at` scopes the projection alone and selects
/// nothing. It is refused beside `t0`/`t1`, whose `t1` is already the caller's live edge.
///
/// **The idle gap is measured, not defaulted** (T-410, ADR-0019 §3). It used to be
/// [`IdleGap::conservative`] here on the grounds that hk-api does not know the scheduler's revisit
/// period. The principle was right — a shorter gap would claim an absence that was not observed
/// (T-262, ADR-0017 §11 q4) — but the premise was wrong: a receiver's revisit period is a
/// *measurement*, recorded in the IQ ring's tune history, and a dwell on one centre revisits its
/// band every STFT frame. Taking 60 s there was not conservatism but discarding a measurement the
/// run had in hand, and once a box runs to the live edge until an END is detected (ADR-0019 §1) it
/// was a box over-claiming a minute of silent air. [`ObservedCoverage`] reads the history;
/// `conservative()` is kept for its real meaning, **nobody recorded whether the receiver looked**.
pub(crate) fn presence_window(
    window: Option<TimeRange>,
    at: Option<Timestamp>,
) -> (TimeRange, Timestamp) {
    match window {
        Some(w) => (w, w.end),
        None => {
            let now = at.unwrap_or_else(Timestamp::now);
            (
                TimeRange::new(Timestamp::from_unix_nanos(ALL_TIME_START_NS), now),
                now,
            )
        }
    }
}

/// The `at` parameter of [`presence_window`] (T-263, ADR-0017 TM-7): the caller's own live edge
/// for an **unwindowed** query, so a scrubbed-back Confirmed list re-derives the liveness its rows
/// had then instead of the liveness they have now.
///
/// Refused beside `t0`/`t1` rather than silently ignored: a window already carries its own live
/// edge in `t1`, so a request giving both is asking two different questions at once and the answer
/// would depend on which one this code happened to prefer.
pub fn parse_presence_at(
    q: &Params,
    window: Option<TimeRange>,
) -> Result<Option<Timestamp>, ApiError> {
    if nonempty(q, "at").is_none() {
        return Ok(None);
    }
    if window.is_some() {
        return Err(bad(
            "at is for a query with no t0/t1: a window's own t1 is already its live edge",
        ));
    }
    let at = num(q, "at")?;
    if !(at > -4e9 && at < 9e9) {
        return Err(bad("at must be a Unix second in range"));
    }
    Ok(Some(Timestamp::from_unix_nanos((at * 1e9).round() as i64)))
}

/// ADR-0017 TM-2 `presence` object: **when** this emitter was on the air, seen through the
/// request's window. Every field is derived from presence-interval boundaries
/// ([`hk_model::presence`]); **none is derived from `count`**, which is a lifetime History total
/// excluded from every liveness decision and from live-list ranking (ADR-0017 §5).
///
/// - `intervals` — how many presence intervals intersect the window ("17 events").
/// - `on_air_s` — time on air *inside* the window: Σ of each interval's intersection with it,
///   **less** any silence a revoked end rejoined (T-413). This is what a live list ranks by, in
///   place of the lifetime `count`.
/// - `last_interval` — the latest interval intersecting the window (`t_start_s`, `t_end_s`,
///   `open`, `revoked_s`), or `null` when none does. It is the box the waterfall draws (TM-4).
///   `revoked_s` is measured silence *inside* the interval whose detected end a resumption revoked
///   (T-413, ADR-0019 §6.1): the interval is one interval, and this says how much of the span it
///   covers was measured empty, so the box is never drawn as if it were on air throughout.
/// - `liveness` — `live` / `ended` / `absent` (§2.3).
/// - `ended_t_s` — when it stopped, for `ended` only; `null` while live or absent. This is the
///   "ended 4 minutes ago" the product previously could not say.
/// - `silence_s` / `confidence` — T-251 (TM-6): how long the row has been silent at this window's
///   live edge, and the decayed confidence in its hypothesis. `confidence` is what ranks a
///   candidate that stopped *inside* the window below one transmitting now — the case
///   window-scoping cannot answer, because `on_air_s` is blind to *when* inside the window the
///   signal was on. It is a rank, never a lifetime: it expires nothing and deletes nothing.
///
/// The interval's `count` is deliberately **not** on the wire: nothing in a live list may rank by
/// it, and the row's lifetime `count` already carries it for History.
///
/// Shared with `GET /api/inventory/{id}/presence` (T-264), which serves this same object beside
/// the full track: both surfaces claim to answer identically about one emitter, so they render it
/// with one function rather than two hand-written blocks that can drift apart — as they did the
/// moment this object gained a field.
pub(crate) fn presence_json(p: &Presence) -> Value {
    json!({
        "intervals": p.intervals,
        "on_air_s": p.on_air_s,
        "last_interval": p.last_interval.as_ref().map(|i| json!({
            "t_start_s": ts_s(i.time.start),
            "t_end_s": ts_s(i.time.end),
            "open": i.open,
            // T-413: measured silence inside this interval whose detected end a resumption revoked.
            // 0 for almost every interval; when it is not, the box covers air that was measured
            // *empty*, and the renderer marks it rather than drawing the join solid.
            "revoked_s": i.revoked_s(),
        })),
        "liveness": p.liveness.as_str(),
        "ended_t_s": p.ended_t.map(ts_s),
        "silence_s": p.silence_s,
        "confidence": p.confidence,
    })
}

pub fn inventory_entry_json(repo: &Repository, entry: &InventoryEntry) -> Result<Value, RepoError> {
    inventory_entry_json_at(repo, entry, None, None, &ObservedCoverage::default())
}

/// [`inventory_entry_json`] for a caller that has the run's tune history in hand (T-410): the
/// single-row routes, so one row read on its own reports the same liveness the list reports for it.
pub fn inventory_entry_json_with_coverage(
    repo: &Repository,
    entry: &InventoryEntry,
    coverage: &ObservedCoverage,
) -> Result<Value, RepoError> {
    inventory_entry_json_at(repo, entry, None, None, coverage)
}

/// [`inventory_entry_json_at`] with no caller-named live edge (the wall clock decides `open`).
pub fn inventory_entry_json_in_window(
    repo: &Repository,
    entry: &InventoryEntry,
    window: Option<TimeRange>,
) -> Result<Value, RepoError> {
    inventory_entry_json_at(repo, entry, window, None, &ObservedCoverage::default())
}

/// [`inventory_entry_json`] with the request's time window, which adds ADR-0017 TM-2's two
/// window-scoped projections. Both are **additive**: every existing field keeps its name, its
/// meaning and its all-time scope.
///
/// - **`presence`** ([`presence_json`]) is served on every row, windowed or not — Explore's
///   Confirmed list is unwindowed and still renders liveness.
/// - **`family_in_window`** is served **only when a window was given**, which is what keeps
///   "the window holds no classification row" (`null`) distinguishable from "no window was asked
///   about" (the key is absent). Without a window the question has no meaning, and answering it
///   with the all-time family would silently assert the very staleness this field exists to
///   disclose.
///
/// Neither is identity-gated, and both are served uniformly on withheld rows: they are timing and
/// family data of exactly the class `recurrence` and `family` already carry unconditionally, and
/// because the fields appear on every row alike their presence can never signal that a row's
/// identity was withheld (the T-159/T-163 rule those gated fields exist under).
/// `at` (T-263) is the caller's own live edge for an unwindowed query; see [`presence_window`].
pub fn inventory_entry_json_at(
    repo: &Repository,
    entry: &InventoryEntry,
    window: Option<TimeRange>,
    at: Option<Timestamp>,
    coverage: &ObservedCoverage,
) -> Result<Value, RepoError> {
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
        // T-219 (C40): the standing relationship, when this row defers to another — suppressed by
        // an overlapping Confirmed entry, the weaker of a duplicate group, or a receiver artifact
        // attributed to its source. The reason is backend-rendered from emitter ids, frequency
        // arithmetic and the rank terms, so it never names an identity.
        let relation = repo.emitter_relations(e.id)?.first().map(|r| {
            json!({
                "kind": r.kind.as_str(),
                "artifact": r.artifact.map(ArtifactKind::as_str),
                "source_id": r.source_id.to_string(),
                "author": r.author.as_str(),
                "actor": r.actor,
                "t_s": ts_s(r.t),
                "reason": r.reason,
                "score": r.score,
                "detail": r.detail,
            })
        });
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
        // T-163 (ADR-0013 gap 7a): the emitter's latest blind-estimated parameters (C13/C14), for
        // the Decode workbench's "Use" suggestions. `null` with no demodulation session recorded
        // yet. On a withheld-identity row this reads `null` too, whatever storage holds: these
        // are DSP measurements, not identity data, but serving them only when unwithheld keeps
        // this route's answer on a withheld row indistinguishable from "nothing measured yet"
        // (the same rule `/api/inventory/{id}/decode`, T-159/T-036, applies to decodes), so a
        // withheld identity is never confirmed indirectly by a new field appearing or not.
        let estimated_params = if withheld {
            None
        } else {
            repo.latest_demodulation_for_emitter(e.id)?
                .map(|d| estimated_params_json(&d))
        };
        // T-202 (ADR-0016 §5): the C18 cluster of unknown emissions this row belongs to — "I have
        // seen this before". Only a *visible* cluster is named: a pending one has too few members
        // to be more than a guess. On a withheld-identity row it reads `null` whatever storage
        // holds, exactly as `estimated_params` and `/api/inventory/{id}/decode` do (T-159/T-163),
        // so cluster membership can never confirm a withheld identity indirectly.
        let cluster_id = if withheld {
            None
        } else {
            match repo.emitter_cluster_id(e.id)? {
                Some(id) => repo
                    .cluster_opt(&id)?
                    .filter(|c| c.state.visible())
                    .map(|c| c.id),
                None => None,
            }
        };
        let cluster_group = cluster_group_json(cluster_id.as_deref(), 1);
        // ADR-0017 TM-2: when this emitter was on the air, through the request's window. The
        // emitter's own `first_seen`/`last_seen` are a *hull* and never an extent, so this — not
        // they — is what a caller reads for "is it on air, and for how long".
        let (span, now) = presence_window(window, at);
        // T-410 (ADR-0019 §3): the idle gap is **measured** off this band's coverage, not defaulted
        // to the 60 s unknown. It closes this row's interval, so it is also how long the row's box
        // runs to the live edge before capping — 60 s on a band the receiver never looked away from
        // was a box over-claiming a minute of silent air.
        let gap = coverage.idle_gap(e.freq(), span);
        let presence = presence_json(&repo.presence(e.id, span, gap, now)?);
        // ADR-0017 §7.1: the same arbitration ladder over the rows inside the window. `family`
        // above stays the all-time answer — identity evidence is time-invariant — and this says
        // whether anything *in these minutes* re-evidenced it. `null` when nothing did, so a
        // view-scoped client can show `family` marked "(from earlier)" instead of asserting it.
        let family_in_window = window
            .map(|w| repo.current_classification_in_window(e.id, w))
            .transpose()?
            .map(|c| c.map(|c| c.classification.family));
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
            // ADR-0017 TM-2: the presence track through the request's window — intervals, time on
            // air inside it, the latest interval, and liveness. `count` above is a lifetime total
            // for History and takes no part in any of it.
            "presence": presence,
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
            "estimated_params": estimated_params,
            // T-202 (ADR-0016 §5): C18 cluster membership — evidence that this emission measures
            // like others, never an identity, a family or a status.
            "cluster_id": cluster_id,
            // T-320: the same membership as *grouping data*, so a list can show which rows measure
            // alike instead of only that each one has been seen before. `rows_in_view` is 1 here —
            // a single row is the whole view — and [`inventory_json`] raises it to the number of
            // rows on the page that share the id. It groups; it never merges.
            "cluster_group": cluster_group,
            "identity_scheme": scheme,
            "identity_class": class,
            "withheld": withheld,
            "snr_db": measurement.map(|(snr, _)| snr),
            "peak_dbfs": measurement.map(|(_, peak)| peak),
            // T-219 (C40): why this row defers to another, when it does. Never a deletion — the
            // row, its detections, tracks and history are all kept and the claim is reversible.
            "relation": relation,
        });
        // ADR-0017 §7.1: present only when the request named a window — see the function docs for
        // why absent and `null` must stay different answers.
        if let Some(f) = family_in_window {
            row["family_in_window"] = json!(f);
        }
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
