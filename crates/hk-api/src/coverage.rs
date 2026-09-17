//! `GET /api/coverage` — the coverage map: which front end actually sampled which frequency, and
//! therefore which cells are grey because nothing ever looked (T-368).
//!
//! # The rule this serves
//!
//! The user's invariant (CLAUDE.md, "Time, the waterfall, and the live view"):
//!
//! > **The waterfall shows the data that exists for the selected (time, frequency); grey means
//! > genuinely unobserved.** … This requires the backend to keep a **coverage map derived from the
//! > SDR configuration/tune history** — for each interval, which centre/span/rate (and which
//! > device) was active — so observed-vs-unobserved is computed from what was actually sampled, and
//! > the frequency navigator's survey view is built from that same coverage.
//!
//! T-341 stopped the view *claiming* detail the front end never captured. This is the other half:
//! the view may *show* what the front end did capture, and must grey only what it did not. Between
//! them sits the failure this route exists to prevent — **painting never-observed spectrum as
//! quiet**, which invents an absence-of-signal finding out of an absence of measurement.
//!
//! # Three states on the wire, and the third cannot be spelled as the second
//!
//! Each cell is one of:
//!
//! ```jsonc
//! { "state": "unobserved" }                                  // 3: nothing ever looked. Grey.
//! { "state": "observed", "duty": 0.5, "observed_s": 30.0, … } // 1 or 2: the radio was here.
//! ```
//!
//! An unobserved cell carries **no measurement keys at all** — not `null` ones. That is stronger
//! than a nullable number, because there is no field a client can read as zero: the absence is
//! structural. It comes straight from [`hk_store::Coverage`], whose only observed-constructor refuses to
//! mint an observation out of nothing, so state 3 is unrepresentable as state 2 in the type as well
//! as on the wire.
//!
//! Whether an observed cell was *quiet* is a different question with a different answer: `shade`,
//! the level the spectrum history holds there. `shade: null` on an **observed** cell means the
//! history kept no level for it — sampled, level not retained — and a client must draw that
//! differently from grey. **Grey is `state == "unobserved"` and nothing else.**
//!
//! # Where the map comes from: provenance already written
//!
//! Nothing new is journalled for this. Two records already say "for each interval, which
//! centre/span/rate was active", and one of them also says which device:
//!
//! | Source | Interval | Centre/span/rate | Device | Horizon |
//! |---|---|---|---|---|
//! | IQ ring journal (`/api/iqbuffer` segments, ADR-0014) | yes | yes | **yes** (`device_id`) | the ring's retention |
//! | observation log (`DwellRecord`/`SweepRecord`, ADR-0012 §1) | yes | yes (`ObservedWindow`) | no | 30 days |
//!
//! The ring journal opens a new segment on **every** provenance change, so retunes are segment
//! boundaries by construction — it is already a tune history, and it is the only one that names the
//! radio. The observation log extends the horizon far beyond the ring but records no `device_id`
//! today, so its spans are [`Device::Unknown`]: evidence that *something* looked, never evidence
//! that a *particular* front end did. The two are kept apart for exactly that reason, and `sources`
//! in the response says which contributed.
//!
//! | Method | Path | Query | Answers |
//! |---|---|---|---|
//! | GET | `/api/coverage` | `f_lo`&`f_hi` (Hz, required), `cells`? (1…4096, default 256), `t0`&`t1`? (Unix s; default the capture window) | `{region, window, grid, devices, any, sources, resolution}` |

use hk_model::attention::observation::{ObservationRecord, ObservedWindow};
use hk_model::{FreqRange, TimeRange, Timestamp};
use hk_store::coverage::{Coverage, CoverageGrid, CoverageSpan, Device, MAX_COVERAGE_CELLS};
use hk_store::observation::{MAX_RECORD_LIMIT, ObservationStore, RecordQuery};
use serde_json::{Value, json};

use crate::http::ApiState;
use crate::query::{ApiError, Params, count, parse_freq_only};

/// Frequency cells the survey strip is drawn in when the caller names none.
pub const DEFAULT_CELLS: usize = 256;

/// Segments read from the IQ ring journal for one answer.
const RING_SEGMENTS: usize = 10_000;

fn f64_of(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64).filter(|x| x.is_finite())
}

/// The tune history the IQ ring journal already holds: one segment per provenance, so one per
/// retune, each naming the device that produced it (ADR-0014).
///
/// A segment whose tuned window misses the region is dropped here rather than folded: coverage of
/// somewhere else is not coverage of here.
fn ring_spans(state: &ApiState, freq: FreqRange, window: TimeRange) -> Vec<CoverageSpan> {
    let Some(c) = state.iq_buffer.as_deref() else {
        return Vec::new();
    };
    let status = c.status(&crate::iqbuffer::IqBufferQuery {
        t0: Some(window.start.as_unix_nanos() as f64 * 1e-9),
        t1: Some(window.end.as_unix_nanos() as f64 * 1e-9),
        limit: RING_SEGMENTS,
    });
    let Some(segments) = status.get("segments").and_then(Value::as_array) else {
        return Vec::new();
    };
    segments
        .iter()
        .filter_map(|s| {
            let center_hz = f64_of(s, "center_hz")?;
            let rate = f64_of(s, "sample_rate_hz").filter(|r| *r > 0.0)?;
            let (t0, t1) = (
                s.get("t0_ns").and_then(Value::as_i64)?,
                s.get("t1_ns").and_then(Value::as_i64)?,
            );
            let half = rate / 2.0;
            let f = FreqRange::new(center_hz - half, center_hz + half);
            if !f.overlaps(&freq) {
                return None;
            }
            Some(CoverageSpan {
                // The journal names the radio, so this span is device-local. A segment without one
                // is `Unknown` rather than borrowing whichever device happens to be running.
                device: s
                    .get("device_id")
                    .and_then(Value::as_str)
                    .filter(|d| !d.is_empty())
                    .map_or(Device::Unknown, |d| Device::Id(d.to_string())),
                time: TimeRange::new(
                    Timestamp::from_unix_nanos(t0),
                    Timestamp::from_unix_nanos(t1),
                ),
                freq: f,
                center_hz,
                sample_rate_hz: rate,
            })
        })
        .collect()
}

/// The tune history the observation log holds: every dwell's and every sweep hop's analysed extent
/// over the interval it was analysed in (ADR-0012 §1).
///
/// These records carry no `device_id` today, so every span is [`Device::Unknown`] — which is a
/// device of its own and never answers for a named front end. `ObservedWindow::covered()` already
/// removes the DC notch, so a notched window contributes two spans and the notch stays honestly
/// unobserved.
fn observation_spans(
    store: &ObservationStore,
    freq: FreqRange,
    window: TimeRange,
) -> Vec<CoverageSpan> {
    let page = store.query(&RecordQuery {
        freq,
        span: window,
        tier: None,
        cursor: 0,
        limit: MAX_RECORD_LIMIT,
    });
    let mut out = Vec::new();
    let mut push = |w: &ObservedWindow, t: TimeRange| {
        for c in w.covered() {
            if c.overlaps(&freq) {
                out.push(CoverageSpan {
                    device: Device::Unknown,
                    time: t,
                    freq: c,
                    center_hz: w.center_hz,
                    sample_rate_hz: w.sample_rate_hz,
                });
            }
        }
    };
    for r in &page.records {
        match r {
            ObservationRecord::Dwell(d) => push(&d.window, d.observed),
            ObservationRecord::Sweep(s) => {
                let Some(g) = page.geometries.iter().find(|g| g.id == s.geometry) else {
                    continue;
                };
                for v in &s.visits {
                    let Some(w) = g.hops.get(v.hop as usize) else {
                        continue;
                    };
                    let start = s
                        .span
                        .start
                        .saturating_add_nanos(i64::from(v.start_ms) * 1_000_000);
                    let end = start.saturating_add_nanos(i64::from(v.observed_ms) * 1_000_000);
                    if end > start {
                        push(w, TimeRange::new(start, end));
                    }
                }
            }
            ObservationRecord::Geometry(_) => {}
        }
    }
    out
}

/// One cell's JSON. An unobserved cell carries **no measurement keys**, so there is nothing a
/// client can read as a zero level or a zero occupancy.
fn cell_json(c: &Coverage, shade: Option<f32>) -> Value {
    match c.sampled() {
        None => json!({ "state": "unobserved" }),
        Some(s) => json!({
            "state": "observed",
            "spans": s.spans,
            "observed_s": s.observed_ns as f64 * 1e-9,
            "duty": s.duty,
            "last_s": s.last.as_unix_nanos() as f64 * 1e-9,
            "center_hz": s.center_hz,
            "sample_rate_hz": s.sample_rate_hz,
            // The level the spectrum history holds here, normalised to this answer's own observed
            // range so the client never picks a colour scale from the numbers it happens to hold.
            // `null` = sampled, level not retained — drawn differently from grey, never as grey and
            // never as the bottom of the ramp.
            "shade": shade.map(|v| json!(v)).unwrap_or(Value::Null),
        }),
    }
}

fn grid_json(g: &CoverageGrid, shades: Option<&[Option<f32>]>) -> Value {
    let cells: Vec<Value> = g
        .cells
        .iter()
        .enumerate()
        .map(|(i, c)| cell_json(c, shades.and_then(|s| s.get(i)).copied().flatten()))
        .collect();
    json!({
        "device": g.device.as_str(),
        // Whether `device` is a real front-end identity. `"unknown"` and `"any"` are labels, not
        // radios, and a client must not attribute their coverage to a device.
        "named": g.device.is_named(),
        "observed_cells": g.observed_cells(),
        "unobserved_cells": g.unobserved_cells(),
        "observed_fraction": g.observed_fraction(),
        "cells": cells,
    })
}

/// The level the spectrum history holds for each cell of the strip, normalised to the strip's own
/// observed range. `None` per cell where the history has nothing.
///
/// This is only the *shading*: it never decides observed-versus-unobserved. A cell the radio
/// demonstrably sampled but whose level the pyramid no longer keeps is observed with no shade, and
/// a cell the pyramid happens to hold a value for is still grey if no tune ever covered it.
fn shades(state: &ApiState, freq: FreqRange, window: TimeRange, cells: usize) -> Vec<Option<f32>> {
    let region = crate::query::Region {
        freq,
        t0_ns: window.start.as_unix_nanos(),
        t1_ns: window.end.as_unix_nanos(),
    };
    crate::http::with_history(state, |p| {
        Ok(crate::query::overview_read(p, &region, 1, cells)?.grid)
    })
    .map(|o| {
        let range = o.range_db;
        o.cells
            .iter()
            .map(|c| match (range, c.observed()) {
                (Some((lo, hi)), true) if c.max_db.is_finite() => {
                    let span = if hi > lo { hi - lo } else { 1.0 };
                    Some(((c.max_db - lo) / span).clamp(0.0, 1.0))
                }
                _ => None,
            })
            .collect()
    })
    .unwrap_or_else(|_| vec![None; cells])
}

/// `GET /api/coverage?f_lo&f_hi[&cells][&t0&t1]`.
pub fn coverage_json(state: &ApiState, q: &Params) -> Result<Value, ApiError> {
    const ALLOWED: [&str; 6] = ["f_lo", "f_hi", "cells", "t0", "t1", "token"];
    if let Some((k, _)) = q.iter().find(|(k, _)| !ALLOWED.contains(&k.as_str())) {
        return Err(ApiError::new(
            400,
            format!("unknown parameter {k:?} (allowed: f_lo, f_hi, cells, t0, t1)"),
        ));
    }
    let cells = count(q, "cells", DEFAULT_CELLS, MAX_COVERAGE_CELLS)?;
    let freq = parse_freq_only(q)?
        .ok_or_else(|| ApiError::new(400, "f_lo and f_hi are required, in Hz"))?;
    let (window, window_source) = window_of(state, q)?;

    let mut spans = ring_spans(state, freq, window);
    let ring = spans.len();
    if let Some(store) = state.observations.as_ref() {
        spans.extend(observation_spans(store, freq, window));
    }
    let log = spans.len() - ring;

    let shades = shades(state, freq, window, cells);
    let any = hk_store::coverage::union_grid(&spans, freq, window, cells);
    let devices: Vec<Value> = hk_store::coverage::by_device(&spans, freq, window, cells)
        .iter()
        .map(|g| grid_json(g, Some(&shades)))
        .collect();

    let source = crate::navigation::live_window_verdict(
        freq.width_hz(),
        crate::http::max_live_span_hz(state),
    );
    Ok(json!({
        "region": { "lo_hz": freq.lo_hz, "hi_hz": freq.hi_hz },
        "window": {
            "t0_s": window.start.as_unix_nanos() as f64 * 1e-9,
            "t1_s": window.end.as_unix_nanos() as f64 * 1e-9,
            "span_s": window.duration_ns() as f64 * 1e-9,
            // Where the window came from: the caller, or the capture window this server holds.
            "source": window_source,
        },
        "grid": { "cells": cells, "f_lo_hz": any.f_lo_hz, "f_cell_hz": any.f_cell_hz },
        // One entry per front end that actually sampled here, never merged. Empty means nothing on
        // this server can say what was sampled — which is **not** the same as "nothing was".
        "devices": devices,
        // The deliberate union, labelled `"any"` so it can never be mistaken for one radio's
        // coverage (T-259/T-305: device-local physics reads the device).
        "any": grid_json(&any, Some(&shades)),
        // Which tune histories answered. A source with no spans still appears, so a client can tell
        // "this record had nothing here" from "this record was not consulted".
        "sources": [
            { "kind": "iq-ring", "spans": ring, "device_known": true,
              "available": state.iq_buffer.is_some() },
            { "kind": "observation-log", "spans": log, "device_known": false,
              "available": state.observations.is_some() },
        ],
        "resolution": {
            "source": source.as_str(),
            "live": source.is_live(),
            "statement": source.statement(),
            "served_span_hz": freq.width_hz(),
            "max_live_span_hz": crate::http::max_live_span_hz(state),
            // Grey is decided here and nowhere else.
            "grey_rule": "grey a cell if and only if its state is \"unobserved\"",
        },
    }))
}

/// The window: the caller's `t0`/`t1` when both are given, else the capture window this server
/// holds. An answer with no window at all is refused rather than defaulted to a plausible span.
fn window_of(state: &ApiState, q: &Params) -> Result<(TimeRange, &'static str), ApiError> {
    let num = |k: &str| -> Result<Option<f64>, ApiError> {
        match q.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str()) {
            None => Ok(None),
            Some(raw) => raw
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .map(Some)
                .ok_or_else(|| ApiError::new(400, format!("{k} must be a finite number"))),
        }
    };
    match (num("t0")?, num("t1")?) {
        (Some(a), Some(b)) if b > a => Ok((
            TimeRange::new(
                Timestamp::from_unix_nanos((a * 1e9).round() as i64),
                Timestamp::from_unix_nanos((b * 1e9).round() as i64),
            ),
            "requested",
        )),
        (None, None) => {
            let (w, _) = crate::timeline::capture_window(state);
            let (t0, t1) = w.band().ok_or_else(|| {
                ApiError::new(
                    404,
                    "no capture window on this server; give t0 and t1 to ask for one",
                )
            })?;
            Ok((
                TimeRange::new(
                    Timestamp::from_unix_nanos((t0 * 1e9).round() as i64),
                    Timestamp::from_unix_nanos((t1 * 1e9).round() as i64),
                ),
                "capture-window",
            ))
        }
        _ => Err(ApiError::new(
            400,
            "t0 and t1 must be given together, t1 > t0",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: i64) -> Timestamp {
        Timestamp::from_unix_nanos(s * 1_000_000_000)
    }

    #[test]
    fn an_unobserved_cell_carries_no_measurement_keys_at_all() {
        // The wire form of state 3: nothing a client can read as a zero level, a zero duty or a
        // zero occupancy. Not a null measurement — no measurement.
        let v = cell_json(&Coverage::Unobserved, Some(0.9));
        assert_eq!(v, json!({ "state": "unobserved" }));
        assert!(v.get("duty").is_none());
        assert!(v.get("observed_s").is_none());
        assert!(v.get("shade").is_none());
    }

    #[test]
    fn an_observed_cell_states_the_sampling_that_makes_it_observed() {
        let c =
            hk_store::coverage::Coverage::of(2, 30_000_000_000, 60_000_000_000, t(1030), 1e8, 2e6);
        let v = cell_json(&c, None);
        assert_eq!(v["state"], json!("observed"));
        assert_eq!(v["spans"], json!(2));
        // Floating seconds, so compare as a number rather than by JSON equality.
        assert!(
            (v["observed_s"].as_f64().unwrap() - 30.0).abs() < 1e-9,
            "{v}"
        );
        assert_eq!(v["duty"], json!(0.5));
        assert_eq!(v["last_s"], json!(1030.0));
        // Sampled, level not retained: an explicit null on an *observed* cell, which is a
        // different thing from grey and must be drawn differently.
        assert_eq!(v["shade"], Value::Null);
        assert_ne!(v["state"], cell_json(&Coverage::Unobserved, None)["state"]);
    }
}
