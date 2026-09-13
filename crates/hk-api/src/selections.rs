//! Persisted region selections (T-052, docs/07 §2.20) over HTTP: the same token, audit and
//! validation rules as bookmarks ([`crate::control`]). Several selections exist at once and are
//! stored in the server's user-metadata database ([`crate::ApiState::bookmarks`], the run's
//! database), so they survive a restart.
//!
//! # Endpoints
//! Bodies are JSON objects; unknown fields are refused (400). Frequencies are Hz, times Unix
//! seconds (floats). Errors answer `{"error", "code"}` with `invalid`, `not_found`, `conflict`,
//! `unavailable`.
//!
//! | Method | Path | Body | Answers |
//! |---|---|---|---|
//! | GET | `/api/selections` | – | `{"selections": [...]}` in creation order |
//! | POST | `/api/selections` | `{"name", "f_lo", "f_hi", "id"?, "t_lo"?, "t_hi"?, "notes"?, "tags"?}` | the created selection (201); 409 `conflict` when `id` exists |
//! | GET, PUT, DELETE | `/api/selections/<id>` | update: any create field but `id` (`null` clears `t_lo`/`t_hi`/`notes`/`tags`) | the selection / `{"deleted": ...}` |
//! | POST | `/api/selections/<id>/links` | `{"kind", "target", "note"?}` | the updated selection (201) |
//!
//! A client may choose the `id` (any UUID) so an optimistic or offline-created selection keeps its
//! identity when it syncs; a retried create answers 409 and the client updates instead.
//!
//! A selection is JSON: `id`, `name`, `f_lo`, `f_hi`, `t_lo`, `t_hi` (null for any time), `notes`,
//! `tags`, `links` (`[{"kind": "demodulation" | "recording" | "bitstream" | "inspection",
//! "target", "t", "note"}]`, oldest first, at most [`hk_model::SELECTION_LINKS_MAX`]), `created`,
//! `updated`.
//!
//! Mutating calls need `Authorization: Bearer` (enforced by [`crate::http`] for every mutating
//! `/api/` request) and are audited as `selection_create`, `selection_update`,
//! `selection_delete`, `selection_link`.

use std::sync::{MutexGuard, PoisonError};

use hk_model::{
    RepoError, Repository, Selection, SelectionId, SelectionLink, SelectionLinkKind, Timestamp,
};
use serde_json::{Map, Value, json};

use crate::control::{
    Applied, CtlRequest, CtlResponse, Fail, dispatch, no_fields, nullable_number, number, ok, only,
    refuse_route, required, text,
};
use crate::http::ApiState;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Action {
    List,
    Create,
    Get(SelectionId),
    Update(SelectionId),
    Delete(SelectionId),
    Link(SelectionId),
}

impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::List => "selections_list",
            Self::Create => "selection_create",
            Self::Get(_) => "selection_get",
            Self::Update(_) => "selection_update",
            Self::Delete(_) => "selection_delete",
            Self::Link(_) => "selection_link",
        }
    }

    fn mutating(self) -> bool {
        !matches!(self, Self::List | Self::Get(_))
    }
}

/// Resolves `(method, path)` like `control::resolve`: `None` when `path` is not a selection path.
fn resolve(method: &str, path: &str) -> Option<Result<Action, Option<&'static str>>> {
    if path == "/api/selections" {
        return Some(match method {
            "GET" => Ok(Action::List),
            "POST" => Ok(Action::Create),
            _ => Err(Some("GET, POST")),
        });
    }
    let rest = path.strip_prefix("/api/selections/")?;
    let (id, links) = match rest.strip_suffix("/links") {
        Some(id) => (id, true),
        None => (rest, false),
    };
    let Ok(id) = id.parse::<SelectionId>() else {
        return Some(Err(None));
    };
    Some(match (links, method) {
        (true, "POST") => Ok(Action::Link(id)),
        (true, _) => Err(Some("POST")),
        (false, "GET") => Ok(Action::Get(id)),
        (false, "PUT") => Ok(Action::Update(id)),
        (false, "DELETE") => Ok(Action::Delete(id)),
        (false, _) => Err(Some("GET, PUT, DELETE")),
    })
}

/// Routes a selection request; `None` when `path` is not one.
pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    let action = match resolve(req.method, req.path)? {
        Ok(a) => a,
        Err(allow) => return Some(refuse_route(state, req, allow)),
    };
    Some(dispatch(
        state,
        req,
        action.name(),
        action.mutating(),
        |s| read(s, action),
        |s, body| apply(s, action, body),
    ))
}

fn store(state: &ApiState) -> Result<MutexGuard<'_, Repository>, Fail> {
    state
        .bookmarks
        .as_ref()
        .map(|r| r.lock().unwrap_or_else(PoisonError::into_inner))
        .ok_or_else(|| Fail::new(503, "unavailable", "no selection store on this server"))
}

fn repo_fail(e: RepoError) -> Fail {
    match e {
        RepoError::Invalid(m) => Fail::invalid(m),
        RepoError::NotFound { .. } => Fail::new(404, "not_found", "no such selection"),
        other => Fail::new(500, "failed", format!("selection store: {other}")),
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

fn kind_text(k: SelectionLinkKind) -> Value {
    serde_json::to_value(k).unwrap_or(Value::Null)
}

/// A selection as the API serves it.
pub fn selection_json(s: &Selection) -> Value {
    json!({
        "id": s.id.to_string(),
        "name": s.name,
        "f_lo": s.f_lo_hz,
        "f_hi": s.f_hi_hz,
        "t_lo": s.t_lo.map(secs),
        "t_hi": s.t_hi.map(secs),
        "notes": s.notes,
        "tags": s.tags,
        "links": s.links.iter().map(|l| json!({
            "kind": kind_text(l.kind),
            "target": l.target,
            "t": secs(l.t),
            "note": l.note,
        })).collect::<Vec<_>>(),
        "created": secs(s.created_at),
        "updated": secs(s.updated_at),
    })
}

fn read(state: &ApiState, action: Action) -> Result<Value, Fail> {
    match action {
        Action::List => {
            let all = store(state)?.selections().map_err(repo_fail)?;
            Ok(json!({ "selections": all.iter().map(selection_json).collect::<Vec<_>>() }))
        }
        Action::Get(id) => store(state)?
            .selection(id)
            .map(|s| selection_json(&s))
            .map_err(repo_fail),
        _ => Err(Fail::new(500, "failed", "not a read")),
    }
}

const CREATE_FIELDS: &[&str] = &[
    "id", "name", "f_lo", "f_hi", "t_lo", "t_hi", "notes", "tags",
];
const UPDATE_FIELDS: &[&str] = &["name", "f_lo", "f_hi", "t_lo", "t_hi", "notes", "tags"];

fn name(v: Option<&str>) -> Result<String, Fail> {
    let n = v.unwrap_or("").trim();
    if n.is_empty() {
        return Err(Fail::invalid("name must be a non-empty string"));
    }
    Ok(n.to_owned())
}

/// `tags`: absent → `None`; `null` → no tags; an array of strings, trimmed, duplicates dropped.
fn tags(body: &Map<String, Value>) -> Result<Option<Vec<String>>, Fail> {
    match body.get("tags") {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(Vec::new())),
        Some(Value::Array(items)) => {
            let mut out: Vec<String> = Vec::with_capacity(items.len());
            for v in items {
                let t = v
                    .as_str()
                    .ok_or_else(|| Fail::invalid("tags must be an array of strings"))?
                    .trim()
                    .to_owned();
                if !out.contains(&t) {
                    out.push(t);
                }
            }
            Ok(Some(out))
        }
        Some(_) => Err(Fail::invalid("tags must be an array of strings or null")),
    }
}

/// Applies the time fields: both numbers, both null (clears), or neither present.
fn apply_times(body: &Map<String, Value>, s: &mut Selection) -> Result<(), Fail> {
    let lo = nullable_number(body, "t_lo")?;
    let hi = nullable_number(body, "t_hi")?;
    match (lo, hi) {
        (None, None) => Ok(()),
        (Some(None), Some(None)) => {
            s.t_lo = None;
            s.t_hi = None;
            Ok(())
        }
        (Some(Some(lo)), Some(Some(hi))) => {
            s.t_lo = Some(from_secs("t_lo", lo)?);
            s.t_hi = Some(from_secs("t_hi", hi)?);
            Ok(())
        }
        _ => Err(Fail::invalid(
            "give both t_lo and t_hi (numbers, or null to clear), or neither",
        )),
    }
}

fn apply_common(body: &Map<String, Value>, s: &mut Selection) -> Result<(), Fail> {
    if let Some(n) = text(body, "name")? {
        s.name = name(n)?;
    }
    if let Some(f) = number(body, "f_lo")? {
        s.f_lo_hz = f;
    }
    if let Some(f) = number(body, "f_hi")? {
        s.f_hi_hz = f;
    }
    apply_times(body, s)?;
    if let Some(notes) = text(body, "notes")? {
        s.notes = notes.map(str::to_owned);
    }
    if let Some(t) = tags(body)? {
        s.tags = t;
    }
    Ok(())
}

fn link_kind(v: Option<&str>) -> Result<SelectionLinkKind, Fail> {
    serde_json::from_value(json!(v.unwrap_or(""))).map_err(|_| {
        Fail::invalid(
            "kind must be \"demodulation\", \"recording\", \"bitstream\" or \"inspection\"",
        )
    })
}

fn apply(state: &ApiState, action: Action, body: &Map<String, Value>) -> Result<Applied, Fail> {
    match action {
        Action::Create => {
            only(body, CREATE_FIELDS)?;
            let mut s = Selection::new(
                name(text(body, "name")?.flatten())?,
                required(body, "f_lo")?,
                required(body, "f_hi")?,
            );
            if let Some(id) = text(body, "id")?.flatten() {
                s.id = id.parse().map_err(|_| Fail::invalid("id must be a UUID"))?;
            }
            apply_common(body, &mut s)?;
            let mut repo = store(state)?;
            s.validate().map_err(repo_fail)?;
            if repo.selection(s.id).is_ok() {
                return Err(Fail::new(
                    409,
                    "conflict",
                    format!("selection {} already exists; update it instead", s.id),
                ));
            }
            repo.insert_selection(&s).map_err(repo_fail)?;
            let new = selection_json(&s);
            Ok(Applied {
                status: 201,
                body: new.clone(),
                old: Value::Null,
                new,
            })
        }
        Action::Update(id) => {
            only(body, UPDATE_FIELDS)?;
            let mut repo = store(state)?;
            let stored = repo.selection(id).map_err(repo_fail)?;
            let mut next = stored.clone();
            apply_common(body, &mut next)?;
            next.updated_at = Timestamp::now().max(stored.created_at);
            let saved = repo.update_selection(&next).map_err(repo_fail)?;
            let new = selection_json(&saved);
            Ok(ok(new.clone(), selection_json(&stored), new))
        }
        Action::Delete(id) => {
            no_fields(body)?;
            let deleted = store(state)?.delete_selection(id).map_err(repo_fail)?;
            let old = selection_json(&deleted);
            Ok(ok(json!({ "deleted": old }), old, Value::Null))
        }
        Action::Link(id) => {
            only(body, &["kind", "target", "note"])?;
            let mut link = SelectionLink::new(
                link_kind(text(body, "kind")?.flatten())?,
                text(body, "target")?.flatten().unwrap_or("").trim(),
            );
            link.note = text(body, "note")?
                .flatten()
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_owned);
            let saved = store(state)?
                .add_selection_link(id, link.clone())
                .map_err(repo_fail)?;
            let new = selection_json(&saved);
            Ok(Applied {
                status: 201,
                body: new,
                old: Value::Null,
                new: json!({ "kind": kind_text(link.kind), "target": link.target, "note": link.note }),
            })
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
        let id = SelectionId::new();
        assert_eq!(resolve("GET", "/api/selections"), Some(Ok(Action::List)));
        assert_eq!(resolve("POST", "/api/selections"), Some(Ok(Action::Create)));
        assert_eq!(
            resolve("PATCH", "/api/selections"),
            Some(Err(Some("GET, POST")))
        );
        assert_eq!(
            resolve("PUT", &format!("/api/selections/{id}")),
            Some(Ok(Action::Update(id)))
        );
        assert_eq!(
            resolve("POST", &format!("/api/selections/{id}")),
            Some(Err(Some("GET, PUT, DELETE")))
        );
        assert_eq!(
            resolve("POST", &format!("/api/selections/{id}/links")),
            Some(Ok(Action::Link(id)))
        );
        assert_eq!(
            resolve("GET", &format!("/api/selections/{id}/links")),
            Some(Err(Some("POST")))
        );
        assert_eq!(resolve("GET", "/api/selections/nope"), Some(Err(None)));
        assert_eq!(resolve("GET", "/api/selectionsx"), None);
        assert_eq!(resolve("GET", "/api/bookmarks"), None);
    }

    #[test]
    fn every_selection_route_is_in_the_route_table() {
        let listed: Vec<_> = ROUTES
            .iter()
            .filter(|(_, p)| p.starts_with("/api/selections"))
            .collect();
        assert_eq!(listed.len(), 6);
        for (method, path) in listed {
            let concrete = path.replace("{id}", &SelectionId::new().to_string());
            assert!(
                matches!(resolve(method, &concrete), Some(Ok(_))),
                "{method} {path}"
            );
        }
    }
}
