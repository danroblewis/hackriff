//! `stage` and live `inspector` stream openers (ADR-0011 §7, stream contract §14.4/§14.8;
//! T-088), registered as `/ws/open/stage`, `/ws/open/inspector` and TCP `open/stage?…`,
//! `open/inspector?…`.
//!
//! - **`stage`** `?pipeline=<id>&node=<node>[&port=<port>][&view=raw|spectrum]`: any output port
//!   of any node of a running pipeline, opened on demand. The tap is adopted by the pipeline
//!   thread at its next chunk boundary and removed when the session guard drops (the consumer
//!   left); an untapped port costs nothing. `view=raw` (default) serves the port's own samples;
//!   `view=spectrum` (`iq`/`real` ports only, T-160) serves a PSD instead (§14.4,
//!   `tap_spectrum`) — requesting it on a `soft`/`bits`/`frames` port is refused (409).
//! - **`inspector`** `?pipeline=<id>[&output=<id>]`: the always-on frames stream of a pipeline's
//!   `inspector` output (the first one by default), i.e. the same stream as
//!   `/ws/inspector/<pipeline>/<output>`. `capture=` (recorded decoded streams) is T-089/T-092's
//!   extension of this opener.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use hk_recipe::PortType;
use hk_stream::{OpenRefusal, OpenRequest, OpenedStream, StreamOpener};
use serde_json::{Value, json};

use crate::recipes::runtime::RecipeRuntime;
use crate::recipes::tap_spectrum;
use crate::recipes::taps::{StageTap, TapPublisher, stage_header, tap_config};

static TAP_SEQ: AtomicU64 = AtomicU64::new(1);

struct CloseOnDrop(Arc<AtomicBool>);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn param<'a>(req: &'a OpenRequest, name: &str) -> Result<&'a str, OpenRefusal> {
    req.param(name)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| OpenRefusal::new(400, "bad-request", format!("give {name}=<id>")))
}

impl RecipeRuntime {
    /// Opens a stage stream (see the module docs).
    pub fn open_stage(&self, req: &OpenRequest) -> Result<OpenedStream, OpenRefusal> {
        let pid = param(req, "pipeline")?;
        let node = param(req, "node")?;
        let view = req.param("view").unwrap_or("raw");
        if !matches!(view, "raw" | "spectrum") {
            return Err(OpenRefusal::new(
                400,
                "bad-request",
                format!("unknown view {view:?} (raw, spectrum)"),
            ));
        }
        let ctl = self
            .pipeline(pid)
            .ok_or_else(|| OpenRefusal::new(404, "not-found", "no such pipeline"))?;
        let ended = || OpenRefusal::new(410, "ended", "the pipeline has ended");
        if !ctl.running.load(Ordering::SeqCst) {
            return Err(ended());
        }
        ctl.drop_retired_taps();
        let (recipe, shape) = {
            let cs = ctl
                .control
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (Arc::clone(&cs.recipe), cs.shape.clone())
        };
        let sh = shape
            .nodes
            .iter()
            .find(|n| n.id == node)
            .ok_or_else(|| OpenRefusal::new(404, "not-found", "no such node"))?;
        let k = match req.param("port") {
            Some(p) => sh.out_names.iter().position(|n| n == p),
            None => sh.diagnostic.iter().position(|d| !d),
        }
        .ok_or_else(|| OpenRefusal::new(404, "not-found", "no such port"))?;
        let info = sh.out_info[k];
        if view == "spectrum" && !matches!(info.ty, PortType::Iq | PortType::Real) {
            return Err(OpenRefusal::new(
                409,
                "view-unsupported",
                "view=spectrum serves iq/real ports only",
            ));
        }
        let port = sh.out_names[k].clone();
        let stream_id = format!(
            "stage/{pid}/{node}.{port}/{}",
            TAP_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let output_id = format!("{node}.{port}");
        let (header, publisher) = if view == "spectrum" {
            let header = tap_spectrum::spectrum_header(
                &ctl.streams_ctx,
                &recipe,
                stream_id,
                info.ty,
                info.rate_hz,
            );
            let publisher = TapPublisher::new_spectrum(header.clone(), tap_config(), info.rate_hz)
                .map_err(|e| OpenRefusal::new(500, "publisher", e.to_string()))?;
            (header, publisher)
        } else {
            let header = stage_header(
                &ctl.streams_ctx,
                &recipe,
                stream_id,
                &output_id,
                info.ty,
                info.rate_hz,
            );
            let publisher =
                TapPublisher::new(header.clone(), tap_config(), &recipe, info.max_items)
                    .map_err(|e| OpenRefusal::new(500, "publisher", e.to_string()))?;
            (header, publisher)
        };
        let closed = Arc::new(AtomicBool::new(false));
        let tap = StageTap::new(node.to_owned(), port, publisher, Some(Arc::clone(&closed)));
        let handle = tap.handle();
        ctl.new_taps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(tap);
        ctl.taps_dirty.store(true, Ordering::SeqCst);
        if !ctl.running.load(Ordering::SeqCst) {
            return Err(ended());
        }
        Ok(OpenedStream {
            header,
            handle,
            session: Box::new(CloseOnDrop(closed)),
        })
    }

    /// Opens a pipeline's live inspector stream (see the module docs).
    pub fn open_inspector(&self, req: &OpenRequest) -> Result<OpenedStream, OpenRefusal> {
        if req.param("capture").is_some() {
            return self.open_capture(req); // T-092 (recipes/capture.rs)
        }
        let pid = param(req, "pipeline")?;
        let ctl = self
            .pipeline(pid)
            .ok_or_else(|| OpenRefusal::new(404, "not-found", "no such pipeline"))?;
        let output = req.param("output");
        let entry = ctl
            .streams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|s| s.kind == "inspector" && output.is_none_or(|o| o == s.output_id))
            .cloned()
            .ok_or_else(|| OpenRefusal::new(404, "not-found", "no such inspector output"))?;
        if !ctl.running.load(Ordering::SeqCst) {
            return Err(OpenRefusal::new(410, "ended", "the pipeline has ended"));
        }
        Ok(OpenedStream {
            header: entry.header,
            handle: entry.handle,
            session: Box::new(()),
        })
    }

    /// The `stage` opener.
    pub fn stage_service(self: &Arc<Self>) -> Arc<dyn StreamOpener> {
        Arc::new(StageOpener(Arc::clone(self)))
    }

    /// The live `inspector` opener.
    pub fn inspector_service(self: &Arc<Self>) -> Arc<dyn StreamOpener> {
        Arc::new(InspectorOpener(Arc::clone(self)))
    }
}

/// `open/stage` (T-088).
pub struct StageOpener(pub Arc<RecipeRuntime>);

impl StreamOpener for StageOpener {
    fn open(&self, request: &OpenRequest) -> Result<OpenedStream, OpenRefusal> {
        self.0.open_stage(request)
    }

    fn describe(&self) -> Value {
        json!({
            "kind": "iq | audio | symbols | bits | messages (by port type); spectrum (view=spectrum, iq/real ports)",
            "params": ["pipeline", "node", "port", "view (raw | spectrum)"],
            "records": "one data record per processed chunk (frames ports: one frame record per frame; view=spectrum: one PSD row at most every 1/25 s)",
        })
    }
}

/// `open/inspector` (T-088 live; T-089/T-092 extend it with captures).
pub struct InspectorOpener(pub Arc<RecipeRuntime>);

impl StreamOpener for InspectorOpener {
    fn open(&self, request: &OpenRequest) -> Result<OpenedStream, OpenRefusal> {
        self.0.open_inspector(request)
    }

    fn describe(&self) -> Value {
        json!({
            "kind": "messages",
            "message_schema": hk_stream::inspector::INSPECTOR_MESSAGE_SCHEMA,
            "params": ["pipeline", "output", "capture", "from_frame", "field_map"],
            "records": "frame (one per frame), status (one per ~250 ms tick), edit (per hot edit)",
        })
    }
}
