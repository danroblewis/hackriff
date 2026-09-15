//! Inventory lifecycle over HTTP (T-078): read one entry, promote a candidate, delete an entry.
//! The same token, audit and `{error, code}` rules as bookmarks and selections
//! ([`crate::control`]). The list itself is `/api/inventory` ([`crate::query::inventory_json`],
//! filter `state=candidate,confirmed,deleted`).
//!
//! # Endpoints
//!
//! | Method | Path | Body | Answers |
//! |---|---|---|---|
//! | GET | `/api/inventory/<id>` | – | the entry (as in `/api/inventory`, deleted entries included) |
//! | POST | `/api/inventory/<id>/promote` | `{"reason"?}` | `{"changed", "entry"}`: candidate → confirmed; `changed: false` when already confirmed |
//! | DELETE | `/api/inventory/<id>` | `{"reason"?}` | `{"deleted": entry}` |
//!
//! Errors: 404 `not_found` (unknown id, or an entry already deleted), 400 `invalid` (unknown
//! body field, bad reason), 503 `unavailable` (no inventory or no audit log).
//!
//! **Delete semantics.** The entry leaves the inventory list; its row, detections, tracks, links
//! and history stay (listed with `state=deleted`). Deletion does not suppress the signal: a later
//! sighting of it creates a new candidate (`hk_model` lifecycle rules). There is no undelete.
//!
//! Mutating calls need `Authorization: Bearer` (enforced by [`crate::http`] for every mutating
//! `/api/` request) and are audited as `inventory_promote` and `inventory_delete`, with the old
//! and new state. The lifecycle history records author `user` and the token fingerprint as actor.

use std::sync::{MutexGuard, PoisonError};

use hk_model::{
    EmitterId, IdentityAccess, LIFECYCLE_TEXT_MAX, LifecycleAuthor, LifecycleState, RepoError,
    Repository, Timestamp,
};
use serde_json::{Map, Value, json};

use crate::control::{
    Applied, CtlRequest, CtlResponse, Fail, dispatch, ok, only, refuse_route, text,
};
use crate::http::ApiState;
use crate::query::inventory_entry_json;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Action {
    Get(EmitterId),
    Promote(EmitterId),
    Delete(EmitterId),
}

impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::Get(_) => "inventory_get",
            Self::Promote(_) => "inventory_promote",
            Self::Delete(_) => "inventory_delete",
        }
    }

    fn mutating(self) -> bool {
        !matches!(self, Self::Get(_))
    }
}

/// Resolves `(method, path)`: `None` when `path` is not an inventory entry path.
fn resolve(method: &str, path: &str) -> Option<Result<Action, Option<&'static str>>> {
    let rest = path.strip_prefix("/api/inventory/")?;
    let (id, promote) = match rest.strip_suffix("/promote") {
        Some(id) => (id, true),
        None => (rest, false),
    };
    let Ok(id) = id.parse::<EmitterId>() else {
        return Some(Err(None));
    };
    Some(match (promote, method) {
        (true, "POST") => Ok(Action::Promote(id)),
        (true, _) => Err(Some("POST")),
        (false, "GET") => Ok(Action::Get(id)),
        (false, "DELETE") => Ok(Action::Delete(id)),
        (false, _) => Err(Some("GET, DELETE")),
    })
}

/// Routes an inventory entry request; `None` when `path` is not one.
pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    let action = match resolve(req.method, req.path)? {
        Ok(a) => a,
        Err(allow) => return Some(refuse_route(state, req, allow)),
    };
    let actor = req
        .caller
        .token_id
        .clone()
        .unwrap_or_else(|| "user".to_owned());
    Some(dispatch(
        state,
        req,
        action.name(),
        action.mutating(),
        |s| read(s, action),
        |s, body| apply(s, action, &actor, body),
    ))
}

fn store(state: &ApiState) -> Result<MutexGuard<'_, Repository>, Fail> {
    state
        .inventory
        .as_ref()
        .map(|r| r.lock().unwrap_or_else(PoisonError::into_inner))
        .ok_or_else(|| Fail::new(503, "unavailable", "no signal inventory on this server"))
}

fn repo_fail(e: RepoError) -> Fail {
    match e {
        RepoError::Invalid(m) => Fail::invalid(m),
        RepoError::NotFound { .. } => Fail::new(404, "not_found", "no such inventory entry"),
        other => Fail::new(500, "failed", format!("inventory store: {other}")),
    }
}

/// The live entry `id` stands for, as the API serves it, and its state.
fn entry(repo: &Repository, id: EmitterId) -> Result<(EmitterId, Value, LifecycleState), Fail> {
    let live = repo.live_emitter_id(id).map_err(repo_fail)?;
    let e = repo
        .emitter_with_access(live, IdentityAccess::Standard)
        .map_err(repo_fail)?;
    let row = inventory_entry_json(repo, &e).map_err(repo_fail)?;
    Ok((live, row, e.lifecycle))
}

fn read(state: &ApiState, action: Action) -> Result<Value, Fail> {
    match action {
        Action::Get(id) => {
            let repo = store(state)?;
            entry(&repo, id).map(|(_, row, _)| row)
        }
        _ => Err(Fail::new(500, "failed", "not a read")),
    }
}

/// Optional `reason`: a non-empty string of at most [`LIFECYCLE_TEXT_MAX`] bytes.
fn reason<'a>(body: &'a Map<String, Value>, default: &'a str) -> Result<&'a str, Fail> {
    only(body, &["reason"])?;
    match text(body, "reason")?.flatten().map(str::trim) {
        None => Ok(default),
        Some(r) if r.is_empty() || r.len() > LIFECYCLE_TEXT_MAX => Err(Fail::invalid(format!(
            "reason must be a non-empty string of at most {LIFECYCLE_TEXT_MAX} bytes"
        ))),
        Some(r) => Ok(r),
    }
}

fn apply(
    state: &ApiState,
    action: Action,
    actor: &str,
    body: &Map<String, Value>,
) -> Result<Applied, Fail> {
    let (id, to, why) = match action {
        Action::Promote(id) => (id, LifecycleState::Confirmed, "promoted by user"),
        Action::Delete(id) => (id, LifecycleState::Deleted, "deleted by user"),
        Action::Get(_) => return Err(Fail::new(500, "failed", "not a mutating action")),
    };
    let why = reason(body, why)?;
    let mut repo = store(state)?;
    let (live, _, before) = entry(&repo, id)?;
    if before == LifecycleState::Deleted {
        return Err(repo_fail(RepoError::NotFound {
            kind: "emitter",
            id: id.to_string(),
        }));
    }
    let change = repo
        .change_emitter_lifecycle(
            live,
            to,
            LifecycleAuthor::User,
            actor,
            why,
            Timestamp::now(),
        )
        .map_err(repo_fail)?;
    let (_, row, after) = entry(&repo, live)?;
    let old = json!({ "id": live.to_string(), "state": before });
    let new = json!({ "id": live.to_string(), "state": after, "changed": change.is_some() });
    Ok(match action {
        Action::Delete(_) => ok(json!({ "deleted": row }), old, new),
        _ => ok(
            json!({ "changed": change.is_some(), "entry": row }),
            old,
            new,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::ROUTES;

    #[test]
    fn routes_resolve_with_methods() {
        let id = EmitterId::new();
        assert_eq!(
            resolve("POST", &format!("/api/inventory/{id}/promote")),
            Some(Ok(Action::Promote(id)))
        );
        assert_eq!(
            resolve("GET", &format!("/api/inventory/{id}/promote")),
            Some(Err(Some("POST")))
        );
        assert_eq!(
            resolve("DELETE", &format!("/api/inventory/{id}")),
            Some(Ok(Action::Delete(id)))
        );
        assert_eq!(
            resolve("PUT", &format!("/api/inventory/{id}")),
            Some(Err(Some("GET, DELETE")))
        );
        assert_eq!(resolve("DELETE", "/api/inventory/nope"), Some(Err(None)));
        assert_eq!(resolve("GET", "/api/inventory"), None);
        let listed: Vec<_> = ROUTES
            .iter()
            .filter(|(_, p)| p.starts_with("/api/inventory/"))
            .collect();
        assert_eq!(listed.len(), 3);
        for (method, path) in listed {
            let concrete = path.replace("{id}", &EmitterId::new().to_string());
            assert!(
                matches!(resolve(method, &concrete), Some(Ok(_))),
                "{method} {path}"
            );
        }
    }
}
