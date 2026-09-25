//! Traced paths over a viewport (T-897, docs/23 §10.6 rule 2): `GET /api/paths`.
//!
//! The backend half of the map's `paths` layer (ADR-0023 §2): every chirp, sweep and hop sequence
//! whose route crosses a (time × frequency) window, as an ordered list of `(t, f)` vertices at
//! **absolute capture time**, each naming the detection it was measured from. The client lays the
//! vertices out through the pane's own capture-time mapping on every render frame; it derives
//! nothing (the thin-client rule).
//!
//! # Endpoint
//!
//! | Method | Path | Query | Answers |
//! |---|---|---|---|
//! | GET | `/api/paths` | `f_lo`, `f_hi` (Hz), `t0`, `t1` (Unix s) — all four; `kind`? (`chirp`/`sweep`/`hop`); `limit`? (200, max 1000) | `{window, context, paths, total, limit, truncated, detections_read, detections_truncated, method}` |
//!
//! **Derived on read, from measurement only.** The paths are [`hk_model::derive_paths`] over the
//! stored detections — immutable rows — recomputed per request exactly as presence intervals are
//! recomputed from the observation ledger (`presence.rs`). No band plan or catalogue is read.
//!
//! **Stable across pans.** A path is derived from the detections around the window, not only
//! inside it: the region read (`context`) is the window widened by its own span on each side in
//! both axes (the time margin clamped to [`MIN_CONTEXT_S`]..=[`MAX_CONTEXT_S`]). A route that crosses the window's edge
//! is therefore the same route — same vertices, same `id` — whichever part of it is on screen, so
//! panning never re-shapes a path. A path is served when its extent overlaps the window.
//!
//! **Bounded.** At most [`MAX_PATH_DETECTIONS`] detections are traced (the newest, when a context
//! holds more; `detections_truncated` says so) and at most `limit` paths are served (the earliest
//! first; `truncated` says so).

use std::sync::{MutexGuard, PoisonError};

use hk_model::{
    Detection, FreqRange, PathConfig, PathKind, Region, RepoError, Repository, TimeRange,
    Timestamp, TracedPath, VertexAt, derive_paths,
};
use serde_json::{Value, json};

use crate::control::{CtlRequest, CtlResponse, Fail, dispatch, refuse_route};
use crate::http::ApiState;
use crate::query::ts_s;

/// Default number of paths served.
pub const DEFAULT_PATHS_LIMIT: usize = 200;
/// Largest `limit`.
pub const MAX_PATHS_LIMIT: usize = 1_000;
/// Most detections one request traces paths through.
pub const MAX_PATH_DETECTIONS: usize = 20_000;
/// Longest time margin read either side of the window, s.
pub const MAX_CONTEXT_S: f64 = 600.0;
/// Shortest time margin read either side of the window, s. A path's kind depends on its
/// neighbours — one ramp of a sawtooth alone is a chirp — so a deep zoom must read the same
/// neighbours a wide one does. Measured through the mock device (`paths_blind.rs`): with only
/// the window's own 0.4 s either side, a 2.2 s sawtooth ramp was served as a chirp.
pub const MIN_CONTEXT_S: f64 = 60.0;

/// One validated request.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PathsQuery {
    /// The viewport.
    pub window: Region,
    /// Only this kind, if given.
    pub kind: Option<PathKind>,
    /// Most paths served.
    pub limit: usize,
}

fn get<'a>(q: &'a [(String, String)], k: &str) -> Option<&'a str> {
    q.iter()
        .find(|(a, _)| a == k)
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
}

fn num(q: &[(String, String)], k: &str) -> Result<f64, Fail> {
    let v =
        get(q, k).ok_or_else(|| Fail::invalid(format!("{k} is required (f_lo, f_hi, t0, t1)")))?;
    v.parse::<f64>()
        .ok()
        .filter(|x| x.is_finite())
        .ok_or_else(|| Fail::invalid(format!("{k} must be a finite number")))
}

fn ts(s: f64) -> Timestamp {
    Timestamp::from_unix_nanos((s * 1e9).round() as i64)
}

/// Parses the query: the whole window, always (a viewport route answers about a viewport).
pub(crate) fn parse(q: &[(String, String)]) -> Result<PathsQuery, Fail> {
    let (f_lo, f_hi, t0, t1) = (
        num(q, "f_lo")?,
        num(q, "f_hi")?,
        num(q, "t0")?,
        num(q, "t1")?,
    );
    if !(f_hi > f_lo && f_lo >= 0.0) {
        return Err(Fail::invalid("need 0 <= f_lo < f_hi (Hz)"));
    }
    if !(t1 > t0 && t0 > -4e9 && t1 < 9e9) {
        return Err(Fail::invalid("need t0 < t1 (Unix seconds)"));
    }
    let kind = match get(q, "kind") {
        None => None,
        Some("chirp") => Some(PathKind::Chirp),
        Some("sweep") => Some(PathKind::Sweep),
        Some("hop") => Some(PathKind::Hop),
        Some(_) => return Err(Fail::invalid("kind is one of chirp, sweep, hop")),
    };
    let limit = match get(q, "limit") {
        None => DEFAULT_PATHS_LIMIT,
        Some(v) => v
            .parse::<usize>()
            .ok()
            .filter(|&n| (1..=MAX_PATHS_LIMIT).contains(&n))
            .ok_or_else(|| Fail::invalid(format!("limit is 1..={MAX_PATHS_LIMIT}")))?,
    };
    Ok(PathsQuery {
        window: Region::new(FreqRange::new(f_lo, f_hi), TimeRange::new(ts(t0), ts(t1))),
        kind,
        limit,
    })
}

/// The region read for a window: widened by its own span each side, the time margin clamped to
/// `MIN_CONTEXT_S..=MAX_CONTEXT_S`.
pub fn context_of(window: &Region) -> Region {
    let (f_lo, f_hi) = (window.freq.lo_hz, window.freq.hi_hz);
    let span_f = f_hi - f_lo;
    let (t0, t1) = (ts_s(window.time.start), ts_s(window.time.end));
    let margin_t = (t1 - t0).clamp(MIN_CONTEXT_S, MAX_CONTEXT_S);
    Region::new(
        FreqRange::new((f_lo - span_f).max(0.0), f_hi + span_f),
        TimeRange::new(ts(t0 - margin_t), ts(t1 + margin_t)),
    )
}

fn vertex_at(at: VertexAt) -> &'static str {
    match at {
        VertexAt::Start => "start",
        VertexAt::Centre => "centre",
        VertexAt::End => "end",
    }
}

fn path_json(p: &TracedPath) -> Value {
    json!({
        "id": p.id(),
        "kind": p.kind.as_str(),
        "t0_s": ts_s(p.time.start),
        "t1_s": ts_s(p.time.end),
        "f_lo_hz": p.freq.lo_hz,
        "f_hi_hz": p.freq.hi_hz,
        "rate_hz_per_s": p.rate_hz_per_s,
        "ramps": p.ramps,
        "hops": p.hops,
        "channels_hz": p.channels_hz,
        "vertices": p.vertices.iter().map(|v| json!({
            "t_s": ts_s(v.t),
            "f_hz": v.f_hz,
            "detection": v.detection.to_string(),
            "at": vertex_at(v.at),
        })).collect::<Vec<_>>(),
        "provenance": {
            "method": p.provenance.method,
            "detections": p.provenance.detections.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "provenance_refs": p.provenance.provenance_refs.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "detector_versions": p.provenance.detector_versions,
            "surveys": p.provenance.surveys.iter().map(ToString::to_string).collect::<Vec<_>>(),
        },
    })
}

fn region_json(r: &Region) -> Value {
    json!({
        "f_lo_hz": r.freq.lo_hz,
        "f_hi_hz": r.freq.hi_hz,
        "t0_s": ts_s(r.time.start),
        "t1_s": ts_s(r.time.end),
    })
}

/// The answer for `q` over `repo`'s detections. Public so a pipeline test can assert on exactly
/// what the route serves for a blind run, without a server.
pub fn answer(repo: &Repository, q: &PathsQuery) -> Result<Value, RepoError> {
    let context = context_of(&q.window);
    let dets = repo.detections_in_region(&context)?;
    Ok(answer_over(dets, q, &context))
}

fn answer_over(mut dets: Vec<Detection>, q: &PathsQuery, context: &Region) -> Value {
    let read = dets.len();
    let detections_truncated = read > MAX_PATH_DETECTIONS;
    if detections_truncated {
        // Ordered by start: keep the newest, the end a live view is looking at.
        dets.drain(..read - MAX_PATH_DETECTIONS);
    }
    let all: Vec<TracedPath> = derive_paths(&dets, &PathConfig::default())
        .into_iter()
        .filter(|p| q.kind.is_none_or(|k| p.kind == k))
        .filter(|p| p.time.overlaps(&q.window.time) && p.freq.overlaps(&q.window.freq))
        .collect();
    let total = all.len();
    json!({
        "window": region_json(&q.window),
        "context": region_json(context),
        "paths": all.iter().take(q.limit).map(path_json).collect::<Vec<_>>(),
        "total": total,
        "limit": q.limit,
        "truncated": total > q.limit,
        "detections_read": read,
        "detections_truncated": detections_truncated,
        "method": hk_model::PATH_METHOD,
    })
}

fn store(state: &ApiState) -> Result<MutexGuard<'_, Repository>, Fail> {
    state
        .inventory
        .as_ref()
        .map(|r| r.lock().unwrap_or_else(PoisonError::into_inner))
        .ok_or_else(|| Fail::new(503, "unavailable", "no signal inventory on this server"))
}

fn read(state: &ApiState, q: &[(String, String)]) -> Result<Value, Fail> {
    let q = parse(q)?;
    let context = context_of(&q.window);
    // The lock is held for the read only; tracing runs after it is released.
    let dets = store(state)?
        .detections_in_region(&context)
        .map_err(|e| match e {
            RepoError::Invalid(m) => Fail::invalid(m),
            _ => Fail::new(500, "failed", "paths query failed"),
        })?;
    Ok(answer_over(dets, &q, &context))
}

/// Routes `/api/paths`; `None` for any other path.
pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    if req.path != "/api/paths" {
        return None;
    }
    if req.method != "GET" {
        return Some(refuse_route(state, req, Some("GET")));
    }
    Some(dispatch(
        state,
        req,
        "paths",
        false,
        |s| read(s, req.query),
        |_, _| Err(Fail::new(500, "failed", "not a mutating action")),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::ROUTES;

    fn q(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    }

    const OK: [(&str, &str); 4] = [
        ("f_lo", "1e8"),
        ("f_hi", "1.002e8"),
        ("t0", "100"),
        ("t1", "110"),
    ];

    #[test]
    fn the_route_is_in_the_table() {
        assert!(
            ROUTES
                .iter()
                .any(|(m, p)| *m == "GET" && *p == "/api/paths")
        );
    }

    #[test]
    fn the_window_is_required_whole_and_well_formed() {
        let p = parse(&q(&OK)).ok().expect("valid");
        assert_eq!(p.limit, DEFAULT_PATHS_LIMIT);
        assert_eq!(p.kind, None);
        for (key, _) in OK {
            let mut v = OK.to_vec();
            v.retain(|(a, _)| *a != key);
            assert!(parse(&q(&v)).is_err(), "missing {key}");
        }
        let bad = |k: &str, val: &str| {
            let mut v = OK.to_vec();
            v.retain(|(a, _)| *a != k);
            v.push((k, val));
            parse(&q(&v)).is_err()
        };
        assert!(bad("f_hi", "1e8"), "an empty band");
        assert!(bad("t1", "100"), "an empty window");
        assert!(bad("t0", "nan"));
        assert!(bad("f_lo", "-5"));
        let mut v = OK.to_vec();
        v.push(("kind", "radar"));
        assert!(parse(&q(&v)).is_err());
        v.pop();
        v.push(("limit", "0"));
        assert!(parse(&q(&v)).is_err());
        v.pop();
        v.push(("kind", "hop"));
        v.push(("limit", "5"));
        let p = parse(&q(&v)).ok().expect("valid");
        assert_eq!((p.kind, p.limit), (Some(PathKind::Hop), 5));
    }

    #[test]
    fn the_context_is_the_window_widened_by_its_own_span_with_the_time_margin_clamped() {
        let p = parse(&q(&OK)).ok().expect("valid");
        let c = context_of(&p.window);
        assert_eq!((c.freq.lo_hz, c.freq.hi_hz), (1e8 - 2e5, 1.002e8 + 2e5));
        // A 10 s window reads the 60 s floor either side, never less.
        assert_eq!((ts_s(c.time.start), ts_s(c.time.end)), (40.0, 170.0));
        let long = parse(&q(&[
            ("f_lo", "0"),
            ("f_hi", "1e6"),
            ("t0", "0"),
            ("t1", "7200"),
        ]))
        .ok()
        .expect("valid");
        let c = context_of(&long.window);
        assert_eq!(c.freq.lo_hz, 0.0, "never below 0 Hz");
        assert_eq!(
            (ts_s(c.time.start), ts_s(c.time.end)),
            (-MAX_CONTEXT_S, 7200.0 + MAX_CONTEXT_S)
        );
    }
}
