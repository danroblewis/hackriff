//! One emitter's presence track (T-264, ADR-0017 stage TM-8):
//! `GET /api/inventory/<id>/presence`.
//!
//! The inventory row's `presence` object (T-284/TM-2) is a *projection through the request's
//! window*: how many intervals intersect it, time on air inside it, the latest one, and liveness.
//! This route serves the **track itself** — every interval, each with its own timespan — which is
//! what History needs for one row and what a live list must never be asked to carry.
//!
//! # Endpoint
//!
//! | Method | Path | Answers |
//! |---|---|---|
//! | GET | `/api/inventory/<id>/presence[?t0&t1]` | `{"emitter", "window", "intervals": [...], "presence", "total", "truncated"}` |
//!
//! `t0`/`t1` (Unix s, together) scope the track and supply its live edge — a window's own `t1` is
//! the caller's live edge, exactly as on `/api/inventory`, so a past window re-derives the truth of
//! its own moment instead of reading the wall clock. Without them the track is all of time up to
//! now.
//!
//! **Append-only, and decay never reaches it.** Every interval is derived from
//! `emitter_observation` — the observation ledger, which is only ever appended to — by
//! [`hk_model::Repository::presence_intervals`]. Candidate confidence (T-251/TM-6) is a ranking
//! over hypotheses that writes nothing here; nothing in this answer can be removed by it.
//!
//! **Resolution and gating.** Like `/api/inventory/<id>`: a merged id resolves to its live survivor
//! and an unknown or unparsable id is `404 not_found`. Unlike `/decode` and `/classification`, the
//! answer is **not** identity-gated: timing is data of exactly the class the row's own `presence`
//! and `recurrence` already carry unconditionally (T-284), and serving it uniformly is what keeps
//! its presence from signalling that a row's identity was withheld.

use std::sync::{MutexGuard, PoisonError};

use hk_model::{
    EmitterId, IdentityAccess, IdleGap, PresenceInterval, RepoError, Repository, TimeRange,
    Timestamp,
};
use serde_json::{Value, json};

use crate::control::{CtlRequest, CtlResponse, Fail, dispatch, refuse_route};
use crate::http::ApiState;
use crate::query::ts_s;

/// Most intervals served for one emitter (bounds the response). The newest are kept.
pub const MAX_PRESENCE_INTERVALS: usize = 5_000;

/// Resolves `(method, path)`: `None` when `path` is not a `/presence` inventory sub-path.
fn resolve(method: &str, path: &str) -> Option<Result<EmitterId, Option<&'static str>>> {
    let id = path
        .strip_prefix("/api/inventory/")?
        .strip_suffix("/presence")?;
    let Ok(id) = id.parse::<EmitterId>() else {
        return Some(Err(None));
    };
    Some(match method {
        "GET" => Ok(id),
        _ => Err(Some("GET")),
    })
}

/// Routes an emitter presence request; `None` when `path` is not one. Must run **before**
/// [`crate::inventory::route`] in the dispatch chain, for the reason [`crate::decode`] documents:
/// that module's resolver special-cases only `/promote` and `/band` and would claim this path.
pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    let id = match resolve(req.method, req.path)? {
        Ok(id) => id,
        Err(allow) => return Some(refuse_route(state, req, allow)),
    };
    Some(dispatch(
        state,
        req,
        "inventory_presence",
        false,
        |s| read(s, id, req.query),
        |_, _| Err(Fail::new(500, "failed", "not a mutating action")),
    ))
}

fn store(state: &ApiState) -> Result<MutexGuard<'_, Repository>, Fail> {
    state
        .inventory
        .as_ref()
        .map(|r| r.lock().unwrap_or_else(PoisonError::into_inner))
        .ok_or_else(|| Fail::new(503, "unavailable", "no signal inventory on this server"))
}

fn repo_fail(e: RepoError) -> Fail {
    match e {
        RepoError::Invalid(m) => Fail::invalid(m),
        RepoError::NotFound { .. } => Fail::new(404, "not_found", "no such emitter"),
        _ => Fail::new(500, "failed", "presence query failed"),
    }
}

/// `t0`/`t1` (Unix s), given together or not at all.
fn window(q: &[(String, String)]) -> Result<Option<TimeRange>, Fail> {
    let get = |k: &str| {
        q.iter()
            .find(|(a, _)| a == k)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    };
    match (get("t0"), get("t1")) {
        (None, None) => Ok(None),
        (Some(a), Some(b)) => {
            let (t0, t1) = (a.parse::<f64>(), b.parse::<f64>());
            match (t0, t1) {
                (Ok(t0), Ok(t1))
                    if t0.is_finite() && t1.is_finite() && t1 >= t0 && t0 > -4e9 && t1 < 9e9 =>
                {
                    Ok(Some(TimeRange::new(
                        Timestamp::from_unix_nanos((t0 * 1e9).round() as i64),
                        Timestamp::from_unix_nanos((t1 * 1e9).round() as i64),
                    )))
                }
                _ => Err(Fail::invalid("need t0 <= t1 (Unix seconds)")),
            }
        }
        _ => Err(Fail::invalid("t0 and t1 must be given together")),
    }
}

fn interval_json(i: &PresenceInterval) -> Value {
    json!({
        "t_start_s": ts_s(i.time.start),
        "t_end_s": ts_s(i.time.end),
        // Backend-computed: a client never derives a timespan from two fields it was handed.
        "duration_s": i.duration_s(),
        "open": i.open,
        "count": i.count,
        "sources": i.sources,
        "f_center_hz": i.f_center_hz,
    })
}

fn read(state: &ApiState, id: EmitterId, q: &[(String, String)]) -> Result<Value, Fail> {
    let w = window(q)?;
    let repo = store(state)?;
    let live = repo.live_emitter_id(id).map_err(repo_fail)?;
    // Resolves or 404s before any timing is served, so an unknown id never reads as "an emitter
    // that has never been on the air". Through the **gated** read like every other inventory
    // lookup in this crate (`inventory_api::hk_api_never_calls_the_ungated_emitter_getters`): the
    // identity it returns is not served here at all, and asking for it ungated would be a way in.
    repo.emitter_with_access(live, IdentityAccess::Standard)
        .map_err(repo_fail)?;
    let now = w.map_or_else(Timestamp::now, |w| w.end);
    let all = repo
        .presence_intervals(live, IdleGap::conservative(), now)
        .map_err(repo_fail)?;
    let selected: Vec<&PresenceInterval> = match w {
        Some(w) => all.iter().filter(|i| i.time.overlaps(&w)).collect(),
        None => all.iter().collect(),
    };
    let total = selected.len();
    // Newest first, and the cap keeps the newest: a truncated track loses its oldest end, which is
    // the end a caller can still reach by asking about an earlier window.
    let mut intervals: Vec<&PresenceInterval> = selected;
    intervals.reverse();
    let truncated = total > MAX_PRESENCE_INTERVALS;
    let listed: Vec<Value> = intervals
        .into_iter()
        .take(MAX_PRESENCE_INTERVALS)
        .map(interval_json)
        .collect();
    let projected = hk_model::presence_in_window(
        &all,
        w.unwrap_or_else(|| {
            TimeRange::new(Timestamp::from_unix_nanos(i64::MIN / 2), Timestamp::now())
        }),
    );
    Ok(json!({
        "emitter": live.to_string(),
        "window": w.map(|w| json!({"t0_s": ts_s(w.start), "t1_s": ts_s(w.end)})),
        "intervals": listed,
        "total": total,
        "truncated": truncated,
        // The same projection `/api/inventory` serves on the row, so the two surfaces can never
        // disagree about liveness for one emitter.
        "presence": json!({
            "intervals": projected.intervals,
            "on_air_s": projected.on_air_s,
            "last_interval": projected.last_interval.map(|i| json!({
                "t_start_s": ts_s(i.time.start),
                "t_end_s": ts_s(i.time.end),
                "open": i.open,
            })),
            "liveness": projected.liveness.as_str(),
            "ended_t_s": projected.ended_t.map(ts_s),
        }),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::ROUTES;

    #[test]
    fn routes_resolve_with_methods() {
        let id = EmitterId::new();
        assert_eq!(
            resolve("GET", &format!("/api/inventory/{id}/presence")),
            Some(Ok(id))
        );
        assert_eq!(
            resolve("POST", &format!("/api/inventory/{id}/presence")),
            Some(Err(Some("GET")))
        );
        assert_eq!(
            resolve("GET", "/api/inventory/not-an-id/presence"),
            Some(Err(None))
        );
        assert_eq!(resolve("GET", &format!("/api/inventory/{id}")), None);
        assert_eq!(resolve("GET", &format!("/api/inventory/{id}/decode")), None);
        assert!(
            ROUTES
                .iter()
                .any(|(m, p)| *m == "GET" && *p == "/api/inventory/{id}/presence")
        );
    }

    #[test]
    fn a_window_is_both_bounds_or_neither() {
        let pairs = |v: &[(&str, &str)]| -> Vec<(String, String)> {
            v.iter()
                .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
                .collect()
        };
        assert!(matches!(window(&pairs(&[])), Ok(None)));
        assert!(matches!(
            window(&pairs(&[("t0", "1"), ("t1", "2")])),
            Ok(Some(_))
        ));
        assert!(window(&pairs(&[("t0", "1")])).is_err());
        assert!(window(&pairs(&[("t0", "5"), ("t1", "1")])).is_err());
        assert!(window(&pairs(&[("t0", "x"), ("t1", "1")])).is_err());
    }
}
