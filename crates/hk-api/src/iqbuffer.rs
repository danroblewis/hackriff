//! Rolling IQ capture buffer over HTTP (T-157, ADR-0013 §4 API gap 1): the always-on raw-IQ
//! history behind the Capture timeline, and clip export from it into the recordings model. The
//! buffer itself (storage, segments, eviction) is `hk_store::iqbuffer`, fed by
//! `hk_pipeline::iqbuffer`; this module only validates, routes, audits and shapes errors.
//!
//! | Method | Path | Body / query | Answers |
//! |---|---|---|---|
//! | GET | `/api/iqbuffer` | `?[t0=<unix s>][&t1=<unix s>][&limit=1..10000, default 1000]` | the buffer status (span, bytes and quota, segments with tuning and gain, gaps, eviction counts, drops) |
//! | POST | `/api/iqbuffer/clip` | `{"t0", "t1", "band"?: {"f_lo", "f_hi"}, "label"?}` | `{"recording": clip}`; 404 `not_found` (nothing buffered there), 409 `conflict` (spans a sample-rate change), 503 `unavailable` (no buffer) |
//!
//! The clip export writes a file and a `Recording` row, so it needs `Authorization: Bearer` and is
//! audited as `iqbuffer_clip`.

use serde_json::{Map, Value, json};

use crate::control::{
    Applied, CtlRequest, CtlResponse, Fail, dispatch, number, ok, only, refuse_route,
};
use crate::http::ApiState;

/// Segments a status lists by default.
pub const IQBUFFER_SEGMENTS_DEFAULT: usize = 1000;
/// Largest `limit`.
pub const IQBUFFER_SEGMENTS_MAX: usize = 10_000;

/// A validated status query (Unix s).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IqBufferQuery {
    /// Segments ending after this.
    pub t0: Option<f64>,
    /// Segments starting before this.
    pub t1: Option<f64>,
    /// The newest this many matching segments are listed.
    pub limit: usize,
}

/// A validated clip request.
#[derive(Clone, Debug, PartialEq)]
pub struct ClipStart {
    /// Start, Unix s (inclusive).
    pub t0: f64,
    /// End, Unix s (exclusive).
    pub t1: f64,
    /// Only segments whose tuned window overlaps `(f_lo, f_hi)`, Hz.
    pub band: Option<(f64, f64)>,
    /// User label.
    pub label: Option<String>,
}

/// A refused or failed clip export.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IqBufferFailure {
    /// HTTP status.
    pub status: u16,
    /// Machine token (`invalid`, `not_found`, `conflict`, `unavailable`, `failed`).
    pub code: String,
    /// Reason.
    pub message: String,
}

/// The rolling IQ buffer of a running pipeline (implemented by the composition over
/// `hk_pipeline::iqbuffer::IqBufferService`).
pub trait IqBufferControl: Send + Sync {
    /// The status JSON.
    fn status(&self, query: &IqBufferQuery) -> Value;
    /// Exports a clip; returns the stored recording JSON.
    fn clip(&self, request: &ClipStart) -> Result<Value, IqBufferFailure>;
}

enum Action {
    Status,
    Clip,
}

fn resolve(method: &str, path: &str) -> Option<Result<Action, Option<&'static str>>> {
    match path {
        "/api/iqbuffer" => Some(if method == "GET" {
            Ok(Action::Status)
        } else {
            Err(Some("GET"))
        }),
        "/api/iqbuffer/clip" => Some(if method == "POST" {
            Ok(Action::Clip)
        } else {
            Err(Some("POST"))
        }),
        _ => None,
    }
}

/// This module's routes; `None` = not mine.
pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    let action = match resolve(req.method, req.path)? {
        Ok(a) => a,
        Err(allow) => return Some(refuse_route(state, req, allow)),
    };
    Some(match action {
        Action::Status => match status(state, req.query) {
            Ok(body) => CtlResponse {
                status: 200,
                body,
                allow: None,
            },
            Err(f) => f.response(),
        },
        Action::Clip => dispatch(
            state,
            req,
            "iqbuffer_clip",
            true,
            |_| Err(Fail::new(500, "failed", "not a read")),
            clip,
        ),
    })
}

fn control(state: &ApiState) -> Result<&dyn IqBufferControl, Fail> {
    state
        .iq_buffer
        .as_deref()
        .ok_or_else(|| Fail::new(503, "unavailable", "no IQ capture buffer on this server"))
}

fn param<'a>(q: &'a [(String, String)], key: &str) -> Option<&'a str> {
    q.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn time_s(q: &[(String, String)], key: &str) -> Result<Option<f64>, Fail> {
    param(q, key)
        .map(|v| {
            v.parse::<f64>()
                .ok()
                .filter(|s| s.is_finite() && s.abs() < 9.0e9)
                .ok_or_else(|| Fail::invalid(format!("{key} is a time in Unix seconds")))
        })
        .transpose()
}

fn status(state: &ApiState, q: &[(String, String)]) -> Result<Value, Fail> {
    const ALLOWED: [&str; 4] = ["t0", "t1", "limit", "token"];
    if let Some((k, _)) = q.iter().find(|(k, _)| !ALLOWED.contains(&k.as_str())) {
        return Err(Fail::invalid(format!(
            "unknown parameter {k:?} (allowed: t0, t1, limit)"
        )));
    }
    let (t0, t1) = (time_s(q, "t0")?, time_s(q, "t1")?);
    if let (Some(a), Some(b)) = (t0, t1) {
        if a >= b {
            return Err(Fail::invalid("t0 must be before t1"));
        }
    }
    let limit = match param(q, "limit") {
        None => IQBUFFER_SEGMENTS_DEFAULT,
        Some(v) => v
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=IQBUFFER_SEGMENTS_MAX).contains(n))
            .ok_or_else(|| {
                Fail::invalid(format!(
                    "limit is an integer in 1..={IQBUFFER_SEGMENTS_MAX}"
                ))
            })?,
    };
    Ok(control(state)?.status(&IqBufferQuery { t0, t1, limit }))
}

fn clip(state: &ApiState, body: &Map<String, Value>) -> Result<Applied, Fail> {
    only(body, &["t0", "t1", "band", "label"])?;
    let required = |k: &str| {
        number(body, k)?.ok_or_else(|| Fail::invalid(format!("{k} (Unix seconds) is required")))
    };
    let (t0, t1) = (required("t0")?, required("t1")?);
    if !(t0 < t1 && t0.abs() < 9.0e9 && t1.abs() < 9.0e9) {
        return Err(Fail::invalid("t0 and t1 are Unix seconds with t0 < t1"));
    }
    let band = match body.get("band") {
        None | Some(Value::Null) => None,
        Some(Value::Object(b)) => {
            only(b, &["f_lo", "f_hi"])?;
            let edge = |k: &str| {
                number(b, k)?.ok_or_else(|| Fail::invalid(format!("band.{k} (Hz) is required")))
            };
            let (lo, hi) = (edge("f_lo")?, edge("f_hi")?);
            if lo >= hi {
                return Err(Fail::invalid("band needs f_lo < f_hi"));
            }
            Some((lo, hi))
        }
        Some(_) => return Err(Fail::invalid("band is an object {f_lo, f_hi}")),
    };
    let label = match body.get("label") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(Fail::invalid("label is a string")),
    };
    let request = ClipStart {
        t0,
        t1,
        band,
        label,
    };
    let recording = control(state)?.clip(&request).map_err(|f| {
        let code = match f.code.as_str() {
            "invalid" => "invalid",
            "not_found" => "not_found",
            "conflict" => "conflict",
            "unavailable" => "unavailable",
            _ => "failed",
        };
        Fail::new(f.status, code, f.message)
    })?;
    let new = json!({ "id": recording["id"], "samples": recording["samples"] });
    Ok(ok(json!({ "recording": recording }), Value::Null, new))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_buffer_routes_resolve() {
        assert!(matches!(
            resolve("GET", "/api/iqbuffer"),
            Some(Ok(Action::Status))
        ));
        assert!(matches!(
            resolve("POST", "/api/iqbuffer/clip"),
            Some(Ok(Action::Clip))
        ));
        assert!(matches!(
            resolve("POST", "/api/iqbuffer"),
            Some(Err(Some("GET")))
        ));
        assert!(matches!(
            resolve("GET", "/api/iqbuffer/clip"),
            Some(Err(Some("POST")))
        ));
        assert!(resolve("GET", "/api/iqbuffer/x").is_none());
    }
}
