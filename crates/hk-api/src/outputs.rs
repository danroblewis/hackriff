//! Output recordings (T-061) over HTTP: record a selection's, emitter's or band's bits, symbols,
//! WAV audio and IQ to files on the device, stop, list, and download. The same token, audit and
//! validation rules as the control API ([`crate::control`]).
//!
//! | Method | Path | Body | Answers |
//! |---|---|---|---|
//! | POST | `/api/outputs/record/start` | `{"selection_id" \| "emitter_id" \| "band": {"f_lo", "f_hi"}, "kinds": ["bits", "symbols", "audio", "iq"], "max_s"?, "max_bytes"?}` | `{"recording": session}`; 507 `quota` when the global output quota is full, 503 `busy`, 404 `not_found`, opener refusals (e.g. 403) when every output is refused |
//! | POST | `/api/outputs/record/stop` | `{"id"}` | `{"recording": session}` once every file is finalised |
//! | GET | `/api/outputs` | – | `{"recordings": [session, ...]}` newest first |
//! | GET | `/api/outputs/<id>/files/<name>` | – | the file's bytes (`?token=` accepted: download links) |
//!
//! A session is JSON: `id`, `active`, `selection_id`, `emitter_id`, `f_lo_hz`, `f_hi_hz`, `kinds`,
//! `max_s`, `max_bytes`, `bytes`, `started_at`, `elapsed_s`, `ended`, `links_saved`, and `files`
//! (`kind`, `file`, `sidecar`, `extra_files`, `state`, `bytes`, `records`, `dropped_records`,
//! `message`, `recording_id`, `bitstream_id`, plus `url`, `sidecar_url` and `extra_urls`: download
//! paths to which a browser appends `?token=`).
//!
//! Mutating calls need `Authorization: Bearer` and are audited as `outputs_record_start` and
//! `outputs_record_stop`.

use std::path::PathBuf;

use serde_json::{Map, Value, json};

use crate::control::{
    Applied, CtlRequest, CtlResponse, Fail, dispatch, no_fields, number, ok, only, refuse_route,
};
use crate::http::ApiState;

/// The target of a start request.
#[derive(Clone, Debug, PartialEq)]
pub enum OutputTarget {
    /// A persisted selection id.
    Selection(String),
    /// An inventory emitter id.
    Emitter(String),
    /// A band, Hz.
    Band {
        /// Lower edge.
        f_lo_hz: f64,
        /// Upper edge.
        f_hi_hz: f64,
    },
}

/// A validated start request.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputStart {
    /// Target.
    pub target: OutputTarget,
    /// Kinds as named (`bits`, `symbols`, `audio`, `iq`).
    pub kinds: Vec<String>,
    /// Longest recording, s.
    pub max_s: Option<f64>,
    /// Largest recording, bytes.
    pub max_bytes: Option<u64>,
}

/// A refused or failed output request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputFailure {
    /// HTTP status.
    pub status: u16,
    /// Machine token.
    pub code: String,
    /// Reason.
    pub message: String,
}

/// Output recording over a running pipeline (implemented by the composition over
/// `hk_pipeline::OutputRecorders`).
pub trait OutputControl: Send + Sync {
    /// Starts a recording; returns the session JSON.
    fn start(&self, request: &OutputStart) -> Result<Value, OutputFailure>;
    /// Stops a recording and waits for its files; returns the session JSON.
    fn stop(&self, id: &str) -> Result<Value, OutputFailure>;
    /// Sessions, newest first.
    fn list(&self) -> Vec<Value>;
    /// A session's file for download.
    fn file(&self, id: &str, name: &str) -> Result<PathBuf, OutputFailure>;
}

fn code(c: &str) -> &'static str {
    match c {
        "invalid" => "invalid",
        "not_found" => "not_found",
        "quota" => "quota",
        "busy" => "busy",
        "unavailable" => "unavailable",
        "failed" => "failed",
        _ => "refused",
    }
}

impl From<OutputFailure> for Fail {
    fn from(f: OutputFailure) -> Self {
        let message = if code(&f.code) == "refused" && f.code != "refused" {
            format!("{} ({})", f.message, f.code)
        } else {
            f.message
        };
        Fail::new(f.status, code(&f.code), message)
    }
}

/// Adds download paths to a session's files.
pub fn with_links(mut session: Value) -> Value {
    let Some(id) = session["id"].as_str().map(str::to_owned) else {
        return session;
    };
    if let Some(files) = session["files"].as_array_mut() {
        for f in files {
            let url = |name: &Value| {
                name.as_str()
                    .map(|n| Value::String(format!("/api/outputs/{id}/files/{n}")))
                    .unwrap_or(Value::Null)
            };
            let (u, s) = (url(&f["file"]), url(&f["sidecar"]));
            let extra: Vec<Value> = f["extra_files"]
                .as_array()
                .map(|a| a.iter().map(url).collect())
                .unwrap_or_default();
            if let Some(o) = f.as_object_mut() {
                o.insert("url".into(), u);
                o.insert("sidecar_url".into(), s);
                o.insert("extra_urls".into(), Value::Array(extra));
            }
        }
    }
    session
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    List,
    Start,
    Stop,
}

fn resolve(method: &str, path: &str) -> Option<Result<Action, Option<&'static str>>> {
    let (action, allow) = match path {
        "/api/outputs" => (Action::List, "GET"),
        "/api/outputs/record/start" => (Action::Start, "POST"),
        "/api/outputs/record/stop" => (Action::Stop, "POST"),
        _ => return None,
    };
    Some(if method == allow {
        Ok(action)
    } else {
        Err(Some(allow))
    })
}

pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    let action = match resolve(req.method, req.path)? {
        Ok(a) => a,
        Err(allow) => return Some(refuse_route(state, req, allow)),
    };
    let name = match action {
        Action::List => "outputs_list",
        Action::Start => "outputs_record_start",
        Action::Stop => "outputs_record_stop",
    };
    Some(dispatch(
        state,
        req,
        name,
        action != Action::List,
        |s| {
            let oc = service(s)?;
            Ok(json!({ "recordings": oc.list().into_iter().map(with_links).collect::<Vec<_>>() }))
        },
        |s, body| apply(s, action, body),
    ))
}

fn service(state: &ApiState) -> Result<&dyn OutputControl, Fail> {
    state
        .outputs
        .as_deref()
        .ok_or_else(|| Fail::new(503, "unavailable", "no output recorder on this server"))
}

fn id_text<'a>(body: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, Fail> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.trim().is_empty() && s.len() <= 64 => Ok(Some(s.trim())),
        Some(_) => Err(Fail::invalid(format!("{key} must be an id string"))),
    }
}

/// Parses a start body.
pub(crate) fn parse_start(body: &Map<String, Value>) -> Result<OutputStart, Fail> {
    only(
        body,
        &[
            "selection_id",
            "emitter_id",
            "band",
            "kinds",
            "max_s",
            "max_bytes",
        ],
    )?;
    let selection = id_text(body, "selection_id")?;
    let emitter = id_text(body, "emitter_id")?;
    let band = match body.get("band") {
        None | Some(Value::Null) => None,
        Some(Value::Object(b)) => {
            only(b, &["f_lo", "f_hi"])?;
            let lo = number(b, "f_lo")?.ok_or_else(|| Fail::invalid("band.f_lo is required"))?;
            let hi = number(b, "f_hi")?.ok_or_else(|| Fail::invalid("band.f_hi is required"))?;
            if !(lo > 0.0 && lo < hi) {
                return Err(Fail::invalid("band needs 0 < f_lo < f_hi (Hz)"));
            }
            Some((lo, hi))
        }
        Some(_) => return Err(Fail::invalid("band must be {\"f_lo\", \"f_hi\"}")),
    };
    let target = match (selection, emitter, band) {
        (Some(s), None, None) => OutputTarget::Selection(s.to_owned()),
        (None, Some(e), None) => OutputTarget::Emitter(e.to_owned()),
        (None, None, Some((f_lo_hz, f_hi_hz))) => OutputTarget::Band { f_lo_hz, f_hi_hz },
        _ => {
            return Err(Fail::invalid(
                "give exactly one of selection_id, emitter_id, band",
            ));
        }
    };
    let kinds = match body.get("kinds") {
        Some(Value::Array(a)) if !a.is_empty() => a
            .iter()
            .map(|k| match k.as_str() {
                Some(s @ ("bits" | "symbols" | "audio" | "iq")) => Ok(s.to_owned()),
                _ => Err(Fail::invalid(
                    "kinds entries must be \"bits\", \"symbols\", \"audio\" or \"iq\"",
                )),
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err(Fail::invalid("kinds must be a non-empty array")),
    };
    let max_s = number(body, "max_s")?;
    let max_bytes = match number(body, "max_bytes")? {
        None => None,
        Some(b) if b >= 1.0 && b.fract() == 0.0 && b <= 9.0e15 => Some(b as u64),
        Some(_) => return Err(Fail::invalid("max_bytes must be a positive integer")),
    };
    Ok(OutputStart {
        target,
        kinds,
        max_s,
        max_bytes,
    })
}

fn apply(state: &ApiState, action: Action, body: &Map<String, Value>) -> Result<Applied, Fail> {
    let oc = service(state)?;
    match action {
        Action::Start => {
            let start = parse_start(body)?;
            let new = with_links(oc.start(&start)?);
            Ok(ok(json!({ "recording": new }), Value::Null, new))
        }
        Action::Stop => {
            let id = id_text(body, "id")?.ok_or_else(|| Fail::invalid("id is required"))?;
            let mut rest = body.clone();
            rest.remove("id");
            no_fields(&rest)?;
            let new = with_links(oc.stop(id)?);
            Ok(ok(json!({ "recording": new }), Value::Null, new))
        }
        Action::List => unreachable!("reads do not apply"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn start_bodies_parse_and_validate() {
        let Ok(s) = parse_start(&body(json!({
            "band": { "f_lo": 433.8e6, "f_hi": 434.1e6 }, "kinds": ["bits", "iq"], "max_bytes": 1000
        }))) else {
            panic!("a valid band start body");
        };
        assert_eq!(
            s.target,
            OutputTarget::Band {
                f_lo_hz: 433.8e6,
                f_hi_hz: 434.1e6
            }
        );
        assert_eq!(s.max_bytes, Some(1000));
        for bad in [
            json!({ "kinds": ["bits"] }),
            json!({ "selection_id": "a", "emitter_id": "b", "kinds": ["bits"] }),
            json!({ "selection_id": "a", "kinds": [] }),
            json!({ "selection_id": "a", "kinds": ["wav"] }),
            json!({ "selection_id": "a", "kinds": ["bits"], "extra": 1 }),
            json!({ "band": { "f_lo": 5.0, "f_hi": 1.0 }, "kinds": ["bits"] }),
        ] {
            assert!(parse_start(&body(bad.clone())).is_err(), "{bad}");
        }
        assert_eq!(
            resolve("GET", "/api/outputs/record/start"),
            Some(Err(Some("POST")))
        );
        assert_eq!(resolve("GET", "/api/outputs"), Some(Ok(Action::List)));
    }

    #[test]
    fn links_are_added_per_file() {
        let v = with_links(json!({ "id": "s1", "files": [
            { "file": "bits.ru8", "sidecar": "bits.json", "extra_files": ["bits.bursts.jsonl"] }
        ] }));
        assert_eq!(v["files"][0]["url"], "/api/outputs/s1/files/bits.ru8");
        assert_eq!(
            v["files"][0]["sidecar_url"],
            "/api/outputs/s1/files/bits.json"
        );
        assert_eq!(
            v["files"][0]["extra_urls"][0],
            "/api/outputs/s1/files/bits.bursts.jsonl"
        );
    }
}
