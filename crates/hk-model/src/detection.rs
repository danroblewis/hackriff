//! Detection (docs/07 §2.9) and Track (§2.10).

use serde::{Deserialize, Serialize};

use crate::ids::{DetectionId, ProvenanceId, SurveyId, TrackId};
use crate::region::{FreqRange, Region, TimeRange};
use crate::time::Timestamp;

/// Suspect flags on a Detection (docs/07 §2.9). Stored as a bitmask.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DetectionFlags {
    /// The ADC clipped or the front end was overloaded during the detection (suspect IMD).
    pub clipped: bool,
    /// Coincides with a SpurMask entry.
    pub spur_candidate: bool,
    /// Could be the IQ image of a stronger signal.
    pub image_candidate: bool,
    /// SNR close to the detection threshold.
    pub marginal: bool,
}

impl DetectionFlags {
    const CLIPPED: u32 = 1 << 0;
    const SPUR_CANDIDATE: u32 = 1 << 1;
    const IMAGE_CANDIDATE: u32 = 1 << 2;
    const MARGINAL: u32 = 1 << 3;

    /// Bitmask form used in storage.
    pub const fn bits(self) -> u32 {
        (self.clipped as u32 * Self::CLIPPED)
            | (self.spur_candidate as u32 * Self::SPUR_CANDIDATE)
            | (self.image_candidate as u32 * Self::IMAGE_CANDIDATE)
            | (self.marginal as u32 * Self::MARGINAL)
    }

    /// Parses the bitmask form. Unknown bits are ignored.
    pub const fn from_bits(bits: u32) -> Self {
        Self {
            clipped: bits & Self::CLIPPED != 0,
            spur_candidate: bits & Self::SPUR_CANDIDATE != 0,
            image_candidate: bits & Self::IMAGE_CANDIDATE != 0,
            marginal: bits & Self::MARGINAL != 0,
        }
    }

    /// Any flag set: the detection should not be trusted as a real emission without checks.
    pub const fn any(self) -> bool {
        self.bits() != 0
    }
}

/// The atomic measurement (docs/07 §2.9). **Immutable once written**: the repository has no
/// update path for it.
///
/// Links formed *after* the detection is written live elsewhere, so the row never changes:
/// track membership in the track↔detection link table, the IQ snippet in
/// `Recording.trigger`, and emitter association in emitter links.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Detection {
    /// Time-sortable id.
    pub id: DetectionId,
    /// Survey that produced it.
    pub survey_id: SurveyId,
    /// Time extent.
    pub time: TimeRange,
    /// Centre frequency, Hz.
    pub f_center_hz: f64,
    /// Occupied bandwidth (99 % power), Hz. The occupied extent is `f_center_hz ± obw_hz / 2`.
    pub obw_hz: f64,
    /// x-dB bandwidth, Hz, if measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xdb_bandwidth_hz: Option<f64>,
    /// The x of `xdb_bandwidth_hz`, dB (e.g. 26).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xdb_level_db: Option<f64>,
    /// Peak SNR, dB.
    pub snr_peak_db: f64,
    /// Mean SNR, dB.
    pub snr_mean_db: f64,
    /// Spectral kurtosis over the detection, if computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sk: Option<f64>,
    /// Trust record.
    pub provenance_ref: ProvenanceId,
    /// Suspect flags.
    pub flags: DetectionFlags,
}

impl Detection {
    /// Occupied frequency extent.
    pub fn freq(&self) -> FreqRange {
        FreqRange::centered(self.f_center_hz, self.obw_hz)
    }

    /// Frequency × time box.
    pub fn region(&self) -> Region {
        Region::new(self.freq(), self.time)
    }
}

/// Track lifecycle (docs/07 §2.10).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "state", content = "into")]
pub enum TrackState {
    /// Still receiving detections.
    Open,
    /// Idle timeout expired.
    Closed,
    /// Merged into another track (recorded, not overwritten).
    MergedInto(TrackId),
}

/// Timing features of a track. All optional: each needs enough detections to estimate.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TimingFeatures {
    /// Repetition period, s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_s: Option<f64>,
    /// Fraction of time on air, 0–1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duty_cycle: Option<f64>,
    /// Mean inter-arrival time, s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inter_arrival_mean_s: Option<f64>,
    /// Inter-arrival standard deviation, s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inter_arrival_std_s: Option<f64>,
    /// Hop set centre frequencies, Hz, for frequency hoppers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hop_set_hz: Vec<f64>,
    /// Hop rate, hops/s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hop_rate_hz: Option<f64>,
    /// TDMA frame period, s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tdma_frame_s: Option<f64>,
    /// Tracks that repeatedly co-occur with this one (inter-channel co-occurrence).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub co_occurring: Vec<TrackId>,
}

/// A linked series of detections (docs/07 §2.10). A mutable aggregate: it grows as detections
/// arrive. Membership is append-only in the link table; merges are recorded in `state`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Track {
    /// Id.
    pub id: TrackId,
    /// Lifecycle state.
    pub state: TrackState,
    /// Track this one was split from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split_from: Option<TrackId>,
    /// Time extent of member detections.
    pub time: TimeRange,
    /// Representative centre frequency, Hz.
    pub f_center_hz: f64,
    /// Representative bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// Number of member detections.
    pub detection_count: u64,
    /// Timing features.
    pub timing: TimingFeatures,
    /// Last update.
    pub updated_at: Timestamp,
}
