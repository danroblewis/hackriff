//! `POST /api/analyze` (T-190): the "Analyze / synthesize decoder" action's target validation.
//!
//! The synthesis engine (MAUTO, `docs/15-decoder-synthesis.md` §8; the region-analyze API and
//! streamed best-pipeline-plus-evidence response are ADR-0014 when that milestone is scheduled)
//! is not built yet. Until then this route only resolves and validates its target — a persisted
//! selection, an inventory emitter (merged ids resolve to the live entity, like
//! `GET /api/inventory/{id}`), or an ad-hoc band with an optional history time window — and
//! answers `501 not_implemented` once the target is known to exist. No engine code, no
//! background work: every call is answered synchronously.
//!
//! # Endpoint
//!
//! | Method | Path | Body | Answers |
//! |---|---|---|---|
//! | POST | `/api/analyze` | `{"selection_id"} \| {"emitter_id"} \| {"band": {"f_lo", "f_hi", "t_lo"?, "t_hi"?}}` (exactly one) | `501 {"error", "code": "not_implemented"}` once the target validates |
//!
//! Errors: `400 invalid` (unknown field, zero or several target forms, malformed `band`,
//! `f_lo >= f_hi`), `404 not_found` (unknown selection or emitter, including a malformed id — the
//! same shape `/api/inventory/{id}` answers with), `503 unavailable` (no selection store or no
//! inventory on this server).
//!
//! This is a mutating control route (auth, audit as `analyze`, cross-origin checks — the same
//! rules as every other route in [`crate::control`]) even though it never changes anything: that
//! keeps the contract stable once MAUTO fills it in.

use std::sync::{MutexGuard, PoisonError};

use hk_model::{EmitterId, IdentityAccess, RepoError, Repository, SelectionId};
use serde_json::{Map, Value};

use crate::control::{
    Applied, CtlRequest, CtlResponse, Fail, dispatch, number, only, refuse_route, text,
};
use crate::http::ApiState;

/// A validated `/api/analyze` target.
#[derive(Clone, Debug, PartialEq)]
enum Target {
    /// A persisted selection ([`crate::selections`]).
    Selection(SelectionId),
    /// An inventory emitter (T-078); resolved through `live_emitter_id` like `/api/inventory/{id}`.
    Emitter(EmitterId),
    /// An ad-hoc band, optionally over a past time window (the IQ ring) rather than live.
    Band {
        f_lo_hz: f64,
        f_hi_hz: f64,
        #[allow(dead_code)] // carried for the future engine; unused by the stub
        t_lo_s: Option<f64>,
        #[allow(dead_code)]
        t_hi_s: Option<f64>,
    },
}

const FIELDS: &[&str] = &["selection_id", "emitter_id", "band"];
const BAND_FIELDS: &[&str] = &["f_lo", "f_hi", "t_lo", "t_hi"];

fn parse_target(body: &Map<String, Value>) -> Result<Target, Fail> {
    only(body, FIELDS)?;
    let selection = text(body, "selection_id")?.flatten();
    let emitter = text(body, "emitter_id")?.flatten();
    let band = match body.get("band") {
        None | Some(Value::Null) => None,
        Some(Value::Object(b)) => {
            only(b, BAND_FIELDS)?;
            let f_lo_hz =
                number(b, "f_lo")?.ok_or_else(|| Fail::invalid("band.f_lo is required"))?;
            let f_hi_hz =
                number(b, "f_hi")?.ok_or_else(|| Fail::invalid("band.f_hi is required"))?;
            if !(f_lo_hz >= 0.0 && f_lo_hz < f_hi_hz) {
                return Err(Fail::invalid("band needs 0 <= f_lo < f_hi (Hz)"));
            }
            let t_lo_s = number(b, "t_lo")?;
            let t_hi_s = number(b, "t_hi")?;
            Some(Target::Band {
                f_lo_hz,
                f_hi_hz,
                t_lo_s,
                t_hi_s,
            })
        }
        Some(_) => {
            return Err(Fail::invalid(
                "band must be {\"f_lo\", \"f_hi\", \"t_lo\"?, \"t_hi\"?}",
            ));
        }
    };
    match (selection, emitter, band) {
        (Some(s), None, None) => s
            .parse::<SelectionId>()
            .map(Target::Selection)
            .map_err(|_| Fail::new(404, "not_found", "no such selection")),
        (None, Some(e), None) => e
            .parse::<EmitterId>()
            .map(Target::Emitter)
            .map_err(|_| Fail::new(404, "not_found", "no such inventory entry")),
        (None, None, Some(band)) => Ok(band),
        _ => Err(Fail::invalid(
            "give exactly one of selection_id, emitter_id, band",
        )),
    }
}

fn selections_store(state: &ApiState) -> Result<MutexGuard<'_, Repository>, Fail> {
    state
        .bookmarks
        .as_ref()
        .map(|r| r.lock().unwrap_or_else(PoisonError::into_inner))
        .ok_or_else(|| Fail::new(503, "unavailable", "no selection store on this server"))
}

fn inventory_store(state: &ApiState) -> Result<MutexGuard<'_, Repository>, Fail> {
    state
        .inventory
        .as_ref()
        .map(|r| r.lock().unwrap_or_else(PoisonError::into_inner))
        .ok_or_else(|| Fail::new(503, "unavailable", "no signal inventory on this server"))
}

fn selection_exists(state: &ApiState, id: SelectionId) -> Result<(), Fail> {
    selections_store(state)?
        .selection(id)
        .map(|_| ())
        .map_err(|e| match e {
            RepoError::NotFound { .. } => Fail::new(404, "not_found", "no such selection"),
            RepoError::Invalid(m) => Fail::invalid(m),
            other => Fail::new(500, "failed", format!("selection store: {other}")),
        })
}

/// Existence of `id`, resolving a merged emitter to the live entity like `/api/inventory/{id}`.
fn emitter_exists(state: &ApiState, id: EmitterId) -> Result<(), Fail> {
    let repo = inventory_store(state)?;
    let not_found = || Fail::new(404, "not_found", "no such inventory entry");
    let fail = |e: RepoError| match e {
        RepoError::NotFound { .. } => not_found(),
        RepoError::Invalid(m) => Fail::invalid(m),
        other => Fail::new(500, "failed", format!("inventory store: {other}")),
    };
    let live = repo.live_emitter_id(id).map_err(fail)?;
    repo.emitter_with_access(live, IdentityAccess::Standard)
        .map(|_| ())
        .map_err(fail)
}

fn apply(state: &ApiState, body: &Map<String, Value>) -> Result<Applied, Fail> {
    let target = parse_target(body)?;
    match target {
        Target::Selection(id) => selection_exists(state, id)?,
        Target::Emitter(id) => emitter_exists(state, id)?,
        Target::Band { .. } => {}
    }
    Err(Fail::new(
        501,
        "not_implemented",
        "analyze is not implemented yet",
    ))
}

fn resolve(method: &str, path: &str) -> Option<Result<(), Option<&'static str>>> {
    if path != "/api/analyze" {
        return None;
    }
    Some(if method == "POST" {
        Ok(())
    } else {
        Err(Some("POST"))
    })
}

/// Routes `/api/analyze`; `None` when `path` is not it.
pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    match resolve(req.method, req.path)? {
        Ok(()) => {}
        Err(allow) => return Some(refuse_route(state, req, allow)),
    }
    Some(dispatch(
        state,
        req,
        "analyze",
        true,
        |_| Err(Fail::new(500, "failed", "not a read")),
        apply,
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn body(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn routes_resolve_with_methods() {
        assert_eq!(resolve("POST", "/api/analyze"), Some(Ok(())));
        assert_eq!(resolve("GET", "/api/analyze"), Some(Err(Some("POST"))));
        assert_eq!(resolve("GET", "/api/analyzex"), None);
        assert_eq!(resolve("GET", "/api/other"), None);
    }

    /// Unwraps a valid target, panicking with the status/body `Fail` would have answered.
    fn expect_ok(v: Value) -> Target {
        match parse_target(&body(v)) {
            Ok(t) => t,
            Err(f) => {
                let r = f.response();
                panic!("expected a valid target, got {} {}", r.status, r.body);
            }
        }
    }

    /// The HTTP status a bad body answers with.
    fn expect_err_status(v: Value) -> u16 {
        match parse_target(&body(v)) {
            Ok(t) => panic!("expected an error, got {t:?}"),
            Err(f) => f.response().status,
        }
    }

    #[test]
    fn target_forms_parse_and_validate() {
        let sid = SelectionId::new();
        let eid = EmitterId::new();
        assert_eq!(
            expect_ok(json!({ "selection_id": sid.to_string() })),
            Target::Selection(sid)
        );
        assert_eq!(
            expect_ok(json!({ "emitter_id": eid.to_string() })),
            Target::Emitter(eid)
        );
        assert_eq!(
            expect_ok(json!({ "band": { "f_lo": 100.0e6, "f_hi": 101.0e6 } })),
            Target::Band {
                f_lo_hz: 100.0e6,
                f_hi_hz: 101.0e6,
                t_lo_s: None,
                t_hi_s: None,
            }
        );
        assert_eq!(
            expect_ok(
                json!({ "band": { "f_lo": 100.0e6, "f_hi": 101.0e6, "t_lo": 1.0, "t_hi": 2.0 } })
            ),
            Target::Band {
                f_lo_hz: 100.0e6,
                f_hi_hz: 101.0e6,
                t_lo_s: Some(1.0),
                t_hi_s: Some(2.0),
            }
        );
        for bad in [
            json!({}),
            json!({ "selection_id": sid.to_string(), "emitter_id": eid.to_string() }),
            json!({ "selection_id": sid.to_string(), "band": { "f_lo": 1.0, "f_hi": 2.0 } }),
            json!({ "selection_id": sid.to_string(), "extra": 1 }),
            json!({ "band": { "f_lo": 2.0, "f_hi": 1.0 } }),
            json!({ "band": { "f_lo": 1.0, "f_hi": 1.0 } }),
            json!({ "band": { "f_lo": -1.0, "f_hi": 1.0 } }),
            json!({ "band": { "f_hi": 1.0 } }),
            json!({ "band": { "f_lo": 1.0, "f_hi": 2.0, "extra": 1 } }),
            json!({ "band": "not an object" }),
        ] {
            assert_eq!(expect_err_status(bad.clone()), 400, "{bad}");
        }
        // A malformed id is 404, not 400 (like a malformed `/api/inventory/{id}` path).
        assert_eq!(
            expect_err_status(json!({ "selection_id": "not-a-uuid" })),
            404
        );
        assert_eq!(
            expect_err_status(json!({ "emitter_id": "not-a-uuid" })),
            404
        );
    }

    #[test]
    fn a_valid_band_target_is_not_yet_implemented() {
        let state = ApiState::default();
        match apply(
            &state,
            &body(json!({ "band": { "f_lo": 1.0, "f_hi": 2.0 } })),
        ) {
            Ok(_) => panic!("expected 501 not_implemented"),
            Err(f) => {
                let r = f.response();
                assert_eq!(r.status, 501);
                assert_eq!(r.body["code"], json!("not_implemented"));
            }
        }
    }
}
