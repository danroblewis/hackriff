//! JSON for the read-only history endpoints.
//!
//! - `/api/history?f_lo&f_hi&t0&t1[&max_cells]`: [`Pyramid::query`] (T-017) at the finest level
//!   whose grid over the region fits in `max_cells` cells (default [`DEFAULT_MAX_CELLS`], at most
//!   [`MAX_API_CELLS`]). Cells are row-major (time then frequency), one array per statistic;
//!   unobserved cells are `null` ("not observed" is not "quiet", C26).
//! - `/api/floor?f_lo&f_hi&t0&t1[&max_steps]`: [`FloorProduct::floor_vs_time`] (T-021) at the
//!   finest level with at most `max_steps` time steps.
//!
//! - `/api/inventory?[f_lo&f_hi][&t0&t1][&status][&tag][&scheme][&family][&cursor][&limit]`:
//!   the T-018 signal inventory ([`inventory_json`]).
//!
//! `f_lo`/`f_hi` are Hz; `t0`/`t1` are Unix seconds.

use hk_model::{
    AnnotationAuthor, AnnotationTarget, FreqRange, IdentityAccess, IdentityScheme,
    InventoryIdentity, InventoryQuery, KnownStatus, Repository, StatusAuthor, TimeRange, Timestamp,
};
use hk_store::history::Geometry;
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

/// Default cell budget of `/api/history`.
pub const DEFAULT_MAX_CELLS: usize = 100_000;
/// Largest cell budget `/api/history` accepts (bounds response size).
pub const MAX_API_CELLS: usize = 500_000;
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

fn ts_s(t: Timestamp) -> f64 {
    t.as_unix_nanos() as f64 / 1e9
}

fn provenance_json(p: &ProvenanceSummary) -> Value {
    let gains: Vec<Value> = p
        .gain_states
        .iter()
        .map(|(g, frames)| {
            json!({"lna_db": g.lna_db, "vga_db": g.vga_db, "amp_on": g.amp_on, "frames": frames})
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
        "first_frame_s": p.first_frame.map(ts_s),
        "last_frame_s": p.last_frame.map(ts_s),
    })
}

/// The JSON of one [`RegionHistory`].
pub fn region_history_json(h: &RegionHistory) -> Value {
    let col = |f: fn(&hk_store::CellStats) -> Value| -> Value {
        Value::Array(h.cells.iter().map(f).collect())
    };
    json!({
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
        "frames": col(|c| json!(c.frames)),
        "provenance": provenance_json(&h.provenance),
        "tiles_read": h.tiles_read,
    })
}

/// `/api/history`.
pub fn history_json(p: &Pyramid, q: &Params) -> Result<Value, ApiError> {
    let r = parse_region(q)?;
    let max_cells = count(q, "max_cells", DEFAULT_MAX_CELLS, MAX_API_CELLS)?;
    let level = choose_level(p.geometry(), &r, |nt, nf| nt * nf <= max_cells as f64)?;
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
    Ok(region_history_json(&h))
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
        tag: short_text(q, "tag")?,
        identity_scheme,
        family: short_text(q, "family")?,
        limit: limit as u32,
        offset,
        access: IdentityAccess::Standard,
    })
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
pub fn inventory_json(repo: &Repository, q: &Params) -> Result<Value, ApiError> {
    let query = parse_inventory_query(q)?;
    let failed = |_| ApiError::new(500, "inventory query failed");
    let page = repo.query_inventory(&query).map_err(failed)?;
    let mut entries = Vec::with_capacity(page.entries.len());
    for entry in &page.entries {
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
        let status = repo
            .known_status_history(e.id)
            .map_err(failed)?
            .last()
            .map(|c| {
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
        let explanations = repo
            .annotations_for(&AnnotationTarget::Emitter(e.id))
            .map_err(failed)?
            .into_iter()
            .rfind(|a| {
                a.author == AnnotationAuthor::Classifier
                    && a.content.is_none()
                    && a.metadata.get("explanations").is_some()
            })
            .map_or_else(|| json!([]), |a| a.metadata["explanations"].clone());
        let freq = e.freq();
        let mut row = json!({
            "explanations": explanations,
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
            "classification": e.current_classification().map(|c| json!({
                "family": c.family,
                "confidence": c.confidence,
                "open_set_score": c.open_set_score,
                "model_version": c.model_version,
                "t_s": ts_s(c.t),
            })),
            "classifications": e.classifications.len(),
            "identity_scheme": scheme,
            "identity_class": class,
            "withheld": withheld,
        });
        if let Some(v) = value {
            row["identity_value"] = json!(v);
        }
        entries.push(row);
    }
    Ok(json!({
        "entries": entries,
        "next_cursor": page
            .next_offset
            .filter(|&o| o <= MAX_INVENTORY_CURSOR)
            .map(|o| o.to_string()),
        "limit": query.limit,
        "identity_access": "standard",
    }))
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
