//! Entity resolution and inventory queries (C18, C27; T-018). Rules and gating: [`crate::cluster`].

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use uuid::Uuid;

use super::inventory::{
    emitter_id_by_identity, identity_label, insert_status, link_kind, link_target,
};
use super::lifecycle;
use super::{
    RepoError, Repository, blob, bump_extent, enum_parse, enum_text, finite, int, region_bounds,
};
use crate::cluster::{
    Assignment, ConflictReason, EmitterMerge, Fingerprint, IdentityAccess, IdentityClaim,
    IdentityConflictReport, InventoryEntry, InventoryIdentity, InventoryPage, InventoryQuery,
    KnownStatusPrior, LinkRecord, MAX_INVENTORY_PAGE, MeasurementKey, RecordedClassification,
    Resolution, Sighting, Tolerances, known_family, most_restrictive, tag_in_vocabulary,
};
use crate::content::ContentClass;
use crate::emitter::{
    Classification, DecodedIdentity, Emitter, EmitterLink, Identity, KnownStatus,
    KnownStatusChange, LifecycleState, StatusAuthor,
};
use crate::ids::EmitterId;
use crate::region::{FreqRange, Region, TimeRange};
use crate::time::Timestamp;

fn eid(b: [u8; 16]) -> EmitterId {
    EmitterId::from_uuid(Uuid::from_bytes(b))
}

const ANY_TIME: TimeRange = TimeRange::new(
    Timestamp::from_unix_nanos(i64::MIN),
    Timestamp::from_unix_nanos(i64::MAX),
);

/// The live emitter an id resolves to (following `merged_into`), or `None` if it does not exist.
pub(super) fn live_id(conn: &Connection, id: EmitterId) -> Result<Option<EmitterId>, RepoError> {
    let mut cur = id;
    // Merges flatten chains, so one hop is normal; the bound only guards corrupt data.
    for _ in 0..32 {
        let row: Option<Option<[u8; 16]>> = conn
            .prepare_cached("SELECT merged_into FROM emitter WHERE emitter_id = ?1")?
            .query_row([blob(cur)], |r| r.get(0))
            .optional()?;
        match row {
            None => return Ok(None),
            Some(None) => return Ok(Some(cur)),
            Some(Some(next)) => cur = eid(next),
        }
    }
    Err(RepoError::Invalid(format!(
        "emitter {id}: merge chain too long"
    )))
}

/// The aggregate columns entity resolution reads.
pub(super) struct EmitterRow {
    f_center: f64,
    bandwidth: f64,
    first: i64,
    last: i64,
    count: i64,
    fingerprint: Option<Fingerprint>,
    pub(super) identity: Option<DecodedIdentity>,
    pub(super) class: Option<ContentClass>,
    merged: bool,
    /// T-078: deleted from the inventory.
    deleted: bool,
}

pub(super) fn load_row(conn: &Connection, id: EmitterId) -> Result<EmitterRow, RepoError> {
    type Raw = (
        f64,
        f64,
        i64,
        i64,
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<[u8; 16]>,
        String,
    );
    let raw: Raw = conn
        .prepare_cached(
            "SELECT f_center, bandwidth, first_seen, last_seen, count, fingerprint, \
             identity_scheme, identity_value, identity_class, merged_into, lifecycle_state \
             FROM emitter WHERE emitter_id = ?1",
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
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
            ))
        })
        .optional()?
        .ok_or_else(|| RepoError::NotFound {
            kind: "emitter",
            id: id.to_string(),
        })?;
    let (f_center, bandwidth, first, last, count, fp, scheme, value, class, merged, state) = raw;
    let identity = match (scheme, value) {
        (Some(s), Some(v)) => Some(DecodedIdentity {
            scheme: s.parse().map_err(RepoError::Invalid)?,
            value: v,
        }),
        _ => None,
    };
    Ok(EmitterRow {
        f_center,
        bandwidth,
        first,
        last,
        count,
        fingerprint: fp
            .map(|s| serde_json::from_str::<serde_json::Value>(&s))
            .transpose()?
            .as_ref()
            .and_then(Fingerprint::from_value),
        identity,
        class: class.map(|c| ContentClass::parse_fail_closed(Some(&c))),
        merged: merged.is_some(),
        deleted: state == "deleted",
    })
}

/// The live emitter an id resolves to, unless it is deleted (T-078: deleted rows take no
/// sightings).
fn active_id(conn: &Connection, id: EmitterId) -> Result<Option<EmitterId>, RepoError> {
    match live_id(conn, id)? {
        Some(live) if !lifecycle::is_deleted(conn, live)? => Ok(Some(live)),
        _ => Ok(None),
    }
}

fn fingerprint_text(fp: &Fingerprint) -> Result<String, RepoError> {
    if !fp.is_finite() {
        return Err(RepoError::Invalid("fingerprint must be finite".into()));
    }
    Ok(serde_json::to_string(fp)?)
}

struct CurrentStatus {
    status: KnownStatus,
    prior_ref: Option<String>,
    author: StatusAuthor,
}

fn current_status(conn: &Connection, id: EmitterId) -> Result<Option<CurrentStatus>, RepoError> {
    let raw: Option<(String, Option<String>, String)> = conn
        .prepare_cached(
            "SELECT status, prior_ref, author FROM emitter_status WHERE emitter_id = ?1 \
             ORDER BY status_id DESC LIMIT 1",
        )?
        .query_row([blob(id)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    raw.map(|(s, prior_ref, a)| {
        Ok(CurrentStatus {
            status: enum_parse(s)?,
            prior_ref,
            author: enum_parse(a)?,
        })
    })
    .transpose()
}

/// Latest classification family, else the fingerprint family.
fn current_family(conn: &Connection, id: EmitterId) -> Result<Option<String>, RepoError> {
    let family: Option<String> = conn
        .prepare_cached(
            "SELECT coalesce((SELECT c.family FROM emitter_classification c \
               WHERE c.emitter_id = emitter.emitter_id ORDER BY c.classification_id DESC LIMIT 1), \
             json_extract(fingerprint, '$.family')) FROM emitter WHERE emitter_id = ?1",
        )?
        .query_row([blob(id)], |r| r.get(0))
        .optional()?
        .flatten();
    Ok(family)
}

/// Most restrictive class of the live-linked decodes carrying `identity`; `None` if there are none.
pub(super) fn derived_identity_class(
    conn: &Connection,
    id: EmitterId,
    identity: &DecodedIdentity,
) -> Result<Option<ContentClass>, RepoError> {
    let mut stmt = conn.prepare_cached(
        "SELECT d.content_class FROM emitter_link l JOIN decode d ON d.decode_id = l.target_id \
         WHERE l.emitter_id = ?1 AND l.target_kind = 'decode' AND l.superseded_by IS NULL \
           AND d.identity_scheme = ?2 AND d.identity_value = ?3",
    )?;
    let classes = stmt
        .query_map(
            params![blob(id), identity.scheme.as_string(), identity.value],
            |r| r.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(classes
        .iter()
        .map(|c| ContentClass::parse_fail_closed(Some(c)))
        .reduce(most_restrictive))
}

/// The output view of an emitter: its identity gated by class for `access` (rules:
/// [`crate::cluster`], fail closed). A withheld identity is cleared from `emitter`.
pub(super) fn gate_entry(
    conn: &Connection,
    mut emitter: Emitter,
    access: IdentityAccess,
) -> Result<InventoryEntry, RepoError> {
    let id = emitter.id;
    let family = current_family(conn, id)?;
    let identity = match &emitter.identity {
        Identity::Unknown => InventoryIdentity::None,
        Identity::Decoded(d) => {
            let class = match load_row(conn, id)?.class {
                Some(c) => Some(c),
                None => derived_identity_class(conn, id, d)?,
            };
            match (access.reveals(class), class) {
                (true, class) => InventoryIdentity::Clear {
                    identity: d.clone(),
                    class: class.unwrap_or(ContentClass::FAIL_CLOSED),
                },
                _ => InventoryIdentity::Withheld {
                    scheme: d.scheme.clone(),
                    class,
                },
            }
        }
    };
    let lifecycle = lifecycle::state_of(conn, id)?;
    let mut tags_withheld = false;
    if matches!(identity, InventoryIdentity::Withheld { .. }) {
        // T-036/T-038: a withheld row shows vocabulary labels only (value-independent rule).
        let before = emitter.tags.len();
        emitter.tags.retain(|tag| tag_in_vocabulary(tag));
        tags_withheld = emitter.tags.len() != before;
    }
    if !matches!(identity, InventoryIdentity::Clear { .. }) {
        emitter.identity = Identity::Unknown;
    }
    Ok(InventoryEntry {
        emitter,
        identity,
        family,
        lifecycle,
        tags_withheld,
    })
}

/// Rule 1 re-measurement: the live emitter and largest counted count of an earlier sighting of
/// the same measurement (key, overlapping span, centre within tolerance, no identity clash).
fn remeasured(
    conn: &Connection,
    s: &Sighting,
    key: &MeasurementKey,
    tol: &Tolerances,
) -> Result<Option<(EmitterId, u64)>, RepoError> {
    let (start, end) = (s.seen.start.as_unix_nanos(), s.seen.end.as_unix_nanos());
    let rows: Vec<([u8; 16], i64, i64, i64, f64)> = {
        let mut stmt = conn.prepare_cached(
            "SELECT emitter_id, count, t_start, t_end, f_center FROM emitter_observation \
             WHERE measurement = ?1 AND t_start <= ?2 AND t_end >= ?3",
        )?;
        stmt.query_map(params![key.text(), end, start], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<Result<_, _>>()?
    };
    let ours = Fingerprint::new(s.f_center_hz, s.bandwidth_hz);
    let mut best: Option<(EmitterId, u64)> = None;
    for (e, count, t0, t1, f) in rows {
        let overlap = i128::from(end.min(t1)) - i128::from(start.max(t0));
        let shorter = (i128::from(end) - i128::from(start)).min(i128::from(t1) - i128::from(t0));
        if overlap < 0 || 2 * overlap < shorter {
            continue;
        }
        let theirs = Fingerprint::new(f, 0.0);
        if (f - s.f_center_hz).abs() > ours.center_tolerance_hz(&theirs, tol) {
            continue;
        }
        let Some(live) = active_id(conn, eid(e))? else {
            continue;
        };
        if let Some(claim) = &s.identity
            && load_row(conn, live)?
                .identity
                .is_some_and(|held| held != claim.identity)
        {
            continue;
        }
        let count = count.max(0) as u64;
        if best.is_none_or(|(_, c)| count > c) {
            best = Some((live, count));
        }
    }
    Ok(best)
}

fn conflict(emitters: Vec<EmitterId>, reason: ConflictReason) -> Option<IdentityConflictReport> {
    Some(IdentityConflictReport { emitters, reason })
}

/// T-082: what a merge carries besides counts, links, tags and identity.
/// - The absorbed row's classification history, appended to the survivor with its inputs.
/// - Its current known status when a decoder, classifier or user decided it, unless the survivor's
///   current status is a user's.
/// - Its lifecycle state: a confirmed row merged into a candidate confirms the survivor, and the
///   survivor's history records it. Deleted rows never reach a merge.
fn carry_evidence(
    conn: &Connection,
    from: EmitterId,
    into: EmitterId,
    t: Timestamp,
) -> Result<(), RepoError> {
    conn.prepare_cached(
        "INSERT INTO emitter_classification (emitter_id, t, family, confidence, open_set_score, \
         model_version, input_kind, input_id, feature_set_version) \
         SELECT ?1, t, family, confidence, open_set_score, model_version, input_kind, input_id, \
         feature_set_version FROM emitter_classification WHERE emitter_id = ?2 \
         ORDER BY classification_id",
    )?
    .execute(params![blob(into), blob(from)])?;
    let decided = |a: StatusAuthor| {
        matches!(
            a,
            StatusAuthor::Decoder | StatusAuthor::Classifier | StatusAuthor::User
        )
    };
    if let Some(theirs) = current_status(conn, from)?
        && decided(theirs.author)
        && current_status(conn, into)?.is_none_or(|ours| {
            ours.author != StatusAuthor::User
                && (ours.status != theirs.status || !decided(ours.author))
        })
    {
        let why: String = conn
            .prepare_cached(
                "SELECT reason FROM emitter_status WHERE emitter_id = ?1 \
                 ORDER BY status_id DESC LIMIT 1",
            )?
            .query_row([blob(from)], |r| r.get(0))?;
        insert_status(
            conn,
            &KnownStatusChange {
                emitter_id: into,
                status: theirs.status,
                prior_ref: theirs.prior_ref,
                reason: format!("{why} (merged from emitter {from})"),
                t,
                author: theirs.author,
            },
        )?;
    }
    if lifecycle::state_of(conn, from)? == LifecycleState::Confirmed
        && lifecycle::state_of(conn, into)? == LifecycleState::Candidate
    {
        lifecycle::carry_confirmation(conn, from, into, t)?;
    }
    Ok(())
}

/// One observation-ledger span of an emitter.
#[derive(Clone, Copy)]
struct Span {
    track: bool,
    t0: i64,
    t1: i64,
    count: i64,
}

/// An emitter's observation spans, by start.
fn observation_spans(conn: &Connection, id: EmitterId) -> Result<Vec<Span>, RepoError> {
    let mut spans: Vec<Span> = conn
        .prepare_cached(
            "SELECT source_kind = 'track', t_start, t_end, count FROM emitter_observation \
             WHERE emitter_id = ?1",
        )?
        .query_map([blob(id)], |r| {
            Ok(Span {
                track: r.get(0)?,
                t0: r.get(1)?,
                t1: r.get(2)?,
                count: r.get(3)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    spans.sort_by_key(|s| (s.t0, s.t1));
    Ok(spans)
}

/// Some span of `a` overlaps some span of `b` in time (both sorted by start).
fn spans_overlap(a: &[Span], b: &[Span]) -> bool {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i].t1 < b[j].t0 {
            i += 1;
        } else if b[j].t1 < a[i].t0 {
            j += 1;
        } else {
            return true;
        }
    }
    false
}

/// The latest refined centre of an emitter (its own refinements and those merged into it).
fn refined_center(conn: &Connection, id: EmitterId) -> Result<Option<f64>, RepoError> {
    Ok(conn
        .prepare_cached(
            "WITH RECURSIVE absorbed(id) AS ( \
                 SELECT ?1 \
                 UNION SELECT e.emitter_id FROM emitter e JOIN absorbed a ON e.merged_into = a.id \
             ) \
             SELECT f_center FROM emitter_refined_tuning WHERE emitter_id IN (SELECT id FROM absorbed) \
             ORDER BY t DESC, refined_id DESC LIMIT 1",
        )?
        .query_row([blob(id)], |r| r.get(0))
        .optional()?)
}

/// The same-emission rule ([`crate::cluster`], "Same emission"): `Some(score)` (smallest centre
/// error over the centre tolerance; lower is closer) when live, listed emitters `a` and `b`
/// observed the same emission.
fn same_emission_score(
    conn: &Connection,
    a: EmitterId,
    b: EmitterId,
    tol: &Tolerances,
) -> Result<Option<f64>, RepoError> {
    if a == b {
        return Ok(None);
    }
    let (ra, rb) = (load_row(conn, a)?, load_row(conn, b)?);
    if ra.merged || rb.merged || ra.deleted || rb.deleted {
        return Ok(None);
    }
    if ra.identity.is_some() && rb.identity.is_some() {
        return Ok(None);
    }
    let hops = |r: &EmitterRow| {
        r.fingerprint
            .as_ref()
            .is_some_and(|f| !f.hop_set_hz.is_empty())
    };
    if hops(&ra) != hops(&rb) {
        return Ok(None);
    }
    let f_tol = Fingerprint::new(ra.f_center, ra.bandwidth)
        .center_tolerance_hz(&Fingerprint::new(rb.f_center, rb.bandwidth), tol);
    let ca = [Some(ra.f_center), refined_center(conn, a)?];
    let cb = [Some(rb.f_center), refined_center(conn, b)?];
    let err = ca
        .iter()
        .flatten()
        .flat_map(|x| cb.iter().flatten().map(move |y| (x - y).abs()))
        .fold(f64::INFINITY, f64::min);
    // NaN (incomparable) counts as out of tolerance.
    if !matches!(
        err.partial_cmp(&f_tol),
        Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
    ) {
        return Ok(None);
    }
    let (sa, sb) = (observation_spans(conn, a)?, observation_spans(conn, b)?);
    if !spans_overlap(&sa, &sb) {
        return Ok(None);
    }
    // Two track-based entries the fingerprint kept apart (another period, burst length, …) are
    // two emitters sharing a channel, not one.
    if sa.iter().any(|s| s.track)
        && sb.iter().any(|s| s.track)
        && let (Some(fa), Some(fb)) = (&ra.fingerprint, &rb.fingerprint)
        && !fa.compare(fb, tol).within
    {
        return Ok(None);
    }
    Ok(Some(err / f_tol))
}

/// Live, listed emitters observing the same emission as `id`, closest first.
fn same_emission_partners(
    conn: &Connection,
    id: EmitterId,
    tol: &Tolerances,
) -> Result<Vec<(EmitterId, f64)>, RepoError> {
    let row = load_row(conn, id)?;
    if row.merged || row.deleted {
        return Ok(Vec::new());
    }
    let mut centres = vec![row.f_center];
    centres.extend(refined_center(conn, id)?);
    let mut ids: Vec<EmitterId> = Vec::new();
    for c in centres {
        // A partner within tolerance has a centre within max(ppm, min, frac·BW) of `c`, so its
        // occupied band overlaps this window (as in `fingerprint_candidates`).
        let reach = (tol.center_ppm * 1e-6 * c.abs())
            .max(tol.center_min_hz)
            .max(row.bandwidth / 2.0);
        let region = Region::new(FreqRange::new(c - reach, c + reach), ANY_TIME);
        let b = region_bounds(conn, "emitter", &region)?;
        let mut stmt = conn.prepare_cached(
            "SELECT emitter_id FROM emitter WHERE f_lo BETWEEN ?1 AND ?2 AND f_hi >= ?3 \
             AND merged_into IS NULL AND lifecycle_state != 'deleted' AND emitter_id != ?4",
        )?;
        let found = stmt
            .query_map(params![b.f_lo_min, b.hi, b.lo, blob(id)], |r| {
                r.get::<_, [u8; 16]>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for e in found.into_iter().map(eid) {
            if !ids.contains(&e) {
                ids.push(e);
            }
        }
    }
    let mut out = Vec::new();
    for other in ids {
        if let Some(score) = same_emission_score(conn, id, other, tol)? {
            out.push((other, score));
        }
    }
    out.sort_by(|x, y| x.1.total_cmp(&y.1).then(x.0.cmp(&y.0)));
    Ok(out)
}

/// Merges two entries of one emission inside the caller's transaction (see
/// [`Repository::merge_same_emission`]); `None` when they are not (or no longer) the same.
fn merge_same_emission_rows(
    conn: &Connection,
    a: EmitterId,
    b: EmitterId,
    t: Timestamp,
    reason: &str,
    tol: &Tolerances,
) -> Result<Option<EmitterMerge>, RepoError> {
    let (Some(a), Some(b)) = (live_id(conn, a)?, live_id(conn, b)?) else {
        return Ok(None);
    };
    if same_emission_score(conn, a, b, tol)?.is_none() {
        return Ok(None);
    }
    // Survivor: confirmed before candidate (the entry a user or rule already accepted keeps its
    // id), then the first seen, then the larger count.
    let rank = |id: EmitterId| -> Result<_, RepoError> {
        let r = load_row(conn, id)?;
        let confirmed = lifecycle::state_of(conn, id)? == LifecycleState::Confirmed;
        Ok((!confirmed, r.first, std::cmp::Reverse(r.count), id))
    };
    let (into, from) = if rank(a)? <= rank(b)? { (a, b) } else { (b, a) };
    // Where the two overlap in time they counted the same bursts: that stretch counts once, as
    // the larger of the two counts; the rest of `from` adds.
    let (ours, theirs) = (
        observation_spans(conn, into)?,
        observation_spans(conn, from)?,
    );
    let overlapping = |a: &[Span], b: &[Span]| -> i64 {
        a.iter()
            .filter(|s| spans_overlap(b, std::slice::from_ref(*s)))
            .map(|s| s.count.max(0))
            .sum()
    };
    let (shared_from, shared_into) = (overlapping(&theirs, &ours), overlapping(&ours, &theirs));
    let from_count = load_row(conn, from)?.count;
    let add = from_count - shared_from + (shared_from - shared_into).max(0);
    let m = merge_rows(conn, from, into, t, reason, Some(add))?;
    super::gating::purge_withheld_tags(conn, into)?;
    Ok(Some(m))
}

/// Merges `from` into `into` inside the caller's transaction. See [`Repository::merge_emitters`].
/// `add` is what `from` adds to the survivor's count (`None`: its whole count).
fn merge_rows(
    conn: &Connection,
    from: EmitterId,
    into: EmitterId,
    t: Timestamp,
    reason: &str,
    add: Option<i64>,
) -> Result<EmitterMerge, RepoError> {
    if from == into {
        return Err(RepoError::Invalid(
            "cannot merge an emitter into itself".into(),
        ));
    }
    let a = load_row(conn, from)?;
    let b = load_row(conn, into)?;
    if a.merged || b.merged {
        return Err(RepoError::Invalid(format!(
            "merge {from} into {into}: both emitters must be live (not already merged)"
        )));
    }
    if a.deleted || b.deleted {
        return Err(RepoError::Invalid(format!(
            "merge {from} into {into}: a deleted inventory entry cannot be merged"
        )));
    }
    if let (Some(ai), Some(_)) = (&a.identity, &b.identity) {
        return Err(RepoError::IdentityConflict {
            identity: identity_label(ai),
            existing: into,
        });
    }
    let identity_moved = a.identity.is_some();
    let add = add.map_or(a.count, |n| n.clamp(0, a.count.max(0)));
    let (f_center, bandwidth) = if a.last > b.last {
        (a.f_center, a.bandwidth)
    } else {
        (b.f_center, b.bandwidth)
    };
    let freq = FreqRange::centered(f_center, bandwidth);
    let fingerprint = match (a.fingerprint, b.fingerprint) {
        (Some(fa), Some(mut fb)) => {
            fb.fold(&fa);
            Some(fb)
        }
        (fa, fb) => fb.or(fa),
    };
    let (scheme, value, class) = match (&a.identity, identity_moved) {
        (Some(d), true) => (
            Some(d.scheme.as_string()),
            Some(d.value.clone()),
            a.class.map(|c| enum_text(&c)).transpose()?,
        ),
        _ => (None, None, None),
    };
    if identity_moved {
        conn.prepare_cached(
            "UPDATE emitter SET identity_scheme = NULL, identity_value = NULL, \
             identity_class = NULL WHERE emitter_id = ?1",
        )?
        .execute([blob(from)])?;
    }
    conn.prepare_cached(
        "UPDATE emitter SET count = count + ?1, first_seen = min(first_seen, ?2), \
         last_seen = max(last_seen, ?3), f_center = ?4, bandwidth = ?5, f_lo = ?6, f_hi = ?7, \
         fingerprint = coalesce(?8, fingerprint), \
         identity_class = CASE WHEN identity_scheme IS NULL THEN ?11 ELSE identity_class END, \
         identity_scheme = coalesce(identity_scheme, ?9), \
         identity_value = coalesce(identity_value, ?10) \
         WHERE emitter_id = ?12",
    )?
    .execute(params![
        add,
        a.first,
        a.last,
        f_center,
        bandwidth,
        freq.lo_hz,
        freq.hi_hz,
        fingerprint.as_ref().map(fingerprint_text).transpose()?,
        scheme,
        value,
        class,
        blob(into)
    ])?;
    conn.prepare_cached(
        "UPDATE emitter SET merged_into = ?1 WHERE emitter_id = ?2 OR merged_into = ?2",
    )?
    .execute(params![blob(into), blob(from)])?;
    conn.prepare_cached("UPDATE emitter_observation SET emitter_id = ?1 WHERE emitter_id = ?2")?
        .execute(params![blob(into), blob(from)])?;
    conn.prepare_cached(
        "INSERT OR IGNORE INTO emitter_link (emitter_id, target_kind, target_id, linked_at) \
         SELECT ?1, target_kind, target_id, ?3 FROM emitter_link \
         WHERE emitter_id = ?2 AND superseded_by IS NULL",
    )?
    .execute(params![blob(into), blob(from), t.as_unix_nanos()])?;
    conn.prepare_cached(
        "UPDATE emitter_link SET superseded_by = ?1, superseded_at = ?3 \
         WHERE emitter_id = ?2 AND superseded_by IS NULL",
    )?
    .execute(params![blob(into), blob(from), t.as_unix_nanos()])?;
    // T-040: callers purge `into` (and the rows merged into it) with `purge_withheld_tags` before
    // committing, once the survivor's final class is known (entity resolution may still set the
    // identity class after this merge), so no free-text tag is committed onto a withheld survivor.
    conn.prepare_cached(
        "INSERT OR IGNORE INTO emitter_tag (emitter_id, tag) \
         SELECT ?1, tag FROM emitter_tag WHERE emitter_id = ?2",
    )?
    .execute(params![blob(into), blob(from)])?;
    conn.prepare_cached(
        "INSERT INTO emitter_merge (from_emitter, into_emitter, t, reason, from_count, \
         identity_moved) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?
    .execute(params![
        blob(from),
        blob(into),
        t.as_unix_nanos(),
        reason,
        a.count,
        identity_moved
    ])?;
    carry_evidence(conn, from, into, t)?;
    bump_extent(conn, "emitter", freq.width_hz(), 0)?;
    Ok(EmitterMerge {
        from,
        into,
        t,
        reason: reason.to_owned(),
        from_count: a.count as u64,
        identity_moved,
    })
}

struct Candidate {
    id: EmitterId,
    score: f64,
    last: i64,
    identity: Option<DecodedIdentity>,
}

/// Live emitters whose fingerprint (or centre/bandwidth when none is stored) is within tolerance,
/// best first.
fn fingerprint_candidates(
    conn: &Connection,
    fp: &Fingerprint,
    tol: &Tolerances,
) -> Result<Vec<Candidate>, RepoError> {
    // A candidate within tolerance has its centre within max(ppm, min, frac·BW) of ours; with
    // frac ≤ 0.5 its occupied band [f_lo, f_hi] then overlaps this window.
    let reach = (tol.center_ppm * 1e-6 * fp.f_center_hz.abs())
        .max(tol.center_min_hz)
        .max(fp.bandwidth_hz / 2.0);
    let region = Region::new(
        FreqRange::new(fp.f_center_hz - reach, fp.f_center_hz + reach),
        ANY_TIME,
    );
    let b = region_bounds(conn, "emitter", &region)?;
    type Raw = (
        [u8; 16],
        f64,
        f64,
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Raw> = {
        let mut stmt = conn.prepare_cached(
            "SELECT emitter_id, f_center, bandwidth, last_seen, fingerprint, identity_scheme, \
             identity_value FROM emitter \
             WHERE f_lo BETWEEN ?1 AND ?2 AND f_hi >= ?3 AND merged_into IS NULL \
             AND lifecycle_state != 'deleted'",
        )?;
        stmt.query_map(params![b.f_lo_min, b.hi, b.lo], |r| {
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
    let mut out = Vec::new();
    for (id, f_center, bw, last, stored, scheme, value) in rows {
        let other = stored
            .map(|s| serde_json::from_str::<serde_json::Value>(&s))
            .transpose()?
            .as_ref()
            .and_then(Fingerprint::from_value)
            .unwrap_or_else(|| Fingerprint::new(f_center, bw));
        let m = fp.compare(&other, tol);
        if !m.within {
            continue;
        }
        let identity = match (scheme, value) {
            (Some(s), Some(v)) => Some(DecodedIdentity {
                scheme: s.parse().map_err(RepoError::Invalid)?,
                value: v,
            }),
            _ => None,
        };
        out.push(Candidate {
            id: eid(id),
            score: m.score,
            last,
            identity,
        });
    }
    out.sort_by(|x, y| {
        x.score
            .total_cmp(&y.score)
            .then(y.last.cmp(&x.last))
            .then(x.id.cmp(&y.id))
    });
    Ok(out)
}

/// Rules 2–5 of [`crate::cluster`]: the target (`None` = create) and how it was chosen.
fn decide(
    conn: &Connection,
    s: &Sighting,
    tol: &Tolerances,
    report: &mut Option<IdentityConflictReport>,
    merge: &mut Option<EmitterMerge>,
) -> Result<(Option<EmitterId>, Assignment), RepoError> {
    let context = match s.context {
        Some(c) => active_id(conn, c)?,
        None => None,
    };
    if let Some(claim) = &s.identity {
        let holder = match emitter_id_by_identity(conn, &claim.identity)? {
            Some(h) => active_id(conn, h)?,
            None => None,
        };
        let ctx = context.filter(|_| !claim.identity.scheme.shares_channel());
        let ctx_identified = match ctx {
            Some(c) => load_row(conn, c)?.identity.is_some(),
            None => false,
        };
        return Ok(match (holder, ctx) {
            (Some(h), Some(c)) if h != c => {
                if ctx_identified {
                    *report = conflict(vec![h, c], ConflictReason::ContextHoldsOtherIdentity);
                } else {
                    *merge = Some(merge_rows(
                        conn,
                        c,
                        h,
                        s.seen.end,
                        "decoded identity observed on an unidentified context emitter",
                        None,
                    )?);
                }
                (Some(h), Assignment::Identity)
            }
            (Some(h), _) => (Some(h), Assignment::Identity),
            (None, Some(c)) if ctx_identified => {
                *report = conflict(vec![c], ConflictReason::ContextHoldsOtherIdentity);
                (None, Assignment::Created)
            }
            (None, Some(c)) => (Some(c), Assignment::Context),
            (None, None) => (None, Assignment::Created),
        });
    }
    if let Some(c) = context {
        return Ok((Some(c), Assignment::Context));
    }
    let Some(fp) = &s.fingerprint else {
        return Ok((None, Assignment::Created));
    };
    let mut cands = fingerprint_candidates(conn, fp, tol)?;
    cands.retain(|c| {
        !c.identity
            .as_ref()
            .is_some_and(|i| i.scheme.shares_channel())
    });
    let identified: Vec<EmitterId> = cands
        .iter()
        .filter(|c| c.identity.is_some())
        .map(|c| c.id)
        .collect();
    let pick = if identified.len() >= 2 {
        *report = conflict(
            identified,
            ConflictReason::FingerprintMatchesSeveralIdentities,
        );
        cands.iter().find(|c| c.identity.is_none())
    } else {
        cands.first()
    };
    Ok(match pick {
        Some(c) => (Some(c.id), Assignment::Fingerprint { score: c.score }),
        None => (None, Assignment::Created),
    })
}

fn create(conn: &Connection, s: &Sighting, why: &str) -> Result<EmitterId, RepoError> {
    let id = EmitterId::new();
    let freq = FreqRange::centered(s.f_center_hz, s.bandwidth_hz);
    let (scheme, value, class) = match &s.identity {
        Some(c) => (
            Some(c.identity.scheme.as_string()),
            Some(c.identity.value.clone()),
            Some(enum_text(&c.content_class)?),
        ),
        None => (None, None, None),
    };
    conn.prepare_cached(
        "INSERT INTO emitter (emitter_id, f_center, bandwidth, first_seen, last_seen, count, \
         fingerprint, identity_scheme, identity_value, identity_class, f_lo, f_hi) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )?
    .execute(params![
        blob(id),
        s.f_center_hz,
        s.bandwidth_hz,
        s.seen.start.as_unix_nanos(),
        s.seen.end.as_unix_nanos(),
        int(s.count, "count")?,
        s.fingerprint.as_ref().map(fingerprint_text).transpose()?,
        scheme,
        value,
        class,
        freq.lo_hz,
        freq.hi_hz
    ])?;
    insert_status(
        conn,
        &KnownStatusChange {
            emitter_id: id,
            status: KnownStatus::Unknown,
            prior_ref: None,
            reason: format!("new emitter: {why}"),
            t: s.seen.start,
            author: StatusAuthor::Clusterer,
        },
    )?;
    bump_extent(conn, "emitter", freq.width_hz(), 0)?;
    Ok(id)
}

fn update_aggregate(
    conn: &Connection,
    id: EmitterId,
    s: &Sighting,
    add: u64,
) -> Result<(), RepoError> {
    let freq = FreqRange::centered(s.f_center_hz, s.bandwidth_hz);
    // SET expressions all see the pre-update row, so `last_seen` below is the old value.
    conn.prepare_cached(
        "UPDATE emitter SET count = count + ?1, first_seen = min(first_seen, ?2), \
           last_seen = max(last_seen, ?3), \
           f_center = CASE WHEN ?3 >= last_seen THEN ?4 ELSE f_center END, \
           bandwidth = CASE WHEN ?3 >= last_seen THEN ?5 ELSE bandwidth END, \
           f_lo = CASE WHEN ?3 >= last_seen THEN ?6 ELSE f_lo END, \
           f_hi = CASE WHEN ?3 >= last_seen THEN ?7 ELSE f_hi END \
         WHERE emitter_id = ?8",
    )?
    .execute(params![
        int(add, "count")?,
        s.seen.start.as_unix_nanos(),
        s.seen.end.as_unix_nanos(),
        s.f_center_hz,
        s.bandwidth_hz,
        freq.lo_hz,
        freq.hi_hz,
        blob(id)
    ])?;
    if add > 0
        && let Some(obs) = &s.fingerprint
    {
        let fp = match load_row(conn, id)?.fingerprint {
            Some(mut stored) => {
                stored.fold(obs);
                stored
            }
            None => obs.clone(),
        };
        conn.prepare_cached("UPDATE emitter SET fingerprint = ?1 WHERE emitter_id = ?2")?
            .execute(params![fingerprint_text(&fp)?, blob(id)])?;
    }
    bump_extent(conn, "emitter", freq.width_hz(), 0)?;
    Ok(())
}

/// Sets or re-classes the identity on an existing target; a collision is reported, not applied.
fn apply_identity(
    conn: &Connection,
    target: EmitterId,
    claim: &IdentityClaim,
) -> Result<Option<IdentityConflictReport>, RepoError> {
    let row = load_row(conn, target)?;
    // T-036: after an audited reclassification, a source of the opened class or less restrictive
    // counts as the opened class; restricted-cellular/paging sources still close the identity.
    let claim_class = super::gating::claim_class(conn, &claim.identity, claim.content_class)?;
    match &row.identity {
        Some(d) if *d == claim.identity => {
            let base = match row.class {
                Some(c) => Some(c),
                None => derived_identity_class(conn, target, d)?,
            };
            let class = base.map_or(claim_class, |c| most_restrictive(c, claim_class));
            conn.prepare_cached("UPDATE emitter SET identity_class = ?1 WHERE emitter_id = ?2")?
                .execute(params![enum_text(&class)?, blob(target)])?;
            Ok(None)
        }
        Some(_) => Ok(conflict(
            vec![target],
            ConflictReason::ContextHoldsOtherIdentity,
        )),
        None => {
            if let Some(h) = emitter_id_by_identity(conn, &claim.identity)?
                && h != target
            {
                return Ok(conflict(
                    vec![h, target],
                    ConflictReason::ContextHoldsOtherIdentity,
                ));
            }
            conn.prepare_cached(
                "UPDATE emitter SET identity_scheme = ?1, identity_value = ?2, \
                 identity_class = ?3 WHERE emitter_id = ?4",
            )?
            .execute(params![
                claim.identity.scheme.as_string(),
                claim.identity.value,
                enum_text(&claim_class)?,
                blob(target)
            ])?;
            Ok(None)
        }
    }
}

fn apply_prior(
    conn: &Connection,
    id: EmitterId,
    family: &str,
    priors: &dyn KnownStatusPrior,
    t: Timestamp,
) -> Result<Option<KnownStatus>, RepoError> {
    let row = load_row(conn, id)?;
    let v = priors.verdict(family, row.f_center, row.bandwidth);
    if let Some(cur) = current_status(conn, id)? {
        let overridable = matches!(
            cur.author,
            StatusAuthor::System | StatusAuthor::Clusterer | StatusAuthor::Prior
        );
        let same =
            cur.status == v.status && (v.prior_ref.is_none() || cur.prior_ref == v.prior_ref);
        if !overridable || same {
            return Ok(None);
        }
    }
    insert_status(
        conn,
        &KnownStatusChange {
            emitter_id: id,
            status: v.status,
            prior_ref: v.prior_ref,
            reason: v.reason,
            t,
            author: StatusAuthor::Prior,
        },
    )?;
    Ok(Some(v.status))
}

fn resolve(
    conn: &Connection,
    s: &Sighting,
    key: Option<&MeasurementKey>,
    tol: &Tolerances,
    priors: Option<&dyn KnownStatusPrior>,
) -> Result<Resolution, RepoError> {
    let (kind, source_id) = link_kind(&s.source);
    let ledger: Option<([u8; 16], i64)> = conn
        .prepare_cached(
            "SELECT emitter_id, count FROM emitter_observation \
             WHERE source_kind = ?1 AND source_id = ?2",
        )?
        .query_row(params![kind, source_id.into_bytes()], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?;
    // T-078: an identity held by a deleted entry goes to whichever live emitter this sighting
    // reaches (a new candidate, unless another live emitter matches).
    if let Some(claim) = &s.identity {
        lifecycle::release_deleted_identity(conn, &claim.identity)?;
    }
    let ledger = match ledger {
        Some((e, counted)) => {
            let live = live_id(conn, eid(e))?.ok_or_else(|| {
                RepoError::Invalid(format!(
                    "observation ledger names missing emitter {}",
                    eid(e)
                ))
            })?;
            // T-078: a source counted into a since-deleted entry is sighted afresh.
            (!lifecycle::is_deleted(conn, live)?).then_some((live, counted))
        }
        None => None,
    };
    let mut report = None;
    let mut merge = None;
    let (target, assignment, add) = match ledger {
        Some((live, counted)) => (
            Some(live),
            Assignment::Replay,
            s.count.saturating_sub(counted as u64),
        ),
        None => match key
            .map(|k| remeasured(conn, s, k, tol))
            .transpose()?
            .flatten()
        {
            Some((live, counted)) => (
                Some(live),
                Assignment::Replay,
                s.count.saturating_sub(counted),
            ),
            None => {
                let (t, a) = decide(conn, s, tol, &mut report, &mut merge)?;
                (t, a, s.count)
            }
        },
    };
    let (id, created, family_before) = match target {
        Some(id) => {
            let family = current_family(conn, id)?;
            update_aggregate(conn, id, s, add)?;
            if let Some(claim) = &s.identity {
                let r = apply_identity(conn, id, claim)?;
                report = report.or(r);
            }
            (id, false, family)
        }
        None => {
            let why = match (&s.identity, &report) {
                (Some(_), Some(_)) => "decoded identity collides with its context emitter",
                (Some(_), None) => "first sighting of a decoded identity",
                (None, Some(_)) => "fingerprint matches several identified emitters",
                (None, None) if s.fingerprint.is_some() => "no fingerprint within tolerance",
                (None, None) => "no identity, context or fingerprint",
            };
            (create(conn, s, why)?, true, None)
        }
    };
    conn.prepare_cached(
        "INSERT INTO emitter_observation (source_kind, source_id, emitter_id, count, t_start, \
         t_end, measurement, f_center) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
         ON CONFLICT (source_kind, source_id) DO UPDATE SET emitter_id = excluded.emitter_id, \
         count = max(count, excluded.count), t_start = min(t_start, excluded.t_start), \
         t_end = max(t_end, excluded.t_end), \
         measurement = coalesce(measurement, excluded.measurement), \
         f_center = coalesce(f_center, excluded.f_center)",
    )?
    .execute(params![
        kind,
        source_id.into_bytes(),
        blob(id),
        int(s.count, "count")?,
        s.seen.start.as_unix_nanos(),
        s.seen.end.as_unix_nanos(),
        key.map(MeasurementKey::text),
        key.map(|_| s.f_center_hz)
    ])?;
    conn.prepare_cached(
        "INSERT OR IGNORE INTO emitter_link (emitter_id, target_kind, target_id, linked_at) \
         VALUES (?1, ?2, ?3, ?4)",
    )?
    .execute(params![
        blob(id),
        kind,
        source_id.into_bytes(),
        s.seen.end.as_unix_nanos()
    ])?;
    // T-038: the sighting's own claim was checked up front; the target's identity (reached by
    // context or fingerprint) gets the same class-only rule, so no producer tag outside the
    // vocabulary lands on an identity no access level reveals. The transaction is not committed.
    if s.tags.iter().any(|t| !tag_in_vocabulary(t))
        && let Some(class) = super::gating::emitter_identity_class(conn, id)?
        && super::gating::vocabulary_only(class)
    {
        return Err(super::gating::vocabulary_refusal());
    }
    for tag in &s.tags {
        conn.prepare_cached("INSERT OR IGNORE INTO emitter_tag (emitter_id, tag) VALUES (?1, ?2)")?
            .execute(params![blob(id), tag])?;
    }
    // T-040: an identity applied or re-classed above (or linked restricted decode) may have
    // tightened the class; free-text tags stored before then are purged in this transaction.
    super::gating::purge_withheld_tags(conn, id)?;
    if let Some(c) = &s.classification {
        insert_classification_from(conn, id, c, Some((kind, source_id)), s.fingerprint.as_ref())?;
    }
    let family_after = current_family(conn, id)?;
    let mut status_appended = None;
    if let (Some(priors), Some(family)) = (priors, known_family(family_after.as_deref()))
        && (created || known_family(family_before.as_deref()) != Some(family))
    {
        status_appended = apply_prior(conn, id, family, priors, s.seen.end)?;
    }
    Ok(Resolution {
        emitter_id: id,
        assignment,
        created,
        count_added: add,
        merge,
        conflict: report,
        status_appended,
    })
}

/// Appends a classification with its input; an identical row for the same input is not repeated.
fn insert_classification_from(
    conn: &Connection,
    id: EmitterId,
    c: &Classification,
    input: Option<(&str, Uuid)>,
    fingerprint: Option<&Fingerprint>,
) -> Result<(), RepoError> {
    let (kind, input_id) = match input {
        Some((k, u)) => (Some(k), Some(u.into_bytes())),
        None => (None, None),
    };
    let dup = conn
        .prepare_cached(
            "SELECT 1 FROM emitter_classification WHERE input_kind IS ?1 AND input_id IS ?2 \
             AND family = ?3 AND model_version = ?4 AND t = ?5 AND input_kind IS NOT NULL",
        )?
        .query_row(
            params![
                kind,
                input_id,
                c.family,
                c.model_version,
                c.t.as_unix_nanos()
            ],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if dup {
        return Ok(());
    }
    conn.prepare_cached(
        "INSERT INTO emitter_classification (emitter_id, t, family, confidence, open_set_score, \
         model_version, input_kind, input_id, feature_set_version) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?
    .execute(params![
        blob(id),
        c.t.as_unix_nanos(),
        c.family,
        finite(c.confidence, "confidence")?,
        finite(c.open_set_score, "open_set_score")?,
        c.model_version,
        kind,
        input_id,
        fingerprint.map(|f| f.version)
    ])?;
    Ok(())
}

impl Repository {
    /// Resolves one sighting to an emitter with the default [`Tolerances`] (rules, idempotency and
    /// merges: [`crate::cluster`]). Everything happens in one transaction.
    pub fn record_sighting(
        &mut self,
        sighting: &Sighting,
        priors: Option<&dyn KnownStatusPrior>,
    ) -> Result<Resolution, RepoError> {
        self.record_sighting_with(sighting, &Tolerances::default(), priors)
    }

    /// [`Self::record_sighting`] for producers that mint new row ids when re-run on the same IQ
    /// (demodulators, decoders): a sighting of the same [`MeasurementKey`], overlapping span and
    /// channel resolves as a replay instead of counting again (rule 1, [`crate::cluster`]).
    pub fn record_sighting_measured(
        &mut self,
        sighting: &Sighting,
        key: &MeasurementKey,
        priors: Option<&dyn KnownStatusPrior>,
    ) -> Result<Resolution, RepoError> {
        self.record(sighting, Some(key), &Tolerances::default(), priors)
    }

    /// [`Self::record_sighting`] with explicit tolerances.
    pub fn record_sighting_with(
        &mut self,
        s: &Sighting,
        tol: &Tolerances,
        priors: Option<&dyn KnownStatusPrior>,
    ) -> Result<Resolution, RepoError> {
        self.record(s, None, tol, priors)
    }

    fn record(
        &mut self,
        s: &Sighting,
        key: Option<&MeasurementKey>,
        tol: &Tolerances,
        priors: Option<&dyn KnownStatusPrior>,
    ) -> Result<Resolution, RepoError> {
        if s.seen.end < s.seen.start {
            return Err(RepoError::Invalid("sighting ends before it starts".into()));
        }
        finite(s.f_center_hz, "f_center_hz")?;
        finite(s.bandwidth_hz, "bandwidth_hz")?;
        if s.bandwidth_hz < 0.0 {
            return Err(RepoError::Invalid("bandwidth_hz must be >= 0".into()));
        }
        int(s.count, "count")?;
        super::gating::check_tags(
            &mut s.tags.iter(),
            s.identity
                .as_ref()
                .map(|c| (&c.identity, Some(c.content_class))),
        )?;
        let tx = self.write_tx()?;
        let r = resolve(&tx, s, key, tol, priors)?;
        tx.commit()?;
        Ok(r)
    }

    /// Merges `from` into `into`: counts summed, seen spans widened, fingerprints folded, tags
    /// copied, the observation ledger and live links re-pointed (old links superseded, not
    /// deleted), an identity moved when only `from` has one, and `from.merged_into = into`
    /// (chains flattened). T-082: classification history appended to `into`, a decided known
    /// status carried, and a confirmed `from` confirms a candidate `into` (recorded in its lifecycle
    /// history). Refused when both hold identities ([`RepoError::IdentityConflict`]), either is
    /// already merged, or either is deleted.
    pub fn merge_emitters(
        &mut self,
        from: EmitterId,
        into: EmitterId,
        t: Timestamp,
        reason: &str,
    ) -> Result<EmitterMerge, RepoError> {
        self.ensure_refined_table()?;
        let tx = self.write_tx()?;
        let m = merge_rows(&tx, from, into, t, reason, None)?;
        // T-040: onto a survivor whose identity no access level reveals, only vocabulary tags stay.
        super::gating::purge_withheld_tags(&tx, into)?;
        tx.commit()?;
        Ok(m)
    }

    /// T-082: whether emitters `a` and `b` (merged ids stand for their survivors) observed the
    /// same emission ([`crate::cluster`], "Same emission"): `Some(score)`, lower is closer.
    pub fn same_emission(
        &self,
        a: EmitterId,
        b: EmitterId,
        tol: &Tolerances,
    ) -> Result<Option<f64>, RepoError> {
        self.ensure_refined_table()?;
        let (a, b) = (self.live_emitter_id(a)?, self.live_emitter_id(b)?);
        let tx = self.read_tx()?;
        same_emission_score(&tx, a, b, tol)
    }

    /// T-082: the live, listed emitters observing the same emission as `id`, closest first.
    pub fn same_emission_partners(
        &self,
        id: EmitterId,
        tol: &Tolerances,
    ) -> Result<Vec<(EmitterId, f64)>, RepoError> {
        self.ensure_refined_table()?;
        let id = self.live_emitter_id(id)?;
        let tx = self.read_tx()?;
        same_emission_partners(&tx, id, tol)
    }

    /// T-082: merges two inventory entries of one emission into one ([`crate::cluster`], "Same
    /// emission"). The survivor is the confirmed entry, else the first seen, else the larger
    /// count; observations the survivor already covers in time are not counted again. Everything
    /// else follows [`Self::merge_emitters`]. `Ok(None)` when the two are not the same emission
    /// (including the same live emitter, a deleted entry, or two identities).
    pub fn merge_same_emission(
        &mut self,
        a: EmitterId,
        b: EmitterId,
        t: Timestamp,
        reason: &str,
        tol: &Tolerances,
    ) -> Result<Option<EmitterMerge>, RepoError> {
        self.ensure_refined_table()?;
        let tx = self.write_tx()?;
        let m = merge_same_emission_rows(&tx, a, b, t, reason, tol)?;
        tx.commit()?;
        Ok(m)
    }

    /// The live emitter `id` resolves to (itself unless merged).
    pub fn live_emitter_id(&self, id: EmitterId) -> Result<EmitterId, RepoError> {
        live_id(&self.conn, id)?.ok_or_else(|| RepoError::NotFound {
            kind: "emitter",
            id: id.to_string(),
        })
    }

    /// Merges an emitter took part in (as absorbed or survivor), oldest first.
    pub fn emitter_merges(&self, id: EmitterId) -> Result<Vec<EmitterMerge>, RepoError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT from_emitter, into_emitter, t, reason, from_count, identity_moved \
             FROM emitter_merge WHERE from_emitter = ?1 OR into_emitter = ?1 ORDER BY merge_id",
        )?;
        let rows = stmt
            .query_map([blob(id)], |r| {
                Ok(EmitterMerge {
                    from: eid(r.get(0)?),
                    into: eid(r.get(1)?),
                    t: Timestamp::from_unix_nanos(r.get(2)?),
                    reason: r.get(3)?,
                    from_count: r.get::<_, i64>(4)? as u64,
                    identity_moved: r.get(5)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Every link ever written from an emitter, including superseded ones, oldest first.
    pub fn emitter_link_history(&self, id: EmitterId) -> Result<Vec<LinkRecord>, RepoError> {
        type Raw = (String, [u8; 16], i64, Option<[u8; 16]>, Option<i64>);
        let rows: Vec<Raw> = {
            let mut stmt = self.conn.prepare_cached(
                "SELECT target_kind, target_id, linked_at, superseded_by, superseded_at \
                 FROM emitter_link WHERE emitter_id = ?1 ORDER BY linked_at, target_kind, target_id",
            )?;
            stmt.query_map([blob(id)], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<Result<_, _>>()?
        };
        rows.into_iter()
            .map(|(kind, target, at, by, sup_at)| {
                Ok(LinkRecord {
                    link: EmitterLink {
                        emitter_id: id,
                        target: link_target(&kind, Uuid::from_bytes(target))?,
                        linked_at: Timestamp::from_unix_nanos(at),
                    },
                    superseded_by: by.map(eid),
                    superseded_at: sup_at.map(Timestamp::from_unix_nanos),
                })
            })
            .collect()
    }

    /// An emitter's classification history with recorded inputs, oldest first.
    pub fn classification_history(
        &self,
        id: EmitterId,
    ) -> Result<Vec<RecordedClassification>, RepoError> {
        type Raw = (
            i64,
            String,
            f64,
            f64,
            String,
            Option<String>,
            Option<[u8; 16]>,
            Option<i64>,
        );
        let rows: Vec<Raw> = {
            let mut stmt = self.conn.prepare_cached(
                "SELECT t, family, confidence, open_set_score, model_version, input_kind, input_id, \
                 feature_set_version FROM emitter_classification WHERE emitter_id = ?1 \
                 ORDER BY classification_id",
            )?;
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
                ))
            })?
            .collect::<Result<_, _>>()?
        };
        rows.into_iter()
            .map(
                |(t, family, confidence, open_set_score, model_version, kind, input, v)| {
                    Ok(RecordedClassification {
                        classification: Classification {
                            t: Timestamp::from_unix_nanos(t),
                            family,
                            confidence,
                            open_set_score,
                            model_version,
                        },
                        input: match (kind, input) {
                            (Some(k), Some(i)) => Some(link_target(&k, Uuid::from_bytes(i))?),
                            _ => None,
                        },
                        feature_set_version: v.map(|v| v as u32),
                    })
                },
            )
            .collect()
    }

    /// An emitter's stored fingerprint (current feature set only).
    pub fn emitter_fingerprint(&self, id: EmitterId) -> Result<Option<Fingerprint>, RepoError> {
        Ok(load_row(&self.conn, id)?.fingerprint)
    }

    /// Replaces an emitter's fingerprint (e.g. after re-estimation or a user edit).
    pub fn update_emitter_fingerprint(
        &mut self,
        id: EmitterId,
        fingerprint: &Fingerprint,
    ) -> Result<(), RepoError> {
        let n = self.conn.execute(
            "UPDATE emitter SET fingerprint = ?1 WHERE emitter_id = ?2",
            params![fingerprint_text(fingerprint)?, blob(id)],
        )?;
        if n == 0 {
            return Err(RepoError::NotFound {
                kind: "emitter",
                id: id.to_string(),
            });
        }
        Ok(())
    }

    /// Live emitters within tolerance of a fingerprint, best first, with their scores.
    pub fn emitters_matching_fingerprint(
        &self,
        fingerprint: &Fingerprint,
        tol: &Tolerances,
    ) -> Result<Vec<(EmitterId, f64)>, RepoError> {
        let tx = self.read_tx()?;
        Ok(fingerprint_candidates(&tx, fingerprint, tol)?
            .into_iter()
            .map(|c| (c.id, c.score))
            .collect())
    }

    /// The inventory: live (unmerged) emitters matching every filter, most recently seen first,
    /// one page. Identities are gated by content class ([`crate::cluster`], fail closed).
    pub fn query_inventory(&self, q: &InventoryQuery) -> Result<InventoryPage, RepoError> {
        let limit = q.limit.clamp(1, MAX_INVENTORY_PAGE);
        let tx = self.read_tx()?;
        let mut sql = String::from("SELECT emitter_id FROM emitter WHERE merged_into IS NULL");
        let mut p: Vec<SqlValue> = Vec::new();
        if let Some(f) = q.freq {
            let b = region_bounds(&tx, "emitter", &Region::new(f, ANY_TIME))?;
            sql.push_str(" AND f_lo BETWEEN ? AND ? AND f_hi >= ?");
            p.extend([b.f_lo_min, b.hi, b.lo].map(SqlValue::Real));
        }
        if let Some(t) = q.time {
            sql.push_str(" AND last_seen >= ? AND first_seen <= ?");
            p.push(SqlValue::Integer(t.start.as_unix_nanos()));
            p.push(SqlValue::Integer(t.end.as_unix_nanos()));
        }
        if q.states.is_empty() {
            sql.push_str(" AND lifecycle_state != 'deleted'");
        } else {
            sql.push_str(" AND lifecycle_state IN (");
            for (i, s) in q.states.iter().enumerate() {
                sql.push_str(if i == 0 { "?" } else { ", ?" });
                p.push(SqlValue::Text(enum_text(s)?));
            }
            sql.push(')');
        }
        if !q.status.is_empty() {
            sql.push_str(
                " AND (SELECT s.status FROM emitter_status s WHERE s.emitter_id = \
                 emitter.emitter_id ORDER BY s.status_id DESC LIMIT 1) IN (",
            );
            for (i, s) in q.status.iter().enumerate() {
                sql.push_str(if i == 0 { "?" } else { ", ?" });
                p.push(SqlValue::Text(enum_text(s)?));
            }
            sql.push(')');
        }
        if let Some(tag) = &q.tag {
            sql.push_str(
                " AND EXISTS (SELECT 1 FROM emitter_tag g WHERE g.emitter_id = \
                 emitter.emitter_id AND g.tag = ?)",
            );
            p.push(SqlValue::Text(tag.clone()));
        }
        if let Some(scheme) = &q.identity_scheme {
            sql.push_str(" AND identity_scheme = ?");
            p.push(SqlValue::Text(scheme.as_string()));
        }
        if let Some(family) = &q.family {
            sql.push_str(
                " AND coalesce((SELECT c.family FROM emitter_classification c WHERE \
                 c.emitter_id = emitter.emitter_id ORDER BY c.classification_id DESC LIMIT 1), \
                 json_extract(fingerprint, '$.family')) = ?",
            );
            p.push(SqlValue::Text(family.clone()));
        }
        sql.push_str(" ORDER BY last_seen DESC, emitter_id");
        // T-036/T-038: a tag outside the vocabulary never matches a row whose identity is
        // withheld (such tags are hidden there), so such a filter is paged after gating.
        let tag_needs_gate = q.tag.as_deref().is_some_and(|t| !tag_in_vocabulary(t));
        if !tag_needs_gate {
            sql.push_str(" LIMIT ? OFFSET ?");
            p.push(SqlValue::Integer(i64::from(limit) + 1));
            p.push(SqlValue::Integer(
                i64::try_from(q.offset).unwrap_or(i64::MAX),
            ));
        }
        let ids: Vec<[u8; 16]> = {
            let mut stmt = tx.prepare(&sql)?;
            stmt.query_map(params_from_iter(p), |r| r.get(0))?
                .collect::<Result<_, _>>()?
        };
        let mut entries = Vec::with_capacity(ids.len().min(limit as usize + 1));
        let mut skip = if tag_needs_gate { q.offset } else { 0 };
        for raw in ids {
            let emitter = self.emitter_ungated(eid(raw))?;
            let entry = gate_entry(&tx, emitter, q.access)?;
            if tag_needs_gate {
                if matches!(entry.identity, InventoryIdentity::Withheld { .. }) {
                    continue;
                }
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
            }
            entries.push(entry);
            if entries.len() > limit as usize {
                break;
            }
        }
        let more = entries.len() > limit as usize;
        entries.truncate(limit as usize);
        Ok(InventoryPage {
            entries,
            next_offset: more.then_some(q.offset + u64::from(limit)),
        })
    }
}
