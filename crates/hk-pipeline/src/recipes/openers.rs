//! `stage` and live `inspector` stream openers (ADR-0011 §7, stream contract §14.4/§14.8;
//! T-088), registered as `/ws/open/stage`, `/ws/open/inspector` and TCP `open/stage?…`,
//! `open/inspector?…`.
//!
//! - **`stage`** `?pipeline=<id>&node=<node>[&port=<port>][&view=raw|spectrum|sync_search|eye]`:
//!   any output port of any node of a running pipeline, opened on demand. The tap is adopted by
//!   the pipeline thread at its next chunk boundary and removed when the session guard drops (the
//!   consumer left); an untapped port costs nothing. `view=raw` (default) serves the port's own
//!   samples; `view=spectrum` (`iq`/`real` ports only, T-160) serves a PSD instead (§14.4,
//!   `tap_spectrum`); `view=sync_search` (`bits` ports only, T-162, needs `sync_word=0x…` and
//!   `sync_bits=<n>`) serves the match score per candidate bit position against that word
//!   instead (§14.4, `tap_sync_search`); `view=eye` (`iq`/`real` ports only, T-161, needs
//!   `symbol_rate_bd=<f>`) serves the clock-recovery eye/timing diagram — the waveform folded
//!   around every estimated symbol instant (§14.4, `tap_eye`) — requesting a `view` on a port
//!   type it doesn't support is refused (409).
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
use crate::recipes::tap_eye;
use crate::recipes::tap_spectrum;
use crate::recipes::tap_sync_search;
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
        if !matches!(view, "raw" | "spectrum" | "sync_search" | "eye") {
            return Err(OpenRefusal::new(
                400,
                "bad-request",
                format!("unknown view {view:?} (raw, spectrum, sync_search, eye)"),
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
        if view == "sync_search" && info.ty != PortType::Bits {
            return Err(OpenRefusal::new(
                409,
                "view-unsupported",
                "view=sync_search serves bits ports only",
            ));
        }
        if view == "eye" && !matches!(info.ty, PortType::Iq | PortType::Real) {
            // A `soft` port carries one decided value per symbol (`clock_recovery` declares its
            // rate as the symbol rate), so there is nothing *between* the instants to draw.
            return Err(OpenRefusal::new(
                409,
                "view-unsupported",
                "view=eye serves iq/real ports only (a soft port is already one value per symbol)",
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
        } else if view == "sync_search" {
            let word_str = param(req, "sync_word")?;
            let word = hk_recipe::parse_hex(word_str).ok_or_else(|| {
                OpenRefusal::new(400, "bad-request", "sync_word must be a 0x… hex string")
            })?;
            let bits: u32 = param(req, "sync_bits")?.parse().map_err(|_| {
                OpenRefusal::new(400, "bad-request", "sync_bits must be an integer")
            })?;
            let header = tap_sync_search::sync_search_header(
                &ctl.streams_ctx,
                &recipe,
                stream_id,
                info.rate_hz,
            );
            let publisher = TapPublisher::new_sync_search(
                header.clone(),
                tap_config(),
                info.rate_hz,
                word,
                bits,
            )
            .map_err(|e| OpenRefusal::new(500, "publisher", e.to_string()))?
            .ok_or_else(|| {
                OpenRefusal::new(
                    400,
                    "bad-request",
                    "sync_bits must be 1..=64 and sync_word must fit in it",
                )
            })?;
            (header, publisher)
        } else if view == "eye" {
            let bd: f64 = param(req, "symbol_rate_bd")?.parse().map_err(|_| {
                OpenRefusal::new(400, "bad-request", "symbol_rate_bd must be a number")
            })?;
            let header =
                tap_eye::eye_header(&ctl.streams_ctx, &recipe, stream_id, info.rate_hz, bd);
            let publisher = TapPublisher::new_eye(header.clone(), tap_config(), info.rate_hz, bd)
                .map_err(|e| OpenRefusal::new(500, "publisher", e.to_string()))?
                .ok_or_else(|| {
                    OpenRefusal::new(
                        400,
                        "bad-request",
                        format!(
                            "symbol_rate_bd must give {}..={} samples per symbol on this port \
                             (rate {} Hz)",
                            tap_eye::EYE_MIN_SPS,
                            tap_eye::EYE_MAX_SPS,
                            info.rate_hz
                        ),
                    )
                })?;
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
            end: hk_stream::SessionEndSlot::default(),
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
            end: hk_stream::SessionEndSlot::default(),
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
            "kind": "iq | audio | symbols | bits | messages (by port type); spectrum (view=spectrum, iq/real ports); sync-search (view=sync_search, bits ports); eye (view=eye, iq/real ports)",
            "params": ["pipeline", "node", "port", "view (raw | spectrum | sync_search | eye)", "sync_word (view=sync_search: 0x… hex)", "sync_bits (view=sync_search: 1..=64)", "symbol_rate_bd (view=eye: symbols/s, 2..=1024 samples per symbol)"],
            "records": "one data record per processed chunk (frames ports: one frame record per frame; view=spectrum, view=sync_search or view=eye: one row at most every 1/25 s)",
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
