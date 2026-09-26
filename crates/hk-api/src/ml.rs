//! The C38 operator surface (ADR-0016 §9, T-844): the model registry and the `(model, consumer)`
//! modes in force, a mode change, and the durable shadow log's per-SNR agreement.
//!
//! The logic lives in the composition (`hk_pipeline::ml::MlStage`, over `hk_ml`'s host and
//! [`hk_store::ml`]'s store); this module only parses, routes, audits and shapes errors, like
//! [`crate::datasets`].
//!
//! # Endpoints
//!
//! | Method | Path | Body / query | Answers |
//! |---|---|---|---|
//! | GET | `/api/ml/models` | – | `{registry, models: [...], hosts: [...], producer, restore_errors}` |
//! | PUT | `/api/ml/models/<id>/mode` | `{"mode": "off" \| "shadow" \| "active", "version"?, "consumer"?, "force"?}` | `{"mode": {model, key, consumer, mode, previous, forced, provider}}` (audited `ml_mode`) |
//! | GET | `/api/ml/shadow` | `?model&consumer&family&t0&t1&limit` | `{records: [...], aggregates: [...], store}` |
//!
//! **Shadow means shadow.** No route here can make a model's output a decision: a mode change
//! only chooses whether a model is *run and recorded* next to the classical cascade, and the
//! pipeline's only call into the host is its shadow path. `active` is refused (409
//! `needs_evidence` / `not_conformant`) without the ADR-0016 §4.6 enable evidence on the manifest
//! and a conformant provider, unless the request carries `"force": true` — which is recorded on
//! the mode and in the audit entry, never silent.

use hk_store::ml::{MAX_SHADOW_LIMIT, ShadowQuery};
use serde_json::{Map, Value, json};

use crate::control::{Applied, CtlRequest, CtlResponse, Fail, dispatch, refuse_route};
use crate::http::ApiState;

/// A refused or failed ML action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MlFailure {
    /// HTTP status.
    pub status: u16,
    /// Machine token (`invalid`, `not_found`, `needs_evidence`, `not_conformant`, `unavailable`,
    /// `failed`).
    pub code: String,
    /// Reason.
    pub message: String,
}

/// The mode a `PUT /api/ml/models/<id>/mode` asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MlModeWanted {
    /// Not run.
    Off,
    /// Run and recorded next to the classical decision; decides nothing.
    Shadow,
    /// Permitted to decide, on §4.6 evidence and a conformant provider (or an audited force).
    Active,
}

impl MlModeWanted {
    /// Wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shadow => "shadow",
            Self::Active => "active",
        }
    }
}

/// A parsed mode change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MlModeChange {
    /// Registry id (the path segment).
    pub id: String,
    /// Version; may be omitted when exactly one is installed.
    pub version: Option<String>,
    /// Consumer; defaults to the manifest's.
    pub consumer: Option<String>,
    /// The mode wanted.
    pub mode: MlModeWanted,
    /// Override §4.6's `active` requirements (audited).
    pub force: bool,
}

/// The ML stage of a running pipeline (implemented by the composition over
/// `hk_pipeline::ml::MlStage`).
pub trait MlControl: Send + Sync {
    /// `GET /api/ml/models`.
    fn models(&self) -> Result<Value, MlFailure>;
    /// `PUT /api/ml/models/<id>/mode`: the applied mode, or why it was refused.
    fn set_mode(&self, change: &MlModeChange) -> Result<Value, MlFailure>;
    /// `GET /api/ml/shadow`.
    fn shadow(&self, q: &ShadowQuery) -> Result<Value, MlFailure>;
}

#[derive(Clone, Debug, PartialEq)]
enum Action {
    Models,
    Mode(String),
    Shadow,
}

fn resolve(method: &str, path: &str) -> Option<Result<Action, Option<&'static str>>> {
    let rest = path.strip_prefix("/api/ml")?;
    if !rest.is_empty() && !rest.starts_with('/') {
        return None;
    }
    let get_only = |a: Action| match method {
        "GET" => Ok(a),
        _ => Err(Some("GET")),
    };
    match rest {
        "/models" => Some(get_only(Action::Models)),
        "/shadow" => Some(get_only(Action::Shadow)),
        _ => {
            let Some(id) = rest
                .strip_prefix("/models/")
                .and_then(|r| r.strip_suffix("/mode"))
            else {
                return Some(Err(None));
            };
            if id.is_empty() || id.contains('/') {
                return Some(Err(None));
            }
            Some(match method {
                "PUT" => Ok(Action::Mode(id.to_owned())),
                _ => Err(Some("PUT")),
            })
        }
    }
}

/// Routes an ML request; `None` when `path` is not one.
pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    let action = match resolve(req.method, req.path)? {
        Ok(a) => a,
        Err(allow) => return Some(refuse_route(state, req, allow)),
    };
    Some(match action {
        Action::Models => respond(control(state).and_then(|c| c.models().map_err(from_failure))),
        Action::Shadow => respond(
            shadow_query(req.query)
                .and_then(|q| control(state).and_then(|c| c.shadow(&q).map_err(from_failure))),
        ),
        Action::Mode(id) => dispatch(
            state,
            req,
            "ml_mode",
            true,
            |_| Err(Fail::new(500, "failed", "not a read")),
            |state, body| set_mode(state, &id, body),
        ),
    })
}

fn respond(r: Result<Value, Fail>) -> CtlResponse {
    match r {
        Ok(body) => CtlResponse {
            status: 200,
            body,
            allow: None,
        },
        Err(f) => f.response(),
    }
}

fn control(state: &ApiState) -> Result<&dyn MlControl, Fail> {
    state
        .ml
        .as_deref()
        .ok_or_else(|| Fail::new(503, "unavailable", "no ML stage on this server"))
}

fn from_failure(f: MlFailure) -> Fail {
    let code: &'static str = match f.code.as_str() {
        "invalid" => "invalid",
        "not_found" => "not_found",
        "needs_evidence" => "needs_evidence",
        "not_conformant" => "not_conformant",
        "unavailable" => "unavailable",
        _ => "failed",
    };
    Fail::new(f.status, code, f.message)
}

fn param<'a>(q: &'a [(String, String)], key: &str) -> Option<&'a str> {
    q.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn text(q: &[(String, String)], key: &str) -> Result<Option<String>, Fail> {
    match param(q, key) {
        None => Ok(None),
        Some(v) if v.trim().is_empty() => Err(Fail::invalid(format!("{key} is not empty"))),
        Some(v) => Ok(Some(v.to_owned())),
    }
}

fn seconds(q: &[(String, String)], key: &str) -> Result<Option<hk_model::Timestamp>, Fail> {
    param(q, key)
        .map(|v| {
            v.parse::<f64>()
                .ok()
                .filter(|s| s.is_finite() && (0.0..9.2e9).contains(s))
                .map(|s| hk_model::Timestamp::from_unix_nanos((s * 1e9).round() as i64))
                .ok_or_else(|| Fail::invalid(format!("{key} is Unix seconds")))
        })
        .transpose()
}

fn shadow_query(q: &[(String, String)]) -> Result<ShadowQuery, Fail> {
    let t0 = seconds(q, "t0")?;
    let t1 = seconds(q, "t1")?;
    if let (Some(a), Some(b)) = (t0, t1)
        && a >= b
    {
        return Err(Fail::invalid("t0 must be before t1"));
    }
    let limit = param(q, "limit")
        .map(|v| {
            v.parse::<usize>()
                .ok()
                .filter(|n| (1..=MAX_SHADOW_LIMIT).contains(n))
                .ok_or_else(|| {
                    Fail::invalid(format!("limit is an integer from 1 to {MAX_SHADOW_LIMIT}"))
                })
        })
        .transpose()?;
    Ok(ShadowQuery {
        model: text(q, "model")?,
        consumer: text(q, "consumer")?,
        family: text(q, "family")?,
        t0,
        t1,
        limit,
    })
}

fn body_text(body: &Map<String, Value>, key: &str) -> Result<Option<String>, Fail> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.clone())),
        Some(_) => Err(Fail::invalid(format!("{key} is a non-empty string"))),
    }
}

fn parse_change(id: &str, body: &Map<String, Value>) -> Result<MlModeChange, Fail> {
    crate::control::only(body, &["mode", "version", "consumer", "force"])?;
    let mode = match body.get("mode") {
        Some(Value::String(s)) if s == "off" => MlModeWanted::Off,
        Some(Value::String(s)) if s == "shadow" => MlModeWanted::Shadow,
        Some(Value::String(s)) if s == "active" => MlModeWanted::Active,
        _ => {
            return Err(Fail::invalid(
                "mode (\"off\", \"shadow\" or \"active\") is required",
            ));
        }
    };
    let force = match body.get("force") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(Fail::invalid("force is a boolean")),
    };
    Ok(MlModeChange {
        id: id.to_owned(),
        version: body_text(body, "version")?,
        consumer: body_text(body, "consumer")?,
        mode,
        force,
    })
}

fn set_mode(state: &ApiState, id: &str, body: &Map<String, Value>) -> Result<Applied, Fail> {
    let change = parse_change(id, body)?;
    let applied = control(state)?.set_mode(&change).map_err(from_failure)?;
    let old = json!({ "mode": applied.get("previous").cloned().unwrap_or(Value::Null) });
    let new = json!({
        "model": applied.get("model").cloned().unwrap_or(Value::Null),
        "consumer": applied.get("consumer").cloned().unwrap_or(Value::Null),
        "mode": change.mode.as_str(),
        "forced": applied.get("forced").cloned().unwrap_or(Value::Bool(false)),
    });
    Ok(Applied {
        status: 200,
        body: json!({ "mode": applied }),
        old,
        new,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn routes_resolve_with_methods() {
        assert_eq!(resolve("GET", "/api/ml/models"), Some(Ok(Action::Models)));
        assert_eq!(resolve("GET", "/api/ml/shadow"), Some(Ok(Action::Shadow)));
        assert_eq!(
            resolve("PUT", "/api/ml/models/amc-fsk/mode"),
            Some(Ok(Action::Mode("amc-fsk".into())))
        );
        assert_eq!(resolve("POST", "/api/ml/models"), Some(Err(Some("GET"))));
        assert_eq!(resolve("PUT", "/api/ml/shadow"), Some(Err(Some("GET"))));
        assert_eq!(
            resolve("GET", "/api/ml/models/amc-fsk/mode"),
            Some(Err(Some("PUT")))
        );
        assert_eq!(resolve("PUT", "/api/ml/models//mode"), Some(Err(None)));
        assert_eq!(resolve("PUT", "/api/ml/models/a/b/mode"), Some(Err(None)));
        assert_eq!(resolve("GET", "/api/ml/other"), Some(Err(None)));
        assert_eq!(resolve("GET", "/api/mlx"), None);
    }

    #[test]
    fn a_mode_change_needs_a_known_mode_and_refuses_unknown_fields() {
        let body = |v: Value| v.as_object().unwrap().clone();
        let c = parse_change("amc-fsk", &body(json!({"mode": "shadow"})))
            .ok()
            .unwrap();
        assert_eq!(c.mode, MlModeWanted::Shadow);
        assert!(!c.force);
        let c = parse_change(
            "amc-fsk",
            &body(json!({"mode": "active", "force": true, "version": "0.1.0"})),
        )
        .ok()
        .unwrap();
        assert_eq!(
            (c.mode, c.force, c.version.as_deref()),
            (MlModeWanted::Active, true, Some("0.1.0"))
        );
        assert!(parse_change("x", &body(json!({}))).is_err());
        assert!(parse_change("x", &body(json!({"mode": "on"}))).is_err());
        assert!(parse_change("x", &body(json!({"mode": "off", "force": "yes"}))).is_err());
        assert!(parse_change("x", &body(json!({"mode": "off", "evidence": "x"}))).is_err());
    }

    #[test]
    fn shadow_queries_parse_filters_times_and_a_bounded_limit() {
        let s = shadow_query(&q(&[
            ("model", "amc-fsk"),
            ("family", "fsk"),
            ("t0", "10"),
            ("t1", "20.5"),
            ("limit", "5"),
        ]))
        .ok()
        .unwrap();
        assert_eq!(s.model.as_deref(), Some("amc-fsk"));
        assert_eq!(s.family.as_deref(), Some("fsk"));
        assert_eq!(s.t0.unwrap().as_unix_nanos(), 10_000_000_000);
        assert_eq!(s.t1.unwrap().as_unix_nanos(), 20_500_000_000);
        assert_eq!(s.limit, Some(5));
        assert_eq!(shadow_query(&[]).ok().unwrap(), ShadowQuery::default());
        assert!(shadow_query(&q(&[("t0", "20"), ("t1", "10")])).is_err());
        assert!(shadow_query(&q(&[("limit", "0")])).is_err());
        assert!(shadow_query(&q(&[("limit", "100000")])).is_err());
        assert!(shadow_query(&q(&[("t0", "nan")])).is_err());
        assert!(shadow_query(&q(&[("model", " ")])).is_err());
    }
}
