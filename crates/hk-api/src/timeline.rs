//! `GET /api/timeline` — the capture window, and the compressed overview drawn on it (T-338).
//!
//! From the user's invariant (CLAUDE.md, "Time, the waterfall, and the live view"):
//!
//! > **The timeline is the capture window, and it is a visualization.** The scrubbable
//! > capture-history timeline spans **exactly the configured recording/retention duration** — no
//! > more (not the longer, lossy spectrum-history retention), no less — and grows or shrinks when
//! > that duration is reconfigured. It is itself a data display: a **compressed "sideways"
//! > overview waterfall** of the retained capture (time along its long axis), never an empty box.
//!
//! Two things had to become the backend's, and this route is both of them.
//!
//! # 1. The horizon is the ring's, not the history's
//!
//! There are two retention horizons on this server and they are **deliberately different lengths**:
//! the IQ capture ring ([ADR-0014], `--iq-retention`, minutes) and the tiered spectrum-history
//! pyramid (lossy, byte-budgeted, days). Sizing the scrubber from the pyramid — or, as the client
//! did, from a hard-coded 48 h constant — makes it **promise capture that no longer exists**: the
//! user can scrub to a time the ring overwrote long ago, and the band looks right while it lies.
//! So `window` here is the ring's, `window.horizon` says so in the response, and
//! `GET /api/navigation`'s `time.latest_s` (the history horizon) is not consulted for it.
//!
//! The span served is `retention_s`, the **configured** window, not `t1 − t0` of what the ring
//! currently holds: a ring five minutes into a one-hour retention is a mostly-empty capture window,
//! not a five-minute one, and the timeline that shrank to fit it would grow under the user as the
//! ring filled. What the ring holds is reported beside it as `window.buffered`, so the client can
//! draw the difference (T-263's ring track) instead of inferring it.
//!
//! # 2. The overview is a measurement, so it is made here
//!
//! The band is a data display, which means something has to decide what each drawn cell shows.
//! T-334 put that decision in the backend: mapping a time to a pixel is presentation, *choosing
//! which value represents an interval* is a measurement. The pyramid alone cannot serve this
//! particular picture — its ladder couples the axes, so the tier with cells coarse enough in
//! frequency for a thin strip's rows (100 kHz) has one-day time cells — so the tier is read at the
//! timeline's own time resolution and folded onto `columns × rows` by
//! [`hk_store::RegionHistory::overview`], which carries only statistics that fold **exactly**
//! (max-hold, peak occupancy, summed frames, mean coverage). The grid's own dynamic range is served
//! too: picking a colour scale from the numbers you happen to hold is a measurement as well.
//!
//! `resolution` is T-334's block, with T-341's three-valued [`DetailSource`]: the timeline draws
//! the ring's window but its pixels come from the **pyramid**, so it claims `spectrum-history`
//! (or `survey-overview` for a span no capture window could hold) and never `live-iq`.
//!
//! | Method | Path | Query | Answers |
//! |---|---|---|---|
//! | GET | `/api/timeline` | `?[f_lo=&f_hi=][&columns=1..4096, default 96][&rows=1..512, default 1]` | `{window, region, grid, resolution}` |
//!
//! [ADR-0014]: ../../../docs/adr/0014-iq-capture-ring.md

use serde_json::{Value, json};

use crate::http::ApiState;
use crate::navigation::DetailSource;
use crate::query::{ApiError, Params, Region, count, parse_freq_only};

/// Columns the overview is drawn in when the caller names none.
pub const DEFAULT_COLUMNS: usize = 96;
/// Most columns accepted (a band cannot draw more than this, and each is a measurement).
pub const MAX_COLUMNS: usize = 4096;
/// Frequency rows when the caller names none: a single-row activity strip.
pub const DEFAULT_ROWS: usize = 1;
/// Most frequency rows accepted.
pub const MAX_ROWS: usize = 512;

/// The capture window the timeline spans, read from the IQ ring's own status.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CaptureWindow {
    /// Whether a ring is running.
    pub enabled: bool,
    /// The configured retention, s — **the band's span, exactly**. `None` when no ring exists here
    /// to state one.
    pub retention_s: Option<f64>,
    /// The live edge of capture, Unix s.
    pub t1_s: Option<f64>,
    /// What the ring currently holds, Unix s; `None` when it holds nothing.
    pub buffered: Option<(f64, f64)>,
}

impl CaptureWindow {
    /// The band: `[t1 − retention, t1]`. `None` unless both a retention and a live edge are known —
    /// a timeline with no capture window behind it is drawn as unknown, never as a default span.
    pub fn band(&self) -> Option<(f64, f64)> {
        match (self.retention_s, self.t1_s) {
            (Some(r), Some(t1)) if r > 0.0 && t1.is_finite() => Some((t1 - r, t1)),
            _ => None,
        }
    }
}

fn f64_of(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64).filter(|x| x.is_finite())
}

/// Reads the ring's status (`limit=1`: the window is wanted, not the segment list) and the history's
/// live edge, which stands in for `t1` only while the ring holds nothing yet.
pub(crate) fn capture_window(state: &ApiState) -> (CaptureWindow, Value) {
    let status = state.iq_buffer.as_deref().map(|c| {
        c.status(&crate::iqbuffer::IqBufferQuery {
            t0: None,
            t1: None,
            limit: 1,
        })
    });
    let Some(s) = status else {
        return (
            CaptureWindow {
                enabled: false,
                retention_s: None,
                t1_s: None,
                buffered: None,
            },
            // No ring at all on this server: the reason there is no capture window.
            json!("no IQ capture buffer on this server"),
        );
    };
    let buffered = match (f64_of(&s, "t0"), f64_of(&s, "t1")) {
        (Some(a), Some(b)) if b > a => Some((a, b)),
        _ => None,
    };
    // The live edge: the ring's newest sample, else the history's newest frame. Never wall-clock —
    // a replay or a time-compressed scene runs on its own clock (T-125), and a band anchored to
    // `now` would place its capture in the future.
    let t1_s = buffered.map(|(_, b)| b).or_else(|| {
        crate::http::with_history(state, |p| {
            Ok(p.latest_frame_end().map(|t| t.as_unix_nanos() as f64 / 1e9))
        })
        .ok()
        .flatten()
    });
    (
        CaptureWindow {
            enabled: s.get("enabled").and_then(Value::as_bool).unwrap_or(false),
            retention_s: f64_of(&s, "retention_s"),
            t1_s,
            buffered,
        },
        s.get("reason").cloned().unwrap_or(Value::Null),
    )
}

fn window_json(w: &CaptureWindow, reason: &Value) -> Value {
    let band = w.band();
    json!({
        // Which retention this span is. The spectrum-history horizon is longer and lossy; a
        // scrubber sized from it offers times the ring has already overwritten.
        "horizon": "iq-ring",
        "enabled": w.enabled,
        "reason": reason.clone(),
        "retention_s": w.retention_s,
        "t0_s": band.map(|(a, _)| a),
        "t1_s": band.map(|(_, b)| b),
        // Equal to `retention_s` by construction, and served so a client never computes it.
        "span_s": band.map(|(a, b)| b - a),
        "buffered": w.buffered.map(|(a, b)| json!({"t0_s": a, "t1_s": b, "span_s": b - a})),
    })
}

fn grid_json(o: &hk_store::Overview) -> Value {
    let num = |x: f32| -> Value { if x.is_finite() { json!(x) } else { Value::Null } };
    json!({
        "nt": o.nt,
        "nf": o.nf,
        "t0_s": o.t0_ns as f64 / 1e9,
        "t_cell_s": o.t_cell_ns / 1e9,
        "f_lo_hz": o.f_lo_hz,
        "f_cell_hz": o.f_cell_hz,
        // Row-major, time then frequency. `null` is **not observed**, never quiet (C26).
        "max_db": Value::Array(o.cells.iter().map(|c| num(c.max_db)).collect()),
        "occupancy_max": Value::Array(o.cells.iter().map(|c| num(c.occupancy_max)).collect()),
        "coverage": Value::Array(o.cells.iter().map(|c| json!(c.coverage)).collect()),
        "frames": Value::Array(o.cells.iter().map(|c| json!(c.frames)).collect()),
        "cells": o.cells.len(),
        "observed_cells": o.observed_cells,
        // The grid's own observed range of `max_db`, so the client draws a scale it was given
        // rather than deciding one from the values it happens to hold. `null` = nothing observed.
        "range_db": o.range_db.map(|(lo, hi)| json!({"lo": lo, "hi": hi})),
    })
}

fn resolution_json(
    r: &crate::query::OverviewRead,
    region: &Region,
    columns: usize,
    rows: usize,
    levels: usize,
    max_live_span_hz: Option<f64>,
) -> Value {
    let served_span_hz = region.freq.width_hz();
    // The timeline's *horizon* is the ring's; its *pixels* are the pyramid's. So it never claims
    // `live-iq`, exactly as `/api/history` does not.
    let source = match crate::navigation::live_window_verdict(served_span_hz, max_live_span_hz) {
        DetailSource::LiveIq => DetailSource::SpectrumHistory,
        other => other,
    };
    json!({
        "source": source.as_str(),
        "live": source.is_live(),
        "statement": source.statement(),
        // Which retention the picture spans, beside which tier drew it: the two questions the
        // scrubber has to answer honestly, and they have different answers.
        "horizon": "iq-ring",
        "served_span_hz": served_span_hz,
        "max_live_span_hz": max_live_span_hz,
        "level": r.level,
        "levels": levels,
        // The drawn cell, and the tier cell it was folded from. When `src_t_cell_s` is the larger
        // of the two, one measured value repeats across columns (T-334's safe direction, stated).
        "t_cell_s": r.grid.t_cell_ns / 1e9,
        "f_cell_hz": r.grid.f_cell_hz,
        "src_t_cell_s": r.src_t_cell_s,
        "src_f_cell_hz": r.src_f_cell_hz,
        "requested": { "columns": columns, "rows": rows },
        "served": { "nt": r.grid.nt, "nf": r.grid.nf, "cells": r.grid.nt * r.grid.nf },
        // The fold happens here, so the grid is always exactly the shape asked for: unlike
        // `/api/history`, this route never hands back more cells than the view can draw.
        "reduced_from": { "nt": r.grid.src_nt, "nf": r.grid.src_nf },
        "matched": true,
        "over_resolved": Value::Array(Vec::new()),
    })
}

/// `GET /api/timeline`.
pub fn timeline_json(state: &ApiState, q: &Params) -> Result<Value, ApiError> {
    const ALLOWED: [&str; 5] = ["f_lo", "f_hi", "columns", "rows", "token"];
    if let Some((k, _)) = q.iter().find(|(k, _)| !ALLOWED.contains(&k.as_str())) {
        return Err(ApiError::new(
            400,
            format!("unknown parameter {k:?} (allowed: f_lo, f_hi, columns, rows)"),
        ));
    }
    let columns = count(q, "columns", DEFAULT_COLUMNS, MAX_COLUMNS)?;
    let rows = count(q, "rows", DEFAULT_ROWS, MAX_ROWS)?;
    let freq = parse_freq_only(q)?;
    let (w, reason) = capture_window(state);
    let mut out = json!({
        "window": window_json(&w, &reason),
        "region": freq.map(|f| json!({"lo_hz": f.lo_hz, "hi_hz": f.hi_hz})),
        "grid": Value::Null,
        "resolution": Value::Null,
    });
    // No band or no region means no picture — and that is said as `null`, not drawn as an empty
    // box with a plausible span. There is nothing to be honest about when there is no window.
    let (Some((t0_s, t1_s)), Some(f)) = (w.band(), freq) else {
        return Ok(out);
    };
    let region = Region {
        freq: f,
        t0_ns: (t0_s * 1e9).round() as i64,
        t1_ns: (t1_s * 1e9).round() as i64,
    };
    let max_live = crate::http::max_live_span_hz(state);
    let (grid, resolution) = crate::http::with_history(state, |p| {
        let r = crate::query::overview_read(p, &region, columns, rows)?;
        let levels = p.geometry().n_levels();
        Ok((
            grid_json(&r.grid),
            resolution_json(&r, &region, columns, rows, levels, max_live),
        ))
    })?;
    if let Some(obj) = out.as_object_mut() {
        obj.insert("grid".into(), grid);
        obj.insert("resolution".into(), resolution);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(retention_s: Option<f64>, t1_s: Option<f64>) -> CaptureWindow {
        CaptureWindow {
            enabled: true,
            retention_s,
            t1_s,
            buffered: None,
        }
    }

    #[test]
    fn the_band_is_the_configured_retention_not_what_the_ring_holds() {
        // A ring five minutes into a one-hour retention is a mostly-empty one-hour capture window.
        // Sizing the band to what it holds would make the timeline grow under the user.
        let mut c = w(Some(3600.0), Some(1_789_300_920.0));
        c.buffered = Some((1_789_300_620.0, 1_789_300_920.0));
        let (t0, t1) = c.band().unwrap();
        assert_eq!(t1 - t0, 3600.0);
        assert_eq!(t1, 1_789_300_920.0);
    }

    #[test]
    fn the_band_grows_and_shrinks_with_the_retention() {
        let edge = 1_789_300_920.0;
        let span = |r: f64| {
            let (a, b) = w(Some(r), Some(edge)).band().unwrap();
            b - a
        };
        assert_eq!(span(120.0), 120.0);
        assert_eq!(span(3600.0), 3600.0);
        assert_eq!(span(30.0), 30.0);
    }

    #[test]
    fn no_retention_or_no_live_edge_is_no_band_rather_than_a_default_one() {
        assert_eq!(w(None, Some(1.0)).band(), None);
        assert_eq!(w(Some(120.0), None).band(), None);
        assert_eq!(w(Some(0.0), Some(1.0)).band(), None);
    }
}
