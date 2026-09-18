//! T-219 (C40, ADR-0015 §11.4): storage and resolution for signal relationships — a Confirmed
//! entry suppressing overlapping candidates, overlapping candidates competing, and geometric
//! receiver artifacts attributed to their source.
//!
//! **Append-only, keyed by emitter, never a mutation or a delete.** Every claim is a row in
//! `emitter_relation` (migration 0008) carrying its reasoning; a revocation is another row with
//! `active = 0`. The losing candidate's own row, detections, tracks, links and history are
//! untouched, so later evidence revives it. Rules and thresholds: [`crate::relate`].
//!
//! **T-369: and overlap itself is an error signal.** The three rules above rank hypotheses against
//! each other, and all of them are gated by [`crate::relate::bands_compete`] at 60 % of *both*
//! bands — which is blind to the two geometries that actually stack boxes on a waterfall (a narrow
//! box inside a wide one, and a staircase of offset boxes each overlapping the next by less than
//! 60 %). Those pairs fell through every rule in silence and were served side by side. So a fourth
//! stage ([`reanalyse_region`]) treats a surviving overlap in **time and frequency** as proof the
//! analysis is wrong and re-analyses the whole region against the measured bands of the detections
//! behind it, rather than ranking one box above another. It resolves to one emission or records the
//! region contested; it never merges past [`crate::relate::distinguishing_evidence`]; and it is
//! bounded by [`crate::relate::REGION_MAX_ROUNDS`], because it runs on a live serving path.
//!
//! **Cost.** [`Repository::resolve_overlaps`] runs on the neighbourhood of one emitter: its
//! overlapping live rows ([`MAX_NEIGHBOURS`]), the strongest confirmed rows as artifact sources
//! ([`MAX_ARTIFACT_SOURCES`]), and the newest linked detections per row
//! ([`MAX_EVIDENCE_DETECTIONS`]) for level, trust and tuning history. All bounded, all indexed.

use rusqlite::{Connection, OptionalExtension, params};

use super::cluster::{live_id, observation_spans};
use super::{RepoError, Repository, blob, lifecycle};
use crate::cluster::{Fingerprint, Tolerances};
use crate::detection::DetectionFlags;
use crate::emitter::DecodedIdentity;
use crate::ids::EmitterId;
use crate::region::{FreqRange, Region, TimeRange};
use crate::relate::{
    ARTIFACT_SOURCE_MIN_SNR_DB, ArtifactKind, ArtifactPrediction, ArtifactSource, EmitterRelation,
    REGION_MAX_ROUNDS, ReceiveChain, RelationAuthor, RelationClaim, RelationKind, RowEvidence,
    TunedLo, bands_compete, boxes_overlap, center_uncertainty_hz, distinguishing_evidence, modes,
    overlap_fraction, predict_artifacts, present_only_with,
};
use crate::time::Timestamp;

/// Overlapping live rows considered around one emitter.
pub const MAX_NEIGHBOURS: usize = 32;

/// Confirmed rows offered to the geometric predictor as artifact sources, strongest first.
pub const MAX_ARTIFACT_SOURCES: usize = 32;

/// Newest linked detections read per row for level, trust and tuning history.
pub const MAX_EVIDENCE_DETECTIONS: usize = 256;

/// Distinct (receive chain, tuning centre) pairs kept per row.
const MAX_TUNED_LO: usize = 16;

/// Candidates looked at near each frequency this emitter's own mechanisms predict, so a station
/// confirmed *after* its image was catalogued still attributes it.
const MAX_PREDICTED_TARGETS: usize = 8;

/// T-369: measured detection bands read per row when a region is re-analysed.
const MAX_REGION_BANDS: usize = 256;

/// T-369: the marker on a contested verdict row's `detail`, so the bound can count them apart from
/// the ordinary revocations [`revoke_kind`] writes (both are `active = 0` rows).
const CONTESTED_VERDICT: &str = "contested";

/// T-369: the marker on a claim the region re-analysis made, so the ranking stages above it do not
/// revoke it for failing *their* pairwise test. See [`is_region_claim`].
const ONE_EMISSION_VERDICT: &str = "one-emission";

/// T-369: the measured bands of an emitter's linked detections, newest first. Reaches detections
/// exactly like [`EMITTER_DETECTION_EVIDENCE_SQL`] — through the currently-linked tracks, or linked
/// directly. `?1` is the emitter id, `?2` the row cap.
const EMITTER_DETECTION_BANDS_SQL: &str = "\
     SELECT f_lo, f_hi FROM ( \
       SELECT d.f_lo AS f_lo, d.f_hi AS f_hi, d.t_start AS t_start \
       FROM emitter_link el \
       JOIN track_detection td ON td.track_id = el.target_id \
       JOIN detection d ON d.detection_id = td.detection_id \
       WHERE el.emitter_id = ?1 AND el.target_kind = 'track' AND el.superseded_by IS NULL \
       UNION ALL \
       SELECT d.f_lo AS f_lo, d.f_hi AS f_hi, d.t_start AS t_start \
       FROM emitter_link el \
       JOIN detection d ON d.detection_id = el.target_id \
       WHERE el.emitter_id = ?1 AND el.target_kind = 'detection' AND el.superseded_by IS NULL \
     ) ORDER BY t_start DESC LIMIT ?2";

/// T-369: how many contested verdicts this row already carries, for the re-analysis bound.
const CONTESTED_ROUNDS_SQL: &str = "\
     SELECT COUNT(*) FROM emitter_relation \
     WHERE emitter_id = ?1 AND active = 0 AND json_extract(detail, '$.verdict') = ?2";

const ANY_TIME: TimeRange = TimeRange::new(
    Timestamp::from_unix_nanos(i64::MIN),
    Timestamp::from_unix_nanos(i64::MAX),
);

/// The newest linked detections of an emitter with their level, −3 dB width, suspect flags and the
/// tuning centre they were measured under — together with the **receive chain** that tuned it
/// (T-302). The provenance join is already here for the LO; reading `device_id` and `antenna_port`
/// out of the same row is what stops one front end's tuning from explaining another's measurement.
/// The emitter table holds no device (a row can be seen by several), so the chain can only come
/// through this detection → provenance join.
///
/// Reaches detections exactly like `EMITTER_LATEST_DETECTION_SQL` (`repo/inventory.rs`): through
/// the emitter's currently-linked tracks, or linked directly. Parameter `?1` is the emitter id,
/// `?2` the row cap.
const EMITTER_DETECTION_EVIDENCE_SQL: &str = "\
     SELECT snr_peak, peak_dbfs, xdb_bw, flags, lo, device_id, antenna_port FROM ( \
       SELECT d.snr_peak AS snr_peak, d.peak_dbfs AS peak_dbfs, d.xdb_bw AS xdb_bw, \
              d.flags AS flags, d.t_start AS t_start, \
              json_extract(p.canonical, '$.tune.center_hz') AS lo, \
              p.device_id AS device_id, \
              json_extract(p.canonical, '$.antenna_port') AS antenna_port \
       FROM emitter_link el \
       JOIN track_detection td ON td.track_id = el.target_id \
       JOIN detection d ON d.detection_id = td.detection_id \
       JOIN provenance p ON p.provenance_id = d.provenance_id \
       WHERE el.emitter_id = ?1 AND el.target_kind = 'track' AND el.superseded_by IS NULL \
       UNION ALL \
       SELECT d.snr_peak AS snr_peak, d.peak_dbfs AS peak_dbfs, d.xdb_bw AS xdb_bw, \
              d.flags AS flags, d.t_start AS t_start, \
              json_extract(p.canonical, '$.tune.center_hz') AS lo, \
              p.device_id AS device_id, \
              json_extract(p.canonical, '$.antenna_port') AS antenna_port \
       FROM emitter_link el \
       JOIN detection d ON d.detection_id = el.target_id \
       JOIN provenance p ON p.provenance_id = d.provenance_id \
       WHERE el.emitter_id = ?1 AND el.target_kind = 'detection' AND el.superseded_by IS NULL \
     ) ORDER BY t_start DESC LIMIT ?2";

/// The current relation for each `(emitter, source, kind)` triple of one emitter: the highest row
/// id wins, and it counts only while `active = 1`. Parameter `?1` is the emitter id.
pub(super) const CURRENT_RELATION_SQL: &str = "\
     SELECT relation_id, emitter_id, source_id, kind, artifact_kind, active, t, author, actor, \
            reason, score, detail \
     FROM emitter_relation r \
     WHERE r.emitter_id = ?1 AND r.active = 1 \
       AND r.relation_id = (SELECT max(r2.relation_id) FROM emitter_relation r2 \
                            WHERE r2.emitter_id = r.emitter_id AND r2.kind = r.kind \
                              AND r2.source_id = r.source_id) \
     ORDER BY r.relation_id";

/// Whether an emitter currently defers to another row (the inventory's default hide predicate),
/// for use inside a larger `WHERE`. Correlates on `emitter.emitter_id`.
///
/// It mirrors [`read_relations`]: a claim counts only while the row it points at is still a live,
/// undeleted row *other than* this one. Without that, a row stays hidden after its host is merged
/// away or deleted — the inventory would lose a real signal to a claim that no longer names
/// anything. Merges flatten `merged_into`, so one hop resolves it; a deeper chain (corrupt data)
/// fails open and the row is listed.
pub(super) const DEFERS_SQL: &str = "\
     EXISTS (SELECT 1 FROM emitter_relation r \
             JOIN emitter src ON src.emitter_id = r.source_id \
             LEFT JOIN emitter live ON live.emitter_id = src.merged_into \
             WHERE r.emitter_id = emitter.emitter_id AND r.active = 1 \
               AND r.relation_id = (SELECT max(r2.relation_id) FROM emitter_relation r2 \
                                    WHERE r2.emitter_id = r.emitter_id AND r2.kind = r.kind \
                                      AND r2.source_id = r.source_id) \
               AND (src.merged_into IS NULL \
                    OR (live.emitter_id IS NOT NULL AND live.merged_into IS NULL)) \
               AND coalesce(live.lifecycle_state, src.lifecycle_state) != 'deleted' \
               AND coalesce(live.emitter_id, src.emitter_id) != emitter.emitter_id)";

fn kind_from(text: &str) -> Result<RelationKind, RepoError> {
    Ok(match text {
        "suppressed-by" => RelationKind::SuppressedBy,
        "duplicate-of" => RelationKind::DuplicateOf,
        "artifact-of" => RelationKind::ArtifactOf,
        other => {
            return Err(RepoError::Invalid(format!(
                "unknown relation kind {other:?}"
            )));
        }
    })
}

fn artifact_from(text: &str) -> Result<ArtifactKind, RepoError> {
    Ok(match text {
        "image" => ArtifactKind::Image,
        "harmonic" => ArtifactKind::Harmonic,
        "intermod" => ArtifactKind::Intermod,
        other => {
            return Err(RepoError::Invalid(format!(
                "unknown artifact kind {other:?}"
            )));
        }
    })
}

fn author_from(text: &str) -> Result<RelationAuthor, RepoError> {
    Ok(match text {
        "system" => RelationAuthor::System,
        "user" => RelationAuthor::User,
        other => {
            return Err(RepoError::Invalid(format!(
                "unknown relation author {other:?}"
            )));
        }
    })
}

type RelationRaw = (
    i64,
    [u8; 16],
    [u8; 16],
    String,
    Option<String>,
    bool,
    i64,
    String,
    String,
    String,
    Option<f64>,
    Option<String>,
);

fn relation_from(raw: RelationRaw) -> Result<EmitterRelation, RepoError> {
    let (relation_id, emitter, source, kind, artifact, active, t, author, actor, reason, score, d) =
        raw;
    Ok(EmitterRelation {
        relation_id,
        emitter_id: EmitterId::from_uuid(uuid::Uuid::from_bytes(emitter)),
        source_id: EmitterId::from_uuid(uuid::Uuid::from_bytes(source)),
        kind: kind_from(&kind)?,
        artifact: artifact.as_deref().map(artifact_from).transpose()?,
        active,
        t: Timestamp::from_unix_nanos(t),
        author: author_from(&author)?,
        actor,
        reason,
        score,
        detail: d.map(|s| serde_json::from_str(&s)).transpose()?,
    })
}

fn read_relations(
    conn: &Connection,
    sql: &str,
    id: EmitterId,
) -> Result<Vec<EmitterRelation>, RepoError> {
    let raw: Vec<RelationRaw> = {
        let mut stmt = conn.prepare_cached(sql)?;
        stmt.query_map([blob(id)], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                r.get(11)?,
            ))
        })?
        .collect::<Result<_, _>>()?
    };
    let mut out = Vec::with_capacity(raw.len());
    for row in raw {
        let rel = relation_from(row)?;
        // A merge may have made the two ends one row; such a claim says nothing.
        let (a, b) = (
            live_id(conn, rel.emitter_id)?,
            live_id(conn, rel.source_id)?,
        );
        if a.is_some() && a == b {
            continue;
        }
        out.push(rel);
    }
    Ok(out)
}

fn insert_relation(conn: &Connection, claim: &RelationClaim) -> Result<EmitterRelation, RepoError> {
    if claim.emitter_id == claim.source_id {
        return Err(RepoError::Invalid(
            "a relation cannot name the same emitter on both sides".into(),
        ));
    }
    if claim.artifact.is_some() != (claim.kind == RelationKind::ArtifactOf) {
        return Err(RepoError::Invalid(
            "artifact-of relations carry an artifact kind and no other kind does".into(),
        ));
    }
    let detail = claim
        .detail
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    conn.prepare_cached(
        "INSERT INTO emitter_relation (emitter_id, source_id, kind, artifact_kind, active, t, \
         author, actor, reason, score, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?
    .execute(params![
        blob(claim.emitter_id),
        blob(claim.source_id),
        claim.kind.as_str(),
        claim.artifact.map(ArtifactKind::as_str),
        claim.active,
        claim.t.as_unix_nanos(),
        claim.author.as_str(),
        claim.actor,
        claim.reason,
        claim.score.filter(|v| v.is_finite()),
        detail,
    ])?;
    let relation_id = conn.last_insert_rowid();
    Ok(EmitterRelation {
        relation_id,
        emitter_id: claim.emitter_id,
        source_id: claim.source_id,
        kind: claim.kind,
        artifact: claim.artifact,
        active: claim.active,
        t: claim.t,
        author: claim.author,
        actor: claim.actor.clone(),
        reason: claim.reason.clone(),
        score: claim.score.filter(|v| v.is_finite()),
        detail: claim.detail.clone(),
    })
}

/// The live, listed emitter `id` resolves to, or `None` when it is merged away or deleted.
fn listed(conn: &Connection, id: EmitterId) -> Result<Option<EmitterId>, RepoError> {
    match live_id(conn, id)? {
        Some(live) if !lifecycle::is_deleted(conn, live)? => Ok(Some(live)),
        _ => Ok(None),
    }
}

/// Everything the rules need about one live row (see [`RowEvidence`]).
pub(super) fn evidence(conn: &Connection, id: EmitterId) -> Result<Option<RowEvidence>, RepoError> {
    type Raw = (
        f64,
        f64,
        i64,
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
    );
    let Some(raw): Option<Raw> = conn
        .prepare_cached(
            "SELECT f_center, bandwidth, first_seen, count, fingerprint, identity_scheme, \
             identity_value, lifecycle_state FROM emitter \
             WHERE emitter_id = ?1 AND merged_into IS NULL",
        )?
        .query_row([blob(id)], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })
        .optional()?
    else {
        return Ok(None);
    };
    let (f_center, bandwidth, first_seen_ns, count, fp, scheme, value, state) = raw;
    if state == "deleted" {
        return Ok(None);
    }
    let identity = match (scheme, value) {
        (Some(s), Some(v)) => Some(DecodedIdentity {
            scheme: s.parse().map_err(RepoError::Invalid)?,
            value: v,
        }),
        _ => None,
    };
    let fingerprint = fp
        .map(|s| serde_json::from_str::<serde_json::Value>(&s))
        .transpose()?
        .as_ref()
        .and_then(Fingerprint::from_value);

    type Det = (
        f64,
        f64,
        Option<f64>,
        i64,
        Option<f64>,
        String,
        Option<String>,
    );
    let dets: Vec<Det> = {
        let mut stmt = conn.prepare_cached(EMITTER_DETECTION_EVIDENCE_SQL)?;
        stmt.query_map(params![blob(id), MAX_EVIDENCE_DETECTIONS as i64], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })?
        .collect::<Result<_, _>>()?
    };
    let (mut snr_db, mut peak_dbfs, mut xdb_bandwidth_hz) = (None, None, None);
    let (mut suspect, mut tuned_lo) = (0usize, Vec::<TunedLo>::new());
    let (mut image_flagged, mut imd_flagged, mut spur_flagged) = (false, false, false);
    for (i, (snr, peak, xdb, flags, lo, device, port)) in dets.iter().enumerate() {
        if i == 0 {
            snr_db = Some(*snr);
            peak_dbfs = Some(*peak);
        }
        if xdb_bandwidth_hz.is_none() {
            xdb_bandwidth_hz = xdb.filter(|v| v.is_finite() && *v > 0.0);
        }
        let f = DetectionFlags::from_bits(u32::try_from(*flags).unwrap_or(0));
        if f.clipped || f.spur_candidate || f.image_candidate || f.suspect_imd || f.compressed {
            suspect += 1;
        }
        // Which mechanism the measurement itself suspects, for [`RowEvidence::corroborates`].
        image_flagged |= f.image_candidate;
        imd_flagged |= f.suspect_imd;
        spur_flagged |= f.spur_candidate;
        // The tuning centre is never recorded without the receive chain that tuned it (T-302): an
        // image belongs to one mixer, and a bare frequency here would let one front end's tuning
        // mirror another front end's emitter. Chains are compared exactly when deduplicating, so
        // every chain that measured this row keeps at least one entry.
        if let Some(lo) = lo.filter(|v| v.is_finite()) {
            let chain = ReceiveChain {
                device_id: device.clone(),
                antenna_port: port.clone(),
            };
            if tuned_lo.len() < MAX_TUNED_LO
                && !tuned_lo
                    .iter()
                    .any(|t| t.chain == chain && (t.lo_hz - lo).abs() < 1.0)
            {
                tuned_lo.push(TunedLo { chain, lo_hz: lo });
            }
        }
    }
    let suspect_fraction = if dets.is_empty() {
        0.0
    } else {
        suspect as f64 / dets.len() as f64
    };
    Ok(Some(RowEvidence {
        emitter_id: id,
        f_center_hz: f_center,
        bandwidth_hz: bandwidth,
        xdb_bandwidth_hz,
        confirmed: state == "confirmed",
        snr_db,
        peak_dbfs,
        duty_cycle: fingerprint.as_ref().and_then(|f| f.duty_cycle),
        suspect_fraction,
        image_flagged,
        imd_flagged,
        spur_flagged,
        identity,
        fingerprint,
        tuned_lo,
        spans: observation_spans(conn, id)?
            .into_iter()
            .map(|s| (s.t0, s.t1))
            .collect(),
        count,
        first_seen_ns,
    }))
}

/// Live, listed emitters whose occupied band overlaps `freq`, most recently seen first.
fn overlapping(
    conn: &Connection,
    freq: FreqRange,
    except: EmitterId,
    limit: usize,
) -> Result<Vec<EmitterId>, RepoError> {
    let b = super::region_bounds(conn, "emitter", &Region::new(freq, ANY_TIME))?;
    let mut stmt = conn.prepare_cached(
        "SELECT emitter_id FROM emitter \
         WHERE f_lo BETWEEN ?1 AND ?2 AND f_hi >= ?3 AND merged_into IS NULL \
           AND lifecycle_state != 'deleted' AND emitter_id != ?4 \
         ORDER BY last_seen DESC, emitter_id LIMIT ?5",
    )?;
    let ids: Vec<[u8; 16]> = stmt
        .query_map(
            params![b.f_lo_min, b.hi, b.lo, blob(except), limit as i64],
            |r| r.get(0),
        )?
        .collect::<Result<_, _>>()?;
    Ok(ids
        .into_iter()
        .map(|b| EmitterId::from_uuid(uuid::Uuid::from_bytes(b)))
        .collect())
}

/// The strong confirmed rows offered to the geometric predictor as artifact sources.
fn artifact_sources(
    conn: &Connection,
    limit: usize,
) -> Result<Vec<(ArtifactSource, RowEvidence)>, RepoError> {
    let ids: Vec<[u8; 16]> = {
        let mut stmt = conn.prepare_cached(
            "SELECT emitter_id FROM emitter WHERE merged_into IS NULL \
             AND lifecycle_state = 'confirmed' ORDER BY last_seen DESC, emitter_id LIMIT ?1",
        )?;
        stmt.query_map([limit as i64], |r| r.get(0))?
            .collect::<Result<_, _>>()?
    };
    let mut out = Vec::new();
    for raw in ids {
        let id = EmitterId::from_uuid(uuid::Uuid::from_bytes(raw));
        let Some(ev) = evidence(conn, id)? else {
            continue;
        };
        // A weak emitter drives no visible image, harmonic or intermod product.
        let (Some(snr), Some(level)) = (ev.snr_db, ev.peak_dbfs) else {
            continue;
        };
        if snr < ARTIFACT_SOURCE_MIN_SNR_DB {
            continue;
        }
        // Which receive chains actually measured this source (T-302). No device predicate belongs
        // on the query above: a source may legitimately be seen by several front ends, and it is
        // the *pairing* with the row being explained that has to share a chain, not the candidate
        // list.
        let chains = ev.chains();
        out.push((
            ArtifactSource {
                emitter_id: id,
                f_center_hz: ev.f_center_hz,
                bandwidth_hz: ev.bandwidth_hz,
                level_dbfs: level,
                chains,
            },
            ev,
        ));
    }
    Ok(out)
}

/// What one call to [`Repository::resolve_overlaps`] recorded. Empty is the normal case.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OverlapOutcome {
    /// Candidates newly recorded as suppressed by a Confirmed entry.
    pub suppressed: Vec<EmitterRelation>,
    /// Candidates newly recorded as the weaker of a duplicate group.
    pub duplicates: Vec<EmitterRelation>,
    /// Candidates newly attributed to the source whose image / harmonic / intermod they are.
    pub artifacts: Vec<EmitterRelation>,
    /// Standing claims revoked because the evidence changed.
    pub revoked: Vec<EmitterRelation>,
    /// T-369: rows whose overlap the region re-analysis could **not** resolve. Both rows stay
    /// shown — a contested verdict never hides anything — and the record says what was tried and
    /// what blocked a merge. Bounded by [`crate::relate::REGION_MAX_ROUNDS`] per row.
    pub contested: Vec<EmitterRelation>,
}

impl OverlapOutcome {
    /// Nothing was recorded.
    pub fn is_empty(&self) -> bool {
        self.suppressed.is_empty()
            && self.duplicates.is_empty()
            && self.artifacts.is_empty()
            && self.revoked.is_empty()
            && self.contested.is_empty()
    }
}

/// Claims `emitter → source` unless that exact claim already stands; revokes any standing claim of
/// the same kind pointing somewhere else. Returns `(claimed, revoked)`.
fn claim(
    conn: &Connection,
    standing: &[EmitterRelation],
    claim: RelationClaim,
) -> Result<(Option<EmitterRelation>, Vec<EmitterRelation>), RepoError> {
    let mut revoked = Vec::new();
    let mut already = false;
    for r in standing
        .iter()
        .filter(|r| r.emitter_id == claim.emitter_id && r.kind == claim.kind)
    {
        if r.source_id == claim.source_id && r.artifact == claim.artifact {
            already = true;
            continue;
        }
        revoked.push(insert_relation(
            conn,
            &RelationClaim {
                active: false,
                reason: format!(
                    "superseded: this row now defers to emitter {} instead",
                    claim.source_id
                ),
                score: None,
                detail: None,
                ..claim.clone()
            }
            .with_source(r.source_id, r.artifact),
        )?);
    }
    if already {
        return Ok((None, revoked));
    }
    Ok((Some(insert_relation(conn, &claim)?), revoked))
}

impl RelationClaim {
    fn with_source(mut self, source: EmitterId, artifact: Option<ArtifactKind>) -> Self {
        self.source_id = source;
        self.artifact = artifact;
        self
    }
}

/// Revokes every standing claim of `kind` on `emitter` (it no longer defers).
fn revoke_kind(
    conn: &Connection,
    standing: &[EmitterRelation],
    emitter: EmitterId,
    kind: RelationKind,
    actor: &str,
    t: Timestamp,
    why: &str,
) -> Result<Vec<EmitterRelation>, RepoError> {
    let mut out = Vec::new();
    for r in standing
        .iter()
        // T-369: never a claim the region re-analysis authored. `why` here is always a pairwise
        // test ("no longer an undistinguished overlap of the row shown"), and a region claim is
        // made precisely when that pairwise test fails and the region's own measurements say the
        // rows are one emission anyway. Stage 4 revokes its own ([`revoke_region_claim`]).
        .filter(|r| r.emitter_id == emitter && r.kind == kind && !is_region_claim(r))
    {
        out.push(insert_relation(
            conn,
            &RelationClaim {
                emitter_id: emitter,
                source_id: r.source_id,
                kind,
                artifact: r.artifact,
                active: false,
                t,
                author: RelationAuthor::System,
                actor: actor.to_owned(),
                reason: why.to_owned(),
                score: None,
                detail: None,
            },
        )?);
    }
    Ok(out)
}

fn artifact_detail(
    p: &ArtifactPrediction,
    target: &RowEvidence,
    corroborating_flag: &str,
) -> serde_json::Value {
    serde_json::json!({
        "kind": p.kind.as_str(),
        "measured_bandwidth_hz": target.bandwidth_hz,
        "corroborating_flag": corroborating_flag,
        "order": p.order,
        "a": p.a,
        "b": p.b,
        "sign": p.sign,
        "lo_hz": p.lo_hz,
        "predicted_hz": p.predicted_hz,
        "predicted_bandwidth_hz": p.predicted_bandwidth_hz,
        "measured_hz": target.f_center_hz,
        "error_hz": p.error_hz,
        "tolerance_hz": p.tolerance_hz,
        "suppression_db": p.suppression_db,
    })
}

fn rank_detail(ev: &RowEvidence) -> serde_json::Value {
    serde_json::json!({
        "proxy": "snr x duty x trust",
        "snr_db": ev.snr_db,
        "duty_cycle": ev.duty_cycle,
        "suspect_fraction": ev.suspect_fraction,
        "score": ev.rank(),
    })
}

/// Sort key that picks the shown row of a duplicate group: strongest by the rank proxy, then the
/// most-counted, then the first seen, then the id — deterministic on every replay.
fn winner_key(ev: &RowEvidence) -> (f64, i64, std::cmp::Reverse<i64>, EmitterId) {
    (
        ev.rank(),
        ev.count,
        std::cmp::Reverse(ev.first_seen_ns),
        ev.emitter_id,
    )
}

fn better(a: &RowEvidence, b: &RowEvidence) -> bool {
    let (ka, kb) = (winner_key(a), winner_key(b));
    match ka.0.total_cmp(&kb.0) {
        std::cmp::Ordering::Equal => (ka.1, ka.2, ka.3) > (kb.1, kb.2, kb.3),
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
    }
}

impl Repository {
    /// The relationships currently in force for `id` (merged ids resolve to their survivor):
    /// which row it defers to and why. Empty is the normal case.
    pub fn emitter_relations(&self, id: EmitterId) -> Result<Vec<EmitterRelation>, RepoError> {
        let id = self.live_emitter_id(id)?;
        read_relations(&self.conn, CURRENT_RELATION_SQL, id)
    }

    /// Every relationship ever claimed or revoked for `id`, oldest first — the audit behind a
    /// suppression, duplicate or artifact claim. Nothing here is ever deleted.
    pub fn emitter_relation_history(
        &self,
        id: EmitterId,
    ) -> Result<Vec<EmitterRelation>, RepoError> {
        let id = self.live_emitter_id(id)?;
        read_relations(
            &self.conn,
            "SELECT relation_id, emitter_id, source_id, kind, artifact_kind, active, t, author, \
             actor, reason, score, detail FROM emitter_relation WHERE emitter_id = ?1 \
             ORDER BY relation_id",
            id,
        )
    }

    /// Records one relationship claim or revocation (the API's user override, and the rules'
    /// writer). Append-only: a revocation is a new row, never an edit.
    pub fn record_emitter_relation(
        &mut self,
        claim: &RelationClaim,
    ) -> Result<EmitterRelation, RepoError> {
        let tx = self.write_tx()?;
        let r = insert_relation(&tx, claim)?;
        tx.commit()?;
        Ok(r)
    }

    /// T-219: resolves the overlapping rows around `id` and records what it finds, in this order —
    /// **artifact, then suppression, then competition** (a receiver artifact is not a duplicate of
    /// the row it overlaps, and a Confirmed entry outranks a candidate contest):
    ///
    /// 1. **Artifact** ([`crate::relate::predict_artifacts`]): a candidate landing on a predicted
    ///    image `2·f_LO − f`, harmonic `n·f` or intermod `a·f1 ± b·f2` of a strong Confirmed
    ///    emitter, at a level consistent with the mechanism and present only while that source is,
    ///    is recorded [`RelationKind::ArtifactOf`] it with the arithmetic disclosed.
    /// 2. **Suppression**: a candidate whose band overlaps a Confirmed entry's by at least
    ///    [`crate::relate::OVERLAP_MIN_FRACTION`] of **both** the narrower and the wider band
    ///    ([`crate::relate::bands_compete`]), with no
    ///    [`crate::relate::distinguishing_evidence`], is recorded [`RelationKind::SuppressedBy`] it.
    ///    A narrow emission inside a wide one therefore never disappears into its host.
    /// 3. **Competition**: the remaining overlapping candidates form a duplicate group ranked by
    ///    the [`crate::relate::rank_score`] proxy; the strongest is shown and the others are
    ///    recorded [`RelationKind::DuplicateOf`] it.
    ///
    /// Every claim is append-only and reversible: raw detections, tracks and history are always
    /// kept, a losing row is never mutated or deleted, and a claim whose evidence no longer holds
    /// is revoked by appending a row (reported in [`OverlapOutcome::revoked`]).
    pub fn resolve_overlaps(
        &mut self,
        id: EmitterId,
        actor: &str,
        t: Timestamp,
        tol: &Tolerances,
    ) -> Result<OverlapOutcome, RepoError> {
        self.ensure_refined_table()?;
        let tx = self.write_tx()?;
        let out = resolve(&tx, id, actor, t, tol)?;
        tx.commit()?;
        Ok(out)
    }
}

fn resolve(
    conn: &Connection,
    id: EmitterId,
    actor: &str,
    t: Timestamp,
    tol: &Tolerances,
) -> Result<OverlapOutcome, RepoError> {
    let mut out = OverlapOutcome::default();
    let Some(live) = listed(conn, id)? else {
        return Ok(out);
    };
    let Some(here) = evidence(conn, live)? else {
        return Ok(out);
    };

    // The neighbourhood: this row plus the live rows its band overlaps.
    let mut rows = vec![here.clone()];
    for other in overlapping(conn, here.freq(), live, MAX_NEIGHBOURS)? {
        if let Some(ev) = evidence(conn, other)? {
            rows.push(ev);
        }
    }

    // --- 1. Geometric artifact attribution ------------------------------------------------
    let sources = artifact_sources(conn, MAX_ARTIFACT_SOURCES)?;
    // Targets: the neighbourhood, plus the candidates sitting where this row's own mechanisms
    // predict — so a station confirmed *after* its image was catalogued still attributes it.
    let mut targets: Vec<RowEvidence> = rows.iter().filter(|r| !r.confirmed).cloned().collect();
    if here.confirmed
        && here.peak_dbfs.is_some()
        && here.snr_db.is_some_and(|s| s >= ARTIFACT_SOURCE_MIN_SNR_DB)
    {
        let mut predicted: Vec<f64> = here
            .tuned_lo
            .iter()
            .map(|t| 2.0 * t.lo_hz - here.f_center_hz)
            .collect();
        predicted.extend(
            (2..=crate::relate::HARMONIC_MAX_ORDER).map(|n| f64::from(n) * here.f_center_hz),
        );
        for f in predicted.into_iter().filter(|f| f.is_finite() && *f > 0.0) {
            let band = FreqRange::centered(f, here.bandwidth_hz.max(1.0));
            for other in overlapping(conn, band, live, MAX_PREDICTED_TARGETS)? {
                if targets.iter().any(|r| r.emitter_id == other) {
                    continue;
                }
                if let Some(ev) = evidence(conn, other)?.filter(|e| !e.confirmed) {
                    targets.push(ev);
                }
            }
        }
    }
    let mut attributed: Vec<EmitterId> = Vec::new();
    for target in &targets {
        let standing = read_relations(conn, CURRENT_RELATION_SQL, target.emitter_id)?;
        let Some(level) = target.peak_dbfs else {
            continue;
        };
        // A source is never an artifact of itself. The tuning history is the **target's own**: an
        // image exists only at the LO the target was measured under, so unioning in every source's
        // tuning history manufactures mirrors of tunings this row was never seen at (32 sources x
        // 16 remembered LOs each = up to 16 384 image frequencies for one box).
        let usable: Vec<ArtifactSource> = sources
            .iter()
            .filter(|(s, _)| s.emitter_id != target.emitter_id)
            .map(|(s, _)| s.clone())
            .collect();
        let present = |id: EmitterId| {
            sources
                .iter()
                .find(|(s, _)| s.emitter_id == id)
                .is_some_and(|(_, ev)| present_only_with(&target.spans, &ev.spans))
        };
        let hit = predict_artifacts(
            target.f_center_hz,
            target.bandwidth_hz,
            level,
            &usable,
            &target.tuned_lo,
        )
        .into_iter()
        .find_map(|p| {
            // Arithmetic alone is a coincidence. Two further measurements have to agree with it:
            // the front end's own suspect flag for this mechanism, and presence only while every
            // source of the product was present.
            let flag = target.corroborates(p.kind)?;
            let present = present(p.source) && p.second_source.is_none_or(present);
            present.then_some((p, flag))
        });
        match hit {
            Some((p, flag)) => {
                let reason = format!(
                    "receiver artifact of emitter {}, not an independent emission: {} \
                     (bandwidth consistent with the mechanism, and the measurement carries {flag})",
                    p.source,
                    p.arithmetic()
                );
                let (claimed, revoked) = claim(
                    conn,
                    &standing,
                    RelationClaim {
                        emitter_id: target.emitter_id,
                        source_id: p.source,
                        kind: RelationKind::ArtifactOf,
                        artifact: Some(p.kind),
                        active: true,
                        t,
                        author: RelationAuthor::System,
                        actor: actor.to_owned(),
                        reason,
                        score: None,
                        detail: Some(artifact_detail(&p, target, flag)),
                    },
                )?;
                out.artifacts.extend(claimed);
                out.revoked.extend(revoked);
                attributed.push(target.emitter_id);
            }
            None => {
                out.revoked.extend(revoke_kind(
                    conn,
                    &standing,
                    target.emitter_id,
                    RelationKind::ArtifactOf,
                    actor,
                    t,
                    "no image, harmonic or intermodulation mechanism fits this row any more \
                     (centre, bandwidth, level, a measured suspect flag and presence while the \
                     source was on air all have to agree)",
                )?);
            }
        }
    }

    // --- 2. A Confirmed entry suppresses overlapping candidates ---------------------------
    let mut suppressed: Vec<EmitterId> = Vec::new();
    for cand in rows.iter().filter(|r| !r.confirmed) {
        if attributed.contains(&cand.emitter_id) {
            continue;
        }
        let standing = read_relations(conn, CURRENT_RELATION_SQL, cand.emitter_id)?;
        let host = rows
            .iter()
            .filter(|r| r.confirmed && r.emitter_id != cand.emitter_id)
            .filter(|r| bands_compete(r.freq(), cand.freq()))
            .find(|r| distinguishing_evidence(r, cand, tol).is_none());
        match host {
            Some(host) => {
                let reason = format!(
                    "overlaps confirmed emitter {} by {:.0}% of the narrower band with nothing to \
                     tell them apart: almost surely the same emission (detections, tracks and \
                     history kept; reversible)",
                    host.emitter_id,
                    100.0 * overlap_fraction(host.freq(), cand.freq()),
                );
                let (claimed, revoked) = claim(
                    conn,
                    &standing,
                    RelationClaim {
                        emitter_id: cand.emitter_id,
                        source_id: host.emitter_id,
                        kind: RelationKind::SuppressedBy,
                        artifact: None,
                        active: true,
                        t,
                        author: RelationAuthor::System,
                        actor: actor.to_owned(),
                        reason,
                        score: None,
                        detail: None,
                    },
                )?;
                out.suppressed.extend(claimed);
                out.revoked.extend(revoked);
                suppressed.push(cand.emitter_id);
            }
            None => {
                out.revoked.extend(revoke_kind(
                    conn,
                    &standing,
                    cand.emitter_id,
                    RelationKind::SuppressedBy,
                    actor,
                    t,
                    "no confirmed entry overlaps this row without distinguishing evidence any more",
                )?);
            }
        }
    }

    // --- 3. Overlapping candidates compete ------------------------------------------------
    let contenders: Vec<&RowEvidence> = rows
        .iter()
        .filter(|r| {
            !r.confirmed
                && !attributed.contains(&r.emitter_id)
                && !suppressed.contains(&r.emitter_id)
        })
        .collect();
    let top = contenders
        .iter()
        .copied()
        .reduce(|best, r| if better(r, best) { r } else { best });
    for other in top.map(|_| &contenders).into_iter().flatten() {
        let top = top.expect("iterated only when a top row exists");
        let standing = read_relations(conn, CURRENT_RELATION_SQL, other.emitter_id)?;
        if other.emitter_id == top.emitter_id {
            // The shown row never defers.
            out.revoked.extend(revoke_kind(
                conn,
                &standing,
                other.emitter_id,
                RelationKind::DuplicateOf,
                actor,
                t,
                "this row now ranks highest of its overlapping group and is the one shown",
            )?);
            continue;
        }
        let fraction = overlap_fraction(top.freq(), other.freq());
        if !bands_compete(top.freq(), other.freq())
            || distinguishing_evidence(top, other, tol).is_some()
        {
            out.revoked.extend(revoke_kind(
                conn,
                &standing,
                other.emitter_id,
                RelationKind::DuplicateOf,
                actor,
                t,
                "this row is no longer an undistinguished overlap of the row shown",
            )?);
            continue;
        }
        let reason = format!(
            "overlaps emitter {} by {:.0}% of the narrower band with nothing to tell them apart; \
             that row ranks higher on the SNR x duty x trust proxy ({:.3} vs {:.3}), so it is the \
             one shown (this row is kept in full and revives if the evidence changes)",
            top.emitter_id,
            100.0 * fraction,
            top.rank(),
            other.rank(),
        );
        let (claimed, revoked) = claim(
            conn,
            &standing,
            RelationClaim {
                emitter_id: other.emitter_id,
                source_id: top.emitter_id,
                kind: RelationKind::DuplicateOf,
                artifact: None,
                active: true,
                t,
                author: RelationAuthor::System,
                actor: actor.to_owned(),
                reason,
                score: Some(other.rank()),
                detail: Some(serde_json::json!({
                    "shown": rank_detail(top),
                    "this": rank_detail(other),
                    "overlap_fraction": fraction,
                })),
            },
        )?;
        out.duplicates.extend(claimed);
        out.revoked.extend(revoked);
    }

    // --- 4. Overlap is an error signal: re-analyse the region ------------------------------
    reanalyse_region(conn, &rows, live, actor, t, tol, &mut out)?;
    Ok(out)
}

/// Whether this claim was authored by the region re-analysis (stage 4) rather than by the ranking
/// stages above it. The two write the same [`RelationKind::DuplicateOf`] on purpose — the wire
/// shape and everything downstream of it are unchanged — so the `detail` verdict is what tells
/// them apart, and it is what stops stage 3 from revoking a claim stage 4 made on the region's
/// measurements because the *pair* does not pass [`bands_compete`] (which is exactly why stage 4
/// exists).
fn is_region_claim(r: &EmitterRelation) -> bool {
    r.detail
        .as_ref()
        .and_then(|d| d.get("verdict"))
        .and_then(|v| v.as_str())
        == Some(ONE_EMISSION_VERDICT)
}

/// Whether `id` currently defers for a reason **other** than the region re-analysis. Such a row is
/// hidden by a stage above and is not a member of any region; a row deferring by a region claim is
/// still a member of its own region, so that the re-analysis can revoke its own claim when the
/// evidence changes.
fn defers_elsewhere(conn: &Connection, id: EmitterId) -> Result<bool, RepoError> {
    Ok(read_relations(conn, CURRENT_RELATION_SQL, id)?
        .iter()
        .any(|r| !is_region_claim(r)))
}

/// Revokes a standing region claim on `id` (it no longer defers to the region's survivor).
fn revoke_region_claim(
    conn: &Connection,
    id: EmitterId,
    actor: &str,
    t: Timestamp,
    why: &str,
) -> Result<Vec<EmitterRelation>, RepoError> {
    let mut out = Vec::new();
    for r in read_relations(conn, CURRENT_RELATION_SQL, id)?
        .iter()
        .filter(|r| is_region_claim(r))
    {
        out.push(insert_relation(
            conn,
            &RelationClaim {
                emitter_id: id,
                source_id: r.source_id,
                kind: r.kind,
                artifact: r.artifact,
                active: false,
                t,
                author: RelationAuthor::System,
                actor: actor.to_owned(),
                reason: why.to_owned(),
                score: None,
                detail: None,
            },
        )?);
    }
    Ok(out)
}

/// The measured bands of `id`'s linked detections, or its own occupied band when nothing is linked
/// (a row minted from a measurement the link table has not caught up with still has a band).
fn detection_bands(conn: &Connection, row: &RowEvidence) -> Result<Vec<FreqRange>, RepoError> {
    let mut stmt = conn.prepare_cached(EMITTER_DETECTION_BANDS_SQL)?;
    let bands: Vec<FreqRange> = stmt
        .query_map(
            params![blob(row.emitter_id), MAX_REGION_BANDS as i64],
            |r| Ok(FreqRange::new(r.get(0)?, r.get(1)?)),
        )?
        .collect::<Result<_, _>>()?;
    Ok(if bands.is_empty() {
        vec![row.freq()]
    } else {
        bands
    })
}

/// How many contested verdicts `id` already carries (the re-analysis bound).
fn contested_rounds(conn: &Connection, id: EmitterId) -> Result<usize, RepoError> {
    let n: i64 = conn
        .prepare_cached(CONTESTED_ROUNDS_SQL)?
        .query_row(params![blob(id), CONTESTED_VERDICT], |r| r.get(0))?;
    Ok(usize::try_from(n).unwrap_or(usize::MAX))
}

/// Appends one contested verdict: an `active = 0` row, so **nothing is hidden** — both boxes stay
/// listed — carrying what the re-analysis found and what blocked a merge. Bounded: at most
/// [`REGION_MAX_ROUNDS`] per row, after which the region is left contested and not examined again.
#[allow(clippy::too_many_arguments)]
fn contest(
    conn: &Connection,
    row: &RowEvidence,
    against: EmitterId,
    region: FreqRange,
    members: usize,
    modes_found: usize,
    blocked_by: Option<&str>,
    actor: &str,
    t: Timestamp,
) -> Result<Option<EmitterRelation>, RepoError> {
    if contested_rounds(conn, row.emitter_id)? >= REGION_MAX_ROUNDS {
        return Ok(None);
    }
    let why = blocked_by.unwrap_or("the measurements separate into more than one emission");
    let reason = format!(
        "contested region {:.6}-{:.6} MHz: {members} boxes overlap in time and frequency, which \
         two real emissions do not, but the re-analysis could not resolve them - {why}. Both are \
         kept and both stay listed; nothing was merged and nothing was hidden.",
        region.lo_hz / 1e6,
        region.hi_hz / 1e6,
    );
    Ok(Some(insert_relation(
        conn,
        &RelationClaim {
            emitter_id: row.emitter_id,
            source_id: against,
            kind: RelationKind::DuplicateOf,
            artifact: None,
            // Never in force: a contested verdict records a finding, it does not defer a row.
            active: false,
            t,
            author: RelationAuthor::System,
            actor: actor.to_owned(),
            reason,
            score: None,
            detail: Some(serde_json::json!({
                "verdict": CONTESTED_VERDICT,
                "region_lo_hz": region.lo_hz,
                "region_hi_hz": region.hi_hz,
                "members": members,
                "modes": modes_found,
                "blocked_by": blocked_by,
            })),
        },
    )?))
}

/// **T-369: overlap is an error signal, and the resolution is re-analysis, not ranking.**
///
/// Stages 1–3 rank *hypotheses against each other*: a Confirmed entry suppresses what it competes
/// with, and the weaker of two competing candidates defers. [`bands_compete`] gates all of it at
/// 60 % of **both** bands, which is deliberately blind to the two geometries that actually stack
/// boxes on the user's waterfall — a narrow box inside a wide one, and a staircase of offset boxes
/// each overlapping the next by less than 60 %. Those pairs fall through every branch above in
/// silence, which is what "the collapse never happens" looked like from the wire.
///
/// So this stage asks a different question, of the air rather than of the rows: take the connected
/// region of still-shown rows that overlap in **time and frequency** ([`boxes_overlap`]), pull the
/// **measured bands of every detection backing them**, and merge those into contiguous
/// [`modes`]. Then:
///
/// - **One mode** — the region's own measurements never separated, so the boxes are cuts of one
///   emission. The best-supported box ([`RowEvidence::rank`]) is kept and the others defer to it
///   ([`RelationKind::DuplicateOf`]) — but only where [`distinguishing_evidence`] finds nothing to
///   tell them apart, so the guard that keeps two genuinely distinct emitters apart still holds
///   over every claim. A row the guard protects is **contested**, not merged.
/// - **More than one mode** — the detections *do* separate, so there is genuinely more than one
///   thing inside the union and this stage cannot say which row is which. Nothing is merged; every
///   member is contested.
///
/// **Merging is the dangerous direction** (T-233): this stage may bypass `bands_compete`, because
/// that is a test of the rows' geometry and the modes are a measurement, but it never bypasses
/// `distinguishing_evidence`.
///
/// **Termination.** The stage does not recurse: it runs once per `resolve`, over a neighbourhood
/// already bounded to [`MAX_NEIGHBOURS`], and it only ever appends. A claim is idempotent
/// ([`claim`]), so a region that resolves stays resolved without further writes; a region that does
/// not resolve appends at most [`REGION_MAX_ROUNDS`] contested verdicts per row and then stops
/// writing entirely. There is therefore no path by which re-analysis feeds itself.
fn reanalyse_region(
    conn: &Connection,
    rows: &[RowEvidence],
    id: EmitterId,
    actor: &str,
    t: Timestamp,
    tol: &Tolerances,
    out: &mut OverlapOutcome,
) -> Result<(), RepoError> {
    // A row hidden by a stage above is not a member of any region; a row deferring by a *region*
    // claim still is, so this stage can revoke its own claim when the evidence changes.
    let Some(seed) = rows
        .iter()
        .find(|r| r.emitter_id == id)
        .filter(|_| !matches!(defers_elsewhere(conn, id), Ok(true)))
        .cloned()
    else {
        return Ok(());
    };

    // The region: the connected component of `id` under [`boxes_overlap`], grown by querying each
    // member's own overlaps so it is **closed transitively**. Without that closure the region would
    // be whatever happened to overlap the one row a sighting arrived for — so the same three
    // stacked boxes would elect a different survivor depending on which of them was touched, and a
    // live pipeline touches all of them. That is churn, not a resolution. Bounded by
    // [`MAX_NEIGHBOURS`] rows, each lookup indexed.
    let mut members: Vec<RowEvidence> = vec![seed];
    let mut frontier = 0;
    while frontier < members.len() && members.len() < MAX_NEIGHBOURS {
        let cur = members[frontier].clone();
        frontier += 1;
        for other in overlapping(conn, cur.freq(), cur.emitter_id, MAX_NEIGHBOURS)? {
            if members.len() >= MAX_NEIGHBOURS || members.iter().any(|m| m.emitter_id == other) {
                continue;
            }
            if defers_elsewhere(conn, other)? {
                continue;
            }
            if let Some(ev) = evidence(conn, other)?.filter(|e| boxes_overlap(&cur, e)) {
                members.push(ev);
            }
        }
    }
    let members: Vec<&RowEvidence> = members.iter().collect();
    if members.len() < 2 {
        // Not (or no longer) an overlapping region: anything this stage claimed here is revoked.
        out.revoked.extend(revoke_region_claim(
            conn,
            id,
            actor,
            t,
            "this row no longer overlaps another in time and frequency",
        )?);
        return Ok(());
    }
    let region = FreqRange::new(
        members
            .iter()
            .map(|m| m.freq().lo_hz)
            .fold(f64::INFINITY, f64::min),
        members
            .iter()
            .map(|m| m.freq().hi_hz)
            .fold(f64::NEG_INFINITY, f64::max),
    );

    // Re-analysis: what the measurements themselves say lives in this region.
    let mut bands: Vec<FreqRange> = Vec::new();
    for m in &members {
        bands.extend(detection_bands(conn, m)?);
    }
    let gap_tol = center_uncertainty_hz(region.center_hz(), tol);
    let found = modes(&bands, gap_tol);

    // The best-supported box of the region, by the same proxy the competition stage ranks on.
    let top = members
        .iter()
        .copied()
        .reduce(|best, r| if better(r, best) { r } else { best })
        .expect("members is non-empty");

    for m in &members {
        if m.emitter_id == top.emitter_id {
            // The survivor never defers to its own region.
            out.revoked.extend(revoke_region_claim(
                conn,
                m.emitter_id,
                actor,
                t,
                "this row now ranks highest of the overlapping region and is the one shown",
            )?);
            continue;
        }
        let blocked = (found.len() != 1)
            .then_some("the measurements separate into more than one emission")
            .or_else(|| distinguishing_evidence(top, m, tol));
        if let Some(why) = blocked {
            out.revoked.extend(revoke_region_claim(
                conn,
                m.emitter_id,
                actor,
                t,
                "the region re-analysis no longer finds one emission here, or the guard now \
                 separates this row from the one shown",
            )?);
            out.contested.extend(contest(
                conn,
                m,
                top.emitter_id,
                region,
                members.len(),
                found.len(),
                Some(why),
                actor,
                t,
            )?);
            continue;
        }
        let mode = found[0];
        let reason = format!(
            "region re-analysis: {} boxes overlap in time and frequency over {:.6}-{:.6} MHz, and \
             the detections behind them measure as one contiguous emission {:.6}-{:.6} MHz - not \
             {} of them. This row defers to emitter {}, the best-supported box of the region \
             (detections, tracks and history kept; reversible)",
            members.len(),
            region.lo_hz / 1e6,
            region.hi_hz / 1e6,
            mode.lo_hz / 1e6,
            mode.hi_hz / 1e6,
            members.len(),
            top.emitter_id,
        );
        let standing = read_relations(conn, CURRENT_RELATION_SQL, m.emitter_id)?;
        let (claimed, revoked) = claim(
            conn,
            &standing,
            RelationClaim {
                emitter_id: m.emitter_id,
                source_id: top.emitter_id,
                kind: RelationKind::DuplicateOf,
                artifact: None,
                active: true,
                t,
                author: RelationAuthor::System,
                actor: actor.to_owned(),
                reason,
                score: Some(m.rank()),
                detail: Some(serde_json::json!({
                    "verdict": ONE_EMISSION_VERDICT,
                    "region_lo_hz": region.lo_hz,
                    "region_hi_hz": region.hi_hz,
                    "mode_lo_hz": mode.lo_hz,
                    "mode_hi_hz": mode.hi_hz,
                    "members": members.len(),
                    "shown": rank_detail(top),
                    "this": rank_detail(m),
                })),
            },
        )?;
        out.duplicates.extend(claimed);
        out.revoked.extend(revoked);
    }
    Ok(())
}
