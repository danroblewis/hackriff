//! Feeding verification-group captures to the cross-capture trust tests (S4 rules 6–7).
//!
//! hk-core cannot depend on hk-detect (hk-detect depends on hk-core), so the trust functions are
//! reached through [`TrustEvaluator`]: the runtime implements it by calling
//! `hk_detect::trust::{gain_step, retune}` on its `CaptureResult`s (the hk-core scheduler tests
//! do exactly that). This module owns the scheduling-side rules: which captures pair up, which
//! is the lower gain state, and **never running gain-step inference when either block
//! clipped** (S4 rule 6: reduce gain first).

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

    /// Rule 7: `moved` was tuned `delta_hz` away from `base`, same gains.
    fn retune(&mut self, poi: PoiKey, base: &C, moved: &C, delta_hz: f64);

    /// Clock-harmonic test: `changed` ran at `rate_hz` instead of `base_rate_hz`, same centre.
    /// (No hk-detect function exists yet; follow-up.)
    fn rate_change(&mut self, poi: PoiKey, base: &C, changed: &C, base_rate_hz: f64, rate_hz: f64);
}

/// A capture that does not belong to this verification group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    /// The step is not a trust-test (or baseline) step of this POI.
    #[error("step {seq} is not a verification step of POI {poi}")]
    NotThisVerification {
        /// Step sequence number.
        seq: u64,
        /// This group's POI.
        poi: PoiKey,
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
    /// Retune captures with no base capture to compare against.
    pub retunes_without_base: u8,
    /// Rate-change comparisons evaluated.
    pub rate_changes_run: u8,
}

/// Collects the captures of one POI's verification group, then feeds the trust tests.
pub struct Verification<C> {
    poi: PoiKey,
    pairs: u8,
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
            baseline: None,
            a: std::array::from_fn(|_| None),
            b: std::array::from_fn(|_| None),
            retunes: [None, None],
            rate: None,
        }
    }

    /// Records the capture made during `step`.
    pub fn record(&mut self, step: &ScheduleStep, capture: C) -> Result<(), VerifyError> {
        match step.purpose {
            Purpose::GainStep { poi, pair, slot } if poi == self.poi && pair < self.pairs => {
                let side = match slot {
                    GainSlot::A => &mut self.a,
                    GainSlot::B => &mut self.b,
                };
                side[usize::from(pair)] = Some((step.gains, capture));
            }
            Purpose::Dwell { poi } if poi == self.poi => self.baseline = Some(capture),
            Purpose::Retune { poi, delta_hz } if poi == self.poi && delta_hz != 0.0 => {
                self.retunes[usize::from(delta_hz < 0.0)] = Some((delta_hz, capture));
            }
            Purpose::RateChange { poi, base_rate_hz } if poi == self.poi => {
                self.rate = Some((base_rate_hz, step.rate_hz, capture));
            }
            _ => {
                return Err(VerifyError::NotThisVerification {
                    seq: step.seq,
                    poi: self.poi,
                });
            }
        }
        Ok(())
    }

    /// Feeds every complete comparison to `evaluator`. Gain-step pairs with a clipped block are
    /// skipped; retune and rate-change comparisons use the first A block (else the baseline
    /// dwell) as their base.
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
            .next()
            .or(self.baseline.as_ref());
        for (delta_hz, moved) in self.retunes.iter().flatten() {
            match base {
                Some(b) => {
                    evaluator.retune(self.poi, b, moved, *delta_hz);
                    report.retunes_run += 1;
                }
                None => report.retunes_without_base += 1,
            }
        }
        if let (Some((base_rate, rate, changed)), Some(b)) = (&self.rate, base) {
            evaluator.rate_change(self.poi, b, changed, *base_rate, *rate);
            report.rate_changes_run += 1;
        }
        report
    }
}
