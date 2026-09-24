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

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::evidence::MetricId;
use crate::stage::CALIBRATED_CLAIM_CAP_BITS;

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

// ---------------------------------------------------------------------------------------------
// The loader (T-660): `hackriff.calibration/1` -> `CalibrationTable`, and the two accessors.
// ---------------------------------------------------------------------------------------------

/// One expressible answer: a level the generator confirmed is within
/// [`EXPRESSIBILITY_TOLERANCE_BITS`] of its claim (§13.2's `levels`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Level {
    /// The claimed significance.
    pub bits: f32,
    /// The raw threshold that realises it.
    pub threshold: f32,
    /// What the threshold actually realises (within tolerance of `bits`).
    pub realised_bits: f32,
}

/// A level the generator tried and refused to publish (§13.2's `unexpressible`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnexpressibleLevel {
    /// What was asked for.
    pub bits: f32,
    /// What the support actually realised at the nearest achievable threshold.
    pub realised_bits: f32,
    /// Why it was refused: `"atom"`, `"above_sample_ceiling"` or `"above_calibrated_claim_cap"`.
    pub reason: String,
}

impl UnexpressibleLevel {
    fn as_reason(&self, sample_ceiling_bits: f32, windows: u32) -> Unexpressible {
        match self.reason.as_str() {
            "above_sample_ceiling" => Unexpressible::AboveSampleCeiling {
                ceiling_bits: sample_ceiling_bits,
                windows,
            },
            "above_calibrated_claim_cap" => Unexpressible::AboveCalibratedClaimCap {
                cap_bits: CALIBRATED_CLAIM_CAP_BITS,
            },
            // "atom" and anything unrecognised: the safe default is the measured shortfall, never
            // a silent pass — an unknown reason string is treated the same as an atom, not
            // dropped.
            _ => Unexpressible::Atom {
                realised_bits: self.realised_bits,
            },
        }
    }
}

/// One `(metric, null, n)` cell within a loaded file — the file's own `bucket` applies to every
/// cell it carries (§13.2: a file is per fill bucket via its `conditioning.bucket`).
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawCell {
    metric: MetricId,
    null: NullKind,
    n: u32,
    windows: u32,
    sample_ceiling_bits: f32,
    #[serde(default)]
    distinct_values: u32,
    levels: Vec<Level>,
    #[serde(default)]
    unexpressible: Vec<UnexpressibleLevel>,
    admissible_bits: f32,
}

/// The `conditioning` block (§13.2). Only `bucket` is read by the loader; the rest rides along
/// for provenance and is not re-validated here.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
struct Conditioning {
    key: String,
    bucket: FillBucket,
    #[serde(default)]
    sigma_lsb_range: Option<[f32; 2]>,
    #[serde(default)]
    clip_fraction_max: Option<f32>,
}

/// The file as written to disk (§13.2's schema, `synth/calibration/<block>.json`).
#[derive(Clone, Debug, Deserialize, Serialize)]
struct RawFile {
    schema: String,
    block: String,
    #[serde(default)]
    generated_utc: Option<String>,
    #[serde(default)]
    generator: Option<String>,
    conditioning: Conditioning,
    #[serde(default)]
    correlation: Option<serde_json::Value>,
    #[serde(default)]
    groups: Option<serde_json::Value>,
    tables: Vec<RawCell>,
}

/// Why a calibration file failed to load.
#[derive(Debug)]
pub enum LoadError {
    /// Malformed JSON, or a field of the wrong shape.
    Parse(serde_json::Error),
    /// `schema` was not [`CALIBRATION_SCHEMA`] — a `raw`-only (pre-§13.2) table is not loadable.
    UnknownSchema(String),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Parse(e) => write!(f, "calibration file: {e}"),
            LoadError::UnknownSchema(s) => {
                write!(
                    f,
                    "calibration file: unknown schema {s:?}, want {CALIBRATION_SCHEMA:?}"
                )
            }
        }
    }
}

impl std::error::Error for LoadError {}

/// A loaded `hackriff.calibration/1` file: one `(block, fill bucket)` pair, indexed by
/// `(metric, null, n)`.
#[derive(Clone, Debug, PartialEq)]
pub struct CalibrationTable {
    block: String,
    bucket: FillBucket,
    cells: HashMap<(MetricId, NullKind, u32), RawCell>,
}

impl CalibrationTable {
    /// Parses a `hackriff.calibration/1` document (§13.2). Refuses anything not carrying that
    /// schema name — there is no fallback to the pre-§13.2 `raw`-only shape.
    pub fn load(json: &str) -> Result<Self, LoadError> {
        let raw: RawFile = serde_json::from_str(json).map_err(LoadError::Parse)?;
        if raw.schema != CALIBRATION_SCHEMA {
            return Err(LoadError::UnknownSchema(raw.schema));
        }
        let bucket = raw.conditioning.bucket;
        let mut cells = HashMap::with_capacity(raw.tables.len());
        for cell in raw.tables {
            cells.insert((cell.metric, cell.null, cell.n), cell);
        }
        Ok(CalibrationTable {
            block: raw.block,
            bucket,
            cells,
        })
    }

    /// The block this table was generated for (`name@version`).
    pub fn block(&self) -> &str {
        &self.block
    }

    /// The fill bucket every cell in this table is conditioned on.
    pub fn bucket(&self) -> FillBucket {
        self.bucket
    }

    fn lookup(&self, cell: &CellId) -> Option<&RawCell> {
        if cell.block != self.block || cell.bucket != self.bucket {
            return None;
        }
        self.cells.get(&(cell.metric, cell.null, cell.n))
    }

    /// raw → bits (§13.2). Looks up the highest expressible level whose threshold `raw` clears;
    /// below the lowest level credits nothing, at or above the cell's ceiling saturates rather
    /// than reporting a level the table never measured. A missing cell credits nothing
    /// ([`Score::NoTable`]).
    pub fn score(&self, cell: &CellId, raw: f32) -> Score {
        let Some(c) = self.lookup(cell) else {
            return Score::NoTable { cell: cell.clone() };
        };
        if !raw.is_finite() {
            return Score::Bits {
                bits: 0.0,
                level: 0.0,
            };
        }
        let mut credited: Option<&Level> = None;
        for level in &c.levels {
            if raw >= level.threshold {
                credited = Some(level);
            } else {
                break;
            }
        }
        match credited {
            None => Score::Bits {
                bits: 0.0,
                level: 0.0,
            },
            Some(level) if level.bits >= c.admissible_bits => Score::Saturated {
                admissible_bits: c.admissible_bits,
            },
            Some(level) => Score::Bits {
                bits: level.bits,
                level: level.bits,
            },
        }
    }

    /// bits → raw (§13.2). A `floor_j` calls this: an expressible level meeting or exceeding
    /// `requested_bits` is snapped **up** and returned as [`Threshold::At`]; nothing reachable at
    /// or above `requested_bits` is [`Threshold::Shortfall`] — the caller's floor is
    /// `floor_unreachable`, never lowered. A missing cell is [`Threshold::NoTable`].
    pub fn threshold(&self, cell: &CellId, requested_bits: f32) -> Threshold {
        let Some(c) = self.lookup(cell) else {
            return Threshold::NoTable { cell: cell.clone() };
        };
        if requested_bits.is_finite() && requested_bits <= c.admissible_bits {
            if let Some(level) = c
                .levels
                .iter()
                .filter(|l| l.bits >= requested_bits)
                .min_by(|a, b| a.bits.total_cmp(&b.bits))
            {
                return Threshold::At {
                    bits: level.bits,
                    raw: level.threshold,
                };
            }
        }
        // Shortfall: attribute the refusal to whichever recorded `unexpressible` entry is the
        // closest one at or above what was asked — that is the generator's own measurement of
        // the shortfall, never a re-derivation.
        let reason = c
            .unexpressible
            .iter()
            .filter(|u| u.bits >= requested_bits)
            .min_by(|a, b| a.bits.total_cmp(&b.bits))
            .map(|u| u.as_reason(c.sample_ceiling_bits, c.windows))
            .unwrap_or_else(|| {
                if requested_bits > CALIBRATED_CLAIM_CAP_BITS {
                    Unexpressible::AboveCalibratedClaimCap {
                        cap_bits: CALIBRATED_CLAIM_CAP_BITS,
                    }
                } else if requested_bits > c.sample_ceiling_bits {
                    Unexpressible::AboveSampleCeiling {
                        ceiling_bits: c.sample_ceiling_bits,
                        windows: c.windows,
                    }
                } else {
                    Unexpressible::Atom {
                        realised_bits: c.admissible_bits,
                    }
                }
            });
        Threshold::Shortfall {
            requested_bits,
            admissible_bits: c.admissible_bits,
            reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0015 §13.2's own worked example: `eye_open` at n = 112, atoms at the 4- and 6-bit
    /// quantiles, `admissible_bits` capped at 3.0 by the atom.
    const EYE_OPEN_TABLE: &str = r#"{
        "schema": "hackriff.calibration/1",
        "block": "clock_recovery@1",
        "generated_utc": "2026-09-21T00:00:00Z",
        "generator": "py/hkpy/calibrate.py@deadbeef",
        "conditioning": { "key": "adc_fill_sigma_lsb", "bucket": "nominal",
                          "sigma_lsb_range": [0.5, 77.0], "clip_fraction_max": 0.30 },
        "tables": [
            { "metric": "eye_open", "null": "noise", "n": 112, "windows": 4200,
              "sample_ceiling_bits": 12.04, "distinct_values": 66,
              "levels": [
                { "bits": 1.0, "threshold": 0.0412, "realised_bits": 1.00 },
                { "bits": 2.0, "threshold": 0.0630, "realised_bits": 1.99 },
                { "bits": 3.0, "threshold": 0.0881, "realised_bits": 2.97 }
              ],
              "unexpressible": [
                { "bits": 4.0, "realised_bits": 2.41, "reason": "atom" },
                { "bits": 6.0, "realised_bits": 2.41, "reason": "atom" }
              ],
              "admissible_bits": 3.0 }
        ]
    }"#;

    fn eye_open_cell(n: u32) -> CellId {
        CellId {
            block: "clock_recovery@1".into(),
            metric: MetricId::EyeOpen,
            null: NullKind::Noise,
            n,
            bucket: FillBucket::Nominal,
        }
    }

    #[test]
    fn the_refusal_is_non_vacuous_not_a_threshold() {
        let table = CalibrationTable::load(EYE_OPEN_TABLE).unwrap();
        // Asking for 6.0 bits from a table whose highest expressible level is 3.0: a Shortfall
        // carrying the atom the generator actually measured, never a threshold value.
        assert_eq!(
            table.threshold(&eye_open_cell(112), 6.0),
            Threshold::Shortfall {
                requested_bits: 6.0,
                admissible_bits: 3.0,
                reason: Unexpressible::Atom {
                    realised_bits: 2.41
                },
            }
        );
    }

    #[test]
    fn a_missing_cell_is_no_table_and_credits_zero_bits() {
        let table = CalibrationTable::load(EYE_OPEN_TABLE).unwrap();
        // n = 999 was never generated: no cell at all.
        let missing = eye_open_cell(999);
        assert_eq!(
            table.threshold(&missing, 3.0),
            Threshold::NoTable {
                cell: missing.clone()
            }
        );
        let score = table.score(&missing, 0.09);
        assert_eq!(score, Score::NoTable { cell: missing });
        assert_eq!(score.credited_bits(), 0.0);
    }

    #[test]
    fn a_floor_above_admissible_bits_is_floor_unreachable_never_lowered() {
        let table = CalibrationTable::load(EYE_OPEN_TABLE).unwrap();
        // The stage default floor (§1.3) is 6 bits; this cell's ceiling is 3.0. The floor must
        // report unreachable, not silently drop to what the table can offer.
        match table.threshold(
            &eye_open_cell(112),
            crate::stage::default_floor_bits(crate::stage::Stage::S2).unwrap(),
        ) {
            Threshold::Shortfall {
                requested_bits,
                admissible_bits,
                ..
            } => {
                assert_eq!(requested_bits, 6.0);
                assert_eq!(admissible_bits, 3.0);
            }
            other => panic!("expected Shortfall (floor_unreachable), got {other:?}"),
        }
    }

    #[test]
    fn an_expressible_level_snaps_up_never_down() {
        let table = CalibrationTable::load(EYE_OPEN_TABLE).unwrap();
        // Asking for 1.5 bits snaps up to the 2.0-bit level, never down to 1.0.
        assert_eq!(
            table.threshold(&eye_open_cell(112), 1.5),
            Threshold::At {
                bits: 2.0,
                raw: 0.0630
            }
        );
    }

    #[test]
    fn scoring_below_the_first_level_credits_nothing_and_at_the_top_saturates() {
        let table = CalibrationTable::load(EYE_OPEN_TABLE).unwrap();
        let cell = eye_open_cell(112);
        assert_eq!(
            table.score(&cell, 0.0),
            Score::Bits {
                bits: 0.0,
                level: 0.0
            }
        );
        assert_eq!(
            table.score(&cell, 0.05),
            Score::Bits {
                bits: 1.0,
                level: 1.0
            }
        );
        // At or beyond the top expressible level's threshold: saturate at admissible_bits, never
        // claim a level the table never measured (that is exactly the eye_open 6-bit lie).
        assert_eq!(
            table.score(&cell, 10.0),
            Score::Saturated {
                admissible_bits: 3.0
            }
        );
    }

    #[test]
    fn a_raw_only_schema_is_refused() {
        // §13.2: "raw"-only tables (the pre-amendment shape) are not loadable, even when every
        // other field is otherwise well-formed.
        let old = r#"{"schema": "raw", "block": "x@1",
            "conditioning": { "key": "adc_fill_sigma_lsb", "bucket": "nominal" },
            "tables": [] }"#;
        assert!(matches!(
            CalibrationTable::load(old),
            Err(LoadError::UnknownSchema(s)) if s == "raw"
        ));
    }

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
