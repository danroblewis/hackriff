//! Output-driven demodulator parameter refinement (T-070, C19, SIGNAL-062).
//!
//! A detection box or a user selection only says roughly where an emission is. Once a chain knows
//! the mode, it can tune itself from what its own demodulated output shows: a WFM demodulator
//! tuned off-centre produces a DC offset on its MPX and a noisier pilot; one whose filter is too
//! narrow distorts, and the distortion lands in the MPX guard bands. [`RefinementLoop`] turns such
//! measurements into a search over the channel's centre, bandwidth and mode parameters.
//!
//! **Blind.** Nothing here looks at a band plan or a channel raster. The only constants are the
//! modulation's own (the 19 kHz stereo pilot of a WFM MPX), which the demodulator needs anyway. An
//! emission 150 kHz off its raster refines to where it is, never to the nearest channel.
//!
//! # The contract: [`Objective`]
//!
//! A chain plugs in by implementing [`Objective`] for its mode:
//!
//! - **Parameter space** ([`Objective::space`]): the centre range and coarse grid step, the
//!   bandwidth range and step, the nominal bandwidth used while the centre is unknown, the quality
//!   tolerance for choosing a bandwidth, and optional discrete mode axes ([`ModeAxis`], e.g. a
//!   de-emphasis constant or an FSK deviation).
//! - **Measurement** ([`Objective::evaluate`]): demodulate a leading part of the IQ window at one
//!   [`Tuning`] and return a [`Measurement`]: a scalar **quality** (higher is better, in the
//!   objective's own unit, typically dB), a **lock** flag (the output is valid for this mode at
//!   all), an optional **centre correction** (a direct estimate of how far the emission sits from
//!   the tuned centre, which turns the centre search into a few tracking steps), and mode
//!   parameters and labels to report. The [`EvalDepth`] says how much work the loop wants: a quick
//!   look during acquisition, a steady measurement while tracking, the full validation (decoders
//!   included) at the end.
//!
//! # The search (strategy and termination)
//!
//! 1. **Acquire** (skipped on a warm start): the centre grid at the nominal bandwidth, nearest the
//!    start first; the best locked candidate wins, nudged by its centre correction. A correction
//!    larger than one grid step is clamped to one step, as tracking clamps it, and re-measured:
//!    the corrected centre stands only when it locks too.
//! 2. **Track**: re-measure and apply the centre correction until it is within
//!    [`Termination::center_tolerance_hz`] (or the iteration limit). An objective without a
//!    centre estimator gets a shrinking three-point local search on quality instead.
//! 3. **Bandwidth**: from the widest candidate down; the chosen bandwidth is the **narrowest whose
//!    quality is within the space's tolerance of the best** (the narrowest filter the output does
//!    not suffer from). The scan stops after two consecutive candidates below the tolerance.
//! 4. **Mode axes**: each axis in turn, best quality wins.
//! 5. **Validate**: one full measurement at the result; its centre correction is applied once more
//!    and its quality, mode parameters and labels are the outcome's.
//!
//! **A lock is a validated lock.** [`RefinementOutcome::locked`] means the returned tuning was
//! itself measured at [`EvalDepth::Validate`] and that measurement locked. An earlier phase's
//! result never vouches for it: acquisition measures a short window, where a centre one grid step
//! off the carrier can read a *higher* quality than the true centre, and reporting that as a lock
//! retuned live audio 52 kHz off a station (T-188). A search the budget stops before validation is
//! likewise unvalidated, however well an earlier phase measured (T-226).
//!
//! Every measurement checks [`Termination::max_evaluations`] and [`Termination::time_budget`]
//! first; a stopped search returns its best result so far with the [`StopReason`], unlocked. The CPU used is
//! bounded by those two limits and the [`WindowPlan`] lengths, and the loop only reads the slice it
//! is given: callers copy ring data into a window and run the loop off the ring reader's thread
//! (or between reads), live or on replay alike.
//!
//! **Hysteresis** ([`RefinementLoop::accept`]): a live chain that refines again replaces its
//! current tuning only when the new result is locked and moved by more than
//! [`Hysteresis::center_hz`] or [`Hysteresis::bandwidth_hz`] without losing more than
//! [`Hysteresis::quality`] of quality, so the audio does not retune on estimation noise.
//!
//! # Hooks for other modes (not implemented yet)
//!
//! | mode | quality | lock | centre correction | mode axes |
//! |---|---|---|---|---|
//! | WFM ([`WfmObjective`]) | 19 kHz pilot C/N0, dB-Hz | pilot C/N0 above threshold | MPX mean (discriminator DC) | — |
//! | NBFM | audio SNR (voice band vs. above-voice noise), squelch opening | squelch open | discriminator DC | deviation, CTCSS |
//! | AM | audio SNR, carrier-to-noise | carrier line present | carrier line offset | — |
//! | FSK | CRC-valid frame rate, EVM | symbol timing lock | discriminator DC between tones | deviation, symbol rate |
//!
//! # WFM objective
//!
//! See [`WfmObjective`]. The centre comes from the **discriminator mean**: broadcast programme
//! audio, the pilot and the subcarriers are all zero-mean, so the MPX's DC is the carrier offset
//! from the tuned centre (a filter clipping one side biases it towards the emission, which the
//! tracking steps remove). The pilot cannot give the carrier offset: an RF offset adds DC to the
//! MPX but leaves the pilot at 19 kHz. The pilot's measured frequency is instead the **receiver
//! clock** error (the pilot is transmitted at 19 kHz ± 2 Hz), reported as `clock_ppm`.

use std::collections::BTreeMap;
use std::f64::consts::TAU;
use std::time::{Duration, Instant};

use hk_core::Discontinuity;
use hk_dsp::{CpuFft, Ddc, DdcSpec, FftBackend, InputInfo, IqSample};
use num_complex::Complex32;
use serde::{Deserialize, Serialize};

pub use hk_model::REFINED_BY_OUTPUT_ANALYSIS;

use crate::dsp::Discriminator;
use crate::receiver::DemodError;
use crate::wfm::{MPX_RATE_HZ, WfmConfig, WfmDemod};

/// Quality reported for a measurement that could not be made (no output).
pub const QUALITY_FLOOR: f64 = -100.0;

/// A contiguous IQ window the loop measures on.
#[derive(Clone, Copy, Debug)]
pub struct IqWindow<'a, T> {
    /// Metadata of the first sample (tuning, rate, time).
    pub info: InputInfo<'a>,
    /// Samples.
    pub samples: &'a [T],
}

impl<'a, T: IqSample> IqWindow<'a, T> {
    /// A window over `samples`, the first described by `info`.
    pub fn new(info: InputInfo<'a>, samples: &'a [T]) -> Self {
        Self { info, samples }
    }

    /// Sample rate, Hz.
    pub fn rate_hz(&self) -> f64 {
        self.info.provenance.tune.sample_rate_hz
    }

    /// Tuned centre, Hz.
    pub fn tuned_center_hz(&self) -> f64 {
        self.info.provenance.tune.center_hz
    }

    /// Length, s.
    pub fn duration_s(&self) -> f64 {
        self.samples.len() as f64 / self.rate_hz()
    }

    /// The leading `seconds` (the whole window when shorter).
    pub fn leading(&self, seconds: f64) -> Self {
        let n = ((seconds * self.rate_hz()).round().max(0.0) as usize).min(self.samples.len());
        Self {
            info: self.info,
            samples: &self.samples[..n],
        }
    }

    /// `info` marked as a stream start (a fresh demodulator per measurement).
    pub fn stream_start_info(&self) -> InputInfo<'a> {
        InputInfo {
            discontinuity: Discontinuity::STREAM_START,
            dropped_before: 0,
            ..self.info
        }
    }
}

/// One point of the parameter space.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Tuning {
    /// Channel centre, RF Hz (receiver frame).
    pub center_hz: f64,
    /// Channel bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// Mode parameters by axis name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mode: BTreeMap<String, f64>,
}

/// How much work a measurement should do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EvalDepth {
    /// A quick look on a short window (acquisition grid).
    Acquire,
    /// A steady measurement (tracking, bandwidth and mode search).
    Track,
    /// The full measurement at the result, decoders included.
    Validate,
}

/// What an objective measured at one tuning.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Measurement {
    /// Output quality, higher is better (the objective's unit).
    pub quality: f64,
    /// The output is valid for this mode (e.g. the pilot locked).
    pub locked: bool,
    /// Estimated emission centre minus the tuned centre, Hz, when the objective has an estimator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub center_correction_hz: Option<f64>,
    /// Numbers to report (mode-specific).
    #[serde(default)]
    pub mode_params: BTreeMap<String, f64>,
    /// Short texts to report (mode-specific, e.g. a decoded station code).
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

impl Measurement {
    /// No output: floor quality, unlocked, `why` as the `error` label.
    pub fn unlocked(why: impl Into<String>) -> Self {
        let mut labels = BTreeMap::new();
        labels.insert("error".to_owned(), why.into());
        Self {
            quality: QUALITY_FLOOR,
            labels,
            ..Self::default()
        }
    }
}

/// A discrete mode parameter axis.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModeAxis {
    /// Name (the key in [`Tuning::mode`]).
    pub name: String,
    /// Candidate values; the first is the default.
    pub values: Vec<f64>,
}

/// Where the loop may search.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParameterSpace {
    /// Centre range, RF Hz.
    pub center_hz: (f64, f64),
    /// Acquisition grid step, Hz (also the largest single tracking step).
    pub center_step_hz: f64,
    /// Bandwidth range, Hz.
    pub bandwidth_hz: (f64, f64),
    /// Bandwidth step, Hz.
    pub bandwidth_step_hz: f64,
    /// Bandwidth while the centre is searched, Hz.
    pub nominal_bandwidth_hz: f64,
    /// The chosen bandwidth is the narrowest within this much quality of the best.
    pub bandwidth_tolerance: f64,
    /// Discrete mode axes, searched after the bandwidth.
    #[serde(default)]
    pub mode_axes: Vec<ModeAxis>,
}

impl ParameterSpace {
    fn clamp_center(&self, f: f64) -> f64 {
        f.clamp(self.center_hz.0, self.center_hz.1)
    }

    /// Acquisition centres: `start + k·step` inside the range, nearest the start first.
    pub fn center_grid(&self, start_hz: f64) -> Vec<f64> {
        let step = self.center_step_hz.max(1.0);
        let start = self.clamp_center(start_hz);
        let mut out = vec![start];
        for k in 1..=10_000 {
            let mut any = false;
            for f in [start - step * f64::from(k), start + step * f64::from(k)] {
                if f >= self.center_hz.0 - 1e-6 && f <= self.center_hz.1 + 1e-6 {
                    out.push(f);
                    any = true;
                }
            }
            if !any {
                break;
            }
        }
        out
    }

    /// Bandwidth candidates, widest first.
    pub fn bandwidth_candidates(&self) -> Vec<f64> {
        let (lo, hi) = self.bandwidth_hz;
        let step = self.bandwidth_step_hz.max(1.0);
        let mut out = Vec::new();
        let mut b = hi;
        while b >= lo - 1e-6 && out.len() < 1000 {
            out.push(b);
            b -= step;
        }
        out
    }

    fn default_mode(&self) -> BTreeMap<String, f64> {
        self.mode_axes
            .iter()
            .filter_map(|a| a.values.first().map(|v| (a.name.clone(), *v)))
            .collect()
    }
}

/// Where the search starts.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RefineStart {
    /// Coarse centre (the selection or detection box centre), RF Hz.
    pub center_hz: f64,
    /// Coarse width (the box width), Hz.
    pub bandwidth_hz: f64,
    /// A previous refinement's result: search locally, no acquisition grid.
    pub warm: bool,
}

/// When the search stops.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Termination {
    /// Centre converged when the correction is within this, Hz.
    pub center_tolerance_hz: f64,
    /// Most tracking steps.
    pub max_track_iterations: u32,
    /// Most measurements.
    pub max_evaluations: u32,
    /// Wall-clock budget.
    pub time_budget: Duration,
}

impl Default for Termination {
    fn default() -> Self {
        Self {
            center_tolerance_hz: 250.0,
            max_track_iterations: 8,
            max_evaluations: 64,
            time_budget: Duration::from_secs(20),
        }
    }
}

/// Leading window length per [`EvalDepth`], s.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowPlan {
    /// Acquisition measurements.
    pub acquire_s: f64,
    /// Tracking, bandwidth and mode measurements.
    pub track_s: f64,
    /// The validation measurement.
    pub validate_s: f64,
}

impl Default for WindowPlan {
    fn default() -> Self {
        Self {
            acquire_s: 0.1,
            track_s: 0.25,
            validate_s: 1.0,
        }
    }
}

impl WindowPlan {
    fn seconds(&self, depth: EvalDepth) -> f64 {
        match depth {
            EvalDepth::Acquire => self.acquire_s,
            EvalDepth::Track => self.track_s,
            EvalDepth::Validate => self.validate_s,
        }
    }
}

/// Live-update hysteresis.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hysteresis {
    /// Smallest centre move that retunes, Hz.
    pub center_hz: f64,
    /// Smallest bandwidth change that retunes, Hz.
    pub bandwidth_hz: f64,
    /// Largest quality loss accepted with a retune.
    pub quality: f64,
}

impl Default for Hysteresis {
    fn default() -> Self {
        Self {
            center_hz: 1_000.0,
            bandwidth_hz: 30e3,
            quality: 3.0,
        }
    }
}

/// Loop settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LoopConfig {
    /// Termination.
    pub termination: Termination,
    /// Window lengths.
    pub windows: WindowPlan,
    /// Live-update hysteresis.
    pub hysteresis: Hysteresis,
}

/// Why the search ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StopReason {
    /// Every phase ran.
    Completed,
    /// No candidate locked.
    NoLock,
    /// [`Termination::max_evaluations`] reached.
    MaxEvaluations,
    /// [`Termination::time_budget`] used up.
    TimeBudget,
}

/// Search phase of a trace step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    /// Centre grid.
    Acquire,
    /// Centre tracking.
    Track,
    /// Bandwidth scan.
    Bandwidth,
    /// Mode axes.
    Mode,
    /// Final measurement.
    Validate,
}

/// One measurement of the search.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TraceStep {
    /// Phase.
    pub phase: Phase,
    /// Centre, Hz.
    pub center_hz: f64,
    /// Bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// Quality.
    pub quality: f64,
    /// Locked.
    pub locked: bool,
    /// Centre correction, Hz.
    pub center_correction_hz: Option<f64>,
}

/// The result of a refinement.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RefinementOutcome {
    /// Always [`REFINED_BY_OUTPUT_ANALYSIS`].
    pub provenance: String,
    /// Objective name and version.
    pub objective: String,
    /// Mode refined (`wfm`, …).
    pub mode: String,
    /// Where the search started (the coarse box).
    pub start: Tuning,
    /// The refined tuning (the start when nothing locked).
    pub tuning: Tuning,
    /// Quality at the result.
    pub quality: f64,
    /// The result's output locked *at validation depth*: the returned tuning was itself measured
    /// over [`WindowPlan::validate_s`] and that measurement locked. Nothing may retune to an
    /// unlocked result (T-188, T-226).
    pub locked: bool,
    /// The returned tuning was measured at [`EvalDepth::Validate`]. False when the budget
    /// ([`Termination::max_evaluations`], [`Termination::time_budget`]) stopped the search before
    /// validation, or nothing locked; [`Self::locked`] then cannot be true.
    #[serde(default)]
    pub validated: bool,
    /// The centre converged within tolerance and the validation locked.
    pub converged: bool,
    /// Why the search ended.
    pub stop: StopReason,
    /// Search iterations (acquisition, each tracking step, bandwidth scan, each mode axis,
    /// validation).
    pub iterations: u32,
    /// Measurements made.
    pub evaluations: u32,
    /// Wall-clock time, s.
    pub elapsed_s: f64,
    /// Mode parameters from the validation.
    pub mode_params: BTreeMap<String, f64>,
    /// Labels from the validation.
    pub labels: BTreeMap<String, String>,
    /// Every measurement.
    pub trace: Vec<TraceStep>,
}

/// A mode's measurement of demodulated output (see the module docs).
pub trait Objective {
    /// Objective name and version, e.g. `hk-demod/wfm-output@1`.
    fn name(&self) -> &str;

    /// Mode name, e.g. `wfm`.
    fn mode(&self) -> &str;

    /// The space to search from `start` on a window sampled at `rate_hz`.
    fn space(&self, start: &RefineStart, rate_hz: f64) -> ParameterSpace;

    /// Demodulates `window` at `tuning` and measures the output. A tuning that cannot be
    /// demodulated (outside the window's band, too short) is an unlocked measurement, not an
    /// error; errors are reserved for broken configurations.
    fn evaluate<T: IqSample>(
        &mut self,
        window: IqWindow<'_, T>,
        tuning: &Tuning,
        depth: EvalDepth,
    ) -> Result<Measurement, DemodError>;

    /// The occupied bandwidth of the emission centred at `center_hz`, Hz, when the objective can
    /// measure it. The bandwidth search never goes below it (output quality alone is often flat
    /// in bandwidth for a strong emission). Default: not measured.
    fn occupied_bandwidth<T: IqSample>(
        &mut self,
        _window: IqWindow<'_, T>,
        _center_hz: f64,
    ) -> Option<f64> {
        None
    }
}

struct RunState {
    t0: Instant,
    evaluations: u32,
    iterations: u32,
    stop: Option<StopReason>,
    trace: Vec<TraceStep>,
    occupied_bandwidth_hz: Option<f64>,
}

/// The generic search (see the module docs).
pub struct RefinementLoop<O> {
    objective: O,
    config: LoopConfig,
}

impl<O: Objective> RefinementLoop<O> {
    /// A loop over `objective`.
    pub fn new(objective: O, config: LoopConfig) -> Self {
        Self { objective, config }
    }

    /// Settings.
    pub fn config(&self) -> &LoopConfig {
        &self.config
    }

    /// The objective.
    pub fn objective(&self) -> &O {
        &self.objective
    }

    fn eval<T: IqSample>(
        &mut self,
        run: &mut RunState,
        window: IqWindow<'_, T>,
        tuning: &Tuning,
        depth: EvalDepth,
        phase: Phase,
    ) -> Option<Measurement> {
        if run.stop.is_some() {
            return None;
        }
        let term = self.config.termination;
        if run.evaluations >= term.max_evaluations {
            run.stop = Some(StopReason::MaxEvaluations);
            return None;
        }
        if run.t0.elapsed() >= term.time_budget {
            run.stop = Some(StopReason::TimeBudget);
            return None;
        }
        run.evaluations += 1;
        let w = window.leading(self.config.windows.seconds(depth));
        let m = self
            .objective
            .evaluate(w, tuning, depth)
            .unwrap_or_else(|e| Measurement::unlocked(e.to_string()));
        run.trace.push(TraceStep {
            phase,
            center_hz: tuning.center_hz,
            bandwidth_hz: tuning.bandwidth_hz,
            quality: m.quality,
            locked: m.locked,
            center_correction_hz: m.center_correction_hz,
        });
        Some(m)
    }

    /// The best locked centre on the grid, nudged by its correction.
    fn acquire<T: IqSample>(
        &mut self,
        run: &mut RunState,
        window: IqWindow<'_, T>,
        space: &ParameterSpace,
        base: &Tuning,
        from_hz: f64,
    ) -> Option<(Tuning, Measurement)> {
        run.iterations += 1;
        let mut found: Option<(Tuning, Measurement)> = None;
        for c in space.center_grid(from_hz) {
            let t = Tuning {
                center_hz: c,
                ..base.clone()
            };
            let Some(m) = self.eval(run, window, &t, EvalDepth::Acquire, Phase::Acquire) else {
                break;
            };
            if m.locked && found.as_ref().is_none_or(|(_, b)| m.quality > b.quality) {
                found = Some((t, m));
            }
        }
        let (mut t, m) = found?;
        // The correction is applied the way tracking applies it — clamped to one grid step, never
        // discarded (T-226: acquisition discarded an over-step correction while tracking clamped
        // the same quantity). Discarding it left the T-188 decoy in place: a WFM channel filter
        // one step beside the carrier cuts MPX noise while the 19 kHz pilot survives, so the short
        // acquisition window can read a *higher* C/N0 there than at the true centre, and the
        // decoy's own estimator pointed back at the emission by more than one step.
        //
        // An over-step correction is re-measured, and the corrected centre stands only when it
        // locks as well: a correction read off a spurious lock may not drag a good centre away.
        let step = space.center_step_hz;
        if let Some(d) = m.center_correction_hz {
            let c = space.clamp_center(t.center_hz + d.clamp(-step, step));
            if d.abs() <= step {
                t.center_hz = c;
            } else if c != t.center_hz {
                let corrected = Tuning {
                    center_hz: c,
                    ..t.clone()
                };
                if let Some(n) =
                    self.eval(run, window, &corrected, EvalDepth::Acquire, Phase::Acquire)
                    && n.locked
                {
                    return Some((corrected, n));
                }
            }
        }
        Some((t, m))
    }

    /// Runs the search on `window` from `start`.
    pub fn run<T: IqSample>(
        &mut self,
        window: IqWindow<'_, T>,
        start: &RefineStart,
    ) -> RefinementOutcome {
        let mut run = RunState {
            t0: Instant::now(),
            evaluations: 0,
            iterations: 0,
            stop: None,
            trace: Vec::new(),
            occupied_bandwidth_hz: None,
        };
        let space = self.objective.space(start, window.rate_hz());
        let start_tuning = Tuning {
            center_hz: start.center_hz,
            bandwidth_hz: start.bandwidth_hz,
            mode: BTreeMap::new(),
        };
        let tol = self.config.termination.center_tolerance_hz;
        let step = space.center_step_hz;
        let mut tuning = Tuning {
            center_hz: space.clamp_center(start.center_hz),
            bandwidth_hz: if start.warm {
                start
                    .bandwidth_hz
                    .clamp(space.bandwidth_hz.0, space.bandwidth_hz.1)
            } else {
                space.nominal_bandwidth_hz
            },
            mode: space.default_mode(),
        };
        let mut last: Option<Measurement> = None;

        // 1. Acquire.
        if !start.warm {
            match self.acquire(&mut run, window, &space, &tuning, start.center_hz) {
                Some((t, m)) => {
                    tuning = t;
                    last = Some(m);
                }
                None => return self.finish(run, start_tuning, None, None, false, false),
            }
        }

        // 2. Track.
        let mut center_converged = false;
        let mut s = 0.5 * step;
        for i in 0..self.config.termination.max_track_iterations {
            let Some(m) = self.eval(&mut run, window, &tuning, EvalDepth::Track, Phase::Track)
            else {
                break;
            };
            run.iterations += 1;
            if !m.locked {
                if i == 0 && start.warm {
                    // The warm tuning lost the emission: acquire around it.
                    match self.acquire(&mut run, window, &space, &tuning, start.center_hz) {
                        Some((t, m)) => {
                            tuning = t;
                            last = Some(m);
                            continue;
                        }
                        None => return self.finish(run, start_tuning, None, None, false, false),
                    }
                }
                break;
            }
            match m.center_correction_hz {
                Some(d) => {
                    last = Some(m);
                    if d.abs() <= tol {
                        center_converged = true;
                        break;
                    }
                    tuning.center_hz = space.clamp_center(tuning.center_hz + d.clamp(-step, step));
                }
                None => {
                    // Three-point local search on quality with a shrinking step.
                    let q0 = m.quality;
                    last = Some(m);
                    if s < tol {
                        center_converged = true;
                        break;
                    }
                    let mut best = (tuning.center_hz, q0);
                    for f in [tuning.center_hz - s, tuning.center_hz + s] {
                        let t = Tuning {
                            center_hz: space.clamp_center(f),
                            ..tuning.clone()
                        };
                        if let Some(n) =
                            self.eval(&mut run, window, &t, EvalDepth::Track, Phase::Track)
                            && n.locked
                            && n.quality > best.1
                        {
                            best = (t.center_hz, n.quality);
                        }
                    }
                    tuning.center_hz = best.0;
                    s *= 0.5;
                }
            }
        }
        if last.as_ref().is_none_or(|m| !m.locked) {
            return self.finish(run, start_tuning, None, None, false, false);
        }

        // 3. Bandwidth: the narrowest at or above the occupied bandwidth within tolerance of the
        //    best.
        run.iterations += 1;
        let (lo_bw, hi_bw) = space.bandwidth_hz;
        let obw = if run.stop.is_none() && run.t0.elapsed() < self.config.termination.time_budget {
            let w = window.leading(self.config.windows.track_s);
            self.objective
                .occupied_bandwidth(w, tuning.center_hz)
                .filter(|o| o.is_finite() && *o > 0.0)
        } else {
            None
        };
        run.occupied_bandwidth_hz = obw;
        let floor = obw.map(|o| o.clamp(lo_bw, hi_bw));
        let mut candidates: Vec<f64> = space
            .bandwidth_candidates()
            .into_iter()
            .filter(|b| floor.is_none_or(|f| *b > f + 1.0))
            .collect();
        if let Some(f) = floor {
            candidates.push(f);
        }
        let mut scored: Vec<(f64, f64)> = Vec::new();
        let mut best_q = f64::NEG_INFINITY;
        let mut below = 0;
        for bw in candidates {
            let t = Tuning {
                bandwidth_hz: bw,
                ..tuning.clone()
            };
            let Some(m) = self.eval(&mut run, window, &t, EvalDepth::Track, Phase::Bandwidth)
            else {
                break;
            };
            if m.locked {
                best_q = best_q.max(m.quality);
                scored.push((bw, m.quality));
            }
            if !m.locked || m.quality < best_q - space.bandwidth_tolerance {
                below += 1;
                if below >= 2 {
                    break;
                }
            } else {
                below = 0;
            }
        }
        if let Some(bw) = scored
            .iter()
            .filter(|(_, q)| *q >= best_q - space.bandwidth_tolerance)
            .map(|(b, _)| *b)
            .reduce(f64::min)
        {
            tuning.bandwidth_hz = bw;
        }

        // 4. Mode axes.
        for axis in &space.mode_axes {
            run.iterations += 1;
            let mut best: Option<(f64, f64)> = None;
            for &v in &axis.values {
                let mut t = tuning.clone();
                t.mode.insert(axis.name.clone(), v);
                let Some(m) = self.eval(&mut run, window, &t, EvalDepth::Track, Phase::Mode) else {
                    break;
                };
                if m.locked && best.is_none_or(|(_, q)| m.quality > q) {
                    best = Some((v, m.quality));
                }
            }
            if let Some((v, _)) = best {
                tuning.mode.insert(axis.name.clone(), v);
            }
        }

        // 5. Validate.
        let validation = self.eval(
            &mut run,
            window,
            &tuning,
            EvalDepth::Validate,
            Phase::Validate,
        );
        let validated = validation.is_some();
        let mut validated_locked = false;
        let final_m = match validation {
            Some(m) => {
                run.iterations += 1;
                if m.locked {
                    validated_locked = true;
                    if let Some(d) = m.center_correction_hz.filter(|d| d.abs() <= step) {
                        tuning.center_hz = space.clamp_center(tuning.center_hz + d);
                        center_converged = center_converged || d.abs() <= tol;
                    }
                }
                // The validation measurement stands for the returned tuning whether or not it
                // locked: an earlier phase's result never vouches for it. Acquisition measures a
                // short window (`WindowPlan::acquire_s`), where a centre one grid step off the
                // carrier can read a *higher* pilot C/N0 than the true centre — the channel
                // filter, centred off the emission, cuts MPX noise while the 19 kHz pilot
                // survives. Keeping that measurement here reported an unvalidated centre as a
                // locked refinement (T-188).
                Some(m)
            }
            // The budget stopped the search before validation (T-226: `max_evaluations` or
            // `time_budget`, which CPU load is exactly what triggers). The last measurement was
            // made at another tuning or another depth and cannot vouch for this one, so the
            // outcome is reported unvalidated and nothing downstream retunes to it.
            None => last,
        };
        let converged = center_converged && validated_locked;
        self.finish(
            run,
            start_tuning,
            Some(tuning),
            final_m,
            validated,
            converged,
        )
    }

    fn finish(
        &self,
        run: RunState,
        start: Tuning,
        tuning: Option<Tuning>,
        m: Option<Measurement>,
        validated: bool,
        converged: bool,
    ) -> RefinementOutcome {
        let locked = validated && tuning.is_some() && m.as_ref().is_some_and(|m| m.locked);
        let stop = run.stop.unwrap_or(if locked {
            StopReason::Completed
        } else {
            StopReason::NoLock
        });
        let mut m = m.unwrap_or_default();
        if let Some(o) = run.occupied_bandwidth_hz {
            m.mode_params.insert("occupied_bandwidth_hz".to_owned(), o);
        }
        RefinementOutcome {
            provenance: REFINED_BY_OUTPUT_ANALYSIS.to_owned(),
            objective: self.objective.name().to_owned(),
            mode: self.objective.mode().to_owned(),
            tuning: tuning.unwrap_or_else(|| start.clone()),
            start,
            quality: if locked { m.quality } else { QUALITY_FLOOR },
            locked,
            validated,
            converged,
            stop,
            iterations: run.iterations,
            evaluations: run.evaluations,
            elapsed_s: run.t0.elapsed().as_secs_f64(),
            mode_params: m.mode_params,
            labels: m.labels,
            trace: run.trace,
        }
    }

    /// Hysteresis for live updates: whether `next` should replace `current`. An unlocked result
    /// never does; with no locked current tuning a locked result always does; otherwise the
    /// tuning must have moved by more than the centre or bandwidth hysteresis without losing more
    /// than the quality hysteresis.
    pub fn accept(&self, current: Option<&RefinementOutcome>, next: &RefinementOutcome) -> bool {
        accept_update(&self.config.hysteresis, current, next)
    }
}

/// [`RefinementLoop::accept`] with explicit settings.
pub fn accept_update(
    h: &Hysteresis,
    current: Option<&RefinementOutcome>,
    next: &RefinementOutcome,
) -> bool {
    if !next.locked {
        return false;
    }
    let Some(cur) = current.filter(|c| c.locked) else {
        return true;
    };
    let moved = (next.tuning.center_hz - cur.tuning.center_hz).abs() > h.center_hz
        || (next.tuning.bandwidth_hz - cur.tuning.bandwidth_hz).abs() > h.bandwidth_hz;
    moved && next.quality >= cur.quality - h.quality
}

// ------------------------------------------------------------------------------------ WFM

/// Name and version of [`WfmObjective`].
pub const WFM_OBJECTIVE: &str = "hk-demod/wfm-output@1";

/// WFM objective settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WfmObjectiveConfig {
    /// Bandwidth range, Hz (at most ~220 kHz fits the 240 kS/s MPX rate).
    pub bandwidth_hz: (f64, f64),
    /// Bandwidth step, Hz.
    pub bandwidth_step_hz: f64,
    /// Bandwidth while the centre is searched, Hz.
    pub nominal_bandwidth_hz: f64,
    /// Bandwidth tolerance, dB of pilot C/N0.
    pub bandwidth_tolerance_db: f64,
    /// Acquisition grid step, Hz.
    pub center_step_hz: f64,
    /// Centre search range beyond the start box on each side, Hz.
    pub search_margin_hz: f64,
    /// Nominal stereo pilot, Hz (the modulation standard, not a band plan).
    pub pilot_hz: f64,
    /// Pilot search half-width, Hz.
    pub pilot_search_hz: f64,
    /// Lock when the pilot C/N0 reaches this, dB-Hz.
    pub lock_pilot_cn0_dbhz: f64,
    /// MPX FFT length.
    pub fft_len: usize,
    /// DDC settling time skipped per measurement, s.
    pub settle_s: f64,
    /// Decode RDS during validation.
    pub rds: bool,
    /// Occupied bandwidth: the power fraction it contains.
    pub obw_fraction: f64,
    /// Occupied bandwidth: two-sided RF analysis band around the centre, Hz.
    pub obw_analysis_hz: f64,
    /// Occupied bandwidth: half-width the emission's power is summed over, Hz (keeps adjacent
    /// channels out).
    pub obw_half_span_hz: f64,
    /// Occupied bandwidth: percentile of the analysis band's bins taken as the noise floor.
    pub obw_floor_percentile: f64,
    /// x-dB bandwidth: level below the peak, dB.
    pub obw_x_db: f64,
    /// x-dB bandwidth: density smoothing width, Hz.
    pub obw_smooth_hz: f64,
}

impl Default for WfmObjectiveConfig {
    fn default() -> Self {
        Self {
            bandwidth_hz: (100e3, 220e3),
            bandwidth_step_hz: 20e3,
            nominal_bandwidth_hz: 200e3,
            // Pilot C/N0 over a 0.25 s window scatters by about ±1.5 dB between bandwidths on a
            // strong station (fm_100p8M fixture): a smaller tolerance picks on noise.
            bandwidth_tolerance_db: 3.0,
            center_step_hz: 50e3,
            search_margin_hz: 100e3,
            pilot_hz: 19_000.0,
            pilot_search_hz: 60.0,
            lock_pilot_cn0_dbhz: 40.0,
            fft_len: 4096,
            settle_s: 0.01,
            rds: true,
            obw_fraction: 0.99,
            obw_analysis_hz: 400e3,
            obw_half_span_hz: 150e3,
            obw_floor_percentile: 0.2,
            obw_x_db: 26.0,
            obw_smooth_hz: 5e3,
        }
    }
}

/// An RF occupied-bandwidth measurement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ObwMeasure {
    /// Bandwidth holding the power fraction, Hz (ITU: equal power left out on either side).
    pub obw_hz: f64,
    /// Middle of that band minus the measured centre, Hz.
    pub obw_center_offset_hz: f64,
    /// Width where the smoothed noise-subtracted density is within `obw_x_db` of its peak, Hz.
    pub x_db_hz: f64,
    /// Noise floor density, dB (full-scale²/Hz).
    pub floor_db: f64,
    /// Emission power over the noise in the summed span, dB.
    pub snr_db: f64,
}

/// Everything one WFM measurement computes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WfmMetrics {
    /// MPX samples measured.
    pub mpx_samples: usize,
    /// MPX mean (discriminator DC): the carrier offset from the tuned centre, Hz.
    pub mpx_mean_hz: f64,
    /// 99.9th percentile of |deviation| about the mean, Hz.
    pub peak_deviation_hz: f64,
    /// Pilot power over the guard-band noise density, dB-Hz.
    pub pilot_cn0_dbhz: f64,
    /// Pilot frequency from the MPX spectrum peak (interpolated), Hz.
    pub pilot_fft_hz: f64,
    /// Guard-band (15.5–18.5 and 19.5–22.5 kHz) noise density, dB Hz²/Hz.
    pub guard_density_db: f64,
    /// Mono audio band (0.05–15 kHz) power over guard-band noise, dB.
    pub audio_snr_db: f64,
    /// 57 kHz RDS band power over the 60–64 kHz noise, dB.
    pub rds_band_snr_db: f64,
}

/// WFM broadcast output objective (see the module docs).
///
/// - **Quality:** the 19 kHz pilot's power over the MPX guard-band noise density (dB-Hz). A
///   filter that is too narrow or off-centre distorts, and the distortion products land in the
///   guard bands on either side of the pilot; noise and adjacent channels raise them too.
/// - **Lock:** pilot C/N0 at or above [`WfmObjectiveConfig::lock_pilot_cn0_dbhz`]. A mono station
///   without a pilot does not lock (not refined; its coarse tuning stays).
/// - **Centre correction:** the MPX mean.
/// - **Validation:** the full [`WfmDemod`] with RDS: PLL pilot frequency (`pilot_hz`), receiver
///   clock error from it (`clock_ppm`), peak deviation, RDS PI (label `rds_pi`) and block/group
///   error rates.
pub struct WfmObjective {
    config: WfmObjectiveConfig,
    fft: CpuFft,
    window: Vec<f32>,
    window_power: f64,
    seg: Vec<Complex32>,
    mpx: Vec<f32>,
    psd: Vec<f64>,
}

impl Default for WfmObjective {
    fn default() -> Self {
        Self::new(WfmObjectiveConfig::default())
    }
}

impl WfmObjective {
    /// An objective.
    pub fn new(config: WfmObjectiveConfig) -> Self {
        let n = config.fft_len.max(256);
        let window: Vec<f32> = (0..n)
            .map(|i| (0.5 - 0.5 * (TAU * i as f64 / n as f64).cos()) as f32)
            .collect();
        let window_power = window.iter().map(|w| f64::from(*w).powi(2)).sum();
        Self {
            fft: CpuFft::new(n),
            window,
            window_power,
            seg: vec![Complex32::default(); n],
            mpx: Vec::new(),
            psd: vec![0.0; n / 2],
            config,
        }
    }

    /// Settings.
    pub fn config(&self) -> &WfmObjectiveConfig {
        &self.config
    }

    /// Demodulates `window` at `tuning` to MPX and measures it; `None` when the tuning cannot be
    /// demodulated (outside the band, window too short). With `demod`, the DDC output is also fed
    /// to it.
    pub fn measure<T: IqSample>(
        &mut self,
        window: IqWindow<'_, T>,
        tuning: &Tuning,
        mut demod: Option<&mut WfmDemod>,
    ) -> Result<Option<WfmMetrics>, DemodError> {
        let c = self.config;
        let fs_in = window.rate_hz();
        let bw = tuning.bandwidth_hz;
        let offset = tuning.center_hz - window.tuned_center_hz();
        if !(bw > 0.0 && bw < 0.95 * MPX_RATE_HZ) || offset.abs() + 0.5 * bw > 0.5 * fs_in {
            return Ok(None);
        }
        let fs = MPX_RATE_HZ;
        let mut ddc = Ddc::new(DdcSpec::new(offset, bw).with_output_rate(fs), fs_in)?;
        let block = ddc.process(window.stream_start_info(), window.samples)?;
        let x = block.samples;
        if let Some(d) = demod.as_mut() {
            d.process(x);
        }
        let n = self.window.len();
        let skip = ((c.settle_s * fs) as usize).max(1);
        if x.len() < skip + 2 * n {
            return Ok(None);
        }
        let mut disc = Discriminator::new(fs);
        disc.push(x[skip - 1]);
        self.mpx.clear();
        self.mpx.extend(x[skip..].iter().map(|&s| disc.push(s)));
        let mpx = &self.mpx;
        let count = mpx.len();
        let mean = mpx.iter().map(|&v| f64::from(v)).sum::<f64>() / count as f64;

        // Peak deviation: 99.9th percentile of |f − mean| in 1 kHz bins.
        let mut hist = [0u32; 256];
        for &v in mpx {
            let b = ((f64::from(v) - mean).abs() / 1000.0) as usize;
            hist[b.min(255)] += 1;
        }
        let target = (count as f64 * 0.999).ceil() as u64;
        let mut acc = 0u64;
        let mut peak_k = 255;
        for (k, &h) in hist.iter().enumerate() {
            acc += u64::from(h);
            if acc >= target {
                peak_k = k;
                break;
            }
        }

        // Averaged MPX density, Hz²/Hz, bins 0..n/2.
        self.psd.iter_mut().for_each(|p| *p = 0.0);
        let segments = count / n;
        let meanf = mean as f32;
        for s in 0..segments {
            let chunk = &mpx[s * n..(s + 1) * n];
            for ((o, &v), &w) in self.seg.iter_mut().zip(chunk).zip(&self.window) {
                *o = Complex32::new((v - meanf) * w, 0.0);
            }
            self.fft.forward(&mut self.seg);
            for (p, z) in self.psd.iter_mut().zip(&self.seg) {
                *p += f64::from(z.norm_sqr());
            }
        }
        let norm = 1.0 / (segments as f64 * self.window_power * fs);
        let bin = fs / n as f64;
        // One-sided: a real signal's power is split between ±f.
        self.psd.iter_mut().for_each(|p| *p *= 2.0 * norm);
        let psd = &self.psd;
        let idx = |hz: f64| ((hz / bin).round() as usize).min(psd.len() - 1);
        let median = |ranges: &[(f64, f64)]| {
            let mut v: Vec<f64> = ranges
                .iter()
                .flat_map(|&(lo, hi)| psd[idx(lo)..=idx(hi)].iter().copied())
                .collect();
            v.sort_by(f64::total_cmp);
            v.get(v.len() / 2).copied().unwrap_or(0.0).max(1e-30)
        };
        let band_power = |lo: f64, hi: f64| psd[idx(lo)..=idx(hi)].iter().sum::<f64>() * bin;
        let guard = median(&[(15_500.0, 18_500.0), (19_500.0, 22_500.0)]);

        let (plo, phi) = (
            idx(c.pilot_hz - c.pilot_search_hz),
            idx(c.pilot_hz + c.pilot_search_hz),
        );
        let k = (plo..=phi)
            .max_by(|&a, &b| psd[a].total_cmp(&psd[b]))
            .unwrap_or(plo);
        let lobe = psd[k.saturating_sub(2)..=(k + 2).min(psd.len() - 1)]
            .iter()
            .sum::<f64>()
            * bin;
        let pilot_power = (lobe - 5.0 * guard * bin).max(1e-30);
        let (a, b0, g) = (
            psd[k.saturating_sub(1)],
            psd[k],
            psd[(k + 1).min(psd.len() - 1)],
        );
        let den = a - 2.0 * b0 + g;
        let frac = if den.abs() > 1e-30 {
            (0.5 * (a - g) / den).clamp(-0.5, 0.5)
        } else {
            0.0
        };
        let audio = band_power(50.0, 15_000.0);
        let audio_noise = guard * 14_950.0;
        let rds_noise = median(&[(60_000.0, 64_000.0)]) * 2_800.0;
        let rds = band_power(55_600.0, 58_400.0);
        let db = |r: f64| 10.0 * r.max(1e-30).log10();
        Ok(Some(WfmMetrics {
            mpx_samples: count,
            mpx_mean_hz: mean,
            peak_deviation_hz: (peak_k + 1) as f64 * 1000.0,
            pilot_cn0_dbhz: db(pilot_power / guard),
            pilot_fft_hz: (k as f64 + frac) * bin,
            guard_density_db: db(guard),
            audio_snr_db: db((audio - audio_noise).max(1e-30) / audio_noise),
            rds_band_snr_db: db((rds - rds_noise).max(1e-30) / rds_noise),
        }))
    }

    /// The RF occupied bandwidth of the emission centred at `center_hz`: the noise-subtracted
    /// power spectrum of the channel (a DDC over [`WfmObjectiveConfig::obw_analysis_hz`]), the
    /// noise floor its [`WfmObjectiveConfig::obw_floor_percentile`], and the narrowest band
    /// symmetric about the centre that holds [`WfmObjectiveConfig::obw_fraction`] of the power
    /// within [`WfmObjectiveConfig::obw_half_span_hz`]. `None` outside the window's band, on a
    /// too-short window or without power above the floor.
    pub fn measure_obw<T: IqSample>(
        &mut self,
        window: IqWindow<'_, T>,
        center_hz: f64,
    ) -> Result<Option<ObwMeasure>, DemodError> {
        let c = self.config;
        let fs_in = window.rate_hz();
        let span = c.obw_analysis_hz;
        let out = 1.2 * span;
        let offset = center_hz - window.tuned_center_hz();
        if span.is_nan() || span <= 0.0 || offset.abs() + 0.5 * span > 0.5 * fs_in || out > fs_in {
            return Ok(None);
        }
        let mut ddc = Ddc::new(DdcSpec::new(offset, span).with_output_rate(out), fs_in)?;
        let block = ddc.process(window.stream_start_info(), window.samples)?;
        let n = 1024;
        let skip = (c.settle_s * out) as usize;
        let x = block.samples;
        if x.len() < skip + 4 * n {
            return Ok(None);
        }
        let win = &self.window;
        let step = win.len() / n;
        let mut fft = CpuFft::new(n);
        let mut buf = vec![Complex32::default(); n];
        let mut psd = vec![0.0f64; n];
        let mut segments = 0usize;
        for seg in x[skip..].chunks_exact(n) {
            for (i, (o, &s)) in buf.iter_mut().zip(seg).enumerate() {
                *o = s * win[i * step];
            }
            fft.forward(&mut buf);
            for (p, z) in psd.iter_mut().zip(&buf) {
                *p += f64::from(z.norm_sqr());
            }
            segments += 1;
        }
        let wpow: f64 = (0..n).map(|i| f64::from(win[i * step]).powi(2)).sum();
        let norm = 1.0 / (segments as f64 * wpow * out);
        let bin = out / n as f64;
        let flat = ((0.5 * span) / bin) as usize;
        // |f| = j·bin → the bins at +j and −j.
        let at = |j: usize| -> (f64, Option<f64>) {
            let pos = psd[j] * norm;
            let neg = (j > 0).then(|| psd[n - j] * norm);
            (pos, neg)
        };
        let mut all: Vec<f64> = (0..=flat.min(n / 2 - 1))
            .flat_map(|j| {
                let (p, q) = at(j);
                std::iter::once(p).chain(q)
            })
            .collect();
        all.sort_by(f64::total_cmp);
        let floor = all[((all.len() - 1) as f64 * c.obw_floor_percentile.clamp(0.0, 1.0)) as usize]
            .max(1e-30);
        let half = ((c.obw_half_span_hz / bin) as usize)
            .min(flat)
            .min(n / 2 - 1);
        // Noise-subtracted density from −half to +half bins (index i ↔ f = (i − half)·bin).
        let excess: Vec<f64> = (0..=2 * half)
            .map(|i| {
                let d = if i >= half {
                    psd[i - half]
                } else {
                    psd[n - (half - i)]
                };
                (d * norm - floor).max(0.0)
            })
            .collect();
        let total: f64 = excess.iter().sum();
        let noise = floor * excess.len() as f64;
        if total <= noise * 0.1 {
            return Ok(None);
        }
        // ITU occupied bandwidth: (1 − fraction)/2 of the power below the lower edge and above
        // the upper edge.
        let tail = 0.5 * (1.0 - c.obw_fraction.clamp(0.0, 1.0)) * total;
        let edge = |target: f64| {
            let mut acc = 0.0;
            for (i, e) in excess.iter().enumerate() {
                if acc + e >= target {
                    let frac = if *e > 0.0 { (target - acc) / e } else { 0.0 };
                    return (i as f64 - 0.5 + frac - half as f64) * bin;
                }
                acc += e;
            }
            (half as f64 + 0.5) * bin
        };
        let (lo, hi) = (edge(tail), edge(total - tail));
        // x-dB bandwidth on the smoothed excess density.
        let k = ((c.obw_smooth_hz / bin) as usize).max(1);
        let smooth: Vec<f64> = (0..excess.len())
            .map(|i| {
                let a = i.saturating_sub(k / 2);
                let b = (i + k / 2 + 1).min(excess.len());
                excess[a..b].iter().sum::<f64>() / (b - a) as f64
            })
            .collect();
        let peak = smooth.iter().copied().fold(0.0, f64::max);
        let thr = peak * 10f64.powf(-c.obw_x_db / 10.0);
        let first = smooth.iter().position(|v| *v >= thr).unwrap_or(0);
        let last = smooth.iter().rposition(|v| *v >= thr).unwrap_or(0);
        Ok(Some(ObwMeasure {
            obw_hz: hi - lo,
            obw_center_offset_hz: 0.5 * (lo + hi),
            x_db_hz: (last - first + 1) as f64 * bin,
            floor_db: 10.0 * floor.log10(),
            snr_db: 10.0 * (total / noise).log10(),
        }))
    }
}

impl Objective for WfmObjective {
    fn name(&self) -> &str {
        WFM_OBJECTIVE
    }

    fn mode(&self) -> &str {
        "wfm"
    }

    fn space(&self, start: &RefineStart, _rate_hz: f64) -> ParameterSpace {
        let c = &self.config;
        let half = if start.warm {
            c.center_step_hz
        } else {
            0.5 * start.bandwidth_hz.max(0.0) + c.search_margin_hz
        };
        ParameterSpace {
            center_hz: (start.center_hz - half, start.center_hz + half),
            center_step_hz: c.center_step_hz,
            bandwidth_hz: c.bandwidth_hz,
            bandwidth_step_hz: c.bandwidth_step_hz,
            nominal_bandwidth_hz: c.nominal_bandwidth_hz,
            bandwidth_tolerance: c.bandwidth_tolerance_db,
            mode_axes: Vec::new(),
        }
    }

    fn evaluate<T: IqSample>(
        &mut self,
        window: IqWindow<'_, T>,
        tuning: &Tuning,
        depth: EvalDepth,
    ) -> Result<Measurement, DemodError> {
        let validate = depth == EvalDepth::Validate;
        let mut demod = if validate {
            let cfg = WfmConfig {
                rds: if self.config.rds {
                    WfmConfig::default().rds
                } else {
                    None
                },
                ..WfmConfig::default()
            };
            Some(WfmDemod::new(cfg, MPX_RATE_HZ)?)
        } else {
            None
        };
        let Some(m) = self.measure(window, tuning, demod.as_mut())? else {
            return Ok(Measurement::unlocked(
                "tuning outside the window or window too short",
            ));
        };
        let locked = m.pilot_cn0_dbhz >= self.config.lock_pilot_cn0_dbhz;
        let mut p = BTreeMap::new();
        p.insert("pilot_cn0_dbhz".to_owned(), m.pilot_cn0_dbhz);
        p.insert("audio_snr_db".to_owned(), m.audio_snr_db);
        p.insert("rds_band_snr_db".to_owned(), m.rds_band_snr_db);
        p.insert("peak_deviation_hz".to_owned(), m.peak_deviation_hz);
        p.insert("mpx_mean_hz".to_owned(), m.mpx_mean_hz);
        let mut labels = BTreeMap::new();
        if let Some(r) = demod.map(|d| d.report()) {
            if let Some(f) = r.pilot.frequency_hz.filter(|_| r.pilot.present) {
                p.insert("pilot_hz".to_owned(), f);
                if let Some(s) = r.pilot.sigma_hz {
                    p.insert("pilot_sigma_hz".to_owned(), s);
                }
                // Receiver clock against the transmitted pilot (negative reads frequencies low),
                // only when the PLL's uncertainty is under 1 ppm of the pilot: a short window's
                // estimate includes the loop's pull-in and can be tens of ppm off.
                if r.pilot.sigma_hz.is_some_and(|s| s / f * 1e6 < 1.0) {
                    p.insert(
                        "clock_ppm".to_owned(),
                        (f / self.config.pilot_hz - 1.0) * 1e6,
                    );
                }
            }
            p.insert("stereo".to_owned(), f64::from(u8::from(r.stereo)));
            if let Some(d) = r.peak_deviation_hz {
                p.insert("peak_deviation_hz".to_owned(), d);
            }
            if let Some(rds) = &r.rds {
                p.insert("rds_blocks_ok".to_owned(), rds.blocks_ok as f64);
                p.insert("rds_groups_ok".to_owned(), rds.groups_ok as f64);
                if let Some(e) = rds.block_error_rate {
                    p.insert("rds_block_error_rate".to_owned(), e);
                }
                if let Some(e) = rds.group_error_rate {
                    p.insert("rds_group_error_rate".to_owned(), e);
                }
                if let Some(pi) = &rds.pi {
                    labels.insert("rds_pi".to_owned(), pi.hex());
                }
            }
        }
        let half = 0.5 * tuning.bandwidth_hz;
        Ok(Measurement {
            quality: m.pilot_cn0_dbhz,
            locked,
            center_correction_hz: Some(m.mpx_mean_hz).filter(|d| d.abs() < half),
            mode_params: p,
            labels,
        })
    }

    fn occupied_bandwidth<T: IqSample>(
        &mut self,
        window: IqWindow<'_, T>,
        center_hz: f64,
    ) -> Option<f64> {
        self.measure_obw(window, center_hz)
            .ok()
            .flatten()
            .map(|o| o.obw_hz)
    }
}

/// Refines a WFM channel on `window` from `start` with default settings.
pub fn refine_wfm<T: IqSample>(window: IqWindow<'_, T>, start: &RefineStart) -> RefinementOutcome {
    RefinementLoop::new(WfmObjective::default(), LoopConfig::default()).run(window, start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_core::ProvenanceHandle;
    use hk_model::{SampleTime, Timestamp};

    /// A toy objective: quality peaks at 1 000 Hz, bandwidth 30 kHz or wider loses nothing, no
    /// centre estimator (exercises the local search).
    struct Toy {
        with_estimator: bool,
    }

    impl Objective for Toy {
        fn name(&self) -> &str {
            "toy@1"
        }
        fn mode(&self) -> &str {
            "toy"
        }
        fn space(&self, start: &RefineStart, _: f64) -> ParameterSpace {
            ParameterSpace {
                center_hz: (start.center_hz - 20e3, start.center_hz + 20e3),
                center_step_hz: 5e3,
                bandwidth_hz: (10e3, 50e3),
                bandwidth_step_hz: 10e3,
                nominal_bandwidth_hz: 40e3,
                bandwidth_tolerance: 1.0,
                mode_axes: vec![ModeAxis {
                    name: "k".into(),
                    values: vec![1.0, 2.0, 3.0],
                }],
            }
        }
        fn evaluate<T: IqSample>(
            &mut self,
            _: IqWindow<'_, T>,
            t: &Tuning,
            _: EvalDepth,
        ) -> Result<Measurement, DemodError> {
            let off = 1_000.0 - t.center_hz;
            let bw_loss = if t.bandwidth_hz >= 30e3 {
                0.0
            } else {
                (30e3 - t.bandwidth_hz) / 2e3
            };
            let k = t.mode.get("k").copied().unwrap_or(1.0);
            Ok(Measurement {
                quality: 40.0 - (off / 1e3).powi(2) - bw_loss - (k - 2.0).abs(),
                locked: off.abs() < 12e3,
                center_correction_hz: self.with_estimator.then_some(off),
                ..Measurement::default()
            })
        }
    }

    fn prov() -> ProvenanceHandle {
        let p: hk_model::Provenance = serde_json::from_value(serde_json::json!({
            "device_id": "synthetic:refine", "tune": { "center_hz": 0.0, "sample_rate_hz": 1e6,
            "lna_db": 0.0, "vga_db": 0.0, "amp_on": false, "bandwidth_hz": 1e6 },
            "overload": false, "quantisation_limited": false, "clock_source": "internal",
            "clock_locked": true, "timestamp_method": "synthetic", "timestamp_error_budget_ns": 0,
        }))
        .unwrap();
        ProvenanceHandle::new(p)
    }

    fn window<'a>(p: &'a ProvenanceHandle, x: &'a [Complex32]) -> IqWindow<'a, Complex32> {
        IqWindow::new(
            InputInfo {
                time: SampleTime {
                    sample_index: 0,
                    host_time: Timestamp::from_unix_nanos(0),
                },
                discontinuity: Discontinuity::NONE,
                dropped_before: 0,
                provenance: p,
            },
            x,
        )
    }

    #[test]
    fn the_loop_finds_centre_bandwidth_and_mode_with_and_without_an_estimator() {
        let p = prov();
        let x = vec![Complex32::default(); 16];
        for with_estimator in [true, false] {
            let mut l = RefinementLoop::new(Toy { with_estimator }, LoopConfig::default());
            let start = RefineStart {
                center_hz: 9_000.0,
                bandwidth_hz: 5e3,
                warm: false,
            };
            let o = l.run(window(&p, &x), &start);
            assert!(o.locked && o.stop == StopReason::Completed, "{o:?}");
            assert!((o.tuning.center_hz - 1_000.0).abs() <= 400.0, "{o:?}");
            assert_eq!(o.tuning.bandwidth_hz, 30e3, "narrowest without loss");
            assert_eq!(o.tuning.mode.get("k"), Some(&2.0));
            assert_eq!(o.start.center_hz, 9_000.0, "start kept");
            assert_eq!(o.provenance, REFINED_BY_OUTPUT_ANALYSIS);
            assert!(o.iterations >= 4 && o.evaluations as usize == o.trace.len());
        }
    }

    /// An objective only the shallow acquisition window locks, and only on a decoy one grid step
    /// off the start: the shape of a WFM channel filter sitting beside the carrier, which cuts
    /// MPX noise while the pilot survives, so a short window reads a *higher* C/N0 there than at
    /// the true centre. Its centre estimator points back at the emission by more than one step,
    /// so acquisition discards the correction (T-188).
    struct ShallowDecoy;

    impl Objective for ShallowDecoy {
        fn name(&self) -> &str {
            "decoy@1"
        }
        fn mode(&self) -> &str {
            "toy"
        }
        fn space(&self, start: &RefineStart, _: f64) -> ParameterSpace {
            ParameterSpace {
                center_hz: (start.center_hz - 20e3, start.center_hz + 20e3),
                center_step_hz: 5e3,
                bandwidth_hz: (10e3, 50e3),
                bandwidth_step_hz: 10e3,
                nominal_bandwidth_hz: 40e3,
                bandwidth_tolerance: 1.0,
                mode_axes: Vec::new(),
            }
        }
        fn evaluate<T: IqSample>(
            &mut self,
            _: IqWindow<'_, T>,
            t: &Tuning,
            depth: EvalDepth,
        ) -> Result<Measurement, DemodError> {
            let decoy = (t.center_hz - 5_000.0).abs() < 1.0;
            let locked = depth == EvalDepth::Acquire && decoy;
            Ok(Measurement {
                quality: if locked { 60.0 } else { 10.0 },
                locked,
                center_correction_hz: Some(-6_000.0),
                ..Measurement::default()
            })
        }
    }

    /// A centre that only ever locked on an acquisition measurement is not a refinement: the
    /// deeper measurements of it (track, bandwidth, validate) never locked, so the outcome must
    /// not be reported locked, and nothing downstream may retune to it (T-188).
    #[test]
    fn an_acquisition_only_lock_is_never_a_validated_refinement() {
        let p = prov();
        let x = vec![Complex32::default(); 16];
        let start = RefineStart {
            center_hz: 0.0,
            bandwidth_hz: 5e3,
            warm: false,
        };
        let o =
            RefinementLoop::new(ShallowDecoy, LoopConfig::default()).run(window(&p, &x), &start);
        assert!(
            !o.locked,
            "an unvalidated acquisition centre must not lock: {o:?}"
        );
        assert!(!o.converged, "{o:?}");
        assert_eq!(o.stop, StopReason::NoLock, "{o:?}");
    }

    /// The T-188 station in miniature: the emission sits 2.9 kHz below the start, and a decoy one
    /// 50 kHz grid step above it reads a higher C/N0 on the shallow acquisition window (a channel
    /// filter beside the carrier cuts MPX noise while the pilot survives) while its own estimator
    /// points back at the emission by more than one step.
    struct OverStepDecoy;

    /// The emission's centre, relative to the start.
    const EMISSION_HZ: f64 = -2_900.0;
    /// The decoy's centre, one acquisition step above the start.
    const DECOY_HZ: f64 = 50e3;

    impl Objective for OverStepDecoy {
        fn name(&self) -> &str {
            "over-step-decoy@1"
        }
        fn mode(&self) -> &str {
            "toy"
        }
        fn space(&self, start: &RefineStart, _: f64) -> ParameterSpace {
            ParameterSpace {
                center_hz: (start.center_hz - 200e3, start.center_hz + 200e3),
                center_step_hz: 50e3,
                bandwidth_hz: (100e3, 220e3),
                bandwidth_step_hz: 20e3,
                nominal_bandwidth_hz: 200e3,
                bandwidth_tolerance: 3.0,
                mode_axes: Vec::new(),
            }
        }
        fn evaluate<T: IqSample>(
            &mut self,
            _: IqWindow<'_, T>,
            t: &Tuning,
            depth: EvalDepth,
        ) -> Result<Measurement, DemodError> {
            let off = EMISSION_HZ - t.center_hz;
            let on_emission = off.abs() < 20e3;
            let decoy = depth == EvalDepth::Acquire && (t.center_hz - DECOY_HZ).abs() < 1.0;
            Ok(Measurement {
                quality: if decoy {
                    60.0
                } else if on_emission {
                    50.0
                } else {
                    QUALITY_FLOOR
                },
                locked: on_emission || decoy,
                center_correction_hz: Some(off),
                ..Measurement::default()
            })
        }
    }

    /// Acquisition's centre correction is clamped to one grid step and re-measured, as tracking
    /// clamps it — discarding an over-step correction left the decoy's centre one step off the
    /// emission, and the search never reached the station it had already estimated (T-226).
    #[test]
    fn an_over_step_acquisition_correction_is_clamped_back_onto_the_emission() {
        let p = prov();
        let x = vec![Complex32::default(); 16];
        let start = RefineStart {
            center_hz: 0.0,
            bandwidth_hz: 200e3,
            warm: false,
        };
        let o =
            RefinementLoop::new(OverStepDecoy, LoopConfig::default()).run(window(&p, &x), &start);
        let err = o.tuning.center_hz - EMISSION_HZ;
        assert!(o.locked && o.validated && o.converged, "{o:?}");
        assert!(err.abs() <= 250.0, "centre error {err} Hz: {o:?}");
        assert!(
            o.trace
                .iter()
                .any(|s| s.phase == Phase::Acquire && (s.center_hz - DECOY_HZ).abs() < 1.0),
            "the decoy was acquired and corrected away from: {o:?}"
        );
    }

    /// The budget (`max_evaluations`, `time_budget` — CPU load is what exhausts them) can stop the
    /// search before validation. An earlier phase's locked measurement was made at another tuning
    /// or another depth and never vouches for the returned one, so the outcome is not locked and
    /// nothing downstream may retune to it (T-226; the same false positive as T-188).
    #[test]
    fn a_budget_that_skips_validation_never_reports_a_lock() {
        let p = prov();
        let x = vec![Complex32::default(); 16];
        let start = RefineStart {
            center_hz: 1_500.0,
            bandwidth_hz: 5e3,
            warm: false,
        };
        let toy = || Toy {
            with_estimator: true,
        };
        let full = RefinementLoop::new(toy(), LoopConfig::default()).run(window(&p, &x), &start);
        assert!(
            full.locked && full.validated && full.stop == StopReason::Completed,
            "the same search validates on a full budget: {full:?}"
        );

        // Nine acquisition measurements and one locked tracking measurement, then nothing: the
        // bandwidth scan, the mode axes and the validation are all cut off.
        let cfg = |t: Termination| LoopConfig {
            termination: t,
            ..LoopConfig::default()
        };
        let o = RefinementLoop::new(
            toy(),
            cfg(Termination {
                max_evaluations: 10,
                ..Termination::default()
            }),
        )
        .run(window(&p, &x), &start);
        assert_eq!(o.stop, StopReason::MaxEvaluations, "{o:?}");
        assert!(
            o.trace.iter().any(|s| s.locked),
            "a stale locked measurement is what could be reported: {o:?}"
        );
        assert!(!o.validated && !o.locked && !o.converged, "{o:?}");
        assert_eq!(o.quality, QUALITY_FLOOR, "{o:?}");

        let o = RefinementLoop::new(
            toy(),
            cfg(Termination {
                time_budget: Duration::from_nanos(1),
                ..Termination::default()
            }),
        )
        .run(window(&p, &x), &start);
        assert_eq!(o.stop, StopReason::TimeBudget, "{o:?}");
        assert!(!o.validated && !o.locked, "{o:?}");
    }

    #[test]
    fn budgets_stop_the_loop_and_nothing_locked_keeps_the_start() {
        let p = prov();
        let x = vec![Complex32::default(); 16];
        let cfg = LoopConfig {
            termination: Termination {
                max_evaluations: 3,
                ..Termination::default()
            },
            ..LoopConfig::default()
        };
        let mut l = RefinementLoop::new(
            Toy {
                with_estimator: true,
            },
            cfg,
        );
        let start = RefineStart {
            center_hz: 1_500.0,
            bandwidth_hz: 5e3,
            warm: false,
        };
        let o = l.run(window(&p, &x), &start);
        assert_eq!(o.stop, StopReason::MaxEvaluations);
        assert_eq!(o.evaluations, 3);

        let mut l = RefinementLoop::new(
            Toy {
                with_estimator: true,
            },
            LoopConfig::default(),
        );
        let far = RefineStart {
            center_hz: 100e3,
            bandwidth_hz: 5e3,
            warm: false,
        };
        let o = l.run(window(&p, &x), &far);
        assert!(!o.locked && o.stop == StopReason::NoLock);
        assert_eq!(o.tuning, o.start);
    }

    #[test]
    fn hysteresis_ignores_small_moves_and_quality_losses() {
        let base = |c: f64, bw: f64, q: f64, locked: bool| RefinementOutcome {
            provenance: REFINED_BY_OUTPUT_ANALYSIS.into(),
            objective: "toy@1".into(),
            mode: "toy".into(),
            start: Tuning::default(),
            tuning: Tuning {
                center_hz: c,
                bandwidth_hz: bw,
                mode: BTreeMap::new(),
            },
            quality: q,
            locked,
            validated: locked,
            converged: locked,
            stop: StopReason::Completed,
            iterations: 1,
            evaluations: 1,
            elapsed_s: 0.0,
            mode_params: BTreeMap::new(),
            labels: BTreeMap::new(),
            trace: Vec::new(),
        };
        let h = Hysteresis::default();
        let cur = base(100e6, 180e3, 50.0, true);
        assert!(accept_update(&h, None, &cur));
        assert!(!accept_update(
            &h,
            Some(&cur),
            &base(100e6 + 400.0, 180e3, 52.0, true)
        ));
        assert!(accept_update(
            &h,
            Some(&cur),
            &base(100e6 + 5e3, 180e3, 49.0, true)
        ));
        assert!(!accept_update(
            &h,
            Some(&cur),
            &base(100e6 + 5e3, 180e3, 40.0, true)
        ));
        assert!(!accept_update(
            &h,
            Some(&cur),
            &base(100e6 + 5e3, 180e3, 60.0, false)
        ));
        assert!(accept_update(
            &h,
            Some(&base(0.0, 0.0, 0.0, false)),
            &base(100e6, 200e3, 30.0, true)
        ));
    }
}
