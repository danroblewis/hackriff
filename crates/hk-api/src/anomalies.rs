//! Anomaly and alarm routes (T-122, ADR-0012 §8), documented in `docs/api.md` "Anomalies and
//! novelty alarms":
//!
//! - `GET /api/anomalies?[f_lo&f_hi][&t0&t1][&kind][&status][&cursor][&limit]`: anomalies of every
//!   kind (floor rises and novelty alarms), newest first, each with its current status, alarm
//!   detail (alarms only) and top explanations; plus the per-kind suppression counts.
//! - `GET /api/anomalies/{id}`: one anomaly with every current explanation (best first) and its
//!   status history.
//! - `POST /api/anomalies/{id}/dismiss` (`{"note"?}`): dismiss a novelty alarm; its key is
//!   suppressed for 7 days of sample time (audited).
//! - `POST /api/anomalies/{id}/reopen` (`{}`): lift a dismissal and mark it open (audited).
//!
//! Times are Unix seconds on the sample clock. The alarm engine lives in hk-pipeline
//! ([`AnomalyControl`] is implemented there, the `RecipeControl` pattern).

use std::str::FromStr;

use hk_model::repo::alarms::{
    AnomalyQuery, AnomalyView, TOP_EXPLANATIONS, anomaly_view_json as view_json,
};
use hk_model::{AnomalyId, AnomalyKind, AnomalyStatus, FreqRange, TimeRange, Timestamp};
use serde_json::{Map, Value, json};

use crate::control::{Applied, CtlRequest, CtlResponse, Fail, dispatch, refuse_route};
use crate::http::ApiState;

/// Default page size of `/api/anomalies`.
pub const ANOMALIES_DEFAULT_LIMIT: usize = 100;

/// A refused or failed call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnomalyFail {
    /// No such anomaly.
    NotFound,
    /// Not applicable (e.g. dismissing a floor episode, reopening a non-dismissed alarm).
    Conflict(String),
    /// Storage failure.
    Failed(String),
}

/// The run's anomalies and alarm engine.
pub trait AnomalyControl: Send + Sync {
    /// A page of anomalies with their top explanations, and the next cursor.
    fn list(&self, q: &AnomalyQuery) -> Result<(Vec<AnomalyView>, Option<usize>), AnomalyFail>;
    /// One anomaly with all current explanations and its history.
    fn get(&self, id: AnomalyId) -> Result<AnomalyView, AnomalyFail>;
    /// Dismisses a novelty alarm.
    fn dismiss(&self, id: AnomalyId, note: Option<String>) -> Result<AnomalyView, AnomalyFail>;
    /// Lifts a dismissal and re-opens the alarm.
    fn reopen(&self, id: AnomalyId) -> Result<AnomalyView, AnomalyFail>;
    /// Suppression counts per kind (ADR-0012 §7.3).
    fn suppressions(&self) -> Value;
}

#[derive(Clone, Copy)]
enum Action {
    List,
    Get(AnomalyId),
    Dismiss(AnomalyId),
    Reopen(AnomalyId),
}

pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    let rest = req.path.strip_prefix("/api/anomalies")?;
    let (action, allow) = if rest.is_empty() {
        (Some(Action::List), "GET")
    } else {
        let rest = rest.strip_prefix('/')?;
        let (id, verb) = match rest.split_once('/') {
            Some((id, verb)) => (id, Some(verb)),
            None => (rest, None),
        };
        let parsed = AnomalyId::from_str(id).ok();
        match verb {
            None => (parsed.map(Action::Get), "GET"),
            Some("dismiss") => (parsed.map(Action::Dismiss), "POST"),
            Some("reopen") => (parsed.map(Action::Reopen), "POST"),
            Some(_) => return None,
        }
    };
    if req.method != allow {
        return Some(refuse_route(state, req, Some(allow)));
    }
    let Some(action) = action else {
        return Some(Fail::invalid("anomaly id must be a UUID").response());
    };
    let (name, mutating) = match action {
        Action::List => ("anomalies", false),
        Action::Get(_) => ("anomaly", false),
        Action::Dismiss(_) => ("anomaly_dismiss", true),
        Action::Reopen(_) => ("anomaly_reopen", true),
    };
    Some(dispatch(
        state,
        req,
        name,
        mutating,
        |s| read(s, req, action),
        |s, body| apply(s, action, body),
    ))
}

fn control(state: &ApiState) -> Result<&dyn AnomalyControl, Fail> {
    state
        .anomalies
        .as_deref()
        .ok_or_else(|| Fail::new(503, "unavailable", "no anomaly service on this server"))
}

fn fail(e: AnomalyFail) -> Fail {
    match e {
        AnomalyFail::NotFound => Fail::new(404, "not_found", "no such anomaly"),
        AnomalyFail::Conflict(m) => Fail::new(409, "conflict", m),
        AnomalyFail::Failed(m) => Fail::new(500, "failed", m),
    }
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

fn pair(req: &CtlRequest<'_>, a: &str, b: &str) -> Result<Option<(f64, f64)>, Fail> {
    match (number(req, a)?, number(req, b)?) {
        (None, None) => Ok(None),
        (Some(x), Some(y)) if y > x => Ok(Some((x, y))),
        _ => Err(Fail::invalid(format!(
            "{a} and {b} must be given together with {a} below {b}"
        ))),
    }
}

fn ts(s: f64) -> Timestamp {
    Timestamp::from_unix_nanos((s * 1e9).round() as i64)
}

fn kind_param(req: &CtlRequest<'_>) -> Result<Option<AnomalyKind>, Fail> {
    param(req, "kind")
        .map(|v| {
            serde_json::from_value::<AnomalyKind>(json!(v)).map_err(|_| {
                Fail::invalid(
                    "kind must be one of new-emitter, busier-than-baseline, \
                     quieter-than-baseline, noise-floor-rise, novelty, level-above-baseline, \
                     change-point",
                )
            })
        })
        .transpose()
}

fn status_param(req: &CtlRequest<'_>) -> Result<Option<AnomalyStatus>, Fail> {
    param(req, "status")
        .map(|v| {
            serde_json::from_value::<AnomalyStatus>(json!(v))
                .map_err(|_| Fail::invalid("status must be one of open, resolved, dismissed"))
        })
        .transpose()
}

fn read(state: &ApiState, req: &CtlRequest<'_>, action: Action) -> Result<Value, Fail> {
    let ctl = control(state)?;
    match action {
        Action::List => {
            let freq = pair(req, "f_lo", "f_hi")?.map(|(a, b)| FreqRange::new(a, b));
            let time = pair(req, "t0", "t1")?.map(|(a, b)| TimeRange::new(ts(a), ts(b)));
            let kind = kind_param(req)?;
            let status = status_param(req)?;
            let cursor = number(req, "cursor")?.unwrap_or(0.0);
            let limit = number(req, "limit")?.unwrap_or(ANOMALIES_DEFAULT_LIMIT as f64);
            if cursor < 0.0 || limit < 1.0 {
                return Err(Fail::invalid("cursor must be ≥ 0 and limit ≥ 1"));
            }
            let q = AnomalyQuery {
                freq,
                time,
                kind,
                status,
                cursor: cursor as usize,
                limit: (limit as usize).min(hk_model::repo::alarms::ANOMALY_PAGE_MAX),
            };
            let (rows, next) = ctl.list(&q).map_err(fail)?;
            Ok(json!({
                "anomalies": rows.iter().map(|v| view_json(v, Some(TOP_EXPLANATIONS))).collect::<Vec<_>>(),
                "next_cursor": next,
                "truncated": next.is_some(),
                "suppressions": ctl.suppressions(),
            }))
        }
        Action::Get(id) => ctl.get(id).map(|v| view_json(&v, None)).map_err(fail),
        Action::Dismiss(_) | Action::Reopen(_) => unreachable!("mutating"),
    }
}

fn apply(state: &ApiState, action: Action, body: &Map<String, Value>) -> Result<Applied, Fail> {
    let ctl = control(state)?;
    let (id, allowed) = match action {
        Action::Dismiss(id) => (id, &["note"][..]),
        Action::Reopen(id) => (id, &[][..]),
        _ => unreachable!("reads"),
    };
    if let Some(k) = body.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(Fail::invalid(format!("unknown field {k:?}")));
    }
    let old = ctl.get(id).map_err(fail)?;
    let new = match action {
        Action::Dismiss(_) => {
            let note = match body.get("note") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) if s.chars().count() <= 500 => Some(s.clone()),
                Some(_) => return Err(Fail::invalid("note must be a string of ≤ 500 characters")),
            };
            ctl.dismiss(id, note)
        }
        _ => ctl.reopen(id),
    }
    .map_err(fail)?;
    let summary = |v: &AnomalyView| {
        json!({
            "status": v.listing.status,
            "state": v.listing.alarm.as_ref().map(|a| a.state),
        })
    };
    Ok(Applied {
        status: 200,
        body: json!({ "anomaly": view_json(&new, None) }),
        old: summary(&old),
        new: summary(&new),
    })
}
