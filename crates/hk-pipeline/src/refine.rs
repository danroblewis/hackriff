//! Output-driven refinement in the running pipeline (T-070, SIGNAL-062): the
//! [`hk_demod::refine`] loop wired into the demodulator chains, its results stored on emitters.
//!
//! # Where it runs
//!
//! - **Listen** ([`crate::chains::listen`]): the probe collects [`RefineSettings::window_s`] of
//!   ring IQ (at least the mode probe). When mode selection chose a mode with an objective (WFM
//!   today) the loop refines the channel from the selection or detection box; otherwise a WFM
//!   **trial** runs on the same IQ, and when its pilot locks, mode selection runs again on the
//!   refined channel box and decides (the selector's rules are unchanged; only the box it sees
//!   moves). The refined centre and bandwidth become the demodulated channel, are reported in the
//!   stream header (`audio.refinement`, `center_hz`, `bandwidth_hz`, `params`) and in every status
//!   record (`refined_center_hz`, `refined_bandwidth_hz`, `refine_updates`). While the stream runs,
//!   [`LiveRefiner`] copies a window every [`RefineSettings::live_interval`] and refines it on its
//!   own thread (warm start, bounded by the loop's budgets), never on the ring reader's thread;
//!   a result retunes the audio only when it passes the loop's hysteresis.
//! - **Analog chain** ([`crate::chains::analog`]): when the probe accepted a refinable mode, the
//!   collected window's leading [`RefineSettings::window_s`] refines the raster channel the chain
//!   was attached to, and the full demodulation (and RDS) runs on the refined channel.
//!
//! # Where results go
//!
//! A locked outcome becomes a [`RefinedTuning`] row (provenance
//! [`hk_model::REFINED_BY_OUTPUT_ANALYSIS`], objective value, iterations, evaluations, time) on the
//! emitter: the analog chain's written emitter, Listen's target emitter, or for a selection or
//! detection target the inventory emitter nearest the refined centre ([`emitter_for_channel`]).
//! The emitter's detected centre and bandwidth are kept. The explanations are re-ranked at once;
//! [`crate::family`] checks rasters and allocations at the refined centre. `/api/inventory` serves
//! the latest row as `refined`.
//!
//! **Blind.** Nothing here consults a band plan or raster: the analog chain's raster channel and a
//! user's selection are only the loop's start.

use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_demod::AnalogMode;
use hk_demod::audio::{AudioConfig, AudioPlan, ProbeResult, probe};
use hk_demod::refine::{
    IqWindow, LoopConfig, RefineStart, RefinementLoop, RefinementOutcome, WfmObjective,
    WfmObjectiveConfig, accept_update,
};
use hk_dsp::{InputInfo, IqSample};
use hk_estimate::SnippetRequest;
use hk_model::{
    EmitterId, FreqRange, RefinedTuning, Region, RepoError, Repository, SampleTime, TimeRange,
    Timestamp,
};
use hk_stream::audio::AudioRefinement;
use num_complex::Complex;

use crate::run::Shared;

/// `RefinedTuning.source` of Listen.
pub const SOURCE_LISTEN: &str = "listen";
/// `RefinedTuning.source` of the analog chain.
pub const SOURCE_ANALOG_CHAIN: &str = "analog-chain";

/// Refinement settings.
#[derive(Clone, Debug, PartialEq)]
pub struct RefineSettings {
    /// Refine at all.
    pub enabled: bool,
    /// IQ the loop measures on, s (validation uses up to this much).
    pub window_s: f64,
    /// Loop settings (termination, windows, hysteresis).
    pub loop_config: LoopConfig,
    /// WFM objective settings.
    pub wfm: WfmObjectiveConfig,
    /// Listen: re-refine this often while streaming (`None`: only at the start).
    pub live_interval: Option<Duration>,
}

impl Default for RefineSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            window_s: 1.0,
            loop_config: LoopConfig::default(),
            wfm: WfmObjectiveConfig::default(),
            live_interval: Some(Duration::from_secs(15)),
        }
    }
}

/// Whether `mode` has an objective.
pub fn refinable(mode: AnalogMode) -> bool {
    mode == AnalogMode::Wfm
}

/// Refines `mode`'s channel on the leading [`RefineSettings::window_s`] of `window` from `start`;
/// `None` when disabled or the mode has no objective.
pub fn refine<T: IqSample>(
    settings: &RefineSettings,
    mode: AnalogMode,
    window: IqWindow<'_, T>,
    start: &RefineStart,
) -> Option<RefinementOutcome> {
    if !settings.enabled || !refinable(mode) {
        return None;
    }
    let w = window.leading(settings.window_s);
    Some(RefinementLoop::new(WfmObjective::new(settings.wfm), settings.loop_config).run(w, start))
}

/// The row for a locked outcome on `emitter` measured at `t` (`None` when unlocked). Non-finite
/// mode parameters are dropped.
pub fn refined_tuning(
    outcome: &RefinementOutcome,
    emitter: EmitterId,
    source: &str,
    t: Timestamp,
) -> Option<RefinedTuning> {
    outcome.locked.then(|| RefinedTuning {
        emitter_id: emitter,
        provenance: outcome.provenance.clone(),
        objective: outcome.objective.clone(),
        mode: outcome.mode.clone(),
        source: source.to_owned(),
        center_hz: outcome.tuning.center_hz,
        bandwidth_hz: outcome.tuning.bandwidth_hz,
        start_center_hz: outcome.start.center_hz,
        start_bandwidth_hz: outcome.start.bandwidth_hz,
        detected_center_hz: 0.0,
        detected_bandwidth_hz: 0.0,
        objective_value: outcome.quality,
        locked: true,
        converged: outcome.converged,
        iterations: outcome.iterations,
        evaluations: outcome.evaluations,
        elapsed_s: outcome.elapsed_s,
        mode_params: outcome
            .mode_params
            .iter()
            .filter(|(_, v)| v.is_finite())
            .map(|(k, v)| (k.clone(), *v))
            .collect(),
        t,
    })
}

/// Stores a locked outcome on `emitter` (no re-ranking; see [`store_and_explain`]).
pub fn persist(
    repo: &mut Repository,
    emitter: EmitterId,
    outcome: &RefinementOutcome,
    source: &str,
    t: Timestamp,
) -> Result<Option<RefinedTuning>, RepoError> {
    match refined_tuning(outcome, emitter, source, t) {
        Some(row) => repo.insert_refined_tuning(&row).map(Some),
        None => Ok(None),
    }
}

/// The inventory emitter a refined channel belongs to when the chain had no emitter of its own:
/// the live emitter nearest the refined centre whose detected centre lies within half the refined
/// bandwidth and which is at most twice as wide.
pub fn emitter_for_channel(
    repo: &Repository,
    center_hz: f64,
    bandwidth_hz: f64,
) -> Result<Option<EmitterId>, RepoError> {
    let ever = TimeRange::new(
        Timestamp::from_unix_nanos(0),
        Timestamp::from_unix_nanos(i64::MAX / 2),
    );
    let rows = repo.emitters_in_region(&Region::new(
        FreqRange::centered(center_hz, bandwidth_hz),
        ever,
    ))?;
    Ok(rows
        .into_iter()
        .filter(|e| {
            (e.f_center_hz - center_hz).abs() <= 0.5 * bandwidth_hz
                && e.bandwidth_hz <= 2.0 * bandwidth_hz
        })
        .min_by(|a, b| {
            (a.f_center_hz - center_hz)
                .abs()
                .total_cmp(&(b.f_center_hz - center_hz).abs())
        })
        .map(|e| e.id))
}

/// Stores a locked outcome on `emitter` (or, without one, on [`emitter_for_channel`]) and re-ranks
/// its explanations. Returns the emitter it was stored on.
pub(crate) fn store_and_explain(
    shared: &Shared,
    emitter: Option<EmitterId>,
    outcome: &RefinementOutcome,
    source: &str,
    t: Timestamp,
) -> Option<EmitterId> {
    if !outcome.locked {
        return None;
    }
    let mut repo = shared.repo();
    let target = emitter.or_else(|| {
        emitter_for_channel(&repo, outcome.tuning.center_hz, outcome.tuning.bandwidth_hz)
            .ok()
            .flatten()
    })?;
    let stored = match persist(&mut repo, target, outcome, source, t) {
        Ok(Some(row)) => row,
        Ok(None) => return None,
        Err(e) => {
            crate::stats::inc(&shared.counters.chains.errors);
            eprintln!("hk-pipeline: refined tuning write: {e}");
            return None;
        }
    };
    let mut inv = shared
        .inventory
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if inv
        .chain_emitter(&mut repo, None, stored.emitter_id)
        .is_err()
    {
        crate::stats::inc(&shared.counters.chains.errors);
    }
    Some(stored.emitter_id)
}

/// `plan` retuned to a locked outcome (noise power scaled to the new bandwidth).
pub fn apply_to_plan(plan: &mut AudioPlan, outcome: &RefinementOutcome) {
    if !outcome.locked {
        return;
    }
    let bw = outcome.tuning.bandwidth_hz;
    if plan.channel_bandwidth_hz > 0.0 {
        let scale = bw / plan.channel_bandwidth_hz;
        plan.noise_power = plan.noise_power.map(|n| n * scale);
    }
    plan.channel_center_hz = outcome.tuning.center_hz;
    plan.channel_bandwidth_hz = bw;
}

/// The stream header's refinement record.
pub fn audio_refinement(o: &RefinementOutcome) -> AudioRefinement {
    AudioRefinement {
        provenance: o.provenance.clone(),
        objective: o.objective.clone(),
        center_hz: o.tuning.center_hz,
        bandwidth_hz: o.tuning.bandwidth_hz,
        start_center_hz: o.start.center_hz,
        start_bandwidth_hz: o.start.bandwidth_hz,
        quality: o.quality,
        converged: o.converged,
        iterations: o.iterations,
        evaluations: o.evaluations,
        elapsed_s: o.elapsed_s,
        mode_params: o
            .mode_params
            .iter()
            .filter(|(_, v)| v.is_finite())
            .map(|(k, v)| (k.clone(), *v))
            .collect(),
        labels: o.labels.clone(),
    }
}

/// What Listen's probe decided once refinement ran.
pub(crate) struct ListenProbe {
    /// The probe that decided the mode (re-run on the refined box after a WFM trial).
    pub probe: ProbeResult,
    /// The audio plan, refined when locked.
    pub plan: Result<AudioPlan, String>,
    /// The locked refinement, if any.
    pub refined: Option<RefinementOutcome>,
}

/// Listen's probe with refinement (see the module docs). `iq` holds the whole collected window;
/// `request` is the mode probe's (its end bounds the probe IQ); the selection is `[lo, hi]`;
/// `probe_bandwidth_hz` bounds a re-probe box.
#[allow(clippy::too_many_arguments)]
pub(crate) fn listen_probe(
    settings: &RefineSettings,
    audio: &AudioConfig,
    info: InputInfo<'_>,
    iq: &[Complex<i8>],
    request: &SnippetRequest,
    (lo, hi): (f64, f64),
    probe_bandwidth_hz: (f64, f64),
    first: ProbeResult,
) -> ListenProbe {
    let plan = AudioPlan::from_probe(&first, audio);
    if !settings.enabled {
        return ListenProbe {
            probe: first,
            plan,
            refined: None,
        };
    }
    let window = IqWindow::new(info, iq);
    let start = RefineStart {
        center_hz: 0.5 * (lo + hi),
        bandwidth_hz: (hi - lo).max(0.0),
        warm: false,
    };
    let debug = |what: &str, o: &RefinementOutcome| {
        if crate::debug_enabled() {
            eprintln!(
                "hk-pipeline: listen refine ({what}) {:.4}/{:.0} kHz -> {:.4} MHz / {:.1} kHz, \
                 locked {} ({:?}), quality {:.1}, {} iterations, {} evaluations, {:.2} s",
                start.center_hz / 1e6,
                start.bandwidth_hz / 1e3,
                o.tuning.center_hz / 1e6,
                o.tuning.bandwidth_hz / 1e3,
                o.locked,
                o.stop,
                o.quality,
                o.iterations,
                o.evaluations,
                o.elapsed_s
            );
        }
    };
    if let Ok(p) = &plan
        && refinable(p.mode)
    {
        let refined = refine(settings, p.mode, window, &start).filter(|o| {
            debug("selected mode", o);
            o.locked
        });
        let mut plan = plan;
        if let (Ok(p), Some(o)) = (&mut plan, &refined) {
            apply_to_plan(p, o);
        }
        return ListenProbe {
            probe: first,
            plan,
            refined,
        };
    }
    // A WFM trial: the selection may hold only part of a wideband emission.
    let Some(o) = refine(settings, AnalogMode::Wfm, window, &start).filter(|o| {
        debug("wfm trial", o);
        o.locked
    }) else {
        return ListenProbe {
            probe: first,
            plan,
            refined: None,
        };
    };
    let probe_len = (request.end_index - request.start_index) as usize;
    let box_bw = (2.0 * o.tuning.bandwidth_hz).clamp(probe_bandwidth_hz.0, probe_bandwidth_hz.1);
    let again = SnippetRequest {
        center_offset_hz: o.tuning.center_hz - info.provenance.tune.center_hz,
        bandwidth_hz: box_bw,
        ..*request
    };
    if let Ok(second) = probe(info, &iq[..probe_len.min(iq.len())], &again) {
        let second_plan = AudioPlan::from_probe(&second, audio);
        if let Ok(mut p) = second_plan
            && p.mode == AnalogMode::Wfm
        {
            apply_to_plan(&mut p, &o);
            return ListenProbe {
                probe: second,
                plan: Ok(p),
                refined: Some(o),
            };
        }
    }
    ListenProbe {
        probe: first,
        plan,
        refined: None,
    }
}

/// Re-refines a listening channel in the background (see the module docs).
pub(crate) struct LiveRefiner {
    settings: RefineSettings,
    mode: AnalogMode,
    current: RefinementOutcome,
    interval: Duration,
    next_due: Instant,
    want: usize,
    buf: Vec<Complex<i8>>,
    head: Option<(SampleTime, ProvenanceHandle)>,
    job: Option<JoinHandle<Option<RefinementOutcome>>>,
    updates: u64,
}

impl LiveRefiner {
    /// A refiner starting from the locked `current` result; `None` when live refinement is off or
    /// the mode has no objective.
    pub fn new(
        settings: &RefineSettings,
        mode: AnalogMode,
        current: RefinementOutcome,
        sample_rate_hz: f64,
    ) -> Option<Self> {
        let interval = settings
            .live_interval
            .filter(|_| settings.enabled && refinable(mode) && current.locked)?;
        Some(Self {
            settings: settings.clone(),
            mode,
            current,
            interval,
            next_due: Instant::now() + interval,
            want: ((settings.window_s * sample_rate_hz) as usize).max(1),
            buf: Vec::new(),
            head: None,
            job: None,
            updates: 0,
        })
    }

    /// Accepted updates so far.
    pub fn updates(&self) -> u64 {
        self.updates
    }

    /// Offers contiguous ring samples. Copies only while a window is due and none is being
    /// refined; a full window is handed to a background thread.
    pub fn feed(
        &mut self,
        time: SampleTime,
        provenance: &ProvenanceHandle,
        samples: &[Complex<i8>],
    ) {
        if self.job.is_some() || Instant::now() < self.next_due || samples.is_empty() {
            return;
        }
        let contiguous = self.head.as_ref().is_some_and(|(t, p)| {
            p.id() == provenance.id() && t.sample_index + self.buf.len() as u64 == time.sample_index
        });
        if !contiguous {
            self.buf.clear();
            self.head = Some((time, provenance.clone()));
        }
        let take = (self.want - self.buf.len()).min(samples.len());
        self.buf.extend_from_slice(&samples[..take]);
        if self.buf.len() < self.want {
            return;
        }
        let iq = std::mem::take(&mut self.buf);
        let Some((t0, prov)) = self.head.take() else {
            return;
        };
        let settings = self.settings.clone();
        let mode = self.mode;
        let start = RefineStart {
            center_hz: self.current.tuning.center_hz,
            bandwidth_hz: self.current.tuning.bandwidth_hz,
            warm: true,
        };
        self.job = thread::Builder::new()
            .name("hk-refine".into())
            .spawn(move || {
                let info = InputInfo {
                    time: t0,
                    discontinuity: Discontinuity::NONE,
                    dropped_before: 0,
                    provenance: &prov,
                };
                refine(&settings, mode, IqWindow::new(info, &iq), &start)
            })
            .ok();
        if self.job.is_none() {
            self.next_due = Instant::now() + self.interval;
        }
    }

    /// A finished background refinement that passed the hysteresis: the new tuning in force.
    pub fn poll(&mut self) -> Option<RefinementOutcome> {
        if !self.job.as_ref().is_some_and(JoinHandle::is_finished) {
            return None;
        }
        let next = self.job.take()?.join().ok().flatten();
        self.next_due = Instant::now() + self.interval;
        let next = next?;
        if accept_update(
            &self.settings.loop_config.hysteresis,
            Some(&self.current),
            &next,
        ) {
            self.current = next.clone();
            self.updates += 1;
            Some(next)
        } else {
            None
        }
    }
}
