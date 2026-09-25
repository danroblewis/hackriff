//! Demodulated, framed bursts → data-model records (docs/07 §2.11, §2.13–2.16), written
//! through the [`Repository`].
//!
//! # Content policy (legal guardrail)
//! The emitter's content class **fails closed** ([`ContentClass::FAIL_CLOSED`],
//! metadata-only). Only a caller that positively classifies the emitter
//! ([`EmitterClassification`], e.g. "the user's own sensor") lets payload content flow, and an
//! `encrypted-or-scrambled` payload is metadata-only whatever the classification (labelled,
//! never decoded). Metadata always flows: rate, deviation, CFO, lock, preamble length, sync
//! position and word, CRC algorithm and result, frame and payload **lengths**.
//!
//! # Rows
//! - **Emitter** sighting through entity resolution (`Repository::record_sighting_measured`,
//!   T-018/T-034), keyed by the first demodulation, identity `other:hk-framing` = the framing
//!   signature (structure) under the **effective content class** (fail closed: metadata-only,
//!   so the identity is withheld from inventory output, unless the caller classifies the
//!   emitter), a 2-FSK fingerprint (centre, bandwidth, symbol rate, deviation), the emitter hint
//!   as context and the `2fsk` classification. Re-demodulating the same bursts is a
//!   re-measurement (producer `hk-infer`, same span and channel): the count does not grow.
//! - **Demodulation** per demodulated burst whose two-level alphabet was measured (mode `2fsk`;
//!   [`FskBurst::alphabet_evidence_framed`], T-614). A burst the estimator abstains on — analogue
//!   FM demodulated at an unconfirmed standard-rate trial, or bits that merely repeat — gets no
//!   row, and when no burst was measured the emitter gets no `2fsk` classification either.
//! - **Decode** per burst with a located sync (decoder `hk-infer`): `frame_model`
//!   `inferred:2fsk:<signature>`, structure metadata, `crc_status`, content (payload hex/bits)
//!   only when the effective class permits it (and, for CRC models, the CRC validates).
//! - **Bitstream** live descriptor for the bits stream ([`super::stream`]).
//! - **Ground-truth Annotation** on the Emitter when a CRC is claimed and at least one frame
//!   validates (metadata-only form).
//! - **Known status** `known` (author decoder) appended only with a ground truth **and** a
//!   positive classification: a third-party emitter keeps its status.

use hk_estimate::framing::{FramingResult, PayloadClass, bits};

/// Classification / fingerprint family and Demodulation mode of this writer's emitters. A
/// modulation, not a service: `hk_pipeline::family` maps it to no band-plan service family, so
/// on its own it leaves an emitter's known status `unknown` (T-039).
pub const FSK_FAMILY: &str = "2fsk";
use hk_model::{
    Annotation, AnnotationAuthor, AnnotationId, AnnotationKind, AnnotationTarget, Bitstream,
    BitstreamId, BitstreamPayload, BitstreamTransport, Classification, ContentClass, CrcStatus,
    Decode, DecodeId, DecodedIdentity, Demodulation, DemodulationId, DetectionId, EmitterId,
    EmitterLink, Fingerprint, Framing, IdentityClaim, IdentityScheme, KnownStatus,
    KnownStatusChange, LinkTarget, MeasurementKey, RecordingId, RepoError, Repository, Sighting,
    StatusAuthor, TimeRange,
};
use serde_json::{Value, json};

use super::demod::FSK_DEMOD_VERSION;
use super::receiver::{FrameEvidence, FskBurst};

/// Decoder id of inferred-framing decodes.
pub const INFER_DECODER_ID: &str = "hk-infer";
/// Its version.
pub const INFER_DECODER_VERSION: &str = "0.1.0";
/// Identity scheme name of a framing signature.
pub const FRAMING_IDENTITY_SCHEME: &str = hk_model::FRAMING_IDENTITY_SCHEME;

/// A caller's positive classification of the emitter, e.g. the user's own device.
#[derive(Clone, Debug, PartialEq)]
pub struct EmitterClassification {
    /// Content class the caller vouches for.
    pub content_class: ContentClass,
    /// Who classified it and why, e.g. `user: own 433 MHz sensor`.
    pub by: String,
}

/// References and policy the caller knows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FramedRecordContext {
    /// Recording replayed, if offline.
    pub recording_ref: Option<RecordingId>,
    /// Detection the bursts came from.
    pub detection_ref: Option<DetectionId>,
    /// Emitter the caller resolved the bursts to. With a framing identity the repository's
    /// identity match takes precedence.
    pub emitter_hint: Option<EmitterId>,
    /// Positive classification. `None` (the default) keeps content closed.
    pub classification: Option<EmitterClassification>,
    /// Live bits-stream endpoint for the Bitstream descriptor.
    pub bitstream_endpoint: Option<String>,
}

impl FramedRecordContext {
    /// The classified content class, else [`ContentClass::FAIL_CLOSED`].
    pub fn content_class(&self) -> ContentClass {
        self.classification
            .as_ref()
            .map_or(ContentClass::FAIL_CLOSED, |c| c.content_class)
    }
}

/// The class records and streams actually use: the context class, forced to
/// [`ContentClass::FAIL_CLOSED`] for an encrypted-or-scrambled payload.
pub fn effective_content_class(ctx: &FramedRecordContext, result: &FramingResult) -> ContentClass {
    let encrypted = result
        .model
        .payload
        .as_ref()
        .is_some_and(|p| p.class == PayloadClass::EncryptedOrScrambled);
    if encrypted {
        ContentClass::FAIL_CLOSED
    } else {
        ctx.content_class()
    }
}

/// What was written.
#[derive(Clone, Debug, PartialEq)]
pub struct WrittenFraming {
    /// Emitter.
    pub emitter_id: EmitterId,
    /// A new Emitter was created.
    pub emitter_created: bool,
    /// Demodulation rows, one per demodulated burst (burst index, id).
    pub demodulation_ids: Vec<(usize, DemodulationId)>,
    /// Decode rows (burst index, id).
    pub decode_ids: Vec<(usize, DecodeId)>,
    /// Live Bitstream descriptor.
    pub bitstream: Option<Bitstream>,
    /// Ground-truth annotation.
    pub ground_truth_id: Option<AnnotationId>,
    /// A known-status change was appended.
    pub known_status_appended: bool,
    /// Effective content class.
    pub content_class: ContentClass,
    /// Decodes whose content was withheld by the class.
    pub content_withheld: usize,
}

/// The emitter identity for a framing model with a sync (structure, not content).
pub fn framing_identity(result: &FramingResult) -> Option<DecodedIdentity> {
    result.model.sync.as_ref().map(|_| DecodedIdentity {
        scheme: IdentityScheme::Other(FRAMING_IDENTITY_SCHEME.into()),
        value: format!("2fsk;{}", result.model.signature()),
    })
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    v.retain(|x| x.is_finite());
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    Some(v[v.len() / 2])
}

/// Structure metadata of one framed burst (never payload values).
fn decode_metadata(result: &FramingResult, burst: &FskBurst, index: usize) -> Value {
    let m = &result.model;
    let f = &result.frames[index];
    let s = burst.symbols.as_ref();
    let payload_len = result.payload(index, burst.bits()).map(|p| p.bits.len());
    json!({
        "framing_signature": m.signature(),
        "framing_status": m.status,
        "burst_index": index,
        "symbols": burst.bits().len(),
        "symbol_rate_bd": s.map(|s| s.lock.tracked_rate_bd),
        "deviation_hz": s.and_then(|s| s.deviation_hz),
        "cfo_hz": s.map(|s| s.cfo_hz()),
        "rf_center_hz": burst.rf_center_hz(),
        "lock_quality": s.map(|s| s.lock.lock_quality),
        "timing_rms_ui": s.map(|s| s.lock.timing_rms_ui),
        "eye_opening": s.map(|s| s.lock.eye_opening),
        "seed": burst.seed.source.as_str(),
        "c14_trusted": burst.seed.c14_trusted,
        "preamble_bits": f.preamble_bits_before_sync,
        "sync_bit": f.sync_bit,
        "sync_bit_errors": f.sync_bit_errors,
        "sync_word": m.sync.as_ref().map(|s| s.bits.clone()),
        "polarity_inverted": f.inverted,
        "crc": m.crc.as_ref().map(|c| json!({
            "algorithm": c.algorithm,
            "valid": f.crc_valid,
            "validate_ratio": c.validate_ratio,
            "span_start_bits": c.span.start_bits,
            "covered_bits": c.span.covered_bits,
            "endianness": c.endianness,
            "bit_order": c.bit_order,
        })),
        "length_field": m.length_field.as_ref().map(|l| json!({
            "offset_bits": l.offset_bits, "kind": l.kind, "support_ratio": l.support_ratio,
        })),
        "whitening": m.whitening.as_ref().map(|w| w.name.clone()),
        "frame_bits": m.frame.frame_bits,
        "payload_bits": payload_len,
        "payload_class": m.payload.as_ref().map(|p| p.class.as_str()),
    })
}

/// What framing found in burst `i` (T-614): the learned sync located, the CRC valid.
fn frame_evidence(result: &FramingResult, i: usize) -> FrameEvidence {
    result
        .frames
        .get(i)
        .map_or_else(FrameEvidence::default, |f| FrameEvidence {
            sync_found: result.model.sync.is_some() && f.sync_bit.is_some(),
            crc_valid: f.crc_valid == Some(true),
        })
}

/// Writes the records. See the [module docs](self). `bursts[i]` must be the burst whose bits
/// were `result`'s input `i`.
pub fn write_framed_bursts(
    repo: &mut Repository,
    bursts: &[FskBurst],
    result: &FramingResult,
    ctx: &FramedRecordContext,
) -> Result<WrittenFraming, RepoError> {
    let class = effective_content_class(ctx, result);
    let model = &result.model;
    let starts = bursts.iter().map(|b| b.time_range().start);
    let ends = bursts.iter().map(|b| b.time_range().end);
    let seen = TimeRange::new(
        starts.min().unwrap_or(hk_model::Timestamp::UNIX_EPOCH),
        ends.max().unwrap_or(hk_model::Timestamp::UNIX_EPOCH),
    );
    let f_center = median(bursts.iter().map(FskBurst::rf_center_hz).collect()).unwrap_or(0.0);
    let bandwidth = median(
        bursts
            .iter()
            .filter_map(|b| b.params.obw99_hz.value())
            .collect(),
    )
    .or_else(|| median(bursts.iter().map(|b| b.request.bandwidth_hz).collect()))
    .unwrap_or(0.0);
    let identity = framing_identity(result);
    let now = seen.end;
    // T-614: which bursts' two-level alphabet was *measured*. A burst the estimator abstains on
    // (analogue FM demodulated at a standard-rate trial nothing confirmed) gets no `2fsk`
    // Demodulation row, and contributes nothing to the 2-FSK fingerprint: bits existing is not
    // evidence of symbols.
    let measured: Vec<bool> = bursts
        .iter()
        .enumerate()
        .map(|(i, b)| {
            b.alphabet_evidence_framed(frame_evidence(result, i))
                .is_measured()
        })
        .collect();
    let any_measured = measured.iter().any(|&m| m);
    let measured_symbols = || {
        bursts
            .iter()
            .zip(&measured)
            .filter(|(_, m)| **m)
            .filter_map(|(b, _)| b.symbols.as_ref())
    };
    // Ids up front: the sighting is keyed by the first demodulation (a fresh id when no burst
    // demodulated), and the rows reference the emitter it resolves to.
    let demod_ids: Vec<Option<DemodulationId>> = bursts
        .iter()
        .zip(&measured)
        .map(|(b, &m)| {
            b.symbols
                .as_ref()
                .filter(|_| m)
                .map(|_| DemodulationId::new())
        })
        .collect();
    let source = demod_ids
        .iter()
        .flatten()
        .next()
        .copied()
        .unwrap_or_else(DemodulationId::new);
    let sighting = Sighting {
        source: LinkTarget::Demodulation(source),
        seen,
        count: bursts.len() as u64,
        f_center_hz: f_center,
        bandwidth_hz: bandwidth,
        fingerprint: Some(Fingerprint {
            family: any_measured.then(|| FSK_FAMILY.into()),
            symbol_rate_hz: median(measured_symbols().map(|s| s.rate_bd).collect()),
            deviation_hz: median(measured_symbols().filter_map(|s| s.deviation_hz).collect()),
            ..Fingerprint::new(f_center, bandwidth)
        }),
        identity: identity.clone().map(|identity| IdentityClaim {
            identity,
            content_class: class,
        }),
        context: ctx.emitter_hint,
        // T-614: no measured alphabet in any burst, no `2fsk` claim on the emitter.
        classification: any_measured.then(|| Classification {
            t: now,
            family: FSK_FAMILY.into(),
            confidence: model.confidence,
            open_set_score: 1.0 - model.confidence,
            model_version: FSK_DEMOD_VERSION.into(),
        }),
        tags: Vec::new(),
    };
    let resolution =
        repo.record_sighting_measured(&sighting, &MeasurementKey::new(INFER_DECODER_ID), None)?;
    let eid = resolution.emitter_id;

    let mut demodulation_ids = Vec::new();
    let mut decode_ids = Vec::new();
    let mut content_withheld = 0;
    for (i, burst) in bursts.iter().enumerate() {
        let (Some(sy), Some(demod_id)) = (&burst.symbols, demod_ids[i]) else {
            continue;
        };
        let demod = Demodulation {
            id: demod_id,
            emitter_ref: Some(eid),
            detection_ref: ctx.detection_ref,
            recording_ref: ctx.recording_ref,
            mode: FSK_FAMILY.into(),
            params: burst.estimated_params_framed(frame_evidence(result, i)),
            lock_quality: Some(sy.lock.lock_quality),
            evm_db: None,
            time: burst.time_range(),
            demod_version: burst.demod_version.clone(),
        };
        repo.insert_demodulation(&demod)?;
        demodulation_ids.push((i, demod.id));
        let frame = &result.frames[i];
        let Some(sync_bit) = frame.sync_bit else {
            continue;
        };
        let crc_status = match (&model.crc, frame.crc_valid) {
            (Some(_), Some(true)) => CrcStatus::Valid,
            (Some(_), Some(false)) => CrcStatus::Invalid,
            _ => CrcStatus::Unknown,
        };
        let content_ok = model.crc.is_none() || crc_status == CrcStatus::Valid;
        let content = if class.permits_content() && content_ok {
            result.payload(i, burst.bits()).map(|p| {
                json!({
                    // Lowercase hex (T-037b): the one `payload_hex` convention, as the synthetic
                    // generators' truth and the other hex identities (ICAO, hashes) are written.
                    "payload_hex": bits::hex(&p.bits[..p.bits.len() / 8 * 8], p.bit_order)
                        .map(|h| h.to_ascii_lowercase()),
                    "payload_bits": bits::bit_string(&p.bits),
                    "bit_order": p.bit_order,
                })
            })
        } else {
            None
        };
        if content.is_none() && content_ok {
            content_withheld += 1;
        }
        let d = Decode {
            id: DecodeId::new(),
            demodulation_ref: Some(demod.id),
            recording_ref: ctx.recording_ref,
            decoder_id: INFER_DECODER_ID.into(),
            decoder_version: INFER_DECODER_VERSION.into(),
            frame_model: format!("inferred:2fsk:{}", model.signature()),
            metadata: decode_metadata(result, burst, i),
            content,
            crc_status,
            identity: identity.clone(),
            content_class: class,
            t: burst.timestamp_of_symbol(sync_bit),
            provenance: None,
        };
        repo.insert_decode(&d)?;
        decode_ids.push((i, d.id));
    }

    let bitstream = model.sync.as_ref().map(|s| Bitstream {
        id: BitstreamId::new(),
        emitter_ref: Some(eid),
        demodulation_ref: demodulation_ids.first().map(|d| d.1),
        framing: Framing {
            payload: BitstreamPayload::HardBits,
            bits_per_symbol: Some(1),
            symbol_rate_hz: median(measured_symbols().map(|s| s.rate_bd).collect()),
            schema_id: Some(format!("{FRAMING_IDENTITY_SCHEME}:{}", model.signature())),
            sync_word_hex: s.hex.clone(),
        },
        transport: BitstreamTransport::Live {
            endpoint: ctx
                .bitstream_endpoint
                .clone()
                .unwrap_or_else(|| format!("hk-stream:bits/{eid}")),
        },
        time: seen,
        provenance_ref: None,
        content_class: class,
    });
    if let Some(b) = &bitstream {
        repo.insert_bitstream(b)?;
    }

    let valid: Vec<DecodeId> = decode_ids
        .iter()
        .filter(|(i, _)| result.frames[*i].crc_valid == Some(true))
        .map(|d| d.1)
        .collect();
    let mut ground_truth_id = None;
    let mut known_status_appended = false;
    if let (Some(crc), false) = (&model.crc, valid.is_empty()) {
        let label = format!(
            "unknown/2fsk/{}/{}",
            model.sync.as_ref().map_or("nosync".into(), |s| format!(
                "sync-{}",
                s.hex
                    .clone()
                    .unwrap_or_else(|| s.bits.clone())
                    .to_lowercase()
            )),
            crc.algorithm.to_lowercase().replace('/', "-")
        );
        let a = Annotation {
            id: AnnotationId::new(),
            target: AnnotationTarget::Emitter(eid),
            author: AnnotationAuthor::Decoder,
            author_ref: format!("{INFER_DECODER_ID}@{INFER_DECODER_VERSION}"),
            kind: AnnotationKind::GroundTruth,
            value: label,
            metadata: json!({
                "source": "CRC-valid inferred framing (C21)",
                "framing_model": serde_json::to_value(model).unwrap_or(Value::Null),
                "valid_frames": valid.len(),
                "decodes": valid,
            }),
            content: None,
            confidence: model.confidence,
            supersedes: None,
            content_class: class,
            t: now,
            exported: false,
        };
        repo.insert_annotation(&a)?;
        ground_truth_id = Some(a.id);
        if let Some(c) = ctx
            .classification
            .as_ref()
            .filter(|c| c.content_class.permits_content())
        {
            repo.append_known_status(&KnownStatusChange {
                emitter_id: eid,
                status: KnownStatus::Known,
                prior_ref: None,
                reason: format!(
                    "CRC-valid inferred framing {} ({}/{} frames); emitter classified by {}",
                    model.signature(),
                    crc.validated,
                    crc.tested,
                    c.by
                ),
                t: now,
                author: StatusAuthor::Decoder,
            })?;
            known_status_appended = true;
        }
    }

    let links = demodulation_ids
        .iter()
        .map(|d| LinkTarget::Demodulation(d.1))
        .chain(decode_ids.iter().map(|d| LinkTarget::Decode(d.1)))
        .chain(ground_truth_id.map(LinkTarget::Annotation));
    for target in links {
        repo.link_emitter(&EmitterLink {
            emitter_id: eid,
            target,
            linked_at: now,
        })?;
    }
    Ok(WrittenFraming {
        emitter_id: eid,
        emitter_created: resolution.created,
        demodulation_ids,
        decode_ids,
        bitstream,
        ground_truth_id,
        known_status_appended,
        content_class: class,
        content_withheld,
    })
}
