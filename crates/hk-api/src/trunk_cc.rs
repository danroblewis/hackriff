//! The control-channel hunt's last pass over HTTP (T-977), documented in `docs/api.md`
//! "Control-channel candidates".
//!
//! `GET /api/trunking/cc-candidates`: which raster channels the occupancy sweep chose, which ones
//! blind detection's own emitters put on the list, and **what each came to** — confirmed, frame
//! sync without a valid check, no sync at all, not demodulated, or refused by the per-pass
//! admission cap.
//!
//! Before this the only trace a rejected candidate left was a counter: `/api/status`'s
//! `chains.cc_demods 12, chains.cc_confirmed 0` says twelve channels were demodulated and none was
//! a control channel, and says nothing about which twelve or why each lost. A P25 C4FM emission the
//! run had already detected could sit in the inventory reading `family: unknown` while the chain
//! demodulated its channel and discarded the answer.
//!
//! **This is the ephemeral half.** The durable half is the `emitter_synthesis` row the same pass
//! writes for every channel it could file one against — served on `GET /api/inventory` as
//! `resolution` and on `POST /api/analyze` as the full trace — and that is where a client should
//! read a *signal's* verdict. This route exists for the channels no durable row can hold: one the
//! admission cap refused, one with no emitter at that frequency, one whose verdict has not changed
//! since the pass that first filed it. It is **one pass**, replaced each time: an accumulator over
//! a hunt that runs every half-second for the life of a run would grow without bound.
//!
//! **No pass is not an empty pass.** A run whose band never triggered a hunt answers `pass: null`,
//! not an empty channel list — un-looked-at and looked-at-and-found-nothing are different facts
//! (ADR-0021 §7A.4), and this route is about nothing else.
//!
//! Read-only, so it skips [`crate::control::dispatch`] like [`crate::trunking`] and
//! [`crate::vlf`]. The measuring lives in `hk_pipeline::chains::trunk`; this crate sees JSON
//! through [`CcHuntControl`] and never names the pipeline.

use serde_json::{Value, json};

use crate::control::{CtlRequest, CtlResponse, Fail, refuse_route};
use crate::http::ApiState;

/// The run's control-channel hunt, as the API sees it.
pub trait CcHuntControl: Send + Sync {
    /// The last completed pass, or `None` when no hunt has finished one on this run.
    fn last_pass(&self) -> Option<Value>;
}

pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    if req.path != "/api/trunking/cc-candidates" {
        return None;
    }
    if req.method != "GET" {
        return Some(refuse_route(state, req, Some("GET")));
    }
    Some(match last_pass(state) {
        Ok(body) => CtlResponse {
            status: 200,
            body,
            allow: None,
        },
        Err(f) => f.response(),
    })
}

fn last_pass(state: &ApiState) -> Result<Value, Fail> {
    let hunt = state.cc_hunt.as_ref().ok_or_else(|| {
        Fail::new(
            503,
            "unavailable",
            "no control-channel hunt on this server (no pipeline is running)",
        )
    })?;
    // `pass: null` is the answer of a run whose hunt has not completed a pass — the band never
    // triggered one, or the first window is still filling. It is deliberately not `{channels: []}`,
    // which would say the hunt looked and chose nothing.
    Ok(json!({ "pass": hunt.last_pass() }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::control::Caller;

    struct Hunt(Option<Value>);

    impl CcHuntControl for Hunt {
        fn last_pass(&self) -> Option<Value> {
            self.0.clone()
        }
    }

    fn req(method: &'static str) -> CtlRequest<'static> {
        CtlRequest {
            method,
            path: "/api/trunking/cc-candidates",
            body: b"",
            content_type: None,
            caller: Caller::default(),
            query: &[],
        }
    }

    fn state(hunt: Option<Hunt>) -> ApiState {
        ApiState {
            cc_hunt: hunt.map(|h| Arc::new(h) as Arc<dyn CcHuntControl>),
            ..ApiState::default()
        }
    }

    /// **A server with no pipeline says so, rather than serving an empty list.** An empty list is
    /// a measurement ("the hunt looked and chose nothing"); 503 is the absence of a measurer.
    #[test]
    fn no_hunt_is_503_and_never_an_empty_channel_list() {
        let r = route(&state(None), &req("GET")).unwrap();
        assert_eq!(r.status, 503);
        assert_eq!(r.body["code"], "unavailable");
    }

    /// **A hunt that has completed no pass answers `pass: null`, not an empty pass.** Not-yet-run
    /// and ran-and-found-nothing are different states and neither may be rendered as the other
    /// (ADR-0021 §7A.4).
    #[test]
    fn an_unrun_hunt_answers_pass_null_rather_than_an_empty_pass() {
        let r = route(&state(Some(Hunt(None))), &req("GET")).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body["pass"], Value::Null);
    }

    /// The pass the hunt reports is served verbatim: this route renders nothing and hides nothing.
    #[test]
    fn a_completed_pass_is_served_whole() {
        let pass = json!({
            "pass": 3,
            "channels_swept": 33,
            "channels": [{ "center_hz": 852.859e6, "outcome": "sync-without-check" }],
        });
        let r = route(&state(Some(Hunt(Some(pass.clone())))), &req("GET")).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body["pass"], pass);
        assert_eq!(
            r.body["pass"]["channels"][0]["outcome"],
            "sync-without-check"
        );
    }

    /// The route is `GET`-only, and says which method it allows rather than 404-ing a real path.
    #[test]
    fn a_write_is_refused_with_the_allowed_method() {
        let r = route(&state(Some(Hunt(None))), &req("POST")).unwrap();
        assert_eq!(r.status, 405);
        assert_eq!(r.allow, Some("GET"));
    }
}
