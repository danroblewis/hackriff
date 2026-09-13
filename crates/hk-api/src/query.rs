//! JSON for the read-only history endpoints.
//!
//! - `/api/history?f_lo&f_hi&t0&t1[&max_cells]`: [`Pyramid::query`] (T-017) at the finest level
//!   whose grid over the region fits in `max_cells` cells (default [`DEFAULT_MAX_CELLS`], at most
//!   [`MAX_API_CELLS`]). Cells are row-major (time then frequency), one array per statistic;
//!   unobserved cells are `null` ("not observed" is not "quiet", C26).
//! - `/api/floor?f_lo&f_hi&t0&t1[&max_steps]`: [`FloorProduct::floor_vs_time`] (T-021) at the
//!   finest level with at most `max_steps` time steps.
//!
//! `f_lo`/`f_hi` are Hz; `t0`/`t1` are Unix seconds.

use hk_model::{FreqRange, TimeRange, Timestamp};
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
