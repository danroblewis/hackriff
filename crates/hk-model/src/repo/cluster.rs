//! Entity resolution and inventory queries (C18, C27; T-018). Rules and gating: [`crate::cluster`].

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use uuid::Uuid;

use super::inventory::{
    emitter_id_by_identity, identity_label, insert_status, link_kind, link_target,
};
use super::{
    RepoError, Repository, blob, bump_extent, enum_parse, enum_text, finite, int, region_bounds,
};
use crate::cluster::{
    Assignment, ConflictReason, EmitterMerge, Fingerprint, IdentityAccess, IdentityClaim,
    IdentityConflictReport, InventoryEntry, InventoryIdentity, InventoryPage, InventoryQuery,
    KnownStatusPrior, LinkRecord, MAX_INVENTORY_PAGE, MeasurementKey, RecordedClassification,
    Resolution, Sighting, Tolerances, known_family, most_restrictive,
};
use crate::content::ContentClass;
use crate::emitter::{
    Classification, DecodedIdentity, Emitter, EmitterLink, Identity, KnownStatus,
    KnownStatusChange, StatusAuthor,
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
struct EmitterRow {
    f_center: f64,
    bandwidth: f64,
    first: i64,
    last: i64,
    count: i64,
    fingerprint: Option<Fingerprint>,
    identity: Option<DecodedIdentity>,
    class: Option<ContentClass>,
    merged: bool,
}

fn load_row(conn: &Connection, id: EmitterId) -> Result<EmitterRow, RepoError> {
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
    );
    let raw: Raw = conn
        .prepare_cached(
            "SELECT f_center, bandwidth, first_seen, last_seen, count, fingerprint, \
             identity_scheme, identity_value, identity_class, merged_into \
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
            ))
        })
        .optional()?
        .ok_or_else(|| RepoError::NotFound {
            kind: "emitter",
            id: id.to_string(),
        })?;
    let (f_center, bandwidth, first, last, count, fp, scheme, value, class, merged) = raw;
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
    })
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
fn derived_identity_class(
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
                (true, Some(class)) => InventoryIdentity::Clear {
                    identity: d.clone(),
                    class,
                },
                _ => InventoryIdentity::Withheld {
                    scheme: d.scheme.clone(),
                    class,
                },
            }
        }
    };
    if !matches!(identity, InventoryIdentity::Clear { .. }) {
        emitter.identity = Identity::Unknown;
    }
    Ok(InventoryEntry {
        emitter,
        identity,
        family,
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
        let Some(live) = live_id(conn, eid(e))? else {
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

/// Merges `from` into `into` inside the caller's transaction. See [`Repository::merge_emitters`].
fn merge_rows(
    conn: &Connection,
    from: EmitterId,
    into: EmitterId,
    t: Timestamp,
    reason: &str,
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
    if let (Some(ai), Some(_)) = (&a.identity, &b.identity) {
        return Err(RepoError::IdentityConflict {
            identity: identity_label(ai),
            existing: into,
        });
    }
    let identity_moved = a.identity.is_some();
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
        a.count,
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
             WHERE f_lo BETWEEN ?1 AND ?2 AND f_hi >= ?3 AND merged_into IS NULL",
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
        Some(c) => live_id(conn, c)?,
        None => None,
    };
    if let Some(claim) = &s.identity {
        let holder = match emitter_id_by_identity(conn, &claim.identity)? {
            Some(h) => live_id(conn, h)?,
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
    match &row.identity {
        Some(d) if *d == claim.identity => {
            let base = match row.class {
                Some(c) => Some(c),
                None => derived_identity_class(conn, target, d)?,
            };
            let class = base.map_or(claim.content_class, |c| {
                most_restrictive(c, claim.content_class)
            });
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
                enum_text(&claim.content_class)?,
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
    let mut report = None;
    let mut merge = None;
    let (target, assignment, add) = match ledger {
        Some((e, counted)) => {
            let live = live_id(conn, eid(e))?.ok_or_else(|| {
                RepoError::Invalid(format!(
                    "observation ledger names missing emitter {}",
                    eid(e)
                ))
            })?;
            (
                Some(live),
                Assignment::Replay,
                s.count.saturating_sub(counted as u64),
            )
        }
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
    for tag in &s.tags {
        conn.prepare_cached("INSERT OR IGNORE INTO emitter_tag (emitter_id, tag) VALUES (?1, ?2)")?
            .execute(params![blob(id), tag])?;
    }
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
        let tx = self.write_tx()?;
        let r = resolve(&tx, s, key, tol, priors)?;
        tx.commit()?;
        Ok(r)
    }

    /// Merges `from` into `into`: counts summed, seen spans widened, fingerprints folded, tags
    /// copied, the observation ledger and live links re-pointed (old links superseded, not
    /// deleted), an identity moved when only `from` has one, and `from.merged_into = into`
    /// (chains flattened). Refused when both hold identities ([`RepoError::IdentityConflict`]) or
    /// either is already merged.
    pub fn merge_emitters(
        &mut self,
        from: EmitterId,
        into: EmitterId,
        t: Timestamp,
        reason: &str,
    ) -> Result<EmitterMerge, RepoError> {
        let tx = self.write_tx()?;
        let m = merge_rows(&tx, from, into, t, reason)?;
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
        sql.push_str(" ORDER BY last_seen DESC, emitter_id LIMIT ? OFFSET ?");
        p.push(SqlValue::Integer(i64::from(limit) + 1));
        p.push(SqlValue::Integer(
            i64::try_from(q.offset).unwrap_or(i64::MAX),
        ));
        let mut ids: Vec<[u8; 16]> = {
            let mut stmt = tx.prepare(&sql)?;
            stmt.query_map(params_from_iter(p), |r| r.get(0))?
                .collect::<Result<_, _>>()?
        };
        let more = ids.len() > limit as usize;
        ids.truncate(limit as usize);
        let mut entries = Vec::with_capacity(ids.len());
        for raw in ids {
            let emitter = self.emitter_ungated(eid(raw))?;
            entries.push(gate_entry(&tx, emitter, q.access)?);
        }
        Ok(InventoryPage {
            entries,
            next_offset: more.then_some(q.offset + u64::from(limit)),
        })
    }
}
