//! C18 signature matches over HTTP (T-201, ADR-0016 §5/§9):
//! `GET /api/signatures/match?emitter=<id>`.
//!
//! # Endpoint
//!
//! | Method | Path | Answers |
//! |---|---|---|
//! | GET | `/api/signatures/match?emitter=<id>` | `{"emitter", "match": SignatureMatch \| null, "history": [...]}` |
//!
//! `match` is the emitter's current match (the most recently appended one) and `history` is the
//! append-only log behind it, newest first. Both are `null`/empty for an emitter the catalogue has
//! never had anything to say about — which is the common case and is *not* an error.
//!
//! # What this route is not
//!
//! **It never identifies anything.** A [`hk_model::SignatureMatch`] is ranked, reasoned evidence
//! with its arithmetic disclosed (per-field `z`, what is missing, what conflicts); it sets no
//! identity, status, lifecycle or family, and a client must present it as a suggestion beside the
//! measurement, never in place of it. `outcome: "none"` means the catalogue has nothing to say,
//! **not** that the emission is unknown.
//!
//! # Gating
//!
//! Like [`crate::decode`], and for the same reason (T-036): a match listing must not become a way
//! to confirm a withheld identity. An emitter whose identity is withheld from
//! [`IdentityAccess::Standard`] answers exactly as one with no matches — no flag, no count, no
//! "withheld" marker, since any of those would themselves confirm that an identity is held. An
//! emitter with *no* decoded identity is served normally: there is nothing to confirm.

use hk_model::{EmitterId, IdentityAccess, InventoryIdentity, RepoError, Repository};
use serde_json::{Value, json};
use std::sync::{MutexGuard, PoisonError};

use crate::control::{CtlRequest, CtlResponse, Fail, dispatch, refuse_route};
use crate::http::ApiState;

/// Most matches `history` returns.
pub const MATCH_HISTORY_MAX: u32 = 50;

/// Resolves `(method, path)`; `None` when `path` is not the signature-match route.
fn resolve(method: &str, path: &str) -> Option<Result<(), Option<&'static str>>> {
    if path != "/api/signatures/match" {
        return None;
    }
    Some(match method {
        "GET" => Ok(()),
        _ => Err(Some("GET")),
    })
}

/// Routes a signature-match request; `None` when `path` is not one.
pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    if let Err(allow) = resolve(req.method, req.path)? {
        return Some(refuse_route(state, req, allow));
    }
    let emitter = req
        .query
        .iter()
        .find(|(k, _)| k == "emitter")
        .map(|(_, v)| v.clone());
    Some(dispatch(
        state,
        req,
        "signature_match",
        false,
        |s| read(s, emitter.as_deref()),
        |_, _| Err(Fail::new(500, "failed", "not a mutating action")),
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

/// An emitter with no matches to show. Identical whether the emitter genuinely has none or its
/// identity is withheld, so the answer cannot be used as an oracle.
fn empty(id: EmitterId) -> Value {
    json!({ "emitter": id.to_string(), "match": null, "history": [] })
}

fn read(state: &ApiState, emitter: Option<&str>) -> Result<Value, Fail> {
    let Some(raw) = emitter else {
        return Err(Fail::invalid("emitter is required"));
    };
    let id: EmitterId = raw
        .parse()
        .map_err(|_| Fail::new(404, "not_found", "no such inventory entry"))?;

    let repo = store(state)?;
    let live = repo.live_emitter_id(id).map_err(repo_fail)?;
    let entry = repo
        .emitter_with_access(live, IdentityAccess::Standard)
        .map_err(repo_fail)?;
    if matches!(entry.identity, InventoryIdentity::Withheld { .. }) {
        // Never confirm a withheld identity by listing what its emission resembles.
        return Ok(empty(live));
    }

    let history = repo
        .signature_matches(live, MATCH_HISTORY_MAX)
        .map_err(repo_fail)?;
    let current = history.first().cloned();
    Ok(json!({
        "emitter": live.to_string(),
        "match": current.map(|m| serde_json::to_value(m).unwrap_or(Value::Null)),
        "history": history
            .into_iter()
            .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
            .collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::ROUTES;

    #[test]
    fn the_route_resolves_with_its_methods_and_is_in_the_route_table() {
        assert_eq!(resolve("GET", "/api/signatures/match"), Some(Ok(())));
        assert_eq!(
            resolve("POST", "/api/signatures/match"),
            Some(Err(Some("GET")))
        );
        assert_eq!(resolve("GET", "/api/signatures"), None);
        assert_eq!(resolve("GET", "/api/inventory"), None);
        assert!(
            ROUTES
                .iter()
                .any(|(m, p)| *m == "GET" && *p == "/api/signatures/match")
        );
    }

    #[test]
    fn an_emitter_with_nothing_to_show_answers_the_same_shape_either_way() {
        let id = EmitterId::new();
        let v = empty(id);
        assert_eq!(v["emitter"], id.to_string());
        assert!(v["match"].is_null());
        assert_eq!(v["history"].as_array().unwrap().len(), 0);
        // No marker distinguishes "withheld" from "none": that is the point.
        let text = v.to_string();
        assert!(
            !text.contains("withheld") && !text.contains("identity"),
            "{text}"
        );
    }
}
