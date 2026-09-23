//! Saved views over HTTP (T-819 / MAP-19, docs/25 §6 and §10, ADR-0023 §5): named, restorable
//! (time × frequency) window extents — the ArcGIS-bookmark analogue — in the run's user-metadata
//! database ([`crate::ApiState::bookmarks`], beside bookmarks, selections and measurements),
//! audited and token-gated like the rest of the control API.
//!
//! # Endpoints
//!
//! | Method | Path | Body / query | Answers |
//! |---|---|---|---|
//! | GET | `/api/views` | `f_lo`/`f_hi`/`t0`/`t1`? (all four or none), `limit`? (500, max 2000), `cursor`? | `{views, count, matched, limit, next_cursor, window}` |
//! | POST | `/api/views` | `{name, note?, center_f_hz, span_f_hz, center_t_s?, span_t_s?, follow_live, pane_layout?, id?, view}` | the view (201); 409 when `id` exists |
//! | GET, PUT, DELETE | `/api/views/<id>` | PUT: any create field but `id` (`null` clears `note`/`center_t_s`/`span_t_s`/`pane_layout`) | the view / `{"deleted": ...}` |
//!
//! **A saved view is a named point in view-arithmetic state, not a device command.** There is no
//! "restore" route: restoring is client view arithmetic over already-captured data, and a
//! frequency extent outside the tuned window raises the client's ordinary gated retune offer
//! through the one `DeviceAction` path — no exemption (ADR-0023 §5). So nothing here reaches a
//! front end, and no audit entry carries a `device` key (docs/25 §10.5).
//!
//! **Shareable.** Every served view carries `share`: exactly the POST body (minus the `view`
//! provenance input) that re-creates it — on this server or another — with its id preserved
//! where it does not collide (docs/25 §8). Provenance is never shared in: the receiving server
//! stamps its own, as for every store (docs/25 §10.2).

use std::sync::{MutexGuard, PoisonError};

use hk_model::{
    RepoError, Repository, SAVED_VIEW_PAGE_MAX, SavedView, SavedViewFilter, SavedViewId, Timestamp,
};
use serde_json::{Map, Value, json};

use crate::control::{
    Applied, CtlRequest, CtlResponse, Fail, dispatch, no_fields, nullable_number, ok, only,
    refuse_route, required, text,
};
use crate::http::ApiState;
use crate::measurements::{SERVER_OWNED, from_secs, query, query_f64, secs, stamp};

/// `GET /api/views` default page size (docs/25 §10.3).
pub const DEFAULT_VIEWS_LIMIT: usize = 500;
/// `GET /api/views` largest page.
pub const MAX_VIEWS_LIMIT: usize = SAVED_VIEW_PAGE_MAX;
/// Largest offset accepted as a cursor.
const MAX_CURSOR: usize = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Action {
    List,
    Create,
    Get(SavedViewId),
    Update(SavedViewId),
    Delete(SavedViewId),
}

impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::List => "views_list",
            Self::Create => "view_create",
            Self::Get(_) => "view_get",
            Self::Update(_) => "view_update",
            Self::Delete(_) => "view_delete",
        }
    }

    fn mutating(self) -> bool {
        !matches!(self, Self::List | Self::Get(_))
    }
}

fn resolve(method: &str, path: &str) -> Option<Result<Action, Option<&'static str>>> {
    if path == "/api/views" {
        return Some(match method {
            "GET" => Ok(Action::List),
            "POST" => Ok(Action::Create),
            _ => Err(Some("GET, POST")),
        });
    }
    let rest = path.strip_prefix("/api/views/")?;
    let Ok(id) = rest.parse::<SavedViewId>() else {
        return Some(Err(None));
    };
    Some(match method {
        "GET" => Ok(Action::Get(id)),
        "PUT" => Ok(Action::Update(id)),
        "DELETE" => Ok(Action::Delete(id)),
        _ => Err(Some("GET, PUT, DELETE")),
    })
}

/// Routes a saved-view request; `None` when `path` is not one.
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
        .ok_or_else(|| Fail::new(503, "unavailable", "no saved-view store on this server"))
}

fn repo_fail(e: RepoError) -> Fail {
    match e {
        RepoError::Invalid(m) => Fail::invalid(m),
        RepoError::NotFound { .. } => Fail::new(404, "not_found", "no such saved view"),
        other => Fail::new(500, "failed", format!("saved-view store: {other}")),
    }
}

/// The re-creating POST body of a view (minus `view`): what `share` carries.
fn share_json(v: &SavedView) -> Value {
    json!({
        "id": v.id.to_string(),
        "name": v.name,
        "note": v.note,
        "center_f_hz": v.center_f_hz,
        "span_f_hz": v.span_f_hz,
        "center_t_s": v.center_t.map(secs),
        "span_t_s": v.span_t_s,
        "follow_live": v.follow_live,
        "pane_layout": v.pane_layout,
    })
}

/// A saved view as the API serves it: seconds on the wire, capture-clock and wall-clock times
/// under distinct names.
pub fn view_json(v: &SavedView) -> Value {
    let p = &v.provenance;
    let mut out = share_json(v);
    let m = out.as_object_mut().expect("share_json is an object");
    m.insert(
        "provenance".into(),
        json!({
            "device_id": p.device_id,
            "center_hz": p.center_hz,
            "span_hz": p.span_hz,
            "sample_rate_hz": p.sample_rate_hz,
            "t_capture": [secs(p.t_capture[0]), secs(p.t_capture[1])],
            "tier": p.tier.as_str(),
            "authored_s": secs(p.authored_at),
            "actor": p.actor,
            "authored": p.authored,
        }),
    );
    m.insert("created_s".into(), json!(secs(v.created_at)));
    m.insert("updated_s".into(), json!(secs(v.updated_at)));
    m.insert("share".into(), share_json(v));
    out
}

fn read(state: &ApiState, action: Action, q: &[(String, String)]) -> Result<Value, Fail> {
    match action {
        Action::List => {
            let parts = [
                query_f64(q, "f_lo")?,
                query_f64(q, "f_hi")?,
                query_f64(q, "t0")?,
                query_f64(q, "t1")?,
            ];
            let window = match parts {
                [None, None, None, None] => None,
                [Some(f_lo), Some(f_hi), Some(t0), Some(t1)] => {
                    if f_hi < f_lo || t1 < t0 {
                        return Err(Fail::invalid("the window needs f_lo <= f_hi and t0 <= t1"));
                    }
                    Some((f_lo, f_hi, t0, t1))
                }
                _ => {
                    return Err(Fail::invalid(
                        "a window is f_lo, f_hi, t0 and t1 together, or none of them",
                    ));
                }
            };
            let limit = match query(q, "limit") {
                None => DEFAULT_VIEWS_LIMIT,
                Some(v) => v
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (1..=MAX_VIEWS_LIMIT).contains(n))
                    .ok_or_else(|| {
                        Fail::invalid(format!("limit must be an integer in 1..={MAX_VIEWS_LIMIT}"))
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
            let filter = SavedViewFilter {
                window: match window {
                    Some((lo, hi, t0, t1)) => {
                        Some((lo, hi, from_secs("t0", t0)?, from_secs("t1", t1)?))
                    }
                    None => None,
                },
            };
            let page = store(state)?
                .saved_views_in(&filter, offset, limit)
                .map_err(repo_fail)?;
            let next = offset + page.rows.len();
            Ok(json!({
                "window": window.map(|(lo, hi, t0, t1)| json!({
                    "f_lo_hz": lo, "f_hi_hz": hi, "t0_s": t0, "t1_s": t1,
                })),
                "views": page.rows.iter().map(view_json).collect::<Vec<_>>(),
                "count": page.rows.len(),
                "matched": page.matched,
                "limit": limit,
                "next_cursor": ((next as u64) < page.matched && next <= MAX_CURSOR)
                    .then(|| next.to_string()),
            }))
        }
        Action::Get(id) => store(state)?
            .saved_view(id)
            .map(|v| view_json(&v))
            .map_err(repo_fail),
        _ => Err(Fail::new(500, "failed", "not a read")),
    }
}

const CREATE_FIELDS: &[&str] = &[
    "id",
    "name",
    "note",
    "center_f_hz",
    "span_f_hz",
    "center_t_s",
    "span_t_s",
    "follow_live",
    "pane_layout",
    "view",
];
const UPDATE_FIELDS: &[&str] = &[
    "name",
    "note",
    "center_f_hz",
    "span_f_hz",
    "center_t_s",
    "span_t_s",
    "follow_live",
    "pane_layout",
    "view",
];

/// Refuses a server-owned field by name before the generic unknown-field check, so the error says
/// why rather than just "unknown" (docs/25 §10.2).
fn refuse_server_owned(body: &Map<String, Value>) -> Result<(), Fail> {
    if body.contains_key("share") {
        return Err(Fail::invalid(
            "share is derived by the server; POST the share object's fields themselves",
        ));
    }
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

fn follow_live(body: &Map<String, Value>) -> Result<Option<bool>, Fail> {
    match body.get("follow_live") {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(Fail::invalid("follow_live must be true or false")),
    }
}

fn pane_layout(body: &Map<String, Value>) -> Result<Option<Option<Value>>, Fail> {
    match body.get("pane_layout") {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(v @ (Value::Object(_) | Value::Array(_))) => Ok(Some(Some(v.clone()))),
        Some(_) => Err(Fail::invalid(
            "pane_layout must be a JSON object, an array or null",
        )),
    }
}

fn center_t(body: &Map<String, Value>) -> Result<Option<Option<Timestamp>>, Fail> {
    match nullable_number(body, "center_t_s")? {
        None => Ok(None),
        Some(None) => Ok(Some(None)),
        Some(Some(s)) => Ok(Some(Some(from_secs("center_t_s", s)?))),
    }
}

fn name(body: &Map<String, Value>) -> Result<Option<String>, Fail> {
    match text(body, "name")? {
        None => Ok(None),
        Some(None) => Err(Fail::invalid("name may not be null")),
        Some(Some(n)) => Ok(Some(n.trim().to_owned())),
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
            refuse_server_owned(body)?;
            only(body, CREATE_FIELDS)?;
            let view = body.get("view").ok_or_else(|| {
                Fail::invalid("view is required: the view context the view was saved from")
            })?;
            let provenance = stamp(state, view, actor)?;
            let id = match text(body, "id")?.flatten() {
                Some(id) => id.parse().map_err(|_| Fail::invalid("id must be a UUID"))?,
                None => SavedViewId::new(),
            };
            let now = Timestamp::now();
            let v = SavedView {
                id,
                name: name(body)?.ok_or_else(|| Fail::invalid("name is required"))?,
                note: text(body, "note")?.flatten().map(str::to_owned),
                center_f_hz: required(body, "center_f_hz")?,
                span_f_hz: required(body, "span_f_hz")?,
                center_t: center_t(body)?.flatten(),
                span_t_s: nullable_number(body, "span_t_s")?.flatten(),
                follow_live: follow_live(body)?.ok_or_else(|| {
                    Fail::invalid(
                        "follow_live is required: true pins the view to the live edge, false \
                         freezes it on center_t_s ± span_t_s/2",
                    )
                })?,
                pane_layout: pane_layout(body)?.flatten(),
                provenance,
                created_at: now,
                updated_at: now,
            };
            v.validate().map_err(repo_fail)?;
            let mut repo = store(state)?;
            if repo.saved_view(v.id).is_ok() {
                return Err(Fail::new(
                    409,
                    "conflict",
                    format!("saved view {} already exists; update it instead", v.id),
                ));
            }
            repo.insert_saved_view(&v).map_err(repo_fail)?;
            let new = view_json(&v);
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
            let stored = repo.saved_view(id).map_err(repo_fail)?;
            let mut next = stored.clone();
            if let Some(n) = name(body)? {
                next.name = n;
            }
            if let Some(note) = text(body, "note")? {
                next.note = note.map(str::to_owned);
            }
            if body.contains_key("center_f_hz") {
                next.center_f_hz = required(body, "center_f_hz")?;
            }
            if body.contains_key("span_f_hz") {
                next.span_f_hz = required(body, "span_f_hz")?;
            }
            if let Some(t) = center_t(body)? {
                next.center_t = t;
            }
            if let Some(s) = nullable_number(body, "span_t_s")? {
                next.span_t_s = s;
            }
            if let Some(f) = follow_live(body)? {
                next.follow_live = f;
            }
            if let Some(l) = pane_layout(body)? {
                next.pane_layout = l;
            }
            // A new view re-stamps provenance (the edit was made from there, by this actor); no
            // view keeps the original stamp.
            if let Some(view) = body.get("view") {
                next.provenance = stamp(state, view, actor)?;
            }
            next.updated_at = Timestamp::now().max(stored.created_at);
            let saved = repo.update_saved_view(&next).map_err(repo_fail)?;
            let new = view_json(&saved);
            Ok(ok(new.clone(), view_json(&stored), new))
        }
        Action::Delete(id) => {
            no_fields(body)?;
            let deleted = store(state)?.delete_saved_view(id).map_err(repo_fail)?;
            let old = view_json(&deleted);
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
        let id = SavedViewId::new();
        assert_eq!(resolve("GET", "/api/views"), Some(Ok(Action::List)));
        assert_eq!(resolve("POST", "/api/views"), Some(Ok(Action::Create)));
        assert_eq!(resolve("PUT", "/api/views"), Some(Err(Some("GET, POST"))));
        assert_eq!(
            resolve("DELETE", &format!("/api/views/{id}")),
            Some(Ok(Action::Delete(id)))
        );
        assert_eq!(
            resolve("POST", &format!("/api/views/{id}")),
            Some(Err(Some("GET, PUT, DELETE")))
        );
        assert_eq!(resolve("GET", "/api/views/nope"), Some(Err(None)));
        assert_eq!(resolve("GET", "/api/viewsx"), None);
        assert_eq!(resolve("GET", "/api/measurements"), None);
    }

    #[test]
    fn every_view_route_is_in_the_route_table() {
        let listed: Vec<_> = ROUTES
            .iter()
            .filter(|(_, p)| *p == "/api/views" || p.starts_with("/api/views/"))
            .collect();
        assert_eq!(listed.len(), 5);
        for (method, path) in listed {
            let concrete = path.replace("{id}", &SavedViewId::new().to_string());
            assert!(
                matches!(resolve(method, &concrete), Some(Ok(_))),
                "{method} {path}"
            );
        }
    }

    #[test]
    fn provenance_and_share_are_refused_by_name() {
        for (body, needle) in [
            (json!({ "provenance": {} }), "provenance"),
            (json!({ "created_s": 1.0 }), "created_s"),
            (json!({ "share": {} }), "share"),
            (
                json!({ "view": { "center_hz": 1.0, "actor": "me" } }),
                "actor",
            ),
        ] {
            let m: Map<String, Value> = serde_json::from_value(body).unwrap();
            let e = refuse_server_owned(&m).unwrap_err().response();
            assert_eq!(e.status, 400);
            let msg = e.body["error"].as_str().unwrap_or_default().to_owned();
            assert!(msg.contains(needle), "{needle}: {msg}");
        }
    }
}
