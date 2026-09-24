//! T-463: `/api/playback` validates, applies in order, audits, and is never a device action.

use std::sync::{Arc, Mutex};

use serde_json::json;

use super::*;
use crate::control::{AuditLog, Caller};

/// A playhead that records the changes it was asked for.
#[derive(Default)]
struct Fake {
    seen: Mutex<Vec<PlaybackChange>>,
    position: Mutex<Option<i64>>,
}

impl PlaybackControl for Fake {
    fn state(&self) -> Value {
        json!({"playhead": {"position_ns": *self.position.lock().unwrap()}, "playheads": 1,
               "analysis": "recorded"})
    }
    fn apply(&self, change: &PlaybackChange) -> Result<Value, PlaybackFailure> {
        self.seen.lock().unwrap().push(*change);
        if change.playing == Some(true)
            && change.t_ns.is_none()
            && self.position.lock().unwrap().is_none()
        {
            return Err(PlaybackFailure {
                status: 409,
                code: "no_position".into(),
                message: "the playhead has no position: seek to a past time first".into(),
            });
        }
        if let Some(t) = change.t_ns {
            *self.position.lock().unwrap() = Some(t);
        }
        Ok(self.state())
    }
}

fn state(fake: &Arc<Fake>) -> (ApiState, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "hk-api-t463-audit-{}-{:?}.jsonl",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_file(&path);
    let s = ApiState {
        playback: Some(Arc::clone(fake) as Arc<dyn PlaybackControl>),
        audit: Some(Arc::new(AuditLog::open(&path).unwrap())),
        ..ApiState::default()
    };
    (s, path)
}

fn call(state: &ApiState, method: &str, body: &str) -> (u16, Value) {
    let q: Vec<(String, String)> = Vec::new();
    let req = CtlRequest {
        method,
        path: "/api/playback",
        body: body.as_bytes(),
        content_type: Some("application/json"),
        caller: Caller::default(),
        query: &q,
    };
    let r = route(state, &req).expect("mine");
    (r.status, r.body)
}

#[test]
fn the_playhead_is_read_moved_played_and_paused_and_every_change_is_audited() {
    let fake = Arc::new(Fake::default());
    let (st, audit) = state(&fake);

    let (s, v) = call(&st, "GET", "");
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["playheads"], 1);
    assert_eq!(v["analysis"], "recorded");

    // Seconds are converted once to integer ns; the fields reach the playhead together.
    let (s, v) = call(
        &st,
        "POST",
        r#"{"t": 1789300800.5, "playing": true, "speed": 2}"#,
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        v["playhead"]["position_ns"],
        json!(1_789_300_800_500_000_000i64)
    );
    let (s, _) = call(&st, "POST", r#"{"t_ns": 1789300801000000000}"#);
    assert_eq!(s, 200);
    let (s, _) = call(&st, "POST", r#"{"playing": false}"#);
    assert_eq!(s, 200);
    let seen = fake.seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            PlaybackChange {
                t_ns: Some(1_789_300_800_500_000_000),
                playing: Some(true),
                speed: Some(2.0)
            },
            PlaybackChange {
                t_ns: Some(1_789_300_801_000_000_000),
                ..PlaybackChange::default()
            },
            PlaybackChange {
                playing: Some(false),
                ..PlaybackChange::default()
            },
        ]
    );

    // Audited as `playback`, and never as a device action: no `device` key.
    let log = std::fs::read_to_string(&audit).unwrap();
    let entries: Vec<Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(entries.len(), 3, "{log}");
    for e in &entries {
        assert_eq!(e["action"], "playback");
        assert!(
            e.get("device").is_none(),
            "playback never reaches a front end: {e}"
        );
    }
    let _ = std::fs::remove_file(&audit);
}

#[test]
fn malformed_changes_are_refused_before_the_playhead_sees_them() {
    let fake = Arc::new(Fake::default());
    let (st, audit) = state(&fake);
    for body in [
        "{}",
        r#"{"t": 1, "t_ns": 1}"#,
        r#"{"t": -1}"#,
        r#"{"t": 1e12}"#,
        r#"{"t_ns": -5}"#,
        r#"{"t_ns": 1.5}"#,
        r#"{"playing": "yes"}"#,
        r#"{"speed": "fast"}"#,
        r#"{"mode": "nbfm"}"#,
    ] {
        let (s, v) = call(&st, "POST", body);
        assert_eq!(s, 400, "{body}: {v}");
        assert_eq!(v["code"], "invalid", "{body}: {v}");
    }
    assert!(fake.seen.lock().unwrap().is_empty());
    // The playhead's own refusal passes through with its status and code.
    let (s, v) = call(&st, "POST", r#"{"playing": true}"#);
    assert_eq!((s, v["code"].as_str()), (409, Some("no_position")), "{v}");
    let (s, _) = call(&st, "DELETE", "");
    assert_eq!(s, 405);
    // No playback on this server: 503.
    let (s, v) = call(&ApiState::default(), "GET", "");
    assert_eq!((s, v["code"].as_str()), (503, Some("unavailable")), "{v}");
    let _ = std::fs::remove_file(&audit);
}
