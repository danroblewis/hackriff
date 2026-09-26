//! [`AnalogSession`] → data-model records (docs/07 §2.11, §2.13–2.15), written through the
//! [`Repository`].
//!
//! - **Demodulation:** mode, estimated params (CFO, bandwidth, deviation), pilot lock quality,
//!   time span, `demod_version`.
//! - **Decode rows** (decoder `hk-rds`), content class **unrestricted** (public broadcast), no
//!   content field (every RDS field used here is identity metadata), CRC valid, identity
//!   `rds-pi` **once the PI is committed** (T-962, below):
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
//! Nothing identity-bearing is written without a **committed** PI.
//!
//! **T-962: a provisional PI is written, and is not an identity.** `hk_demod::rds` reports a PI
//! from `pi_min_votes` agreeing CRC-valid blocks but marks it
//! [`provisional`](crate::rds::PiDecision::provisional) until `pi_commit_votes` of them fall
//! within [`hk_model::RDS_PI_COMMIT_WINDOW_NS`] (5 s) of stream time — a rate, not a count,
//! and this writer also refuses an identity below [`hk_model::RDS_PI_COMMIT_VOTES`] (10 — the one
//! bar every RDS producer shares, the `rds` recipe included; see that constant, and
//! `GroupConfig::pi_commit_votes` for why a vote count and not ADR-0022 §6's bits budget). A
//! session whose PI is still provisional writes:
//!
//! - the `rds-pi` Decode row, with `pi`, `pi_votes`, `pi_total_votes`, `pi_share`,
//!   `pi_provisional: true` and the scheme-generic `identity_provisional` / `identity_votes` /
//!   `identity_votes_needed` in its metadata, and **no [`DecodedIdentity`]**. The row is linked to
//!   the caller's emitter hint, and `GET /api/inventory/{id}/decode` serves an identity-less
//!   emitter's linked rows, so the UI can show "PI 1704 (3 groups, provisional)";
//! - **no identity-bearing sighting**, so the emitter is never created, merged or keyed by that
//!   PI, and `Repository::identity_decode_evidence` finds nothing for it. A Confirmed state
//!   therefore cannot rest on it: `ConfirmPolicy`'s route A (decoded identity) needs an identity,
//!   and none was claimed.
//!
//! The session is otherwise recorded exactly as a no-PI session is — Demodulation, classification
//! on the caller's emitter hint, decodes, links, label. This is the same distinction the module
//! already draws for [`write_declined`]: short evidence leaves a **record**, not a **claim**.
//!
//! **A refusal is a record too (T-416).** [`write_declined`] is the counterpart of
//! [`write_session`] for a probe that measured a window and then declined to go on with it — the
//! mode was not one the chain accepts, or the chain wanted a subcarrier lock the emission does not
//! carry. It writes the Demodulation and nothing else: the mode the selector arrived at, the
//! parameters it estimated, the absent lock, and the window they were measured over. **A refusal
//! that leaves no record is indistinguishable from never having looked** — the same distinction
//! `Coverage::Unobserved` draws against observed-and-quiet (T-368) and `BiasTee::Unknown` against
//! off (T-359) — so short evidence leaves a log rather than a promotion. Nothing identity-bearing,
//! no sighting, no classification, no decode, no label: the row is evidence, not a claim.

use hk_model::{
    Annotation, AnnotationAuthor, AnnotationId, AnnotationKind, AnnotationTarget, Classification,
    ContentClass, CrcStatus, Decode, DecodeId, DecodedIdentity, Demodulation, DemodulationId,
    DetectionId, EmitterId, EmitterLink, Fingerprint, IdentityClaim, IdentityScheme, LinkTarget,
    MeasurementKey, RecordingId, RepoError, Repository, Sighting,
};
use hk_model::{TimeRange, Timestamp};
use serde_json::json;

use crate::rds::RdsReport;
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

/// The PI votes that count toward the identity bar (T-962): the decoder's in-window vote, capped
/// at [`IdentityScheme::commit_votes`] — the **model** constant, never the decoder's own
/// `GroupConfig::pi_commit_votes`, which bounds how many vote times the decoder keeps and so what
/// `window_votes` can reach. Capping at the scheme's bar keeps the served contract
/// `identity_provisional == identity_votes_in_window < identity_votes_needed` true for any
/// decoder configuration: a config below the bar keeps `window_votes` under it (never an
/// identity), and one above it cannot report more in-window votes than the bar it is compared to.
fn identity_votes_in_window(pi: &crate::rds::PiDecision) -> u32 {
    pi.window_votes.min(IdentityScheme::RdsPi.commit_votes())
}

/// Whether `pi` may be written as an identity (T-962): its in-window vote reached the one bar
/// every RDS producer shares, [`IdentityScheme::commit_votes`] — so a `GroupConfig` configured
/// below [`hk_model::RDS_PI_COMMIT_VOTES`] cannot weaken it.
///
/// The bar is a rate (round 2): [`crate::rds::PiDecision::window_votes`] must reach it — the votes
/// inside one [`IdentityScheme::commit_window_ns`] span of stream time — not the lifetime count.
fn pi_is_identity(pi: &crate::rds::PiDecision) -> bool {
    identity_votes_in_window(pi) >= IdentityScheme::RdsPi.commit_votes()
}

/// The `rds-pi` row's metadata: the station's accumulated RDS field view (T-971) under `pi`.
fn rds_pi_metadata(rds: &RdsReport, pi: &crate::rds::PiDecision) -> serde_json::Value {
    let committed = pi_is_identity(pi);
    json!({
        "pi": pi.hex(),
        "pi_votes": pi.votes,
        "pi_total_votes": pi.total_votes,
        "pi_share": pi.share,
        "pi_provisional": !committed,
        // The scheme-generic provisional-identity fields every vote-gated producer writes
        // (the `rds` recipe's rows carry the same keys), so one reader serves both.
        "identity_provisional": !committed,
        "identity_votes": pi.votes,
        "identity_votes_needed": IdentityScheme::RdsPi.commit_votes(),
        "identity_votes_in_window": identity_votes_in_window(pi),
        "identity_votes_window_s": IdentityScheme::RdsPi.commit_window_ns() as f64 / 1e9,
        "ps": rds.ps(),
        "ps_frames": rds.ps_frames,
        // T-971: PS as it stands (stable until changed) and a dynamic PS's sequence.
        "ps_current": rds.ps_current,
        "ps_sequence": rds.ps_sequence,
        "ps_dynamic": rds.ps_dynamic,
        // T-971: RadioText (latest complete message, its A/B flag, and the recent messages).
        "rt": rds.rt,
        "rt_ab": rds.rt_ab,
        "rt_messages": rds
            .rt_messages
            .iter()
            .filter(|m| m.pi == pi.pi)
            .map(|m| json!({ "text": m.text, "ab": m.ab }))
            .collect::<Vec<_>>(),
        // T-971: alternative frequencies (MHz) and the latest clock time.
        "af_mhz": rds.af_mhz,
        "ct": rds.ct.map(|c| json!({
            "utc_unix_s": c.utc_unix_s,
            "offset_half_hours": c.offset_half_hours,
            "mjd": c.mjd,
            "hour": c.hour,
            "minute": c.minute,
        })),
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
    })
}

/// An `hk-rds` decode row of `demod_id`.
fn rds_row(
    demod_id: DemodulationId,
    frame_model: &str,
    metadata: serde_json::Value,
    identity: Option<DecodedIdentity>,
    t: Timestamp,
) -> Decode {
    Decode {
        id: DecodeId::new(),
        demodulation_ref: Some(demod_id),
        recording_ref: None,
        decoder_id: RDS_DECODER_ID.into(),
        decoder_version: RDS_DECODER_VERSION.into(),
        frame_model: frame_model.into(),
        metadata,
        content: None,
        crc_status: CrcStatus::Valid,
        identity,
        content_class: ContentClass::Unrestricted,
        t,
        provenance: None,
    }
}

/// Builds the RDS Decode rows (no repository access).
pub fn rds_decodes(session: &AnalogSession, demod_id: DemodulationId) -> Vec<Decode> {
    let Some(rds) = session.rds() else {
        return Vec::new();
    };
    let Some(pi) = rds.pi else {
        return Vec::new();
    };
    // T-962: a provisional PI is evidence, not a claim. The row carries it and its vote; the
    // identity column stays empty, which is what keeps the confirm gate's route A off it.
    let committed = pi_is_identity(&pi);
    let identity = committed.then(|| DecodedIdentity {
        scheme: IdentityScheme::RdsPi,
        value: pi.hex(),
    });
    let start = session.time_range().start;
    let decode = |frame_model: &str, metadata, t| {
        rds_row(demod_id, frame_model, metadata, identity.clone(), t)
    };
    let mut out = vec![decode("rds-pi", rds_pi_metadata(rds, &pi), start)];
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

/// Records a probe that **declined** the window it measured (T-416). See the [module docs](self).
///
/// Writes one Demodulation carrying what was measured — mode, estimated parameters, the absent
/// lock, and the time span — and links it to `ctx.emitter_hint` when the caller knows which
/// inventory entry this emission is. Returns the row's id.
///
/// Deliberately **not** a promotion, and it cannot become one: no sighting is recorded, no
/// classification is appended, no identity, decode or label is written. The confirmation rule's
/// route C reads this row like any other (`ConfirmPolicy::verified_reason`) and refuses it on its
/// own terms, because a declined session carries neither `lock_quality` nor `pilot_hz`.
pub fn write_declined(
    repo: &mut Repository,
    session: &AnalogSession,
    ctx: &RecordContext,
) -> Result<DemodulationId, RepoError> {
    let time = session.time_range();
    let demod = Demodulation {
        id: DemodulationId::new(),
        emitter_ref: ctx.emitter_hint,
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
    if let Some(eid) = ctx.emitter_hint {
        repo.link_emitter(&EmitterLink {
            emitter_id: eid,
            target: LinkTarget::Demodulation(demod.id),
            linked_at: time.end,
        })?;
    }
    Ok(demod.id)
}

/// Writes the session's records. See the [module docs](self).
pub fn write_session(
    repo: &mut Repository,
    session: &AnalogSession,
    ctx: &RecordContext,
) -> Result<WrittenSession, RepoError> {
    let time = session.time_range();
    let pi = session.rds().and_then(|r| r.pi);
    // T-962: only a committed PI is identity-bearing. A provisional one takes the no-identity
    // path below — recorded, shown, never resolved against or confirmed on.
    let committed = pi.filter(pi_is_identity);
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
    if let Some(pi) = committed {
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
        if committed.is_none() {
            // With a committed PI the sighting carried it (input: the demodulation). A
            // provisional PI recorded no sighting, so the classification is appended here.
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
                    // T-962: a label is revisable where a confirm is not (ADR-0022 §1.3), so a
                    // provisional PI may still label — saying so.
                    "pi_provisional": !pi_is_identity(&pi),
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

/// T-971: what a followed station's accumulated RDS rows are written against, and what the
/// follow has written so far. Built from the window's [`WrittenSession`] by [`Self::after`];
/// [`write_follow`] keeps it up to date.
#[derive(Clone, Debug, PartialEq)]
pub struct FollowRecord {
    /// The window's Demodulation: the follow extends it, so its rows name it as their source.
    pub demodulation_id: DemodulationId,
    /// Recording replayed, if offline.
    pub recording_ref: Option<RecordingId>,
    /// The station's emitter, once known.
    pub emitter_id: Option<EmitterId>,
    /// Producer key of the window's sighting (as [`RecordContext::counted_as`]).
    pub counted_as: Option<&'static str>,
    /// An identity-bearing sighting of the PI has been recorded (by the window or the follow).
    pub identity_claimed: bool,
    /// The station label last written (text and annotation).
    pub label: Option<(String, AnnotationId)>,
}

impl FollowRecord {
    /// The follow of the window `written` recorded for `session` under `ctx`.
    pub fn after(session: &AnalogSession, written: &WrittenSession, ctx: &RecordContext) -> Self {
        Self {
            demodulation_id: written.demodulation_id,
            recording_ref: ctx.recording_ref,
            emitter_id: written.emitter_id,
            counted_as: ctx.counted_as,
            identity_claimed: session
                .rds()
                .and_then(|r| r.pi)
                .is_some_and(|p| pi_is_identity(&p)),
            label: written.label.clone().zip(written.label_id),
        }
    }
}

/// T-971: one follow checkpoint's writes.
#[derive(Clone, Debug, PartialEq)]
pub struct FollowWrite {
    /// The `rds-pi` row.
    pub decode_id: DecodeId,
    /// The PI's identity was claimed by this checkpoint (it committed during the follow).
    pub identity_claimed: bool,
    /// A new label was written.
    pub label_id: Option<AnnotationId>,
}

/// T-971: writes a followed station's **accumulated RDS field view** as one `rds-pi` Decode row.
///
/// `rds` is the station's report over the window *and* everything followed since (the decoder
/// never restarts, so nothing is decoded twice); `span` is the capture time it covers, window
/// start to the newest sample. The row carries exactly the metadata the window's `rds-pi` row
/// does — PI and its vote, PS (most sent, current, sequence), RadioText, PTY, AF, CT, error
/// rates — plus `follow: {span_start, span_end}`, and is stamped at `span.end`, so
/// `GET /api/inventory/{id}/decode` (latest row per `(decoder, frame_model)`) serves the station's
/// fields as they stand and a windowed query serves them as they stood then. It is **not** a
/// packet: no per-frame row is written for the followed part.
///
/// Identity follows the window's rules (T-962): the row names the PI only once it is committed;
/// a PI that commits *during* the follow is claimed then — the same identity-bearing sighting
/// [`write_session`] records for a committed window, offered as a re-measurement of the window's
/// sighting — and the emitter it resolves to becomes the follow's. The row is linked to the
/// emitter. A label is written when the most-sent PS differs from the one last written.
///
/// `None` (nothing written) when the report has no PI.
pub fn write_follow(
    repo: &mut Repository,
    session: &AnalogSession,
    rds: &RdsReport,
    record: &mut FollowRecord,
    span: TimeRange,
) -> Result<Option<FollowWrite>, RepoError> {
    let Some(pi) = rds.pi else {
        return Ok(None);
    };
    let committed = pi_is_identity(&pi);
    let identity = DecodedIdentity {
        scheme: IdentityScheme::RdsPi,
        value: pi.hex(),
    };
    let mut claimed_now = false;
    if committed && !record.identity_claimed {
        let key = match (record.counted_as, record.emitter_id) {
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
        let family = session.mode.mode.as_str();
        let bandwidth = session.params.obw99_hz.value().unwrap_or(200e3);
        let sighting = Sighting {
            source: LinkTarget::Demodulation(record.demodulation_id),
            seen: span,
            count: 1,
            f_center_hz: session.rf_center_hz,
            bandwidth_hz: bandwidth,
            fingerprint: Some(Fingerprint {
                family: Some(family.into()),
                ..Fingerprint::new(session.rf_center_hz, bandwidth)
            }),
            identity: Some(IdentityClaim {
                identity: identity.clone(),
                content_class: ContentClass::Unrestricted,
            }),
            context: record.emitter_id,
            classification: None,
            tags: Vec::new(),
        };
        let r = repo.record_sighting_measured(&sighting, &MeasurementKey::new(key), None)?;
        record.emitter_id = Some(r.emitter_id);
        record.identity_claimed = true;
        claimed_now = true;
    }
    let mut metadata = rds_pi_metadata(rds, &pi);
    metadata["follow"] = json!({
        "span_start": span.start.as_unix_nanos() as f64 / 1e9,
        "span_end": span.end.as_unix_nanos() as f64 / 1e9,
    });
    let mut row = rds_row(
        record.demodulation_id,
        "rds-pi",
        metadata,
        committed.then_some(identity),
        span.end,
    );
    row.recording_ref = record.recording_ref;
    repo.insert_decode(&row)?;
    let mut label_id = None;
    if let Some(eid) = record.emitter_id {
        repo.link_emitter(&EmitterLink {
            emitter_id: eid,
            target: LinkTarget::Decode(row.id),
            linked_at: span.end,
        })?;
        let text = rds
            .ps()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        if let Some(text) = text
            && record.label.as_ref().is_none_or(|(held, _)| *held != text)
        {
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
                    "pi_provisional": !committed,
                    "ps_frames": rds.ps_frames,
                    "decodes": [row.id],
                }),
                content: None,
                confidence: if frames > 0 {
                    f64::from(top) / f64::from(frames)
                } else {
                    0.0
                },
                supersedes: record.label.as_ref().map(|(_, id)| *id),
                content_class: ContentClass::Unrestricted,
                t: span.end,
                exported: false,
            };
            repo.insert_annotation(&a)?;
            record.label = Some((text, a.id));
            label_id = Some(a.id);
        }
    }
    Ok(Some(FollowWrite {
        decode_id: row.id,
        identity_claimed: claimed_now,
        label_id,
    }))
}
