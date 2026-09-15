//! Observation log routes (T-115, ADR-0012 §8): `/api/observations`,
//! `/api/observations/coverage` (documented in `docs/api.md` "Observation log").
//!
//! - `GET /api/observations?f_lo&f_hi&t0&t1[&tier][&cursor][&limit]`: dwell and sweep records
//!   overlapping the box, in log order, with the sweep geometries the page references.
//! - `GET /api/observations/coverage?f_lo&f_hi&t0&t1[&channel_hz][&tau_s][&min_gap_s]`:
//!   [`ObservationTotals`](hk_model::attention::observation::ObservationTotals) of the range (it
//!   counts as observed only while entirely inside a covered extent), per-channel totals when
//!   `channel_hz` tiles the range, unobserved gaps, and POI rows for burst durations `tau_s`
//!   (comma-separated seconds).
//!
//! `f_lo`/`f_hi` are Hz; `t0`/`t1` are Unix seconds on the sample clock the records carry.

use hk_model::attention::observation::Tier;
use hk_model::attention::schedule::{PoiEntry, poi_fraction};
use hk_model::{FreqRange, TimeRange, Timestamp};
use hk_store::observation::{
    DEFAULT_RECORD_LIMIT, MAX_RECORD_LIMIT, ObservationStore, RecordQuery, coverage_gaps,
    totals_from_visits,
};
use serde_json::{Value, json};

use crate::control::{CtlRequest, CtlResponse, Fail, refuse_route};
use crate::http::ApiState;

/// Most channels `/api/observations/coverage` tiles a range into.
pub const MAX_COVERAGE_CHANNELS: usize = 4096;
/// Most gaps `/api/observations/coverage` lists.
pub const MAX_COVERAGE_GAPS: usize = 1000;
/// Most burst durations `/api/observations/coverage` accepts.
pub const MAX_TAU: usize = 16;

pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    let read: fn(&ObservationStore, &CtlRequest<'_>) -> Result<Value, Fail> = match req.path {
        "/api/observations" => records,
        "/api/observations/coverage" => coverage,
        _ => return None,
    };
    if req.method != "GET" {
        return Some(refuse_route(state, req, Some("GET")));
    }
    let Some(store) = &state.observations else {
        return Some(Fail::new(503, "unavailable", "no observation log on this server").response());
    };
    Some(match read(store, req) {
        Ok(body) => CtlResponse {
            status: 200,
            body,
            allow: None,
        },
        Err(f) => f.response(),
    })
}

fn param<'a>(req: &'a CtlRequest<'_>, key: &str) -> Option<&'a str> {
    req.query
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn number(req: &CtlRequest<'_>, key: &str) -> Result<Option<f64>, Fail> {
    param(req, key)
        .map(|v| {
            v.parse::<f64>()
                .ok()
                .filter(|x| x.is_finite())
                .ok_or_else(|| Fail::invalid(format!("{key} must be a number")))
        })
        .transpose()
}

fn required(req: &CtlRequest<'_>, key: &str) -> Result<f64, Fail> {
    number(req, key)?.ok_or_else(|| Fail::invalid(format!("{key} is required")))
}

fn ts(s: f64) -> Timestamp {
    Timestamp::from_unix_nanos((s * 1e9).round() as i64)
}

fn secs(t: Timestamp) -> f64 {
    t.as_unix_nanos() as f64 * 1e-9
}

/// The `f_lo&f_hi&t0&t1` box.
fn bounds(req: &CtlRequest<'_>) -> Result<(FreqRange, TimeRange), Fail> {
    let (f_lo, f_hi) = (required(req, "f_lo")?, required(req, "f_hi")?);
    let (t0, t1) = (required(req, "t0")?, required(req, "t1")?);
    if !(f_lo >= 0.0 && f_hi > f_lo) {
        return Err(Fail::invalid("f_lo must be ≥ 0 and below f_hi"));
    }
    if t1 <= t0 {
        return Err(Fail::invalid("t0 must be before t1"));
    }
    Ok((FreqRange::new(f_lo, f_hi), TimeRange::new(ts(t0), ts(t1))))
}

fn records(store: &ObservationStore, req: &CtlRequest<'_>) -> Result<Value, Fail> {
    let (freq, span) = bounds(req)?;
    let tier = param(req, "tier")
        .map(|t| {
            serde_json::from_value::<Tier>(json!(t)).map_err(|_| {
                Fail::invalid(
                    "tier must be interactive, pinned-lease, scheduled-plan, bandit or \
                     background-sweep",
                )
            })
        })
        .transpose()?;
    let cursor = number(req, "cursor")?.unwrap_or(0.0);
    let limit = number(req, "limit")?.unwrap_or(DEFAULT_RECORD_LIMIT as f64);
    if cursor < 0.0 || limit < 1.0 {
        return Err(Fail::invalid("cursor must be ≥ 0 and limit ≥ 1"));
    }
    let page = store.query(&RecordQuery {
        freq,
        span,
        tier,
        cursor: cursor as usize,
        limit: (limit as usize).min(MAX_RECORD_LIMIT),
    });
    Ok(json!({
        "f_lo": freq.lo_hz,
        "f_hi": freq.hi_hz,
        "t0": secs(span.start),
        "t1": secs(span.end),
        "records": page.records,
        "geometries": page.geometries,
        "next_cursor": page.next_cursor,
        "truncated": page.next_cursor.is_some(),
        "log": log_json(store),
    }))
}

fn log_json(store: &ObservationStore) -> Value {
    let mut v = store.stats().to_json();
    v["bytes"] = json!(store.bytes());
    v
}

fn coverage(store: &ObservationStore, req: &CtlRequest<'_>) -> Result<Value, Fail> {
    let (freq, span) = bounds(req)?;
    let min_gap_s = number(req, "min_gap_s")?.unwrap_or(0.0).max(0.0);
    let taus: Vec<f64> = match param(req, "tau_s") {
        None => Vec::new(),
        Some(list) => list
            .split(',')
            .map(|s| {
                s.trim()
                    .parse::<f64>()
                    .ok()
                    .filter(|x| x.is_finite() && *x >= 0.0)
            })
            .collect::<Option<Vec<f64>>>()
            .filter(|v| v.len() <= MAX_TAU)
            .ok_or_else(|| {
                Fail::invalid(format!(
                    "tau_s must be up to {MAX_TAU} comma-separated seconds ≥ 0"
                ))
            })?,
    };
    let channels: Vec<FreqRange> = match number(req, "channel_hz")? {
        None => Vec::new(),
        Some(w) if w > 0.0 && (freq.width_hz() / w).ceil() as usize <= MAX_COVERAGE_CHANNELS => {
            let n = (freq.width_hz() / w).ceil() as usize;
            (0..n)
                .map(|i| {
                    let lo = freq.lo_hz + i as f64 * w;
                    FreqRange::new(lo, (lo + w).min(freq.hi_hz))
                })
                .collect()
        }
        Some(_) => {
            return Err(Fail::invalid(format!(
                "channel_hz must be > 0 and tile f_lo..f_hi into at most \
                 {MAX_COVERAGE_CHANNELS} channels"
            )));
        }
    };
    let visits = store.observations_of(freq, span);
    let totals = totals_from_visits(freq, span, &visits);
    let mut gaps = coverage_gaps(span, &visits, (min_gap_s * 1e9) as i64);
    let gaps_truncated = gaps.len() > MAX_COVERAGE_GAPS;
    gaps.truncate(MAX_COVERAGE_GAPS);
    let observed: Vec<TimeRange> = visits.iter().map(|v| v.observed).collect();
    let poi: Vec<PoiEntry> = taus
        .iter()
        .map(|&tau_s| PoiEntry {
            tau_s,
            p_poi: poi_fraction(&observed, span, (tau_s * 1e9) as i64),
            rate_hz: None,
            p_at_least_one: None,
        })
        .collect();
    let per_channel = if channels.is_empty() {
        Value::Null
    } else {
        json!(store.totals(&channels, span))
    };
    Ok(json!({
        "f_lo": freq.lo_hz,
        "f_hi": freq.hi_hz,
        "t0": secs(span.start),
        "t1": secs(span.end),
        "totals": totals,
        "channels": per_channel,
        "gaps": gaps
            .iter()
            .map(|g| json!({ "t0": secs(g.start), "t1": secs(g.end) }))
            .collect::<Vec<_>>(),
        "gaps_truncated": gaps_truncated,
        "poi": poi,
    }))
}
