//! Always-on reader 5: the **receiver-line survey**, measured once per capture state (T-399).
//!
//! # Why this is a reader and not a step of classification
//!
//! T-394 built the measurement that replaced a growing list of named notches: channelise the
//! tuned span, find the channels holding nothing, whiten each through C14's own transform, take
//! the median across them, and call its peaks the receiver's ([`hk_estimate::blind::receiver`]).
//! It landed with **no production caller**, and the reason was stated at birth rather than found
//! by audit: the survey needs **at least a second of the raw tuned span**, and
//! [`crate::classify::classify_box`] is handed **one burst's samples**. At 27 ms the survey's
//! native cell is `M/(2·0.027)` ≈ 600 Hz, so the "notch" it produced would be most of the band.
//!
//! Nor is it a per-classification cost. [`crate::classify::classify_and_record`] documents a bound
//! **per event** — one snippet, one C13 estimate, one C14 window — and the survey is none of those
//! sizes: measured in release on `capture-2026-09-15-fm-band` (2.4 Msps), one survey costs
//! **785 ms of one core for a 2 s window** (376 ms for 1 s, 179 ms for 0.5 s — about 0.39 core-
//! seconds per second of capture, near-linear in samples). Charging that to every classification
//! would be a hundredfold regression in a documented bound.
//!
//! But it is not a *recurring* cost either, because **it is a property of the receiver in a
//! configuration, not of an emission**. Device, tune, gain: while those hold, one survey describes
//! every window captured under them. So it is measured **once per [`CaptureState`]** and reused
//! until the state changes — which is exactly the condition
//! [`hk_estimate::blind::ReceiverLines::applies_to`] already tests before the estimator will use
//! one, so the cadence control and the exclusion gate ask the **same** question through the same
//! type rather than drifting apart.
//!
//! `CaptureState::device_id` is [`hk_model::Provenance::device_id`] — the source's own
//! `DeviceInfo::device_id`, the string `ChainKey::of_device` hashes into the run's receive chain
//! (T-314) and `hk_store::history::source_key` hashes into each frame's history origin, and the
//! one the observation log names (T-378). One spelling of "which receiver", not a fifth.
//!
//! # It never delays first detection
//!
//! A survey needs a second of capture before it can say anything, and T-398 measured this exact
//! chain's cold start: **0.0 s of capture to first detection**, 1.0 s to `family=wfm`, 1.0 s to
//! Confirmed. Running the survey *before* detection, or making the first classification wait for
//! one, would put a second of capture plus its compute in front of the number the user has been
//! complaining about — a regression they would feel immediately.
//!
//! So it runs **concurrently, on its own reader thread**, from the first block of the segment:
//!
//! - Detection, history, the spectrum stream and the chains are untouched and unblocked. Nothing
//!   on the detection path calls into this module.
//! - Until the first survey lands, classification runs exactly as it does today — with the
//!   per-capture [`hk_model::Provenance::capture_artefacts`] of T-373 and T-382 and nothing else.
//!   That is the status quo, and the status quo is survivable; a delayed first detection is not.
//! - After it lands, every classification of a window captured under that state gets the
//!   exclusion, and goes on reporting it (`excluded_receiver_hz`,
//!   `CyclicLine::artefact_suppressed`, `BlindReason::CaptureArtefact`). The survey excludes lines
//!   from the **argmax only** and never from the whitening floor — that is real power, and T-394's
//!   geometry is unchanged here.
//!
//! In lossless replay the reader registers a [`crate::gate::GateCursor`] like every other, so a
//! replay stays deterministic: the survey sees the same samples whatever the machine load, and the
//! one-off compute shows up as capture-clock-neutral wall time rather than as a different answer.
//!
//! # Abstention, and why there is a small attempt budget
//!
//! [`hk_estimate::blind::receiver::survey`] abstains when too few channels hold nothing — it will
//! not take a median over three. That can be transient: the first moments of a capture carry the
//! source's own start transient (T-394's own fixture reads skip the first second for exactly this
//! reason), and occupancy moves. So a state that abstains is retried, at most
//! [`SurveyCadence::max_attempts`] times, each over its own fresh window. A state that has
//! **succeeded** is never re-surveyed. Either way the count is bounded per capture state and
//! independent of how many boxes are classified under it, which is the whole point.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use hk_classify::SymbolEstimator;
use hk_core::ReadOutcome;
use hk_dsp::InputInfo;
use hk_estimate::blind::receiver::{CaptureState, ReceiverLines, SurveyConfig};
use hk_model::Provenance;
use num_complex::Complex;

use crate::run::Shared;

/// How often the survey may run for one receiver state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurveyCadence {
    /// Attempts allowed per [`CaptureState`] while every one of them has abstained. A state that
    /// has produced a survey is never measured again (see the module docs).
    pub max_attempts: u32,
}

impl Default for SurveyCadence {
    fn default() -> Self {
        Self { max_attempts: 3 }
    }
}

/// What the survey has measured for one receiver state.
#[derive(Default)]
struct State {
    /// The state being tracked, and how many times it has been surveyed.
    current: Option<(CaptureState, u32)>,
    /// The survey in force, when the tracked state produced one.
    lines: Option<Arc<ReceiverLines>>,
}

/// The receiver-line survey in force for the run, and the cadence that keeps it to one measurement
/// per capture state (see the [module docs](self)).
///
/// Lives for the whole run rather than for a segment: a re-plumb that lands back on the same
/// device, tune and gain is the same receiver state, and re-measuring it would be paying twice for
/// an answer that has not changed.
pub struct ReceiverSurvey {
    state: Mutex<State>,
    cfg: SurveyConfig,
    cadence: SurveyCadence,
    /// Surveys that produced lines.
    measured: AtomicU64,
    /// Surveys that abstained.
    abstained: AtomicU64,
    /// Capture states seen by the reader.
    states: AtomicU64,
    /// Total time spent measuring, µs — the cost this module's bound is about.
    cost_us: AtomicU64,
}

impl Default for ReceiverSurvey {
    fn default() -> Self {
        Self::new(SurveyConfig::default(), SurveyCadence::default())
    }
}

impl ReceiverSurvey {
    /// A survey holder at `cfg`, measuring at most `cadence.max_attempts` times per capture state.
    pub fn new(cfg: SurveyConfig, cadence: SurveyCadence) -> Self {
        Self {
            state: Mutex::new(State::default()),
            cfg,
            cadence,
            measured: AtomicU64::new(0),
            abstained: AtomicU64::new(0),
            states: AtomicU64::new(0),
            cost_us: AtomicU64::new(0),
        }
    }

    /// The survey settings in force.
    pub fn config(&self) -> &SurveyConfig {
        &self.cfg
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The lines measured for the receiver state `p` was captured under, if any.
    ///
    /// The gate is [`ReceiverLines::applies_to`]: a survey is device-local physics and is never
    /// applied across a device, a retune or a gain step (T-259/T-305).
    pub fn lines_for(&self, p: &Provenance) -> Option<Arc<ReceiverLines>> {
        self.lock().lines.clone().filter(|r| r.applies_to(p))
    }

    /// The survey in force, whatever state it describes (diagnostics and tests).
    pub fn current(&self) -> Option<Arc<ReceiverLines>> {
        self.lock().lines.clone()
    }

    /// Gives `c14` the survey that applies to `p`, or clears it when none does.
    ///
    /// Called by [`crate::classify::classify_box`] for every classification, which is what makes
    /// the exclusion impossible to forget at the one call site that needs it — and cheap, because
    /// the measurement itself happened once, on another thread, at most
    /// [`SurveyCadence::max_attempts`] times for this whole receiver state.
    pub fn apply(&self, c14: &mut SymbolEstimator, p: &Provenance) {
        c14.set_receiver_lines(self.lines_for(p).map(|r| (*r).clone()));
    }

    /// Whether the reader should measure a window captured under `p`: the state is new, or it has
    /// abstained fewer than [`SurveyCadence::max_attempts`] times and has produced nothing.
    fn wanted(&self, p: &Provenance) -> bool {
        let s = self.lock();
        match &s.current {
            Some((state, attempts)) if state.matches(p) => {
                s.lines.is_none() && *attempts < self.cadence.max_attempts
            }
            _ => true,
        }
    }

    /// Records one measurement of `p`'s state. `lines` is `None` for an abstention.
    fn record(&self, p: &Provenance, lines: Option<ReceiverLines>, cost: Duration) {
        let mut s = self.lock();
        let fresh = !matches!(&s.current, Some((state, _)) if state.matches(p));
        if fresh {
            self.states.fetch_add(1, Ordering::Relaxed);
            s.current = Some((CaptureState::of(p), 0));
            s.lines = None;
        }
        if let Some((_, attempts)) = s.current.as_mut() {
            *attempts += 1;
        }
        self.cost_us.fetch_add(
            cost.as_micros().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
        match lines {
            Some(r) => {
                self.measured.fetch_add(1, Ordering::Relaxed);
                s.lines = Some(Arc::new(r));
            }
            None => {
                self.abstained.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Surveys measured, surveys abstained, capture states seen, and total measuring time (µs).
    ///
    /// The cadence is observable rather than asserted only in prose: over a run, `measured +
    /// abstained` is bounded by `states × max_attempts` however many boxes were classified.
    pub fn counts(&self) -> SurveyCounts {
        SurveyCounts {
            measured: self.measured.load(Ordering::Relaxed),
            abstained: self.abstained.load(Ordering::Relaxed),
            states: self.states.load(Ordering::Relaxed),
            cost_us: self.cost_us.load(Ordering::Relaxed),
        }
    }
}

/// What [`ReceiverSurvey::counts`] reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SurveyCounts {
    /// Surveys that produced lines.
    pub measured: u64,
    /// Surveys that abstained.
    pub abstained: u64,
    /// Capture states seen.
    pub states: u64,
    /// Time spent measuring, µs.
    pub cost_us: u64,
}

impl SurveyCounts {
    /// Surveys run, measured and abstained together.
    pub fn attempts(&self) -> u64 {
        self.measured + self.abstained
    }
}

/// The `hk-survey` reader: accumulates one window of the raw tuned span per capture state and
/// measures the receiver's own cyclic lines over it (see the [module docs](self)).
pub(crate) fn run(shared: Arc<Shared>, survey: Arc<ReceiverSurvey>) -> anyhow::Result<()> {
    let mut reader = shared.ring.reader_at(0);
    let cursor = shared.gate.register(0);
    // One estimator for the thread, so its FFT plans are built once and reused across states.
    let mut c14 = SymbolEstimator::new();
    let mut buf = vec![Complex::<i8>::default(); 1 << 16];
    // The window being accumulated: the samples, the provenance they were captured under, the
    // time of the first one, and the index the next block must start at to continue it.
    let mut window: Vec<Complex<i8>> = Vec::new();
    let mut held: Option<(hk_core::ProvenanceHandle, hk_model::SampleTime)> = None;
    let mut next_index: Option<u64> = None;
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        match reader.read_timeout(&mut buf, Duration::from_millis(50)) {
            ReadOutcome::Data(chunk) => {
                cursor.set(chunk.end_sample());
                let p = chunk.provenance.get();
                if !survey.wanted(p) {
                    // This receiver state is answered (or has spent its attempts): hold nothing.
                    window = Vec::new();
                    held = None;
                    next_index = None;
                    continue;
                }
                let fs = p.tune.sample_rate_hz;
                if !(fs.is_finite() && fs > 0.0) {
                    continue;
                }
                // A new state, a gap in the stream, or a flagged discontinuity: the window must be
                // one contiguous record of one receiver state, so start it again rather than
                // splice two.
                let broken = chunk.block_start && !chunk.discontinuity.is_empty();
                let continues = !broken
                    && next_index == Some(chunk.first_sample())
                    && held
                        .as_ref()
                        .is_some_and(|(h, _)| h.id() == chunk.provenance.id());
                if !continues {
                    window.clear();
                    held = Some((chunk.provenance.clone(), chunk.time));
                }
                let want = ((survey.cfg.window_s * fs) as usize).max(1);
                window.reserve(want.saturating_sub(window.len()).min(chunk.len));
                window.extend_from_slice(&buf[..chunk.len]);
                next_index = Some(chunk.end_sample());
                if window.len() < want {
                    continue;
                }
                window.truncate(want);
                let Some((prov, time)) = held.take() else {
                    continue;
                };
                next_index = None;
                let info = InputInfo {
                    time,
                    discontinuity: hk_core::Discontinuity::NONE,
                    dropped_before: 0,
                    provenance: &prov,
                };
                // The measurement. It runs here, on this thread, holding nothing but the ring
                // cursor it has already advanced past the samples it copied out — no repository
                // lock, no chain, nothing the detector or the chains wait on.
                let t0 = Instant::now();
                let ran = c14.survey_receiver_lines(info, &window, &survey.cfg);
                let cost = t0.elapsed();
                let lines = ran.then(|| c14.receiver_lines().cloned()).flatten();
                survey.record(prov.get(), lines, cost);
                window = Vec::new();
            }
            ReadOutcome::Overrun { resume_at, .. } => {
                // Lapped: whatever was held is no longer contiguous with what comes next.
                window = Vec::new();
                held = None;
                next_index = None;
                cursor.set(resume_at);
            }
            ReadOutcome::Empty => {}
            ReadOutcome::Closed => break,
        }
    }
    drop(cursor);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn provenance(device: &str, center_hz: f64, lna_db: f64) -> Provenance {
        serde_json::from_value(serde_json::json!({
            "device_id": device,
            "tune": {
                "center_hz": center_hz, "sample_rate_hz": 2.4e6, "lna_db": lna_db,
                "vga_db": 20.0, "amp_on": false, "bandwidth_hz": 1.75e6,
            },
            "overload": false, "quantisation_limited": false, "clock_source": "internal",
            "clock_locked": true, "timestamp_method": "synthetic",
            "timestamp_error_budget_ns": 0,
        }))
        .expect("a provenance record")
    }

    fn lines_for(p: &Provenance) -> ReceiverLines {
        ReceiverLines {
            device_id: p.device_id.clone(),
            center_hz: p.tune.center_hz,
            sample_rate_hz: p.tune.sample_rate_hz,
            gain: (p.tune.lna_db, p.tune.vga_db, p.tune.amp_on),
            lines: Vec::new(),
            reference_channels: 16,
            surveyed_channels: 32,
            channel_spacing_hz: 75e3,
            band_hz: (0.0, 60e3),
            resolution_hz: 8.0,
            source_index: 0,
            source_samples: 4_800_000,
        }
    }

    /// The cadence: one measurement per capture state, not one per classification, and never one
    /// carried across a retune (T-399).
    #[test]
    fn t399_a_state_is_surveyed_once_and_a_retune_starts_again() {
        let s = ReceiverSurvey::default();
        let a = provenance("hackrf:one", 100.0e6, 24.0);
        assert!(s.wanted(&a), "an unseen state is surveyed");
        s.record(&a, Some(lines_for(&a)), Duration::from_millis(785));
        assert!(
            !s.wanted(&a),
            "a state that produced a survey is never measured again"
        );
        // However many boxes are classified under it, the measurement does not run again.
        for _ in 0..1000 {
            assert!(s.lines_for(&a).is_some());
            assert!(!s.wanted(&a));
        }
        assert_eq!(s.counts().attempts(), 1);

        // A retune is a different receiver, so the old survey neither applies nor suppresses a new
        // measurement.
        let b = provenance("hackrf:one", 101.3e6, 24.0);
        assert!(s.lines_for(&b).is_none(), "a survey never crosses a retune");
        assert!(s.wanted(&b));
        s.record(&b, Some(lines_for(&b)), Duration::from_millis(785));
        assert_eq!(s.counts().states, 2);
        assert_eq!(s.counts().attempts(), 2);
        // And the old state is gone: only one survey is in force at a time.
        assert!(s.lines_for(&a).is_none());
        assert!(s.lines_for(&b).is_some());
    }

    /// The delivery path: [`ReceiverSurvey::apply`] is what puts a measured survey in front of
    /// C14 at the classification call site, and what takes it away again for a window captured
    /// under a state it does not describe. A stale survey silently left in place would be a
    /// measurement of one receiver charged to another.
    #[test]
    fn t399_apply_hands_the_survey_to_c14_and_clears_it_off_a_foreign_window() {
        let s = ReceiverSurvey::default();
        let a = provenance("hackrf:one", 100.0e6, 24.0);
        let mut c14 = SymbolEstimator::new();
        s.apply(&mut c14, &a);
        assert!(
            c14.receiver_lines().is_none(),
            "nothing is applied before anything is measured"
        );
        s.record(&a, Some(lines_for(&a)), Duration::ZERO);
        s.apply(&mut c14, &a);
        let held = c14.receiver_lines().expect("the survey reached C14");
        assert!(
            held.applies_to(&a),
            "and it describes this window's receiver"
        );
        // A window from another receiver state clears it rather than keeping the last one.
        let b = provenance("hackrf:one", 101.3e6, 24.0);
        s.apply(&mut c14, &b);
        assert!(
            c14.receiver_lines().is_none(),
            "a stale survey is never left in place"
        );
    }

    /// A gain step is a different receiver state too: the noise contribution and everything riding
    /// on it move with it.
    #[test]
    fn t399_a_gain_step_invalidates_the_survey() {
        let s = ReceiverSurvey::default();
        let a = provenance("hackrf:one", 100.0e6, 24.0);
        s.record(&a, Some(lines_for(&a)), Duration::ZERO);
        let louder = provenance("hackrf:one", 100.0e6, 32.0);
        assert!(s.lines_for(&louder).is_none());
        assert!(s.wanted(&louder));
        // As is a different front end at the same tune (T-259/T-305).
        let other = provenance("hackrf:two", 100.0e6, 24.0);
        assert!(s.lines_for(&other).is_none());
    }

    /// An abstention is retried, but a bounded number of times — never once per block, and never
    /// once per classification.
    #[test]
    fn t399_an_abstention_is_retried_within_a_bounded_budget() {
        let cadence = SurveyCadence { max_attempts: 3 };
        let s = ReceiverSurvey::new(SurveyConfig::default(), cadence);
        let a = provenance("hackrf:one", 100.0e6, 24.0);
        for i in 0..cadence.max_attempts {
            assert!(s.wanted(&a), "attempt {i} is allowed");
            s.record(&a, None, Duration::from_millis(785));
        }
        assert!(!s.wanted(&a), "the budget is spent");
        assert_eq!(s.counts().attempts(), u64::from(cadence.max_attempts));
        assert_eq!(s.counts().measured, 0);
        assert!(
            s.lines_for(&a).is_none(),
            "an abstention is not an empty answer"
        );
        // A success inside the budget stops the retries.
        let b = provenance("hackrf:one", 102.0e6, 24.0);
        s.record(&b, None, Duration::ZERO);
        assert!(s.wanted(&b));
        s.record(&b, Some(lines_for(&b)), Duration::ZERO);
        assert!(!s.wanted(&b));
    }
}
