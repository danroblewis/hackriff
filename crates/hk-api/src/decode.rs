//! An emitter's latest decode fields (T-159, ADR-0013 API GAP 3): `GET /api/inventory/<id>/decode`.
//!
//! Serves the focus panel and per-signal output panels (docs/14 "Added scope from docs/15 §7"):
//! the emitter's most recent parsed fields, one row per `(decoder_id, frame_model)` — a plugin
//! decoder (`readsb`, `hk-rds`) names one frame model per row; a recipe's several `messages`
//! outputs share one `decoder_id` (`recipe:<id>`) but each names its own `frame_model` (RDS's
//! `rds.recipe.json` has `rds-group`, `rds-ps`, `rds-rt`), so each gets its own row.
//!
//! # Endpoint
//!
//! | Method | Path | Answers |
//! |---|---|---|
//! | GET | `/api/inventory/<id>/decode` | `{"decodes": [...]}`, latest first |
//!
//! Each row: `{decoder, recipe_id, frame_model, at, fields, crc: {valid}, source_session}`.
//! `recipe_id` is the recipe id when `decoder` is `recipe:<id>`, else `null`. `fields` merges the
//! row's metadata and content (content only when the caller's identity access reveals it — off by
//! default, ADR-0004/T-143 content gating). `source_session` is the producing demodulation's id,
//! else the replayed recording's id, else `null` (live plugin/recipe decodes with neither).
//!
//! **Resolution.** Like `/api/inventory/<id>`: a merged id resolves to its live survivor
//! ([`hk_model::Repository::live_emitter_id`]), 404 `not_found` for an unknown or unparsable id.
//! **Never a lookup that confirms a withheld identity** (T-036): an emitter with no decoded
//! identity, or whose identity is withheld from [`IdentityAccess::Standard`] (the access every
//! unauthenticated-beyond-the-token caller gets here, matching every other inventory read),
//! answers `{"decodes": []}` rather than naming the identity through its decodes.
//!
//! **Evidence rule (T-185).** A CRC-invalid group never reaches a recipe `messages` output (the
//! `fields` block's `skip_invalid` drops it before a row is built, and a recipe row is always
//! written `crc_status: Valid`), so `crc.valid` is always `true` here today. T-210 (in progress)
//! adds bounded block error correction with a `corrected` flag on consensus-committed fields; this
//! route has no such flag to serve yet — **TODO(T-210):** once corrected-group provenance lands on
//! `Decode`, add `crc.corrected` here without changing `crc.valid`'s meaning.

use std::collections::BTreeMap;
use std::sync::{MutexGuard, PoisonError};

use hk_model::{
    CrcStatus, Decode, EmitterId, IdentityAccess, InventoryIdentity, RepoError, Repository,
};
use serde_json::{Map, Value, json};

use crate::control::{CtlRequest, CtlResponse, Fail, dispatch, refuse_route};
use crate::http::ApiState;
use crate::query::ts_s;

/// Resolves `(method, path)`: `None` when `path` is not a `/decode` inventory sub-path.
fn resolve(method: &str, path: &str) -> Option<Result<EmitterId, Option<&'static str>>> {
    let id = path
        .strip_prefix("/api/inventory/")?
        .strip_suffix("/decode")?;
    let Ok(id) = id.parse::<EmitterId>() else {
        return Some(Err(None));
    };
    Some(match method {
        "GET" => Ok(id),
        _ => Err(Some("GET")),
    })
}

/// Routes an emitter decode request; `None` when `path` is not one. Must run before
/// [`crate::inventory::route`] in the dispatch chain: that module's own resolver also matches an
/// unparsable `/api/inventory/<id>/decode` path (it only special-cases `/promote` and `/band`) and
/// would otherwise claim it first.
pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    let id = match resolve(req.method, req.path)? {
        Ok(id) => id,
        Err(allow) => return Some(refuse_route(state, req, allow)),
    };
    Some(dispatch(
        state,
        req,
        "inventory_decode",
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

fn read(state: &ApiState, id: EmitterId) -> Result<Value, Fail> {
    let repo = store(state)?;
    let live = repo.live_emitter_id(id).map_err(repo_fail)?;
    let entry = repo
        .emitter_with_access(live, IdentityAccess::Standard)
        .map_err(repo_fail)?;
    let identity = match entry.identity {
        InventoryIdentity::Clear { identity, .. } => identity,
        // No decoded identity, or withheld: never confirm it by naming its decodes.
        InventoryIdentity::None | InventoryIdentity::Withheld { .. } => {
            return Ok(json!({ "decodes": [] }));
        }
    };
    let decodes = repo.decodes_for_identity(&identity).map_err(repo_fail)?;
    Ok(json!({ "decodes": latest_per_frame(decodes) }))
}

/// The latest decode per `(decoder_id, frame_model)`, newest first. `decodes_for_identity`
/// returns rows oldest first, so the last write per key wins.
fn latest_per_frame(decodes: Vec<Decode>) -> Vec<Value> {
    let mut latest: BTreeMap<(String, String), Decode> = BTreeMap::new();
    for d in decodes {
        latest.insert((d.decoder_id.clone(), d.frame_model.clone()), d);
    }
    let mut rows: Vec<Decode> = latest.into_values().collect();
    rows.sort_by(|a, b| {
        b.t.cmp(&a.t)
            .then_with(|| a.frame_model.cmp(&b.frame_model))
    });
    rows.into_iter().map(decode_json).collect()
}

/// `metadata` and `content` merged into one `fields` object (both already gated by
/// `decodes_for_identity`: content is `None` unless the caller's access reveals it).
fn fields_of(d: &Decode) -> Value {
    let mut fields = match &d.metadata {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    };
    if let Some(Value::Object(c)) = &d.content {
        for (k, v) in c {
            fields.insert(k.clone(), v.clone());
        }
    }
    Value::Object(fields)
}

fn decode_json(d: Decode) -> Value {
    let recipe_id = d.decoder_id.strip_prefix("recipe:").map(str::to_owned);
    let fields = fields_of(&d);
    json!({
        "decoder": d.decoder_id,
        "recipe_id": recipe_id,
        "frame_model": d.frame_model,
        "at": ts_s(d.t),
        "fields": fields,
        "crc": { "valid": d.crc_status == CrcStatus::Valid },
        "source_session": d
            .demodulation_ref
            .map(|r| r.to_string())
            .or_else(|| d.recording_ref.map(|r| r.to_string())),
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
            resolve("GET", &format!("/api/inventory/{id}/decode")),
            Some(Ok(id))
        );
        assert_eq!(
            resolve("POST", &format!("/api/inventory/{id}/decode")),
            Some(Err(Some("GET")))
        );
        assert_eq!(
            resolve("GET", "/api/inventory/not-an-id/decode"),
            Some(Err(None))
        );
        assert_eq!(resolve("GET", &format!("/api/inventory/{id}")), None);
        assert_eq!(resolve("GET", &format!("/api/inventory/{id}/band")), None);
        assert!(
            ROUTES
                .iter()
                .any(|(m, p)| *m == "GET" && *p == "/api/inventory/{id}/decode")
        );
    }

    #[test]
    fn latest_per_frame_keeps_one_row_per_decoder_and_frame_model_newest_first() {
        use hk_model::{DecodeId, Timestamp};

        let mk = |frame_model: &str, t_s: i64| Decode {
            id: DecodeId::new(),
            demodulation_ref: None,
            recording_ref: None,
            decoder_id: "recipe:rds".to_owned(),
            decoder_version: "1".to_owned(),
            frame_model: frame_model.to_owned(),
            metadata: json!({}),
            content: None,
            crc_status: CrcStatus::Valid,
            identity: None,
            content_class: hk_model::ContentClass::Unrestricted,
            t: Timestamp::from_unix_nanos(t_s * 1_000_000_000),
        };

        let decodes = vec![
            mk("rds-group", 1),
            mk("rds-ps", 2),
            mk("rds-group", 5),
            mk("rds-ps", 3),
        ];
        let rows = latest_per_frame(decodes);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0]["frame_model"], json!("rds-group"));
        assert_eq!(rows[0]["at"], json!(5.0));
        assert_eq!(rows[1]["frame_model"], json!("rds-ps"));
        assert_eq!(rows[1]["at"], json!(3.0));
    }
}
