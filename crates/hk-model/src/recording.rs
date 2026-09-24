//! Recording (docs/07 §2.12) and Annotation (§2.13).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::content::ContentClass;
use crate::ids::{
    AnnotationId, AnomalyId, DemodulationId, DetectionId, EmitterId, ExplanationId, ProvenanceId,
    RecordingId,
};
use crate::region::{Region, TimeRange};
use crate::time::Timestamp;

/// What a Recording holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecordingKind {
    /// Raw IQ around a detection, at the dwell rate.
    IqSnippet,
    /// A channelised, decimated IQ stream.
    ChannelDecimated,
    /// Demodulated audio.
    Audio,
}

/// Why a Recording exists. Children point at their origin, never the reverse, so origin rows
/// never need updating.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind", content = "id")]
pub enum RecordingTrigger {
    /// Triggered by a detection (pre-trigger from the ring buffer, C03).
    Detection(DetectionId),
    /// Output of a demodulation session (audio, symbols).
    Demodulation(DemodulationId),
    /// Scheduled by a ScanPlan.
    Scheduler,
    /// Requested by the user.
    Manual,
    /// Pinned by a region analysis at job start (ADR-0015 §6 "pin on analyze", T-857): the ring
    /// windows the job acquired, exported before the search so ring eviction cannot race it.
    Analyze,
}

/// Eviction ranking class (C25: pinned > unknown > decoder-confirmed > routine).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RetentionClass {
    /// Never evicted automatically.
    Pinned,
    /// Unknown or unexplained: kept ahead of known traffic.
    Unknown,
    /// Decoder-confirmed known signal.
    DecoderConfirmed,
    /// Routine; evicted first.
    Routine,
}

/// A SigMF dataset reference (docs/07 §2.12). **Immutable** measurement row; the samples live
/// on disk, not in the database.
///
/// Annotations on a recording are Annotation rows whose target is this recording (mirrored into
/// the SigMF meta by C25), not an embedded list, so labelling never rewrites the recording row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Recording {
    /// Id.
    pub id: RecordingId,
    /// `.sigmf-meta` path, relative to the device data directory.
    pub meta_uri: String,
    /// `.sigmf-data` path, relative to the device data directory.
    pub data_uri: String,
    /// What it holds.
    pub kind: RecordingKind,
    /// Time span of the samples.
    pub time: TimeRange,
    /// Centre frequency, Hz.
    pub f_center_hz: f64,
    /// Sample rate, Hz.
    pub sample_rate_hz: f64,
    /// Origin.
    pub trigger: RecordingTrigger,
    /// Pre-trigger span included, s.
    pub pre_trigger_s: f64,
    /// Post-trigger hold included, s.
    pub post_trigger_s: f64,
    /// Size of the data file, bytes.
    pub size_bytes: u64,
    /// Eviction class.
    pub retention_class: RetentionClass,
    /// Content gating class (ADR-0004). IQ and audio are content, so the repository refuses a
    /// Recording whose class does not permit content (`RepoError::GatedContent`); C25 must not
    /// write the files in the first place.
    pub content_class: ContentClass,
    /// Trust record.
    pub provenance_ref: ProvenanceId,
}

/// Who wrote an annotation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AnnotationAuthor {
    /// A person, through the UI.
    User,
    /// A decoder (C21/C22), e.g. from a CRC-valid frame.
    Decoder,
    /// A classifier (C15).
    Classifier,
}

/// What kind of label it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AnnotationKind {
    /// A plain label.
    Label,
    /// Corrects an earlier annotation (see `supersedes`).
    Correction,
    /// Ground truth, e.g. from a CRC-valid decode; usable for fine-tuning (C38) and tests.
    GroundTruth,
}

/// A sample range and frequency box within a recording. Time and frequency are stored as well
/// as sample indices because indices break after decimation (C25 pitfall).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordingSpan {
    /// Recording.
    pub recording_id: RecordingId,
    /// First sample, if the label covers part of the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_start: Option<u64>,
    /// Sample count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_count: Option<u64>,
    /// Time/frequency box, if the label covers part of the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<Region>,
}

/// What an annotation labels.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind", content = "ref")]
pub enum AnnotationTarget {
    /// A Detection.
    Detection(DetectionId),
    /// An Emitter.
    Emitter(EmitterId),
    /// A Recording or part of one.
    Recording(RecordingSpan),
    /// An Anomaly.
    Anomaly(AnomalyId),
    /// An Explanation (user confirm/reject, docs/07 §2.19).
    Explanation(ExplanationId),
    /// A free time-frequency box.
    Region(Region),
}

/// A label (docs/07 §2.13). **Append-only**: a change is a new annotation of kind `correction`
/// whose `supersedes` names the old one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Annotation {
    /// Id.
    pub id: AnnotationId,
    /// What it labels.
    pub target: AnnotationTarget,
    /// Who wrote it.
    pub author: AnnotationAuthor,
    /// Author detail: user name, decoder id@version, or model version.
    pub author_ref: String,
    /// Kind.
    pub kind: AnnotationKind,
    /// Label path, e.g. `fm/rds`, `adsb`, `unknown/2fsk`. A label is **metadata**: decoded message
    /// text never goes here, it goes in `content`.
    pub value: String,
    /// Metadata detail: evidence such as CRC passes and frame count, identifiers. Always stored.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub metadata: Value,
    /// Content detail: decoded message text or payload used as ground truth. The repository
    /// refuses `Some` unless `content_class` permits content (`RepoError::GatedContent`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    /// Confidence, 0–1.
    pub confidence: f64,
    /// Earlier annotation this one corrects or replaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<AnnotationId>,
    /// Content class of `content`, chosen explicitly (fail closed when unknown).
    pub content_class: ContentClass,
    /// When it was written.
    pub t: Timestamp,
    /// Included in a labelled SigMF export. Export bookkeeping, not label content; the only
    /// field the repository lets change after insert.
    pub exported: bool,
}
