//! DDC planning: output rate, filter edges and the two-stage split, chosen by a cost model.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::filter::{
    DEFAULT_STOPBAND_DB, DesignError, FirDesign, LowpassSpec, design_lowpass, design_lowpass_with,
    kaiser_taps,
};

/// Largest interpolation factor used for an exact rational resampler.
pub const MAX_RATIONAL_UP: usize = 64;
/// Polyphase branches of the fractional (linearly interpolated) resampler.
pub const FRACTIONAL_PHASES: usize = 64;
const MAX_PROTOTYPE_TAPS: usize = 1 << 19;
/// A rational split must be this much cheaper than an integer one to win (integer
/// decimation keeps the time map on whole samples and needs no phase tables).
const INEXACT_INTEGER_PENALTY: f64 = 1.05;
/// A fractional split must be this much cheaper to win (it only approximates the rate
/// through interpolation).
const FRACTIONAL_PENALTY: f64 = 1.25;

/// A DDC request as data (ADR-0001: chains are built at runtime from specs).
///
/// ```json
/// {"center_offset_hz": 250000, "bandwidth_hz": 200000, "output_rate_hz": 250000,
///  "transition_hz": 25000, "stopband_db": 60}
/// ```
/// Only `center_offset_hz` and `bandwidth_hz` are required. `transition` is accepted as an
/// alias of `transition_hz`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DdcSpec {
    /// Channel centre relative to the input centre, Hz.
    pub center_offset_hz: f64,
    /// Two-sided bandwidth passed flat, Hz (passband edge = `bandwidth_hz / 2`).
    pub bandwidth_hz: f64,
    /// Output sample rate, Hz. Default: `fs / floor(fs / (2·bandwidth))`, an integer
    /// decimation giving at least 2× the bandwidth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_rate_hz: Option<f64>,
    /// Transition width beyond the passband edge, Hz. Default: up to the output Nyquist
    /// (alias-free output), or up to `output_rate − bandwidth/2` (alias-free passband) when
    /// the Nyquist margin is under 10% of the passband edge.
    #[serde(default, alias = "transition", skip_serializing_if = "Option::is_none")]
    pub transition_hz: Option<f64>,
    /// Stopband attenuation, dB (default 60).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopband_db: Option<f64>,
}

impl DdcSpec {
    /// A spec with default rate, transition and stopband.
    pub fn new(center_offset_hz: f64, bandwidth_hz: f64) -> Self {
        Self {
            center_offset_hz,
            bandwidth_hz,
            output_rate_hz: None,
            transition_hz: None,
            stopband_db: None,
        }
    }

    /// Sets the output rate.
    pub fn with_output_rate(mut self, hz: f64) -> Self {
        self.output_rate_hz = Some(hz);
        self
    }

    /// Sets the transition width.
    pub fn with_transition(mut self, hz: f64) -> Self {
        self.transition_hz = Some(hz);
        self
    }

    /// Sets the stopband attenuation.
    pub fn with_stopband_db(mut self, db: f64) -> Self {
        self.stopband_db = Some(db);
        self
    }
}

/// Why a DDC could not be planned.
#[derive(Clone, Debug, PartialEq)]
pub enum DdcError {
    /// The spec is inconsistent with the input rate (reason attached).
    InvalidSpec(String),
    /// A filter could not be designed.
    Design(DesignError),
}

impl fmt::Display for DdcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DdcError::InvalidSpec(why) => write!(f, "invalid DDC spec: {why}"),
            DdcError::Design(e) => write!(f, "DDC filter: {e}"),
        }
    }
}

impl std::error::Error for DdcError {}

impl From<DesignError> for DdcError {
    fn from(e: DesignError) -> Self {
        DdcError::Design(e)
    }
}

/// How the second stage changes rate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ResampleKind {
    /// Decimate by an integer.
    Integer {
        /// Decimation factor.
        decimation: usize,
    },
    /// Exact rational `up/down` polyphase resampler (`up ≤ MAX_RATIONAL_UP`).
    Rational {
        /// Interpolation factor (polyphase branches).
        up: usize,
        /// Decimation factor.
        down: usize,
    },
    /// Arbitrary ratio: `FRACTIONAL_PHASES` polyphase branches, linearly interpolated.
    Fractional {
        /// Input samples per output sample.
        ratio: f64,
        /// Polyphase branches.
        phases: usize,
    },
}

impl ResampleKind {
    /// Polyphase branches in the prototype.
    pub fn phases(&self) -> usize {
        match *self {
            ResampleKind::Integer { .. } => 1,
            ResampleKind::Rational { up, .. } => up,
            ResampleKind::Fractional { phases, .. } => phases,
        }
    }
}

/// The second (rate-change) stage.
#[derive(Clone, Debug, PartialEq)]
pub struct ResamplePlan {
    /// Rate change.
    pub kind: ResampleKind,
    /// Prototype at `phases × stage-1 rate`, DC gain `phases`, length `phases × taps_per_phase`.
    pub design: FirDesign,
    /// Taps evaluated per output sample.
    pub taps_per_phase: usize,
}

/// A complete DDC plan.
///
/// **Strategy: NCO mix + xlating decimating FIR, then an optional polyphase resampler.**
/// Stage 1 multiplies complex band-pass taps (the low-pass shifted to the channel centre)
/// against the full-rate input and decimates by an integer, rotating each output by the NCO
/// phase at its absolute source index. That is mathematically "mix to baseband, low-pass,
/// decimate" at the cost of computing the NCO only at the decimated rate. Its stopband only
/// has to protect what stage 2 keeps (`stage1_rate − stopband_hz`), so it is short. Stage 2
/// sets the final passband/stopband at the low rate: integer decimation, exact rational `P/Q`,
/// or a fractional resampler. The split (`xlate_decimation`) minimises estimated multiplies
/// per input sample; a single xlating stage is chosen when that is cheaper.
///
/// The DDC is for on-demand channels at an arbitrary centre and bandwidth (one per detection).
/// For many channels on a fixed raster use the PFB ([`crate::channelizer`]), whose cost does
/// not grow with the number of active channels.
#[derive(Clone, Debug, PartialEq)]
pub struct DdcPlan {
    /// Input rate, Hz.
    pub input_rate_hz: f64,
    /// Output rate, Hz.
    pub output_rate_hz: f64,
    /// Channel centre offset, Hz.
    pub center_offset_hz: f64,
    /// Passband edge (half the bandwidth), Hz.
    pub passband_hz: f64,
    /// Final stopband edge, Hz.
    pub stopband_hz: f64,
    /// Stopband attenuation, dB.
    pub stopband_db: f64,
    /// Stage 1 prototype (real low-pass, before shifting to the centre).
    pub xlate: FirDesign,
    /// Stage 1 decimation.
    pub xlate_decimation: usize,
    /// Stage 2, if any.
    pub resample: Option<ResamplePlan>,
}

fn rational(ratio: f64, max_up: usize) -> Option<(usize, usize)> {
    (1..=max_up).find_map(|p| {
        let q = (ratio * p as f64).round();
        ((ratio * p as f64 - q).abs() <= 1e-9 * q.max(1.0) && q >= 1.0).then_some((p, q as usize))
    })
}

fn integer(ratio: f64) -> Option<usize> {
    let r = ratio.round();
    ((ratio - r).abs() <= 1e-9 * r.max(1.0) && r >= 1.0).then_some(r as usize)
}

struct Candidate {
    d1: usize,
    stage1_stop: f64,
    resample: Option<ResampleKind>,
    cost: f64,
}

impl DdcPlan {
    /// Plans `spec` for input rate `input_rate_hz` (designs and verifies the filters).
    pub fn new(spec: &DdcSpec, input_rate_hz: f64) -> Result<Self, DdcError> {
        let bad = |why: String| Err(DdcError::InvalidSpec(why));
        let fs = input_rate_hz;
        let bw = spec.bandwidth_hz;
        let f0 = spec.center_offset_hz;
        let a = spec.stopband_db.unwrap_or(DEFAULT_STOPBAND_DB);
        if !(fs.is_finite() && fs > 0.0) {
            return bad(format!("input rate {fs} must be positive"));
        }
        if !(bw.is_finite() && bw > 0.0 && f0.is_finite()) {
            return bad(format!("bandwidth {bw} / centre {f0} invalid"));
        }
        let fp = bw / 2.0;
        if f0.abs() + fp > fs / 2.0 * (1.0 + 1e-12) {
            return bad(format!(
                "channel {f0} ± {fp} Hz extends beyond the input band ±{}",
                fs / 2.0
            ));
        }
        let out = match spec.output_rate_hz {
            Some(r) => r,
            None => fs / (fs / (2.0 * bw)).floor().max(1.0),
        };
        if !(out.is_finite() && out > 0.0) || out > fs * (1.0 + 1e-12) {
            return bad(format!("output rate {out} must be in (0, {fs}]"));
        }
        let out = out.min(fs);
        let fst = match spec.transition_hz {
            Some(t) if t.is_finite() && t > 0.0 => fp + t,
            Some(t) => return bad(format!("transition {t} must be positive")),
            None if out / 2.0 - fp >= 0.1 * fp => out / 2.0,
            None => out - fp,
        };
        if fst <= fp * (1.0 + 1e-9) || fst > (out - fp) * (1.0 + 1e-9) {
            return bad(format!(
                "bandwidth {bw} Hz does not fit output rate {out} Hz with stopband edge {fst} Hz \
                 (need bandwidth/2 < stopband <= rate − bandwidth/2)"
            ));
        }
        // Content between Nyquist and fst would alias into the transition band only.
        let fst = fst.min(fs / 2.0);

        let best = Self::choose(fs, out, fp, fst, a).ok_or_else(|| {
            DdcError::InvalidSpec(format!(
                "no realisable filter split for {spec:?} at {fs} Hz"
            ))
        })?;
        let xlate = design_lowpass(LowpassSpec::new(fs, fp, best.stage1_stop, a))?;
        let resample = match best.resample {
            None => None,
            Some(kind) => {
                let fs1 = fs / best.d1 as f64;
                let p = kind.phases();
                let design =
                    design_lowpass_with(LowpassSpec::new(fs1 * p as f64, fp, fst, a), p, p as f64)?;
                let taps_per_phase = design.len() / p;
                Some(ResamplePlan {
                    kind,
                    design,
                    taps_per_phase,
                })
            }
        };
        Ok(Self {
            input_rate_hz: fs,
            output_rate_hz: out,
            center_offset_hz: f0,
            passband_hz: fp,
            stopband_hz: fst,
            stopband_db: a,
            xlate,
            xlate_decimation: best.d1,
            resample,
        })
    }

    fn choose(fs: f64, out: f64, fp: f64, fst: f64, a: f64) -> Option<Candidate> {
        let mut best: Option<Candidate> = None;
        let mut consider = |c: Candidate| {
            if best.as_ref().is_none_or(|b| c.cost < b.cost) {
                best = Some(c);
            }
        };
        // Single xlating stage straight to the output rate.
        if let Some(d) = integer(fs / out) {
            let taps = kaiser_taps(a, (fst - fp) / fs) as f64;
            if taps <= MAX_PROTOTYPE_TAPS as f64 {
                consider(Candidate {
                    d1: d,
                    stage1_stop: fst,
                    resample: None,
                    cost: 2.0 * taps / d as f64,
                });
            }
        }
        let max_d1 = (fs / out).floor().max(1.0) as usize;
        for d1 in 1..=max_d1 {
            let fs1 = fs / d1 as f64;
            let ratio = fs1 / out;
            if integer(ratio) == Some(1) {
                continue; // the single-stage candidate
            }
            // Stage 1 aliases must land at or above fst, where stage 2 removes them.
            let stop1 = (fs1 - fst).min(fs / 2.0);
            if stop1 <= fp * 1.01 {
                continue;
            }
            let taps1 = kaiser_taps(a, (stop1 - fp) / fs) as f64;
            let kind = if let Some(d2) = integer(ratio) {
                ResampleKind::Integer { decimation: d2 }
            } else if let Some((up, down)) = rational(ratio, MAX_RATIONAL_UP) {
                ResampleKind::Rational { up, down }
            } else {
                ResampleKind::Fractional {
                    ratio,
                    phases: FRACTIONAL_PHASES,
                }
            };
            let p = kind.phases();
            if fst > fs1 / 2.0 && p > 1 {
                continue; // images of the stage-1 band would not be rejected
            }
            let per_phase = kaiser_taps(a, (fst - fp) / (fs1 * p as f64)) as f64 / p as f64;
            if per_phase * p as f64 > MAX_PROTOTYPE_TAPS as f64 {
                continue;
            }
            let (lerp, preference) = match kind {
                ResampleKind::Integer { .. } => (1.0, 1.0),
                ResampleKind::Rational { .. } => (1.0, INEXACT_INTEGER_PENALTY),
                ResampleKind::Fractional { .. } => (2.0, FRACTIONAL_PENALTY),
            };
            // Complex×complex taps cost ~2× real×complex; stage 2 runs at the output rate.
            let cost = (2.0 * taps1 / d1 as f64 + lerp * per_phase * out / fs) * preference;
            consider(Candidate {
                d1,
                stage1_stop: stop1,
                resample: Some(kind),
                cost,
            });
        }
        best
    }

    /// Input samples per output sample.
    pub fn decimation(&self) -> f64 {
        self.input_rate_hz / self.output_rate_hz
    }

    /// Stage 1 output rate, Hz.
    pub fn stage1_rate_hz(&self) -> f64 {
        self.input_rate_hz / self.xlate_decimation as f64
    }

    /// Estimated real multiply-adds per input sample (both stages).
    pub fn cost_per_input_sample(&self) -> f64 {
        let s1 = 4.0 * self.xlate.len() as f64 / self.xlate_decimation as f64;
        let s2 = self.resample.as_ref().map_or(0.0, |r| {
            let lerp = if matches!(r.kind, ResampleKind::Fractional { .. }) {
                2.0
            } else {
                1.0
            };
            2.0 * lerp * r.taps_per_phase as f64 / self.decimation()
        });
        s1 + s2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rational_detection() {
        assert_eq!(rational(25.0 / 3.0, 64), Some((3, 25)));
        assert_eq!(rational(4.0, 64), Some((1, 4)));
        assert_eq!(rational(std::f64::consts::PI, 64), None);
        assert_eq!(integer(40.0000000001), Some(40));
        assert_eq!(integer(40.3), None);
    }

    #[test]
    fn default_rate_and_transition() {
        let p = DdcPlan::new(&DdcSpec::new(1e5, 25e3), 2e6).unwrap();
        assert_eq!(p.output_rate_hz, 50e3);
        assert_eq!(p.stopband_hz, 25e3);
        let wfm = DdcPlan::new(&DdcSpec::new(0.0, 200e3).with_output_rate(250e3), 20e6).unwrap();
        assert_eq!(wfm.stopband_hz, 125e3);
        let r = wfm.resample.as_ref().expect("two stages for /80");
        let total = wfm.xlate_decimation as f64
            * match r.kind {
                ResampleKind::Integer { decimation } => decimation as f64,
                ResampleKind::Rational { up, down } => down as f64 / up as f64,
                ResampleKind::Fractional { ratio, .. } => ratio,
            };
        assert!(
            (total - 80.0).abs() < 1e-9,
            "d1 {} kind {:?} total {total}",
            wfm.xlate_decimation,
            r.kind
        );
        assert!(
            !matches!(r.kind, ResampleKind::Fractional { .. }),
            "exact ratio must not use the fractional resampler: d1 {} {:?} cost {}",
            wfm.xlate_decimation,
            r.kind,
            wfm.cost_per_input_sample()
        );
        eprintln!(
            "WFM plan: d1 {} ({} taps) {:?} ({} taps/phase), cost {:.2}",
            wfm.xlate_decimation,
            wfm.xlate.len(),
            r.kind,
            r.taps_per_phase,
            wfm.cost_per_input_sample()
        );
    }

    #[test]
    fn rejects_impossible_specs() {
        assert!(DdcPlan::new(&DdcSpec::new(0.9e6, 400e3), 2e6).is_err());
        assert!(DdcPlan::new(&DdcSpec::new(0.0, 60e3).with_output_rate(50e3), 2e6).is_err());
        assert!(DdcPlan::new(&DdcSpec::new(0.0, 10e3).with_output_rate(4e6), 2e6).is_err());
        assert!(DdcPlan::new(&DdcSpec::new(0.0, 10e3).with_transition(-1.0), 2e6).is_err());
    }
}
