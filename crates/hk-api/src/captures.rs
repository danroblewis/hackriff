//! Decoded-capture routes (T-092, ADR-0011 §7, stream contract §14.7, `docs/api.md` "Decoded
//! captures"): list, inspect, delete and scrub the always-on recordings of pipeline inspector
//! streams. The store (`hk_store::decoded`, behind [`ApiState::captures`]) keeps a frame index,
//! so a frame or time scrub seeks instead of scanning. Re-parsing with a field map is
//! `POST /api/captures/{id}/parse` ([`crate::inspector`]).
//!
//! | Method | Path | Answers |
//! |---|---|---|
//! | GET | `/api/captures` | `{captures: [Capture]}`, newest first |
//! | GET | `/api/captures/{id}` | `Capture` |
//! | DELETE | `/api/captures/{id}` | `{deleted: Capture}`; 409 while recording (audited) |
//! | GET | `/api/captures/{id}/frames?[from_frame|from_t][&to_t][&limit]` | a page of stored frame records |
//!
//! Frames are served fail closed like the parse route: a record whose class (or stream's class)
//! forbids content, or that is local-only, comes back `gated: true` without `content`.

use std::io;

use hk_stream::inspector::{CaptureDelete, CaptureSource, RecordedFrames};
use serde_json::{Map, Value, json};

use crate::control::{Applied, CtlRequest, CtlResponse, Fail, dispatch, no_fields, refuse_route};
use crate::http::ApiState;
use crate::inspector::{is_capture_id, servable, serve_record, stream_json};

/// Most frames per scrub page.
pub const MAX_FRAMES_LIMIT: u64 = 500;
/// Default scrub page size.
pub const DEFAULT_FRAMES_LIMIT: u64 = 100;

enum Action<'a> {
    List,
    Get(&'a str),
    Delete(&'a str),
    Frames(&'a str),
}

/// `None` when `path` is not a capture path of this module (`/parse` is the inspector's).
fn resolve<'a>(method: &str, path: &'a str) -> Option<Result<Action<'a>, Option<&'static str>>> {
    if path == "/api/captures" {
        return Some(if method == "GET" {
            Ok(Action::List)
        } else {
            Err(Some("GET"))
        });
    }
    let rest = path.strip_prefix("/api/captures/")?;
    if let Some(id) = rest.strip_suffix("/frames") {
        if id.is_empty() || id.contains('/') {
            return None;
        }
        return Some(if method == "GET" {
            Ok(Action::Frames(id))
        } else {
            Err(Some("GET"))
        });
    }
    if rest.is_empty() || rest.contains('/') {
        return None;
    }
    Some(match method {
        "GET" => Ok(Action::Get(rest)),
        "DELETE" => Ok(Action::Delete(rest)),
        _ => Err(Some("GET, DELETE")),
    })
}

/// This module's routes; `None` = not mine.
pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    let action = match resolve(req.method, req.path)? {
        Ok(a) => a,
        Err(allow) => return Some(refuse_route(state, req, allow)),
    };
    Some(match action {
        Action::Delete(id) => dispatch(
            state,
            req,
            "capture_delete",
            true,
            |_| Err(Fail::new(500, "failed", "not a read")),
            |s, body| delete(s, id, body),
        ),
        Action::List => answer(list(state)),
        Action::Get(id) => answer(get(state, id)),
        Action::Frames(id) => answer(frames(state, id, req.query)),
    })
}

fn answer(r: Result<Value, Fail>) -> CtlResponse {
    match r {
        Ok(body) => CtlResponse {
            status: 200,
            body,
            allow: None,
        },
        Err(f) => f.response(),
    }
}

fn source(state: &ApiState) -> Result<&dyn CaptureSource, Fail> {
    state.captures.as_deref().ok_or_else(|| {
        Fail::new(
            503,
            "unavailable",
            "no decoded-stream capture store on this server",
        )
    })
}

fn io_fail(e: &io::Error) -> Fail {
    if e.kind() == io::ErrorKind::Unsupported {
        Fail::new(
            503,
            "unavailable",
            "this capture store does not support that",
        )
    } else {
        Fail::new(500, "unreadable", "the capture store failed")
    }
}

fn checked_id(id: &str) -> Result<&str, Fail> {
    if is_capture_id(id) {
        Ok(id)
    } else {
        Err(Fail::invalid("capture ids are [A-Za-z0-9_.:-]{1,128}"))
    }
}

fn not_found() -> Fail {
    Fail::new(404, "not_found", "no such capture")
}

fn list(state: &ApiState) -> Result<Value, Fail> {
    let captures = source(state)?.list().map_err(|e| io_fail(&e))?;
    Ok(json!({ "captures": captures }))
}

fn get(state: &ApiState, id: &str) -> Result<Value, Fail> {
    let id = checked_id(id)?;
    let info = source(state)?
        .info(id)
        .map_err(|e| io_fail(&e))?
        .ok_or_else(not_found)?;
    Ok(json!(info))
}

fn delete(state: &ApiState, id: &str, body: &Map<String, Value>) -> Result<Applied, Fail> {
    no_fields(body)?;
    let id = checked_id(id)?;
    let src = source(state)?;
    let info = src
        .info(id)
        .map_err(|e| io_fail(&e))?
        .ok_or_else(not_found)?;
    match src.delete(id).map_err(|e| io_fail(&e))? {
        CaptureDelete::Deleted => Ok(Applied {
            status: 200,
            body: json!({ "deleted": info }),
            old: json!({ "id": id, "frames": info.frames, "bytes": info.bytes }),
            new: Value::Null,
        }),
        CaptureDelete::Recording => {
            Err(Fail::new(409, "conflict", "the capture is still recording"))
        }
        CaptureDelete::NotFound => Err(not_found()),
    }
}

fn query<'a>(q: &'a [(String, String)], key: &str) -> Option<&'a str> {
    q.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn uint(q: &[(String, String)], key: &str) -> Result<Option<u64>, Fail> {
    query(q, key)
        .map(|v| {
            v.parse::<u64>()
                .map_err(|_| Fail::invalid(format!("{key} is a non-negative integer")))
        })
        .transpose()
}

/// Unix seconds → Unix ns.
fn time_ns(q: &[(String, String)], key: &str) -> Result<Option<i64>, Fail> {
    query(q, key)
        .map(|v| {
            v.parse::<f64>()
                .ok()
                .filter(|s| s.is_finite() && s.abs() < 9.0e9)
                .map(|s| (s * 1e9).round() as i64)
                .ok_or_else(|| Fail::invalid(format!("{key} is a time in Unix seconds")))
        })
        .transpose()
}

fn frames(state: &ApiState, id: &str, q: &[(String, String)]) -> Result<Value, Fail> {
    const ALLOWED: [&str; 5] = ["from_frame", "from_t", "to_t", "limit", "token"];
    if let Some((k, _)) = q.iter().find(|(k, _)| !ALLOWED.contains(&k.as_str())) {
        return Err(Fail::invalid(format!(
            "unknown parameter {k:?} (allowed: from_frame, from_t, to_t, limit)"
        )));
    }
    let id = checked_id(id)?;
    let from_frame = uint(q, "from_frame")?;
    let from_t = time_ns(q, "from_t")?;
    let to_t = time_ns(q, "to_t")?;
    if from_frame.is_some() && from_t.is_some() {
        return Err(Fail::invalid("give from_frame or from_t, not both"));
    }
    let limit = uint(q, "limit")?.unwrap_or(DEFAULT_FRAMES_LIMIT);
    if !(1..=MAX_FRAMES_LIMIT).contains(&limit) {
        return Err(Fail::invalid(format!(
            "limit is an integer in 1..={MAX_FRAMES_LIMIT}"
        )));
    }
    let src = source(state)?;
    let info = src
        .info(id)
        .map_err(|e| io_fail(&e))?
        .ok_or_else(not_found)?;
    let from = match from_t {
        Some(t) => src
            .frame_at_time(id, t)
            .map_err(|e| io_fail(&e))?
            .ok_or_else(not_found)?,
        None => from_frame.unwrap_or(0),
    };
    let cursor = src
        .open_at(id, from)
        .map_err(|e| io_fail(&e))?
        .ok_or_else(not_found)?;
    let total = cursor.total_frames.unwrap_or(info.frames);
    let unreadable = |_| {
        Fail::new(
            422,
            "unreadable",
            "the capture is not a readable inspector stream",
        )
    };
    let mut records = RecordedFrames::open(cursor.reader).map_err(unreadable)?;
    let stream_ok = servable(records.header().content_class);
    let stream = stream_json(records.header().clone(), id, false);
    let mut index = cursor.first_frame;
    let mut page = Vec::new();
    let mut past_to_t = false;
    while (page.len() as u64) < limit {
        let Some(rec) = records.next_frame().map_err(unreadable)? else {
            break;
        };
        index += 1;
        if index <= from {
            continue;
        }
        if to_t.is_some_and(|t| rec.t > t) {
            past_to_t = true;
            break;
        }
        let parseable = stream_ok && servable(rec.content_class) && !rec.gated;
        page.push(serve_record(rec, parseable, None));
    }
    let next = from.saturating_add(page.len() as u64);
    let more = !past_to_t && (page.len() as u64) == limit && next < total;
    Ok(json!({
        "capture_id": id,
        "capture": info,
        "stream": stream,
        "total_frames": total,
        "from_frame": from,
        "limit": limit,
        "next_from_frame": more.then_some(next),
        "frames": page,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_only_its_paths() {
        assert!(matches!(
            resolve("GET", "/api/captures"),
            Some(Ok(Action::List))
        ));
        assert!(matches!(
            resolve("POST", "/api/captures"),
            Some(Err(Some("GET")))
        ));
        assert!(matches!(
            resolve("GET", "/api/captures/c1"),
            Some(Ok(Action::Get("c1")))
        ));
        assert!(matches!(
            resolve("DELETE", "/api/captures/c1"),
            Some(Ok(Action::Delete("c1")))
        ));
        assert!(matches!(
            resolve("PUT", "/api/captures/c1"),
            Some(Err(Some("GET, DELETE")))
        ));
        assert!(matches!(
            resolve("GET", "/api/captures/c1/frames"),
            Some(Ok(Action::Frames("c1")))
        ));
        assert!(resolve("POST", "/api/captures/c1/parse").is_none());
        assert!(resolve("GET", "/api/captures//frames").is_none());
        assert!(resolve("GET", "/api/capturesx").is_none());
    }
}
