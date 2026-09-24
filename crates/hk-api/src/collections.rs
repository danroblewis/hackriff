//! Marker collections over HTTP (T-817, MAP-17; docs/25 §3 and §10): named, toggleable
//! collections of **time-frequency markers**, the durable research state the map UI's collection
//! layer and table read. The same token, audit and error rules as bookmarks and selections
//! ([`crate::control`]); stored in the run's user-metadata database ([`crate::ApiState::bookmarks`]),
//! so they survive a restart.
//!
//! # Endpoints
//! Bodies are JSON objects; unknown fields are refused (400). Frequencies are Hz, times Unix
//! seconds (floats). Errors answer `{"error", "code"}` with `invalid`, `not_found`, `conflict`,
//! `unavailable`.
//!
//! | Method | Path | Body / query | Answers |
//! |---|---|---|---|
//! | GET | `/api/collections` | `limit`? (500, max 2000), `cursor`? | `{collections, count, matched, limit, next_cursor}` |
//! | POST | `/api/collections` | `{"name", "note"?, "color"?, "visible"?, "id"?}` | `Collection` (201); 409 `conflict` when `id` exists |
//! | GET, PUT, DELETE | `/api/collections/<id>` | any create field but `id`; `{"visible"}` toggles | `Collection` / `{deleted, members_deleted}` |
//! | GET | `/api/collections/<id>/markers` | `limit`?, `cursor`?, `f_lo`/`f_hi`?, `t0`/`t1`? | `{markers, count, matched, limit, next_cursor}` |
//! | POST | `/api/collections/<id>/markers` | `{"name", "f_center_hz", "view", "bandwidth_hz"?, "t_center_s"?, "duration_s"?, "note"?, "id"?}` | `Marker` (201) |
//! | GET | `/api/markers` | as the collection's list, over every collection | `{markers, …}` |
//! | GET, PUT, DELETE | `/api/markers/<id>` | any create field but `id` (`null` clears optionals; `view` re-stamps provenance) | `Marker` / `{deleted}` |
//!
//! **Provenance is stamped here, never accepted** (docs/25 §10.2). The client sends `view` — the
//! pane it was on: `center_hz`, `span_hz`, `t_capture` (a capture-clock instant or `[t0, t1]`),
//! `tier` and, where the pane has one, `device_id`. The server adds `actor` (the token id, never
//! the token), `authored_s` (wall clock), `authored: true` and, when the named front end is live,
//! its `sample_rate_hz`. A `view` naming a server-owned field is `400 invalid`.
//!
//! **No entry carries a `device` key**: authoring a marker is a view act and reaches no radio.
//! Nothing here feeds blind detection (docs/25 §10.7).
//!
//! `/api/bookmarks` is a facade over the reserved `Bookmarks` collection
//! ([`hk_model::BOOKMARKS_COLLECTION`]): the two routes see the same rows. That collection cannot
//! be deleted (`400 invalid`) and holds frequency-only markers only.
//!
//! Mutating calls are audited as `collection_create`, `collection_update`, `collection_delete`,
//! `marker_create`, `marker_update`, `marker_delete`.

use std::sync::{MutexGuard, PoisonError};

use hk_model::{
    AuthoredProvenance, Collection, CollectionId, CollectionSummary, Marker, MarkerId,
    MarkerWindow, RepoError, Repository, Timestamp, ViewTier,
};
use serde_json::{Map, Value, json};

use crate::control::{
    Applied, CtlRequest, CtlResponse, Fail, dispatch, no_fields, nullable_number, number, ok, only,
    refuse_route, required, text,
};
use crate::http::ApiState;

/// Default page size of every list route here (docs/25 §10.3).
pub const DEFAULT_LIMIT: usize = 500;
/// Largest page size.
pub const MAX_LIMIT: usize = 2000;
/// Largest offset accepted as a cursor.
const MAX_CURSOR: usize = 10_000_000;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Action {
    List,
    Create,
    Get(CollectionId),
    Update(CollectionId),
    Delete(CollectionId),
    ListMarkers(Option<CollectionId>),
    CreateMarker(CollectionId),
    GetMarker(MarkerId),
    UpdateMarker(MarkerId),
    DeleteMarker(MarkerId),
}

impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::List => "collections_list",
            Self::Create => "collection_create",
            Self::Get(_) => "collection_get",
            Self::Update(_) => "collection_update",
            Self::Delete(_) => "collection_delete",
            Self::ListMarkers(_) => "markers_list",
            Self::CreateMarker(_) => "marker_create",
            Self::GetMarker(_) => "marker_get",
            Self::UpdateMarker(_) => "marker_update",
            Self::DeleteMarker(_) => "marker_delete",
        }
    }

    fn mutating(self) -> bool {
        !matches!(
            self,
            Self::List | Self::Get(_) | Self::ListMarkers(_) | Self::GetMarker(_)
        )
    }
}

/// Resolves `(method, path)` like `control::resolve`: `None` when `path` is not a collection or
/// marker path.
fn resolve(method: &str, path: &str) -> Option<Result<Action, Option<&'static str>>> {
    if path == "/api/collections" {
        return Some(match method {
            "GET" => Ok(Action::List),
            "POST" => Ok(Action::Create),
            _ => Err(Some("GET, POST")),
        });
    }
    if path == "/api/markers" {
        return Some(match method {
            "GET" => Ok(Action::ListMarkers(None)),
            _ => Err(Some("GET")),
        });
    }
    if let Some(rest) = path.strip_prefix("/api/markers/") {
        let Ok(id) = rest.parse::<MarkerId>() else {
            return Some(Err(None));
        };
        return Some(match method {
            "GET" => Ok(Action::GetMarker(id)),
            "PUT" => Ok(Action::UpdateMarker(id)),
            "DELETE" => Ok(Action::DeleteMarker(id)),
            _ => Err(Some("GET, PUT, DELETE")),
        });
    }
    let rest = path.strip_prefix("/api/collections/")?;
    let (id, markers) = match rest.strip_suffix("/markers") {
        Some(id) => (id, true),
        None => (rest, false),
    };
    let Ok(id) = id.parse::<CollectionId>() else {
        return Some(Err(None));
    };
    Some(match (markers, method) {
        (true, "GET") => Ok(Action::ListMarkers(Some(id))),
        (true, "POST") => Ok(Action::CreateMarker(id)),
        (true, _) => Err(Some("GET, POST")),
        (false, "GET") => Ok(Action::Get(id)),
        (false, "PUT") => Ok(Action::Update(id)),
        (false, "DELETE") => Ok(Action::Delete(id)),
        (false, _) => Err(Some("GET, PUT, DELETE")),
    })
}

/// Routes a collection or marker request; `None` when `path` is not one.
pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    let action = match resolve(req.method, req.path)? {
        Ok(a) => a,
        Err(allow) => return Some(refuse_route(state, req, allow)),
    };
    let actor = req.caller.token_id.clone();
    Some(dispatch(
        state,
        req,
        action.name(),
        action.mutating(),
        |s| read(s, action, req.query),
        |s, body| apply(s, action, body, actor),
    ))
}

fn store(state: &ApiState) -> Result<MutexGuard<'_, Repository>, Fail> {
    state
        .bookmarks
        .as_ref()
        .map(|r| r.lock().unwrap_or_else(PoisonError::into_inner))
        .ok_or_else(|| Fail::new(503, "unavailable", "no collection store on this server"))
}

fn repo_fail(e: RepoError) -> Fail {
    match e {
        RepoError::Invalid(m) => Fail::invalid(m),
        RepoError::NotFound { kind, .. } => Fail::new(404, "not_found", format!("no such {kind}")),
        other => Fail::new(500, "failed", format!("collection store: {other}")),
    }
}

fn secs(t: Timestamp) -> f64 {
    t.as_unix_nanos() as f64 / 1e9
}

/// Unix seconds → timestamp (finite, within ±292 years of 1970).
fn from_secs(key: &str, s: f64) -> Result<Timestamp, Fail> {
    if !(s.is_finite() && s.abs() < 9.2e9) {
        return Err(Fail::invalid(format!(
            "{key} must be a finite Unix time in seconds"
        )));
    }
    Ok(Timestamp::from_unix_nanos((s * 1e9).round() as i64))
}

/// A collection as the API serves it.
pub fn collection_json(c: &Collection, member_count: u64) -> Value {
    json!({
        "id": c.id.to_string(),
        "name": c.name,
        "note": c.note,
        "color": c.color,
        "visible": c.visible,
        "reserved": c.reserved,
        "member_count": member_count,
        "created_s": secs(c.created_at),
        "updated_s": secs(c.updated_at),
    })
}

fn summary_json(s: &CollectionSummary) -> Value {
    collection_json(&s.collection, s.member_count)
}

/// A marker as the API serves it. The frequency edges and time extent are computed here, so a
/// client never derives a span from two fields it was handed (the shared-time-axis rule).
pub fn marker_json(m: &Marker) -> Value {
    let (f_lo, f_hi) = m.f_range();
    let t = m.t_range();
    let p = &m.provenance;
    json!({
        "id": m.id.to_string(),
        "collection_id": m.collection_id.to_string(),
        "name": m.name,
        "note": m.note,
        "f_center_hz": m.f_center_hz,
        "bandwidth_hz": m.bandwidth_hz,
        "f_lo_hz": f_lo,
        "f_hi_hz": f_hi,
        "t_center_s": m.t_center.map(secs),
        "duration_s": m.duration_s,
        "t_start_s": t.map(|(a, _)| secs(a)),
        "t_end_s": t.map(|(_, b)| secs(b)),
        "provenance": {
            "device_id": p.device_id,
            "center_hz": p.center_hz,
            "span_hz": p.span_hz,
            "sample_rate_hz": p.sample_rate_hz,
            "t_capture": p.t_capture.map(|(a, b)| [secs(a), secs(b)]),
            "tier": p.tier.map(ViewTier::as_str),
            "authored_s": secs(p.authored_at),
            "actor": p.actor,
            "authored": true,
        },
        "created_s": secs(m.created_at),
        "updated_s": secs(m.updated_at),
    })
}

fn param<'a>(q: &'a [(String, String)], key: &str) -> Option<&'a str> {
    q.iter()
        .find(|(k, v)| k == key && !v.is_empty())
        .map(|(_, v)| v.as_str())
}

/// `limit` and `cursor` (docs/25 §10.3; the `/api/events` contract).
fn paging(q: &[(String, String)]) -> Result<(usize, usize), Fail> {
    let limit = match param(q, "limit") {
        None => DEFAULT_LIMIT,
        Some(v) => v
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=MAX_LIMIT).contains(n))
            .ok_or_else(|| Fail::invalid(format!("limit must be an integer in 1..={MAX_LIMIT}")))?,
    };
    let offset = match param(q, "cursor") {
        None => 0,
        Some(v) => v
            .parse::<usize>()
            .ok()
            .filter(|&o| o <= MAX_CURSOR)
            .ok_or_else(|| Fail::invalid("invalid cursor"))?,
    };
    Ok((offset, limit))
}

/// The optional window box: `f_lo`/`f_hi` (Hz) and `t0`/`t1` (Unix seconds), each pair both or
/// neither.
fn window(q: &[(String, String)]) -> Result<MarkerWindow, Fail> {
    let pair = |a: &str, b: &str| -> Result<Option<(f64, f64)>, Fail> {
        let num = |k: &str| -> Result<f64, Fail> {
            param(q, k)
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|v| v.is_finite())
                .ok_or_else(|| Fail::invalid(format!("{k} must be a finite number")))
        };
        match (param(q, a), param(q, b)) {
            (None, None) => Ok(None),
            (Some(_), Some(_)) => {
                let (lo, hi) = (num(a)?, num(b)?);
                if hi < lo {
                    return Err(Fail::invalid(format!("{b} must not be below {a}")));
                }
                Ok(Some((lo, hi)))
            }
            _ => Err(Fail::invalid(format!("{a} and {b} go together"))),
        }
    };
    let freq = pair("f_lo", "f_hi")?;
    let time = match pair("t0", "t1")? {
        None => None,
        Some((a, b)) => Some((from_secs("t0", a)?, from_secs("t1", b)?)),
    };
    Ok(MarkerWindow { freq, time })
}

fn next_cursor(offset: usize, count: usize, matched: u64) -> Value {
    let next = offset + count;
    if (next as u64) < matched && next <= MAX_CURSOR {
        Value::String(next.to_string())
    } else {
        Value::Null
    }
}

fn read(state: &ApiState, action: Action, q: &[(String, String)]) -> Result<Value, Fail> {
    match action {
        Action::List => {
            let (offset, limit) = paging(q)?;
            let page = store(state)?
                .collections(offset, limit)
                .map_err(repo_fail)?;
            Ok(json!({
                "collections": page.items.iter().map(summary_json).collect::<Vec<_>>(),
                "count": page.items.len(),
                "matched": page.matched,
                "limit": limit,
                "next_cursor": next_cursor(offset, page.items.len(), page.matched),
            }))
        }
        Action::Get(id) => {
            let repo = store(state)?;
            let c = repo.collection(id).map_err(repo_fail)?;
            let n = repo.collection_member_count(id).map_err(repo_fail)?;
            Ok(collection_json(&c, n))
        }
        Action::ListMarkers(coll) => {
            let (offset, limit) = paging(q)?;
            let w = window(q)?;
            let repo = store(state)?;
            if let Some(id) = coll {
                repo.collection(id).map_err(repo_fail)?;
            }
            let page = repo.markers(coll, w, offset, limit).map_err(repo_fail)?;
            Ok(json!({
                "collection_id": coll.map(|c| c.to_string()),
                "markers": page.items.iter().map(marker_json).collect::<Vec<_>>(),
                "count": page.items.len(),
                "matched": page.matched,
                "limit": limit,
                "next_cursor": next_cursor(offset, page.items.len(), page.matched),
            }))
        }
        Action::GetMarker(id) => store(state)?
            .marker(id)
            .map(|m| marker_json(&m))
            .map_err(repo_fail),
        _ => Err(Fail::new(500, "failed", "not a read")),
    }
}

const COLLECTION_CREATE: &[&str] = &["id", "name", "note", "color", "visible"];
const COLLECTION_UPDATE: &[&str] = &["name", "note", "color", "visible"];
const MARKER_CREATE: &[&str] = &[
    "id",
    "name",
    "note",
    "f_center_hz",
    "bandwidth_hz",
    "t_center_s",
    "duration_s",
    "view",
];
const MARKER_UPDATE: &[&str] = &[
    "name",
    "note",
    "f_center_hz",
    "bandwidth_hz",
    "t_center_s",
    "duration_s",
    "view",
];
/// What a client may say about the view it was on.
const VIEW_FIELDS: &[&str] = &["center_hz", "span_hz", "t_capture", "tier", "device_id"];
/// What only the server stamps (docs/25 §10.2).
const SERVER_OWNED: &[&str] = &["actor", "authored_s", "authored", "sample_rate_hz"];

fn name_field(body: &Map<String, Value>) -> Result<Option<String>, Fail> {
    match text(body, "name")? {
        None => Ok(None),
        Some(n) => Ok(Some(n.unwrap_or("").trim().to_owned())),
    }
}

fn boolean(body: &Map<String, Value>, key: &str) -> Result<Option<bool>, Fail> {
    match body.get(key) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(Fail::invalid(format!("{key} must be true or false"))),
    }
}

fn supplied_id<T: std::str::FromStr>(body: &Map<String, Value>) -> Result<Option<T>, Fail> {
    match body.get("id") {
        None => Ok(None),
        Some(Value::String(s)) => s
            .parse::<T>()
            .map(Some)
            .map_err(|_| Fail::invalid("id must be a UUID")),
        Some(_) => Err(Fail::invalid("id must be a UUID string")),
    }
}

fn apply_collection(body: &Map<String, Value>, c: &mut Collection) -> Result<(), Fail> {
    if let Some(n) = name_field(body)? {
        c.name = n;
    }
    if let Some(n) = text(body, "note")? {
        c.note = n.map(str::to_owned);
    }
    if let Some(col) = text(body, "color")? {
        c.color = col.map(str::to_owned);
    }
    if let Some(v) = boolean(body, "visible")? {
        c.visible = v;
    }
    Ok(())
}

/// The provenance stamp for a `view` the client reported (docs/25 §2, §10.2).
fn stamp(
    state: &ApiState,
    view: &Value,
    actor: Option<String>,
) -> Result<AuthoredProvenance, Fail> {
    let Value::Object(v) = view else {
        return Err(Fail::invalid(
            "view must be an object: {center_hz, span_hz, t_capture, tier, device_id?}",
        ));
    };
    if let Some(k) = v.keys().find(|k| SERVER_OWNED.contains(&k.as_str())) {
        return Err(Fail::invalid(format!(
            "view.{k} is stamped by the server; provenance is evidence, not input"
        )));
    }
    only(v, VIEW_FIELDS).map_err(|f| Fail::invalid(format!("view: {}", f.message())))?;
    let positive = |k: &str| -> Result<f64, Fail> {
        let x = required(v, k).map_err(|f| Fail::invalid(format!("view.{}", f.message())))?;
        if x > 0.0 {
            Ok(x)
        } else {
            Err(Fail::invalid(format!("view.{k} must be positive")))
        }
    };
    let center_hz = positive("center_hz")?;
    let span_hz = positive("span_hz")?;
    let tier = match v.get("tier") {
        Some(Value::String(s)) => ViewTier::parse(s).ok_or_else(|| {
            Fail::invalid(
                "view.tier must be \"live-iq\", \"spectrum-history\" or \"survey-overview\"",
            )
        })?,
        _ => return Err(Fail::invalid("view.tier is required")),
    };
    let t_capture = match v.get("t_capture") {
        Some(Value::Number(n)) => {
            let t = from_secs("view.t_capture", n.as_f64().unwrap_or(f64::NAN))?;
            (t, t)
        }
        Some(Value::Array(a)) if a.len() == 2 => {
            let t0 = from_secs("view.t_capture", a[0].as_f64().unwrap_or(f64::NAN))?;
            let t1 = from_secs("view.t_capture", a[1].as_f64().unwrap_or(f64::NAN))?;
            if t1 < t0 {
                return Err(Fail::invalid("view.t_capture ends before it starts"));
            }
            (t0, t1)
        }
        _ => {
            return Err(Fail::invalid(
                "view.t_capture is required: a capture-clock instant or [t0, t1], Unix seconds",
            ));
        }
    };
    let device_id = match v.get("device_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.is_empty() && s.chars().count() <= 200 => Some(s.clone()),
        Some(_) => {
            return Err(Fail::invalid(
                "view.device_id must be a non-empty string of at most 200 characters",
            ));
        }
    };
    // The one thing the server knows that the pane does not: the named front end's sample rate
    // right now. Only when the selector resolves to a live front end — never a guess, and a read
    // that never enters the device gate.
    let sample_rate_hz = state
        .live_controls
        .select(device_id.as_deref())
        .ok()
        .map(|c| c.tuning().sample_rate_hz)
        .filter(|r| r.is_finite() && *r > 0.0);
    Ok(AuthoredProvenance {
        device_id,
        center_hz: Some(center_hz),
        span_hz: Some(span_hz),
        sample_rate_hz,
        t_capture: Some(t_capture),
        tier: Some(tier),
        authored_at: Timestamp::now(),
        actor,
    })
}

/// Applies the marker fields of a create or update body to `m`.
fn apply_marker(body: &Map<String, Value>, m: &mut Marker) -> Result<(), Fail> {
    if let Some(n) = name_field(body)? {
        m.name = n;
    }
    if let Some(n) = text(body, "note")? {
        m.note = n.map(str::to_owned);
    }
    if let Some(f) = number(body, "f_center_hz")? {
        m.f_center_hz = f;
    }
    if let Some(bw) = nullable_number(body, "bandwidth_hz")? {
        m.bandwidth_hz = bw;
    }
    if let Some(t) = nullable_number(body, "t_center_s")? {
        m.t_center = t.map(|s| from_secs("t_center_s", s)).transpose()?;
        // Clearing the time makes a frequency-only pin, which has no duration either.
        if m.t_center.is_none() && !body.contains_key("duration_s") {
            m.duration_s = None;
        }
    }
    if let Some(d) = nullable_number(body, "duration_s")? {
        m.duration_s = d;
    }
    Ok(())
}

fn created(v: Value) -> Applied {
    Applied {
        status: 201,
        body: v.clone(),
        old: Value::Null,
        new: v,
    }
}

fn apply(
    state: &ApiState,
    action: Action,
    body: &Map<String, Value>,
    actor: Option<String>,
) -> Result<Applied, Fail> {
    match action {
        Action::Create => {
            only(body, COLLECTION_CREATE)?;
            let id = supplied_id::<CollectionId>(body)?;
            if name_field(body)?.is_none() {
                return Err(Fail::invalid("name is required"));
            }
            let mut c = Collection::new(String::new());
            if let Some(id) = id {
                c.id = id;
            }
            apply_collection(body, &mut c)?;
            c.validate().map_err(repo_fail)?;
            let mut repo = store(state)?;
            if repo.collection(c.id).is_ok() {
                return Err(Fail::new(
                    409,
                    "conflict",
                    format!("collection {} already exists; update it instead", c.id),
                ));
            }
            repo.insert_collection(&c).map_err(repo_fail)?;
            Ok(created(collection_json(&c, 0)))
        }
        Action::Update(id) => {
            only(body, COLLECTION_UPDATE)?;
            let mut repo = store(state)?;
            let stored = repo.collection(id).map_err(repo_fail)?;
            let n = repo.collection_member_count(id).map_err(repo_fail)?;
            let mut next = stored.clone();
            apply_collection(body, &mut next)?;
            next.updated_at = Timestamp::now().max(stored.created_at);
            let saved = repo.update_collection(&next).map_err(repo_fail)?;
            let new = collection_json(&saved, n);
            Ok(ok(new.clone(), collection_json(&stored, n), new))
        }
        Action::Delete(id) => {
            no_fields(body)?;
            let (deleted, n) = store(state)?.delete_collection(id).map_err(repo_fail)?;
            let old = collection_json(&deleted, n);
            Ok(ok(
                json!({ "deleted": old, "members_deleted": n }),
                old,
                Value::Null,
            ))
        }
        Action::CreateMarker(coll) => {
            only(body, MARKER_CREATE)?;
            let id = supplied_id::<MarkerId>(body)?;
            if name_field(body)?.is_none() {
                return Err(Fail::invalid("name is required"));
            }
            let f = required(body, "f_center_hz")?;
            let view = body.get("view").ok_or_else(|| {
                Fail::invalid("view is required: the pane the marker was placed on")
            })?;
            let provenance = stamp(state, view, actor)?;
            let mut m = Marker::new(coll, String::new(), f);
            if let Some(id) = id {
                m.id = id;
            }
            m.provenance = provenance;
            apply_marker(body, &mut m)?;
            m.validate().map_err(repo_fail)?;
            let mut repo = store(state)?;
            if repo.marker(m.id).is_ok() {
                return Err(Fail::new(
                    409,
                    "conflict",
                    format!("marker {} already exists; update it instead", m.id),
                ));
            }
            repo.insert_marker(&m).map_err(repo_fail)?;
            Ok(created(marker_json(&m)))
        }
        Action::UpdateMarker(id) => {
            only(body, MARKER_UPDATE)?;
            let restamp = body
                .get("view")
                .map(|v| stamp(state, v, actor))
                .transpose()?;
            let mut repo = store(state)?;
            let stored = repo.marker(id).map_err(repo_fail)?;
            let mut next = stored.clone();
            apply_marker(body, &mut next)?;
            if let Some(p) = restamp {
                next.provenance = p;
            }
            next.updated_at = Timestamp::now().max(stored.created_at);
            let saved = repo.update_marker(&next).map_err(repo_fail)?;
            let new = marker_json(&saved);
            Ok(ok(new.clone(), marker_json(&stored), new))
        }
        Action::DeleteMarker(id) => {
            no_fields(body)?;
            let deleted = store(state)?.delete_marker(id).map_err(repo_fail)?;
            let old = marker_json(&deleted);
            Ok(ok(json!({ "deleted": old }), old, Value::Null))
        }
        Action::List | Action::Get(_) | Action::ListMarkers(_) | Action::GetMarker(_) => {
            Err(Fail::new(500, "failed", "not a mutating action"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_resolve_to_their_actions() {
        let c = CollectionId::new();
        let m = MarkerId::new();
        assert_eq!(resolve("GET", "/api/collections"), Some(Ok(Action::List)));
        assert_eq!(
            resolve("POST", &format!("/api/collections/{c}/markers")),
            Some(Ok(Action::CreateMarker(c)))
        );
        assert_eq!(
            resolve("GET", "/api/markers"),
            Some(Ok(Action::ListMarkers(None)))
        );
        assert_eq!(
            resolve("DELETE", &format!("/api/markers/{m}")),
            Some(Ok(Action::DeleteMarker(m)))
        );
        assert_eq!(resolve("POST", "/api/markers"), Some(Err(Some("GET"))));
        assert_eq!(resolve("GET", "/api/collections/nope"), Some(Err(None)));
        assert_eq!(resolve("GET", "/api/bookmarks"), None);
    }
}
