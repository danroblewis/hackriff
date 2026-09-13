//! Estimate records. Every estimator returns a value with a one-sigma uncertainty, its method
//! and evidence, or abstains with a reason (C13: "no defaults substituted").

use serde::{Deserialize, Serialize};

/// The estimator behind a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Method {
    /// N0 = mean Welch density of the signal-free pads (mean, never median: S5 pitfall 3).
    NoisePad,
    /// N0 supplied by the caller (a C08 floor).
    NoiseCaller,
    /// N0 = mean density of in-channel bins outside the detection box, used when neither pad
    /// is signal-free. The lower side is taken when the two sides disagree.
    NoiseSideband,
    /// β = 99 % occupied bandwidth of the 5-bin-smoothed, noise-subtracted Welch PSD.
    Obw99,
    /// x-dB bandwidth of the same PSD (outermost bins within x dB of its peak).
    XdbBandwidth,
    /// Unclipped in-band power / (N0·OBW99) over the detection box.
    SnrBox,
    /// The same ratio re-measured over the burst extent.
    SnrExtent,
    /// Power-weighted mean frequency of the noise-subtracted PSD within OBW99.
    CfoCentroid,
    /// Half the frequency of the x² spectral line (DSB-SC, BPSK, biphase).
    CfoSquareLine,
    /// A quarter of the frequency of the x⁴ spectral line (QPSK).
    CfoFourthPowerLine,
    /// Mid-point of the outer instantaneous-frequency clusters (FSK, no symbol timing).
    CfoFskMidpoint,
    /// Mid-point of FSK levels supplied by a symbol-timing estimator (T-011 hook).
    CfoFskLevels,
    /// Strongest discrete line of the snippet itself (carrier, pilot).
    CarrierLine,
    /// Snippet RF centre plus the selected CFO, in the receiver's (uncorrected) frame.
    RfCenter,
    /// [`Method::RfCenter`] corrected by a clock ppm.
    RfCenterCorrected,
    /// Burst extent length.
    Duration,
    /// Tone frequency: coarse periodogram line refined by the phase slope.
    ToneFrequency,
    /// Clock error from a measured line against its nominal frequency.
    ClockPpm,
    /// T-011: symbol rate from the guarded transition least squares (with line support).
    SymbolRateTransitions,
    /// T-011: symbol rate from independent whitened cyclic lines.
    SymbolRateLines,
    /// T-011: FSK deviation, median |IF − mid| at symbol centres with same-decision neighbours.
    FskDeviation,
    /// T-011: modulation index h = 2·deviation / symbol rate.
    ModulationIndex,
}

/// Why an estimator abstained (C13 `null` reasons plus the ones this implementation needs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// Not enough signal above the noise for this estimator.
    LowSnr,
    /// Too few samples or PSD bins.
    TooShort,
    /// ADC clipping above the threshold: amplitudes and power are not trustworthy.
    Clipped,
    /// More than one emission in the snippet.
    MultiSignal,
    /// Truncated by the edge of the analysed band.
    EdgeOfBand,
    /// No usable noise reference (pads carry signal, no sidebands, no caller floor).
    NoNoiseReference,
    /// No spectral line above the significance threshold.
    NoLine,
    /// Two comparable lines (e.g. MSK-like x²): the line does not identify one carrier.
    AmbiguousLine,
    /// The method needs a family hint it was not given (power-of-M with the wrong M gives
    /// confident garbage).
    NotRequested,
    /// A correction needs a calibration (clock ppm) that was not supplied.
    NoCalibration,
    /// The occupied band fills the snippet: the signal is wider than the extraction or the
    /// "signal" is noise.
    FillsBand,
    /// Instantaneous-frequency clusters are not separated.
    NoClusters,
    /// A value this one depends on abstained.
    Upstream,
    /// Invalid input (non-finite or inconsistent arguments).
    InvalidInput,
    /// T-011: candidates exist but the trust rule failed (they are reported, not asserted).
    Untrusted,
    /// T-011: the estimator does not apply to this signal (e.g. deviation of a non-FSK signal).
    NotApplicable,
}

/// Evidence behind a value. Fields are filled by the methods that have them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    /// Detection statistic in dB: a line's whitened significance, or the presence z-score
    /// (`10·log10 z`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub significance_db: Option<f64>,
    /// Threshold the significance was compared with, dB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold_db: Option<f64>,
    /// Line coherence `|X(f)| / Σ|x|^p` (≈ 1 for a pure carrier of `x^p`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coherence: Option<f64>,
    /// Amplitude ratio of the second-strongest line to the strongest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub second_line_ratio: Option<f64>,
    /// PSD bins used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bins: Option<u32>,
    /// Samples used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub samples: Option<u64>,
}

/// A value with uncertainty and evidence, or an abstention with a reason.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum Estimate {
    /// A measured value.
    Measured {
        /// The value (units per field).
        value: f64,
        /// One-sigma uncertainty, same units (a first-order propagation, not a CRLB, unless the
        /// method says otherwise).
        sigma: f64,
        /// Estimator.
        method: Method,
        /// Evidence.
        #[serde(default)]
        evidence: Evidence,
    },
    /// No value.
    Abstained {
        /// Estimator that declined.
        method: Method,
        /// Why.
        reason: Reason,
        /// Whatever evidence led to the decision.
        #[serde(default)]
        evidence: Evidence,
    },
}

impl Estimate {
    /// A measured value with empty evidence.
    pub fn measured(value: f64, sigma: f64, method: Method) -> Self {
        Estimate::Measured {
            value,
            sigma,
            method,
            evidence: Evidence::default(),
        }
    }

    /// An abstention with empty evidence.
    pub fn abstain(method: Method, reason: Reason) -> Self {
        Estimate::Abstained {
            method,
            reason,
            evidence: Evidence::default(),
        }
    }

    /// Replaces the evidence.
    pub fn with_evidence(mut self, e: Evidence) -> Self {
        match &mut self {
            Estimate::Measured { evidence, .. } | Estimate::Abstained { evidence, .. } => {
                *evidence = e
            }
        }
        self
    }

    /// The value, if measured.
    pub fn value(&self) -> Option<f64> {
        match self {
            Estimate::Measured { value, .. } => Some(*value),
            Estimate::Abstained { .. } => None,
        }
    }

    /// The uncertainty, if measured.
    pub fn sigma(&self) -> Option<f64> {
        match self {
            Estimate::Measured { sigma, .. } => Some(*sigma),
            Estimate::Abstained { .. } => None,
        }
    }

    /// The abstention reason, if abstained.
    pub fn reason(&self) -> Option<Reason> {
        match self {
            Estimate::Measured { .. } => None,
            Estimate::Abstained { reason, .. } => Some(*reason),
        }
    }

    /// The estimator.
    pub fn method(&self) -> Method {
        match self {
            Estimate::Measured { method, .. } | Estimate::Abstained { method, .. } => *method,
        }
    }

    /// The evidence.
    pub fn evidence(&self) -> &Evidence {
        match self {
            Estimate::Measured { evidence, .. } | Estimate::Abstained { evidence, .. } => evidence,
        }
    }

    /// True when measured.
    pub fn is_measured(&self) -> bool {
        matches!(self, Estimate::Measured { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialises_with_status_tag() {
        let m = Estimate::measured(1.5, 0.1, Method::Obw99).with_evidence(Evidence {
            bins: Some(12),
            ..Default::default()
        });
        let j = serde_json::to_value(m).unwrap();
        assert_eq!(j["status"], "measured");
        assert_eq!(j["method"], "obw99");
        assert_eq!(j["evidence"]["bins"], 12);
        assert_eq!(serde_json::from_value::<Estimate>(j).unwrap(), m);
        let a = Estimate::abstain(Method::SnrBox, Reason::LowSnr);
        let j = serde_json::to_value(a).unwrap();
        assert_eq!(j["reason"], "low_snr");
        assert_eq!(a.value(), None);
        assert_eq!(a.reason(), Some(Reason::LowSnr));
    }
}
