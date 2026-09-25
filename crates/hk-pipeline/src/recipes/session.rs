//! **Ephemeral, session-owned audio pipelines (ADR-0015 §12.3, LP-5 = T-869).**
//!
//! The `listen` opener is target-shaped and per-consumer; a recipe pipeline is a named object
//! with a lifecycle. Both are needed, and the difference is not cosmetic, so this module owns the
//! seam between them:
//!
//! - **Ephemeral ownership.** `/ws/open/listen?emitter=…` with the chooser's answer starts a
//!   pipeline owned by the session ([`Owner::Session`]): it is never saved, it shows in
//!   `GET /api/pipelines` with `owner: "session"` while it runs, and it stops when its last
//!   listener goes away. Today's Listen semantics, preserved exactly.
//! - **Attach, don't duplicate.** An opener whose target already has a running audio pipeline
//!   **attaches to that pipeline's `audio` output** instead of building a second DDC. Two browser
//!   tabs listening to one station must not build two channels — under the pipeline model that
//!   would also be two rows. This rule is load-bearing, not an optimisation (§12.10 objection 4).
//!   The match is on the **target** (the same emitter, or a channel containing the chosen
//!   centre), deliberately **not** on which recipe the chooser picked: what must not exist twice
//!   is a channel on one station, and §12.7's one-output model wants the listener on the
//!   pipeline that is already there. While `analog-wfm` is the only audio recipe the two
//!   readings coincide; when a second one exists, whether a differently-recipe'd pipeline should
//!   be attached to or replaced is a decision for that ticket, not a silent default.
//! - **Attaching can never kill a named pipeline.** A listener that leaves stops only an
//!   [`Owner::Session`] pipeline, and only when it was the last one. Attaching to an
//!   [`Owner::Explicit`] pipeline (one somebody started with `POST /api/pipelines`) leaves it
//!   running, exactly as `listen?pipeline=<id>` must.
//!
//! **A pipeline that cannot be started is not a refusal.** When the recipe path fails for a
//! reason that is not the client's — an unrealisable channel rate, a recipe that has gone, a
//! thread that would not spawn — this module answers [`NoPipeline::Legacy`] and the opener runs
//! today's chain. Only refusals Listen *itself* would have sent (the gate, the window, the
//! budget, a source that ended) travel back as refusals, with Listen's own codes. Inventing a new
//! refusal code on a frozen surface, or failing to deliver audio a working chain could deliver,
//! are both worse than paying for the legacy path.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use hk_model::EmitterId;
use hk_recipe::matching::Entry;
use hk_stream::{OpenRefusal, OpenedStream, SessionEndSlot};

use crate::recipes::audio::Measured;
use crate::recipes::runtime::{Owner, PipelineCtl, RecipeRuntime, RuntimeError, StartOpts, Target};

/// Why no audio pipeline serves this request.
pub(crate) enum NoPipeline {
    /// Listen's own refusal: send it to the client.
    Refuse(Box<OpenRefusal>),
    /// The recipe path is not available for this request; run the legacy chain and say why.
    Legacy(String),
}

impl From<OpenRefusal> for NoPipeline {
    fn from(r: OpenRefusal) -> Self {
        Self::Refuse(Box::new(r))
    }
}

/// A [`RuntimeError`] as Listen answers it: the refusals Listen itself sends keep Listen's codes
/// (§12.4); anything else means the recipe path is unavailable, not that the request is bad.
fn as_listen(e: RuntimeError) -> NoPipeline {
    let code = match (e.status, e.code) {
        (403, _) => "restricted-class",
        (404, _) => "not-found",
        (409, _) => "outside-window",
        (410, _) => "source-ended",
        (503, "busy") => "busy",
        (503, _) => "replumbing",
        _ => return NoPipeline::Legacy(format!("the audio pipeline did not start: {e}")),
    };
    OpenRefusal::new(e.status, code, e.message).into()
}

/// One listener attached to an audio pipeline. Dropping it detaches, and the last listener to
/// leave stops a session-owned pipeline (see the module docs).
struct Attachment {
    rt: Arc<RecipeRuntime>,
    ctl: Arc<PipelineCtl>,
    /// Counted back out of the run's listen counters on drop, the way a legacy chain is.
    counters: Arc<crate::stats::Counters>,
    /// How the transport said the session ended (T-633).
    end: SessionEndSlot,
}

impl Drop for Attachment {
    fn drop(&mut self) {
        // Serialised against [`RecipeRuntime::open_listen_audio`]: the "am I the last one?"
        // decision and the stop must not race a listener arriving between them, or the new
        // listener would be handed a stream that ends in its face.
        let _serial = self.rt.listen_attach();
        let lc = &self.counters.listen;
        let left = self
            .ctl
            .listeners
            .fetch_sub(1, Ordering::SeqCst)
            .saturating_sub(1);
        lc.running.fetch_sub(1, Ordering::SeqCst);
        crate::stats::inc(&lc.detached);
        let shared = self.ctl.shared.upgrade();
        let ended = self.ctl.end_reason();
        crate::chains::listen::count_session_end(
            &self.counters,
            shared.as_deref(),
            ended.as_deref(),
            self.end.get(),
        );
        if left == 0 && self.ctl.owner == Owner::Session {
            let _ = self.rt.stop_json(&self.ctl.id);
        }
    }
}

impl RecipeRuntime {
    /// The audio recipes the chooser ranks: every stored recipe whose latest version runs on a
    /// live IQ channel and declares an `audio` output.
    pub fn audio_entries(&self) -> Vec<Entry> {
        self.store()
            .list()
            .into_iter()
            .filter(|s| {
                s.latest.has_audio_output() && s.latest.input.port == hk_recipe::PortType::Iq
            })
            .map(|s| Entry {
                id: s.id,
                version: s.version,
                name: s.name,
                hints: s.latest.match_hints.clone(),
            })
            .collect()
    }

    /// A running pipeline with an `audio` output serving this target: the same emitter, or a
    /// channel that contains the chosen centre. `None` means nothing is listening there yet.
    fn audio_pipeline_for(
        &self,
        emitter: Option<EmitterId>,
        center_hz: f64,
    ) -> Option<Arc<PipelineCtl>> {
        self.running_pipelines().into_iter().find(|ctl| {
            if !ctl.running.load(Ordering::SeqCst) || audio_stream(ctl).is_none() {
                return false;
            }
            if let (Some(want), Some(have)) = (emitter, ctl.streams_ctx.emitter_id) {
                return want == have;
            }
            let (c, bw) = ctl.channel();
            (center_hz - c).abs() <= 0.5 * bw
        })
    }

    /// Attaches a listener to `ctl`'s `audio` output. **Called with [`Self::listen_attach`]
    /// held**, so no [`Attachment`] is ever constructed — and so no detach can ever run — on a
    /// path that then fails.
    fn attach(self: &Arc<Self>, ctl: &Arc<PipelineCtl>) -> Result<OpenedStream, NoPipeline> {
        let entry = audio_stream(ctl)
            .ok_or_else(|| NoPipeline::Legacy("the pipeline has no audio output".into()))?;
        // Every check first: a session that is not served counts nothing, so `attached`,
        // `running` and `detached` stay in balance whatever happens here (a guard built before
        // the check would count a detach for a session that never attached).
        if !ctl.running.load(Ordering::SeqCst) {
            return Err(NoPipeline::Legacy(
                "the pipeline ended as it started".into(),
            ));
        }
        ctl.listeners.fetch_add(1, Ordering::SeqCst);
        let counters = self.counters();
        counters.listen.running.fetch_add(1, Ordering::SeqCst);
        let end = SessionEndSlot::default();
        let session = Attachment {
            rt: Arc::clone(self),
            ctl: Arc::clone(ctl),
            counters,
            end: end.clone(),
        };
        Ok(OpenedStream {
            header: entry.header,
            handle: entry.handle,
            session: Box::new(session),
            end,
        })
    }

    /// The `listen` opener's recipe path (ADR-0015 §12.9 stage 4): attach to the audio pipeline
    /// already serving this target, or start an ephemeral one on `recipe` seeded at
    /// `[lo, hi]`, and return its `audio` output as the opened stream.
    pub(crate) fn open_listen_audio(
        self: &Arc<Self>,
        (recipe_id, version): (&str, u32),
        emitter: Option<EmitterId>,
        seed: &crate::audio::Seed,
        measured: Measured,
    ) -> Result<OpenedStream, NoPipeline> {
        let (lo, hi) = seed.extent_hz();
        let center = 0.5 * (lo + hi);
        // Held across "is one already serving this target?" → start → attach. Without it two
        // openers probing the same station at the same moment both miss and both start a
        // pipeline — the very duplicate this path exists to prevent — and a pipeline can be
        // stopped by its last listener leaving between the two steps.
        let _serial = self.listen_attach();
        if let Some(ctl) = self.audio_pipeline_for(emitter, center) {
            return self.attach(&ctl);
        }
        let mut recipe = self
            .store()
            .get(recipe_id, Some(version))
            .map_err(|e| NoPipeline::Legacy(format!("recipe {recipe_id}@{version}: {e}")))?;
        seed_recipe(&mut recipe, seed);
        let target = match emitter {
            Some(id) => Target::Emitter(id),
            None => Target::Band { f_lo: lo, f_hi: hi },
        };
        let id = self
            .start_with(
                recipe,
                target,
                &StartOpts {
                    session: true,
                    measured: Some(Arc::new(measured)),
                },
            )
            .map_err(as_listen)?;
        let ctl = self
            .pipeline(&id)
            .ok_or_else(|| NoPipeline::Legacy("the pipeline ended as it started".into()))?;
        match self.attach(&ctl) {
            Ok(s) => Ok(s),
            Err(e) => {
                // Nothing is listening to a pipeline nobody can reach. `attach` failed before it
                // built an [`Attachment`], so nothing re-enters the detach path here.
                let _ = self.stop_json(&id);
                Err(e)
            }
        }
    }
}

/// Applies the chooser's seed parameters to the recipe it chose (ADR-0015 §12.2). Only what the
/// **measurement** decides is seeded: whether this mode's audio is AGC'd. The recipe's structure,
/// and every parameter the measurement says nothing about, are untouched.
fn seed_recipe(recipe: &mut hk_recipe::Recipe, seed: &crate::audio::Seed) {
    if let Some(node) = recipe.nodes.iter_mut().find(|n| n.block == "agc") {
        node.params
            .insert("enabled".to_owned(), serde_json::Value::Bool(seed.agc));
    }
}

/// A pipeline's first `audio` output stream, if it has one.
fn audio_stream(ctl: &PipelineCtl) -> Option<crate::recipes::runtime::StreamEntry> {
    ctl.streams_of("audio").into_iter().next()
}
