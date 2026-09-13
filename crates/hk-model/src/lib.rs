//! hackriff data model ([docs/07](../../../docs/07-data-model.md)): the domain objects every
//! capability reads and writes, and (from T-002) the SQLite repository that stores them.
//! It carries Provenance for C01 and every frame, detection and recording, the Recording/SigMF
//! format for C25, and the object types for C02–C30.
//!
//! **Seeded by T-001, extended by T-002.** The seed is the small shared base that T-002 (data
//! model + SQLite), T-003 (replay source + ring buffer) and T-023 (synthetic generator + replay
//! harness) build on in parallel:
//!
//! - [`ids`]: UUIDv7 id newtypes, one per docs/07 object that has an id.
//! - [`time`]: [`Timestamp`], [`SampleTime`] and [`TimestampMethod`].
//! - [`provenance`]: the [`Provenance`] trust record (docs/07 §2.6).
//! - [`sigmf`]: SigMF `.sigmf-meta` types with the `hackriff:` extension namespace
//!   (docs/sigmf-extension.md).
//!
//! Field lists follow docs/07 and are provisional; T-002 pins the schema.

pub mod ids;
pub mod provenance;
pub mod sigmf;
pub mod time;

pub use ids::{
    AnnotationId, AnomalyId, BitstreamId, CalibrationStateId, DecodeId, DemodulationId,
    DetectionId, EmitterId, ExplanationId, ProvenanceId, RecordingId, ScanPlanId, SpurMaskId,
    SurveyId, TrackId,
};
pub use provenance::{ClockSource, Provenance, Tune};
pub use time::{SampleTime, Timestamp, TimestampMethod};
