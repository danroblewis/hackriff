//! Human-authored annotations over HTTP (T-816 / MAP-16, docs/25 §5 and §10, ADR-0023): durable
//! time–frequency notes with a server-stamped provenance, in the run's user-metadata database
//! ([`crate::ApiState::bookmarks`], beside bookmarks and selections), audited and token-gated like
//! the rest of the control API.
//!
//! # Endpoints
//!
//! | Method | Path | Body / query | Answers |
//! |---|---|---|---|
//! | GET | `/api/annotations` | `f_lo`, `f_hi`, `t0`, `t1` **required**; `limit`? (200, max 2000), `cursor`? | `{annotations, count, matched, next_cursor, window}` |
//! | POST | `/api/annotations` | `{kind, f_lo_hz, f_hi_hz, t0_s, t1_s, label, body?, collection_id?, id?, view}` | the annotation (201); 409 when `id` exists |
//! | GET, PUT, DELETE | `/api/annotations/<id>` | PUT: any create field but `id` (`null` clears `body`/`collection_id`) | the annotation / `{"deleted": ...}` |
//!
//! `view` is the view context the client was on — `{center_hz, span_hz, t_capture: [t0, t1], tier,
//! device_id?}` — and it is the **only** provenance input. The server stamps the rest (docs/25
//! §10.2): `actor` (the token fingerprint, never the token), `authored_s` (wall clock), `authored:
//! true`, and `sample_rate_hz` when this run holds the named device. A body carrying `provenance`,
//! `author` or any server-owned field is `400 invalid`: provenance is evidence, not input.
//!
//! **Never detection input.** Nothing here reaches the pipeline, the inventory or a radio route:
//! authoring is a view act, so no audit entry carries a `device` key (docs/25 §9, §10.7).

use std::sync::{MutexGuard, PoisonError};

use hk_model::{
    AnnotationId, AuthoredAnnotation, AuthoredKind, AuthoredProvenance, AuthoredTier, RepoError,
    Repository, Timestamp,
};
use serde_json::{Map, Value, json};

use crate::control::{
    Applied, CtlRequest, CtlResponse, Fail, dispatch, no_fields, number, ok, only, refuse_route,
    required, text,
};
use crate::http::ApiState;

/// `GET /api/annotations` default page size (docs/25 §10.3).
pub const DEFAULT_ANNOTATIONS_LIMIT: usize = 200;
/// `GET /api/annotations` largest page.
pub const MAX_ANNOTATIONS_LIMIT: usize = hk_model::AUTHORED_PAGE_MAX;
/// Largest offset accepted as a cursor.
const MAX_CURSOR: usize = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Action {
    List,
    Create,
    Get(AnnotationId),
    Update(AnnotationId),
    Delete(AnnotationId),
}

impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::List => "annotations_list",
            Self::Create => "annotation_create",
            Self::Get(_) => "annotation_get",
            Self::Update(_) => "annotation_update",
            Self::Delete(_) => "annotation_delete",
        }
    }

    fn mutating(self) -> bool {
        !matches!(self, Self::List | Self::Get(_))
    }
}

fn resolve(method: &str, path: &str) -> Option<Result<Action, Option<&'static str>>> {
    if path == "/api/annotations" {
        return Some(match method {
            "GET" => Ok(Action::List),
            "POST" => Ok(Action::Create),
            _ => Err(Some("GET, POST")),
        });
    }
    let rest = path.strip_prefix("/api/annotations/")?;
    let Ok(id) = rest.parse::<AnnotationId>() else {
        return Some(Err(None));
    };
    Some(match method {
        "GET" => Ok(Action::Get(id)),
        "PUT" => Ok(Action::Update(id)),
        "DELETE" => Ok(Action::Delete(id)),
        _ => Err(Some("GET, PUT, DELETE")),
    })
}

/// Routes an annotation request; `None` when `path` is not one.
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
        .ok_or_else(|| Fail::new(503, "unavailable", "no annotation store on this server"))
}

fn repo_fail(e: RepoError) -> Fail {
    match e {
        RepoError::Invalid(m) => Fail::invalid(m),
        RepoError::NotFound { .. } => Fail::new(404, "not_found", "no such annotation"),
        other => Fail::new(500, "failed", format!("annotation store: {other}")),
    }
}

fn secs(t: Timestamp) -> f64 {
    t.as_unix_nanos() as f64 / 1e9
}

/// Unix seconds (capture clock) → timestamp.
fn from_secs(key: &str, s: f64) -> Result<Timestamp, Fail> {
    if !(s.is_finite() && s.abs() < 9.2e9) {
        return Err(Fail::invalid(format!(
            "{key} must be a finite Unix time in seconds"
        )));
    }
    Ok(Timestamp::from_unix_nanos((s * 1e9).round() as i64))
}

/// An annotation as the API serves it: seconds on the wire, capture-clock and wall-clock times
/// under distinct names.
pub fn annotation_json(a: &AuthoredAnnotation) -> Value {
    let p = &a.provenance;
    json!({
        "id": a.id.to_string(),
        "collection_id": a.collection_id,
        "kind": a.kind.as_str(),
        "f_lo_hz": a.f_lo_hz,
        "f_hi_hz": a.f_hi_hz,
        "t0_s": secs(a.t0),
        "t1_s": secs(a.t1),
        "label": a.label,
        "body": a.body,
        "author": a.author,
        "provenance": {
            "device_id": p.device_id,
            "center_hz": p.center_hz,
            "span_hz": p.span_hz,
            "sample_rate_hz": p.sample_rate_hz,
            "t_capture": [secs(p.t_capture[0]), secs(p.t_capture[1])],
            "tier": p.tier.as_str(),
            "authored_s": secs(p.authored_at),
            "actor": p.actor,
            "authored": p.authored,
        },
        "created_s": secs(a.created_at),
        "updated_s": secs(a.updated_at),
    })
}

fn query<'a>(q: &'a [(String, String)], key: &str) -> Option<&'a str> {
    q.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())
}

fn query_f64(q: &[(String, String)], key: &str) -> Result<f64, Fail> {
    let v = query(q, key).ok_or_else(|| {
        Fail::invalid(format!(
            "{key} is required: GET /api/annotations answers a window (f_lo, f_hi, t0, t1)"
        ))
    })?;
    v.parse::<f64>()
        .ok()
        .filter(|x| x.is_finite())
        .ok_or_else(|| Fail::invalid(format!("{key} must be a finite number")))
}

fn read(state: &ApiState, action: Action, q: &[(String, String)]) -> Result<Value, Fail> {
    match action {
        Action::List => {
            let (f_lo, f_hi) = (query_f64(q, "f_lo")?, query_f64(q, "f_hi")?);
            let (t0, t1) = (query_f64(q, "t0")?, query_f64(q, "t1")?);
            if f_hi < f_lo || t1 < t0 {
                return Err(Fail::invalid("the window needs f_lo <= f_hi and t0 <= t1"));
            }
            let limit = match query(q, "limit") {
                None => DEFAULT_ANNOTATIONS_LIMIT,
                Some(v) => v
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (1..=MAX_ANNOTATIONS_LIMIT).contains(n))
                    .ok_or_else(|| {
                        Fail::invalid(format!(
                            "limit must be an integer in 1..={MAX_ANNOTATIONS_LIMIT}"
                        ))
                    })?,
            };
            let offset = match query(q, "cursor") {
                None => 0,
                Some(c) => c
                    .parse::<usize>()
                    .ok()
                    .filter(|&o| o <= MAX_CURSOR)
                    .ok_or_else(|| Fail::invalid("invalid cursor"))?,
            };
            let page = store(state)?
                .authored_annotations_in(
                    f_lo,
                    f_hi,
                    from_secs("t0", t0)?,
                    from_secs("t1", t1)?,
                    offset,
                    limit,
                )
                .map_err(repo_fail)?;
            let next = offset + page.rows.len();
            Ok(json!({
                "window": { "f_lo_hz": f_lo, "f_hi_hz": f_hi, "t0_s": t0, "t1_s": t1 },
                "annotations": page.rows.iter().map(annotation_json).collect::<Vec<_>>(),
                "count": page.rows.len(),
                "matched": page.matched,
                "limit": limit,
                "next_cursor": ((next as u64) < page.matched && next <= MAX_CURSOR)
                    .then(|| next.to_string()),
            }))
        }
        Action::Get(id) => store(state)?
            .authored_annotation(id)
            .map(|a| annotation_json(&a))
            .map_err(repo_fail),
        _ => Err(Fail::new(500, "failed", "not a read")),
    }
}

const CREATE_FIELDS: &[&str] = &[
    "id",
    "kind",
    "f_lo_hz",
    "f_hi_hz",
    "t0_s",
    "t1_s",
    "label",
    "body",
    "collection_id",
    "view",
];
const UPDATE_FIELDS: &[&str] = &[
    "kind",
    "f_lo_hz",
    "f_hi_hz",
    "t0_s",
    "t1_s",
    "label",
    "body",
    "collection_id",
    "view",
];
/// Provenance fields only the server may write (docs/25 §10.2).
const SERVER_OWNED: &[&str] = &[
    "provenance",
    "author",
    "actor",
    "authored",
    "authored_s",
    "created_s",
    "updated_s",
];
const VIEW_FIELDS: &[&str] = &["center_hz", "span_hz", "t_capture", "tier", "device_id"];

/// Refuses a server-owned field by name before the generic unknown-field check, so the error says
/// why rather than just "unknown".
fn refuse_server_owned(body: &Map<String, Value>) -> Result<(), Fail> {
    let view_keys = body
        .get("view")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|m| m.keys());
    match body
        .keys()
        .chain(view_keys)
        .find(|k| SERVER_OWNED.contains(&k.as_str()))
    {
        Some(k) => Err(Fail::invalid(format!(
            "{k} is stamped by the server and may not be supplied: send only `view`, the view \
             context you were on"
        ))),
        None => Ok(()),
    }
}

fn kind(v: Option<&str>) -> Result<AuthoredKind, Fail> {
    serde_json::from_value(json!(v.unwrap_or("")))
        .map_err(|_| Fail::invalid("kind must be \"text\", \"box\" or \"marker\""))
}

/// Parses `view` and stamps it into provenance: the client's view context plus what the server
/// knows authoritatively.
fn stamp(
    state: &ApiState,
    view: &Value,
    actor: Option<String>,
) -> Result<AuthoredProvenance, Fail> {
    let m = view.as_object().ok_or_else(|| {
        Fail::invalid(
            "view must be an object {center_hz, span_hz, t_capture: [t0, t1], tier, device_id?}",
        )
    })?;
    only(m, VIEW_FIELDS).map_err(|_| {
        Fail::invalid(format!(
            "view carries only {} (the server stamps the rest)",
            VIEW_FIELDS.join(", ")
        ))
    })?;
    let center_hz =
        required(m, "center_hz").map_err(|_| Fail::invalid("view.center_hz is required"))?;
    let span_hz = required(m, "span_hz").map_err(|_| Fail::invalid("view.span_hz is required"))?;
    let t_capture = match m
        .get("t_capture")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
    {
        Some([a, b]) => match (a.as_f64(), b.as_f64()) {
            (Some(a), Some(b)) => [
                from_secs("view.t_capture", a)?,
                from_secs("view.t_capture", b)?,
            ],
            _ => return Err(Fail::invalid("view.t_capture must be [t0_s, t1_s]")),
        },
        _ => {
            return Err(Fail::invalid(
                "view.t_capture is required: [t0_s, t1_s] on the capture clock",
            ));
        }
    };
    let tier: AuthoredTier = m
        .get("tier")
        .and_then(|t| serde_json::from_value(t.clone()).ok())
        .ok_or_else(|| {
            Fail::invalid(
                "view.tier must be \"live-iq\", \"spectrum-history\" or \"survey-overview\"",
            )
        })?;
    let device_id = text(m, "device_id")
        .map_err(|_| Fail::invalid("view.device_id must be a string or null"))?
        .flatten()
        .map(str::to_owned);
    // The named device's rate, only when this run holds it — never a guess for a device we cannot
    // see, and never the "primary" radio standing in for an unnamed pane.
    let sample_rate_hz = device_id.as_deref().and_then(|d| {
        state
            .live_controls
            .iter()
            .find(|l| l.device_id() == Some(d))
            .map(|l| l.tuning().sample_rate_hz)
    });
    Ok(AuthoredProvenance {
        device_id,
        center_hz,
        span_hz,
        sample_rate_hz,
        t_capture,
        tier,
        authored_at: Timestamp::now(),
        actor,
        authored: true,
    })
}

/// Applies the geometry/text fields present in `body` (PUT) or all of them (POST).
fn apply_fields(body: &Map<String, Value>, a: &mut AuthoredAnnotation) -> Result<(), Fail> {
    if let Some(k) = text(body, "kind")? {
        a.kind = kind(k)?;
    }
    if let Some(f) = number(body, "f_lo_hz")? {
        a.f_lo_hz = f;
    }
    if let Some(f) = number(body, "f_hi_hz")? {
        a.f_hi_hz = f;
    }
    if let Some(t) = number(body, "t0_s")? {
        a.t0 = from_secs("t0_s", t)?;
    }
    if let Some(t) = number(body, "t1_s")? {
        a.t1 = from_secs("t1_s", t)?;
    }
    if let Some(l) = text(body, "label")? {
        a.label = l
            .map(str::trim)
            .ok_or_else(|| Fail::invalid("label must be a non-empty string"))?
            .to_owned();
    }
    if let Some(b) = text(body, "body")? {
        a.body = b.map(str::to_owned);
    }
    if let Some(c) = text(body, "collection_id")? {
        // Checked as a UUID by `AuthoredAnnotation::validate` (400 otherwise).
        a.collection_id = c.map(|c| c.trim().to_owned());
    }
    Ok(())
}

fn apply(
    state: &ApiState,
    action: Action,
    body: &Map<String, Value>,
    actor: Option<String>,
) -> Result<Applied, Fail> {
    match action {
        Action::Create => {
            refuse_server_owned(body)?;
            only(body, CREATE_FIELDS)?;
            for key in ["kind", "label"] {
                if text(body, key)?.flatten().is_none() {
                    return Err(Fail::invalid(format!("{key} is required")));
                }
            }
            let view = body.get("view").ok_or_else(|| {
                Fail::invalid("view is required: the view context the annotation was authored on")
            })?;
            let provenance = stamp(state, view, actor.clone())?;
            let now = Timestamp::now();
            let mut a = AuthoredAnnotation {
                id: AnnotationId::new(),
                collection_id: None,
                kind: AuthoredKind::Text,
                f_lo_hz: required(body, "f_lo_hz")?,
                f_hi_hz: required(body, "f_hi_hz")?,
                t0: from_secs("t0_s", required(body, "t0_s")?)?,
                t1: from_secs("t1_s", required(body, "t1_s")?)?,
                label: String::new(),
                body: None,
                author: actor,
                provenance,
                created_at: now,
                updated_at: now,
            };
            if let Some(id) = text(body, "id")?.flatten() {
                a.id = id.parse().map_err(|_| Fail::invalid("id must be a UUID"))?;
            }
            apply_fields(body, &mut a)?;
            a.validate().map_err(repo_fail)?;
            let mut repo = store(state)?;
            if repo.authored_annotation(a.id).is_ok() {
                return Err(Fail::new(
                    409,
                    "conflict",
                    format!("annotation {} already exists; update it instead", a.id),
                ));
            }
            repo.insert_authored_annotation(&a).map_err(repo_fail)?;
            let new = annotation_json(&a);
            Ok(Applied {
                status: 201,
                body: new.clone(),
                old: Value::Null,
                new,
            })
        }
        Action::Update(id) => {
            refuse_server_owned(body)?;
            only(body, UPDATE_FIELDS)?;
            let mut repo = store(state)?;
            let stored = repo.authored_annotation(id).map_err(repo_fail)?;
            let mut next = stored.clone();
            apply_fields(body, &mut next)?;
            // A new view re-stamps provenance (the edit was made from there, by this actor); no
            // view keeps the original stamp. The author is who first wrote it either way.
            if let Some(view) = body.get("view") {
                next.provenance = stamp(state, view, actor)?;
            }
            next.updated_at = Timestamp::now().max(stored.created_at);
            let saved = repo.update_authored_annotation(&next).map_err(repo_fail)?;
            let new = annotation_json(&saved);
            Ok(ok(new.clone(), annotation_json(&stored), new))
        }
        Action::Delete(id) => {
            no_fields(body)?;
            let deleted = store(state)?
                .delete_authored_annotation(id)
                .map_err(repo_fail)?;
            let old = annotation_json(&deleted);
            Ok(ok(json!({ "deleted": old }), old, Value::Null))
        }
        Action::List | Action::Get(_) => Err(Fail::new(500, "failed", "not a mutating action")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::ROUTES;

    #[test]
    fn routes_resolve_with_methods() {
        let id = AnnotationId::new();
        assert_eq!(resolve("GET", "/api/annotations"), Some(Ok(Action::List)));
        assert_eq!(
            resolve("POST", "/api/annotations"),
            Some(Ok(Action::Create))
        );
        assert_eq!(
            resolve("DELETE", "/api/annotations"),
            Some(Err(Some("GET, POST")))
        );
        assert_eq!(
            resolve("PUT", &format!("/api/annotations/{id}")),
            Some(Ok(Action::Update(id)))
        );
        assert_eq!(
            resolve("POST", &format!("/api/annotations/{id}")),
            Some(Err(Some("GET, PUT, DELETE")))
        );
        assert_eq!(resolve("GET", "/api/annotations/nope"), Some(Err(None)));
        assert_eq!(resolve("GET", "/api/annotationsx"), None);
        assert_eq!(resolve("GET", "/api/selections"), None);
    }

    #[test]
    fn every_annotation_route_is_in_the_route_table() {
        let listed: Vec<_> = ROUTES
            .iter()
            .filter(|(_, p)| p.starts_with("/api/annotations"))
            .collect();
        assert_eq!(listed.len(), 5);
        for (method, path) in listed {
            let concrete = path.replace("{id}", &AnnotationId::new().to_string());
            assert!(
                matches!(resolve(method, &concrete), Some(Ok(_))),
                "{method} {path}"
            );
        }
    }

    #[test]
    fn server_owned_provenance_is_refused_by_name() {
        let body: Map<String, Value> = serde_json::from_value(json!({
            "view": { "center_hz": 1.0, "actor": "me" }
        }))
        .unwrap();
        assert!(refuse_server_owned(&body).is_err());
        let body: Map<String, Value> = serde_json::from_value(json!({ "provenance": {} })).unwrap();
        assert!(refuse_server_owned(&body).is_err());
        let body: Map<String, Value> =
            serde_json::from_value(json!({ "view": { "center_hz": 1.0 } })).unwrap();
        assert!(refuse_server_owned(&body).is_ok());
    }
}
