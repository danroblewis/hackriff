//! Detection (docs/07 §2.9) and Track (§2.10).

use serde::{Deserialize, Serialize};

use crate::ids::{DetectionId, ProvenanceId, SpurMaskId, SurveyId, TrackId};
use crate::region::{FreqRange, Region, TimeRange};
use crate::time::Timestamp;

/// Why a detection is a spur candidate (spike S4). Only meaningful with
/// [`DetectionFlags::spur_candidate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum SpurReason {
    /// A harmonic of the reference clock (e.g. multiples of the 10 MHz / 25 MHz reference).
    RefHarmonic,
    /// DC offset / LO leakage at the tuned centre.
    Dc,
    /// At a fixed offset from the LO or a synthesiser product of it.
    LoRelative,
    /// Part of a regularly spaced comb (switching supplies, USB, digital clocks).
    Comb,
    /// At a harmonic of the sample clock (`n × fs`), crystal-locked to the receiver (added in the
    /// T-006 review: the 433 MHz control's 434.000 MHz line is 217 × 2 Msps). A candidate only:
    /// a real carrier can sit on a clock harmonic too.
    ClockHarmonic,
    /// Listed in a measured SpurMask version.
    SpurMap {
        /// The SpurMask version that lists it.
        mask: SpurMaskId,
    },
}

impl SpurReason {
    /// Storage name of the reason kind (the serde `kind` tag).
    pub const fn kind_str(&self) -> &'static str {
        match self {
            SpurReason::RefHarmonic => "ref-harmonic",
            SpurReason::Dc => "dc",
            SpurReason::LoRelative => "lo-relative",
            SpurReason::Comb => "comb",
            SpurReason::ClockHarmonic => "clock-harmonic",
            SpurReason::SpurMap { .. } => "spur-map",
        }
    }

    /// Rebuilds a reason from its storage parts. `spur-map` needs `mask`; the others ignore it.
    pub fn from_parts(kind: &str, mask: Option<SpurMaskId>) -> Option<SpurReason> {
        Some(match kind {
            "ref-harmonic" => SpurReason::RefHarmonic,
            "dc" => SpurReason::Dc,
            "lo-relative" => SpurReason::LoRelative,
            "comb" => SpurReason::Comb,
            "clock-harmonic" => SpurReason::ClockHarmonic,
            "spur-map" => SpurReason::SpurMap { mask: mask? },
            _ => return None,
        })
    }

    /// The SpurMask version, for `spur-map`.
    pub const fn mask(&self) -> Option<SpurMaskId> {
        match self {
            SpurReason::SpurMap { mask } => Some(*mask),
            _ => None,
        }
    }
}

/// Suspect flags on a Detection (docs/07 §2.9; extended from spike S4). The booleans are stored
/// as a bitmask ([`Self::bits`]); `spur_reason` is stored beside it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DetectionFlags {
    /// The ADC clipped or the front end was overloaded during the detection. **Required** when
    /// `Detection::clip_count > 0` or the detection's Provenance has `overload = true`; the
    /// repository and schema refuse the detection otherwise.
    pub clipped: bool,
    /// Coincides with a known or suspected internal spur.
    pub spur_candidate: bool,
    /// Why it is a spur candidate, if known. Requires `spur_candidate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spur_reason: Option<SpurReason>,
    /// Could be the IQ image of a stronger signal (mirror about the tuned centre).
    pub image_candidate: bool,
    /// The retune test confirmed the image: the candidate moved opposite to the LO when the
    /// front end was retuned. Requires `image_candidate`.
    pub image_retune_confirmed: bool,
    /// SNR close to the detection threshold.
    pub marginal: bool,
    /// Likely an intermodulation product of strong signals (frequency relationship to strong
    /// carriers, and/or the gain-step test changed its level faster than 1 dB per dB).
    pub suspect_imd: bool,
    /// The front end was in gain compression (non-linear, not yet clipping) during the detection,
    /// so levels and the noise floor are unreliable.
    pub compressed: bool,
    /// Impulsive/broadband transient energy (high spectral kurtosis, very short), e.g. ignition,
    /// switching or lightning noise, rather than a structured emission.
    pub impulsive: bool,
    /// Touches the edge of the analysed span (dwell-window or sweep-slice edge, filter roll-off),
    /// so its bandwidth and level may be truncated.
    pub edge: bool,
}

impl DetectionFlags {
    const CLIPPED: u32 = 1 << 0;
    const SPUR_CANDIDATE: u32 = 1 << 1;
    const IMAGE_CANDIDATE: u32 = 1 << 2;
    const MARGINAL: u32 = 1 << 3;
    const SUSPECT_IMD: u32 = 1 << 4;
    const COMPRESSED: u32 = 1 << 5;
    const IMPULSIVE: u32 = 1 << 6;
    const EDGE: u32 = 1 << 7;
    const IMAGE_RETUNE_CONFIRMED: u32 = 1 << 8;

    /// Bitmask of the boolean flags, as stored. `spur_reason` is not part of it.
    pub const fn bits(self) -> u32 {
        (self.clipped as u32 * Self::CLIPPED)
            | (self.spur_candidate as u32 * Self::SPUR_CANDIDATE)
            | (self.image_candidate as u32 * Self::IMAGE_CANDIDATE)
            | (self.marginal as u32 * Self::MARGINAL)
            | (self.suspect_imd as u32 * Self::SUSPECT_IMD)
            | (self.compressed as u32 * Self::COMPRESSED)
            | (self.impulsive as u32 * Self::IMPULSIVE)
            | (self.edge as u32 * Self::EDGE)
            | (self.image_retune_confirmed as u32 * Self::IMAGE_RETUNE_CONFIRMED)
    }

    /// Parses the bitmask. Unknown bits are ignored; `spur_reason` comes back `None`.
    pub const fn from_bits(bits: u32) -> Self {
        Self {
            clipped: bits & Self::CLIPPED != 0,
            spur_candidate: bits & Self::SPUR_CANDIDATE != 0,
            spur_reason: None,
            image_candidate: bits & Self::IMAGE_CANDIDATE != 0,
            image_retune_confirmed: bits & Self::IMAGE_RETUNE_CONFIRMED != 0,
            marginal: bits & Self::MARGINAL != 0,
            suspect_imd: bits & Self::SUSPECT_IMD != 0,
            compressed: bits & Self::COMPRESSED != 0,
            impulsive: bits & Self::IMPULSIVE != 0,
            edge: bits & Self::EDGE != 0,
        }
    }

    /// Any flag set: the detection should not be trusted as a real emission without checks.
    pub const fn any(self) -> bool {
        self.bits() != 0
    }

    /// Checks the dependent fields: a spur reason needs `spur_candidate`, a retune confirmation
    /// needs `image_candidate`. Returns the rule broken, if any.
    pub const fn inconsistency(self) -> Option<&'static str> {
        if self.spur_reason.is_some() && !self.spur_candidate {
            Some("spur_reason requires spur_candidate")
        } else if self.image_retune_confirmed && !self.image_candidate {
            Some("image_retune_confirmed requires image_candidate")
        } else {
            None
        }
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
    /// Absolute peak level, dBFS. Kept alongside SNR so a detection is interpretable without the
    /// noise-floor estimate that produced the SNR (and so near-full-scale peaks are visible).
    pub peak_level_dbfs: f32,
    /// Absolute peak level, dBm at the antenna port, when a CalibrationState power table applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_level_dbm: Option<f32>,
    /// Spectral kurtosis over the detection, if computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sk: Option<f64>,
    /// Clipped ADC samples within the detection's span. A per-span count, so it lives here and not
    /// in the deduplicated Provenance. Non-zero requires `flags.clipped`.
    pub clip_count: u32,
    /// Detector and threshold configuration that produced it, e.g.
    /// `hk-detect/cfar@0.1.0;pfa=1e-6`. Lets history be filtered or re-thresholded by detector.
    pub detector_version: String,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_boolean_flag_has_its_own_bit() {
        let singles = [
            DetectionFlags {
                clipped: true,
                ..Default::default()
            },
            DetectionFlags {
                spur_candidate: true,
                ..Default::default()
            },
            DetectionFlags {
                image_candidate: true,
                ..Default::default()
            },
            DetectionFlags {
                image_retune_confirmed: true,
                ..Default::default()
            },
            DetectionFlags {
                marginal: true,
                ..Default::default()
            },
            DetectionFlags {
                suspect_imd: true,
                ..Default::default()
            },
            DetectionFlags {
                compressed: true,
                ..Default::default()
            },
            DetectionFlags {
                impulsive: true,
                ..Default::default()
            },
            DetectionFlags {
                edge: true,
                ..Default::default()
            },
        ];
        let mut seen = 0u32;
        for f in singles {
            let b = f.bits();
            assert_eq!(b.count_ones(), 1, "{f:?}");
            assert_eq!(seen & b, 0, "bit reused by {f:?}");
            seen |= b;
            assert_eq!(DetectionFlags::from_bits(b), f);
        }
        assert_eq!(DetectionFlags::from_bits(seen).bits(), seen);
    }

    #[test]
    fn spur_reasons_round_trip_through_storage_parts() {
        let mask = SpurMaskId::new();
        for r in [
            SpurReason::RefHarmonic,
            SpurReason::Dc,
            SpurReason::LoRelative,
            SpurReason::Comb,
            SpurReason::ClockHarmonic,
            SpurReason::SpurMap { mask },
        ] {
            assert_eq!(SpurReason::from_parts(r.kind_str(), r.mask()), Some(r));
            let json = serde_json::to_value(r).unwrap();
            assert_eq!(json["kind"], r.kind_str());
        }
        assert_eq!(SpurReason::from_parts("spur-map", None), None);
        assert_eq!(SpurReason::from_parts("bogus", None), None);
        let bad = DetectionFlags {
            spur_reason: Some(SpurReason::Comb),
            ..Default::default()
        };
        assert!(bad.inconsistency().is_some());
    }
}
