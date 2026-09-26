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
//! | GET | `/api/inventory/<id>/decode[?t0=&t1=]` | `{"decodes": [...]}`, latest first |
//!
//! Each row: `{decoder, recipe_id, frame_model, at, fields, crc: {valid}, source_session}`.
//! `recipe_id` is the recipe id when `decoder` is `recipe:<id>`, else `null`. `fields` merges the
//! row's metadata and content (content only when the caller's identity access reveals it — off by
//! default, ADR-0004/T-143 content gating). `source_session` is the producing demodulation's id,
//! else the replayed recording's id, else `null` (live plugin/recipe decodes with neither).
//!
//! # The window (T-384)
//!
//! `t0`/`t1` (Unix seconds on the **capture clock**, given together) scope the answer to
//! *what was decoded in that window*; without them it is the latest of all time, as before.
//! The user's whole-UI rule (CLAUDE.md, 2026-09-16) makes every surface a view over one
//! (time × frequency) window, and the output/decode panels had **no way to ask** — this route
//! carried no time parameter at all, so a panel scrubbed back to an hour ago could only keep
//! showing the live edge's fields and call them the window's. That is the same
//! *we-have-it-but-didn't-render-it* failure as the empty sidebar, inverted: it renders the wrong
//! window's data rather than none of the right window's.
//!
//! **This filters; it never re-decodes.** A `Decode` row is what the decoder or recipe already
//! committed, with its own capture-clock `t`. Serving a past window means selecting the stored
//! rows whose `t` falls inside it — the incremental-decode invariant (CLAUDE.md: *live decoding
//! extends the region's time extent and decodes only the newly-arrived part, never re-decoding
//! what is already done*) holds trivially, because no decoder runs here. Re-running a decoder over
//! already-decoded frames to answer a scrub would break that invariant and is deliberately not
//! what this does.
//!
//! **Latest-per-frame-model is computed after the filter, not before.** The other order would
//! answer "the all-time latest row, if it happens to fall in the window", which reports *nothing*
//! for a window that holds an older row — data that exists and would not be rendered.
//!
//! **Not `/api/captures/{id}/frames`.** That route is keyed by *capture* (one recording of one
//! pipeline's decoded stream) and serves raw stream records verbatim; it already has `from_t`/
//! `to_t` and needs nothing. It answers *which records did this recording hold*, at frame
//! granularity, and is the right route for the packet inspector's own scrubbing. It cannot answer
//! *what has this emitter decoded* — there is no emitter→capture link to follow, and assembling
//! an RDS station name out of raw group records in the client would be both a thin-client
//! violation and the re-decoding the invariant above forbids. The two routes stay distinct: this
//! one is emitter-keyed and serves committed fields; that one is capture-keyed and serves records.
//!
//! **Resolution.** Like `/api/inventory/<id>`: a merged id resolves to its live survivor
//! ([`hk_model::Repository::live_emitter_id`]), 404 `not_found` for an unknown or unparsable id.
//! **Never a lookup that confirms a withheld identity** (T-036): an emitter whose identity is
//! withheld from [`IdentityAccess::Standard`] (the access every unauthenticated-beyond-the-token
//! caller gets here, matching every other inventory read) answers `{"decodes": []}` rather than
//! naming the identity through its decodes.
//!
//! **A provisional identity is served (T-962).** An emitter with no decoded identity answers the
//! rows linked to it that carry a *provisional* identity — a reading whose vote has not reached
//! its scheme's bar (`hk_model::IdentityScheme::commit_votes`; RDS PI: 10 agreeing CRC-valid
//! groups), written with no identity and `identity_provisional: true`, `identity_value`,
//! `identity_votes`, `identity_votes_needed` in its fields — so the panel can say "PI 1704
//! (3 groups, provisional)". There is no identity on such an emitter to confirm or withhold, and
//! each row is gated at `Standard` like every other. Otherwise (nothing linked, or nothing
//! provisional) it answers `{"decodes": []}`.
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
    TimeRange,
};
use serde_json::{Map, Value, json};

use crate::control::{CtlRequest, CtlResponse, Fail, dispatch, refuse_route};
use crate::http::ApiState;
use crate::query::{Params, parse_time_window, ts_s};

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
        |s| read(s, id, req.query),
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

fn read(state: &ApiState, id: EmitterId, q: &Params) -> Result<Value, Fail> {
    let window = parse_time_window(q).map_err(|e| Fail::invalid(e.message))?;
    let repo = store(state)?;
    let live = repo.live_emitter_id(id).map_err(repo_fail)?;
    let entry = repo
        .emitter_with_access(live, IdentityAccess::Standard)
        .map_err(repo_fail)?;
    let identity = match entry.identity {
        InventoryIdentity::Clear { identity, .. } => identity,
        // No decoded identity yet: what it has is at most a provisional reading (T-962) — rows a
        // decoder recorded and linked to this emitter while their identity's vote was below its
        // scheme's bar. Served so "PI 1704 (3 groups, provisional)" is visible; there is no
        // identity here to confirm or withhold.
        InventoryIdentity::None => {
            let decodes = repo
                .provisional_decodes_of_emitter(live)
                .map_err(repo_fail)?;
            return Ok(json!({ "decodes": latest_per_frame(decodes, window) }));
        }
        // Withheld: never confirm it by naming its decodes.
        InventoryIdentity::Withheld { .. } => {
            return Ok(json!({ "decodes": [] }));
        }
    };
    let decodes = repo.decodes_for_identity(&identity).map_err(repo_fail)?;
    Ok(json!({ "decodes": latest_per_frame(decodes, window) }))
}

/// The latest decode per `(decoder_id, frame_model)` **within `window`**, newest first.
/// `decodes_for_identity` returns rows oldest first, so the last write per key wins.
///
/// The window is applied *first* (T-384). Taking the all-time latest and then testing it against
/// the window would answer "nothing decoded here" for a window that holds an older row — which is
/// data that exists, not rendered. Closed interval on both ends, matching every other windowed
/// route (`TimeRange` is inclusive, `hk_model::region`).
fn latest_per_frame(decodes: Vec<Decode>, window: Option<TimeRange>) -> Vec<Value> {
    let mut latest: BTreeMap<(String, String), Decode> = BTreeMap::new();
    for d in decodes {
        if let Some(w) = window
            && (d.t < w.start || d.t > w.end)
        {
            continue;
        }
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
            provenance: None,
        };

        let decodes = vec![
            mk("rds-group", 1),
            mk("rds-ps", 2),
            mk("rds-group", 5),
            mk("rds-ps", 3),
        ];
        let rows = latest_per_frame(decodes, None);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0]["frame_model"], json!("rds-group"));
        assert_eq!(rows[0]["at"], json!(5.0));
        assert_eq!(rows[1]["frame_model"], json!("rds-ps"));
        assert_eq!(rows[1]["at"], json!(3.0));
    }

    /// T-384: the window filters *before* the latest-per-frame-model collapse, so a window holding
    /// only an older row serves that row rather than reporting nothing.
    #[test]
    fn a_window_filters_before_the_latest_per_frame_collapse() {
        use hk_model::{DecodeId, Timestamp};

        let at = |t_s: i64| Timestamp::from_unix_nanos(t_s * 1_000_000_000);
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
            t: at(t_s),
            provenance: None,
        };
        let decodes = || vec![mk("rds-group", 1), mk("rds-ps", 2), mk("rds-group", 5)];

        let early = latest_per_frame(decodes(), Some(TimeRange::new(at(0), at(3))));
        assert_eq!(early.len(), 2, "{early:?}");
        assert_eq!(early[0]["at"], json!(2.0), "rds-ps, newest in the window");
        assert_eq!(
            early[1]["at"],
            json!(1.0),
            "the OLDER rds-group is this window's latest: {early:?}"
        );

        let quiet = latest_per_frame(decodes(), Some(TimeRange::new(at(6), at(9))));
        assert!(quiet.is_empty(), "nothing was decoded then: {quiet:?}");

        // Closed on both ends.
        let point = latest_per_frame(decodes(), Some(TimeRange::new(at(5), at(5))));
        assert_eq!(point.len(), 1, "{point:?}");
        assert_eq!(point[0]["at"], json!(5.0));
    }
}
