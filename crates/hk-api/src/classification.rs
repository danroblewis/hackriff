//! An emitter's full C15 classification (T-247, ADR-0016 §2/§9):
//! `GET /api/inventory/<id>/classification`.
//!
//! # Endpoint
//!
//! | Method | Path | Answers |
//! |---|---|---|
//! | GET | `/api/inventory/<id>/classification` | `{"emitter", "classification", "latest"}` |
//!
//! `/api/inventory` carries only the summary of a classification (family, confidence, `top`,
//! `coarse`, `flags`). The parts too large for a list row — the full posterior and likelihood
//! distributions, the prior that was fused, the provenance (features and rules versions, SNR
//! against its gate, suspect flags) and the machine reason codes — are served here, per emitter.
//!
//! - **`classification`** is the full [`hk_model::classify::Classification`] of the row that sets
//!   the emitter's family (the arbitration winner: lowest rank, latest among equals), or `null`
//!   when that row was written before M3 or by a pre-M3 writer, which carries a bare family string
//!   and no distribution.
//! - **`latest`** is the full classification of the most recently appended row when that is a
//!   *different* row, else `null` — the same rule `/api/inventory`'s `latest_classification`
//!   follows, and the normal shape for a demodulated emitter: ADR-0016 §2 leaves the family to the
//!   chain that locked (rank 2) while the C15 cascade's posterior is recorded at rank 3 beside it
//!   (`hk_pipeline::classify`). Reading only `classification` there would report `null` for an
//!   emitter the classifier did measure.
//!
//! Both are `null` for an emitter nothing has classified — which is not an error.
//!
//! # What this route is not
//!
//! **A classification never identifies anything.** It is a distribution with an explicit `unknown`
//! outcome and its reasoning disclosed, ranked evidence beside the measurement; only a CRC-valid
//! decode confirms what a signal is (ADR-0016 §4.7). A high `confidence` is not a licence for a
//! client to present the family as fact.
//!
//! # Gating
//!
//! Like [`crate::decode`] and [`crate::signatures`], and for the same reason (T-036): an emitter
//! whose decoded identity is withheld from [`IdentityAccess::Standard`] answers exactly as one
//! nothing has classified — no flag, no count, no marker — so this route can never become a way to
//! confirm a withheld identity. An emitter with no decoded identity is served normally.

use hk_model::{
    EmitterId, IdentityAccess, InventoryIdentity, RecordedClassification, RepoError, Repository,
};
use serde_json::{Value, json};
use std::sync::{MutexGuard, PoisonError};

use crate::control::{CtlRequest, CtlResponse, Fail, dispatch, refuse_route};
use crate::http::ApiState;

/// Resolves `(method, path)`: `None` when `path` is not a `/classification` inventory sub-path.
fn resolve(method: &str, path: &str) -> Option<Result<EmitterId, Option<&'static str>>> {
    let id = path
        .strip_prefix("/api/inventory/")?
        .strip_suffix("/classification")?;
    let Ok(id) = id.parse::<EmitterId>() else {
        return Some(Err(None));
    };
    Some(match method {
        "GET" => Ok(id),
        _ => Err(Some("GET")),
    })
}

/// Routes an emitter classification request; `None` when `path` is not one. Must run before
/// [`crate::inventory::route`] in the dispatch chain, for the reason [`crate::decode`] documents:
/// that module's resolver only special-cases `/promote` and `/band`, so it would otherwise claim
/// an unparsable `/api/inventory/<id>/classification` first.
pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    let id = match resolve(req.method, req.path)? {
        Ok(id) => id,
        Err(allow) => return Some(refuse_route(state, req, allow)),
    };
    Some(dispatch(
        state,
        req,
        "inventory_classification",
        false,
        |s| read(s, id),
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

/// An emitter with no classification to show. Identical whether nothing has classified it or its
/// identity is withheld, so the answer cannot be used as an oracle.
fn empty(id: EmitterId) -> Value {
    json!({ "emitter": id.to_string(), "classification": null, "latest": null })
}

/// The full M3 classification a row carries, or `None` for a pre-M3 row (a bare family string).
fn detail(r: Option<&RecordedClassification>) -> Option<Value> {
    r.and_then(|r| r.detail.as_ref())
        .map(|c| serde_json::to_value(c).unwrap_or(Value::Null))
}

fn read(state: &ApiState, id: EmitterId) -> Result<Value, Fail> {
    let repo = store(state)?;
    let live = repo.live_emitter_id(id).map_err(repo_fail)?;
    let entry = repo
        .emitter_with_access(live, IdentityAccess::Standard)
        .map_err(repo_fail)?;
    if matches!(entry.identity, InventoryIdentity::Withheld { .. }) {
        // Never confirm a withheld identity by describing what its emission measures like.
        return Ok(empty(live));
    }

    let current = repo.current_classification(live).map_err(repo_fail)?;
    let latest = repo.latest_classification(live).map_err(repo_fail)?;
    let differs = matches!((&current, &latest), (Some(c), Some(l)) if c != l);
    Ok(json!({
        "emitter": live.to_string(),
        "classification": detail(current.as_ref()),
        "latest": if differs { detail(latest.as_ref()) } else { None },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_resolve_with_methods() {
        let id = EmitterId::new();
        assert_eq!(
            resolve("GET", &format!("/api/inventory/{id}/classification")),
            Some(Ok(id))
        );
        assert_eq!(
            resolve("POST", &format!("/api/inventory/{id}/classification")),
            Some(Err(Some("GET")))
        );
        // An unparsable id is this route's to refuse (404), not the inventory module's.
        assert_eq!(
            resolve("GET", "/api/inventory/not-an-id/classification"),
            Some(Err(None))
        );
        // Not this route.
        assert_eq!(resolve("GET", &format!("/api/inventory/{id}")), None);
        assert_eq!(resolve("GET", &format!("/api/inventory/{id}/decode")), None);
    }

    #[test]
    fn an_emitter_with_nothing_to_show_is_indistinguishable_from_a_withheld_one() {
        let id = EmitterId::new();
        let v = empty(id);
        assert_eq!(v["classification"], Value::Null);
        assert_eq!(v["latest"], Value::Null);
        // No flag, no count, no marker that an identity is held.
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 3, "{v}");
        for forbidden in ["withheld", "identity", "known_status"] {
            assert!(obj.get(forbidden).is_none(), "{forbidden} leaked: {v}");
        }
    }
}
