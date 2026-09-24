//! Labelled-capture dataset export (T-205, ADR-0016 §7): normalised IQ snippets plus labels and
//! provenance, for later model fine-tuning (C38) and blind evaluation (`EvalReport`, T-213).
//!
//! **Label sources**, both marked in provenance ([`LabelSourceKind`]):
//! - **decoder-validated**: a decode whose [`CrcStatus::Valid`] passed (never
//!   [`CrcStatus::Corrected`], once T-210 lands: [`find_labelled_emissions`] matches `Valid`
//!   explicitly, not `!= Invalid`). The label is the demodulation's `mode`, mapped into the
//!   current `hk-mod@1` taxonomy ([`hk_model::classify::family_of`]) — the same mapping ADR-0016
//!   §1 gives pre-M3 labels.
//! - **user**: an emitter's latest classification at [`Stage::User`] (an explicit reclassify),
//!   carrying its own `hk-mod@1` family/class and confidence.
//!
//! **Format.** Each sample is an existing-format SigMF `Recording` (IQ snippet) plus a
//! `GroundTruth`/`Label` [`Annotation`] on it (decoder-sourced rows use `GroundTruth`, user rows
//! use `Label`; ADR-0016 §9's `docs/07 §2.13` convention), carrying the `hk-mod@1` label, split
//! and provenance in `metadata`, marked `exported: true` at creation. The IQ itself is written by
//! whatever already writes SigMF clips in the running system (the IQ capture buffer's clip
//! exporter, `hk_pipeline::iqbuffer::IqBufferService::export_clip`, which already produces a
//! `Recording` row from `hk_store::iqbuffer`) — this module never re-implements SigMF I/O, it
//! only decides *what* to export and attaches the label, through the [`SnippetExporter`] seam so
//! it stays testable without a live device or ring buffer. A **manifest** (this module's
//! [`DatasetManifest`], JSON) indexes the resulting recordings and stamps one [`Split`] on the
//! whole export, so training data never mixes with the acceptance split (ADR-0016 §7). No
//! database migration is needed: the manifest is a file (like a SigMF `.sigmf-meta`), not a row,
//! and the labelled data itself is ordinary Recording + Annotation rows the repository already
//! has.
//!
//! Not yet covered (left for later dataset-path tasks, e.g. T-213's `EvalReport` loader):
//! synthetic dev/acceptance seed generation (that's `py/hkpy/synth`), and BCH-checked (non-CRC)
//! decoders, which this reads through the same [`CrcStatus`] contract once a decoder reports one.

use hk_model::classify::{Stage, TaxonomyRef, UNKNOWN, family_of};
use hk_model::cluster::MAX_INVENTORY_PAGE;
use hk_model::recording::{Annotation, AnnotationAuthor, AnnotationKind, AnnotationTarget};
use hk_model::region::TimeRange;
use hk_model::time::Timestamp;
use hk_model::{
    AnnotationId, ContentClass, CrcStatus, EmitterId, InventoryQuery, RecordingId, RepoError,
    Repository,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// A candidate filter for a dataset export (ADR-0016 §9 `POST /api/datasets`). All set filters
/// must hold; `emitter` alone selects one emitter regardless of the others.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DatasetFilter {
    /// Only this emitter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emitter: Option<EmitterId>,
    /// Only emitters last seen in this window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<TimeRange>,
    /// Only this `hk-mod@1` family.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
}

/// The evaluation split an export belongs to (ADR-0016 §7): disjoint by construction — one export
/// carries one split, stamped on the manifest and every sample and never inferred from content —
/// so acceptance data can never leak into training.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Split {
    /// Training / model-development data.
    Dev,
    /// Held out for the M3 blind acceptance suite (T-206); never used to fit anything.
    Acceptance,
}

impl Split {
    /// The serde/manifest string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Split::Dev => "dev",
            Split::Acceptance => "acceptance",
        }
    }
}

/// Where a label came from (ADR-0016: "decoder-validated ... and user labels, both marked in
/// provenance").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LabelSourceKind {
    /// A CRC-valid decode named the modulation (via its demodulation's `mode`).
    Decoder,
    /// A user's explicit reclassification ([`Stage::User`]).
    User,
}

/// One `hk-mod@1` label with its source and provenance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DatasetLabel {
    /// Taxonomy the label belongs to (`hk-mod@1`).
    pub taxonomy: TaxonomyRef,
    /// Family (or within-family class) label.
    pub label: String,
    /// Decoder-validated or user.
    pub source: LabelSourceKind,
    /// What produced it: `decode:<id>` for a decoder label, the classification's
    /// `provenance.rules` for a user label.
    pub provenance: String,
    /// Confidence, 0–1 (1.0 for a CRC-valid decode: "the frame is ground truth").
    pub confidence: f64,
}

/// A labelled emission [`find_labelled_emissions`] found, not yet exported: enough to place and
/// caption an IQ snippet.
#[derive(Clone, Debug, PartialEq)]
pub struct LabelledEmission {
    /// The emitter it came from.
    pub emitter_id: EmitterId,
    /// The label.
    pub label: DatasetLabel,
    /// When the label event happened (decode time, or the user classification's time).
    pub t: Timestamp,
    /// The snippet window to export (the label event padded, ADR-0016 §7).
    pub window: TimeRange,
    /// Frequency band to request, the emitter's measured extent.
    pub band: Option<(f64, f64)>,
    /// Measured in-band SNR, if known ([`Repository::emitter_latest_measurement`]).
    pub snr_db: Option<f64>,
    /// Producing session: the demodulation id for a decoder label, `None` for a user label
    /// (ADR-0016 §7: "split by session", never by frame).
    pub session: Option<String>,
}

/// A dataset request (ADR-0016 §9 `POST /api/datasets`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DatasetRequest {
    /// Candidate filter.
    pub filter: DatasetFilter,
    /// Split every sample is stamped with.
    pub split: Split,
    /// Snippet padding before the label event, s.
    #[serde(default = "default_pad")]
    pub pad_pre_s: f64,
    /// Snippet padding after the label event, s.
    #[serde(default = "default_pad")]
    pub pad_post_s: f64,
    /// At most this many samples (a guard rail; also bounds the label search).
    #[serde(default = "default_max_samples")]
    pub max_samples: usize,
}

fn default_pad() -> f64 {
    0.05
}

fn default_max_samples() -> usize {
    200
}

impl Default for DatasetRequest {
    fn default() -> Self {
        Self {
            filter: DatasetFilter::default(),
            split: Split::Dev,
            pad_pre_s: default_pad(),
            pad_post_s: default_pad(),
            max_samples: default_max_samples(),
        }
    }
}

/// One exported sample: the labelled emission plus where its snippet landed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DatasetSample {
    /// Source emitter.
    pub emitter_id: EmitterId,
    /// Source session (see [`LabelledEmission::session`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The exported SigMF recording carrying the IQ.
    pub recording_id: RecordingId,
    /// The `GroundTruth`/`Label` annotation carrying the label and provenance.
    pub annotation_id: AnnotationId,
    /// The label.
    pub label: DatasetLabel,
    /// Measured in-band SNR, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snr_db: Option<f64>,
    /// The snippet's sample rate, Hz.
    pub sample_rate_hz: f64,
    /// The emission's centre offset from the snippet's tuned centre, Hz (0 when the exporter
    /// centred the snippet on the emission).
    pub center_offset_hz: f64,
    /// When the label event happened.
    pub t: Timestamp,
    /// Split this sample belongs to.
    pub split: Split,
}

/// A finished export: the request that produced it plus every sample. Written as JSON (a
/// manifest file, like a SigMF `.sigmf-meta`; no database row).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DatasetManifest {
    /// Export id (also its manifest file's stem).
    pub id: String,
    /// The filter it was built from.
    pub filter: DatasetFilter,
    /// The split every sample is stamped with.
    pub split: Split,
    /// When the export ran.
    pub created_at: Timestamp,
    /// Exported samples.
    pub samples: Vec<DatasetSample>,
    /// Labelled emissions found but not exported (the snippet's IQ was unavailable, e.g. evicted
    /// from the ring buffer): counted, never silently dropped.
    pub skipped: usize,
}

/// A snippet the exporter placed and wrote (IQ already stored as a `Recording`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExportedSnippet {
    /// The stored recording.
    pub recording_id: RecordingId,
    /// Its sample rate, Hz.
    pub sample_rate_hz: f64,
    /// Its tuned centre, Hz.
    pub center_hz: f64,
}

/// Something that can turn a labelled emission's window into a stored SigMF `Recording`: in a
/// running system, the IQ capture buffer's existing clip exporter; in tests, a synthetic double.
/// Errors are per-snippet and never abort the rest of the export ([`export_dataset`] counts them
/// as `skipped`).
pub trait SnippetExporter {
    /// Exports the samples in `window` (and `band`, if given) as a new `Recording`.
    fn export(
        &mut self,
        window: TimeRange,
        band: Option<(f64, f64)>,
    ) -> Result<ExportedSnippet, DatasetError>;
}

/// A dataset export failure.
#[derive(Debug, thiserror::Error)]
pub enum DatasetError {
    /// The request itself is invalid.
    #[error("invalid dataset request: {0}")]
    Invalid(String),
    /// The repository failed.
    #[error("dataset store: {0}")]
    Repo(#[from] RepoError),
    /// The snippet could not be exported (evicted, empty, or a write failure); the caller counts
    /// this as skipped rather than failing the whole export.
    #[error("snippet unavailable: {0}")]
    SnippetUnavailable(String),
}

fn label_matches_filter(label: &str, filter: &DatasetFilter) -> bool {
    filter.family.as_deref().is_none_or(|f| f == label)
}

fn snr_of(repo: &Repository, emitter_id: EmitterId) -> Result<Option<f64>, DatasetError> {
    Ok(repo
        .emitter_latest_measurement(emitter_id)?
        .map(|m| m.snr_peak_db))
}

fn window_of(t: Timestamp, pad_pre_s: f64, pad_post_s: f64) -> TimeRange {
    let ns = |s: f64| (s * 1e9).round() as i64;
    TimeRange::new(
        t.saturating_add_nanos(-ns(pad_pre_s)),
        t.saturating_add_nanos(ns(pad_post_s)),
    )
}

/// Decoder-validated labelled emissions from `emitter_id`'s demodulations: one per CRC-valid
/// decode whose demodulation's `mode` maps into `hk-mod@1` (ADR-0016 §7). **Only
/// [`CrcStatus::Valid`] counts** — matched explicitly, so a future `Corrected` status is never
/// mistaken for evidence.
fn decoder_emissions(
    repo: &Repository,
    emitter_id: EmitterId,
    filter: &DatasetFilter,
    pad_pre_s: f64,
    pad_post_s: f64,
    band: Option<(f64, f64)>,
    snr_db: Option<f64>,
) -> Result<Vec<LabelledEmission>, DatasetError> {
    let taxonomy = TaxonomyRef::current();
    let mut out = Vec::new();
    for evidence in repo.decode_evidence_for_emitter(emitter_id)? {
        if evidence.crc_status != CrcStatus::Valid {
            continue; // never Corrected/Invalid/NoCrc/Unknown (ADR-0016 §7).
        }
        let demod = repo.demodulation(evidence.demodulation_id)?;
        let Some(label) = family_of(&demod.mode, &taxonomy) else {
            continue; // a service label (not a modulation), or outside the taxonomy.
        };
        if !label_matches_filter(label, filter) {
            continue;
        }
        out.push(LabelledEmission {
            emitter_id,
            label: DatasetLabel {
                taxonomy: taxonomy.clone(),
                label: label.to_owned(),
                source: LabelSourceKind::Decoder,
                provenance: format!("decode:{}", evidence.decode_id),
                confidence: 1.0,
            },
            t: evidence.t,
            window: window_of(evidence.t, pad_pre_s, pad_post_s),
            band,
            snr_db,
            session: Some(evidence.demodulation_id.to_string()),
        });
    }
    Ok(out)
}

/// The user-labelled emission for `emitter_id`, if its current classification (arbitration rank
/// [`Stage::User`]) carries a full M3 [`hk_model::classify::Classification`]. An `unknown` call is
/// not a positive training label and is skipped.
fn user_emission(
    repo: &Repository,
    emitter_id: EmitterId,
    filter: &DatasetFilter,
    pad_pre_s: f64,
    pad_post_s: f64,
    band: Option<(f64, f64)>,
    snr_db: Option<f64>,
) -> Result<Option<LabelledEmission>, DatasetError> {
    let history = repo.classification_history(emitter_id)?;
    let Some(recorded) = history
        .iter()
        .rev()
        .find(|r| r.stage == Stage::User && r.detail.is_some())
    else {
        return Ok(None);
    };
    let detail = recorded.detail.as_ref().expect("checked above");
    if detail.family == UNKNOWN {
        return Ok(None);
    }
    let label = detail
        .class
        .as_ref()
        .map_or_else(|| detail.family.clone(), |c| c.label.clone());
    if !label_matches_filter(&detail.family, filter) {
        return Ok(None);
    }
    Ok(Some(LabelledEmission {
        emitter_id,
        label: DatasetLabel {
            taxonomy: detail.taxonomy.clone(),
            label,
            source: LabelSourceKind::User,
            provenance: detail.provenance.rules.clone(),
            confidence: detail.confidence,
        },
        t: detail.t,
        window: window_of(detail.t, pad_pre_s, pad_post_s),
        band,
        snr_db,
        session: None,
    }))
}

/// Candidate emitters for `filter`: the one named emitter, or a page walk of
/// [`Repository::query_inventory`] over `filter.time`, capped at `max_candidates` rows (a guard:
/// this only bounds *search*, not the samples returned). `filter.family` is **not** passed to the
/// inventory query: that filters on an emitter's *current* classification family, which a
/// decoder-validated label (§7) need not have set. It is applied per label instead
/// ([`label_matches_filter`]).
fn candidate_emitters(
    repo: &Repository,
    filter: &DatasetFilter,
    max_candidates: usize,
) -> Result<Vec<EmitterId>, DatasetError> {
    if let Some(id) = filter.emitter {
        return Ok(vec![id]);
    }
    let mut ids = Vec::new();
    let mut offset = 0u64;
    loop {
        let page = repo.query_inventory(&InventoryQuery {
            time: filter.time,
            limit: MAX_INVENTORY_PAGE,
            offset,
            // T-219: training data must not inherit the inventory's display filter. A row that
            // defers to another still carries its own decoder-validated labels and samples, and an
            // export that silently dropped them would drop exactly the confusable cases.
            relations: hk_model::RelationVisibility::All,
            ..InventoryQuery::default()
        })?;
        let got = page.entries.len();
        ids.extend(page.entries.into_iter().map(|e| e.emitter.id));
        match page.next_offset {
            Some(next) if ids.len() < max_candidates => offset = next,
            _ => break,
        }
        if got == 0 {
            break;
        }
    }
    Ok(ids)
}

/// Finds labelled emissions matching `filter` (ADR-0016 §7): CRC-valid decoder evidence and user
/// classifications, each carrying enough context to place and caption an IQ snippet. Returns at
/// most `max` emissions, decoder emissions before user emissions within an emitter, emitters in
/// [`Repository::query_inventory`] order (or the one named emitter).
pub fn find_labelled_emissions(
    repo: &Repository,
    filter: &DatasetFilter,
    pad_pre_s: f64,
    pad_post_s: f64,
    max: usize,
) -> Result<Vec<LabelledEmission>, DatasetError> {
    let mut out = Vec::new();
    for emitter_id in candidate_emitters(repo, filter, max.saturating_mul(8).max(max))? {
        if out.len() >= max {
            break;
        }
        let emitter = match repo.emitter(emitter_id) {
            Ok(e) => e,
            Err(RepoError::NotFound { .. }) => continue, // merged away between the two reads.
            Err(e) => return Err(e.into()),
        };
        let band = Some((emitter.freq().lo_hz, emitter.freq().hi_hz));
        let snr_db = snr_of(repo, emitter_id)?;
        out.extend(decoder_emissions(
            repo, emitter_id, filter, pad_pre_s, pad_post_s, band, snr_db,
        )?);
        if let Some(u) = user_emission(
            repo, emitter_id, filter, pad_pre_s, pad_post_s, band, snr_db,
        )? {
            out.push(u);
        }
    }
    out.truncate(max);
    Ok(out)
}

/// Exports `req` against `repo`, writing each labelled emission's snippet through `exporter` and a
/// `GroundTruth`/`Label` [`Annotation`] naming its `hk-mod@1` label, source, split and provenance
/// (ADR-0016 §7/§9). A snippet the exporter cannot produce (evicted, empty) is counted in
/// [`DatasetManifest::skipped`] rather than failing the export.
pub fn export_dataset(
    repo: &mut Repository,
    exporter: &mut dyn SnippetExporter,
    req: &DatasetRequest,
) -> Result<DatasetManifest, DatasetError> {
    if req.max_samples == 0 {
        return Err(DatasetError::Invalid(
            "max_samples must be at least 1".into(),
        ));
    }
    for (name, v) in [("pad_pre_s", req.pad_pre_s), ("pad_post_s", req.pad_post_s)] {
        if !(v.is_finite() && v >= 0.0) {
            return Err(DatasetError::Invalid(format!(
                "{name} must be finite and non-negative"
            )));
        }
    }
    let emissions = find_labelled_emissions(
        repo,
        &req.filter,
        req.pad_pre_s,
        req.pad_post_s,
        req.max_samples,
    )?;
    let mut samples = Vec::with_capacity(emissions.len());
    let mut skipped = 0usize;
    for emission in emissions {
        let snippet = match exporter.export(emission.window, emission.band) {
            Ok(s) => s,
            Err(DatasetError::SnippetUnavailable(_)) => {
                skipped += 1;
                continue;
            }
            Err(e) => return Err(e),
        };
        let annotation = Annotation {
            id: AnnotationId::new(),
            target: AnnotationTarget::Recording(hk_model::recording::RecordingSpan {
                recording_id: snippet.recording_id,
                sample_start: None,
                sample_count: None,
                region: None,
            }),
            author: match emission.label.source {
                LabelSourceKind::Decoder => AnnotationAuthor::Decoder,
                LabelSourceKind::User => AnnotationAuthor::User,
            },
            author_ref: emission.label.provenance.clone(),
            kind: match emission.label.source {
                LabelSourceKind::Decoder => AnnotationKind::GroundTruth,
                LabelSourceKind::User => AnnotationKind::Label,
            },
            value: format!("{}/{}", emission.label.taxonomy, emission.label.label),
            metadata: json!({
                "taxonomy": emission.label.taxonomy.to_string(),
                "label": emission.label.label,
                "source": match emission.label.source {
                    LabelSourceKind::Decoder => "decoder",
                    LabelSourceKind::User => "user",
                },
                "split": req.split.as_str(),
                "emitter_id": emission.emitter_id.to_string(),
                "session": emission.session,
                "snr_db": emission.snr_db,
            }),
            content: None,
            confidence: emission.label.confidence,
            supersedes: None,
            content_class: ContentClass::MetadataOnly,
            t: emission.t,
            exported: true,
        };
        repo.insert_annotation(&annotation)?;
        let center_offset_hz = emission
            .band
            .map(|(lo, hi)| (lo + hi) / 2.0 - snippet.center_hz)
            .unwrap_or(0.0);
        samples.push(DatasetSample {
            emitter_id: emission.emitter_id,
            session: emission.session,
            recording_id: snippet.recording_id,
            annotation_id: annotation.id,
            label: emission.label,
            snr_db: emission.snr_db,
            sample_rate_hz: snippet.sample_rate_hz,
            center_offset_hz,
            t: emission.t,
            split: req.split,
        });
    }
    Ok(DatasetManifest {
        id: uuid::Uuid::now_v7().to_string(),
        filter: req.filter.clone(),
        split: req.split,
        created_at: Timestamp::now(),
        samples,
        skipped,
    })
}

#[cfg(test)]
mod tests {
    use hk_model::classify::{
        ArbRank, ClassCall, ClassProvenance, Classification, LabelP, entropy_norm,
    };
    use hk_model::cluster::Sighting;
    use hk_model::{Decode, Demodulation, EstimatedParams, Fingerprint, MeasurementKey};

    use super::*;

    fn tmp_repo() -> Repository {
        Repository::open_in_memory().unwrap()
    }

    /// Sights a fresh emitter at `f_center_hz` and returns its id.
    fn sight(repo: &mut Repository, f_center_hz: f64, t: Timestamp) -> EmitterId {
        let sighting = Sighting {
            source: hk_model::LinkTarget::Track(hk_model::TrackId::new()),
            seen: TimeRange::new(t, t),
            count: 1,
            f_center_hz,
            bandwidth_hz: 20_000.0,
            fingerprint: Some(Fingerprint::new(f_center_hz, 20_000.0)),
            identity: None,
            context: None,
            classification: None,
            tags: Vec::new(),
        };
        repo.record_sighting_measured(&sighting, &MeasurementKey::new("test"), None)
            .unwrap()
            .emitter_id
    }

    /// Records a demodulation of `mode` on `emitter_id`, then a decode of `crc` on it at `t`.
    fn demod_and_decode(
        repo: &mut Repository,
        emitter_id: EmitterId,
        mode: &str,
        crc: CrcStatus,
        t: Timestamp,
    ) -> hk_model::DemodulationId {
        let demod = Demodulation {
            id: hk_model::DemodulationId::new(),
            emitter_ref: Some(emitter_id),
            detection_ref: None,
            recording_ref: None,
            mode: mode.into(),
            params: EstimatedParams::default(),
            lock_quality: None,
            evm_db: None,
            time: TimeRange::new(t, t),
            demod_version: "test@1".into(),
        };
        repo.insert_demodulation(&demod).unwrap();
        repo.insert_decode(&Decode {
            id: hk_model::DecodeId::new(),
            demodulation_ref: Some(demod.id),
            recording_ref: None,
            decoder_id: "test-decoder".into(),
            decoder_version: "1".into(),
            frame_model: "test-frame".into(),
            metadata: json!({}),
            content: None,
            crc_status: crc,
            identity: None,
            content_class: ContentClass::MetadataOnly,
            t,
            provenance: None,
        })
        .unwrap();
        demod.id
    }

    fn sample_classification(family: &str, t: Timestamp) -> Classification {
        let posterior = vec![
            LabelP {
                label: family.into(),
                p: 0.9,
            },
            LabelP {
                label: "unknown".into(),
                p: 0.1,
            },
        ];
        let tax = TaxonomyRef::current();
        let k = tax.resolve().unwrap().families.len() + 1;
        Classification {
            schema: 1,
            t,
            taxonomy: tax.clone(),
            input: None,
            coarse: tax.resolve().unwrap().coarse_of(family).unwrap(),
            entropy_norm: entropy_norm(&posterior, k),
            likelihood: posterior.clone(),
            posterior,
            prior: None,
            family: family.into(),
            confidence: 0.9,
            class: Some(ClassCall {
                label: family_class(family),
                p: 0.9,
                dist: vec![],
                stage: Stage::User,
            }),
            open_set_score: 0.1,
            stage: Stage::User,
            provenance: ClassProvenance {
                rules: "hk-ui/reclassify@1".into(),
                // Determinate (T-292): this fixture stands for a row a current writer produced,
                // not the pre-T-290 `FEATURES_VERSION_INDETERMINATE` marker. hk-store doesn't
                // depend on hk-classify, so it can't name `hk_classify::FEATURES_VERSION`
                // directly.
                features_version: 2,
                features_ref: None,
                ml: None,
                snr_db: Some(18.0),
                snr_gate_db: 15.0,
                gated: false,
                thresholds: "thresholds@1".into(),
                suspect: Default::default(),
                power_mode: None,
            },
            flags: vec![],
            reasons: vec![],
        }
    }

    fn family_class(family: &str) -> String {
        TaxonomyRef::current()
            .resolve()
            .unwrap()
            .family(family)
            .unwrap()
            .classes[0]
            .to_owned()
    }

    fn t(s: i64) -> Timestamp {
        Timestamp::from_unix_nanos(s * 1_000_000_000)
    }

    /// A fake exporter: always succeeds, mints a fresh RecordingId and no real files.
    struct FakeExporter {
        calls: Vec<(TimeRange, Option<(f64, f64)>)>,
    }

    impl SnippetExporter for FakeExporter {
        fn export(
            &mut self,
            window: TimeRange,
            band: Option<(f64, f64)>,
        ) -> Result<ExportedSnippet, DatasetError> {
            self.calls.push((window, band));
            Ok(ExportedSnippet {
                recording_id: RecordingId::new(),
                sample_rate_hz: 2_000_000.0,
                center_hz: band.map_or(100e6, |(lo, hi)| (lo + hi) / 2.0),
            })
        }
    }

    /// An exporter that always says the ring evicted the range.
    struct EvictingExporter;

    impl SnippetExporter for EvictingExporter {
        fn export(
            &mut self,
            _window: TimeRange,
            _band: Option<(f64, f64)>,
        ) -> Result<ExportedSnippet, DatasetError> {
            Err(DatasetError::SnippetUnavailable("evicted".into()))
        }
    }

    #[test]
    fn crc_valid_decode_becomes_a_decoder_label_and_crc_invalid_does_not() {
        let mut repo = tmp_repo();
        let emitter = sight(&mut repo, 433_920_000.0, t(0));
        demod_and_decode(&mut repo, emitter, "2fsk", CrcStatus::Valid, t(1));
        let bad_emitter = sight(&mut repo, 434_000_000.0, t(0));
        demod_and_decode(&mut repo, bad_emitter, "2fsk", CrcStatus::Invalid, t(1));

        let found = find_labelled_emissions(
            &repo,
            &DatasetFilter {
                emitter: Some(emitter),
                ..Default::default()
            },
            0.05,
            0.05,
            10,
        )
        .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].label.label, "fsk");
        assert_eq!(found[0].label.source, LabelSourceKind::Decoder);
        assert_eq!(found[0].label.confidence, 1.0);
        assert!(found[0].label.provenance.starts_with("decode:"));

        let none = find_labelled_emissions(
            &repo,
            &DatasetFilter {
                emitter: Some(bad_emitter),
                ..Default::default()
            },
            0.05,
            0.05,
            10,
        )
        .unwrap();
        assert!(none.is_empty(), "an invalid CRC must never become a label");
    }

    #[test]
    fn a_user_classification_becomes_a_user_label() {
        let mut repo = tmp_repo();
        let emitter = sight(&mut repo, 100_800_000.0, t(0));
        repo.record_classification(
            emitter,
            &sample_classification("analog", t(5)),
            ArbRank::User,
        )
        .unwrap();

        let found =
            find_labelled_emissions(&repo, &DatasetFilter::default(), 0.05, 0.05, 10).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].label.source, LabelSourceKind::User);
        assert_eq!(found[0].label.label, family_class("analog"));
        assert_eq!(found[0].session, None);
    }

    #[test]
    fn family_filter_matches_only_that_family() {
        let mut repo = tmp_repo();
        let fsk = sight(&mut repo, 433_920_000.0, t(0));
        demod_and_decode(&mut repo, fsk, "2fsk", CrcStatus::Valid, t(1));
        let wfm = sight(&mut repo, 100_800_000.0, t(0));
        demod_and_decode(&mut repo, wfm, "wfm", CrcStatus::Valid, t(1));

        let found = find_labelled_emissions(
            &repo,
            &DatasetFilter {
                family: Some("fsk".into()),
                ..Default::default()
            },
            0.05,
            0.05,
            10,
        )
        .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].emitter_id, fsk);
    }

    #[test]
    fn export_writes_a_recording_and_a_ground_truth_annotation_per_sample() {
        let mut repo = tmp_repo();
        let emitter = sight(&mut repo, 433_920_000.0, t(0));
        demod_and_decode(&mut repo, emitter, "2fsk", CrcStatus::Valid, t(1));

        let req = DatasetRequest {
            filter: DatasetFilter {
                emitter: Some(emitter),
                ..Default::default()
            },
            split: Split::Dev,
            ..Default::default()
        };
        let mut exporter = FakeExporter { calls: Vec::new() };
        let manifest = export_dataset(&mut repo, &mut exporter, &req).unwrap();

        assert_eq!(manifest.samples.len(), 1);
        assert_eq!(manifest.skipped, 0);
        assert_eq!(manifest.split, Split::Dev);
        let sample = &manifest.samples[0];
        assert_eq!(sample.split, Split::Dev);
        assert_eq!(sample.label.label, "fsk");

        let annotation = repo.annotation(sample.annotation_id).unwrap();
        assert_eq!(annotation.kind, AnnotationKind::GroundTruth);
        assert_eq!(annotation.author, AnnotationAuthor::Decoder);
        assert!(annotation.exported);
        assert_eq!(annotation.metadata["split"], "dev");
        assert_eq!(annotation.metadata["label"], "fsk");

        // The window padding reached the exporter.
        assert_eq!(exporter.calls.len(), 1);
        let (window, _band) = exporter.calls[0];
        assert!(window.start < t(1) && window.end > t(1));
    }

    #[test]
    fn an_evicted_snippet_is_skipped_not_a_failure() {
        let mut repo = tmp_repo();
        let emitter = sight(&mut repo, 433_920_000.0, t(0));
        demod_and_decode(&mut repo, emitter, "2fsk", CrcStatus::Valid, t(1));

        let req = DatasetRequest {
            filter: DatasetFilter {
                emitter: Some(emitter),
                ..Default::default()
            },
            split: Split::Acceptance,
            ..Default::default()
        };
        let manifest = export_dataset(&mut repo, &mut EvictingExporter, &req).unwrap();
        assert_eq!(manifest.samples.len(), 0);
        assert_eq!(manifest.skipped, 1);
    }

    #[test]
    fn manifest_round_trips_through_json() {
        let mut repo = tmp_repo();
        let emitter = sight(&mut repo, 433_920_000.0, t(0));
        demod_and_decode(&mut repo, emitter, "2fsk", CrcStatus::Valid, t(1));
        let req = DatasetRequest {
            filter: DatasetFilter {
                emitter: Some(emitter),
                ..Default::default()
            },
            split: Split::Dev,
            ..Default::default()
        };
        let mut exporter = FakeExporter { calls: Vec::new() };
        let manifest = export_dataset(&mut repo, &mut exporter, &req).unwrap();
        let json = serde_json::to_string(&manifest).unwrap();
        let back: DatasetManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, manifest);
    }

    #[test]
    fn zero_max_samples_is_refused() {
        let mut repo = tmp_repo();
        let req = DatasetRequest {
            max_samples: 0,
            ..Default::default()
        };
        let mut exporter = FakeExporter { calls: Vec::new() };
        assert!(matches!(
            export_dataset(&mut repo, &mut exporter, &req),
            Err(DatasetError::Invalid(_))
        ));
    }
}
