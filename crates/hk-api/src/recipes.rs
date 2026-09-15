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
//! | GET, DELETE | `/api/recipes/{id}` | the latest document / every user version deleted |
//! | GET | `/api/recipes/{id}/versions/{version}` | one saved version |
//! | GET, POST | `/api/pipelines` | `{"pipelines": [...]}` / start (201; `503 busy` at the chain budget) |
//! | GET, DELETE | `/api/pipelines/{id}` | one pipeline / stop it |
//! | PUT | `/api/pipelines/{id}/recipe` | hot edit → `{edit_rev, applied_at_sample, plan, swap}` |
//! | POST | `/api/pipelines/{id}/save` | the running revision as the next version (201) |
//!
//! A validation failure answers `400 invalid` with `errors: [{path, message}]` and `warnings`.
//! Mutating routes are audited like every other; `POST /api/recipes/validate` saves nothing and
//! is not audited.

use std::sync::Arc;

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
    Get(String),
    Version(String, u32),
    Delete(String),
    Pipelines,
    Start,
    Pipeline(String),
    Edit(String),
    SavePipeline(String),
    Stop(String),
}

impl Action {
    fn name(&self) -> &'static str {
        match self {
            Self::Blocks => "blocks_list",
            Self::List => "recipes_list",
            Self::Save => "recipe_save",
            Self::Validate => "recipe_validate",
            Self::Get(_) | Self::Version(..) => "recipe_get",
            Self::Delete(_) => "recipe_delete",
            Self::Pipelines => "pipelines_list",
            Self::Start => "pipeline_start",
            Self::Pipeline(_) => "pipeline_get",
            Self::Edit(_) => "pipeline_edit",
            Self::SavePipeline(_) => "pipeline_save",
            Self::Stop(_) => "pipeline_stop",
        }
    }

    fn mutating(&self) -> bool {
        !matches!(
            self,
            Self::Blocks
                | Self::Validate
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
        assert_eq!(listed.len(), 13);
        for (method, path) in listed {
            let concrete = path.replace("{id}", "x1").replace("{version}", "3");
            assert!(
                matches!(resolve(method, &concrete), Some(Ok(_))),
                "{method} {path}"
            );
        }
    }
}
