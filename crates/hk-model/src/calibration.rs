//! CalibrationState (docs/07 §2.7) and SpurMask (§2.8).
//!
//! Both are **versioned by row**: each version has its own id and is immutable. A new
//! measurement inserts a new row whose `supersedes` names the previous version. Provenance rows
//! pin the exact version they were measured under.

use serde::{Deserialize, Serialize};

use crate::ids::{CalibrationStateId, SpurMaskId};
use crate::region::{FreqRange, TimeRange};
use crate::time::Timestamp;

/// How the frequency correction was measured.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CalibrationMethod {
    /// LTE primary synchronisation signal.
    LtePss,
    /// Broadcast FM 19 kHz pilot.
    FmPilot,
    /// GNSS-derived.
    Gnss,
    /// A known reference tone or signal generator.
    ReferenceTone,
    /// A land-mobile-radio channel raster (docs/19 §7.6a, T-560): the circular-mean offset of
    /// occupied channels from an assumed grid, which is the receiver's own clock error because
    /// every real emission on the grid shares it. Needs no known-frequency reference signal --
    /// only that *some* of the detections it already has sit on a raster.
    LmrRaster,
    /// Entered by hand or a factory value.
    Manual,
}

/// The front-end gain setting a power-calibration point was measured at.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GainSetting {
    /// LNA gain, dB.
    pub lna_db: f64,
    /// VGA gain, dB.
    pub vga_db: f64,
    /// RF amp on.
    pub amp_on: bool,
}

/// A frequency × gain point of the dBFS→dBm power calibration table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PowerCalPoint {
    /// Frequency, Hz.
    pub f_hz: f64,
    /// Total front-end gain, dB (LNA + VGA + amp).
    pub gain_db: f64,
    /// Add to dBFS to get dBm.
    pub offset_db: f64,
    /// The exact LNA/VGA/amp setting (T-021). A total gain cannot identify a HackRF state (24/20
    /// and 16/28 both total 44 dB, and the amp's real gain is not its nominal 11 dB), so the
    /// radiometry product applies only points that carry this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gain: Option<GainSetting>,
    /// Standard uncertainty of `offset_db`, dB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncertainty_db: Option<f64>,
}

/// One calibration version (docs/07 §2.7).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationState {
    /// Version id.
    pub id: CalibrationStateId,
    /// Previous version this one replaces, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<CalibrationStateId>,
    /// Device calibrated.
    pub device_id: String,
    /// Frequency error, ppm (positive = oscillator fast).
    pub ppm: f64,
    /// How `ppm` was measured.
    pub method: CalibrationMethod,
    /// When it was measured.
    pub measured_at: Timestamp,
    /// Validity window, if bounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid: Option<TimeRange>,
    /// Board temperature at measurement, °C.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature_c: Option<f64>,
    /// Power calibration table; empty when uncalibrated for power.
    #[serde(default)]
    pub power_table: Vec<PowerCalPoint>,
    /// How concentrated the fit that produced `ppm` was, 0–1, when the method reports one (e.g.
    /// [`CalibrationMethod::LmrRaster`]'s circular-mean concentration). `None` for methods that
    /// have no analogous figure (a signal generator's tone, a manual entry).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}

/// One internal spur or image rule.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum SpurRule {
    /// A fixed internal spur, measured with a terminated input.
    Spur {
        /// Affected frequencies.
        freq: FreqRange,
        /// Level, dBFS, at the gain it was measured.
        level_dbfs: f64,
        /// Gain setting (total dB) it was measured at, if gain-specific.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gain_db: Option<f64>,
    },
    /// IQ image of a signal: mirror about the tuned centre.
    IqImage {
        /// Image rejection, dB.
        rejection_db: f64,
    },
    /// DC/LO leakage at the tuned centre.
    CenterLeak {
        /// Half-width of the masked region, Hz.
        half_width_hz: f64,
    },
}

/// One spur-mask version (docs/07 §2.8).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpurMask {
    /// Version id.
    pub id: SpurMaskId,
    /// Previous version this one replaces, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<SpurMaskId>,
    /// Device measured.
    pub device_id: String,
    /// When it was measured.
    pub measured_at: Timestamp,
    /// Rules.
    pub rules: Vec<SpurRule>,
}
