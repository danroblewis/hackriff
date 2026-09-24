//! Calibrated nulls: what a `hackriff.calibration/1` table may answer, and the ADC-fill
//! conditioning key (ADR-0015 §13.2, §13.3).
//!
//! **The loader, the two accessors and the generator are T-660's.** This module fixes their
//! answer types so every caller is written against the refusal from day one:
//!
//! > A calibration table never answers a question it cannot answer. Asked for a level it does not
//! > hold, it returns [`Threshold::Shortfall`]; asked for a cell it does not have, it returns
//! > `NoTable` and the metric contributes **0.0 bits**. Nearest-quantile, interpolation and
//! > extrapolation are all forbidden (§13.2).
//!
//! It also carries §13.3's runtime fill rule ([`FillBucket::classify`]), which is a pure function
//! of two recorded numbers and fails closed. **Known open item:** T-619 (docs/21 §10) measured the
//! `nominal` bucket over-claiming 5–6 bits on AM/OOK at high clip and found the runtime rule must
//! read the **noise floor's** fill, not the window's; that amendment is not taken in ADR-0015, so
//! the thresholds below are §13.3's as written (ADR-0015 §16.4).

use serde::{Deserialize, Serialize};

use crate::evidence::MetricId;

/// Schema name of a calibration file (`synth/calibration/<block>.json`).
pub const CALIBRATION_SCHEMA: &str = "hackriff.calibration/1";

/// A level is expressible when its realised significance is within this many bits of the claim
/// (§13.2). The generator's tolerance, stated in the file.
pub const EXPRESSIBILITY_TOLERANCE_BITS: f32 = 0.25;

/// Below this per-component noise σ, in ADC LSB, a window is `under_filled` (§13.3).
pub const FILL_SIGMA_MIN_LSB: f32 = 0.5;

/// Above this clipped-sample fraction a window is `over_clipped` (§13.3).
pub const CLIP_FRACTION_MAX: f32 = 0.30;

/// Minimum windows per (block, metric, null, n, fill bucket) cell (§13.3's sampling budget). A
/// cell with fewer is loadable but declares its lower `sample_ceiling_bits`.
pub const WINDOWS_PER_CELL_FLOOR: u32 = 4096;

/// The ADC-fill bucket a window's calibrated metrics are conditioned on (§13.3). Only `Nominal`
/// has tables; the other two contribute 0.0 bits from every calibrated metric.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FillBucket {
    /// σ ≥ 0.5 LSB and clip fraction ≤ 0.30.
    Nominal,
    /// σ < 0.5 LSB, **or σ unknown** (an unknown fill is not a good fill).
    UnderFilled,
    /// Clip fraction > 0.30 (a boundary of the measurement, not the physics).
    OverClipped,
}

impl FillBucket {
    /// §13.3's runtime rule. `sigma_lsb` is the per-component noise σ in LSB
    /// (`Provenance.noise_sigma_lsb`); `clip_fraction` the window's clipped share. A missing or
    /// non-finite σ is `UnderFilled`; a missing clip fraction is read as no clipping, because the
    /// sticky `Provenance.overload` flag, not this function, owns the unknown-clip case.
    /// Under-fill is checked first: a window both under-filled and clipped earns nothing either
    /// way, and under-fill is the measured hazard.
    pub fn classify(sigma_lsb: Option<f32>, clip_fraction: Option<f32>) -> FillBucket {
        match sigma_lsb {
            Some(s) if s.is_finite() && s >= FILL_SIGMA_MIN_LSB => {}
            _ => return FillBucket::UnderFilled,
        }
        match clip_fraction {
            Some(c) if c > CLIP_FRACTION_MAX || c.is_nan() => FillBucket::OverClipped,
            _ => FillBucket::Nominal,
        }
    }

    /// Whether calibrated metrics may score at all in this bucket.
    pub const fn has_tables(self) -> bool {
        matches!(self, FillBucket::Nominal)
    }
}

/// The null a table was sampled under (§13.2's `null` field).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NullKind {
    /// Pure noise.
    #[serde(rename = "noise")]
    Noise,
    /// The right signal at a mismatched symbol rate.
    #[serde(rename = "mismatch:symbol_rate")]
    MismatchSymbolRate,
    /// The right signal at a mismatched centre.
    #[serde(rename = "mismatch:centre")]
    MismatchCentre,
    /// The right signal at a mismatched deviation.
    #[serde(rename = "mismatch:deviation")]
    MismatchDeviation,
}

/// One calibration cell: per (block@version, metric, null, support `n`, fill bucket). Supports are
/// enumerated, never interpolated.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellId {
    /// `name@version`; a version bump invalidates the table.
    pub block: String,
    /// The metric.
    pub metric: MetricId,
    /// The null.
    pub null: NullKind,
    /// Support: symbols, bursts or frames.
    pub n: u32,
    /// The fill bucket.
    pub bucket: FillBucket,
}

/// raw → bits (scoring). Saturates at `admissible_bits`; never interpolates above the top level.
#[derive(Clone, Debug, PartialEq)]
pub enum Score {
    /// An expressible level was met.
    Bits {
        /// Bits credited.
        bits: f32,
        /// The table level those bits came from.
        level: f32,
    },
    /// Beyond the table's reach: credited at `admissible_bits`, flagged as a floor on what is
    /// knowable rather than a measurement of it.
    Saturated {
        /// The table's `admissible_bits`.
        admissible_bits: f32,
    },
    /// No table for this cell: credits 0.0 bits.
    NoTable {
        /// The missing cell.
        cell: CellId,
    },
}

impl Score {
    /// The bits this answer credits.
    pub fn credited_bits(&self) -> f32 {
        match self {
            Score::Bits { bits, .. } => *bits,
            Score::Saturated { admissible_bits } => *admissible_bits,
            Score::NoTable { .. } => 0.0,
        }
    }
}

/// bits → raw (a floor or threshold test, e.g. `floor_j`). May **refuse**.
#[derive(Clone, Debug, PartialEq)]
pub enum Threshold {
    /// An expressible level, snapped **up** (never down).
    At {
        /// The level actually used (≥ the one requested).
        bits: f32,
        /// The raw threshold for it.
        raw: f32,
    },
    /// The table cannot express the requested level. The caller gets the achievable number or
    /// nothing — never the raw value sitting at that quantile. For a `floor_j` this is
    /// `floor_unreachable`: the floor is not lowered and is not treated as met.
    Shortfall {
        /// What was asked.
        requested_bits: f32,
        /// The most this table can express.
        admissible_bits: f32,
        /// Why.
        reason: Unexpressible,
    },
    /// No table for this cell.
    NoTable {
        /// The missing cell.
        cell: CellId,
    },
}

/// Why a level cannot be expressed (§13.2).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Unexpressible {
    /// An atom in the null distribution sits on the quantile: `eye_open` at n = 112, asked for
    /// 6.0 bits, realises 2.41.
    Atom {
        /// The significance the threshold actually has.
        realised_bits: f32,
    },
    /// Above `log₂ N` for the N windows sampled.
    AboveSampleCeiling {
        /// `log₂ windows`.
        ceiling_bits: f32,
        /// N.
        windows: u32,
    },
    /// Above the per-metric calibrated claim cap (6 bits, §13.2).
    AboveCalibratedClaimCap {
        /// The cap.
        cap_bits: f32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_fill_is_under_filled_not_nominal() {
        assert_eq!(
            FillBucket::classify(None, Some(0.0)),
            FillBucket::UnderFilled
        );
        assert_eq!(
            FillBucket::classify(Some(f32::NAN), None),
            FillBucket::UnderFilled
        );
        assert_eq!(
            FillBucket::classify(Some(0.21), Some(0.0)),
            FillBucket::UnderFilled
        );
        assert_eq!(
            FillBucket::classify(Some(0.21), Some(0.9)),
            FillBucket::UnderFilled
        );
        assert_eq!(
            FillBucket::classify(Some(0.5), Some(0.30)),
            FillBucket::Nominal
        );
        assert_eq!(FillBucket::classify(Some(43.6), None), FillBucket::Nominal);
        assert_eq!(
            FillBucket::classify(Some(2.0), Some(0.31)),
            FillBucket::OverClipped
        );
        assert!(FillBucket::Nominal.has_tables());
        assert!(!FillBucket::UnderFilled.has_tables());
        assert!(!FillBucket::OverClipped.has_tables());
    }

    #[test]
    fn a_missing_table_credits_nothing_and_saturation_credits_only_the_admissible_bits() {
        let cell = CellId {
            block: "clock_recovery@1".into(),
            metric: MetricId::EyeOpen,
            null: NullKind::Noise,
            n: 112,
            bucket: FillBucket::Nominal,
        };
        assert_eq!(Score::NoTable { cell }.credited_bits(), 0.0);
        assert_eq!(
            Score::Saturated {
                admissible_bits: 3.0
            }
            .credited_bits(),
            3.0
        );
    }

    #[test]
    fn null_kinds_use_the_file_spelling() {
        assert_eq!(
            serde_json::to_value(NullKind::MismatchSymbolRate).unwrap(),
            "mismatch:symbol_rate"
        );
        assert_eq!(serde_json::to_value(NullKind::Noise).unwrap(), "noise");
        assert_eq!(
            serde_json::to_value(FillBucket::UnderFilled).unwrap(),
            "under_filled"
        );
    }
}
