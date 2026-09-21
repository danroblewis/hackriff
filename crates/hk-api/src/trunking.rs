//! Trunking load index over HTTP (T-273, AWARE-067): a derived view over the [`GrantEvent`]
//! stream — how busy a trunked system's control channel is — computed from channel-grant
//! **metadata** alone, documented in `docs/api.md` "Trunking load index".
//!
//! `GET /api/trunking/load?t0&t1[&system]`: for each known [`TrunkSystem`] (or just `system` when
//! given), counts the channel-grant events (`grant`, `grant-update`, `call-start` — see
//! [`GrantKind`]) whose `t` falls in `[t0, t1)`, and reports `grants`, `distinct_talkgroups` and
//! `grants_per_min`, so a busy system reads higher than a quiet one over the same window.
//!
//! **Metadata only.** This computes over [`GrantEvent`] — never [`hk_model::CallRecord`], never
//! any audio or vocoder frame — and the response carries counts and a talkgroup identifier count
//! only, never an audio or content field (asserted by this module's tests, and the actual
//! constraint the ticket cares about).
//!
//! No mutation, so unlike most of this crate's routes this one skips [`crate::control::dispatch`]
//! and reads the repository directly, the same shape as [`crate::occupancy`]. The index is
//! computed on demand from the stored event stream rather than accumulated: nothing here grows
//! without bound, and [`MAX_GRANTS_PER_QUERY`] bounds the one query per system a request makes.

use std::collections::BTreeSet;
use std::str::FromStr;
use std::sync::{MutexGuard, PoisonError};

use hk_model::{GrantEvent, GrantKind, Repository, Timestamp, TrunkSystemId};
use serde_json::{Value, json};

use crate::control::{CtlRequest, CtlResponse, Fail, refuse_route};
use crate::http::ApiState;

/// Rows read per system per request — bounded so the query itself cannot grow unbounded, even
/// though the underlying stream is append-only forever.
pub const MAX_GRANTS_PER_QUERY: u32 = 20_000;

pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    if req.path != "/api/trunking/load" {
        return None;
    }
    if req.method != "GET" {
        return Some(refuse_route(state, req, Some("GET")));
    }
    Some(match load(state, req.query) {
        Ok(body) => CtlResponse {
            status: 200,
            body,
            allow: None,
        },
        Err(f) => f.response(),
    })
}

fn repo(state: &ApiState) -> Result<MutexGuard<'_, Repository>, Fail> {
    state
        .trunking
        .as_ref()
        .map(|r| r.lock().unwrap_or_else(PoisonError::into_inner))
        .ok_or_else(|| Fail::new(503, "unavailable", "no trunking store on this server"))
}

fn param<'a>(q: &'a [(String, String)], key: &str) -> Option<&'a str> {
    q.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn num(q: &[(String, String)], key: &str) -> Result<f64, Fail> {
    param(q, key)
        .ok_or_else(|| Fail::invalid(format!("missing {key}")))?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .ok_or_else(|| Fail::invalid(format!("{key} must be a number")))
}

/// The channel-grant events that count as traffic. `denied`, `outside-window` and
/// `unmapped-channel` are logged (C23) but are not grants of a channel, and `call-end` closes
/// traffic already counted at its `call-start`/`grant`.
fn is_activity(kind: GrantKind) -> bool {
    matches!(
        kind,
        GrantKind::Grant | GrantKind::GrantUpdate | GrantKind::CallStart
    )
}

/// One system's load over the requested window.
fn system_load(events: &[GrantEvent], until: Timestamp, minutes: f64) -> Value {
    let mut grants = 0u32;
    let mut talkgroups: BTreeSet<&str> = BTreeSet::new();
    for g in events {
        if g.t > until || !is_activity(g.kind) {
            continue;
        }
        grants += 1;
        if let Some(tg) = g.talkgroup.as_deref() {
            talkgroups.insert(tg);
        }
    }
    json!({
        "grants": grants,
        "distinct_talkgroups": talkgroups.len(),
        "grants_per_min": if minutes > 0.0 { f64::from(grants) / minutes } else { 0.0 },
    })
}

fn load(state: &ApiState, q: &[(String, String)]) -> Result<Value, Fail> {
    let t0 = num(q, "t0")?;
    let t1 = num(q, "t1")?;
    if t1 <= t0 {
        return Err(Fail::invalid("need t0 < t1"));
    }
    let ns = |s: f64| Timestamp::from_unix_nanos((s * 1e9).clamp(-9.2e18, 9.2e18) as i64);
    let (since, until) = (ns(t0), ns(t1));
    let minutes = (t1 - t0) / 60.0;

    let repo = repo(state)?;
    let systems: Vec<TrunkSystemId> = match param(q, "system") {
        Some(s) => {
            let id =
                TrunkSystemId::from_str(s).map_err(|_| Fail::invalid("system must be a UUID"))?;
            vec![id]
        }
        None => repo
            .trunk_systems()
            .map_err(|e| Fail::new(500, "failed", format!("trunking store: {e}")))?
            .iter()
            .map(|s| s.id)
            .collect(),
    };

    let mut rows = Vec::with_capacity(systems.len());
    for id in systems {
        let events = repo
            .grants_for_system(id, since, MAX_GRANTS_PER_QUERY)
            .map_err(|e| Fail::new(500, "failed", format!("trunking store: {e}")))?;
        let mut row = system_load(&events, until, minutes);
        if let Some(o) = row.as_object_mut() {
            o.insert("system".into(), json!(id.to_string()));
        }
        rows.push(row);
    }

    Ok(json!({
        "window": { "t0": t0, "t1": t1 },
        "systems": rows,
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use hk_model::{GrantEvent, GrantKind, TrunkSystem};

    use super::*;
    use crate::control::Caller;

    fn ts(s: f64) -> Timestamp {
        Timestamp::from_unix_nanos((s * 1e9) as i64)
    }

    fn state_with(repo: Repository) -> ApiState {
        ApiState {
            trunking: Some(Arc::new(Mutex::new(repo))),
            ..ApiState::default()
        }
    }

    fn req<'a>(path: &'a str, query: &'a [(String, String)]) -> CtlRequest<'a> {
        CtlRequest {
            method: "GET",
            path,
            body: b"",
            content_type: None,
            caller: Caller::default(),
            query,
        }
    }

    fn q(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn grant(system: TrunkSystemId, t: f64, talkgroup: Option<&str>) -> GrantEvent {
        let mut g = GrantEvent::new(system, GrantKind::Grant, ts(t));
        g.talkgroup = talkgroup.map(str::to_string);
        g
    }

    /// AWARE-067: a busy system (many grants across several talkgroups) reads a higher load index
    /// than a quiet one over the same window, from the GrantEvent stream alone.
    #[test]
    fn busy_system_reads_higher_than_quiet() {
        let mut repo = Repository::open_in_memory().unwrap();
        let busy = TrunkSystem::new(hk_model::TrunkProtocol::P25Phase1, Some(1.0e8), ts(0.0));
        let quiet = TrunkSystem::new(hk_model::TrunkProtocol::P25Phase1, Some(2.0e8), ts(0.0));
        repo.put_trunk_system(&busy).unwrap();
        repo.put_trunk_system(&quiet).unwrap();

        for i in 0..20 {
            repo.append_grant(&grant(
                busy.id,
                10.0 + f64::from(i),
                Some(if i % 3 == 0 { "100" } else { "200" }),
            ))
            .unwrap();
        }
        repo.append_grant(&grant(quiet.id, 15.0, Some("300")))
            .unwrap();

        let state = state_with(repo);
        let query = q(&[("t0", "0"), ("t1", "60")]);
        let resp = route(&state, &req("/api/trunking/load", &query)).unwrap();
        assert_eq!(resp.status, 200, "{:?}", resp.body);

        let rows = resp.body["systems"].as_array().unwrap();
        let row = |id: TrunkSystemId| {
            rows.iter()
                .find(|r| r["system"] == id.to_string())
                .unwrap_or_else(|| panic!("no row for {id}: {rows:?}"))
        };
        let busy_row = row(busy.id);
        let quiet_row = row(quiet.id);
        assert_eq!(busy_row["grants"], 20);
        assert_eq!(busy_row["distinct_talkgroups"], 2);
        assert_eq!(quiet_row["grants"], 1);
        assert!(
            busy_row["grants_per_min"].as_f64().unwrap()
                > quiet_row["grants_per_min"].as_f64().unwrap(),
            "busy system must read a higher load index than a quiet one: {:?}",
            resp.body
        );
    }

    /// The hard boundary: no talkgroup audio or call content ever appears on this route, only
    /// counts. This is the ticket's actual constraint, and the one a future change could silently
    /// break by wiring in `CallRecord` or a decoded payload.
    #[test]
    fn no_audio_or_content_field() {
        let mut repo = Repository::open_in_memory().unwrap();
        let sys = TrunkSystem::new(hk_model::TrunkProtocol::P25Phase1, Some(1.0e8), ts(0.0));
        repo.put_trunk_system(&sys).unwrap();
        let mut g = grant(sys.id, 5.0, Some("100"));
        g.detail = json!({"opcode": "grant", "note": "no audio ever carried here"});
        repo.append_grant(&g).unwrap();

        let state = state_with(repo);
        let query = q(&[("t0", "0"), ("t1", "60")]);
        let resp = route(&state, &req("/api/trunking/load", &query)).unwrap();
        assert_eq!(resp.status, 200, "{:?}", resp.body);

        let body_text = resp.body.to_string();
        for banned in ["audio", "content", "vocoder", "payload", "bits", "pcm"] {
            assert!(
                !body_text.to_lowercase().contains(banned),
                "load index response must never carry {banned:?}: {body_text}"
            );
        }
        let row = &resp.body["systems"][0];
        let mut keys: Vec<&str> = row
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["distinct_talkgroups", "grants", "grants_per_min", "system"],
            "the route carries only the load index's own metadata fields"
        );
    }

    #[test]
    fn no_trunking_store_answers_503() {
        let state = ApiState::default();
        let query = q(&[("t0", "0"), ("t1", "60")]);
        let resp = route(&state, &req("/api/trunking/load", &query)).unwrap();
        assert_eq!(resp.status, 503);
    }

    #[test]
    fn bad_window_is_invalid() {
        let state = state_with(Repository::open_in_memory().unwrap());
        let query = q(&[("t0", "10"), ("t1", "10")]);
        let resp = route(&state, &req("/api/trunking/load", &query)).unwrap();
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn other_method_is_405() {
        let state = state_with(Repository::open_in_memory().unwrap());
        let mut r = req("/api/trunking/load", &[]);
        r.method = "POST";
        let resp = route(&state, &r).unwrap();
        assert_eq!(resp.status, 405);
    }

    #[test]
    fn unrelated_path_is_none() {
        let state = state_with(Repository::open_in_memory().unwrap());
        let query = q(&[]);
        assert!(route(&state, &req("/api/trunk-systems", &query)).is_none());
    }
}
