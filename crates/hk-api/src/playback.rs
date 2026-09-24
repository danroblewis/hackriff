//! `GET/POST /api/playback` — the one playhead of historical playback (T-463, MPLAY).
//!
//! From CLAUDE.md's playback invariant: *play from a chosen past time with play/pause, advancing
//! as if live. Detections and history are never re-run — they replay exactly as recorded; demod
//! and decode are, being functions of raw IQ.*
//!
//! | Method | Path | Body | Answers |
//! |---|---|---|---|
//! | GET | `/api/playback` | – | `{playhead, max_speed, playheads, analysis, rerun, iq_horizon, streams}` |
//! | POST | `/api/playback` | any of `{"t"}` (Unix s) or `{"t_ns"}` (integer Unix ns), `"playing"` (bool), `"speed"` (0 < x ≤ `max_speed`) — at least one | the same object, after the change (audited `playback`) |
//!
//! The playhead is **view state over recorded history**, not a device action: moving, playing or
//! pausing it never reaches a front end, never stops or slows capture, the ring or detection
//! (CLAUDE.md, "Pause freezes the view, not the capture"). Audio at the playhead is the on-demand
//! opener `/ws/open/playback?f_lo&f_hi[&t][&speed]`, which follows this playhead and re-runs
//! demod/decode from the raw IQ the ring or a recording still holds — or refuses `409 no-iq` with
//! the horizon stated. The analysis a client draws around the playhead comes from the windowed
//! read routes it already uses (`/api/inventory?at=`, `/api/events`, `/api/tiles`,
//! `/api/inventory/{id}/decode?t0&t1`), which serve the records as written; nothing here writes
//! any.
//!
//! **One playhead.** The per-pane playhead is deferred by the user; see `hk_pipeline::playback`
//! for where it would go.

use serde_json::{Map, Value};

use crate::control::{
    Applied, CtlRequest, CtlResponse, Fail, dispatch, number, only, refuse_route,
};
use crate::http::ApiState;

/// A requested playhead change. Applied in this order: speed, position, then play/pause.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlaybackChange {
    /// Move to this capture time, Unix ns.
    pub t_ns: Option<i64>,
    /// Play (`true`) or pause (`false`).
    pub playing: Option<bool>,
    /// × real time.
    pub speed: Option<f64>,
}

/// A refused playhead change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaybackFailure {
    /// HTTP status.
    pub status: u16,
    /// Machine token.
    pub code: String,
    /// Reason.
    pub message: String,
}

/// The playhead behind this route (implemented by the composition over
/// `hk_pipeline::playback::PlaybackService`).
pub trait PlaybackControl: Send + Sync {
    /// The state JSON.
    fn state(&self) -> Value;
    /// Applies `change`; returns the state JSON after it.
    fn apply(&self, change: &PlaybackChange) -> Result<Value, PlaybackFailure>;
}

fn resolve(method: &str, path: &str) -> Option<Result<bool, Option<&'static str>>> {
    if path != "/api/playback" {
        return None;
    }
    Some(match method {
        "GET" => Ok(false),
        "POST" => Ok(true),
        _ => Err(Some("GET, POST")),
    })
}

/// This module's route; `None` = not mine.
pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    let mutating = match resolve(req.method, req.path)? {
        Ok(m) => m,
        Err(allow) => return Some(refuse_route(state, req, allow)),
    };
    if !mutating {
        if let Some((k, _)) = req.query.iter().find(|(k, _)| k != "token") {
            return Some(
                Fail::invalid(format!("unknown parameter {k:?} (none allowed)")).response(),
            );
        }
    }
    Some(dispatch(
        state,
        req,
        "playback",
        mutating,
        |s| Ok(control(s)?.state()),
        apply,
    ))
}

fn control(state: &ApiState) -> Result<&dyn PlaybackControl, Fail> {
    state
        .playback
        .as_deref()
        .ok_or_else(|| Fail::new(503, "unavailable", "no playback on this server"))
}

fn apply(state: &ApiState, body: &Map<String, Value>) -> Result<Applied, Fail> {
    only(body, &["t", "t_ns", "playing", "speed"])?;
    let ctl = control(state)?;
    let t_ns = match (body.get("t"), body.get("t_ns")) {
        (Some(_), Some(_)) => return Err(Fail::invalid("give t (Unix s) or t_ns, not both")),
        (Some(_), None) => {
            let t = number(body, "t")?.unwrap_or(-1.0);
            if !(0.0..9.2e9).contains(&t) {
                return Err(Fail::invalid("t is Unix seconds on the capture clock"));
            }
            Some((t * 1e9).round() as i64)
        }
        (None, Some(v)) => Some(
            v.as_i64()
                .filter(|n| *n >= 0)
                .ok_or_else(|| Fail::invalid("t_ns is a non-negative integer (Unix ns)"))?,
        ),
        (None, None) => None,
    };
    let playing = match body.get("playing") {
        None => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => return Err(Fail::invalid("playing is a boolean")),
    };
    let speed = number(body, "speed")?;
    let change = PlaybackChange {
        t_ns,
        playing,
        speed,
    };
    if change == PlaybackChange::default() {
        return Err(Fail::invalid(
            "give at least one of t, t_ns, playing, speed",
        ));
    }
    let old = ctl.state();
    let new = ctl.apply(&change).map_err(|f| {
        let code: &'static str = match f.code.as_str() {
            "invalid" => "invalid",
            "no_position" => "no_position",
            _ => "failed",
        };
        Fail::new(f.status, code, f.message)
    })?;
    Ok(Applied {
        status: 200,
        body: new.clone(),
        old: old["playhead"].clone(),
        new: new["playhead"].clone(),
    })
}

#[cfg(test)]
mod tests;
