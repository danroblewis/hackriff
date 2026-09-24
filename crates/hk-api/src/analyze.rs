//! `/api/analyze` (T-190, T-546, T-859): **region-analyze jobs**, and why a signal's decoding
//! pipeline was chosen.
//!
//! Two answers live at `POST /api/analyze`, told apart by the body:
//!
//! - **A job** (ADR-0015 §5.1, MAUTO M-8): a selection, emitter or band target plus any of the
//!   job fields (`profile`, `max_wall_s`, `source`, `live_s`, `templates`, `attach`) — or a
//!   selection or band target with none — answers `202 {"job"}` with `Location:
//!   /api/analyze/{id}`, audited `analyze_start`. The job runs in the pipeline (`hk_pipeline::
//!   synth::jobs`) behind [`AnalyzeControl`]; this module validates, resolves the target to a band
//!   and a window, and serves the job's routes. `GET /api/analyze[?state=]` lists jobs newest
//!   first, `GET /api/analyze/{id}` is one, `DELETE /api/analyze/{id}` cancels a queued or running
//!   job or forgets a finished one (audited `analyze_cancel`), and `GET /api/analyze/{id}/trace`
//!   is ADR-0021 §4.2's filtered trace fetch. The stream is `/ws/analyze/{id}`.
//! - **The emitter's persisted analysis** (T-546): a bare `{"emitter_id"}` — no job field —
//!   answers `200` with the emitter's latest analysis: the chosen demod + decode pipeline, the
//!   per-stage evidence it rests on, the ADR-0021 trace of what else was considered, and a sealed
//!   `Resolution` when nothing was fully resolved. It reads the `emitter_synthesis` row the
//!   pipeline wrote (ADR-0015 §5.4); it starts no DSP and is answered synchronously. This is the
//!   T-546 contract the acceptance suite reads, kept as it was.
//!
//! **`not-searched` is not `unknown`** (ADR-0021 §7A.4). An emitter no analysis has run on
//! answers `200` with `resolution.kind: "not-searched"` and a null `pipeline` — *un-looked-at*,
//! which is a different fact from *looked at and found nothing*. A job that failed or was
//! cancelled carries the same `not-searched`: it ruled nothing out.
//!
//! **`404` is not `410`** (ADR-0021 §4.2). An id this server never issued is `404 not_found`; a
//! job that ran and has since been forgotten (only the last fifty finished are kept) is `410
//! gone`. *We forgot* is not *it never ran*.
//!
//! # Endpoint
//!
//! | Method | Path | Body / query | Answers |
//! |---|---|---|---|
//! | POST | `/api/analyze` | `{"emitter_id"}` | `200 {"emitter_id", "engine", "t", "verdict", "stage_reached", "pipeline", "evidence", "trace", "resolution", "receiver"}` |
//! | POST | `/api/analyze` | a target + job fields | `202 {"job"}`, `Location` |
//! | GET | `/api/analyze` | `?state=` | `{"jobs": [AnalyzeJob]}` |
//! | GET | `/api/analyze/{id}` | – | `AnalyzeJob` |
//! | DELETE | `/api/analyze/{id}` | – | `{"job", "forgotten"}` |
//! | GET | `/api/analyze/{id}/trace` | `?stage=&outcome=&family=&tried=&limit=` | the trace |
//!
//! Errors: `400 invalid`, `404 not_found`, `409 outside_window`, `410 evicted` / `410 gone`,
//! `422 no_iq` / `422 power`, `503 busy` / `503 unavailable`. Messages never echo request values.

use std::sync::{MutexGuard, PoisonError};

use hk_model::repo::synthesis::Resolution;
use hk_model::{
    EmitterId, IdentityAccess, LifecycleState, RepoError, Repository, SelectionId, Timestamp,
};
use serde_json::{Map, Value, json};

use crate::control::{
    Applied, CtlRequest, CtlResponse, Fail, dispatch, number, only, refuse_route, text,
};
use crate::http::ApiState;

// ---------------------------------------------------------------------------------------------
// The control seam
// ---------------------------------------------------------------------------------------------

/// Which templates may seed a job (`templates` on `POST /api/analyze`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnalyzeTemplates {
    /// Only these ids, when given.
    pub only: Option<Vec<String>>,
    /// Never these.
    pub exclude: Vec<String>,
    /// No templates: open skeletons only.
    pub off: bool,
}

/// A validated job request, its target resolved to a band and an optional window.
#[derive(Clone, Debug, PartialEq)]
pub struct AnalyzeStart {
    /// The target as the caller gave it (echoed on the job).
    pub target: Value,
    /// The live emitter id, for an emitter target.
    pub emitter_id: Option<EmitterId>,
    /// Lower band edge, Hz.
    pub f_lo_hz: f64,
    /// Upper band edge, Hz.
    pub f_hi_hz: f64,
    /// Window start, Unix s.
    pub t_lo_s: Option<f64>,
    /// Window end, Unix s.
    pub t_hi_s: Option<f64>,
    /// Whether the caller named the window (an emitter's default window is not).
    pub window_explicit: bool,
    /// `quick`, `standard` or `deep`.
    pub profile: &'static str,
    /// Lowers the profile's wall backstop.
    pub max_wall_s: Option<f64>,
    /// `auto`, `ring` or `live`.
    pub source: &'static str,
    /// Live collection length, s.
    pub live_s: Option<f64>,
    /// Template filter.
    pub templates: AnalyzeTemplates,
    /// Attach results to an emitter when done.
    pub attach: bool,
}

/// The trace fetch's filters, as given (the implementation parses the enums).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnalyzeTraceQuery {
    /// `S0`…`S6`.
    pub stage: Option<String>,
    /// An outcome kind (`pruned_floor`, …).
    pub outcome: Option<String>,
    /// An `hk-mod@1` family.
    pub family: Option<String>,
    /// Tried (`true`) or not-tried (`false`) nodes only.
    pub tried: Option<bool>,
    /// At most this many nodes.
    pub limit: Option<usize>,
}

/// A refused analyze operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnalyzeFailure {
    /// HTTP status.
    pub status: u16,
    /// Machine code.
    pub code: String,
    /// Human text (never echoes a request value).
    pub message: String,
}

/// The region-analyze job manager, as `hk-api` sees it (implemented over
/// `hk_pipeline::synth::jobs::AnalyzeJobs` in `hk-cli`). JSON out, so this crate names nothing
/// from the pipeline.
pub trait AnalyzeControl: Send + Sync {
    /// Admits a job; the `AnalyzeJob` JSON.
    fn start(&self, request: &AnalyzeStart) -> Result<Value, AnalyzeFailure>;
    /// Jobs, newest first, optionally in one state (validated by the implementation).
    fn list(&self, state: Option<&str>) -> Result<Value, AnalyzeFailure>;
    /// One job.
    fn get(&self, id: &str) -> Result<Value, AnalyzeFailure>;
    /// Cancels or forgets; `(job, forgotten)`.
    fn cancel(&self, id: &str) -> Result<(Value, bool), AnalyzeFailure>;
    /// The filtered trace.
    fn trace(&self, id: &str, query: &AnalyzeTraceQuery) -> Result<Value, AnalyzeFailure>;
}

fn failure(f: AnalyzeFailure) -> Fail {
    let code = match f.code.as_str() {
        "invalid" => "invalid",
        "not_found" => "not_found",
        "outside_window" => "outside_window",
        "evicted" => "evicted",
        "gone" => "gone",
        "no_iq" => "no_iq",
        "power" => "power",
        "busy" => "busy",
        "unavailable" => "unavailable",
        _ => "failed",
    };
    Fail::new(f.status, code, f.message)
}

fn control(state: &ApiState) -> Result<&dyn AnalyzeControl, Fail> {
    state.analyze.as_deref().ok_or_else(|| {
        Fail::new(
            503,
            "unavailable",
            "region analysis is not available on this server",
        )
    })
}

// ---------------------------------------------------------------------------------------------
// The target
// ---------------------------------------------------------------------------------------------

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
        t_lo_s: Option<f64>,
        t_hi_s: Option<f64>,
    },
}

const TARGET_FIELDS: &[&str] = &["selection_id", "emitter_id", "band"];
/// The fields that make a request a job (ADR-0015 §5.1).
const JOB_FIELDS: &[&str] = &[
    "profile",
    "max_wall_s",
    "source",
    "live_s",
    "templates",
    "attach",
];
const BAND_FIELDS: &[&str] = &["f_lo", "f_hi", "t_lo", "t_hi"];
const TEMPLATE_FIELDS: &[&str] = &["only", "exclude", "off"];

fn parse_target(body: &Map<String, Value>) -> Result<Target, Fail> {
    let allowed: Vec<&str> = TARGET_FIELDS.iter().chain(JOB_FIELDS).copied().collect();
    only(body, &allowed)?;
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
            match (t_lo_s, t_hi_s) {
                (None, None) => {}
                (Some(lo), Some(hi)) if lo < hi => {}
                (Some(_), Some(_)) => {
                    return Err(Fail::invalid("band needs t_lo < t_hi (Unix s)"));
                }
                _ => return Err(Fail::invalid("band needs both t_lo and t_hi, or neither")),
            }
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

/// Whether the body asks for a job rather than the emitter's persisted analysis.
fn wants_job(body: &Map<String, Value>) -> bool {
    !body.contains_key("emitter_id") || JOB_FIELDS.iter().any(|k| body.contains_key(*k))
}

fn one_of(
    body: &Map<String, Value>,
    key: &str,
    names: &[&'static str],
) -> Result<Option<&'static str>, Fail> {
    match text(body, key)?.flatten() {
        None => Ok(None),
        Some(v) => names
            .iter()
            .find(|n| **n == v)
            .copied()
            .map(Some)
            .ok_or_else(|| Fail::invalid(format!("{key} must be one of {}", names.join(", ")))),
    }
}

fn strings(body: &Map<String, Value>, key: &str) -> Result<Option<Vec<String>>, Fail> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| {
                v.as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| Fail::invalid(format!("{key} must be an array of template ids")))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(Fail::invalid(format!(
            "{key} must be an array of template ids"
        ))),
    }
}

/// The job fields, validated.
struct JobOptions {
    profile: &'static str,
    max_wall_s: Option<f64>,
    source: &'static str,
    live_s: Option<f64>,
    templates: AnalyzeTemplates,
    attach: bool,
}

fn parse_options(body: &Map<String, Value>) -> Result<JobOptions, Fail> {
    let profile = one_of(body, "profile", &["quick", "standard", "deep"])?.unwrap_or("standard");
    let source = one_of(body, "source", &["auto", "ring", "live"])?.unwrap_or("auto");
    let max_wall_s = number(body, "max_wall_s")?;
    if max_wall_s.is_some_and(|w| w <= 0.0) {
        return Err(Fail::invalid("max_wall_s must be positive"));
    }
    let live_s = number(body, "live_s")?;
    if live_s.is_some_and(|l| !(l > 0.0 && l <= 30.0)) {
        return Err(Fail::invalid("live_s must be in (0, 30] s"));
    }
    let attach = match body.get("attach") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(Fail::invalid("attach must be a boolean")),
    };
    let templates = match body.get("templates") {
        None | Some(Value::Null) => AnalyzeTemplates::default(),
        Some(Value::Object(t)) => {
            only(t, TEMPLATE_FIELDS)?;
            AnalyzeTemplates {
                only: strings(t, "only")?,
                exclude: strings(t, "exclude")?.unwrap_or_default(),
                off: match t.get("off") {
                    None | Some(Value::Null) => false,
                    Some(Value::Bool(b)) => *b,
                    Some(_) => return Err(Fail::invalid("templates.off must be a boolean")),
                },
            }
        }
        Some(_) => {
            return Err(Fail::invalid(
                "templates must be {\"only\"?, \"exclude\"?, \"off\"?}",
            ));
        }
    };
    Ok(JobOptions {
        profile,
        max_wall_s,
        source,
        live_s,
        templates,
        attach,
    })
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

fn repo_fail(what: &'static str, not_found: &'static str) -> impl Fn(RepoError) -> Fail {
    move |e| match e {
        RepoError::NotFound { .. } => Fail::new(404, "not_found", not_found),
        RepoError::Invalid(m) => Fail::invalid(m),
        other => Fail::new(500, "failed", format!("{what}: {other}")),
    }
}

fn secs(t: Timestamp) -> f64 {
    t.as_unix_nanos() as f64 / 1e9
}

/// A band and a window: `(f_lo, f_hi, t_lo, t_hi, explicit, live emitter)`.
type Resolved = (f64, f64, Option<f64>, Option<f64>, bool, Option<EmitterId>);

/// Resolves a target to what the job reads (ADR-0015 §5.3). An emitter's default window is its
/// span of appearances — not explicit, so an `auto` job falls back to live once the ring no longer
/// holds it; a selection's or band's window is the caller's.
fn resolve_target(state: &ApiState, target: &Target) -> Result<Resolved, Fail> {
    match *target {
        Target::Selection(id) => {
            let s = selections_store(state)?
                .selection(id)
                .map_err(repo_fail("selection store", "no such selection"))?;
            let (t_lo, t_hi) = (s.t_lo.map(secs), s.t_hi.map(secs));
            Ok((s.f_lo_hz, s.f_hi_hz, t_lo, t_hi, t_lo.is_some(), None))
        }
        Target::Emitter(id) => {
            let repo = inventory_store(state)?;
            let fail = repo_fail("inventory store", "no such inventory entry");
            let live = repo.live_emitter_id(id).map_err(&fail)?;
            // T-860: a user-deleted entry is out of the inventory, and a user delete wins — a job
            // may not analyse it, attach to it or confirm it. `404`, as every mutating inventory
            // route answers for a deleted entry.
            if repo.emitter_lifecycle_state(live).map_err(&fail)? == LifecycleState::Deleted {
                return Err(Fail::new(404, "not_found", "no such inventory entry"));
            }
            let e = repo
                .emitter_with_access(live, IdentityAccess::Standard)
                .map_err(&fail)?
                .emitter;
            // A zero-width measurement still names a channel: give it a floor rather than refuse.
            let half = e.bandwidth_hz.max(1_000.0) / 2.0;
            // A single-instant appearance has no window to read, so it goes live.
            let (first, last) = (secs(e.first_seen), secs(e.last_seen));
            let (t_lo, t_hi) = if last > first {
                (Some(first), Some(last))
            } else {
                (None, None)
            };
            Ok((
                (e.f_center_hz - half).max(0.0),
                e.f_center_hz + half,
                t_lo,
                t_hi,
                false,
                Some(live),
            ))
        }
        Target::Band {
            f_lo_hz,
            f_hi_hz,
            t_lo_s,
            t_hi_s,
        } => Ok((f_lo_hz, f_hi_hz, t_lo_s, t_hi_s, t_lo_s.is_some(), None)),
    }
}

/// Existence of `id`, resolving a merged emitter to the live entity like `/api/inventory/{id}`.
fn emitter_exists(state: &ApiState, id: EmitterId) -> Result<(), Fail> {
    let repo = inventory_store(state)?;
    let fail = repo_fail("inventory store", "no such inventory entry");
    let live = repo.live_emitter_id(id).map_err(&fail)?;
    repo.emitter_with_access(live, IdentityAccess::Standard)
        .map(|_| ())
        .map_err(&fail)
}

/// The emitter's latest analysis, or the `not-searched` answer when none has run.
///
/// **The distinction is the point.** A `None` here is never served as "unknown": an emitter
/// nothing looked at and an emitter a finished search could not identify are different findings,
/// and collapsing them is the defect ADR-0021 §7A.4 names.
fn analysis(state: &ApiState, id: EmitterId) -> Result<Value, Fail> {
    let repo = inventory_store(state)?;
    let fail = repo_fail("inventory store", "no such inventory entry");
    let live = repo.live_emitter_id(id).map_err(&fail)?;
    Ok(match repo.synthesis(live).map_err(&fail)? {
        Some(row) => serde_json::to_value(&row)
            .map_err(|e| Fail::new(500, "failed", format!("serialising the analysis: {e}")))?,
        None => json!({
            "emitter_id": live.to_string(),
            "pipeline": Value::Null,
            "evidence": [],
            "trace": [],
            "resolution": serde_json::to_value(Resolution::not_searched()).map_err(|e| {
                Fail::new(500, "failed", format!("serialising the resolution: {e}"))
            })?,
        }),
    })
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

fn apply_post(state: &ApiState, body: &Map<String, Value>) -> Result<Applied, Fail> {
    let target = parse_target(body)?;
    if !wants_job(body)
        && let Target::Emitter(id) = target
    {
        emitter_exists(state, id)?;
        let body = analysis(state, id)?;
        // A read dressed as a control route: the target is audited, and nothing changes.
        return Ok(crate::control::ok(body, Value::Null, Value::Null));
    }
    let options = parse_options(body)?;
    let (f_lo_hz, f_hi_hz, t_lo_s, t_hi_s, window_explicit, emitter_id) =
        resolve_target(state, &target)?;
    let echo: Map<String, Value> = body
        .iter()
        .filter(|(k, _)| TARGET_FIELDS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let start = AnalyzeStart {
        target: Value::Object(echo),
        emitter_id,
        f_lo_hz,
        f_hi_hz,
        t_lo_s,
        t_hi_s,
        window_explicit,
        profile: options.profile,
        max_wall_s: options.max_wall_s,
        source: options.source,
        live_s: options.live_s,
        templates: options.templates,
        attach: options.attach,
    };
    let job = control(state)?.start(&start).map_err(failure)?;
    Ok(Applied {
        status: 202,
        new: json!({ "id": job["id"], "state": job["state"] }),
        body: json!({ "job": job }),
        old: Value::Null,
    })
}

fn apply_delete(state: &ApiState, id: &str, body: &Map<String, Value>) -> Result<Applied, Fail> {
    crate::control::no_fields(body)?;
    let (job, forgotten) = control(state)?.cancel(id).map_err(failure)?;
    Ok(crate::control::ok(
        json!({ "job": job, "forgotten": forgotten }),
        Value::Null,
        json!({ "id": job["id"], "state": job["state"], "forgotten": forgotten }),
    ))
}

fn param<'a>(query: &'a [(String, String)], name: &str) -> Option<&'a str> {
    query
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

fn only_params(query: &[(String, String)], allowed: &[&str]) -> Result<(), Fail> {
    match query
        .iter()
        .find(|(k, _)| k != "token" && !allowed.contains(&k.as_str()))
    {
        Some(_) => Err(Fail::invalid(format!(
            "unknown query parameter (allowed: {})",
            allowed.join(", ")
        ))),
        None => Ok(()),
    }
}

fn read_list(state: &ApiState, query: &[(String, String)]) -> Result<Value, Fail> {
    only_params(query, &["state"])?;
    control(state)?.list(param(query, "state")).map_err(failure)
}

fn read_trace(state: &ApiState, id: &str, query: &[(String, String)]) -> Result<Value, Fail> {
    only_params(query, &["stage", "outcome", "family", "tried", "limit"])?;
    let tried = match param(query, "tried") {
        None => None,
        Some("true") => Some(true),
        Some("false") => Some(false),
        Some(_) => return Err(Fail::invalid("tried must be true or false")),
    };
    let limit = match param(query, "limit") {
        None => None,
        Some(l) => Some(
            l.parse::<usize>()
                .map_err(|_| Fail::invalid("limit must be a positive integer"))?,
        ),
    };
    let q = AnalyzeTraceQuery {
        stage: param(query, "stage").map(str::to_owned),
        outcome: param(query, "outcome").map(str::to_owned),
        family: param(query, "family").map(str::to_owned),
        tried,
        limit,
    };
    control(state)?.trace(id, &q).map_err(failure)
}

// ---------------------------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum Action<'a> {
    Start,
    List,
    Get(&'a str),
    Cancel(&'a str),
    Trace(&'a str),
}

fn resolve<'a>(method: &str, path: &'a str) -> Option<Result<Action<'a>, Option<&'static str>>> {
    if path == "/api/analyze" {
        return Some(match method {
            "POST" => Ok(Action::Start),
            "GET" => Ok(Action::List),
            _ => Err(Some("GET, POST")),
        });
    }
    let rest = path.strip_prefix("/api/analyze/")?;
    if let Some(id) = rest.strip_suffix("/trace") {
        if id.is_empty() || id.contains('/') {
            return None;
        }
        return Some(if method == "GET" {
            Ok(Action::Trace(id))
        } else {
            Err(Some("GET"))
        });
    }
    if rest.is_empty() || rest.contains('/') {
        return None;
    }
    Some(match method {
        "GET" => Ok(Action::Get(rest)),
        "DELETE" => Ok(Action::Cancel(rest)),
        _ => Err(Some("GET, DELETE")),
    })
}

/// The audit action of a `POST /api/analyze`: `analyze` for the emitter read, `analyze_start`
/// for everything else (including a body that does not parse, which then answers `400`).
fn post_action(body: &[u8]) -> &'static str {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(m)) if !wants_job(&m) => "analyze",
        _ => "analyze_start",
    }
}

/// Routes `/api/analyze*`; `None` when `path` is not one.
pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    let action = match resolve(req.method, req.path)? {
        Ok(a) => a,
        Err(allow) => return Some(refuse_route(state, req, allow)),
    };
    let unused = |_: &ApiState, _: &Map<String, Value>| -> Result<Applied, Fail> {
        Err(Fail::new(500, "failed", "not a mutation"))
    };
    let query = req.query;
    Some(match action {
        Action::Start => dispatch(
            state,
            req,
            post_action(req.body),
            true,
            |_| Err(Fail::new(500, "failed", "not a read")),
            apply_post,
        ),
        Action::Cancel(id) => dispatch(
            state,
            req,
            "analyze_cancel",
            true,
            |_| Err(Fail::new(500, "failed", "not a read")),
            |s, b| apply_delete(s, id, b),
        ),
        Action::List => dispatch(
            state,
            req,
            "analyze",
            false,
            |s| read_list(s, query),
            unused,
        ),
        Action::Get(id) => dispatch(
            state,
            req,
            "analyze",
            false,
            |s| {
                only_params(query, &[])?;
                control(s)?.get(id).map_err(failure)
            },
            unused,
        ),
        Action::Trace(id) => dispatch(
            state,
            req,
            "analyze",
            false,
            |s| read_trace(s, id, query),
            unused,
        ),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;

    fn body(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn routes_resolve_with_methods() {
        assert_eq!(resolve("POST", "/api/analyze"), Some(Ok(Action::Start)));
        assert_eq!(resolve("GET", "/api/analyze"), Some(Ok(Action::List)));
        assert_eq!(resolve("PUT", "/api/analyze"), Some(Err(Some("GET, POST"))));
        assert_eq!(
            resolve("GET", "/api/analyze/a1"),
            Some(Ok(Action::Get("a1")))
        );
        assert_eq!(
            resolve("DELETE", "/api/analyze/a1"),
            Some(Ok(Action::Cancel("a1")))
        );
        assert_eq!(
            resolve("POST", "/api/analyze/a1"),
            Some(Err(Some("GET, DELETE")))
        );
        assert_eq!(
            resolve("GET", "/api/analyze/a1/trace"),
            Some(Ok(Action::Trace("a1")))
        );
        assert_eq!(
            resolve("DELETE", "/api/analyze/a1/trace"),
            Some(Err(Some("GET")))
        );
        assert_eq!(resolve("GET", "/api/analyzex"), None);
        assert_eq!(resolve("GET", "/api/analyze/a1/x"), None);
        assert_eq!(resolve("GET", "/api/analyze/"), None);
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
            expect_ok(json!({ "emitter_id": eid.to_string(), "profile": "quick" })),
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
            json!({ "band": { "f_lo": 1.0, "f_hi": 2.0, "t_lo": 1.0 } }),
            json!({ "band": { "f_lo": 1.0, "f_hi": 2.0, "t_lo": 2.0, "t_hi": 1.0 } }),
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
    fn a_bare_emitter_id_is_the_persisted_read_and_anything_more_is_a_job() {
        let e = EmitterId::new().to_string();
        assert!(!wants_job(&body(json!({ "emitter_id": e }))));
        assert_eq!(
            post_action(json!({ "emitter_id": e }).to_string().as_bytes()),
            "analyze"
        );
        for job in [
            json!({ "emitter_id": e, "profile": "quick" }),
            json!({ "emitter_id": e, "attach": true }),
            json!({ "band": { "f_lo": 1.0, "f_hi": 2.0 } }),
            json!({ "selection_id": "x" }),
        ] {
            assert!(wants_job(&body(job.clone())), "{job}");
            assert_eq!(post_action(job.to_string().as_bytes()), "analyze_start");
        }
        assert_eq!(post_action(b"not json"), "analyze_start");
    }

    #[test]
    fn job_options_validate() {
        let ok = parse_options(&body(json!({
            "profile": "deep", "source": "ring", "max_wall_s": 5.0, "live_s": 3.0,
            "templates": { "only": ["generic-fsk-framed"], "exclude": [], "off": false },
            "attach": false,
        })))
        .unwrap_or_else(|f| panic!("{}", f.response().body));
        assert_eq!((ok.profile, ok.source), ("deep", "ring"));
        assert_eq!(
            (ok.max_wall_s, ok.live_s, ok.attach),
            (Some(5.0), Some(3.0), false)
        );
        assert_eq!(
            ok.templates.only.as_deref(),
            Some(&["generic-fsk-framed".to_owned()][..])
        );
        let d = parse_options(&Map::new()).unwrap_or_else(|_| panic!());
        assert_eq!((d.profile, d.source, d.attach), ("standard", "auto", true));
        for bad in [
            json!({ "profile": "turbo" }),
            json!({ "profile": 3 }),
            json!({ "source": "file" }),
            json!({ "max_wall_s": 0.0 }),
            json!({ "live_s": 31.0 }),
            json!({ "live_s": 0.0 }),
            json!({ "attach": "yes" }),
            json!({ "templates": [] }),
            json!({ "templates": { "only": "x" } }),
            json!({ "templates": { "only": [""] } }),
            json!({ "templates": { "nope": 1 } }),
            json!({ "templates": { "off": 1 } }),
        ] {
            match parse_options(&body(bad.clone())) {
                Ok(_) => panic!("{bad} should be refused"),
                Err(f) => assert_eq!(f.response().status, 400, "{bad}"),
            }
        }
    }

    /// A recording fake: what the route handed the manager, and a canned job back.
    #[derive(Default)]
    struct Fake {
        started: Mutex<Vec<AnalyzeStart>>,
    }

    impl AnalyzeControl for Fake {
        fn start(&self, r: &AnalyzeStart) -> Result<Value, AnalyzeFailure> {
            self.started.lock().unwrap().push(r.clone());
            Ok(json!({ "id": "a1", "state": "queued", "href": "/api/analyze/a1" }))
        }
        fn list(&self, _: Option<&str>) -> Result<Value, AnalyzeFailure> {
            Ok(json!({ "jobs": [] }))
        }
        fn get(&self, _: &str) -> Result<Value, AnalyzeFailure> {
            Err(AnalyzeFailure {
                status: 410,
                code: "gone".into(),
                message: "forgotten".into(),
            })
        }
        fn cancel(&self, _: &str) -> Result<(Value, bool), AnalyzeFailure> {
            Ok((json!({ "id": "a1", "state": "cancelled" }), false))
        }
        fn trace(&self, _: &str, _: &AnalyzeTraceQuery) -> Result<Value, AnalyzeFailure> {
            Ok(json!({ "nodes": [] }))
        }
    }

    #[test]
    fn a_band_job_is_handed_to_the_manager_resolved_and_answers_202() {
        let fake = Arc::new(Fake::default());
        let state = ApiState {
            analyze: Some(Arc::clone(&fake) as Arc<dyn AnalyzeControl>),
            ..ApiState::default()
        };
        let a = apply_post(
            &state,
            &body(json!({
                "band": { "f_lo": 1.0e6, "f_hi": 2.0e6, "t_lo": 10.0, "t_hi": 12.0 },
                "profile": "quick", "live_s": 4.0,
            })),
        )
        .unwrap_or_else(|f| panic!("{}", f.response().body));
        assert_eq!(a.status, 202);
        assert_eq!(a.body["job"]["id"], json!("a1"));
        let got = fake.started.lock().unwrap().pop().unwrap();
        assert_eq!((got.f_lo_hz, got.f_hi_hz), (1.0e6, 2.0e6));
        assert_eq!((got.t_lo_s, got.t_hi_s), (Some(10.0), Some(12.0)));
        assert!(got.window_explicit);
        assert_eq!(
            (got.profile, got.source, got.live_s),
            ("quick", "auto", Some(4.0))
        );
        assert_eq!(
            got.target,
            json!({ "band": { "f_lo": 1.0e6, "f_hi": 2.0e6, "t_lo": 10.0, "t_hi": 12.0 } })
        );
        // Codes from the manager pass through by name.
        let ctl = control(&state).unwrap_or_else(|_| panic!("wired"));
        match ctl.get("a9").map_err(failure) {
            Err(f) => {
                let r = f.response();
                assert_eq!((r.status, r.body["code"].clone()), (410, json!("gone")));
            }
            Ok(_) => panic!(),
        }
    }

    #[test]
    fn without_a_job_manager_a_job_is_503_unavailable() {
        let state = ApiState::default();
        match apply_post(
            &state,
            &body(json!({ "band": { "f_lo": 1.0, "f_hi": 2.0 } })),
        ) {
            Ok(_) => panic!("expected 503"),
            Err(f) => {
                let r = f.response();
                assert_eq!(r.status, 503);
                assert_eq!(r.body["code"], json!("unavailable"));
            }
        }
    }
}
