//! ExternalEvent (docs/07 §2.17), Anomaly (§2.18) and Explanation (§2.19): the attack map.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::hash::ContentHash;
use crate::ids::{AnomalyId, DetectionId, EmitterId, ExplanationId, ExternalEventId, RecordingId};
use crate::region::{Region, TimeRange};
use crate::time::Timestamp;

/// Where an external event applies.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Geo {
    /// Planet-wide or sunlit-hemisphere effects (flares, Kp).
    Global,
    /// A point, e.g. a lightning strike or launch site.
    Point {
        /// Latitude, degrees.
        lat_deg: f64,
        /// Longitude, degrees.
        lon_deg: f64,
    },
    /// A circle, e.g. a jamming cell approximation.
    Circle {
        /// Centre latitude, degrees.
        lat_deg: f64,
        /// Centre longitude, degrees.
        lon_deg: f64,
        /// Radius, km.
        radius_km: f64,
    },
    /// A latitude/longitude box, e.g. a gpsjam hex cell's bounds.
    BoundingBox {
        /// South edge, degrees.
        south_deg: f64,
        /// West edge, degrees.
        west_deg: f64,
        /// North edge, degrees.
        north_deg: f64,
        /// East edge, degrees.
        east_deg: f64,
    },
    /// A satellite pass over the device, computed locally from cached TLEs (C29).
    OrbitPass {
        /// NORAD catalogue number.
        norad_id: u32,
        /// Maximum elevation, degrees.
        max_elevation_deg: f64,
    },
}

/// A cached fact from a context feed (docs/07 §2.17).
///
/// Identity is the natural key `(source, native_id)`; `id` is a local UUID for references from
/// Explanations. Upserting the same natural key keeps `id` and refreshes the payload: a feed can
/// revise a fact (SWPC updates flare classes), and the cache holds the latest revision. The row
/// also stores [`ExternalEvent::payload_hash`], and Explanation evidence pins the hash it used,
/// so a revision after the fact is detectable.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExternalEvent {
    /// Local id, stable across refreshes of the same natural key.
    pub id: ExternalEventId,
    /// Feed, e.g. `swpc-scales`, `goes-xray`, `kp`, `lightning`, `tle-pass`,
    /// `sondehub-launch`, `gpsjam`, `pskreporter`, `fmlist`.
    pub source: String,
    /// The feed's own id for the fact (or a deterministic key derived from it).
    pub native_id: String,
    /// Event type within the source, e.g. `xray-flare`, `jamming-cell`, `pass`.
    pub event_type: String,
    /// When it applies; `start == end` for an instant.
    pub time: TimeRange,
    /// Where it applies.
    pub geo: Geo,
    /// Feed payload as fetched.
    pub payload: Value,
    /// When it was fetched.
    pub fetched_at: Timestamp,
    /// Cache validity end, if the source defines one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_until: Option<Timestamp>,
}

impl ExternalEvent {
    /// SHA-256 of the canonical payload JSON (key order and `-0.0` do not matter).
    pub fn payload_hash(&self) -> Result<ContentHash, serde_json::Error> {
        ContentHash::of(&self.payload)
    }
}

/// Anomaly kinds (docs/07 §2.18).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AnomalyKind {
    /// An emitter not in the inventory appeared.
    NewEmitter,
    /// Occupancy above the learned baseline.
    BusierThanBaseline,
    /// Noise floor rose (jamming, space weather, local interference).
    NoiseFloorRise,
    /// Something unlike anything seen before (open-set novelty).
    Novelty,
}

/// What an anomaly is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind", content = "id")]
pub enum AnomalySubject {
    /// A detection.
    Detection(DetectionId),
    /// An emitter.
    Emitter(EmitterId),
    /// The region itself (e.g. a band-wide floor rise).
    Region,
}

/// A local deviation from baseline (docs/07 §2.18). The row is append-only; status changes are
/// appended to its status history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Anomaly {
    /// Id.
    pub id: AnomalyId,
    /// Kind.
    pub kind: AnomalyKind,
    /// Subject.
    pub subject: AnomalySubject,
    /// Frequency × time extent.
    pub region: Region,
    /// Deviation score (kind-specific scale; higher is stronger).
    pub score: f64,
    /// Baseline it deviated from (a C12 baseline/tile reference; shape not yet pinned).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_ref: Option<String>,
    /// When it was raised.
    pub t: Timestamp,
    /// Detector id and version.
    pub detector_version: String,
}

/// Anomaly status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AnomalyStatus {
    /// Raised, not yet acted on.
    Open,
    /// Explained or ended.
    Resolved,
    /// Dismissed by the user as uninteresting.
    Dismissed,
}

/// One entry of an anomaly's append-only status history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnomalyStatusChange {
    /// Anomaly.
    pub anomaly_id: AnomalyId,
    /// New status.
    pub status: AnomalyStatus,
    /// When.
    pub t: Timestamp,
    /// Optional note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// How an explanation links anomaly and cause (docs/07 §2.19).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CorrelationType {
    /// Overlap in time within a rule's lag window.
    TimeCoincidence,
    /// Geometric compatibility (distance, elevation, cell containment).
    Geometry,
    /// Signal signature matches the cause.
    Signature,
}

/// The candidate cause of an anomaly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Cause {
    /// A cached external event.
    ExternalEvent {
        /// Event.
        id: ExternalEventId,
    },
    /// An emitter in the inventory.
    Emitter {
        /// Emitter.
        id: EmitterId,
    },
    /// A pattern in the device's own history.
    OwnHistory {
        /// Description.
        description: String,
    },
    /// The device itself: gain/filter change, clipping, retune, calibration update (C30 step 1).
    SelfInflicted {
        /// Which provenance change.
        reason: String,
    },
    /// No hypothesis fits.
    Unexplained,
}

/// A supporting link.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Evidence {
    /// A recording.
    Recording {
        /// Recording.
        id: RecordingId,
    },
    /// A detection.
    Detection {
        /// Detection.
        id: DetectionId,
    },
    /// An external event, pinned to the payload revision the correlation used.
    ExternalEvent {
        /// Event.
        id: ExternalEventId,
        /// [`ExternalEvent::payload_hash`] at correlation time. The repository refuses evidence
        /// whose hash no longer matches the cache; later readers compare it to detect revisions.
        payload_hash: ContentHash,
    },
    /// A history region (tiles).
    History {
        /// Region.
        region: Region,
    },
    /// A numeric fact, e.g. lag or elevation.
    Value {
        /// Name.
        name: String,
        /// Value.
        value: f64,
    },
}

/// A candidate explanation of an anomaly (docs/07 §2.19). Append-only: recomputation writes a
/// new row with `supersedes`; user confirm/reject is an Annotation on it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Explanation {
    /// Id.
    pub id: ExplanationId,
    /// Anomaly explained. Its region is indexed with the explanation.
    pub anomaly_ref: AnomalyId,
    /// Candidate cause.
    pub cause: Cause,
    /// Correlation type.
    pub correlation_type: CorrelationType,
    /// Score, 0–1.
    pub score: f64,
    /// Evidence links.
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    /// Earlier explanation this recomputation replaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<ExplanationId>,
    /// Computed while context feeds were stale (re-run after sync).
    pub provisional: bool,
    /// Rule set id and version.
    pub rule_version: String,
    /// When computed.
    pub t: Timestamp,
}
