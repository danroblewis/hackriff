//! hackriff data model ([docs/07](../../../docs/07-data-model.md)): the domain objects every
//! capability reads and writes, and the SQLite repository that stores them.
//! It carries Provenance for C01 and every frame, detection and recording, the Recording/SigMF
//! format for C25, and the object types for C02–C30.
//!
//! Seeded by T-001 (ids, time, provenance, sigmf); T-002 added the remaining docs/07 objects and
//! the repository.
//!
//! - [`ids`]: UUIDv7 id newtypes, one per docs/07 object that has an id.
//! - [`time`]: [`Timestamp`], [`SampleTime`] and [`TimestampMethod`].
//! - [`region`]: closed frequency/time extents, the two indexed axes.
//! - [`provenance`]: the [`Provenance`] trust record (§2.6).
//! - [`calibration`]: [`CalibrationState`] and [`SpurMask`] versions (§2.7–2.8).
//! - [`plan`]: [`ScanPlan`] and [`Survey`] (§2.1–2.2).
//! - [`frames`]: [`SweepFrame`], [`SpectrumFrame`], SpectrumTile key and stats (§2.3–2.5).
//! - [`detection`]: [`Detection`] and [`Track`] (§2.9–2.10).
//! - [`emitter`]: the [`Emitter`] inventory entry (§2.11).
//! - [`cluster`]: emitter [`Fingerprint`]s, entity resolution rules and gated inventory queries
//!   (C18/C27, T-018).
//! - [`recording`]: [`Recording`] and [`Annotation`] (§2.12–2.13).
//! - [`decode`]: [`Demodulation`], [`Decode`], [`Bitstream`] (§2.14–2.16).
//! - [`context`]: [`ExternalEvent`], [`Anomaly`], [`Explanation`] (§2.17–2.19).
//! - [`content`]: [`ContentClass`] for ADR-0004 content gating (off by default; opt-in).
//! - [`hash`]: canonical JSON and [`ContentHash`] (provenance dedup, evidence pinning).
//! - [`repo`]: the SQLite [`Repository`] (ADR-0006).
//! - [`sigmf`]: SigMF `.sigmf-meta` types with the `hackriff:` extension namespace
//!   (docs/sigmf-extension.md).
//! - [`attention`]: M2 attention + memory contracts (ADR-0012): observation log, occupancy,
//!   baselines, interestingness, bandit/POI, survey reports, novelty alarms. Not re-exported at
//!   the crate root; use `hk_model::attention::…`.
//!
//! # Measurement vs interpretation (docs/07 intro; planning-log P2.1)
//!
//! Every persisted object falls in one of three categories, and the repository API and schema
//! triggers enforce it:
//!
//! | Category | Objects | Allowed writes |
//! |---|---|---|
//! | **Measurement** (immutable) | Provenance, Detection, Recording, CalibrationState, SpurMask, ScanPlan versions (and frames, not stored) | insert only; `UPDATE` aborts |
//! | **Interpretation** (append-only, versioned) | Classification, known-status history, Demodulation, Decode, Bitstream, Annotation, Anomaly (+ status history), Explanation, emitter and track links | insert only; a new version is a new row (`supersedes` / version strings) |
//! | **Aggregate** (mutable summary) | Survey lifecycle, Track, Emitter counters/identity/tags, ExternalEvent cache | upsert through named repository calls |
//!
//! Aggregates are summaries that can be rebuilt from the measurements and interpretations.

pub mod attention;
pub mod calibration;
pub mod classify; // T-211 (ADR-0016)
pub mod cluster;
pub mod content;
pub mod context;
pub mod decode;
pub mod detection;
pub mod emitter;
pub mod frames;
pub mod hash;
pub mod ids;
pub mod plan;
pub mod provenance;
pub mod recording;
pub mod region;
pub mod repo;
pub mod sigmf;
pub mod time;

pub use calibration::{
    CalibrationMethod, CalibrationState, GainSetting, PowerCalPoint, SpurMask, SpurRule,
};
pub use cluster::{
    Assignment, ConflictReason, EmitterMerge, FEATURE_SET_VERSION, FeatureMatch, Fingerprint,
    IdentityAccess, IdentityClaim, IdentityConflictReport, IdentityReclassification,
    InventoryEntry, InventoryIdentity, InventoryPage, InventoryQuery, KnownStatusPrior, LinkRecord,
    MeasurementKey, PriorVerdict, RecordedClassification, Resolution, Sighting, TAG_VOCABULARY,
    Tolerances, never_openable, tag_in_vocabulary, tag_is_identity_free,
};
pub use content::{ContentClass, content_gating_enabled, set_content_gating};
pub use context::{
    Anomaly, AnomalyKind, AnomalyStatus, AnomalyStatusChange, AnomalySubject, Cause,
    CorrelationType, Evidence, Explanation, ExternalEvent, Geo,
};
pub use decode::{
    Bitstream, BitstreamPayload, BitstreamTransport, CrcStatus, Decode, DecodeView, Demodulation,
    EstimatedParams, Framing, WITHHELD_LABEL,
};
pub use detection::{
    BurstLengths, Detection, DetectionFlags, MAX_TRACK_PAGE, PageRequest, SegmentKind,
    TimingFeatures, Track, TrackFilter, TrackKind, TrackPage, TrackSegment, TrackState,
};
pub use emitter::{
    Appearance, Classification, DecodedIdentity, Emitter, EmitterLink, EmitterObservation,
    Identity, IdentityScheme, KnownStatus, KnownStatusChange, LifecycleAuthor, LifecycleChange,
    LifecycleState, LinkTarget, Recurrence, StatusAuthor,
};
pub use frames::{
    FrameKey, Persistence, PowerUnit, SpectrumFrame, SpectrumTile, SweepFrame, TileKey, TileStats,
};
pub use hash::{ContentHash, canonical_json};
pub use ids::{
    AnnotationId, AnomalyId, BitstreamId, BookmarkId, CalibrationStateId, DecodeId, DemodulationId,
    DetectionId, EmitterId, ExplanationId, ExternalEventId, ProvenanceId, RecordingId, ScanPlanId,
    SelectionId, SpurMaskId, SurveyId, TrackId,
};
pub use plan::{
    GainTableEntry, PlanRegion, ScanPlan, ScanPolicy, Schedule, Survey, SurveyState, SurveySummary,
};
pub use provenance::{ClockSource, Provenance, Tune};
pub use recording::{
    Annotation, AnnotationAuthor, AnnotationKind, AnnotationTarget, Recording, RecordingKind,
    RecordingSpan, RecordingTrigger, RetentionClass,
};
pub use region::{FreqRange, Region, TimeRange};
pub use repo::{
    BOOKMARK_NAME_MAX, BOOKMARK_NOTE_MAX, BOOKMARKS_MAX, Bookmark, BookmarkKind, EmitterUpsert,
    LIFECYCLE_TEXT_MAX, ProvenanceChain, REFINED_BY_OUTPUT_ANALYSIS, REFINED_HISTORY_MAX,
    RefinedTuning, RepoBatch, RepoError, Repository, SELECTION_LINK_REF_MAX, SELECTION_LINKS_MAX,
    SELECTION_NAME_MAX, SELECTION_NOTES_MAX, SELECTION_TAG_MAX, SELECTION_TAGS_MAX, SELECTIONS_MAX,
    Selection, SelectionLink, SelectionLinkKind, TrustTest, TrustVerdict, USER_BAND_MAX_GAP_HZ,
    USER_BAND_MAX_WIDTH_HZ, UserBand,
};
pub use time::{SampleTime, Timestamp, TimestampMethod};
