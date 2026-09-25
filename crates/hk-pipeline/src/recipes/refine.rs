//! Output-driven refinement of a recipe pipeline's channel: `refine.objective.builtin`
//! (ADR-0011 §8.7) with every accepted result **applied as a hot edit** (ADR-0015 §12.6; T-870 =
//! LP-6).
//!
//! # The loop does not move
//!
//! It is T-070's [`RefinementLoop`] over a registered [`hk_demod::refine::Objective`], with the
//! same settings, window, background interval and hysteresis as Listen's [`crate::refine`]
//! (`ListenSettings.refine`). What changes is only that the objective is **declared by the
//! recipe** (`"refine": {"objective": {"builtin": "wfm-pilot"}, "tune": [...]}`) instead of being
//! hard-coded in a chain.
//!
//! # Applying a refinement is an ordinary hot edit
//!
//! Listen retunes its demodulator by hand (`retune_refined`). A recipe pipeline instead gets the
//! refined centre and bandwidth written into its `input` — the bandwidth as the running revision's
//! `input.bandwidth_hz`, the centre as the pipeline's channel centre — through
//! [`RecipeRuntime::apply_refinement`], i.e. the same staged edit `PUT /api/pipelines/{id}/recipe`
//! makes: the channel is re-plumbed and every node rebuilt at a chunk boundary (ADR-0011 §2.3,
//! "`input` changed"), the ring reader's cursor never moves, capture never pauses, unchanged
//! outputs keep their streams and consumers, and the edit bumps `edit_rev`. Only the axes the
//! recipe lists in `refine.tune` are written.
//!
//! # Threads
//!
//! The pipeline thread only **copies** [`RefineSettings::window_s`] of ring IQ into a buffer made
//! on the control thread at start and handed back after each run (no allocation, no spawn on the
//! pipeline thread); it offers a window when one is due and none is being refined. The objective,
//! the hysteresis and the edit run on one `hk-refine` worker per pipeline, which ends when the
//! pipeline does. An edit waits for the pipeline thread's next chunk boundary, which is why it
//! can never be made from the pipeline thread itself.
//!
//! # Where results go
//!
//! Exactly as for Listen: a locked, accepted outcome becomes an `emitter_refined_tuning` row
//! (provenance `refined by output analysis`, source [`SOURCE_RECIPE`]) on the pipeline's target
//! emitter or the inventory emitter at the refined channel, and the emitter's **measured** centre
//! and bandwidth are never overwritten. The pipeline JSON serves the state as `refinement`, and an
//! audio output's status records carry `refined_center_hz`, `refined_bandwidth_hz` and
//! `refine_updates` (stream contract §12.2's keys, as Listen's do).
//!
//! **Blind.** The search starts from the pipeline's channel (the user's target); nothing here
//! consults a band plan or raster.

use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_demod::refine::{
    IqWindow, RefineStart, RefinementLoop, RefinementOutcome, WfmObjective, accept_update,
};
use hk_dsp::InputInfo;
use hk_model::{EmitterId, SampleTime, Timestamp};
use hk_recipe::Recipe;
use num_complex::Complex;
use serde_json::{Value, json};

use crate::recipes::runtime::{PipelineCtl, RecipeRuntime};
use crate::refine::RefineSettings;

/// `RefinedTuning.source` of a recipe pipeline's refinement.
pub const SOURCE_RECIPE: &str = "recipe";

/// A registered builtin objective ([`hk_recipe::REFINE_BUILTINS`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Builtin {
    /// `wfm-pilot`: T-070's [`WfmObjective`] (19 kHz pilot C/N₀ against the MPX guard bands, the
    /// occupied-bandwidth floor, RDS validation) over the channel IQ window.
    WfmPilot,
}

impl Builtin {
    /// The objective a recipe names, `None` when it is not registered.
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "wfm-pilot" => Some(Self::WfmPilot),
            _ => None,
        }
    }

    /// Its name in a recipe.
    pub fn name(self) -> &'static str {
        match self {
            Self::WfmPilot => "wfm-pilot",
        }
    }

    fn run(
        self,
        settings: &RefineSettings,
        window: IqWindow<'_, Complex<i8>>,
        start: &RefineStart,
    ) -> RefinementOutcome {
        let w = window.leading(settings.window_s);
        match self {
            Self::WfmPilot => {
                RefinementLoop::new(WfmObjective::new(settings.wfm), settings.loop_config)
                    .run(w, start)
            }
        }
    }
}

/// The builtin a recipe declares, `None` for a recipe without one (no refinement, or a
/// `{node, metric}` objective).
pub fn declared(recipe: &Recipe) -> Option<Builtin> {
    recipe
        .refine
        .as_ref()
        .and_then(|r| r.objective.builtin_name())
        .and_then(Builtin::named)
}

/// A refinement's result as the pipeline JSON serves it.
#[derive(Clone, Debug)]
struct Attempt {
    locked: bool,
    accepted: bool,
    center_hz: f64,
    bandwidth_hz: f64,
    quality: f64,
    stop: String,
    t: Timestamp,
}

#[derive(Clone, Debug, Default)]
struct State {
    attempts: u64,
    updates: u64,
    /// The accepted outcome in force (the hysteresis compares against it).
    current: Option<RefinementOutcome>,
    /// `edit_rev` of the edit that applied [`Self::current`].
    applied_edit_rev: Option<u64>,
    last: Option<Attempt>,
    /// Why the latest accepted outcome could not be applied (an edit refusal code).
    last_refusal: Option<String>,
}

/// A pipeline's refinement state, shared by the worker and the control routes.
#[derive(Debug)]
pub struct RefineCtl {
    builtin: Builtin,
    tune: Vec<String>,
    state: Mutex<State>,
}

fn lock(m: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl RefineCtl {
    /// The state for `recipe`'s builtin objective; `None` when it declares none.
    pub fn for_recipe(recipe: &Recipe) -> Option<Self> {
        Some(Self {
            builtin: declared(recipe)?,
            tune: recipe.refine.as_ref()?.tune.clone(),
            state: Mutex::new(State::default()),
        })
    }

    /// The refined `(centre, bandwidth)` in force, if any, and how many refinements were applied
    /// (an audio output's `refined_*` / `refine_updates` status keys). One short lock, no
    /// allocation: the pipeline thread reads it on each status tick.
    pub fn readout(&self) -> (Option<(f64, f64)>, u64) {
        let s = lock(&self.state);
        (
            s.current
                .as_ref()
                .map(|o| (o.tuning.center_hz, o.tuning.bandwidth_hz)),
            s.updates,
        )
    }

    /// `refinement` of the pipeline JSON.
    pub fn json(&self) -> Value {
        let s = lock(&self.state).clone();
        let t = |t: Timestamp| t.as_unix_nanos() as f64 / 1e9;
        json!({
            "objective": {"builtin": self.builtin.name()},
            "tune": self.tune,
            "attempts": s.attempts,
            "updates": s.updates,
            "applied_edit_rev": s.applied_edit_rev,
            "current": s.current.as_ref().map(|o| {
                let mut v = serde_json::to_value(crate::refine::audio_refinement(o))
                    .unwrap_or(Value::Null);
                if let Value::Object(m) = &mut v {
                    m.insert("locked".into(), o.locked.into());
                }
                v
            }),
            "last": s.last.as_ref().map(|a| json!({
                "locked": a.locked,
                "accepted": a.accepted,
                "center_hz": a.center_hz,
                "bandwidth_hz": a.bandwidth_hz,
                "quality": a.quality,
                "stop": a.stop,
                "t": t(a.t),
            })),
            "last_refusal": s.last_refusal,
        })
    }
}

/// One window for the worker: ring IQ, its first sample's time and provenance.
struct Job {
    iq: Vec<Complex<i8>>,
    time: SampleTime,
    provenance: ProvenanceHandle,
}

/// The pipeline thread's side: collects windows and hands them to the worker.
pub(crate) struct Refiner {
    tx: SyncSender<Job>,
    returned: Receiver<Vec<Complex<i8>>>,
    buf: Option<Vec<Complex<i8>>>,
    head: Option<(SampleTime, ProvenanceHandle)>,
    want: usize,
    interval: Option<Duration>,
    next_due: Option<Instant>,
}

impl Refiner {
    /// Starts the worker for pipeline `ctl` (which must carry a [`RefineCtl`]). Runs on the
    /// control thread: the window buffer is allocated here. `None` when refinement is disabled
    /// or the worker cannot be spawned (the pipeline then runs unrefined, as Listen does).
    pub(crate) fn start(
        ctl: &Arc<PipelineCtl>,
        rt: Weak<RecipeRuntime>,
        settings: &RefineSettings,
        sample_rate_hz: f64,
    ) -> Option<Self> {
        if !settings.enabled || ctl.refine.is_none() {
            return None;
        }
        let want = ((settings.window_s * sample_rate_hz) as usize).max(1);
        let (tx, jobs) = sync_channel::<Job>(1);
        let (give_back, returned) = sync_channel(1);
        let worker = Worker {
            ctl: Arc::clone(ctl),
            rt,
            settings: settings.clone(),
        };
        thread::Builder::new()
            .name("hk-refine".into())
            .spawn(move || worker.run(&jobs, &give_back))
            .ok()?;
        Some(Self {
            tx,
            returned,
            buf: Some(Vec::with_capacity(want)),
            head: None,
            want,
            interval: settings.live_interval,
            // The first window is due at once: the target is only the search's start.
            next_due: Some(Instant::now()),
        })
    }

    /// Offers contiguous ring samples (pipeline thread). Copies only while a window is due and
    /// none is being refined; never allocates.
    pub(crate) fn feed(
        &mut self,
        time: SampleTime,
        provenance: &ProvenanceHandle,
        samples: &[Complex<i8>],
    ) {
        if self.buf.is_none() {
            match self.returned.try_recv() {
                Ok(mut b) => {
                    b.clear();
                    self.buf = Some(b);
                    self.next_due = self.interval.map(|i| Instant::now() + i);
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            }
        }
        if samples.is_empty() || self.next_due.is_none_or(|d| Instant::now() < d) {
            return;
        }
        let Some(buf) = self.buf.as_mut() else {
            return;
        };
        let contiguous = self.head.as_ref().is_some_and(|(t, p)| {
            p.id() == provenance.id() && t.sample_index + buf.len() as u64 == time.sample_index
        });
        if !contiguous {
            buf.clear();
            self.head = Some((time, provenance.clone()));
        }
        let take = (self.want - buf.len()).min(samples.len());
        buf.extend_from_slice(&samples[..take]);
        if buf.len() < self.want {
            return;
        }
        let (Some(iq), Some((time, provenance))) = (self.buf.take(), self.head.take()) else {
            return;
        };
        if let Err(e) = self.tx.try_send(Job {
            iq,
            time,
            provenance,
        }) {
            // The worker is gone (or, impossibly, busy): keep the buffer and stop offering.
            let job = match e {
                std::sync::mpsc::TrySendError::Full(j)
                | std::sync::mpsc::TrySendError::Disconnected(j) => j,
            };
            self.buf = Some(job.iq);
            self.next_due = None;
        }
    }
}

/// The `hk-refine` worker: objective, hysteresis, hot edit, stored row.
struct Worker {
    ctl: Arc<PipelineCtl>,
    rt: Weak<RecipeRuntime>,
    settings: RefineSettings,
}

impl Worker {
    fn run(self, jobs: &Receiver<Job>, give_back: &SyncSender<Vec<Complex<i8>>>) {
        // Ends when the pipeline thread drops its `Refiner`.
        while let Ok(job) = jobs.recv() {
            self.refine(&job);
            if give_back.send(job.iq).is_err() {
                break;
            }
        }
    }

    fn refine(&self, job: &Job) {
        let Some(rc) = self.ctl.refine.as_ref() else {
            return;
        };
        let current = lock(&rc.state).current.clone();
        let start = match &current {
            Some(o) => RefineStart {
                center_hz: o.tuning.center_hz,
                bandwidth_hz: o.tuning.bandwidth_hz,
                warm: true,
            },
            None => {
                let (c, bw) = self.ctl.channel();
                RefineStart {
                    center_hz: c,
                    bandwidth_hz: bw,
                    warm: false,
                }
            }
        };
        let info = InputInfo {
            time: job.time,
            discontinuity: Discontinuity::NONE,
            dropped_before: 0,
            provenance: &job.provenance,
        };
        let outcome = rc
            .builtin
            .run(&self.settings, IqWindow::new(info, &job.iq), &start);
        let accepted = accept_update(
            &self.settings.loop_config.hysteresis,
            current.as_ref(),
            &outcome,
        );
        if crate::debug_enabled() {
            eprintln!(
                "hk-pipeline: recipe pipeline {} refine ({}) {:.4} MHz / {:.1} kHz -> {:.4} MHz / \
                 {:.1} kHz, locked {} ({:?}), accepted {accepted}",
                self.ctl.id,
                rc.builtin.name(),
                start.center_hz / 1e6,
                start.bandwidth_hz / 1e3,
                outcome.tuning.center_hz / 1e6,
                outcome.tuning.bandwidth_hz / 1e3,
                outcome.locked,
                outcome.stop,
            );
        }
        {
            let mut s = lock(&rc.state);
            s.attempts += 1;
            s.last = Some(Attempt {
                locked: outcome.locked,
                accepted,
                center_hz: outcome.tuning.center_hz,
                bandwidth_hz: outcome.tuning.bandwidth_hz,
                quality: outcome.quality,
                stop: serde_json::to_value(outcome.stop)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                t: job.time.host_time,
            });
        }
        if !accepted {
            return;
        }
        let Some(rt) = self.rt.upgrade() else {
            return;
        };
        match rt.apply_refinement(&self.ctl.id, &outcome) {
            Ok(rev) => {
                {
                    let mut s = lock(&rc.state);
                    s.current = Some(outcome.clone());
                    s.updates += 1;
                    s.applied_edit_rev = Some(u64::from(rev));
                    s.last_refusal = None;
                }
                if let Some(shared) = self.ctl.shared.upgrade() {
                    let emitter: Option<EmitterId> = self.ctl.streams_ctx.emitter_id;
                    crate::refine::store_and_explain(
                        &shared,
                        emitter,
                        &outcome,
                        SOURCE_RECIPE,
                        job.time.host_time,
                    );
                }
            }
            Err(e) => {
                if crate::debug_enabled() {
                    eprintln!(
                        "hk-pipeline: recipe pipeline {} refinement not applied: {e}",
                        self.ctl.id
                    );
                }
                lock(&rc.state).last_refusal = Some(e.code.to_owned());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_recipe::REFINE_BUILTINS;

    /// The drift guard for [`hk_recipe::REFINE_BUILTINS`]: every name the schema accepts has an objective.
    #[test]
    fn every_builtin_the_schema_accepts_has_an_objective() {
        for n in REFINE_BUILTINS {
            assert_eq!(Builtin::named(n).map(Builtin::name), Some(*n));
        }
        assert_eq!(Builtin::named("wfm-pilot"), Some(Builtin::WfmPilot));
        assert_eq!(Builtin::named("nope"), None);
    }
}
