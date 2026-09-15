//! Inspector routes (T-089, ADR-0011 §3–4, `docs/api.md` "Inspector"): evaluate a **draft** field
//! map over frames and over a recorded decoded stream, without saving anything. The server does
//! all the parsing; responses carry layer trees with absolute bit and byte ranges per node and
//! the per-byte leaf index, so the UI links fields and bytes without range arithmetic.
//!
//! | Method | Path | Body | Answers |
//! |---|---|---|---|
//! | POST | `/api/inspector/parse` | `{field_map, frames: [{hex, bit_len?}]}` (1–500 frames) | `{frames: [{bit_len, hex, layers}], fit}` |
//! | POST | `/api/captures/{id}/parse` | `{field_map?, from_frame?, limit?}` | `{capture_id, stream, total_frames, from_frame, limit, next_from_frame, frames, fit}` |
//!
//! A recorded decoded stream is the §3 byte stream itself (`docs/stream-contract.md` §14.7),
//! opened through [`ApiState::captures`] ([`hk_stream::inspector::CaptureSource`], implemented by
//! the capture store). `from_frame`/`limit` page over the recording's frame records in order;
//! the `fit` summary covers **every** frame of the recording (up to [`MAX_FIT_FRAMES`], then
//! marked `truncated`), so one request answers "does my guess hold across the whole capture".
//! Frames whose class forbids content (or that are only
//! served to local consumers) are returned metadata-only and are never parsed.
//!
//! Errors are `{"error", "code"}` (`invalid`, `not_found`, `unavailable`, `unreadable`,
//! `unsupported_media_type`); an invalid field map answers `400 invalid` with
//! `errors: [{path, message}]`. Messages never echo values. These routes read only: they need
//! the token like every POST but are not audited.

use hk_model::ContentClass;
use hk_recipe::FieldMap;
use hk_recipe::fields::eval::Evaluator;
use hk_stream::inspector::{
    FitSummary, FrameRecord, InspectorSource, RecordedFrames, from_hex, to_hex,
};
use serde_json::{Map, Value, json};

use crate::control::{CtlRequest, CtlResponse, refuse_route};
use crate::http::ApiState;

/// Most frames per parse request or page.
pub const MAX_PARSE_FRAMES: usize = 500;
/// Default page size of a capture re-parse.
pub const DEFAULT_PARSE_LIMIT: usize = 100;
/// Most frames a capture re-parse evaluates for its `fit` summary, bounding CPU per request;
/// a longer recording's summary covers its first frames and says `truncated: true`. Page frames
/// past the cap are still parsed.
pub const MAX_FIT_FRAMES: u64 = 100_000;

enum Action<'a> {
    ParseFrames,
    ParseCapture(&'a str),
}

/// `None` when `path` is not an inspector path.
fn resolve<'a>(method: &str, path: &'a str) -> Option<Result<Action<'a>, Option<&'static str>>> {
    let action = if path == "/api/inspector/parse" {
        Action::ParseFrames
    } else {
        let id = path
            .strip_prefix("/api/captures/")?
            .strip_suffix("/parse")?;
        if id.is_empty() || id.contains('/') {
            return None;
        }
        Action::ParseCapture(id)
    };
    Some(if method == "POST" {
        Ok(action)
    } else {
        Err(Some("POST"))
    })
}

/// This module's routes; `None` = not mine.
pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    let action = match resolve(req.method, req.path)? {
        Ok(a) => a,
        Err(allow) => return Some(refuse_route(state, req, allow)),
    };
    let result = parse_body(req).and_then(|body| match action {
        Action::ParseFrames => parse_frames(&body),
        Action::ParseCapture(id) => parse_capture(state, id, &body),
    });
    Some(match result {
        Ok(body) => CtlResponse {
            status: 200,
            body,
            allow: None,
        },
        Err(r) => r,
    })
}

fn fail(status: u16, code: &str, message: impl Into<String>) -> CtlResponse {
    CtlResponse {
        status,
        body: json!({ "error": message.into(), "code": code }),
        allow: None,
    }
}

fn invalid(message: impl Into<String>) -> CtlResponse {
    fail(400, "invalid", message)
}

fn parse_body(req: &CtlRequest<'_>) -> Result<Map<String, Value>, CtlResponse> {
    let json_type = req.content_type.is_some_and(|t| {
        t.split(';')
            .next()
            .is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"))
    });
    if !json_type {
        return Err(fail(
            415,
            "unsupported_media_type",
            "bodies must be Content-Type: application/json",
        ));
    }
    match serde_json::from_slice::<Value>(req.body) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(invalid("the body must be a JSON object")),
        Err(e) => Err(invalid(format!(
            "malformed JSON body (line {}, column {})",
            e.line(),
            e.column()
        ))),
    }
}

fn only(body: &Map<String, Value>, allowed: &[&str]) -> Result<(), CtlResponse> {
    match body.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(invalid(format!(
            "unknown field {k:?} (allowed: {})",
            allowed.join(", ")
        ))),
        None => Ok(()),
    }
}

/// The body's `field_map`, validated and compiled; `None` when absent.
fn evaluator(body: &Map<String, Value>) -> Result<Option<Evaluator>, CtlResponse> {
    let Some(v) = body.get("field_map").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let map = FieldMap::deserialize_value(v).map_err(|message| CtlResponse {
        status: 400,
        body: json!({
            "error": "field_map does not match the field-map schema",
            "code": "invalid",
            "errors": [{ "path": "", "message": message }],
        }),
        allow: None,
    })?;
    Evaluator::new(&map)
        .map(Some)
        .map_err(|errors| CtlResponse {
            status: 400,
            body: json!({
                "error": "field_map is invalid",
                "code": "invalid",
                "errors": errors
                    .iter()
                    .map(|e| json!({ "path": e.path, "message": e.message }))
                    .collect::<Vec<_>>(),
            }),
            allow: None,
        })
}

trait DeserializeValue: Sized {
    fn deserialize_value(v: &Value) -> Result<Self, String>;
}

impl DeserializeValue for FieldMap {
    /// Schema errors name the offending key or type, never the submitted value.
    fn deserialize_value(v: &Value) -> Result<Self, String> {
        serde_json::from_value(v.clone()).map_err(|e| {
            let s = e.to_string();
            if s.starts_with("unknown field") || s.starts_with("missing field") {
                s
            } else {
                "a key has the wrong type or value".to_owned()
            }
        })
    }
}

fn parse_frames(body: &Map<String, Value>) -> Result<Value, CtlResponse> {
    only(body, &["field_map", "frames"])?;
    let ev = evaluator(body)?.ok_or_else(|| invalid("field_map is required"))?;
    let frames = body
        .get("frames")
        .and_then(Value::as_array)
        .filter(|a| (1..=MAX_PARSE_FRAMES).contains(&a.len()))
        .ok_or_else(|| {
            invalid(format!(
                "frames is a list of 1..={MAX_PARSE_FRAMES} {{hex, bit_len?}} objects"
            ))
        })?;
    let mut fit = FitSummary::default();
    let mut out = Vec::with_capacity(frames.len());
    for (i, f) in frames.iter().enumerate() {
        let obj = f
            .as_object()
            .ok_or_else(|| invalid(format!("frames[{i}] is an object")))?;
        only(obj, &["hex", "bit_len"])?;
        let bytes = obj
            .get("hex")
            .and_then(Value::as_str)
            .and_then(from_hex)
            .ok_or_else(|| invalid(format!("frames[{i}].hex is an even-length hex string")))?;
        let max_bits = bytes.len() as u64 * 8;
        let bit_len = match obj.get("bit_len") {
            None => max_bits,
            Some(v) => v.as_u64().filter(|b| *b <= max_bits).ok_or_else(|| {
                invalid(format!(
                    "frames[{i}].bit_len is an integer no larger than 8 × the bytes"
                ))
            })?,
        };
        let bit_len = u32::try_from(bit_len).map_err(|_| invalid("frame too long"))?;
        let tree = ev.eval(&bytes, bit_len);
        fit.add(Some(&tree));
        out.push(json!({ "bit_len": bit_len, "hex": to_hex(&bytes), "layers": tree }));
    }
    Ok(json!({ "frames": out, "fit": fit }))
}

/// Whether frame content may be served over HTTP: the class permits content and is not
/// local-only (`own-key-decrypted` is served to Unix-socket consumers only, contract §2).
pub(crate) fn servable(class: ContentClass) -> bool {
    hk_stream::gate::remote_transport_permitted(class) && class.permits_content()
}

pub(crate) fn is_capture_id(id: &str) -> bool {
    id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
}

fn parse_capture(
    state: &ApiState,
    id: &str,
    body: &Map<String, Value>,
) -> Result<Value, CtlResponse> {
    only(body, &["field_map", "from_frame", "limit"])?;
    if !is_capture_id(id) {
        return Err(invalid("capture ids are [A-Za-z0-9_.:-]{1,128}"));
    }
    let ev = evaluator(body)?;
    let from = match body.get("from_frame") {
        None => 0,
        Some(v) => v
            .as_u64()
            .ok_or_else(|| invalid("from_frame is a non-negative integer"))?,
    };
    let limit = match body.get("limit") {
        None => DEFAULT_PARSE_LIMIT as u64,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_PARSE_FRAMES as u64).contains(n))
            .ok_or_else(|| invalid(format!("limit is an integer in 1..={MAX_PARSE_FRAMES}")))?,
    };
    let source = state.captures.as_deref().ok_or_else(|| {
        fail(
            503,
            "unavailable",
            "no decoded-stream capture store on this server",
        )
    })?;
    let open_at = |at: u64| {
        source
            .open_at(id, at)
            .map_err(|_| fail(500, "unreadable", "the capture could not be opened"))?
            .ok_or_else(|| fail(404, "not_found", "no such capture"))
    };
    let unreadable = |_| {
        fail(
            422,
            "unreadable",
            "the capture is not a readable inspector stream",
        )
    };
    // T-092: a store with a frame index seeks. Without a field map only the page is read; with
    // one, the fit pass reads the first MAX_FIT_FRAMES and a page past them is a second seek.
    let cursor = open_at(if ev.is_some() { 0 } else { from })?;
    let known_total = cursor.total_frames;
    let first = cursor.first_frame;
    let mut frames = RecordedFrames::open(cursor.reader).map_err(unreadable)?;
    let header = frames.header().clone();
    let scan = match known_total {
        None => {
            let pass = scan_frames(
                &mut frames,
                first,
                ev.as_ref(),
                from,
                limit,
                MAX_FIT_FRAMES,
                u64::MAX,
            )
            .map_err(unreadable)?;
            Scan {
                fit: fit_json(ev.as_ref(), pass.fit, pass.end, MAX_FIT_FRAMES),
                page: pass.page,
                total: pass.end,
            }
        }
        Some(total) => {
            let page_end = total.min(from.saturating_add(limit));
            let stop = if ev.is_some() {
                total.min(MAX_FIT_FRAMES)
            } else {
                page_end
            };
            let mut pass = scan_frames(
                &mut frames,
                first,
                ev.as_ref(),
                from,
                limit,
                MAX_FIT_FRAMES,
                stop,
            )
            .map_err(unreadable)?;
            if pass.end < page_end {
                let c = open_at(from.max(pass.end))?;
                let start = c.first_frame;
                let mut rest = RecordedFrames::open(c.reader).map_err(unreadable)?;
                let more = scan_frames(
                    &mut rest,
                    start,
                    ev.as_ref(),
                    from,
                    limit,
                    MAX_FIT_FRAMES,
                    page_end,
                )
                .map_err(unreadable)?;
                // Page frames below the first pass's end were already served by it.
                let skip = pass.end.saturating_sub(from.max(start)) as usize;
                pass.page.extend(more.page.into_iter().skip(skip));
            }
            Scan {
                fit: fit_json(ev.as_ref(), pass.fit, total, MAX_FIT_FRAMES),
                page: pass.page,
                total,
            }
        }
    };
    let total = scan.total;
    let stream = stream_json(header, id, ev.is_some());
    let next = from.saturating_add(limit);
    Ok(json!({
        "capture_id": id,
        "stream": stream,
        "total_frames": total,
        "from_frame": from,
        "limit": limit,
        "next_from_frame": (next < total).then_some(next),
        "frames": scan.page,
        "fit": scan.fit,
    }))
}

struct Scan {
    page: Vec<Value>,
    /// The fit summary with `truncated`; `None` without a field map.
    fit: Option<Value>,
    total: u64,
}

/// The `stream` object of a capture response: header identity with `inspector.source` set to
/// the capture.
pub(crate) fn stream_json(header: hk_stream::StreamHeader, id: &str, reparse: bool) -> Value {
    let mut stream = json!({
        "stream_id": header.stream_id,
        "content_class": header.content_class,
        "message_schema": header.message_schema,
    });
    if let Some(mut profile) = header.inspector {
        profile.source = InspectorSource::Capture {
            capture_id: id.to_owned(),
            reparse,
        };
        stream["inspector"] = json!(profile);
    }
    stream
}

fn fit_json(ev: Option<&Evaluator>, fit: FitSummary, total: u64, fit_cap: u64) -> Option<Value> {
    ev.map(|_| {
        let mut v = json!(fit);
        v["truncated"] = json!(total > fit_cap);
        v
    })
}

/// Reads every frame record: serves `[from, from + limit)` and, with a field map, sums the fit
/// of the first `fit_cap` frames.
#[cfg(test)]
fn reparse<R: std::io::Read>(
    frames: &mut RecordedFrames<R>,
    ev: Option<&Evaluator>,
    from: u64,
    limit: u64,
    fit_cap: u64,
) -> Result<Scan, hk_stream::ClientError> {
    let pass = scan_frames(frames, 0, ev, from, limit, fit_cap, u64::MAX)?;
    Ok(Scan {
        fit: fit_json(ev, pass.fit, pass.end, fit_cap),
        page: pass.page,
        total: pass.end,
    })
}

struct Pass {
    page: Vec<Value>,
    fit: FitSummary,
    /// Index after the last frame record read.
    end: u64,
}

/// Reads frame records numbered from `first` until index `stop` (or the end): serves those in
/// `[from, from + limit)` and, with a field map, sums the fit of those below `fit_cap`.
fn scan_frames<R: std::io::Read>(
    frames: &mut RecordedFrames<R>,
    first: u64,
    ev: Option<&Evaluator>,
    from: u64,
    limit: u64,
    fit_cap: u64,
    stop: u64,
) -> Result<Pass, hk_stream::ClientError> {
    let stream_servable = servable(frames.header().content_class);
    let mut fit = FitSummary::default();
    let mut page = Vec::new();
    let mut total = first;
    while total < stop {
        let Some(rec) = frames.next_frame()? else {
            break;
        };
        let index = total;
        total += 1;
        let in_page = index >= from && index - from < limit;
        let in_fit = ev.is_some() && index < fit_cap;
        if !in_page && !in_fit {
            continue;
        }
        let parseable = stream_servable && servable(rec.content_class) && !rec.gated;
        let tree = match ev {
            Some(ev) if parseable => ev.eval_record(&rec),
            _ => None,
        };
        if in_fit {
            fit.add(tree.as_ref());
        }
        if in_page {
            page.push(serve_record(rec, parseable, tree));
        }
    }
    Ok(Pass {
        page,
        fit,
        end: total,
    })
}

/// A stored frame record as served: content withheld when not servable (fail closed), layers
/// from the re-parse when one ran.
pub(crate) fn serve_record(
    mut rec: FrameRecord,
    parseable: bool,
    tree: Option<hk_stream::inspector::LayerTree>,
) -> Value {
    if !parseable {
        rec.content = None;
        rec.gated = true;
    } else if let (Some(tree), Some(content)) = (tree, rec.content.as_mut()) {
        rec.metadata.fit = Some(tree.fit);
        content.layers = Some(tree);
    }
    serde_json::to_value(rec).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_only_its_paths() {
        assert!(matches!(
            resolve("POST", "/api/inspector/parse"),
            Some(Ok(Action::ParseFrames))
        ));
        assert!(matches!(
            resolve("POST", "/api/captures/c1/parse"),
            Some(Ok(Action::ParseCapture("c1")))
        ));
        assert!(matches!(
            resolve("GET", "/api/captures/c1/parse"),
            Some(Err(Some("POST")))
        ));
        assert!(resolve("GET", "/api/captures/c1/frames").is_none());
        assert!(resolve("POST", "/api/captures//parse").is_none());
        assert!(resolve("GET", "/api/captures").is_none());
    }

    /// The fit summary stops at the cap and says so; paging and `total` still see every frame.
    #[test]
    fn capture_fit_summary_is_capped_and_marked_truncated() {
        use hk_stream::frame::encode_frame;
        use hk_stream::inspector::{FrameContent, FrameMetadata, INSPECTOR_MESSAGE_SCHEMA};
        use hk_stream::{StreamHeader, StreamKind};

        let mut h = StreamHeader::new(
            "inspector/p/frames",
            StreamKind::Messages,
            ContentClass::Unrestricted,
            "test",
        );
        h.message_schema = Some(INSPECTOR_MESSAGE_SCHEMA.into());
        let mut bytes = Vec::new();
        encode_frame(&mut bytes, &h.to_json_bytes().unwrap(), h.max_frame_len).unwrap();
        for i in 0..10u64 {
            let rec = FrameRecord {
                record_type: "frame".into(),
                seq: i,
                t: 0,
                content_class: ContentClass::Unrestricted,
                gated: false,
                crc_status: None,
                decoder: None,
                frame_model: None,
                emitter_id: None,
                metadata: FrameMetadata {
                    frame: Some(i),
                    bit_len: Some(8),
                    ..Default::default()
                },
                content: Some(FrameContent {
                    hex: to_hex(&[i as u8]),
                    layers: None,
                }),
            };
            let json = serde_json::to_vec(&rec).unwrap();
            encode_frame(&mut bytes, &json, h.max_frame_len).unwrap();
        }
        let map: FieldMap = serde_json::from_value(json!({
            "fields": [{"name": "b", "type": "uint", "length": 1}]
        }))
        .unwrap();
        let ev = Evaluator::new(&map).unwrap();
        let scan = |cap| {
            let mut frames = RecordedFrames::open(std::io::Cursor::new(bytes.clone())).unwrap();
            reparse(&mut frames, Some(&ev), 8, 2, cap).unwrap()
        };

        let capped = scan(4);
        assert_eq!(capped.total, 10);
        let fit = capped.fit.unwrap();
        assert_eq!(
            (fit["frames"].as_u64(), fit["ok"].as_u64()),
            (Some(4), Some(4))
        );
        assert_eq!(fit["truncated"], true);
        assert_eq!(capped.page.len(), 2, "pages past the cap are served");
        assert_eq!(
            capped.page[1]["content"]["layers"]["fit"], "ok",
            "and parsed"
        );

        let whole = scan(10).fit.unwrap();
        assert_eq!(whole["frames"], 10);
        assert_eq!(whole["truncated"], false);
    }
}
