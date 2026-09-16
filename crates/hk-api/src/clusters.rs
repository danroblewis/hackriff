//! C18 clusters of unknown emissions over HTTP (T-202, ADR-0016 §5/§9).
//!
//! # Endpoints
//!
//! | Method | Path | Answers |
//! |---|---|---|
//! | GET | `/api/clusters` | `{"clusters": [Cluster, ...]}` — the visible ones, oldest first |
//! | GET | `/api/clusters/{id}` | one `Cluster` with its members and history |
//! | POST | `/api/clusters/{id}/promote` | `{"signature": {...}, "cluster": Cluster}` |
//!
//! `{id}` may be written `cluster:0199…` or just `0199…`: the prefix is optional in the path so a
//! client never has to think about encoding the colon.
//!
//! # What a cluster is here
//!
//! **Evidence, never identity.** A cluster groups emitters that *measure* alike — a *type* above
//! instances — and changes nothing about any of them: not identity, not family, not
//! `known_status`, not lifecycle. Two identical sensors share a cluster and stay two inventory
//! rows. Promotion mints a [`hk_model::Signature`] the matcher then scores like any other
//! catalogue entry; it still names nothing.
//!
//! Only *visible* clusters (at least three members, or one emitter seen in at least three
//! separated appearances) are served. A pending group is a guess, and a guess with an id in the UI
//! would read as a finding.
//!
//! # Gating
//!
//! Like [`crate::decode`] and [`crate::signatures`], and for the same reason (T-036): a member
//! list must not become a way to confirm a withheld identity. Members whose identity is withheld
//! from [`IdentityAccess::Standard`] are left out of `member_ids` **and** out of `members`, so the
//! answer carries no flag, no count and no marker that such a member exists.

use hk_model::{
    FeatValue, IdentityAccess, InventoryIdentity, RepoError, Repository, SignatureCluster,
    Timestamp, is_cluster_id,
};
use serde_json::{Map, Value, json};
use std::sync::{MutexGuard, PoisonError};

use crate::control::{Applied, CtlRequest, CtlResponse, Fail, dispatch, ok, refuse_route};
use crate::http::ApiState;

/// Most history rows `GET /api/clusters/{id}` returns.
pub const CLUSTER_EVENTS_MAX: u32 = 50;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Action {
    List,
    Get(String),
    Promote(String),
}

impl Action {
    fn name(&self) -> &'static str {
        match self {
            Self::List => "clusters",
            Self::Get(_) => "cluster",
            Self::Promote(_) => "cluster_promote",
        }
    }

    fn mutating(&self) -> bool {
        matches!(self, Self::Promote(_))
    }
}

/// Accepts `cluster:0199…` or the bare tail, so a path never has to carry an encoded colon.
fn normalise_id(raw: &str) -> Option<String> {
    let decoded = raw.replace("%3A", ":").replace("%3a", ":");
    let id = if decoded.starts_with("cluster:") {
        decoded
    } else {
        format!("cluster:{decoded}")
    };
    is_cluster_id(&id).then_some(id)
}

/// Resolves `(method, path)`; `None` when `path` is not a cluster route.
fn resolve(method: &str, path: &str) -> Option<Result<Action, Option<&'static str>>> {
    let rest = path.strip_prefix("/api/clusters")?;
    if rest.is_empty() {
        return Some(match method {
            "GET" => Ok(Action::List),
            _ => Err(Some("GET")),
        });
    }
    let rest = rest.strip_prefix('/')?;
    let (raw, promote) = match rest.strip_suffix("/promote") {
        Some(id) => (id, true),
        None => (rest, false),
    };
    let Some(id) = normalise_id(raw) else {
        return Some(Err(None));
    };
    Some(match (promote, method) {
        (false, "GET") => Ok(Action::Get(id)),
        (false, _) => Err(Some("GET")),
        (true, "POST") => Ok(Action::Promote(id)),
        (true, _) => Err(Some("POST")),
    })
}

/// Routes a cluster request; `None` when `path` is not one.
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
    let for_read = action.clone();
    Some(dispatch(
        state,
        req,
        action.name(),
        action.mutating(),
        |s| read(s, &for_read),
        |s, body| apply(s, &action, &actor, body),
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
        RepoError::NotFound { .. } => Fail::new(404, "not_found", "no such cluster"),
        other => Fail::new(500, "failed", format!("inventory store: {other}")),
    }
}

fn not_found() -> Fail {
    Fail::new(404, "not_found", "no such cluster")
}

fn ts_s(t: Timestamp) -> f64 {
    t.as_unix_nanos() as f64 / 1e9
}

/// The members this access level may see. A withheld-identity member is simply absent: no count,
/// no placeholder, nothing that would confirm one exists.
fn visible_members(repo: &Repository, cluster_id: &str) -> Result<Vec<String>, Fail> {
    let mut out = Vec::new();
    for id in repo.cluster_members(cluster_id).map_err(repo_fail)? {
        let entry = repo
            .emitter_with_access(id, IdentityAccess::Standard)
            .map_err(repo_fail)?;
        if !matches!(entry.identity, InventoryIdentity::Withheld { .. }) {
            out.push(id.to_string());
        }
    }
    Ok(out)
}

/// One centroid field, with the uncertainty it carries: the arithmetic is disclosed, as everywhere
/// else a verdict is offered.
fn centroid_json(c: &SignatureCluster) -> Vec<Value> {
    c.centroid
        .fields
        .iter()
        .map(|(name, f)| {
            let (kind, value) = match &f.value {
                FeatValue::Num { value } => ("num", json!(value)),
                FeatValue::Bits { bits } => ("bits", json!(bits)),
                FeatValue::Text { text } => ("text", json!(text)),
            };
            json!({
                "field": name,
                "kind": kind,
                "value": value,
                "sigma": f.sigma,
                "spread": f.spread,
                "agreement": f.agreement,
                "n": f.n,
                "method": f.method,
            })
        })
        .collect()
}

fn cluster_json(repo: &Repository, c: &SignatureCluster, history: bool) -> Result<Value, Fail> {
    let members = visible_members(repo, &c.id)?;
    let events = if history {
        repo.cluster_events(&c.id, CLUSTER_EVENTS_MAX)
            .map_err(repo_fail)?
            .into_iter()
            .map(|e| {
                json!({
                    "kind": e.kind.as_str(),
                    "other_cluster_id": e.other_cluster_id,
                    "t_s": ts_s(e.t),
                    "detail": e.detail,
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    Ok(json!({
        "id": c.id,
        "state": c.state.as_str(),
        "members": members.len(),
        "member_ids": members,
        "created_at_s": ts_s(c.created_at),
        "updated_at_s": ts_s(c.updated_at),
        "observations": c.centroid.folds,
        "suspect_fraction": c.centroid.suspect_fraction,
        "feature_set_version": c.feature_set_version,
        "merged_into": c.merged_into,
        "signature": c.signature.as_ref().map(|s| json!({ "id": s.id, "version": s.version })),
        "centroid": centroid_json(c),
        "events": events,
    }))
}

/// The cluster at `id`, following merges, when it is visible.
fn visible_cluster(repo: &Repository, id: &str) -> Result<SignatureCluster, Fail> {
    let live = repo.live_cluster_id(id).map_err(repo_fail)?;
    let c = repo
        .cluster_opt(&live)
        .map_err(repo_fail)?
        .ok_or_else(not_found)?;
    if !c.state.visible() {
        // A pending group is a guess; served as "no such cluster" rather than as a finding.
        return Err(not_found());
    }
    Ok(c)
}

fn read(state: &ApiState, action: &Action) -> Result<Value, Fail> {
    let repo = store(state)?;
    match action {
        Action::List => {
            let mut out = Vec::new();
            for c in repo.visible_clusters().map_err(repo_fail)? {
                out.push(cluster_json(&repo, &c, false)?);
            }
            Ok(json!({ "clusters": out }))
        }
        Action::Get(id) => {
            let c = visible_cluster(&repo, id)?;
            cluster_json(&repo, &c, true)
        }
        Action::Promote(_) => Err(Fail::new(500, "failed", "not a read action")),
    }
}

fn apply(
    state: &ApiState,
    action: &Action,
    actor: &str,
    body: &Map<String, Value>,
) -> Result<Applied, Fail> {
    let Action::Promote(id) = action else {
        return Err(Fail::new(500, "failed", "not a mutating action"));
    };
    if !body.is_empty() {
        return Err(Fail::invalid("this action takes no body fields"));
    }
    let mut repo = store(state)?;
    let before = visible_cluster(&repo, id)?;
    let signature = repo
        .promote_cluster(&before.id, actor, Timestamp::now())
        .map_err(repo_fail)?;
    let after = repo.cluster(&before.id).map_err(repo_fail)?;
    let row = cluster_json(&repo, &after, true)?;
    Ok(ok(
        json!({
            "signature": { "id": signature.id, "version": signature.version, "name": signature.name },
            "cluster": row,
        }),
        json!({ "id": before.id, "state": before.state.as_str() }),
        json!({
            "id": after.id,
            "state": after.state.as_str(),
            "signature": signature.id,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::ROUTES;
    use hk_model::new_cluster_id;

    #[test]
    fn the_routes_resolve_with_their_methods_and_are_in_the_route_table() {
        let id = new_cluster_id();
        let tail = id.trim_start_matches("cluster:").to_owned();
        assert_eq!(resolve("GET", "/api/clusters"), Some(Ok(Action::List)));
        assert_eq!(resolve("POST", "/api/clusters"), Some(Err(Some("GET"))));
        assert_eq!(
            resolve("GET", &format!("/api/clusters/{id}")),
            Some(Ok(Action::Get(id.clone())))
        );
        // The `cluster:` prefix is optional, and an encoded colon is accepted.
        assert_eq!(
            resolve("GET", &format!("/api/clusters/{tail}")),
            Some(Ok(Action::Get(id.clone())))
        );
        assert_eq!(
            resolve("GET", &format!("/api/clusters/cluster%3A{tail}")),
            Some(Ok(Action::Get(id.clone())))
        );
        assert_eq!(
            resolve("POST", &format!("/api/clusters/{id}/promote")),
            Some(Ok(Action::Promote(id.clone())))
        );
        assert_eq!(
            resolve("GET", &format!("/api/clusters/{id}/promote")),
            Some(Err(Some("POST")))
        );
        assert_eq!(
            resolve("GET", "/api/clusters/not a cluster"),
            Some(Err(None))
        );
        assert_eq!(resolve("GET", "/api/clustersx"), None);
        assert_eq!(resolve("GET", "/api/inventory"), None);
        for (method, path) in [
            ("GET", "/api/clusters"),
            ("GET", "/api/clusters/{id}"),
            ("POST", "/api/clusters/{id}/promote"),
        ] {
            assert!(
                ROUTES.iter().any(|(m, p)| *m == method && *p == path),
                "{method} {path} is not in the route table"
            );
        }
    }

    #[test]
    fn only_promote_mutates() {
        assert!(!Action::List.mutating());
        assert!(!Action::Get(new_cluster_id()).mutating());
        assert!(Action::Promote(new_cluster_id()).mutating());
    }
}
