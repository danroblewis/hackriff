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
pub mod harmonic; // T-374 (C40): harmonics of a fundamental nobody can see
#[cfg(test)]
mod harmonic_tests;
pub mod hash;
pub mod ids;
pub mod multipath; // T-222 (C40): content-correlated multipath
pub mod path; // T-897 (docs/23 §10.6 rule 2): traced (t, f) paths derived from detections
pub mod plan;
pub mod presence; // T-262 (ADR-0017 TM-5): presence intervals, close and revive
pub mod provenance;
pub mod recording;
pub mod region;
pub mod relate; // T-219 (C40)
pub mod repo;
pub mod retune; // T-586 (AWARE-011): retune diversity, absolute vs LO-relative
pub mod sigmf;
pub mod signature; // T-218 (ADR-0016 §5)
pub mod synth; // T-848 (ADR-0015 §2.1, MAUTO M-1): the evidence vocabulary hk-blocks and hk-synth share
pub mod time;
pub mod trunking; // T-266 (C23 trunking metadata; metadata only, no call audio)

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
    Bitstream, BitstreamPayload, BitstreamTransport, CrcStatus, DcsCode, Decode, DecodeEvidence,
    DecodeProvenance, DecodeView, Demodulation, EstimatedParams, Framing, Subaudible,
    SubaudibleKind, SubaudibleTone, WITHHELD_LABEL,
};
pub use detection::{
    BurstLengths, Detection, DetectionFlags, MAX_TRACK_PAGE, PageRequest, SegmentKind,
    TimingFeatures, Track, TrackFilter, TrackKind, TrackPage, TrackSegment, TrackState,
};
pub use emitter::{
    Appearance, Classification, DecodedIdentity, Emitter, EmitterLink, EmitterObservation,
    FRAMING_IDENTITY_SCHEME, Identity, IdentityScheme, KnownStatus, KnownStatusChange,
    LifecycleAuthor, LifecycleChange, LifecycleState, LinkTarget, RDS_PI_COMMIT_VOTES,
    RDS_PI_COMMIT_WINDOW_NS, Recurrence, StatusAuthor, VoteWindow,
};
pub use frames::{
    FrameKey, Persistence, PowerUnit, SpectrumFrame, SpectrumTile, SweepFrame, TileKey, TileStats,
};
pub use harmonic::{
    FamilyAssignment, FamilyMember, FamilyRejection, HarmonicFamily, LineFit, MAX_INDEX,
    MIN_MEMBERS, WidthEvidence, find_harmonic_families, fit_line, judge_family,
    residual_tolerance_hz, width_evidence,
};
pub use hash::{ContentHash, canonical_json};
pub use ids::{
    AnnotationId, AnomalyId, BitstreamId, BookmarkId, CalibrationStateId, CallRecordId,
    CollectionId, DecodeId, DemodulationId, DetectionId, EmitterId, ExplanationId, ExternalEventId,
    MarkerId, MeasurementId, ProvenanceId, RecordingId, SavedViewId, ScanPlanId, SelectionId,
    SpurMaskId, SurveyId, TrackId, TrunkSystemId,
};
pub use multipath::{
    ContentCorrelation, ContentKind, IdentityAgreement, MultipathFinding, MultipathRow,
    MultipathVerdict, content_multipath,
};
pub use path::{
    PATH_METHOD, PathConfig, PathKind, PathProvenance, PathVertex, TracedPath, VertexAt,
    derive_paths,
};
pub use plan::{
    GainTableEntry, PlanRegion, ScanPlan, ScanPolicy, Schedule, Survey, SurveyState, SurveySummary,
};
pub use presence::{
    IdleGap, Liveness, MAX_IDLE_GAP_S, MIN_IDLE_GAP_S, NEGLIGIBLE_CONFIDENCE, ObservationSpan,
    Presence, PresenceInterval, REVISIT_FACTOR, Watched, confidence_after_silence,
    intervals_from_spans, intervals_observed, presence_in_window, recheck_horizon_s,
};
pub use provenance::{
    BiasTee, CaptureArtefact, ClockSource, CyclicComb, FillBucket, OVER_CLIP_FRACTION, Provenance,
    Tune, UNDER_FILL_SIGMA_LSB,
};
pub use recording::{
    Annotation, AnnotationAuthor, AnnotationKind, AnnotationTarget, Recording, RecordingKind,
    RecordingSpan, RecordingTrigger, RetentionClass,
};
pub use region::{FreqRange, Region, TimeRange};
pub use relate::{
    ArtifactKind, ArtifactPrediction, ArtifactSource, EmitterRelation, MeasuredEmission,
    OVERLAP_MIN_FRACTION, REGION_IDENTITY, REGION_MERGE_UNCOVERED, REGION_MIN_BINS,
    REGION_NO_EMISSION, REGION_OFF_CENTRE, REGION_ROW_UNEXPLAINED, REGION_TOO_COARSE, ReceiveChain,
    RegionMeasurement, RegionVerdict, RelationAuthor, RelationClaim, RelationKind,
    RelationVisibility, RowEvidence, TunedLo, distinct_chains, distinguishing_evidence,
    overlap_fraction, predict_artifacts, present_only_with, rank_score, region_verdicts,
};
pub use repo::{
    AUTHORED_BODY_MAX, AUTHORED_LABEL_MAX, AUTHORED_PAGE_MAX, AUTHORED_REF_MAX, AuthoredAnnotation,
    AuthoredKind, AuthoredPage, AuthoredProvenance, BOOKMARK_NAME_MAX, BOOKMARK_NOTE_MAX,
    BOOKMARKS_COLLECTION, BOOKMARKS_COLLECTION_COLOR, BOOKMARKS_COLLECTION_NAME, BOOKMARKS_MAX,
    Bookmark, BookmarkKind, COLLECTION_NAME_MAX, COLLECTION_NOTE_MAX, COLLECTIONS_MAX, Collection,
    CollectionSummary, EmitterSynthesis, EmitterUpsert, HarmonicFamilyRow, LIFECYCLE_TEXT_MAX,
    LatestMeasurement, MARKERS_PER_COLLECTION_MAX, MAX_ARTEFACT_DETECTIONS, MAX_FAMILY_CANDIDATES,
    MAX_LO_SPAN_HZ, MAX_RETUNE_DETECTIONS, MAX_RETUNE_ROWS, Marker, MarkerWindow,
    PROVENANCE_TEXT_MAX, ProvenanceChain, REFINED_BY_OUTPUT_ANALYSIS, REFINED_HISTORY_MAX,
    RETUNE_RULE, ReceiverArtefactShare, RefinedTuning, RepoBatch, RepoError, Repository,
    RetuneFamily, RetuneOutcome, RetuneVerdict, SELECTION_LINK_REF_MAX, SELECTION_LINKS_MAX,
    SELECTION_NAME_MAX, SELECTION_NOTES_MAX, SELECTION_TAG_MAX, SELECTION_TAGS_MAX, SELECTIONS_MAX,
    SYNTHESIZED_BY_OUTPUT_ANALYSIS, Selection, SelectionLink, SelectionLinkKind, SelectionWatch,
    StorePage, TrustTest, TrustVerdict, USER_BAND_MAX_GAP_HZ, USER_BAND_MAX_WIDTH_HZ,
    UnresolvedRegion, UserBand, ViewTier, authored_block,
};
// T-904 per-frame detection retention and rollup (docs/07 §2.9).
pub use repo::{
    DetectionRetention, DetectionRollup, DetectionStorage, KEEP_PER_EMITTER, PruneReport,
};
// T-818 MAP-18 saved measurements (docs/25 §4).
pub use repo::{
    MEASUREMENT_N_MAX, MEASUREMENT_NOTE_MAX, MEASUREMENT_PAGE_MAX, MEASUREMENT_REF_MAX,
    Measurement, MeasurementBasis, MeasurementComputed, MeasurementCursor, MeasurementFilter,
    MeasurementKind, MeasurementPage, MeasurementProvenance, MeasurementTier, compute_measurement,
};
// T-819 MAP-19 saved views (docs/25 §6).
pub use repo::{
    SAVED_VIEW_LAYOUT_MAX, SAVED_VIEW_NAME_MAX, SAVED_VIEW_NOTE_MAX, SAVED_VIEW_PAGE_MAX,
    SavedView, SavedViewFilter, SavedViewPage,
};
pub use retune::{
    RETUNE_MIN_CENTRES, RETUNE_MIN_TOLERANCE_HZ, RETUNE_TOLERANCE_BW_FRACTION, RetuneGroup,
    RetuneObservation, RetuneSlope, RetuneSummary, RetuneTolerance, SLOPES, classify,
};
pub use signature::cluster::{
    CLUSTER_FIELDS, CLUSTER_LABEL_LEN, CLUSTER_MIN_APPEARANCES, CLUSTER_MIN_MEMBERS,
    ClusterCentroid, ClusterEvent, ClusterEventKind, ClusterState, EmitterClusterLink,
    SignatureCluster, cluster_label, is_cluster_field, is_cluster_id, new_cluster_id,
};
pub use signature::{
    EMISSION_FEATURES_VERSION, EmissionFeatures, Feat, FeatValue, FieldAgreement, FieldExpect,
    FieldSpec, InvalidSignature, MatchOutcome, SIGNATURE_SCHEMA, Signature, SignatureCandidate,
    SignatureKind, SignatureMatch, SignatureProvenance, SignatureRef, fold_field,
};
pub use time::{SampleTime, Timestamp, TimestampMethod};
pub use trunking::{
    CALL_REASONS_MAX, CallEnding, CallRecord, ChannelPlanEntry, Encryption, EncryptionEvidence,
    GrantEvent, GrantKind, InvalidTrunking, LabelSource, MAX_SLOT, MAX_TDMA_SLOTS, NeighbourSite,
    P25_ALGID_CLEAR, TRUNK_LABEL_MAX, TRUNK_TEXT_MAX, Talkgroup, TrunkProtocol, TrunkSystem,
};
