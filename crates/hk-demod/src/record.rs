//! [`AnalogSession`] → data-model records (docs/07 §2.11, §2.13–2.15), written through the
//! [`Repository`].
//!
//! - **Demodulation:** mode, estimated params (CFO, bandwidth, deviation), pilot lock quality,
//!   time span, `demod_version`.
//! - **Decode rows** (decoder `hk-rds`), content class **unrestricted** (public broadcast), no
//!   content field (every RDS field used here is identity metadata), CRC valid, identity
//!   `rds-pi`:
//!   - one `rds-pi` row for the accepted PI with the vote, PTY/TP/TA, group types, PS frames and
//!     block/group error rates;
//!   - one `rds-group-0-ps-frame` row per complete PS frame sent under the accepted PI, with
//!     `pi`, `ps`, `pty`, `tp`.
//! - **Emitter:** a sighting through entity resolution (`Repository::record_sighting_measured`,
//!   T-018/T-034) keyed by the demodulation, with identity `rds-pi:<hex>` of class
//!   **unrestricted** (broadcast), the WFM fingerprint, the caller's emitter hint as context and
//!   the mode classification; links to the decodes; a **label** annotation (author decoder)
//!   from the most frequent PS frame.
//! - **Re-demodulation** of the same IQ is a re-measurement (producer `hk-rds`, same span and
//!   channel): new rows are written and linked, the emitter's count does not grow.
//!
//! Nothing identity-bearing is written without an accepted PI.

use hk_model::{
    Annotation, AnnotationAuthor, AnnotationId, AnnotationKind, AnnotationTarget, Classification,
    ContentClass, CrcStatus, Decode, DecodeId, DecodedIdentity, Demodulation, DemodulationId,
    DetectionId, EmitterId, EmitterLink, Fingerprint, IdentityClaim, IdentityScheme, LinkTarget,
    MeasurementKey, RecordingId, RepoError, Repository, Sighting,
};
use serde_json::json;

use crate::receiver::AnalogSession;
use crate::{RDS_DECODER_ID, RDS_DECODER_VERSION};

/// References the caller knows.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RecordContext {
    /// Recording replayed, if offline.
    pub recording_ref: Option<RecordingId>,
    /// Detection the channel came from.
    pub detection_ref: Option<DetectionId>,
    /// Emitter the caller resolved the channel to (T-018). With a decoded PI the repository's
    /// identity match takes precedence.
    pub emitter_hint: Option<EmitterId>,
    /// T-209: producer key of a sighting of this same session the caller already counted into
    /// `emitter_hint` (e.g. an early write from the leading part of the window). The decoded PI's
    /// sighting is then offered under that key, so it resolves as a re-measurement of that
    /// sighting and adds no count, unless another emitter holds the PI (identity rule 2 applies
    /// then, under the decoder's own key).
    pub counted_as: Option<&'static str>,
}

/// What was written.
#[derive(Clone, Debug, PartialEq)]
pub struct WrittenSession {
    /// The Demodulation row.
    pub demodulation_id: DemodulationId,
    /// Decode rows.
    pub decode_ids: Vec<DecodeId>,
    /// Emitter the session was attached to.
    pub emitter_id: Option<EmitterId>,
    /// A new Emitter was created.
    pub emitter_created: bool,
    /// Label annotation.
    pub label_id: Option<AnnotationId>,
    /// Label text.
    pub label: Option<String>,
}

/// The label for an RDS station: the most frequent PS frame, trimmed.
pub fn rds_label(session: &AnalogSession) -> Option<String> {
    session
        .rds()
        .filter(|r| r.pi.is_some())
        .and_then(|r| r.ps())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Builds the RDS Decode rows (no repository access).
pub fn rds_decodes(session: &AnalogSession, demod_id: DemodulationId) -> Vec<Decode> {
    let Some(rds) = session.rds() else {
        return Vec::new();
    };
    let Some(pi) = rds.pi else {
        return Vec::new();
    };
    let identity = DecodedIdentity {
        scheme: IdentityScheme::RdsPi,
        value: pi.hex(),
    };
    let start = session.time_range().start;
    let decode = |frame_model: &str, metadata, t| Decode {
        id: DecodeId::new(),
        demodulation_ref: Some(demod_id),
        recording_ref: None,
        decoder_id: RDS_DECODER_ID.into(),
        decoder_version: RDS_DECODER_VERSION.into(),
        frame_model: frame_model.into(),
        metadata,
        content: None,
        crc_status: CrcStatus::Valid,
        identity: Some(identity.clone()),
        content_class: ContentClass::Unrestricted,
        t,
    };
    let mut out = vec![decode(
        "rds-pi",
        json!({
            "pi": pi.hex(),
            "pi_votes": pi.votes,
            "pi_total_votes": pi.total_votes,
            "pi_share": pi.share,
            "ps": rds.ps(),
            "ps_frames": rds.ps_frames,
            "pty": rds.pty,
            "tp": rds.tp,
            "ta": rds.ta,
            "group_types": rds.group_types,
            "blocks_total": rds.blocks_total,
            "blocks_ok": rds.blocks_ok,
            "block_error_rate": rds.block_error_rate,
            "groups_total": rds.groups_total,
            "groups_ok": rds.groups_ok,
            "group_error_rate": rds.group_error_rate,
            "sync_acquisitions": rds.sync_acquisitions,
            "bit_slips": rds.bit_slips,
        }),
        start,
    )];
    for f in rds.frame_log.iter().filter(|f| f.pi == pi.pi) {
        let t = session.timestamp_of_mpx(f.position).unwrap_or(start);
        out.push(decode(
            "rds-group-0-ps-frame",
            json!({
                "pi": format!("{:04X}", f.pi),
                "ps": f.text,
                "pty": rds.pty,
                "tp": rds.tp,
            }),
            t,
        ));
    }
    out
}

/// Writes the session's records. See the [module docs](self).
pub fn write_session(
    repo: &mut Repository,
    session: &AnalogSession,
    ctx: &RecordContext,
) -> Result<WrittenSession, RepoError> {
    let time = session.time_range();
    let pi = session.rds().and_then(|r| r.pi);
    let demod_id = DemodulationId::new();
    let family = session.mode.mode.as_str();
    let classification = Classification {
        t: time.end,
        family: family.into(),
        confidence: session.mode.confidence,
        open_set_score: 1.0 - session.mode.confidence,
        model_version: session.mode.rules_version.clone(),
    };
    let mut emitter_id = ctx.emitter_hint;
    let mut emitter_created = false;
    if let Some(pi) = pi {
        let identity = DecodedIdentity {
            scheme: IdentityScheme::RdsPi,
            value: pi.hex(),
        };
        let key = match (ctx.counted_as, ctx.emitter_hint) {
            (Some(producer), Some(hint)) => {
                let hint = repo.live_emitter_id(hint)?;
                let holder = repo.emitter_by_identity(&identity)?.map(|e| e.id);
                if holder.is_none_or(|h| h == hint) {
                    producer
                } else {
                    RDS_DECODER_ID
                }
            }
            _ => RDS_DECODER_ID,
        };
        let bandwidth = session.params.obw99_hz.value().unwrap_or(200e3);
        let sighting = Sighting {
            source: LinkTarget::Demodulation(demod_id),
            seen: time,
            count: 1,
            f_center_hz: session.rf_center_hz,
            bandwidth_hz: bandwidth,
            fingerprint: Some(Fingerprint {
                family: Some(family.into()),
                ..Fingerprint::new(session.rf_center_hz, bandwidth)
            }),
            identity: Some(IdentityClaim {
                identity,
                // RDS is public broadcast.
                content_class: ContentClass::Unrestricted,
            }),
            context: ctx.emitter_hint,
            classification: Some(classification.clone()),
            tags: Vec::new(),
        };
        let r = repo.record_sighting_measured(&sighting, &MeasurementKey::new(key), None)?;
        emitter_id = Some(r.emitter_id);
        emitter_created = r.created;
    }
    let demod = Demodulation {
        id: demod_id,
        emitter_ref: emitter_id,
        detection_ref: ctx.detection_ref,
        recording_ref: ctx.recording_ref,
        mode: session.mode.mode.as_str().into(),
        params: session.estimated_params(),
        lock_quality: session.wfm.as_ref().and_then(|w| w.pilot.lock_quality),
        evm_db: None,
        time,
        demod_version: session.demod_version.clone(),
    };
    repo.insert_demodulation(&demod)?;
    let mut decodes = rds_decodes(session, demod.id);
    for d in &mut decodes {
        d.recording_ref = ctx.recording_ref;
        repo.insert_decode(d)?;
    }
    let mut label_id = None;
    let label = rds_label(session);
    if let Some(eid) = emitter_id {
        let now = time.end;
        if pi.is_none() {
            // With a PI the sighting carried it (input: the demodulation).
            repo.append_classification(eid, &classification)?;
        }
        let links = std::iter::once(LinkTarget::Demodulation(demod.id))
            .chain(decodes.iter().map(|d| LinkTarget::Decode(d.id)));
        for target in links {
            repo.link_emitter(&EmitterLink {
                emitter_id: eid,
                target,
                linked_at: now,
            })?;
        }
        if let (Some(text), Some(rds), Some(pi)) = (&label, session.rds(), pi) {
            let frames: u32 = rds.ps_frames.iter().map(|(_, n)| n).sum();
            let top = rds.ps_frames.first().map_or(0, |(_, n)| *n);
            let a = Annotation {
                id: AnnotationId::new(),
                target: AnnotationTarget::Emitter(eid),
                author: AnnotationAuthor::Decoder,
                author_ref: format!("{RDS_DECODER_ID}@{RDS_DECODER_VERSION}"),
                kind: AnnotationKind::Label,
                value: text.clone(),
                metadata: json!({
                    "family": "fm/rds",
                    "source": "most frequent RDS PS frame",
                    "pi": pi.hex(),
                    "ps_frames": rds.ps_frames,
                    "decodes": decodes.iter().map(|d| d.id).collect::<Vec<_>>(),
                }),
                content: None,
                confidence: if frames > 0 {
                    f64::from(top) / f64::from(frames)
                } else {
                    0.0
                },
                supersedes: None,
                content_class: ContentClass::Unrestricted,
                t: now,
                exported: false,
            };
            repo.insert_annotation(&a)?;
            label_id = Some(a.id);
        }
    }
    Ok(WrittenSession {
        demodulation_id: demod.id,
        decode_ids: decodes.iter().map(|d| d.id).collect(),
        emitter_id,
        emitter_created,
        label_id,
        label,
    })
}
