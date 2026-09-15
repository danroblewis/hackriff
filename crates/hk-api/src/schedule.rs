//! Scheduler routes (T-127, ADR-0012 §5.5, §8), documented in `docs/api.md` "Attention
//! scheduler":
//!
//! - `GET /api/scheduler[?f_lo&f_hi][&t0&t1][&tau_s]`: tier shares over the sweep-floor window,
//!   floor status, the bandit summary (provider version, counters, settings), active leases, and
//!   POI + coverage gaps per region computed from the **observation log** (T-115) through
//!   `visits_from_records`. Regions default to the plan's; the span defaults to the hour before the
//!   scheduler's sample-clock now.
//! - `GET /api/scheduler/arms`: the bandit arm table.
//! - `GET /api/scheduler/leases`, `POST /api/scheduler/leases` (create or update a pin),
//!   `DELETE /api/scheduler/leases/<id>` (release). Mutations are audited.
//!
//! A run without the scheduler answers the reads with `"scheduler": null` (POI from the log is
//! still reported when a span is given) and refuses lease changes with 409.

use std::sync::Arc;

use hk_core::scheduler::bandit::{
    ArmStatus, AttentionStatus, BanditCounters, DEFAULT_POI_TAUS_S, Lease, RegionPoi, region_poi,
    visits_from_records,
};
use hk_model::attention::observation::{LeaseKind, ObservationRecord};
use hk_model::{FreqRange, TimeRange, Timestamp};
use hk_store::observation::{MAX_RECORD_LIMIT, ObservationStore, RecordQuery};
use serde_json::{Map, Value, json};

use crate::control::{Applied, CtlRequest, CtlResponse, Fail, dispatch, refuse_route};
use crate::http::ApiState;

/// Most regions one `/api/scheduler` call reports POI for.
pub const MAX_POI_REGIONS: usize = 16;
/// Most burst durations accepted in `tau_s`.
pub const MAX_POI_TAUS: usize = 16;
/// Most observation records read for one POI computation (`poi_truncated` beyond).
pub const MAX_POI_RECORDS: usize = 200_000;
/// Default POI span before the scheduler's now, s.
pub const DEFAULT_POI_SPAN_S: f64 = 3600.0;
/// POI cell width, Hz.
pub const POI_CELL_HZ: f64 = 1e6;

/// The scheduler state the routes read (a snapshot the pipeline's control thread publishes).
#[derive(Clone, Debug)]
pub struct SchedulerView {
    /// Tier shares, floor status and the bandit summary.
    pub status: AttentionStatus,
    /// The plan's regions (POI defaults).
    pub regions: Vec<FreqRange>,
    /// Active leases.
    pub leases: Vec<Lease>,
    /// Plan version.
    pub plan_version: u32,
}

/// Why a scheduler command failed.
#[derive(Clone, Debug, PartialEq)]
pub enum SchedulerFail {
    /// This run has no scheduler.
    NoScheduler,
    /// The control thread did not answer in time.
    Busy,
    /// The scheduler refused it (capability, table full).
    Refused(String),
}

/// The pipeline's scheduler as the API sees it (implemented over the control thread's hub).
pub trait SchedulerControl: Send + Sync {
    /// The latest snapshot; `None` when this run has no scheduler.
    fn view(&self) -> Option<SchedulerView>;
    /// The arm table; `None` when this run has no scheduler.
    fn arms(&self) -> Option<Vec<ArmStatus>>;
    /// Adds or updates a lease (`id` 0 assigns one). Returns the lease as scheduled.
    fn add_lease(&self, lease: Lease) -> Result<Lease, SchedulerFail>;
    /// Releases a lease. Returns whether it was active.
    fn release_lease(&self, id: u64) -> Result<bool, SchedulerFail>;
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Action {
    Status,
    Arms,
    Leases,
    CreateLease,
    ReleaseLease(u64),
}

impl Action {
    fn name(self) -> &'static str {
        match self {
            Action::Status => "scheduler_status",
            Action::Arms => "scheduler_arms",
            Action::Leases => "scheduler_leases",
            Action::CreateLease => "scheduler_lease_create",
            Action::ReleaseLease(_) => "scheduler_lease_release",
        }
    }

    fn mutating(self) -> bool {
        matches!(self, Action::CreateLease | Action::ReleaseLease(_))
    }
}

type Resolved = Result<Action, Option<&'static str>>;

fn resolve(method: &str, path: &str) -> Option<Resolved> {
    let r = match path {
        "/api/scheduler" => match method {
            "GET" => Ok(Action::Status),
            _ => Err(Some("GET")),
        },
        "/api/scheduler/arms" => match method {
            "GET" => Ok(Action::Arms),
            _ => Err(Some("GET")),
        },
        "/api/scheduler/leases" => match method {
            "GET" => Ok(Action::Leases),
            "POST" => Ok(Action::CreateLease),
            _ => Err(Some("GET, POST")),
        },
        _ => {
            let id = path.strip_prefix("/api/scheduler/leases/")?;
            match (method, id.parse::<u64>()) {
                ("DELETE", Ok(id)) if id > 0 => Ok(Action::ReleaseLease(id)),
                ("DELETE", _) => Err(None),
                _ => Err(Some("DELETE")),
            }
        }
    };
    Some(r)
}

pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    let action = match resolve(req.method, req.path)? {
        Ok(a) => a,
        Err(Some(allow)) => return Some(refuse_route(state, req, Some(allow))),
        Err(None) => {
            return Some(Fail::invalid("lease id must be a positive integer").response());
        }
    };
    Some(dispatch(
        state,
        req,
        action.name(),
        action.mutating(),
        |s| read(s, req, action),
        |s, body| apply(s, action, body),
    ))
}

fn control(state: &ApiState) -> Option<&Arc<dyn SchedulerControl>> {
    state.scheduler.as_ref()
}

fn view(state: &ApiState) -> Option<SchedulerView> {
    control(state).and_then(|c| c.view())
}

fn read(state: &ApiState, req: &CtlRequest<'_>, action: Action) -> Result<Value, Fail> {
    match action {
        Action::Status => status(state, req),
        Action::Arms => {
            let arms = control(state).and_then(|c| c.arms());
            Ok(json!({
                "scheduler": arms.is_some(),
                "bandit": view(state).is_some_and(|v| v.status.bandit.is_some()),
                "arms": arms.unwrap_or_default().iter().map(arm_json).collect::<Vec<_>>(),
            }))
        }
        Action::Leases => {
            let v = view(state);
            Ok(json!({
                "scheduler": v.is_some(),
                "leases": v.map(|v| v.leases.iter().map(lease_json).collect::<Vec<_>>())
                    .unwrap_or_default(),
            }))
        }
        Action::CreateLease | Action::ReleaseLease(_) => unreachable!("mutating"),
    }
}

fn fail(e: SchedulerFail) -> Fail {
    match e {
        SchedulerFail::NoScheduler => Fail::new(409, "no_scheduler", "this run has no scheduler"),
        SchedulerFail::Busy => Fail::new(503, "busy", "the scheduler did not answer in time"),
        SchedulerFail::Refused(m) => Fail::invalid(m),
    }
}

fn apply(state: &ApiState, action: Action, body: &Map<String, Value>) -> Result<Applied, Fail> {
    let ctl = control(state).ok_or_else(|| fail(SchedulerFail::NoScheduler))?;
    match action {
        Action::CreateLease => {
            let lease = lease_from(body)?;
            let old = view(state)
                .and_then(|v| {
                    v.leases
                        .iter()
                        .find(|l| l.id == lease.id && lease.id > 0)
                        .copied()
                })
                .map_or(Value::Null, |l| lease_json(&l));
            let scheduled = ctl.add_lease(lease).map_err(fail)?;
            let new = lease_json(&scheduled);
            Ok(Applied {
                status: if old.is_null() { 201 } else { 200 },
                body: json!({ "lease": new }),
                old,
                new,
            })
        }
        Action::ReleaseLease(id) => {
            let old = view(state)
                .and_then(|v| v.leases.iter().find(|l| l.id == id).copied())
                .map_or(Value::Null, |l| lease_json(&l));
            if !ctl.release_lease(id).map_err(fail)? {
                return Err(Fail::new(404, "not_found", "no such active lease"));
            }
            Ok(Applied {
                status: 200,
                body: json!({ "released": id }),
                old,
                new: Value::Null,
            })
        }
        _ => unreachable!("reads"),
    }
}

fn lease_from(body: &Map<String, Value>) -> Result<Lease, Fail> {
    for key in body.keys() {
        if !matches!(key.as_str(), "id" | "kind" | "center_hz" | "duration_s") {
            return Err(Fail::invalid(format!("unknown field {key}")));
        }
    }
    let id = match body.get("id") {
        None => 0,
        Some(v) => v
            .as_u64()
            .filter(|&id| id > 0)
            .ok_or_else(|| Fail::invalid("id must be a positive integer"))?,
    };
    let kind = match body.get("kind") {
        None => LeaseKind::UserPin,
        Some(v) => serde_json::from_value::<LeaseKind>(v.clone())
            .map_err(|_| Fail::invalid("kind must be a lease kind (e.g. \"user-pin\")"))?,
    };
    let center_hz = body
        .get("center_hz")
        .and_then(Value::as_f64)
        .filter(|f| f.is_finite() && *f > 0.0)
        .ok_or_else(|| Fail::invalid("center_hz is required (Hz)"))?;
    let duration_ns = match body.get("duration_s") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_f64()
                .filter(|s| s.is_finite() && *s > 0.0 && *s < 9.2e9)
                .map(|s| (s * 1e9).round() as i64)
                .ok_or_else(|| Fail::invalid("duration_s must be positive seconds"))?,
        ),
    };
    Ok(Lease {
        id,
        kind,
        center_hz,
        // The pipeline runs every step at its own rate; it fills this in.
        rate_hz: 0.0,
        gains: None,
        duration_ns,
    })
}

fn secs(t: Timestamp) -> f64 {
    t.as_unix_nanos() as f64 / 1e9
}

fn ts(s: f64) -> Timestamp {
    Timestamp::from_unix_nanos((s * 1e9).round() as i64)
}

fn param<'a>(req: &'a CtlRequest<'_>, key: &str) -> Option<&'a str> {
    req.query
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn number(req: &CtlRequest<'_>, key: &str) -> Result<Option<f64>, Fail> {
    param(req, key)
        .map(|v| {
            v.parse::<f64>()
                .ok()
                .filter(|x| x.is_finite())
                .ok_or_else(|| Fail::invalid(format!("{key} must be a number")))
        })
        .transpose()
}

fn pair(req: &CtlRequest<'_>, a: &str, b: &str) -> Result<Option<(f64, f64)>, Fail> {
    match (number(req, a)?, number(req, b)?) {
        (None, None) => Ok(None),
        (Some(x), Some(y)) if y > x => Ok(Some((x, y))),
        (Some(_), Some(_)) => Err(Fail::invalid(format!("{b} must be greater than {a}"))),
        _ => Err(Fail::invalid(format!("{a} and {b} go together"))),
    }
}

fn taus(req: &CtlRequest<'_>) -> Result<Vec<f64>, Fail> {
    let Some(list) = param(req, "tau_s") else {
        return Ok(DEFAULT_POI_TAUS_S.to_vec());
    };
    let out: Vec<f64> = list
        .split(',')
        .map(|t| {
            t.trim()
                .parse::<f64>()
                .ok()
                .filter(|x| x.is_finite() && *x >= 0.0)
        })
        .collect::<Option<_>>()
        .ok_or_else(|| Fail::invalid("tau_s must be comma-separated non-negative seconds"))?;
    if out.is_empty() || out.len() > MAX_POI_TAUS {
        return Err(Fail::invalid(format!(
            "tau_s takes 1–{MAX_POI_TAUS} values"
        )));
    }
    Ok(out)
}

fn status(state: &ApiState, req: &CtlRequest<'_>) -> Result<Value, Fail> {
    let v = view(state);
    let explicit = pair(req, "f_lo", "f_hi")?;
    let span = pair(req, "t0", "t1")?;
    let taus = taus(req)?;
    let regions: Vec<FreqRange> = match (explicit, &v) {
        (Some((lo, hi)), _) => vec![FreqRange::new(lo, hi)],
        (None, Some(v)) => v.regions.iter().take(MAX_POI_REGIONS).copied().collect(),
        (None, None) => Vec::new(),
    };
    let span = match (span, &v) {
        (Some((a, b)), _) => Some(TimeRange::new(ts(a), ts(b))),
        (None, Some(v)) => Some(TimeRange::new(
            v.status
                .now
                .saturating_add_nanos(-(DEFAULT_POI_SPAN_S * 1e9) as i64),
            v.status.now,
        )),
        (None, None) => None,
    };
    let (poi, log, truncated) = match (&state.observations, span) {
        (Some(store), Some(span)) if !regions.is_empty() => {
            let (records, truncated) = records_in(store, &regions, span);
            let visits = visits_from_records(&records);
            let rows = regions
                .iter()
                .map(|r| {
                    poi_json(&region_poi(
                        &visits,
                        *r,
                        span,
                        POI_CELL_HZ,
                        &taus,
                        None,
                        None,
                    ))
                })
                .collect();
            (rows, true, truncated)
        }
        (store, _) => (Vec::new(), store.is_some(), false),
    };
    Ok(json!({
        "scheduler": v.as_ref().map(status_json),
        "leases": v.as_ref().map(|v| v.leases.iter().map(lease_json).collect::<Vec<_>>())
            .unwrap_or_default(),
        "observation_log": log,
        "span": span.map(|s| json!({ "t0": secs(s.start), "t1": secs(s.end) })),
        "poi": poi,
        "poi_truncated": truncated,
    }))
}

/// Records overlapping the regions' hull over `span`, with the geometries their sweeps reference.
fn records_in(
    store: &ObservationStore,
    regions: &[FreqRange],
    span: TimeRange,
) -> (Vec<ObservationRecord>, bool) {
    let lo = regions
        .iter()
        .map(|r| r.lo_hz)
        .fold(f64::INFINITY, f64::min);
    let hi = regions
        .iter()
        .map(|r| r.hi_hz)
        .fold(f64::NEG_INFINITY, f64::max);
    let mut out = Vec::new();
    let mut ids = Vec::new();
    let mut cursor = 0;
    loop {
        let page = store.query(&RecordQuery {
            freq: FreqRange::new(lo, hi),
            span,
            tier: None,
            cursor,
            limit: MAX_RECORD_LIMIT,
        });
        for g in page.geometries {
            if !ids.contains(&g.id) {
                ids.push(g.id);
                out.push(ObservationRecord::Geometry(g));
            }
        }
        out.extend(page.records);
        match page.next_cursor {
            Some(next) if out.len() < MAX_POI_RECORDS => cursor = next,
            Some(_) => return (out, true),
            None => return (out, false),
        }
    }
}

fn poi_json(p: &RegionPoi) -> Value {
    json!({
        "f_lo": p.region.lo_hz,
        "f_hi": p.region.hi_hz,
        "cell_hz": p.cell_hz,
        "cells": p.cells,
        "observed_cells": p.observed_cells,
        "observed_fraction": p.observed_fraction,
        "mean_revisit_s": p.mean_revisit_s,
        "poi": p.poi.iter().zip(&p.poi_min).map(|(e, min)| json!({
            "tau_s": e.tau_s,
            "p_poi": e.p_poi,
            "p_poi_min": min,
        })).collect::<Vec<_>>(),
        "gap_threshold_s": p.gap_threshold_s,
        "gaps": p.gaps.iter().map(|g| json!({
            "f_lo": g.freq.lo_hz,
            "f_hi": g.freq.hi_hz,
            "t0": secs(g.time.start),
            "t1": secs(g.time.end),
        })).collect::<Vec<_>>(),
        "gaps_truncated": p.gaps_truncated,
    })
}

fn counters_json(c: &BanditCounters) -> Value {
    json!({
        "repacks": c.repacks,
        "outcomes": c.outcomes,
        "outcomes_unmatched": c.outcomes_unmatched,
        "exploit_dwells": c.exploit_dwells,
        "explore_dwells": c.explore_dwells,
        "stale_forced": c.stale_forced,
        "beacon_dwells": c.beacon_dwells,
        "verifications_started": c.verifications_started,
        "verifications_dropped": c.verifications_dropped,
        "verifications_passed": c.verifications_passed,
        "verifications_failed": c.verifications_failed,
        "floor_deferrals": c.floor_deferrals,
        "arms_dropped": c.arms_dropped,
        "suspect_wasted_s": c.suspect_wasted_s,
    })
}

fn status_json(v: &SchedulerView) -> Value {
    let s = &v.status;
    json!({
        "now": secs(s.now),
        "plan_version": v.plan_version,
        "window_s": s.window_s,
        "shares_s": {
            "discovery": s.discovery_s,
            "exploit": s.exploit_s,
            "explore": s.explore_s,
            "other": s.other_s,
        },
        "sweep_floor": s.sweep_floor,
        "sweep_floor_met": s.sweep_floor_met,
        "floor_violations": s.floor_violations,
        "interactive": s.interactive,
        "leases": s.leases,
        "scheduled": s.scheduled,
        "low_power": s.low_power,
        "bandit": s.bandit.map(|b| json!({
            "provider_version": b.provider_version,
            "arms": b.arms,
            "active_arms": b.active_arms,
            "pending_verifications": b.pending_verifications,
            "banned": b.banned,
            "total_dwell_s": b.total_dwell_s,
            "config": b.config,
            "counters": counters_json(&b.counters),
        })),
    })
}

fn arm_json(a: &ArmStatus) -> Value {
    json!({
        "index": a.index,
        "key": { "rf_path": a.key.rf_path, "center_q": a.key.center_q, "rate_hz": a.key.rate_hz },
        "center_hz": a.center_hz,
        "rate_hz": a.rate_hz,
        "active": a.active,
        "exploration": a.exploration,
        "on_dc": a.on_dc,
        "prior": a.prior,
        "mean_reward": a.mean_reward,
        "dwell_s": a.dwell_s,
        "ucb": if a.ucb.is_finite() { json!(a.ucb) } else { json!("inf") },
        "visits": a.visits,
        "staleness_s": a.staleness_s,
        "suspect_fraction": a.suspect_fraction,
        "lead": a.lead.map(|k| format!("{k:016x}")),
        "members": a.members,
        "dwell_planned_s": a.dwell_planned_s,
        "required_revisit_s": a.required_revisit_s,
        "complete_capture": a.complete_capture,
        "last_reward": a.last_reward,
    })
}

fn lease_json(l: &Lease) -> Value {
    json!({
        "id": l.id,
        "kind": l.kind,
        "center_hz": l.center_hz,
        "rate_hz": l.rate_hz,
        "duration_s": l.duration_ns.map(|ns| ns as f64 / 1e9),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_resolve_with_methods() {
        assert_eq!(resolve("GET", "/api/scheduler"), Some(Ok(Action::Status)));
        assert_eq!(resolve("POST", "/api/scheduler"), Some(Err(Some("GET"))));
        assert_eq!(
            resolve("GET", "/api/scheduler/arms"),
            Some(Ok(Action::Arms))
        );
        assert_eq!(
            resolve("POST", "/api/scheduler/leases"),
            Some(Ok(Action::CreateLease))
        );
        assert_eq!(
            resolve("DELETE", "/api/scheduler/leases/7"),
            Some(Ok(Action::ReleaseLease(7)))
        );
        assert_eq!(
            resolve("DELETE", "/api/scheduler/leases/x"),
            Some(Err(None))
        );
        assert_eq!(
            resolve("GET", "/api/scheduler/leases/7"),
            Some(Err(Some("DELETE")))
        );
        assert_eq!(resolve("GET", "/api/schedulers"), None);
    }
}
