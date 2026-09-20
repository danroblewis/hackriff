//! `GET /api/recordings` — the persisted IQ recordings that extend the audio horizon (T-469).
//!
//! From CLAUDE.md's playback invariant:
//!
//! > playback has **two horizons**: spectrum history is long, but raw IQ exists only in the IQ
//! > ring (~30 min) **and in recordings**. Beyond the IQ horizon playback is waterfall-only with
//! > **no audio**, and **IQ availability is its own visible, predictable boundary on the time
//! > axis**.
//!
//! [`crate::iqbuffer`] already answers the ring's half of that boundary exactly. Nothing answered
//! the other half: `GET /api/captures` serves **decoded** captures, which is a different thing, so
//! "where can I hear audio" was answerable for the ring alone and any playhead built on it would
//! silently under-report the horizon over every recording beyond the ring. A client cannot draw a
//! boundary for files it cannot enumerate, so this route enumerates them.
//!
//! | Method | Path | Query | Answers |
//! |---|---|---|---|
//! | GET | `/api/recordings` | `?[t0=<unix s>][&t1=<unix s>][&kind=iq-snippet\|channel-decimated\|audio][&limit=1..1000, default 200]` | `{recordings, count, matched, omitted, iq_available}` |
//!
//! **It is a read, and an honest one.** The catalogue is a query over the immutable `Recording`
//! rows joined to their provenance ([`hk_store::recordings`]), with each row checked once against
//! the filesystem. A row is written after the samples are and never changes, so it keeps claiming
//! what it claimed when the file is later truncated, evicted or moved; `state` is therefore what
//! is on disk **now**, and only `state: "complete"` recordings appear in `iq_available.spans`. A
//! partially written or missing recording is listed — hiding it would be its own dishonesty — but
//! never as available. See [`hk_store::recordings`] for what the check does and does not cover.
//!
//! **`iq_available` is the ring plus these recordings**, as one list of spans each naming its
//! source, deliberately *not* merged into a single `t0`/`t1`: IQ availability is not contiguous,
//! and an envelope over a hole would promise audio that does not exist — the same defect as
//! implying resolution that was never captured. The spans cover exactly the page listed, so a
//! truncated page (`omitted > 0`) carries a truncated horizon.

use serde_json::{Value, json};

use hk_store::recordings::{
    DEFAULT_RECORDINGS_PAGE, MAX_RECORDINGS_PAGE, RecordingEntry, RecordingsQuery,
};

use crate::control::{CtlRequest, CtlResponse, Fail, refuse_route};
use crate::http::ApiState;

/// The catalogue of persisted recordings behind this route, implemented by the composition over
/// the run's repository and data directory.
pub trait RecordingCatalog: Send + Sync {
    /// Lists recordings matching `query`.
    fn list(
        &self,
        query: &RecordingsQuery,
    ) -> Result<hk_store::recordings::RecordingsCatalogue, RecordingsFailure>;
}

/// A failed catalogue read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordingsFailure {
    /// HTTP status.
    pub status: u16,
    /// Machine token (`failed`, `unavailable`).
    pub code: String,
    /// Reason.
    pub message: String,
}

fn resolve(method: &str, path: &str) -> Option<Result<(), Option<&'static str>>> {
    (path == "/api/recordings").then(|| {
        if method == "GET" {
            Ok(())
        } else {
            Err(Some("GET"))
        }
    })
}

/// This module's route; `None` = not mine.
pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    if let Err(allow) = resolve(req.method, req.path)? {
        return Some(refuse_route(state, req, allow));
    }
    Some(match list(state, req.query) {
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

/// A Unix-seconds bound, converted once to integer ns (the model's own unit).
fn time_ns(q: &[(String, String)], key: &str) -> Result<Option<i64>, Fail> {
    param(q, key)
        .map(|v| {
            v.parse::<f64>()
                .ok()
                .filter(|s| s.is_finite() && (0.0..9.2e9).contains(s))
                .map(|s| (s * 1e9).round() as i64)
                .ok_or_else(|| Fail::invalid(format!("{key} is a time in Unix seconds")))
        })
        .transpose()
}

fn parse(q: &[(String, String)]) -> Result<RecordingsQuery, Fail> {
    const ALLOWED: [&str; 5] = ["t0", "t1", "kind", "limit", "token"];
    if let Some((k, _)) = q.iter().find(|(k, _)| !ALLOWED.contains(&k.as_str())) {
        return Err(Fail::invalid(format!(
            "unknown parameter {k:?} (allowed: t0, t1, kind, limit)"
        )));
    }
    let (t0_ns, t1_ns) = (time_ns(q, "t0")?, time_ns(q, "t1")?);
    if let (Some(a), Some(b)) = (t0_ns, t1_ns) {
        if a >= b {
            return Err(Fail::invalid("t0 must be before t1"));
        }
    }
    let kind = param(q, "kind")
        .map(|v| {
            serde_json::from_value(Value::String(v.to_owned()))
                .map_err(|_| Fail::invalid("kind is one of iq-snippet, channel-decimated, audio"))
        })
        .transpose()?;
    let limit = match param(q, "limit") {
        None => DEFAULT_RECORDINGS_PAGE,
        Some(v) => v
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=MAX_RECORDINGS_PAGE).contains(n))
            .ok_or_else(|| {
                Fail::invalid(format!("limit is an integer in 1..={MAX_RECORDINGS_PAGE}"))
            })?,
    };
    Ok(RecordingsQuery {
        t0_ns,
        t1_ns,
        kind,
        limit,
    })
}

fn list(state: &ApiState, q: &[(String, String)]) -> Result<Value, Fail> {
    let query = parse(q)?;
    let catalog = state.recordings.as_deref().ok_or_else(|| {
        Fail::new(
            503,
            "unavailable",
            "no recording catalogue on this server (no data directory or database)",
        )
    })?;
    let cat = catalog.list(&query).map_err(|f| {
        let code = match f.code.as_str() {
            "unavailable" => "unavailable",
            "invalid" => "invalid",
            _ => "failed",
        };
        Fail::new(f.status, code, f.message)
    })?;
    let mut spans: Vec<Value> = cat
        .spans
        .iter()
        .map(|s| {
            span_json(
                s.t0_ns,
                s.t1_ns,
                "recording",
                Some(&s.recording.to_string()),
            )
        })
        .collect();
    let ring = ring_window(state);
    if let Some((t0_ns, t1_ns)) = ring.1 {
        spans.push(span_json(t0_ns, t1_ns, "ring", None));
    }
    // Oldest first, on the one shared time axis, whatever the source.
    spans.sort_by(|a, b| {
        let key = |v: &Value| {
            (
                v["t0_ns"].as_i64().unwrap_or(0),
                v["t1_ns"].as_i64().unwrap_or(0),
            )
        };
        key(a).cmp(&key(b))
    });
    Ok(json!({
        "recordings": cat.entries.iter().map(entry_json).collect::<Vec<_>>(),
        "count": cat.entries.len(),
        "matched": cat.matched,
        "omitted": cat.omitted,
        "iq_available": {
            // Both horizons, named, so a client never reads this as the ring alone or as the
            // (much longer, lossy) spectrum-history horizon.
            "horizon": "iq-ring + recordings",
            "ring": ring.0,
            // Never merged into one envelope: availability has holes, and an envelope over a
            // hole promises audio that does not exist.
            "spans": spans,
        },
    }))
}

fn span_json(t0_ns: i64, t1_ns: i64, source: &str, recording: Option<&str>) -> Value {
    let s = |ns: i64| ns as f64 / 1e9;
    json!({
        "t0": s(t0_ns),
        "t1": s(t1_ns),
        "t0_ns": t0_ns,
        "t1_ns": t1_ns,
        "span_s": s(t1_ns - t0_ns),
        "source": source,
        "recording": recording,
    })
}

/// The ring's own window, read from [`crate::iqbuffer`]'s status exactly as `/api/timeline` reads
/// it (`limit=1`: the window is wanted, not the segment list).
fn ring_window(state: &ApiState) -> (Value, Option<(i64, i64)>) {
    let Some(status) = state.iq_buffer.as_deref().map(|c| {
        c.status(&crate::iqbuffer::IqBufferQuery {
            t0: None,
            t1: None,
            limit: 1,
        })
    }) else {
        return (
            json!({
                "enabled": false,
                "reason": "no IQ capture buffer on this server",
                "t0": Value::Null,
                "t1": Value::Null,
            }),
            None,
        );
    };
    let f = |k: &str| {
        status
            .get(k)
            .and_then(Value::as_f64)
            .filter(|x| x.is_finite())
    };
    let held = match (f("t0"), f("t1")) {
        (Some(a), Some(b)) if b > a => Some(((a * 1e9).round() as i64, (b * 1e9).round() as i64)),
        _ => None,
    };
    (
        json!({
            "enabled": status.get("enabled").and_then(Value::as_bool).unwrap_or(false),
            "reason": status.get("reason").cloned().unwrap_or(Value::Null),
            "t0": held.map(|(a, _)| a as f64 / 1e9),
            "t1": held.map(|(_, b)| b as f64 / 1e9),
        }),
        held,
    )
}

fn entry_json(e: &RecordingEntry) -> Value {
    let r = &e.recording;
    let (t0_ns, t1_ns) = (r.time.start.as_unix_nanos(), r.time.end.as_unix_nanos());
    let s = |ns: i64| ns as f64 / 1e9;
    let (f_lo, f_hi) = e.window_hz();
    let p = e.provenance.as_ref();
    json!({
        "id": r.id.to_string(),
        "kind": serde_json::to_value(r.kind).unwrap_or(Value::Null),
        // Whether this can be re-demodulated and re-decoded on playback. Audio cannot.
        "iq": e.is_iq(),
        "t0": s(t0_ns),
        "t1": s(t1_ns),
        "t0_ns": t0_ns,
        "t1_ns": t1_ns,
        "duration_s": s(t1_ns - t0_ns),
        "center_hz": r.f_center_hz,
        "sample_rate_hz": r.sample_rate_hz,
        // The tuned window as captured, centre ± rate/2 — the ring's segment convention.
        "f_lo": f_lo,
        "f_hi": f_hi,
        "pre_trigger_s": r.pre_trigger_s,
        "post_trigger_s": r.post_trigger_s,
        "trigger": serde_json::to_value(r.trigger).unwrap_or(Value::Null),
        "retention_class": serde_json::to_value(r.retention_class).unwrap_or(Value::Null),
        "content_class": serde_json::to_value(r.content_class).unwrap_or(Value::Null),
        // Where the pipeline reads it from, relative to the data directory (as written).
        "meta_uri": r.meta_uri,
        "data_uri": r.data_uri,
        "size_bytes": r.size_bytes,
        // What is on disk NOW, not what the immutable row claims.
        "state": e.availability.as_str(),
        "available": e.availability.available(),
        "bytes_on_disk": e.bytes_on_disk,
        "meta_present": e.meta_present,
        "detail": e.detail,
        // Which front end captured it, and under what gain state (null when the provenance row
        // cannot be read — never guessed).
        "device_id": p.map(|p| p.device_id.clone()),
        "antenna_port": p.and_then(|p| p.antenna_port.clone()),
        "bias_tee": p.map(|p| p.bias_tee.as_str()),
        "bandwidth_hz": p.map(|p| p.tune.bandwidth_hz),
        "lna_db": p.map(|p| p.tune.lna_db),
        "vga_db": p.map(|p| p.tune.vga_db),
        "amp_on": p.map(|p| p.tune.amp_on),
        "overload": p.map(|p| p.overload),
    })
}

#[cfg(test)]
mod tests;
