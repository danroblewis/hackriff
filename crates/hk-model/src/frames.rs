//! Spectrum frames (docs/07 §2.3, §2.4) and SpectrumTile keys and stats (§2.5).
//!
//! Frames are **not stored in SQLite**: they are ephemeral, consumed by detection and folded into
//! tiles. They have no UUID; identity is [`FrameKey`] `(survey_id, seq)`. SpectrumTiles are
//! persisted by hk-store's pyramid (T-017), keyed by [`TileKey`]
//! `(scheme, level, f_block, t_block)`; this module only defines the key and the stats shape.
//!
//! Per-bin arrays are `f32`: they dominate hot-path memory (a 60 000-bin sweep row is 240 kB
//! instead of 480 kB) and dB values need nowhere near f64 precision. Scalars stay `f64`.

use serde::{Deserialize, Serialize};

use crate::ids::{CalibrationStateId, ProvenanceId, SurveyId};
use crate::region::{FreqRange, TimeRange};
use crate::time::Timestamp;

/// Identity of a SweepFrame or SpectrumFrame: its position in a Survey's frame sequence.
///
/// `seq` is one counter per survey per frame kind. A SpectrumFrame stream at a second resolution
/// (burst vs narrow carrier, docs/07 §2.4) shares the counter and is told apart by `rbw_hz`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FrameKey {
    /// Survey the frame belongs to.
    pub survey_id: SurveyId,
    /// Sequence number within the survey, from 0.
    pub seq: u64,
}

/// Unit of power values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PowerUnit {
    /// dB relative to ADC full scale (uncalibrated).
    Dbfs,
    /// dBm at the antenna port, via a CalibrationState power table.
    Dbm,
}

/// One wideband survey observation (docs/07 §2.3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SweepFrame {
    /// Identity.
    pub key: FrameKey,
    /// Sweep timestamp (hackrf_sweep stamps once per full sweep).
    pub t: Timestamp,
    /// Frequency extent of `power`.
    pub freq: FreqRange,
    /// Bin width, Hz.
    pub bin_width_hz: f64,
    /// Unit of `power`.
    pub unit: PowerUnit,
    /// Power per bin, ascending frequency.
    pub power: Vec<f32>,
    /// Trust record.
    pub provenance_ref: ProvenanceId,
}

/// Digital-phosphor persistence histogram: `counts[bin * levels + level]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Persistence {
    /// Power of level 0, same unit as the frame.
    pub floor: f64,
    /// Power step per level, dB.
    pub step_db: f64,
    /// Levels per bin.
    pub levels: u32,
    /// Hit counts, row-major by bin then level.
    pub counts: Vec<u32>,
}

/// A dwell-window frame (docs/07 §2.4).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpectrumFrame {
    /// Identity.
    pub key: FrameKey,
    /// Frame timestamp (centre of the FFT window).
    pub t: Timestamp,
    /// Centre frequency, Hz.
    pub f_center_hz: f64,
    /// Span, Hz.
    pub span_hz: f64,
    /// Resolution bandwidth, Hz.
    pub rbw_hz: f64,
    /// Unit of `psd`.
    pub unit: PowerUnit,
    /// Power spectral density per bin, ascending frequency.
    pub psd: Vec<f32>,
    /// Persistence histogram, if computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistence: Option<Persistence>,
    /// Spectral kurtosis per bin, if computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sk: Option<Vec<f32>>,
    /// Trust record.
    pub provenance_ref: ProvenanceId,
}

/// Address of a SpectrumTile in the history pyramid (docs/07 §2.5).
///
/// `f_block` and `t_block` are block indices at `level`: `floor(f_hz / block_width_hz(level))`
/// and `floor(t_ns / block_duration_ns(level))`. The block sizes and level ladder are defined by
/// the pyramid scheme `scheme` (T-017), so a reconfigured pyramid gets a new scheme id and old
/// tiles are never read with the wrong block sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TileKey {
    /// Pyramid scheme/version id (block sizes, level ladder, regrid rules).
    pub scheme: u16,
    /// Pyramid level, 0 = finest.
    pub level: u8,
    /// Frequency block index at this level.
    pub f_block: i64,
    /// Time block index at this level.
    pub t_block: i64,
}

/// Per-bin statistics of a tile. All vectors have one entry per bin.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TileStats {
    /// Max-hold (bursts and persistent emitters).
    pub max: Vec<f32>,
    /// Mean.
    pub mean: Vec<f32>,
    /// Low percentile (noise floor).
    pub low_percentile: Vec<f32>,
    /// Which percentile `low_percentile` is, e.g. 10.0.
    pub percentile: f64,
    /// Bin was actually observed during the tile; "not observed" is not "quiet" (C26).
    pub observed: Vec<bool>,
}

/// A persisted history tile (docs/07 §2.5). Stored by hk-store, not by the SQLite repository.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpectrumTile {
    /// Pyramid address.
    pub key: TileKey,
    /// Frequency extent.
    pub freq: FreqRange,
    /// Time extent.
    pub time: TimeRange,
    /// Bin width, Hz.
    pub bin_width_hz: f64,
    /// Unit of the stats. dBFS is kept alongside `calibration_ref` so later tables can be
    /// reapplied (C26).
    pub unit: PowerUnit,
    /// Calibration in force when the tile was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration_ref: Option<CalibrationStateId>,
    /// Fraction of contributing frames flagged clipped or overloaded.
    pub suspect_fraction: f64,
    /// The statistics.
    pub stats: TileStats,
}
