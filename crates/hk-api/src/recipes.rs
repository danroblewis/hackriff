//! Recipe and pipeline routes (T-088, ADR-0011 §2.3–§2.5; `docs/api.md` "Recipes and
//! pipelines"): the decoder workbench's block catalogue, recipe store and running pipelines over
//! HTTP. All signal logic lives behind [`RecipeControl`] (the pipeline's recipe runtime); this
//! module only routes, audits and shapes errors.
//!
//! | Method | Path | Answers |
//! |---|---|---|
//! | GET | `/api/blocks` | `{"blocks": [BlockDescriptor]}` |
//! | GET, POST | `/api/recipes` | `{"recipes": [...]}` / save as `latest + 1` (201) |
//! | POST | `/api/recipes/validate` | `{valid, errors, warnings, edges}` |
//! | GET | `/api/recipes/match?emitter=<id>` | recipes ranked against that emitter's *measured* parameters, with reasons |
//! | GET, DELETE | `/api/recipes/{id}` | the latest document / every user version deleted |
//! | GET | `/api/recipes/{id}/versions/{version}` | one saved version |
//! | GET, POST | `/api/pipelines` | `{"pipelines": [...]}` / start (201; `503 busy` at the chain budget) |
//! | GET, DELETE | `/api/pipelines/{id}` | one pipeline / stop it |
//! | PUT | `/api/pipelines/{id}/recipe` | hot edit → `{edit_rev, applied_at_sample, plan, swap}` |
//! | POST | `/api/pipelines/{id}/save` | the running revision as the next version (201) |
//! | PUT | `/api/pipelines/{id}/channels` | follow-hops channel set → `{channels, added, removed, applied_at_sample}` |
//! | POST | `/api/pipelines/{id}/channels/refresh` | re-resolve the channel source and apply it (same answer) |
//!
//! A validation failure answers `400 invalid` with `errors: [{path, message}]` and `warnings`.
//! Mutating routes are audited like every other; `POST /api/recipes/validate` saves nothing and
//! is not audited.
//!
//! `validate` and `match` are fixed sub-paths of `/api/recipes/`, so (as for `validate` since
//! T-088) a recipe whose id is literally `match` is not addressable at `/api/recipes/match`.
//!
//! # Matching (T-164, ADR-0013 §4.9 gap 7b)
//!
//! `GET /api/recipes/match?emitter=<id>` ranks the recipe store against **what was measured on
//! one emitter** — the classified modulation family, the detected or demodulated bandwidth, the
//! estimated symbol rate, a duty-cycle-derived burstiness and measured feature tokens such as a
//! locked 19 kHz pilot. All the arithmetic is [`hk_recipe::matching`]; this module only gathers
//! the measurements from the repository and shapes the answer.
//!
//! **It never looks a frequency up to decide the order.** A recipe's `freq_hz` is a band-plan
//! prior, and carries no weight in the score: it survives only as a tie-break between candidates
//! the measurements cannot separate, the rule T-212 established for C17 classification priors. A
//! parameter nothing has measured is scored as *no evidence*, never as agreement (T-163 serves
//! those as `null` precisely so this holds), and an emission that fits nothing gets an empty
//! ranking rather than a forced top choice. Nothing here tunes anything or starts a pipeline.

use std::sync::{Arc, MutexGuard, PoisonError};

use hk_model::{EmitterId, IdentityAccess, InventoryIdentity, RepoError, Repository};
use hk_recipe::matching::{self, MeasuredSignal};
use hk_recipe::{Entry, MatchHints};
use serde_json::{Map, Value, json};

use crate::control::{
    Applied, CtlRequest, CtlResponse, Fail, dispatch, no_fields, parse_body, refuse_route,
};
use crate::http::ApiState;

/// A request to the recipe runtime.
#[derive(Clone, Debug, PartialEq)]
pub enum RecipeCall {
    /// The block catalogue.
    Blocks,
    /// Every recipe.
    ListRecipes,
    /// A recipe document (latest or one version).
    GetRecipe {
        /// Recipe id.
        id: String,
        /// Version; `None` = latest.
        version: Option<u32>,
    },
    /// Save a recipe document as the next version.
    SaveRecipe(Value),
    /// Validate a recipe document.
    ValidateRecipe(Value),
    /// Delete every user version of a recipe.
    DeleteRecipe(String),
    /// Start a pipeline (`{recipe_id, version?} | {recipe}` and `target`).
    StartPipeline(Value),
    /// Every pipeline.
    ListPipelines,
    /// One pipeline.
    GetPipeline(String),
    /// Hot-edit a pipeline to a draft recipe document.
    EditPipeline {
        /// Pipeline id.
        id: String,
        /// The draft.
        recipe: Value,
    },
    /// Save a pipeline's running revision.
    SavePipeline(String),
    /// Change a follow-hops pipeline's channel set (T-107).
    SetChannels {
        /// Pipeline id.
        id: String,
        /// Requested channel centres, Hz.
        channels_hz: Vec<f64>,
    },
    /// Re-resolve a follow-hops pipeline's channel source and apply it (T-107).
    RefreshChannels(String),
    /// Stop a pipeline.
    StopPipeline(String),
}

/// A refused or failed recipe call.
#[derive(Clone, Debug, PartialEq)]
pub struct RecipeFail {
    /// HTTP status.
    pub status: u16,
    /// Stable code.
    pub code: &'static str,
    /// Message (never echoes values).
    pub message: String,
    /// `{errors, warnings}` of a validation failure, else `null`.
    pub detail: Value,
}

/// The pipeline's recipe runtime as the API sees it.
pub trait RecipeControl: Send + Sync {
    /// Runs one call; the value is the response body.
    fn call(&self, call: RecipeCall) -> Result<Value, RecipeFail>;
}

#[derive(Clone, Debug, PartialEq)]
enum Action {
    Blocks,
    List,
    Save,
    Validate,
    /// Rank recipes against an emitter's measured parameters (T-164).
    Match,
    Get(String),
    Version(String, u32),
    Delete(String),
    Pipelines,
    Start,
    Pipeline(String),
    Edit(String),
    SavePipeline(String),
    Stop(String),
    Channels(String),
    RefreshChannels(String),
}

impl Action {
    fn name(&self) -> &'static str {
        match self {
            Self::Blocks => "blocks_list",
            Self::List => "recipes_list",
            Self::Save => "recipe_save",
            Self::Validate => "recipe_validate",
            Self::Match => "recipes_match",
            Self::Get(_) | Self::Version(..) => "recipe_get",
            Self::Delete(_) => "recipe_delete",
            Self::Pipelines => "pipelines_list",
            Self::Start => "pipeline_start",
            Self::Pipeline(_) => "pipeline_get",
            Self::Edit(_) => "pipeline_edit",
            Self::SavePipeline(_) => "pipeline_save",
            Self::Stop(_) => "pipeline_stop",
            Self::Channels(_) => "pipeline_channels",
            Self::RefreshChannels(_) => "pipeline_channels_refresh",
        }
    }

    fn mutating(&self) -> bool {
        !matches!(
            self,
            Self::Blocks
                | Self::Validate
                | Self::Match
                | Self::List
                | Self::Get(_)
                | Self::Version(..)
                | Self::Pipelines
                | Self::Pipeline(_)
        )
    }
}

fn segment(s: &str) -> Option<String> {
    (!s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
    .then(|| s.to_owned())
}

/// Resolves `(method, path)`: `None` when the path is not a recipe or pipeline path.
fn resolve(method: &str, path: &str) -> Option<Result<Action, Option<&'static str>>> {
    let by = |pairs: &[(&str, Action)], allow: &'static str| {
        Some(
            pairs
                .iter()
                .find(|(m, _)| *m == method)
                .map(|(_, a)| a.clone())
                .ok_or(Some(allow)),
        )
    };
    match path {
        "/api/blocks" => return by(&[("GET", Action::Blocks)], "GET"),
        "/api/recipes" => {
            return by(
                &[("GET", Action::List), ("POST", Action::Save)],
                "GET, POST",
            );
        }
        "/api/recipes/validate" => return by(&[("POST", Action::Validate)], "POST"),
        "/api/recipes/match" => return by(&[("GET", Action::Match)], "GET"),
        "/api/pipelines" => {
            return by(
                &[("GET", Action::Pipelines), ("POST", Action::Start)],
                "GET, POST",
            );
        }
        _ => {}
    }
    if let Some(rest) = path.strip_prefix("/api/recipes/") {
        let parts: Vec<&str> = rest.split('/').collect();
        return match parts.as_slice() {
            [id] => match segment(id) {
                Some(id) => by(
                    &[
                        ("GET", Action::Get(id.clone())),
                        ("DELETE", Action::Delete(id)),
                    ],
                    "GET, DELETE",
                ),
                None => Some(Err(None)),
            },
            [id, "versions", v] => match (segment(id), v.parse::<u32>()) {
                (Some(id), Ok(v)) if v > 0 => by(&[("GET", Action::Version(id, v))], "GET"),
                _ => Some(Err(None)),
            },
            _ => Some(Err(None)),
        };
    }
    if let Some(rest) = path.strip_prefix("/api/pipelines/") {
        let parts: Vec<&str> = rest.split('/').collect();
        return match parts.as_slice() {
            [id] => match segment(id) {
                Some(id) => by(
                    &[
                        ("GET", Action::Pipeline(id.clone())),
                        ("DELETE", Action::Stop(id)),
                    ],
                    "GET, DELETE",
                ),
                None => Some(Err(None)),
            },
            [id, "recipe"] => match segment(id) {
                Some(id) => by(&[("PUT", Action::Edit(id))], "PUT"),
                None => Some(Err(None)),
            },
            [id, "save"] => match segment(id) {
                Some(id) => by(&[("POST", Action::SavePipeline(id))], "POST"),
                None => Some(Err(None)),
            },
            [id, "channels"] => match segment(id) {
                Some(id) => by(&[("PUT", Action::Channels(id))], "PUT"),
                None => Some(Err(None)),
            },
            [id, "channels", "refresh"] => match segment(id) {
                Some(id) => by(&[("POST", Action::RefreshChannels(id))], "POST"),
                None => Some(Err(None)),
            },
            _ => Some(Err(None)),
        };
    }
    None
}

/// This module's routes; `None` = not mine.
pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    let action = match resolve(req.method, req.path)? {
        Ok(a) => a,
        Err(allow) => return Some(refuse_route(state, req, allow)),
    };
    if action == Action::Validate {
        // Saves nothing: token-checked like every POST, but not an audited mutation.
        return Some(
            match parse_body(req).and_then(|body| apply(state, &action, &body)) {
                Ok(a) => CtlResponse {
                    status: a.status,
                    body: a.body,
                    allow: None,
                },
                Err(f) => f.response(),
            },
        );
    }
    if action == Action::Match {
        // The emitter is a query parameter, which `resolve` (path only) never sees.
        let emitter = req
            .query
            .iter()
            .find(|(k, _)| k == "emitter")
            .map(|(_, v)| v.clone());
        return Some(dispatch(
            state,
            req,
            action.name(),
            false,
            |s| match_read(s, emitter.as_deref()),
            |_, _| Err(Fail::new(500, "failed", "not a mutating action")),
        ));
    }
    Some(dispatch(
        state,
        req,
        action.name(),
        action.mutating(),
        |s| read(s, &action),
        |s, body| apply(s, &action, body),
    ))
}

fn control(state: &ApiState) -> Result<&Arc<dyn RecipeControl>, Fail> {
    state
        .recipes
        .as_ref()
        .ok_or_else(|| Fail::new(503, "unavailable", "no recipe runtime on this server"))
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

/// Every recipe the runtime knows, as ranking entries. A recipe whose `match` block is missing or
/// unreadable still competes, with no declared expectations — [`matching::rank`] then rules it out
/// for having nothing to rank it by, rather than this route silently dropping it.
fn entries(state: &ApiState) -> Result<Vec<Entry>, Fail> {
    let listed = control(state)?
        .call(RecipeCall::ListRecipes)
        .map_err(|f| Fail::new(f.status, f.code, f.message))?;
    let rows = listed
        .get("recipes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(rows
        .iter()
        .filter_map(|r| {
            Some(Entry {
                id: r.get("id")?.as_str()?.to_owned(),
                version: r.get("version").and_then(Value::as_u64).unwrap_or(1) as u32,
                name: r
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                hints: r
                    .get("match")
                    .cloned()
                    .and_then(|m| serde_json::from_value::<MatchHints>(m).ok())
                    .unwrap_or_default(),
            })
        })
        .collect())
}

/// What has actually been measured on one emitter, and an echo of it for the response.
///
/// Every field comes from a measurement: the classified family (falling back to the mode the
/// demodulator locked), the bandwidth the demodulator filtered to (falling back to the detected
/// extent), the estimated symbol rate, burstiness derived from a measured duty cycle, and feature
/// tokens the estimator evidenced. A measurement that was never taken stays `None` and is scored
/// as no evidence — nothing substitutes a default (T-163).
///
/// **Withheld identities** (T-036/T-163): an emitter whose identity is withheld from
/// [`IdentityAccess::Standard`] contributes no demodulation session, exactly as its
/// `estimated_params` read `null` on `/api/inventory/{id}`. Its detection-level measurements are
/// used, since `/api/inventory` already serves those in clear on the same row, so this route adds
/// no way to tell a withheld emitter from one nothing has demodulated yet.
fn measured_signal(
    repo: &Repository,
    id: EmitterId,
) -> Result<(EmitterId, MeasuredSignal, Value), Fail> {
    let live = repo.live_emitter_id(id).map_err(repo_fail)?;
    let entry = repo
        .emitter_with_access(live, IdentityAccess::Standard)
        .map_err(repo_fail)?;
    let withheld = matches!(entry.identity, InventoryIdentity::Withheld { .. });
    let session = if withheld {
        None
    } else {
        repo.latest_demodulation_for_emitter(live)
            .map_err(repo_fail)?
    };
    // Burstiness comes from a measured duty cycle — and only a measured one. `Recurrence` reports
    // `duty_cycle = on_air_s / span_s`, where an appearance whose duty cycle was never measured
    // adds nothing to `on_air_s` ("unknown duty is not evidence of continuity"). A zero
    // `on_air_s` therefore means *nothing was measured*, not that the emitter is silent, and
    // reading that 0.0 as "bursty" would hand a bare carrier the burstiness of a pager burst —
    // the unmeasured-as-evidence mistake this route exists to avoid.
    let recurrence = repo.emitter_recurrence(live, 0).map_err(repo_fail)?;
    let duty = recurrence.duty_cycle.filter(|_| recurrence.on_air_s > 0.0);

    let params = session.as_ref().map(|d| &d.params);
    let classified = entry.family.clone();
    let family = classified
        .clone()
        .or_else(|| session.as_ref().map(|d| d.mode.clone()));
    let family_source = match (&classified, &family) {
        (Some(_), _) => Some("classification"),
        (None, Some(_)) => Some("demodulation"),
        (None, None) => None,
    };
    let demodulated_bw = params.and_then(|p| p.bandwidth_hz);
    let bandwidth_source = if demodulated_bw.is_some() {
        "demodulation"
    } else {
        "detection"
    };

    let measured = MeasuredSignal {
        family,
        f_center_hz: Some(entry.emitter.f_center_hz),
        bandwidth_hz: demodulated_bw.or(Some(entry.emitter.bandwidth_hz)),
        symbol_rate_bd: params.and_then(|p| p.symbol_rate_hz),
        bursty: duty.map(matching::bursty_from_duty),
        features: params
            .map(matching::features_from_params)
            .unwrap_or_default(),
    };
    let echo = json!({
        "family": measured.family,
        "family_source": family_source,
        "f_center_hz": entry.emitter.f_center_hz,
        "bandwidth_hz": measured.bandwidth_hz,
        "bandwidth_source": bandwidth_source,
        "symbol_rate_bd": measured.symbol_rate_bd,
        "bursty": measured.bursty,
        "duty_cycle": duty,
        "features": measured.features,
        "session": session.as_ref().map(|d| d.id.to_string()),
    });
    Ok((live, measured, echo))
}

/// `GET /api/recipes/match?emitter=<id>`.
fn match_read(state: &ApiState, emitter: Option<&str>) -> Result<Value, Fail> {
    let Some(raw) = emitter else {
        return Err(Fail::invalid("emitter is required"));
    };
    // Like `/api/signatures/match`: an unparsable id is a 404, never a 200 that could be probed
    // for which ids exist.
    let id: EmitterId = raw
        .parse()
        .map_err(|_| Fail::new(404, "not_found", "no such inventory entry"))?;
    // Ask the recipe runtime before taking the inventory lock: never hold one across the other.
    let entries = entries(state)?;
    let (live, measured, echo) = {
        let repo = store(state)?;
        measured_signal(&repo, id)?
    };
    let ranked = matching::rank(&entries, &measured);
    Ok(json!({
        "emitter": live.to_string(),
        "measured": echo,
        "outcome": ranked.outcome.as_str(),
        "reasons": ranked.reasons,
        "recipes": ranked.candidates,
        "ruled_out": ranked.ruled_out,
    }))
}

fn read(state: &ApiState, action: &Action) -> Result<Value, Fail> {
    let call = match action {
        Action::Blocks => RecipeCall::Blocks,
        Action::List => RecipeCall::ListRecipes,
        Action::Get(id) => RecipeCall::GetRecipe {
            id: id.clone(),
            version: None,
        },
        Action::Version(id, v) => RecipeCall::GetRecipe {
            id: id.clone(),
            version: Some(*v),
        },
        Action::Pipelines => RecipeCall::ListPipelines,
        Action::Pipeline(id) => RecipeCall::GetPipeline(id.clone()),
        _ => return Err(Fail::new(500, "failed", "not a read")),
    };
    control(state)?
        .call(call)
        .map_err(|f| Fail::new(f.status, f.code, f.message))
}

fn apply(state: &ApiState, action: &Action, body: &Map<String, Value>) -> Result<Applied, Fail> {
    let doc = || Value::Object(body.clone());
    let (call, status) = match action {
        Action::Save => (RecipeCall::SaveRecipe(doc()), 201),
        Action::Validate => (RecipeCall::ValidateRecipe(doc()), 200),
        Action::Start => (RecipeCall::StartPipeline(doc()), 201),
        Action::Edit(id) => (
            RecipeCall::EditPipeline {
                id: id.clone(),
                recipe: doc(),
            },
            200,
        ),
        Action::Delete(id) => {
            no_fields(body)?;
            (RecipeCall::DeleteRecipe(id.clone()), 200)
        }
        Action::SavePipeline(id) => {
            no_fields(body)?;
            (RecipeCall::SavePipeline(id.clone()), 201)
        }
        Action::Stop(id) => {
            no_fields(body)?;
            (RecipeCall::StopPipeline(id.clone()), 200)
        }
        Action::Channels(id) => (
            RecipeCall::SetChannels {
                id: id.clone(),
                channels_hz: channels_body(body)?,
            },
            200,
        ),
        Action::RefreshChannels(id) => {
            no_fields(body)?;
            (RecipeCall::RefreshChannels(id.clone()), 200)
        }
        _ => return Err(Fail::new(500, "failed", "not a mutating action")),
    };
    match control(state)?.call(call) {
        Ok(v) => Ok(Applied {
            status,
            new: summary(&v),
            body: v,
            old: Value::Null,
        }),
        // A validation failure keeps its paths: 400 with errors and warnings.
        Err(f) if !f.detail.is_null() => Ok(Applied {
            status: f.status,
            body: json!({
                "error": f.message,
                "code": f.code,
                "errors": f.detail.get("errors").cloned().unwrap_or(json!([])),
                "warnings": f.detail.get("warnings").cloned().unwrap_or(json!([])),
            }),
            old: Value::Null,
            new: Value::Null,
        }),
        Err(f) => Err(Fail::new(f.status, f.code, f.message)),
    }
}

/// `{"channels_hz": [Hz, ...]}`: finite positive numbers, nothing else (`400 invalid`).
fn channels_body(body: &Map<String, Value>) -> Result<Vec<f64>, Fail> {
    let invalid = |m: &str| Fail::new(400, "invalid", m);
    if body.keys().any(|k| k != "channels_hz") {
        return Err(invalid("the body has only channels_hz"));
    }
    let list = body
        .get("channels_hz")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("channels_hz must be an array of frequencies (Hz)"))?;
    list.iter()
        .map(|v| {
            v.as_f64()
                .filter(|f| f.is_finite() && *f > 0.0)
                .ok_or_else(|| invalid("channels_hz must hold positive frequencies (Hz)"))
        })
        .collect()
}

/// What the audit log keeps of a response: ids and revisions, not whole documents.
fn summary(v: &Value) -> Value {
    let mut m = Map::new();
    for k in [
        "id",
        "version",
        "edit_rev",
        "applied_at_sample",
        "pipeline_id",
        "valid",
    ] {
        if let Some(x) = v.get(k) {
            m.insert(k.into(), x.clone());
        }
    }
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::ROUTES;

    #[test]
    fn routes_resolve_with_methods() {
        assert_eq!(resolve("GET", "/api/blocks"), Some(Ok(Action::Blocks)));
        assert_eq!(resolve("POST", "/api/blocks"), Some(Err(Some("GET"))));
        assert_eq!(resolve("POST", "/api/recipes"), Some(Ok(Action::Save)));
        assert_eq!(
            resolve("POST", "/api/recipes/validate"),
            Some(Ok(Action::Validate))
        );
        assert_eq!(
            resolve("GET", "/api/recipes/validate"),
            Some(Err(Some("POST")))
        );
        // T-164: a fixed sub-path, so it is never read as the recipe id "match".
        assert_eq!(
            resolve("GET", "/api/recipes/match"),
            Some(Ok(Action::Match))
        );
        assert_eq!(
            resolve("POST", "/api/recipes/match"),
            Some(Err(Some("GET")))
        );
        assert_eq!(
            resolve("GET", "/api/recipes/rds/versions/2"),
            Some(Ok(Action::Version("rds".into(), 2)))
        );
        assert_eq!(
            resolve("GET", "/api/recipes/rds/versions/x"),
            Some(Err(None))
        );
        assert_eq!(
            resolve("PUT", "/api/pipelines/p1/recipe"),
            Some(Ok(Action::Edit("p1".into())))
        );
        assert_eq!(
            resolve("POST", "/api/pipelines/p1"),
            Some(Err(Some("GET, DELETE")))
        );
        assert_eq!(
            resolve("PUT", "/api/pipelines/p1/channels"),
            Some(Ok(Action::Channels("p1".into())))
        );
        assert_eq!(
            resolve("POST", "/api/pipelines/p1/channels"),
            Some(Err(Some("PUT")))
        );
        assert_eq!(
            resolve("POST", "/api/pipelines/p1/channels/refresh"),
            Some(Ok(Action::RefreshChannels("p1".into())))
        );
        assert_eq!(resolve("GET", "/api/pipelines/a/b/c"), Some(Err(None)));
        assert_eq!(resolve("GET", "/api/recipesx"), None);
        assert_eq!(resolve("GET", "/api/selections"), None);
    }

    #[test]
    fn every_recipe_route_is_in_the_route_table() {
        let listed: Vec<_> = ROUTES
            .iter()
            .filter(|(_, p)| {
                p.starts_with("/api/blocks")
                    || p.starts_with("/api/recipes")
                    || p.starts_with("/api/pipelines")
            })
            .collect();
        assert_eq!(listed.len(), 16);
        for (method, path) in listed {
            let concrete = path.replace("{id}", "x1").replace("{version}", "3");
            assert!(
                matches!(resolve(method, &concrete), Some(Ok(_))),
                "{method} {path}"
            );
        }
    }
}
