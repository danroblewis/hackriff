//! Front-end events over a time window (T-981), documented in `docs/api.md`
//! "`GET /api/frontend/events`".
//!
//! `GET /api/frontend/events?t0=&t1=[&device=][&limit=]`: the front-end events — runs of spectrum
//! rows whose samples clipped **and** whose energy stepped up across the whole tuned window — that
//! overlap `[t0, t1)` (Unix s, capture clock). Each is a time–frequency region (`t0..t1` over the
//! tuned window `f_lo_hz..f_hi_hz`), so the canvas marks it at its own place on the time axis as
//! the **front end's** energy, distinct from every signal mark. The judgement is the pipeline's
//! (`hk_pipeline::frontend`); this crate sees JSON through [`FrontEndControl`] and never names the
//! pipeline. Read-only, so it skips [`crate::control::dispatch`] like [`crate::vlf`].

use serde_json::{Value, json};

use crate::control::{CtlRequest, CtlResponse, Fail, refuse_route};
use crate::http::ApiState;

/// Default and largest `limit`.
pub const DEFAULT_LIMIT: usize = 256;
/// See [`DEFAULT_LIMIT`].
pub const MAX_LIMIT: usize = 1024;

/// The run's front-end event log, as the API sees it.
pub trait FrontEndControl: Send + Sync {
    /// The events overlapping `[t0_s, t1_s)`, oldest first, each in the documented shape.
    fn events(&self, t0_s: f64, t1_s: f64) -> Vec<Value>;
    /// What the log holds: `{capacity, retained, oldest_s, evicted}`, and the rule in force.
    fn log(&self) -> Value;
}

pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    if req.path != "/api/frontend/events" {
        return None;
    }
    if req.method != "GET" {
        return Some(refuse_route(state, req, Some("GET")));
    }
    Some(match read(state, req.query) {
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

fn read(state: &ApiState, q: &[(String, String)]) -> Result<Value, Fail> {
    for (k, _) in q {
        if !matches!(k.as_str(), "t0" | "t1" | "device" | "limit" | "token") {
            return Err(Fail::invalid("unknown query parameter"));
        }
    }
    let time = |key: &str| -> Result<f64, Fail> {
        let v =
            param(q, key).ok_or_else(|| Fail::invalid(format!("{key} is required (Unix s)")))?;
        v.parse::<f64>()
            .ok()
            .filter(|x| x.is_finite() && *x >= 0.0)
            .ok_or_else(|| Fail::invalid(format!("{key} must be a non-negative number (Unix s)")))
    };
    let (t0, t1) = (time("t0")?, time("t1")?);
    if t1 <= t0 {
        return Err(Fail::invalid("t1 must be after t0"));
    }
    let limit = match param(q, "limit") {
        None => DEFAULT_LIMIT,
        Some(v) => v
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=MAX_LIMIT).contains(n))
            .ok_or_else(|| Fail::invalid(format!("limit must be 1..={MAX_LIMIT}")))?,
    };
    let fe = state
        .frontend
        .as_ref()
        .ok_or_else(|| Fail::new(503, "unavailable", "no front-end event log on this server"))?;
    let mut events = fe.events(t0, t1);
    if let Some(device) = param(q, "device") {
        events.retain(|e| e["device_id"].as_str() == Some(device));
    }
    let total = events.len();
    // The newest `limit`: the live edge is what a following pane looks at.
    let events: Vec<Value> = events.split_off(total.saturating_sub(limit));
    Ok(json!({
        "window": { "t0": t0, "t1": t1 },
        "events": events,
        "total": total,
        "limit": limit,
        "truncated": total > limit,
        "log": fe.log(),
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::http::ROUTES;

    struct Fixed(Vec<Value>);

    impl FrontEndControl for Fixed {
        fn events(&self, t0: f64, t1: f64) -> Vec<Value> {
            self.0
                .iter()
                .filter(|e| e["t0"].as_f64().unwrap() < t1 && e["t1"].as_f64().unwrap() > t0)
                .cloned()
                .collect()
        }
        fn log(&self) -> Value {
            json!({ "capacity": 1024, "retained": self.0.len(), "oldest_s": 100.0, "evicted": 0 })
        }
    }

    fn get(state: &ApiState, method: &str, query: &[(&str, &str)]) -> CtlResponse {
        let q: Vec<(String, String)> = query
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        route(
            state,
            &CtlRequest {
                method,
                path: "/api/frontend/events",
                body: b"",
                content_type: None,
                caller: crate::control::Caller::default(),
                query: &q,
            },
        )
        .unwrap()
    }

    fn event(t0: f64, device: &str) -> Value {
        json!({ "kind": "clip", "t0": t0, "t1": t0 + 0.04, "device_id": device,
                "f_lo_hz": 914e6, "f_hi_hz": 916e6 })
    }

    #[test]
    fn events_are_answered_by_window_device_and_limit_and_bad_input_is_refused() {
        assert!(
            ROUTES
                .iter()
                .any(|(m, p)| *m == "GET" && *p == "/api/frontend/events")
        );
        let state = ApiState {
            frontend: Some(Arc::new(Fixed(vec![
                event(100.0, "hackrf:a"),
                event(105.0, "hackrf:b"),
                event(110.0, "hackrf:a"),
            ]))),
            ..ApiState::default()
        };
        let r = get(&state, "GET", &[("t0", "99"), ("t1", "106")]);
        assert_eq!(r.status, 200, "{}", r.body);
        assert_eq!(r.body["total"], 2);
        assert_eq!(r.body["truncated"], false);
        assert_eq!(r.body["window"]["t0"], 99.0);
        assert_eq!(r.body["log"]["capacity"], 1024);
        let r = get(
            &state,
            "GET",
            &[
                ("t0", "0"),
                ("t1", "200"),
                ("device", "hackrf:a"),
                ("limit", "1"),
            ],
        );
        assert_eq!(
            (r.body["total"].clone(), r.body["truncated"].clone()),
            (json!(2), json!(true))
        );
        assert_eq!(r.body["events"][0]["t0"], 110.0, "the newest is kept");
        for bad in [
            vec![("t0", "1")],
            vec![("t0", "5"), ("t1", "5")],
            vec![("t0", "-1"), ("t1", "5")],
            vec![("t0", "1"), ("t1", "5"), ("limit", "0")],
            vec![("t0", "1"), ("t1", "5"), ("f_lo", "1")],
        ] {
            assert_eq!(get(&state, "GET", &bad).status, 400, "{bad:?}");
        }
        assert_eq!(get(&state, "POST", &[]).status, 405);
        assert_eq!(
            get(&ApiState::default(), "GET", &[("t0", "1"), ("t1", "2")]).status,
            503
        );
    }
}
