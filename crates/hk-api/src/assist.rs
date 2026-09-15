//! Authoring-assist routes (`/api/assist/*`) (T-091, ADR-0011 §7): thin HTTP over
//! [`hk_estimate::assist`]. The client posts recorded bits or frames; the backend runs the
//! classical assist and answers **scored suggestions** (sync words, periods, field-map drafts,
//! `crc`/`bch`/`parity`/`sync_search` fragments) the user accepts or edits into a recipe. Nothing
//! is saved or applied, so the routes are compute-only: they need the token in the header (as
//! every POST) but no audit log.
//!
//! Input (`docs/api.md` "Authoring assist"):
//! - `bits`: one stream as `"0101…"`, or `{"hex": "…", "bit_len": n}`;
//! - `frames`: `[{"bits": "0101…"} | {"hex": "…", "bit_len": n}]` (MSB of byte 0 first, the frame
//!   packing rule), in capture order.
//!
//! Every call is bounded: `max_ops` (default and ceiling [`MAX_OPS`]) caps the work, and a capped
//! answer carries `work.partial: true`. The search runs on the connection thread, so at most
//! [`MAX_CONCURRENT`] calls compute at once; another call meanwhile answers 503 `busy` without
//! waiting.

use std::sync::atomic::{AtomicUsize, Ordering};

use hk_estimate::assist::{
    Budget, CodeSearchConfig, FieldsConfig, SyncAlign, SyncConfig, analyze_stream,
    hunt_sync_frames, search_codes, suggest_fields,
};
use serde_json::{Map, Value, json};

use crate::control::{CtlRequest, CtlResponse};
use crate::http::ApiState;

/// A refused request: status, stable code, message (the `{error, code}` body every route uses).
struct Bad {
    status: u16,
    code: &'static str,
    message: String,
}

impl Bad {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(400, "invalid", message)
    }

    fn into_response(self) -> CtlResponse {
        CtlResponse {
            status: self.status,
            body: json!({ "error": self.message, "code": self.code }),
            allow: None,
        }
    }
}

/// Ceiling for `max_ops`: about 2–3 s of one connection thread in a release build on the dev
/// Mac (the default, [`hk_estimate::assist::DEFAULT_MAX_OPS`], is about 1 s or less).
pub const MAX_OPS: u64 = hk_estimate::assist::MAX_OPS;
/// Most assist calls computing at once; another call meanwhile answers 503 `busy` at once.
pub const MAX_CONCURRENT: usize = 1;

static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// Assist calls computing right now (for tests and status).
#[doc(hidden)]
pub fn in_flight() -> usize {
    IN_FLIGHT.load(Ordering::SeqCst)
}

/// One of the [`MAX_CONCURRENT`] compute slots, released on drop.
struct Permit;

impl Permit {
    /// Takes a slot without waiting; `None` when all are taken.
    fn try_acquire() -> Option<Self> {
        IN_FLIGHT
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < MAX_CONCURRENT).then_some(n + 1)
            })
            .ok()
            .map(|_| Self)
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
    }
}
/// Most bits accepted in one call (stream or all frames together).
pub const MAX_BITS: usize = 400_000;
/// Most frames accepted in one call.
pub const MAX_FRAMES: usize = 20_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Sync,
    Fields,
    Crc,
}

fn resolve(method: &str, path: &str) -> Option<Result<Action, &'static str>> {
    let action = match path {
        "/api/assist/sync" => Action::Sync,
        "/api/assist/fields" => Action::Fields,
        "/api/assist/crc" => Action::Crc,
        _ => return None,
    };
    Some(if method == "POST" {
        Ok(action)
    } else {
        Err("POST")
    })
}

/// This module's routes; `None` = not mine.
pub(crate) fn route(_state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    let action = match resolve(req.method, req.path)? {
        Ok(a) => a,
        Err(allow) => {
            return Some(CtlResponse {
                status: 405,
                body: json!({ "error": format!("use {allow}"), "code": "method_not_allowed" }),
                allow: Some(allow),
            });
        }
    };
    // The search runs on this connection thread: bound how many run at once so assist calls
    // cannot tie up the server's connection threads (no queueing: a busy server answers now).
    let Some(_permit) = Permit::try_acquire() else {
        return Some(
            Bad::new(
                503,
                "busy",
                format!(
                    "an assist call is already running (at most {MAX_CONCURRENT} at once); retry"
                ),
            )
            .into_response(),
        );
    };
    Some(match run(action, req) {
        Ok(body) => CtlResponse {
            status: 200,
            body,
            allow: None,
        },
        Err(f) => f.into_response(),
    })
}

fn run(action: Action, req: &CtlRequest<'_>) -> Result<Value, Bad> {
    let body = parse_body(req)?;
    let budget = budget(&body)?;
    match action {
        Action::Sync => {
            only(
                &body,
                &[
                    "bits",
                    "frames",
                    "max_ops",
                    "max_errors",
                    "max_sync_bits",
                    "max_block_bits",
                    "max_lag",
                ],
            )?;
            let mut cfg = SyncConfig {
                budget,
                ..SyncConfig::default()
            };
            cfg.codes.budget = budget;
            if let Some(v) = opt_usize(&body, "max_errors", 16)? {
                cfg.max_errors = Some(v);
            }
            if let Some(v) = opt_usize(&body, "max_sync_bits", 64)? {
                cfg.max_sync_bits = v.max(8);
            }
            if let Some(v) = opt_usize(&body, "max_block_bits", 128)? {
                cfg.max_block_bits = v;
            }
            if let Some(v) = opt_usize(&body, "max_lag", 65_536)? {
                cfg.max_lag = v;
            }
            match (body.get("bits"), body.get("frames")) {
                (Some(b), None) => {
                    let bits = parse_bits("bits", b)?;
                    let r = analyze_stream(&bits, &cfg);
                    let mut v = ser(serde_json::to_value(&r))?;
                    v["input"] = json!("bits");
                    Ok(v)
                }
                (None, Some(f)) => {
                    let frames = parse_frames(f)?;
                    let r = hunt_sync_frames(&frames, &cfg);
                    let mut v = ser(serde_json::to_value(&r))?;
                    v["input"] = json!("frames");
                    Ok(v)
                }
                _ => Err(Bad::invalid("give exactly one of `bits` or `frames`")),
            }
        }
        Action::Fields => {
            only(&body, &["frames", "align", "find_crc", "max_ops"])?;
            let frames = parse_frames(
                body.get("frames")
                    .ok_or_else(|| Bad::invalid("`frames` is required"))?,
            )?;
            let given = frames.len();
            let mut cfg = FieldsConfig {
                budget,
                ..FieldsConfig::default()
            };
            cfg.codes.budget = budget;
            if let Some(a) = body.get("align") {
                let a = a.as_object().ok_or_else(|| {
                    Bad::invalid("`align` must be {\"sync\": \"0101…\", \"max_errors\"?}")
                })?;
                only(a, &["sync", "max_errors"])?;
                let sync = parse_bits(
                    "align.sync",
                    a.get("sync")
                        .ok_or_else(|| Bad::invalid("`align.sync` is required"))?,
                )?;
                if !(8..=64).contains(&sync.len()) {
                    return Err(Bad::invalid("`align.sync` must be 8–64 bits"));
                }
                let max_errors = opt_usize(a, "max_errors", 16)?.unwrap_or(sync.len() / 16);
                // Aligned inside the work cap.
                cfg.align = Some(SyncAlign { sync, max_errors });
            }
            if let Some(v) = body.get("find_crc") {
                cfg.find_codes = v
                    .as_bool()
                    .ok_or_else(|| Bad::invalid("`find_crc` must be a boolean"))?;
            }
            let r = suggest_fields(&frames, &cfg);
            let mut v = ser(serde_json::to_value(&r))?;
            v["frames_given"] = json!(given);
            v["frames_aligned"] = json!(r.frames);
            Ok(v)
        }
        Action::Crc => {
            only(
                &body,
                &[
                    "frames",
                    "max_ops",
                    "min_width",
                    "max_width",
                    "max_tail_bits",
                    "max_classes",
                ],
            )?;
            let frames = parse_frames(
                body.get("frames")
                    .ok_or_else(|| Bad::invalid("`frames` is required"))?,
            )?;
            let mut cfg = CodeSearchConfig {
                budget,
                ..CodeSearchConfig::default()
            };
            if let Some(v) = opt_usize(&body, "min_width", 32)? {
                cfg.min_width = (v as u8).max(3);
            }
            if let Some(v) = opt_usize(&body, "max_width", 32)? {
                cfg.max_width = (v as u8).max(3);
            }
            if let Some(v) = opt_usize(&body, "max_tail_bits", 256)? {
                cfg.max_tail_bits = v;
            }
            if let Some(v) = opt_usize(&body, "max_classes", 16)? {
                cfg.max_classes = v.max(1);
            }
            ser(serde_json::to_value(search_codes(&frames, &cfg)))
        }
    }
}

fn ser(v: Result<Value, serde_json::Error>) -> Result<Value, Bad> {
    v.map_err(|e| Bad::new(500, "failed", format!("serialising: {e}")))
}

fn parse_body(req: &CtlRequest<'_>) -> Result<Map<String, Value>, Bad> {
    let json_type = req.content_type.is_some_and(|t| {
        t.split(';')
            .next()
            .is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"))
    });
    if !json_type {
        return Err(Bad::new(
            415,
            "unsupported_media_type",
            "assist bodies must be Content-Type: application/json",
        ));
    }
    match serde_json::from_slice::<Value>(req.body) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(Bad::invalid("the body must be a JSON object")),
        Err(e) => Err(Bad::invalid(format!("malformed JSON body: {e}"))),
    }
}

fn only(body: &Map<String, Value>, allowed: &[&str]) -> Result<(), Bad> {
    match body.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(Bad::invalid(format!(
            "unknown key `{k}` (allowed: {})",
            allowed.join(", ")
        ))),
        None => Ok(()),
    }
}

fn budget(body: &Map<String, Value>) -> Result<Budget, Bad> {
    match body.get("max_ops") {
        None => Ok(Budget::default()),
        Some(v) => {
            let ops = v
                .as_u64()
                .filter(|&o| o > 0)
                .ok_or_else(|| Bad::invalid("`max_ops` must be a positive integer"))?;
            Ok(Budget {
                max_ops: ops.min(MAX_OPS),
            })
        }
    }
}

fn opt_usize(body: &Map<String, Value>, key: &str, max: usize) -> Result<Option<usize>, Bad> {
    body.get(key)
        .map(|v| {
            v.as_u64()
                .map(|x| x as usize)
                .filter(|&x| x <= max)
                .ok_or_else(|| Bad::invalid(format!("`{key}` must be an integer 0–{max}")))
        })
        .transpose()
}

fn parse_bits(key: &str, v: &Value) -> Result<Vec<u8>, Bad> {
    let bits = match v {
        Value::String(s) => s
            .bytes()
            .map(|c| match c {
                b'0' => Ok(0),
                b'1' => Ok(1),
                _ => Err(Bad::invalid(format!(
                    "`{key}` bit strings hold only 0 and 1"
                ))),
            })
            .collect::<Result<Vec<u8>, Bad>>()?,
        Value::Object(o) => {
            only(o, &["hex", "bit_len"])?;
            let hex = o
                .get("hex")
                .and_then(Value::as_str)
                .ok_or_else(|| Bad::invalid(format!("`{key}.hex` must be a hex string")))?;
            let digits: Vec<u8> = hex
                .trim_start_matches("0x")
                .bytes()
                .map(|c| {
                    (c as char)
                        .to_digit(16)
                        .map(|d| d as u8)
                        .ok_or_else(|| Bad::invalid(format!("`{key}.hex` is not hex")))
                })
                .collect::<Result<_, _>>()?;
            let all: Vec<u8> = digits
                .iter()
                .flat_map(|d| (0..4).rev().map(move |i| (d >> i) & 1))
                .collect();
            let bit_len = match o.get("bit_len") {
                None => all.len(),
                Some(b) => b
                    .as_u64()
                    .map(|x| x as usize)
                    .filter(|&x| x <= all.len())
                    .ok_or_else(|| {
                        Bad::invalid(format!("`{key}.bit_len` must be ≤ 4 × hex digits"))
                    })?,
            };
            all[..bit_len].to_vec()
        }
        _ => {
            return Err(Bad::invalid(format!(
                "`{key}` must be a bit string or {{\"hex\", \"bit_len\"}}"
            )));
        }
    };
    if bits.len() > MAX_BITS {
        return Err(Bad::invalid(format!("at most {MAX_BITS} bits per call")));
    }
    Ok(bits)
}

fn parse_frames(v: &Value) -> Result<Vec<Vec<u8>>, Bad> {
    let arr = v
        .as_array()
        .ok_or_else(|| Bad::invalid("`frames` must be an array"))?;
    if arr.len() > MAX_FRAMES {
        return Err(Bad::invalid(format!(
            "at most {MAX_FRAMES} frames per call"
        )));
    }
    let mut total = 0;
    arr.iter()
        .enumerate()
        .map(|(i, f)| {
            let key = format!("frames[{i}]");
            let bits = match f {
                Value::Object(o) if o.contains_key("bits") => {
                    only(o, &["bits"])?;
                    parse_bits(&key, &o["bits"])?
                }
                other => parse_bits(&key, other)?,
            };
            total += bits.len();
            if total > MAX_BITS {
                return Err(Bad::invalid(format!("at most {MAX_BITS} bits per call")));
            }
            Ok(bits)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assist_routes_resolve_with_methods() {
        assert_eq!(resolve("POST", "/api/assist/sync"), Some(Ok(Action::Sync)));
        assert_eq!(
            resolve("POST", "/api/assist/fields"),
            Some(Ok(Action::Fields))
        );
        assert_eq!(resolve("POST", "/api/assist/crc"), Some(Ok(Action::Crc)));
        assert_eq!(resolve("GET", "/api/assist/crc"), Some(Err("POST")));
        assert_eq!(resolve("POST", "/api/assistx"), None);
    }

    #[test]
    fn assist_bit_inputs_parse() {
        assert_eq!(parse_bits("b", &json!("0110")).ok(), Some(vec![0, 1, 1, 0]));
        assert_eq!(
            parse_bits("b", &json!({"hex": "0xA5", "bit_len": 6})).ok(),
            Some(vec![1, 0, 1, 0, 0, 1])
        );
        assert!(parse_bits("b", &json!("01x")).is_err());
        let frames = parse_frames(&json!([{"bits": "01"}, {"hex": "F0"}]))
            .ok()
            .unwrap();
        assert_eq!(frames, vec![vec![0, 1], vec![1, 1, 1, 1, 0, 0, 0, 0]]);
    }

    #[test]
    fn assist_second_call_is_busy_while_one_runs() {
        let state = ApiState::default();
        let body = br#"{"frames": ["0110", "1010", "1100"]}"#;
        let req = CtlRequest {
            method: "POST",
            path: "/api/assist/crc",
            body,
            content_type: Some("application/json"),
            caller: crate::control::Caller::default(),
        };
        // A running call holds the slot.
        let running = Permit::try_acquire().expect("free slot");
        let r = route(&state, &req).expect("mine");
        assert_eq!((r.status, r.body["code"].as_str()), (503, Some("busy")));
        drop(running);
        let r = route(&state, &req).expect("mine");
        assert_eq!(r.status, 200, "{}", r.body);
        assert_eq!(in_flight(), 0);
    }
}
