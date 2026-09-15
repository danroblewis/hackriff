//! Occupancy routes (T-118, ADR-0012 §8): `/api/occupancy`, `/api/channels`.
//!
//! - `GET /api/occupancy?f_lo&f_hi&t0&t1[&subject=channel|band][&interval=15m|1h|span][&site]`:
//!   `OccupancyStat` rows (ADR-0012 §2) whose subject overlaps `[f_lo, f_hi]` and whose interval
//!   overlaps `[t0, t1]`. `15m` (default) and `1h` read the persisted series; `span` computes one
//!   row per subject over exactly `[t0, t1]` from the history (band ≤ 20 MHz, span ≤ 7 days).
//!   Rows are only produced where something was observed; the `coverage` object says so.
//! - `GET /api/channels?f_lo&f_hi[&site]`: the learned channel plan (blind; raster hints only).
//!
//! The engine lives in hk-pipeline; this module reaches it through [`OccupancyControl`] (the
//! `RecipeControl` pattern), `None` in `ApiState` answers 503.

use hk_model::attention::occupancy::{Channel, OccupancyStat, OccupancySubject};
use hk_model::{FreqRange, TimeRange, Timestamp};
use hk_store::occupancy::{SeriesInterval, SubjectKind};
use serde_json::{Value, json};

use crate::control::{CtlRequest, CtlResponse, Fail, refuse_route};
use crate::http::ApiState;

/// Row cap of `/api/occupancy`.
pub const MAX_OCCUPANCY_ROWS: usize = 10_000;

/// Which rows `/api/occupancy` answers from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OccupancyInterval {
    /// A persisted series.
    Series(SeriesInterval),
    /// Computed over the request span.
    Span,
}

/// A parsed `/api/occupancy` request.
#[derive(Clone, Debug, PartialEq)]
pub struct OccupancyRequest {
    /// Frequency range.
    pub freq: FreqRange,
    /// Time span.
    pub span: TimeRange,
    /// Subject filter.
    pub subject: Option<SubjectKind>,
    /// Series or span.
    pub interval: OccupancyInterval,
    /// Row cap.
    pub limit: usize,
}

/// Rows and plan context.
#[derive(Clone, Debug, PartialEq)]
pub struct OccupancyAnswer {
    /// Rows.
    pub rows: Vec<OccupancyStat>,
    /// More rows matched than the cap.
    pub truncated: bool,
    /// Level-0 cell width (channel keys are cell indices), Hz.
    pub f_cell_hz: f64,
    /// Current plan version.
    pub plan_version: u32,
}

/// The learned plan.
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelPlanAnswer {
    /// Version.
    pub version: u32,
    /// History grid scheme.
    pub scheme: u16,
    /// Level-0 cell width, Hz.
    pub f_cell_hz: f64,
    /// Channels overlapping the request.
    pub channels: Vec<Channel>,
}

/// The pipeline's occupancy engine as the API sees it.
pub trait OccupancyControl: Send + Sync {
    /// `/api/occupancy`. `Err` is a store/engine failure message (500).
    fn occupancy(&self, req: &OccupancyRequest) -> Result<OccupancyAnswer, String>;
    /// `/api/channels`.
    fn channels(&self, freq: FreqRange) -> ChannelPlanAnswer;
}

pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    let which = match req.path {
        "/api/occupancy" => 0,
        "/api/channels" => 1,
        _ => return None,
    };
    if req.method != "GET" {
        return Some(refuse_route(state, req, Some("GET")));
    }
    let Some(ctl) = state.occupancy.as_deref() else {
        return Some(Fail::new(503, "unavailable", "no occupancy engine on this server").response());
    };
    let r = if which == 0 {
        occupancy(ctl, req.query)
    } else {
        channels(ctl, req.query)
    };
    Some(match r {
        Ok(body) => CtlResponse {
            status: 200,
            body,
            allow: None,
        },
        Err(f) => f.response(),
    })
}

fn param<'a>(q: &'a [(String, String)], key: &str) -> Option<&'a str> {
    q.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn num(q: &[(String, String)], key: &str) -> Result<f64, Fail> {
    param(q, key)
        .ok_or_else(|| Fail::invalid(format!("missing {key}")))?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .ok_or_else(|| Fail::invalid(format!("{key} must be a number")))
}

fn freq(q: &[(String, String)]) -> Result<FreqRange, Fail> {
    let (lo, hi) = (num(q, "f_lo")?, num(q, "f_hi")?);
    if lo < 0.0 || hi <= lo {
        return Err(Fail::invalid("need 0 <= f_lo < f_hi"));
    }
    Ok(FreqRange::new(lo, hi))
}

fn site_ok(q: &[(String, String)]) -> Result<(), Fail> {
    // One site per run today (T-119 adds site keying); any well-formed value is accepted and rows
    // carry their `site`.
    match param(q, "site") {
        Some(s) if s.is_empty() || s.len() > 128 => Err(Fail::invalid("bad site")),
        _ => Ok(()),
    }
}

fn subject_json(s: &OccupancySubject, f_cell_hz: f64) -> Value {
    match s {
        OccupancySubject::Channel { key } => {
            let f = key.freq(f_cell_hz);
            json!({"f_lo_hz": f.lo_hz, "f_hi_hz": f.hi_hz})
        }
        OccupancySubject::Band { freq } => json!({"f_lo_hz": freq.lo_hz, "f_hi_hz": freq.hi_hz}),
    }
}

fn occupancy(ctl: &dyn OccupancyControl, q: &[(String, String)]) -> Result<Value, Fail> {
    let f = freq(q)?;
    let (t0, t1) = (num(q, "t0")?, num(q, "t1")?);
    if t1 <= t0 {
        return Err(Fail::invalid("need t0 < t1"));
    }
    site_ok(q)?;
    let ns = |s: f64| Timestamp::from_unix_nanos((s * 1e9).clamp(-9.2e18, 9.2e18) as i64);
    let subject = match param(q, "subject") {
        None => None,
        Some("channel") => Some(SubjectKind::Channel),
        Some("band") => Some(SubjectKind::Band),
        Some(_) => return Err(Fail::invalid("subject must be channel or band")),
    };
    let interval = match param(q, "interval") {
        None => OccupancyInterval::Series(SeriesInterval::Min15),
        Some("span") => OccupancyInterval::Span,
        Some(s) => OccupancyInterval::Series(
            SeriesInterval::parse(s).ok_or_else(|| Fail::invalid("interval must be 15m, 1h or span"))?,
        ),
    };
    let req = OccupancyRequest {
        freq: f,
        span: TimeRange::new(ns(t0), ns(t1)),
        subject,
        interval,
        limit: MAX_OCCUPANCY_ROWS,
    };
    let a = ctl.occupancy(&req).map_err(|e| {
        if interval == OccupancyInterval::Span && (e.contains("at most") || e.contains("positive")) {
            Fail::invalid(e)
        } else {
            Fail::new(500, "failed", e)
        }
    })?;
    let with_fco = a.rows.iter().filter(|r| r.fco.is_some()).count();
    let rows: Vec<Value> = a
        .rows
        .iter()
        .map(|r| {
            let mut v = serde_json::to_value(r).unwrap_or(Value::Null);
            if let Some(o) = v.as_object_mut() {
                o.insert("subject_extent".into(), subject_json(&r.subject, a.f_cell_hz));
            }
            v
        })
        .collect();
    Ok(json!({
        "interval": match interval {
            OccupancyInterval::Span => "span",
            OccupancyInterval::Series(SeriesInterval::Min15) => "15m",
            OccupancyInterval::Series(SeriesInterval::Hour1) => "1h",
        },
        "f_cell_hz": a.f_cell_hz,
        "plan_version": a.plan_version,
        "rows": rows,
        "truncated": a.truncated,
        "coverage": {
            "rows": a.rows.len(),
            "rows_with_fco": with_fco,
            "observed_s": a.rows.iter().map(|r| r.observed_s).sum::<f64>(),
            "unobserved_is_not_quiet": true,
        },
    }))
}

fn channels(ctl: &dyn OccupancyControl, q: &[(String, String)]) -> Result<Value, Fail> {
    let f = freq(q)?;
    site_ok(q)?;
    let a = ctl.channels(f);
    let channels: Vec<Value> = a
        .channels
        .iter()
        .map(|c| {
            let mut v = serde_json::to_value(c).unwrap_or(Value::Null);
            let e = c.key.freq(a.f_cell_hz);
            if let Some(o) = v.as_object_mut() {
                o.insert("f_lo_hz".into(), json!(e.lo_hz));
                o.insert("f_hi_hz".into(), json!(e.hi_hz));
            }
            v
        })
        .collect();
    Ok(json!({
        "plan_version": a.version,
        "scheme": a.scheme,
        "f_cell_hz": a.f_cell_hz,
        "source": "learned-from-detections",
        "channels": channels,
    }))
}
