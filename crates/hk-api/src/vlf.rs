//! VLF/LF science over HTTP (T-891; SPACE-001, SPACE-041, PROP-019), documented in `docs/api.md`
//! "VLF accessory".
//!
//! `GET /api/vlf[?device=<device_id>][&points=1]`: one report per **accessory-fed** source
//! attached to the run — a VLF/LF receiver into a soundcard, below the HackRF's 1 MHz floor.
//! Each report carries the accessory stream's provenance (so every result says it came through
//! the accessory), the carriers found blind with their SPACE-001 amplitude steps and PROP-019
//! phase steps (and reflection-height changes where path geometry was given), and the SPACE-041
//! sferics as Detections. `points=1` adds each carrier's amplitude/phase track.
//!
//! An empty `accessories` list is the honest answer of a run with no accessory: the base device
//! reaches none of these use cases. Read-only, so it skips [`crate::control::dispatch`] like
//! [`crate::trunking`]. The analysis itself lives in `hk_pipeline::vlf`; this crate sees JSON
//! through [`VlfControl`] and never names the pipeline.

use serde_json::{Value, json};

use crate::control::{CtlRequest, CtlResponse, Fail, refuse_route};
use crate::http::ApiState;

/// The run's accessory services, as the API sees them.
pub trait VlfControl: Send + Sync {
    /// One report per attached accessory source (see the module docs for the shape).
    fn reports(&self, include_points: bool) -> Vec<Value>;
}

pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    if req.path != "/api/vlf" {
        return None;
    }
    if req.method != "GET" {
        return Some(refuse_route(state, req, Some("GET")));
    }
    Some(match reports(state, req.query) {
        Ok(body) => CtlResponse {
            status: 200,
            body,
            allow: None,
        },
        Err(f) => f.response(),
    })
}

fn param<'a>(q: &'a [(String, String)], key: &str) -> Option<&'a str> {
    q.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn reports(state: &ApiState, q: &[(String, String)]) -> Result<Value, Fail> {
    let vlf = state
        .vlf
        .as_ref()
        .ok_or_else(|| Fail::new(503, "unavailable", "no accessory services on this server"))?;
    let points = match param(q, "points") {
        None | Some("0") | Some("false") => false,
        Some("1") | Some("true") => true,
        Some(_) => return Err(Fail::invalid("points must be 0 or 1")),
    };
    let mut all = vlf.reports(points);
    if let Some(device) = param(q, "device") {
        all.retain(|r| r["device_id"].as_str() == Some(device));
        if all.is_empty() {
            return Err(Fail::new(
                404,
                "not_found",
                format!("no accessory source {device:?} on this run"),
            ));
        }
    }
    Ok(json!({ "accessories": all }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    struct Fixed(Vec<Value>);

    impl VlfControl for Fixed {
        fn reports(&self, include_points: bool) -> Vec<Value> {
            self.0
                .iter()
                .cloned()
                .map(|mut r| {
                    if include_points {
                        r["carriers"][0]["points"] = json!([]);
                    }
                    r
                })
                .collect()
        }
    }

    fn get(state: &ApiState, method: &str, query: &[(String, String)]) -> CtlResponse {
        route(
            state,
            &CtlRequest {
                method,
                path: "/api/vlf",
                body: b"",
                content_type: None,
                caller: crate::control::Caller::default(),
                query,
            },
        )
        .unwrap()
    }

    fn q(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn space_001_reports_filter_by_device_and_refuse_bad_input() {
        let state = ApiState {
            vlf: Some(Arc::new(Fixed(vec![json!({
                "device_id": "vlf-receiver:mock:a",
                "accessory": "vlf-receiver",
                "carriers": [{"carrier_hz": 19800.0}],
            })]))),
            ..ApiState::default()
        };
        let r = get(&state, "GET", &[]);
        assert_eq!(r.status, 200);
        assert_eq!(r.body["accessories"][0]["accessory"], "vlf-receiver");
        assert!(
            r.body["accessories"][0]["carriers"][0]
                .get("points")
                .is_none()
        );
        let r = get(&state, "GET", &q(&[("points", "1")]));
        assert!(r.body["accessories"][0]["carriers"][0]["points"].is_array());
        let r = get(&state, "GET", &q(&[("device", "vlf-receiver:mock:a")]));
        assert_eq!(r.status, 200);
        assert_eq!(
            get(&state, "GET", &q(&[("device", "hackrf:1")])).status,
            404
        );
        assert_eq!(get(&state, "GET", &q(&[("points", "yes")])).status, 400);
        assert_eq!(get(&state, "POST", &[]).status, 405);
        assert_eq!(get(&ApiState::default(), "GET", &[]).status, 503);
        // A run with no accessory: an honest empty list, not an error.
        let none = ApiState {
            vlf: Some(Arc::new(Fixed(Vec::new()))),
            ..ApiState::default()
        };
        let r = get(&none, "GET", &[]);
        assert_eq!(
            (r.status, r.body["accessories"].as_array().map(Vec::len)),
            (200, Some(0))
        );
    }
}
