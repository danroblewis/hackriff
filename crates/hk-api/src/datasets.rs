//! Labelled-capture dataset export over HTTP (T-205, ADR-0016 §7/§9): CRC-valid decoder evidence
//! and user classifications become exportable, labelled IQ snippets for later model fine-tuning
//! (C38) and blind evaluation (T-213). The label logic and manifest shape are
//! [`hk_store::dataset`]; this module only parses, routes, audits and shapes errors, like
//! [`crate::iqbuffer`].
//!
//! # Endpoints
//! Bodies are JSON objects; unknown fields are refused (400). Frequencies are Hz, times Unix
//! seconds (floats), like every other route.
//!
//! | Method | Path | Body / query | Answers |
//! |---|---|---|---|
//! | POST | `/api/datasets` | `{"filter": {"emitter"?, "time"? {"t0","t1"}, "family"?}, "split": "dev" \| "acceptance", "pad_pre_s"?, "pad_post_s"?, "max_samples"?}` | `{"dataset": DatasetManifest}` (201; audited `dataset_export`) |
//! | GET | `/api/datasets` | – | `{"datasets": [...]}`, newest first |
//! | GET | `/api/datasets/<id>` | – | `{"dataset": DatasetManifest}`; 404 `not_found` |
//!
//! A `DatasetManifest` is `{id, filter, split, created_at, samples: [DatasetSample], skipped}`;
//! `DatasetSample` is `{emitter_id, session, recording_id, annotation_id, label: {taxonomy,
//! label, source, provenance, confidence}, snr_db, sample_rate_hz, center_offset_hz, t, split}`.
//! Every sample's IQ is an ordinary SigMF `Recording` (see "IQ capture buffer" above); the manifest
//! only indexes them and stamps the split, so no database migration was needed for it.
//!
//! The export writes files and `Recording`/`Annotation` rows, so it needs
//! `Authorization: Bearer`.

use hk_store::dataset::{DatasetFilter, DatasetRequest, Split};
use serde_json::{Map, Value, json};

use crate::control::{
    Applied, CtlRequest, CtlResponse, Fail, dispatch, number, only, refuse_route,
};
use crate::http::ApiState;

/// A refused or failed dataset action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatasetFailure {
    /// HTTP status.
    pub status: u16,
    /// Machine token (`invalid`, `not_found`, `unavailable`, `failed`).
    pub code: String,
    /// Reason.
    pub message: String,
}

/// Labelled-capture dataset export of a running pipeline (implemented by the composition over
/// [`hk_store::dataset::export_dataset`] and the repository).
pub trait DatasetControl: Send + Sync {
    /// Runs an export now and returns its manifest JSON.
    fn export(&self, req: &DatasetRequest) -> Result<Value, DatasetFailure>;
    /// Stored manifests (id, split, created_at, sample and skipped counts), newest first.
    fn list(&self) -> Result<Value, DatasetFailure>;
    /// One stored manifest by id.
    fn get(&self, id: &str) -> Result<Value, DatasetFailure>;
}

#[derive(Clone, Debug, PartialEq)]
enum Action {
    Export,
    List,
    Get(String),
}

fn resolve(method: &str, path: &str) -> Option<Result<Action, Option<&'static str>>> {
    if path == "/api/datasets" {
        return Some(match method {
            "GET" => Ok(Action::List),
            "POST" => Ok(Action::Export),
            _ => Err(Some("GET, POST")),
        });
    }
    let id = path.strip_prefix("/api/datasets/")?;
    if id.is_empty() || id.contains('/') {
        return Some(Err(None));
    }
    Some(match method {
        "GET" => Ok(Action::Get(id.to_owned())),
        _ => Err(Some("GET")),
    })
}

/// Routes a dataset request; `None` when `path` is not one.
pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    let action = match resolve(req.method, req.path)? {
        Ok(a) => a,
        Err(allow) => return Some(refuse_route(state, req, allow)),
    };
    Some(match action {
        Action::Export => dispatch(
            state,
            req,
            "dataset_export",
            true,
            |_| Err(Fail::new(500, "failed", "not a read")),
            export,
        ),
        Action::List => match control(state).and_then(|c| c.list().map_err(from_failure)) {
            Ok(body) => CtlResponse {
                status: 200,
                body,
                allow: None,
            },
            Err(f) => f.response(),
        },
        Action::Get(id) => match control(state).and_then(|c| c.get(&id).map_err(from_failure)) {
            Ok(body) => CtlResponse {
                status: 200,
                body: json!({ "dataset": body }),
                allow: None,
            },
            Err(f) => f.response(),
        },
    })
}

fn control(state: &ApiState) -> Result<&dyn DatasetControl, Fail> {
    state
        .datasets
        .as_deref()
        .ok_or_else(|| Fail::new(503, "unavailable", "no dataset export on this server"))
}

fn from_failure(f: DatasetFailure) -> Fail {
    let code: &'static str = match f.code.as_str() {
        "invalid" => "invalid",
        "not_found" => "not_found",
        "unavailable" => "unavailable",
        _ => "failed",
    };
    Fail::new(f.status, code, f.message)
}

fn time_range(body: &Map<String, Value>) -> Result<Option<hk_model::TimeRange>, Fail> {
    match body.get("time") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(t)) => {
            only(t, &["t0", "t1"])?;
            let edge = |k: &str| {
                number(t, k)?.ok_or_else(|| Fail::invalid(format!("time.{k} (Unix s) is required")))
            };
            let (t0, t1) = (edge("t0")?, edge("t1")?);
            if !(t0.is_finite() && t1.is_finite() && (0.0..9.2e9).contains(&t0) && t0 < t1) {
                return Err(Fail::invalid(
                    "time.t0 and time.t1 are Unix seconds with 0 ≤ t0 < t1",
                ));
            }
            let ns = |s: f64| (s * 1e9).round() as i64;
            Ok(Some(hk_model::TimeRange::new(
                hk_model::Timestamp::from_unix_nanos(ns(t0)),
                hk_model::Timestamp::from_unix_nanos(ns(t1)),
            )))
        }
        Some(_) => Err(Fail::invalid("time is an object {t0, t1}")),
    }
}

fn filter(body: &Map<String, Value>) -> Result<DatasetFilter, Fail> {
    match body.get("filter") {
        None | Some(Value::Null) => Ok(DatasetFilter::default()),
        Some(Value::Object(f)) => {
            only(f, &["emitter", "time", "family"])?;
            let emitter = match f.get("emitter") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => Some(
                    s.parse::<hk_model::EmitterId>()
                        .map_err(|_| Fail::invalid("filter.emitter is not an id"))?,
                ),
                Some(_) => return Err(Fail::invalid("filter.emitter is a string id")),
            };
            let family = match f.get("family") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
                Some(Value::String(_)) => return Err(Fail::invalid("filter.family is not empty")),
                Some(_) => return Err(Fail::invalid("filter.family is a string")),
            };
            Ok(DatasetFilter {
                emitter,
                time: time_range(f)?,
                family,
            })
        }
        Some(_) => Err(Fail::invalid("filter is an object")),
    }
}

fn split(body: &Map<String, Value>) -> Result<Split, Fail> {
    match body.get("split") {
        Some(Value::String(s)) if s == "dev" => Ok(Split::Dev),
        Some(Value::String(s)) if s == "acceptance" => Ok(Split::Acceptance),
        Some(Value::String(_)) => Err(Fail::invalid("split must be \"dev\" or \"acceptance\"")),
        _ => Err(Fail::invalid(
            "split (\"dev\" or \"acceptance\") is required",
        )),
    }
}

fn positive(body: &Map<String, Value>, key: &str) -> Result<Option<f64>, Fail> {
    match number(body, key)? {
        Some(v) if v >= 0.0 => Ok(Some(v)),
        Some(_) => Err(Fail::invalid(format!("{key} must be non-negative"))),
        None => Ok(None),
    }
}

fn export(state: &ApiState, body: &Map<String, Value>) -> Result<Applied, Fail> {
    only(
        body,
        &["filter", "split", "pad_pre_s", "pad_post_s", "max_samples"],
    )?;
    let mut req = DatasetRequest {
        filter: filter(body)?,
        split: split(body)?,
        ..DatasetRequest::default()
    };
    if let Some(v) = positive(body, "pad_pre_s")? {
        req.pad_pre_s = v;
    }
    if let Some(v) = positive(body, "pad_post_s")? {
        req.pad_post_s = v;
    }
    if let Some(v) = body.get("max_samples") {
        req.max_samples = v
            .as_u64()
            .filter(|n| *n >= 1)
            .ok_or_else(|| Fail::invalid("max_samples must be an integer of at least 1"))?
            as usize;
    }
    let manifest = control(state)?.export(&req).map_err(from_failure)?;
    let new = json!({ "id": manifest["id"], "samples": manifest["samples"] });
    Ok(Applied {
        status: 201,
        body: json!({ "dataset": manifest }),
        old: Value::Null,
        new,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_resolve_with_methods() {
        assert_eq!(resolve("GET", "/api/datasets"), Some(Ok(Action::List)));
        assert_eq!(resolve("POST", "/api/datasets"), Some(Ok(Action::Export)));
        assert_eq!(
            resolve("DELETE", "/api/datasets"),
            Some(Err(Some("GET, POST")))
        );
        assert_eq!(
            resolve("GET", "/api/datasets/abc"),
            Some(Ok(Action::Get("abc".into())))
        );
        assert_eq!(resolve("POST", "/api/datasets/abc"), Some(Err(Some("GET"))));
        assert_eq!(resolve("GET", "/api/datasets/"), Some(Err(None)));
        assert_eq!(resolve("GET", "/api/other"), None);
    }
}
