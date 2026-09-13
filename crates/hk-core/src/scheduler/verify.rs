//! Feeding verification-group captures to the cross-capture trust tests (S4 rules 6–7).
//!
//! hk-core cannot depend on hk-detect (hk-detect depends on hk-core), so the trust functions are
//! reached through [`TrustEvaluator`]: the runtime implements it by calling
//! `hk_detect::trust::{gain_step, retune, rate_change}` on its `CaptureResult`s (the hk-core
//! scheduler tests do exactly that). This module owns the scheduling-side rules: which captures
//! pair up (only captures of one verification group, [`ScheduleStep::verification_group`]), which
//! is the lower gain state, and **never running inference on a clipped capture** (S4 rule 6:
//! reduce gain first).

use super::config::MAX_GAIN_STEP_PAIRS;
use super::step::{GainSlot, PoiKey, Purpose, ScheduleStep};
use crate::source::Gains;

const PAIRS: usize = MAX_GAIN_STEP_PAIRS as usize;

/// Nominal amp gain used only to order a gain-step pair into lower/higher, dB (HackRF ~11 dB;
/// the trust test measures the real step from anchors).
pub const AMP_NOMINAL_DB: f64 = 11.0;

/// What the scheduling rules need to know about a capture.
pub trait CaptureTrust {
    /// The ADC clipped (or the provenance was overloaded) during the capture.
    fn clipped(&self) -> bool;
}

/// Runs the trust tests on paired captures.
pub trait TrustEvaluator<C> {
    /// Rule 6 on one unclipped gain-step pair, ordered by nominal gain.
    fn gain_step(&mut self, poi: PoiKey, pair: u8, lower: &C, higher: &C);

    /// Rule 7: `moved` was tuned `delta_hz` away from `base`, same gains. Both unclipped.
    fn retune(&mut self, poi: PoiKey, base: &C, moved: &C, delta_hz: f64);

    /// Clock-harmonic test: `changed` ran at `rate_hz` instead of `base_rate_hz`, same centre
    /// (`hk_detect::trust::rate_change`). Both unclipped.
    fn rate_change(&mut self, poi: PoiKey, base: &C, changed: &C, base_rate_hz: f64, rate_hz: f64);
}

/// A capture that does not belong to this verification group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    /// The step is not a verification step (trust test or group baseline) of this POI.
    #[error("step {seq} is not a verification step of POI {poi}")]
    NotThisVerification {
        /// Step sequence number.
        seq: u64,
        /// This group's POI.
        poi: PoiKey,
    },
    /// The step belongs to an older verification group than the one being collected (a group
    /// cut by a preemption or plan update and restarted under a new id).
    #[error(
        "step {seq} belongs to verification group {group}, older than the collected group {current}"
    )]
    StaleGroup {
        /// Step sequence number.
        seq: u64,
        /// The step's group.
        group: u64,
        /// The group being collected.
        current: u64,
    },
}

/// What [`Verification::evaluate`] ran.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VerificationReport {
    /// Gain-step pairs evaluated.
    pub gain_pairs_run: u8,
    /// Pairs skipped because a block clipped.
    pub gain_pairs_skipped_clipped: u8,
    /// Pairs missing a capture (cut or not scheduled).
    pub gain_pairs_incomplete: u8,
    /// Retune comparisons evaluated.
    pub retunes_run: u8,
    /// Retune captures with no unclipped base capture to compare against.
    pub retunes_without_base: u8,
    /// Retune captures skipped because they clipped.
    pub retunes_skipped_clipped: u8,
    /// Rate-change comparisons evaluated.
    pub rate_changes_run: u8,
    /// Rate-change captures with no unclipped base capture.
    pub rate_changes_without_base: u8,
    /// Rate-change captures skipped because they clipped.
    pub rate_changes_skipped_clipped: u8,
}

/// Collects the captures of one POI's verification group, then feeds the trust tests.
///
/// The collector binds to the group of the first capture recorded. A capture from a newer group
/// (the scheduler restarted the group) discards what was collected and starts over; one from an
/// older group is rejected ([`VerifyError::StaleGroup`]). Captures of different groups never pair.
pub struct Verification<C> {
    poi: PoiKey,
    pairs: u8,
    group: Option<u64>,
    baseline: Option<C>,
    a: [Option<(Gains, C)>; PAIRS],
    b: [Option<(Gains, C)>; PAIRS],
    retunes: [Option<(f64, C)>; 2],
    rate: Option<(f64, f64, C)>,
}

fn nominal_db(g: &Gains) -> f64 {
    g.lna_db + g.vga_db + if g.amp_on { AMP_NOMINAL_DB } else { 0.0 }
}

impl<C: CaptureTrust> Verification<C> {
    /// A group for `poi` expecting `pairs` gain-step pairs (the scheduler's `gain_step_pairs`).
    pub fn new(poi: PoiKey, pairs: u8) -> Self {
        Self {
            poi,
            pairs: pairs.min(MAX_GAIN_STEP_PAIRS),
            group: None,
            baseline: None,
            a: std::array::from_fn(|_| None),
            b: std::array::from_fn(|_| None),
            retunes: [None, None],
            rate: None,
        }
    }

    /// The verification group being collected, once a capture was recorded.
    pub fn group(&self) -> Option<u64> {
        self.group
    }

    fn clear(&mut self) {
        self.baseline = None;
        self.a = std::array::from_fn(|_| None);
        self.b = std::array::from_fn(|_| None);
        self.retunes = [None, None];
        self.rate = None;
    }

    /// Records the capture made during `step`.
    pub fn record(&mut self, step: &ScheduleStep, capture: C) -> Result<(), VerifyError> {
        let ours = match step.purpose {
            Purpose::GainStep { poi, pair, .. } => poi == self.poi && pair < self.pairs,
            Purpose::Dwell { poi } | Purpose::RateChange { poi, .. } => poi == self.poi,
            Purpose::Retune { poi, delta_hz } => poi == self.poi && delta_hz != 0.0,
            _ => false,
        };
        let (true, Some(group)) = (ours, step.verification_group) else {
            return Err(VerifyError::NotThisVerification {
                seq: step.seq,
                poi: self.poi,
            });
        };
        match self.group {
            Some(current) if group < current => {
                return Err(VerifyError::StaleGroup {
                    seq: step.seq,
                    group,
                    current,
                });
            }
            Some(current) if group == current => {}
            _ => {
                self.clear();
                self.group = Some(group);
            }
        }
        match step.purpose {
            Purpose::GainStep { pair, slot, .. } => {
                let side = match slot {
                    GainSlot::A => &mut self.a,
                    GainSlot::B => &mut self.b,
                };
                side[usize::from(pair)] = Some((step.gains, capture));
            }
            Purpose::Dwell { .. } => self.baseline = Some(capture),
            Purpose::Retune { delta_hz, .. } => {
                self.retunes[usize::from(delta_hz < 0.0)] = Some((delta_hz, capture));
            }
            Purpose::RateChange { base_rate_hz, .. } => {
                self.rate = Some((base_rate_hz, step.rate_hz, capture));
            }
            _ => {}
        }
        Ok(())
    }

    /// Feeds every complete comparison to `evaluator`. Clipped captures never reach a test:
    /// gain-step pairs with a clipped block are skipped, and retune and rate-change comparisons
    /// use the first unclipped A block (else the unclipped baseline dwell) as their base and skip
    /// clipped moved captures.
    pub fn evaluate(&self, evaluator: &mut impl TrustEvaluator<C>) -> VerificationReport {
        let mut report = VerificationReport::default();
        for pair in 0..self.pairs {
            let i = usize::from(pair);
            match (&self.a[i], &self.b[i]) {
                (Some((ga, ca)), Some((gb, cb))) => {
                    if ca.clipped() || cb.clipped() {
                        report.gain_pairs_skipped_clipped += 1;
                    } else {
                        let (lower, higher) = if nominal_db(ga) <= nominal_db(gb) {
                            (ca, cb)
                        } else {
                            (cb, ca)
                        };
                        evaluator.gain_step(self.poi, pair, lower, higher);
                        report.gain_pairs_run += 1;
                    }
                }
                _ => report.gain_pairs_incomplete += 1,
            }
        }
        let base = self
            .a
            .iter()
            .flatten()
            .map(|(_, c)| c)
            .find(|c| !c.clipped())
            .or(self.baseline.as_ref().filter(|c| !c.clipped()));
        for (delta_hz, moved) in self.retunes.iter().flatten() {
            match base {
                _ if moved.clipped() => report.retunes_skipped_clipped += 1,
                Some(b) => {
                    evaluator.retune(self.poi, b, moved, *delta_hz);
                    report.retunes_run += 1;
                }
                None => report.retunes_without_base += 1,
            }
        }
        if let Some((base_rate, rate, changed)) = &self.rate {
            match base {
                _ if changed.clipped() => report.rate_changes_skipped_clipped += 1,
                Some(b) => {
                    evaluator.rate_change(self.poi, b, changed, *base_rate, *rate);
                    report.rate_changes_run += 1;
                }
                None => report.rate_changes_without_base += 1,
            }
        }
        report
    }
}
