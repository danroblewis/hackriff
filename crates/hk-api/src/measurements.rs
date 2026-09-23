//! Saved measurements over HTTP (T-818 / MAP-18, docs/25 §4 and §10, ADR-0023): Δf, Δt,
//! bandwidth, duration, symbol rate and period as durable objects — value + unit + place + time +
//! provenance — in the run's user-metadata database ([`crate::ApiState::bookmarks`], beside
//! bookmarks and selections), audited and token-gated like the rest of the control API.
//!
//! # Endpoints
//!
//! | Method | Path | Body / query | Answers |
//! |---|---|---|---|
//! | GET | `/api/measurements` | `collection`?, `f_lo`/`f_hi`/`t0`/`t1`? (all four or none), `limit`? (500, max 2000), `cursor`? | `{measurements, count, matched, limit, next_cursor, window, collection}` |
//! | POST | `/api/measurements` | `{kind, cursors: [{f_hz, t_s}, {f_hz, t_s}], n?, note?, collection_id?, id?, view}` | the measurement (201); 409 when `id` exists |
//! | GET, PUT, DELETE | `/api/measurements/<id>` | PUT: `{cursors?, n?, note?, collection_id?, view?}` (`null` clears `n`/`note`/`collection_id`) | the measurement / `{"deleted": ...}` |
//!
//! **Cursors in, value out** (docs/25 §10.4): the body carries the place; the value, unit and
//! extent are computed server-side by [`hk_model::compute_measurement`], and a body carrying
//! `value` or `unit` (or any other computed field) is `400 invalid`. A `PUT` that moves a cursor
//! re-computes. `view` is the only provenance input, as for annotations (docs/25 §10.2); the
//! server stamps `actor` (the token fingerprint, never the token), `authored_s` (wall clock),
//! `authored: true` and the named device's `sample_rate_hz` when this run holds it.
//!
//! **Never detection input, never a device route.** Authoring is a view act: no audit entry
//! carries a `device` key (docs/25 §9, §10.5, §10.7).

use std::sync::{MutexGuard, PoisonError};

use hk_model::{
    MEASUREMENT_N_MAX, Measurement, MeasurementCursor, MeasurementFilter, MeasurementId,
    MeasurementKind, MeasurementProvenance, MeasurementTier, RepoError, Repository, Timestamp,
};
use serde_json::{Map, Value, json};

use crate::control::{
    Applied, CtlRequest, CtlResponse, Fail, dispatch, no_fields, ok, only, refuse_route, required,
    text,
};
use crate::http::ApiState;

/// `GET /api/measurements` default page size (docs/25 §10.3).
pub const DEFAULT_MEASUREMENTS_LIMIT: usize = 500;
/// `GET /api/measurements` largest page.
pub const MAX_MEASUREMENTS_LIMIT: usize = hk_model::MEASUREMENT_PAGE_MAX;
/// Largest offset accepted as a cursor.
const MAX_CURSOR: usize = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Action {
    List,
    Create,
    Get(MeasurementId),
    Update(MeasurementId),
    Delete(MeasurementId),
}

impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::List => "measurements_list",
            Self::Create => "measurement_create",
            Self::Get(_) => "measurement_get",
            Self::Update(_) => "measurement_update",
            Self::Delete(_) => "measurement_delete",
        }
    }

    fn mutating(self) -> bool {
        !matches!(self, Self::List | Self::Get(_))
    }
}

fn resolve(method: &str, path: &str) -> Option<Result<Action, Option<&'static str>>> {
    if path == "/api/measurements" {
        return Some(match method {
            "GET" => Ok(Action::List),
            "POST" => Ok(Action::Create),
            _ => Err(Some("GET, POST")),
        });
    }
    let rest = path.strip_prefix("/api/measurements/")?;
    let Ok(id) = rest.parse::<MeasurementId>() else {
        return Some(Err(None));
    };
    Some(match method {
        "GET" => Ok(Action::Get(id)),
        "PUT" => Ok(Action::Update(id)),
        "DELETE" => Ok(Action::Delete(id)),
        _ => Err(Some("GET, PUT, DELETE")),
    })
}

/// Routes a measurement request; `None` when `path` is not one.
pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    let action = match resolve(req.method, req.path)? {
        Ok(a) => a,
        Err(allow) => return Some(refuse_route(state, req, allow)),
    };
    let actor = req.caller.token_id.clone();
    Some(dispatch(
        state,
        req,
        action.name(),
        action.mutating(),
        |s| read(s, action, req.query),
        |s, body| apply(s, action, body, actor),
    ))
}

fn store(state: &ApiState) -> Result<MutexGuard<'_, Repository>, Fail> {
    state
        .bookmarks
        .as_ref()
        .map(|r| r.lock().unwrap_or_else(PoisonError::into_inner))
        .ok_or_else(|| Fail::new(503, "unavailable", "no measurement store on this server"))
}

fn repo_fail(e: RepoError) -> Fail {
    match e {
        RepoError::Invalid(m) => Fail::invalid(m),
        RepoError::NotFound { .. } => Fail::new(404, "not_found", "no such measurement"),
        other => Fail::new(500, "failed", format!("measurement store: {other}")),
    }
}

pub(crate) fn secs(t: Timestamp) -> f64 {
    t.as_unix_nanos() as f64 / 1e9
}

/// Unix seconds (capture clock) → timestamp.
pub(crate) fn from_secs(key: &str, s: f64) -> Result<Timestamp, Fail> {
    if !(s.is_finite() && s.abs() < 9.2e9) {
        return Err(Fail::invalid(format!(
            "{key} must be a finite Unix time in seconds"
        )));
    }
    Ok(Timestamp::from_unix_nanos((s * 1e9).round() as i64))
}

/// A measurement as the API serves it: seconds on the wire, capture-clock and wall-clock times
/// under distinct names.
pub fn measurement_json(m: &Measurement) -> Value {
    let p = &m.provenance;
    json!({
        "id": m.id.to_string(),
        "collection_id": m.collection_id,
        "kind": m.kind.as_str(),
        "value": m.value,
        "unit": m.unit,
        "basis": m.basis.as_str(),
        "f_lo_hz": m.f_lo_hz,
        "f_hi_hz": m.f_hi_hz,
        "t0_s": secs(m.t0),
        "t1_s": secs(m.t1),
        "cursors": m.cursors.iter()
            .map(|c| json!({ "f_hz": c.f_hz, "t_s": secs(c.t) }))
            .collect::<Vec<_>>(),
        "n": m.n,
        "note": m.note,
        "provenance": {
            "device_id": p.device_id,
            "center_hz": p.center_hz,
            "span_hz": p.span_hz,
            "sample_rate_hz": p.sample_rate_hz,
            "t_capture": [secs(p.t_capture[0]), secs(p.t_capture[1])],
            "tier": p.tier.as_str(),
            "authored_s": secs(p.authored_at),
            "actor": p.actor,
            "authored": p.authored,
        },
        "created_s": secs(m.created_at),
        "updated_s": secs(m.updated_at),
    })
}

pub(crate) fn query<'a>(q: &'a [(String, String)], key: &str) -> Option<&'a str> {
    q.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())
}

pub(crate) fn query_f64(q: &[(String, String)], key: &str) -> Result<Option<f64>, Fail> {
    query(q, key)
        .map(|v| {
            v.parse::<f64>()
                .ok()
                .filter(|x| x.is_finite())
                .ok_or_else(|| Fail::invalid(format!("{key} must be a finite number")))
        })
        .transpose()
}

fn read(state: &ApiState, action: Action, q: &[(String, String)]) -> Result<Value, Fail> {
    match action {
        Action::List => {
            let parts = [
                query_f64(q, "f_lo")?,
                query_f64(q, "f_hi")?,
                query_f64(q, "t0")?,
                query_f64(q, "t1")?,
            ];
            let window = match parts {
                [None, None, None, None] => None,
                [Some(f_lo), Some(f_hi), Some(t0), Some(t1)] => {
                    if f_hi < f_lo || t1 < t0 {
                        return Err(Fail::invalid("the window needs f_lo <= f_hi and t0 <= t1"));
                    }
                    Some((f_lo, f_hi, t0, t1))
                }
                _ => {
                    return Err(Fail::invalid(
                        "a window is f_lo, f_hi, t0 and t1 together, or none of them",
                    ));
                }
            };
            let collection = query(q, "collection")
                .map(|c| {
                    // Any typed id parses exactly the UUIDs; the collection id is one.
                    c.parse::<MeasurementId>()
                        .map(|_| c.to_owned())
                        .map_err(|_| Fail::invalid("collection must be a UUID"))
                })
                .transpose()?;
            let limit = match query(q, "limit") {
                None => DEFAULT_MEASUREMENTS_LIMIT,
                Some(v) => v
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (1..=MAX_MEASUREMENTS_LIMIT).contains(n))
                    .ok_or_else(|| {
                        Fail::invalid(format!(
                            "limit must be an integer in 1..={MAX_MEASUREMENTS_LIMIT}"
                        ))
                    })?,
            };
            let offset = match query(q, "cursor") {
                None => 0,
                Some(c) => c
                    .parse::<usize>()
                    .ok()
                    .filter(|&o| o <= MAX_CURSOR)
                    .ok_or_else(|| Fail::invalid("invalid cursor"))?,
            };
            let filter = MeasurementFilter {
                collection_id: collection.clone(),
                window: match window {
                    Some((lo, hi, t0, t1)) => {
                        Some((lo, hi, from_secs("t0", t0)?, from_secs("t1", t1)?))
                    }
                    None => None,
                },
            };
            let page = store(state)?
                .measurements_in(&filter, offset, limit)
                .map_err(repo_fail)?;
            let next = offset + page.rows.len();
            Ok(json!({
                "window": window.map(|(lo, hi, t0, t1)| json!({
                    "f_lo_hz": lo, "f_hi_hz": hi, "t0_s": t0, "t1_s": t1,
                })),
                "collection": collection,
                "measurements": page.rows.iter().map(measurement_json).collect::<Vec<_>>(),
                "count": page.rows.len(),
                "matched": page.matched,
                "limit": limit,
                "next_cursor": ((next as u64) < page.matched && next <= MAX_CURSOR)
                    .then(|| next.to_string()),
            }))
        }
        Action::Get(id) => store(state)?
            .measurement(id)
            .map(|m| measurement_json(&m))
            .map_err(repo_fail),
        _ => Err(Fail::new(500, "failed", "not a read")),
    }
}

const CREATE_FIELDS: &[&str] = &[
    "id",
    "kind",
    "cursors",
    "n",
    "note",
    "collection_id",
    "view",
];
const UPDATE_FIELDS: &[&str] = &["cursors", "n", "note", "collection_id", "view"];
/// Fields the server computes from the cursors (docs/25 §10.4).
const COMPUTED: &[&str] = &[
    "value", "unit", "basis", "f_lo_hz", "f_hi_hz", "t0_s", "t1_s",
];
/// Provenance fields only the server may write (docs/25 §10.2).
pub(crate) const SERVER_OWNED: &[&str] = &[
    "provenance",
    "author",
    "actor",
    "authored",
    "authored_s",
    "created_s",
    "updated_s",
];
const VIEW_FIELDS: &[&str] = &["center_hz", "span_hz", "t_capture", "tier", "device_id"];

/// Refuses a computed or server-owned field by name before the generic unknown-field check, so the
/// error says why rather than just "unknown".
fn refuse_server_owned(body: &Map<String, Value>) -> Result<(), Fail> {
    if let Some(k) = body.keys().find(|k| COMPUTED.contains(&k.as_str())) {
        return Err(Fail::invalid(format!(
            "{k} is computed by the server from the cursors and may not be supplied: send kind, \
             cursors and n"
        )));
    }
    let view_keys = body
        .get("view")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|m| m.keys());
    match body
        .keys()
        .chain(view_keys)
        .find(|k| SERVER_OWNED.contains(&k.as_str()))
    {
        Some(k) => Err(Fail::invalid(format!(
            "{k} is stamped by the server and may not be supplied: send only `view`, the view \
             context you were on"
        ))),
        None => Ok(()),
    }
}

fn kind(v: &str) -> Result<MeasurementKind, Fail> {
    serde_json::from_value(json!(v)).map_err(|_| {
        Fail::invalid(
            "kind must be \"delta_f\", \"delta_t\", \"bandwidth\", \"duration\", \
             \"symbol_rate\" or \"period\"",
        )
    })
}

fn cursors(v: &Value) -> Result<Vec<MeasurementCursor>, Fail> {
    const SHAPE: &str = "cursors must be [{f_hz, t_s}, {f_hz, t_s}] (Hz, capture-clock Unix s)";
    let arr = v.as_array().ok_or_else(|| Fail::invalid(SHAPE))?;
    if arr.len() != 2 {
        return Err(Fail::invalid(format!("{SHAPE}; got {} cursors", arr.len())));
    }
    arr.iter()
        .map(|c| {
            let m = c.as_object().ok_or_else(|| Fail::invalid(SHAPE))?;
            only(m, &["f_hz", "t_s"]).map_err(|_| Fail::invalid(SHAPE))?;
            let f_hz = required(m, "f_hz").map_err(|_| Fail::invalid(SHAPE))?;
            let t_s = required(m, "t_s").map_err(|_| Fail::invalid(SHAPE))?;
            Ok(MeasurementCursor {
                f_hz,
                t: from_secs("cursor t_s", t_s)?,
            })
        })
        .collect()
}

fn cycles(body: &Map<String, Value>) -> Result<Option<Option<u32>>, Fail> {
    match body.get("n") {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=u64::from(MEASUREMENT_N_MAX)).contains(n))
            .map(|n| Some(Some(n as u32)))
            .ok_or_else(|| {
                Fail::invalid(format!(
                    "n must be an integer in 1..={MEASUREMENT_N_MAX} (the cycle count the cursors \
                     span)"
                ))
            }),
    }
}

/// Parses `view` and stamps it into provenance: the client's view context plus what the server
/// knows authoritatively.
pub(crate) fn stamp(
    state: &ApiState,
    view: &Value,
    actor: Option<String>,
) -> Result<MeasurementProvenance, Fail> {
    let m = view.as_object().ok_or_else(|| {
        Fail::invalid(
            "view must be an object {center_hz, span_hz, t_capture: [t0, t1], tier, device_id?}",
        )
    })?;
    only(m, VIEW_FIELDS).map_err(|_| {
        Fail::invalid(format!(
            "view carries only {} (the server stamps the rest)",
            VIEW_FIELDS.join(", ")
        ))
    })?;
    let center_hz =
        required(m, "center_hz").map_err(|_| Fail::invalid("view.center_hz is required"))?;
    let span_hz = required(m, "span_hz").map_err(|_| Fail::invalid("view.span_hz is required"))?;
    let t_capture = match m
        .get("t_capture")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
    {
        Some([a, b]) => match (a.as_f64(), b.as_f64()) {
            (Some(a), Some(b)) => [
                from_secs("view.t_capture", a)?,
                from_secs("view.t_capture", b)?,
            ],
            _ => return Err(Fail::invalid("view.t_capture must be [t0_s, t1_s]")),
        },
        _ => {
            return Err(Fail::invalid(
                "view.t_capture is required: [t0_s, t1_s] on the capture clock",
            ));
        }
    };
    let tier: MeasurementTier = m
        .get("tier")
        .and_then(|t| serde_json::from_value(t.clone()).ok())
        .ok_or_else(|| {
            Fail::invalid(
                "view.tier must be \"live-iq\", \"spectrum-history\" or \"survey-overview\"",
            )
        })?;
    let device_id = text(m, "device_id")
        .map_err(|_| Fail::invalid("view.device_id must be a string or null"))?
        .flatten()
        .map(str::to_owned);
    // The named device's rate, only when this run holds it — never a guess for a device we cannot
    // see, and never the "primary" radio standing in for an unnamed pane.
    let sample_rate_hz = device_id.as_deref().and_then(|d| {
        state
            .live_controls
            .iter()
            .find(|l| l.device_id() == Some(d))
            .map(|l| l.tuning().sample_rate_hz)
    });
    Ok(MeasurementProvenance {
        device_id,
        center_hz,
        span_hz,
        sample_rate_hz,
        t_capture,
        tier,
        authored_at: Timestamp::now(),
        actor,
        authored: true,
    })
}

fn collection(body: &Map<String, Value>) -> Result<Option<Option<String>>, Fail> {
    // Checked as a UUID by `Measurement::validate` (400 otherwise).
    Ok(text(body, "collection_id")?.map(|c| c.map(|c| c.trim().to_owned())))
}

fn apply(
    state: &ApiState,
    action: Action,
    body: &Map<String, Value>,
    actor: Option<String>,
) -> Result<Applied, Fail> {
    match action {
        Action::Create => {
            refuse_server_owned(body)?;
            only(body, CREATE_FIELDS)?;
            let kind = kind(
                text(body, "kind")?
                    .flatten()
                    .ok_or_else(|| Fail::invalid("kind is required"))?,
            )?;
            let cursors = cursors(body.get("cursors").ok_or_else(|| {
                Fail::invalid("cursors is required: the two points the measurement spans")
            })?)?;
            let view = body.get("view").ok_or_else(|| {
                Fail::invalid("view is required: the view context the measurement was taken on")
            })?;
            let provenance = stamp(state, view, actor)?;
            let id = match text(body, "id")?.flatten() {
                Some(id) => id.parse().map_err(|_| Fail::invalid("id must be a UUID"))?,
                None => MeasurementId::new(),
            };
            let m = Measurement::new(
                id,
                kind,
                cursors,
                cycles(body)?.flatten(),
                collection(body)?.flatten(),
                text(body, "note")?.flatten().map(str::to_owned),
                provenance,
                Timestamp::now(),
            )
            .map_err(repo_fail)?;
            let mut repo = store(state)?;
            if repo.measurement(m.id).is_ok() {
                return Err(Fail::new(
                    409,
                    "conflict",
                    format!("measurement {} already exists; update it instead", m.id),
                ));
            }
            repo.insert_measurement(&m).map_err(repo_fail)?;
            let new = measurement_json(&m);
            Ok(Applied {
                status: 201,
                body: new.clone(),
                old: Value::Null,
                new,
            })
        }
        Action::Update(id) => {
            refuse_server_owned(body)?;
            only(body, UPDATE_FIELDS)?;
            let mut repo = store(state)?;
            let stored = repo.measurement(id).map_err(repo_fail)?;
            let mut next = stored.clone();
            if let Some(c) = body.get("cursors") {
                next.cursors = cursors(c)?;
            }
            if let Some(n) = cycles(body)? {
                next.n = n;
            }
            if let Some(note) = text(body, "note")? {
                next.note = note.map(str::to_owned);
            }
            if let Some(c) = collection(body)? {
                next.collection_id = c;
            }
            // A moved cursor (or a new n) is re-measured here, never taken from the client.
            next.recompute().map_err(repo_fail)?;
            // A new view re-stamps provenance (the edit was made from there, by this actor); no
            // view keeps the original stamp.
            if let Some(view) = body.get("view") {
                next.provenance = stamp(state, view, actor)?;
            }
            next.updated_at = Timestamp::now().max(stored.created_at);
            let saved = repo.update_measurement(&next).map_err(repo_fail)?;
            let new = measurement_json(&saved);
            Ok(ok(new.clone(), measurement_json(&stored), new))
        }
        Action::Delete(id) => {
            no_fields(body)?;
            let deleted = store(state)?.delete_measurement(id).map_err(repo_fail)?;
            let old = measurement_json(&deleted);
            Ok(ok(json!({ "deleted": old }), old, Value::Null))
        }
        Action::List | Action::Get(_) => Err(Fail::new(500, "failed", "not a mutating action")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::ROUTES;

    #[test]
    fn routes_resolve_with_methods() {
        let id = MeasurementId::new();
        assert_eq!(resolve("GET", "/api/measurements"), Some(Ok(Action::List)));
        assert_eq!(
            resolve("POST", "/api/measurements"),
            Some(Ok(Action::Create))
        );
        assert_eq!(
            resolve("DELETE", "/api/measurements"),
            Some(Err(Some("GET, POST")))
        );
        assert_eq!(
            resolve("PUT", &format!("/api/measurements/{id}")),
            Some(Ok(Action::Update(id)))
        );
        assert_eq!(
            resolve("POST", &format!("/api/measurements/{id}")),
            Some(Err(Some("GET, PUT, DELETE")))
        );
        assert_eq!(resolve("GET", "/api/measurements/nope"), Some(Err(None)));
        assert_eq!(resolve("GET", "/api/measurementsx"), None);
        assert_eq!(resolve("GET", "/api/selections"), None);
    }

    #[test]
    fn every_measurement_route_is_in_the_route_table() {
        let listed: Vec<_> = ROUTES
            .iter()
            .filter(|(_, p)| p.starts_with("/api/measurements"))
            .collect();
        assert_eq!(listed.len(), 5);
        for (method, path) in listed {
            let concrete = path.replace("{id}", &MeasurementId::new().to_string());
            assert!(
                matches!(resolve(method, &concrete), Some(Ok(_))),
                "{method} {path}"
            );
        }
    }

    #[test]
    fn a_supplied_value_or_provenance_is_refused_by_name() {
        for (body, needle) in [
            (json!({ "value": 1.0 }), "value"),
            (json!({ "unit": "Hz" }), "unit"),
            (json!({ "f_lo_hz": 1.0 }), "f_lo_hz"),
            (json!({ "provenance": {} }), "provenance"),
            (
                json!({ "view": { "center_hz": 1.0, "actor": "me" } }),
                "actor",
            ),
        ] {
            let m: Map<String, Value> = serde_json::from_value(body).unwrap();
            let e = refuse_server_owned(&m).unwrap_err().response();
            assert_eq!(e.status, 400);
            let msg = e.body["error"].as_str().unwrap_or_default().to_owned();
            assert!(msg.contains(needle), "{needle}: {msg}");
        }
        let m: Map<String, Value> =
            serde_json::from_value(json!({ "view": { "center_hz": 1.0 } })).unwrap();
        assert!(refuse_server_owned(&m).is_ok());
    }
}
