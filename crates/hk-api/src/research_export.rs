//! `GET /api/research/export` — the durable research objects out as one file (T-823, MAP-23).
//!
//! A researcher's collections, markers, annotations and saved measurements are durable
//! (docs/25 §10); this route bundles them so the artifact outlives the session and the device.
//! **Read-only, offline, no new signal logic:** every figure is the stored object's own JSON, exactly
//! as its own route serves it; nothing is recomputed and nothing reaches a device.
//!
//! * `collection` (UUID, optional) — only that collection's markers, annotations and measurements.
//! * `rate` (Hz, optional, default 1e6) — the sample rate the SigMF-adjacent annotation block is
//!   expressed against (SigMF positions are sample indices).
//!
//! The bundle carries `sigmf`: a SigMF-shaped `global` / `captures` / `annotations` document whose
//! recording starts (sample 0) at `recording_start_s`, the earliest annotation start. Each entry is
//! [`hk_model::AuthoredAnnotation::to_sigmf`], with its `hackriff:annotation` block, so an authored
//! note can never be mistaken for a `hackriff:truth` entry.

use std::sync::PoisonError;

use hk_model::{
    AUTHORED_PAGE_MAX, AuthoredAnnotation, CollectionId, MarkerWindow, MeasurementFilter, Timestamp,
};
use serde_json::{Value, json};

use crate::annotations::annotation_json;
use crate::collections::{collection_json, marker_json};
use crate::control::{CtlRequest, CtlResponse, Fail, refuse_route};
use crate::http::ApiState;
use crate::measurements::{measurement_json, query, query_f64};

/// The bundle's format tag.
pub const EXPORT_FORMAT: &str = "hackriff-research-export@1";
const DEFAULT_RATE_HZ: f64 = 1e6;
/// Pages of [`AUTHORED_PAGE_MAX`] rows read per object type; a store larger than this reports
/// `truncated: true` rather than dropping rows quietly.
const MAX_PAGES: usize = 10;

pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    if req.path != "/api/research/export" {
        return None;
    }
    if req.method != "GET" {
        return Some(refuse_route(state, req, Some("GET")));
    }
    Some(match build(state, req.query) {
        Ok(body) => CtlResponse {
            status: 200,
            body,
            allow: None,
        },
        Err(f) => f.response(),
    })
}

fn fail(what: &str, e: impl std::fmt::Display) -> Fail {
    Fail::new(500, "failed", format!("{what}: {e}"))
}

fn build(state: &ApiState, q: &[(String, String)]) -> Result<Value, Fail> {
    let rate = query_f64(q, "rate")?.unwrap_or(DEFAULT_RATE_HZ);
    if rate.is_nan() || rate <= 0.0 {
        return Err(Fail::invalid("rate must be a positive number of Hz"));
    }
    let collection = query(q, "collection")
        .map(|c| {
            c.parse::<CollectionId>()
                .map_err(|_| Fail::invalid("collection must be a UUID"))
        })
        .transpose()?;
    let repo = state
        .bookmarks
        .as_ref()
        .ok_or_else(|| Fail::new(503, "unavailable", "no research store on this server"))?
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let mut truncated = false;

    let mut collections = Vec::new();
    for page in 0..MAX_PAGES {
        let p = repo
            .collections(page * AUTHORED_PAGE_MAX, AUTHORED_PAGE_MAX)
            .map_err(|e| fail("collections", e))?;
        collections.extend(
            p.items
                .iter()
                .map(|s| (s.collection.clone(), s.member_count)),
        );
        if collections.len() as u64 >= p.matched {
            break;
        }
        truncated |= page + 1 == MAX_PAGES;
    }
    if let Some(id) = collection {
        collections.retain(|(c, _)| c.id == id);
        if collections.is_empty() {
            return Err(Fail::new(404, "not_found", "no such collection"));
        }
    }

    let mut markers = Vec::new();
    for page in 0..MAX_PAGES {
        let p = repo
            .markers(
                collection,
                MarkerWindow::default(),
                page * AUTHORED_PAGE_MAX,
                AUTHORED_PAGE_MAX,
            )
            .map_err(|e| fail("markers", e))?;
        markers.extend(p.items);
        if markers.len() as u64 >= p.matched {
            break;
        }
        truncated |= page + 1 == MAX_PAGES;
    }

    let mut annotations: Vec<AuthoredAnnotation> = Vec::new();
    for page in 0..MAX_PAGES {
        let p = repo
            .authored_annotations_in(
                0.0,
                f64::MAX,
                Timestamp::from_unix_nanos(i64::MIN),
                Timestamp::from_unix_nanos(i64::MAX),
                page * AUTHORED_PAGE_MAX,
                AUTHORED_PAGE_MAX,
            )
            .map_err(|e| fail("annotations", e))?;
        annotations.extend(p.rows);
        if annotations.len() as u64 >= p.matched {
            break;
        }
        truncated |= page + 1 == MAX_PAGES;
    }
    if let Some(id) = collection {
        let id = id.to_string();
        annotations.retain(|a| a.collection_id.as_deref() == Some(id.as_str()));
    }

    let filter = MeasurementFilter {
        collection_id: collection.map(|c| c.to_string()),
        window: None,
    };
    let mut measurements = Vec::new();
    for page in 0..MAX_PAGES {
        let p = repo
            .measurements_in(&filter, page * AUTHORED_PAGE_MAX, AUTHORED_PAGE_MAX)
            .map_err(|e| fail("measurements", e))?;
        measurements.extend(p.rows);
        if measurements.len() as u64 >= p.matched {
            break;
        }
        truncated |= page + 1 == MAX_PAGES;
    }
    drop(repo);

    // SigMF-adjacent: the recording starts at the earliest annotation; sample positions follow.
    let start = annotations.iter().map(|a| a.t0).min();
    let sigmf_annotations: Vec<Value> = start
        .map(|s| {
            annotations
                .iter()
                .filter_map(|a| a.to_sigmf(s, rate))
                .filter_map(|x| serde_json::to_value(x).ok())
                .collect()
        })
        .unwrap_or_default();
    let start_s = start.map(|s| s.as_unix_nanos() as f64 / 1e9);

    Ok(json!({
        "format": EXPORT_FORMAT,
        "collection": collection.map(|c| c.to_string()),
        "counts": {
            "collections": collections.len(),
            "markers": markers.len(),
            "annotations": annotations.len(),
            "measurements": measurements.len(),
        },
        "truncated": truncated,
        "collections": collections.iter().map(|(c, n)| collection_json(c, *n)).collect::<Vec<_>>(),
        "markers": markers.iter().map(marker_json).collect::<Vec<_>>(),
        "annotations": annotations.iter().map(annotation_json).collect::<Vec<_>>(),
        "measurements": measurements.iter().map(measurement_json).collect::<Vec<_>>(),
        "sigmf": {
            "global": {
                "core:version": "1.0.0",
                "core:sample_rate": rate,
                "core:description": "hackriff authored annotations (a note file, not an IQ recording)",
            },
            "captures": [{ "core:sample_start": 0 }],
            "annotations": sigmf_annotations,
            "recording_start_s": start_s,
        },
    }))
}
