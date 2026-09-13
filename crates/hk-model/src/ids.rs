//! Identifier newtypes over UUIDv7 (time-sortable), one per docs/07 object with its own id.
//!
//! Each id serialises as a plain UUID string (`#[serde(transparent)]`). Distinct types stop a
//! `DetectionId` being passed where an `EmitterId` is expected.
//!
//! Objects without a UUID of their own are deliberately absent. SweepFrame and SpectrumFrame are
//! keyed by [`crate::frames::FrameKey`] `(survey_id, seq)` and SpectrumTile by
//! [`crate::frames::TileKey`] `(level, f_block, t_block)`; neither is stored in SQLite.
//! ExternalEvent's identity is its natural key `(source, native_id)`, but it also gets an
//! [`ExternalEventId`] so Explanations can reference it compactly. Provenance rows are
//! deduplicated by content and addressed by [`ProvenanceId`] (see `repo`).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! define_ids {
    ($($(#[$doc:meta])* $name:ident;)+) => {
        $(
            $(#[$doc])*
            #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
            #[serde(transparent)]
            pub struct $name(Uuid);

            #[allow(clippy::new_without_default)]
            impl $name {
                /// Generates a fresh, time-sortable UUIDv7 id.
                pub fn new() -> Self {
                    Self(Uuid::now_v7())
                }

                /// Wraps an existing UUID (e.g. one read back from storage).
                pub const fn from_uuid(uuid: Uuid) -> Self {
                    Self(uuid)
                }

                /// The underlying UUID.
                pub const fn as_uuid(&self) -> &Uuid {
                    &self.0
                }
            }

            impl fmt::Debug for $name {
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    write!(f, concat!(stringify!($name), "({})"), self.0)
                }
            }

            impl fmt::Display for $name {
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    fmt::Display::fmt(&self.0, f)
                }
            }

            impl FromStr for $name {
                type Err = uuid::Error;

                fn from_str(s: &str) -> Result<Self, Self::Err> {
                    Uuid::parse_str(s).map(Self)
                }
            }

            impl From<$name> for Uuid {
                fn from(id: $name) -> Uuid {
                    id.0
                }
            }
        )+
    };
}

define_ids! {
    /// ScanPlan (docs/07 §2.1). The plan version is tracked separately.
    ScanPlanId;
    /// Survey, one scanning run (§2.2).
    SurveyId;
    /// Provenance trust record (§2.6).
    ProvenanceId;
    /// CalibrationState version (§2.7, `cal_id`).
    CalibrationStateId;
    /// SpurMask version (§2.8, `spur_id`).
    SpurMaskId;
    /// Detection (§2.9).
    DetectionId;
    /// Track of linked detections (§2.10).
    TrackId;
    /// Emitter inventory entry (§2.11).
    EmitterId;
    /// Recording, a SigMF dataset (§2.12).
    RecordingId;
    /// Annotation (§2.13).
    AnnotationId;
    /// Demodulation session (§2.14, `demod_id`).
    DemodulationId;
    /// Decode / Message (§2.15).
    DecodeId;
    /// Bitstream (§2.16).
    BitstreamId;
    /// Anomaly (§2.18).
    AnomalyId;
    /// Explanation / Correlation (§2.19).
    ExplanationId;
    /// ExternalEvent (§2.17): a local id for references from Explanations. The event's identity
    /// is its natural key `(source, native_id)`; upserting that key keeps this id stable.
    ExternalEventId;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_ids_are_uuid_v7() {
        let id = DetectionId::new();
        assert_eq!(id.as_uuid().get_version_num(), 7);
    }

    #[test]
    fn serde_is_transparent_and_display_parses_back() {
        let id = SurveyId::new();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<SurveyId>(&json).unwrap(), id);
        assert_eq!(id.to_string().parse::<SurveyId>().unwrap(), id);
        assert!(format!("{id:?}").starts_with("SurveyId("));
    }
}
