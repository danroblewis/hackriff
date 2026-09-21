//! T-598 (AWARE-011): **persisting** the cross-centre retune verdict, so the record is
//! load-bearing.
//!
//! # What was wrong
//!
//! T-586 built the measurement ([`crate::retune`]): project every detection into the invariant
//! coordinate `f − slope·f_LO`, using the LO the detection's own [`Provenance`](crate::Provenance)
//! records, and a line that tracks the local oscillator is manufactured inside the receiver. It
//! proved out on a three-centre scene — real emitters agreeing to 0–1 Hz across centres, artefact
//! families to 2–3 Hz — and then **threw the verdict away**. Nothing wrote it back, so one
//! LO-relative spur seen from three centres stayed three inventory rows at three different
//! absolute frequencies. That is exactly the user's 2026-09-21 field report: a bright signal that
//! "moved" when the radio was retuned.
//!
//! The single-capture flaggers cannot close it, and T-586 measured that too: they caught the DC
//! family and caught a +370 kHz LO-relative spur **not at all**. Three detections, three absolute
//! frequencies, each individually indistinguishable from a real emission. The information exists
//! only in the relationship *between* centres.
//!
//! # What is persisted, and where
//!
//! [`Repository::resolve_retune`] runs the classifier over the stored rows around one emitter and
//! writes two things:
//!
//! 1. **The verdict about each detection** — `absolute`, `lo-locked` or `image`, with the
//!    invariant coordinate and how many distinct LOs decided it, in the append-only
//!    `detection_retune` record. A detection row is **immutable** — it is what was measured, and
//!    an interpretation must never rewrite it — so the verdict is its own claim *about* the
//!    measurement, in the same discipline as an emitter relationship. The flag bits it implies
//!    ([`RetuneSlope::apply`]: `spur_candidate` + `SpurReason::LoRelative`, or a retune-confirmed
//!    image) are carried on the verdict and applied to the `flags` a reader sees
//!    ([`Repository::detection`], [`Repository::detections_in_region`]), so what the row says it
//!    is now includes what the retune found, without the measurement being edited.
//! 2. **The relationship between the sightings** — the rows of one LO-relative family are related
//!    by [`RelationKind::RetuneSiblingOf`] to the one that represents them, so `/api/inventory`
//!    lists **one** artefact instead of N emitters. Nothing is deleted: every sighting keeps its
//!    id, its detections, its time extent `[start, end?]` and its history, and is still reachable
//!    by id or with `relations=all`.
//!
//! # Three properties the signal model demands
//!
//! - **An artefact is still a time–frequency region with a real extent.** The sightings are
//!   *related*, never deleted or collapsed into a point: an artefact is not an exception to the
//!   one unified `[start, end?]` shape, and its ephemera are as first-class as any other.
//! - **"This is LO-relative" is revocable**, the same way a detected end is provisional. A verdict
//!   is a function of the centres seen so far, and a fourth centre can overturn it. So every write
//!   here is reversible *exactly*: nothing is ever unset, because nothing is ever set — the
//!   measured flags stay as measured, the verdict is a separate claim, and revoking it (an
//!   appended `active = 0` row, never an edit) restores what the single-capture flaggers said by
//!   construction. A standing sibling relation is revoked the same way.
//! - **`Absolute` is the weak claim.** It means "not LO-relative", **not** "real". A
//!   reference-clock harmonic is absolute-fixed and still an artefact. So an absolute verdict is
//!   recorded (it is evidence, and it revokes a previous LO-relative claim) but sets no flag,
//!   clears no other mechanism's flag, and never promotes anything.
//!
//! # What bounds it
//!
//! A verdict needs diversity, so nothing is claimed below [`RetuneTolerance::min_centres`]
//! distinct LOs — silence, not a guess. The neighbourhood is bounded on every axis:
//! [`MAX_RETUNE_ROWS`] rows, [`MAX_RETUNE_DETECTIONS`] detections per row, and a band no wider
//! than the LOs actually visited within [`MAX_LO_SPAN_HZ`] of this row's own LO, so a survey that
//! crossed gigahertz never turns one sighting into a whole-spectrum query.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, params};

use super::relate::{CURRENT_RELATION_SQL, insert_relation, read_relations};
use super::{RepoError, Repository, blob};
use crate::detection::{Detection, DetectionFlags};
use crate::ids::{DetectionId, EmitterId};
use crate::relate::{RelationAuthor, RelationClaim, RelationKind};
use crate::retune::{RetuneObservation, RetuneSlope, RetuneTolerance, classify};
use crate::time::Timestamp;

/// Rows examined around one seed. A retune neighbourhood is a handful of lines over a handful of
/// centres; this bounds a pathological band, not the normal case.
pub const MAX_RETUNE_ROWS: usize = 64;

/// Detections read per row, most recent first. Enough to cover every centre a row was seen at.
pub const MAX_RETUNE_DETECTIONS: usize = 32;

/// How far from the seed's own LO another tuning centre may be and still be compared, Hz.
///
/// Retune diversity is a statement about one region looked at from several centres. Centres a
/// gigahertz apart share no coverage, and comparing them would only widen the search band for
/// nothing. 100 MHz is far wider than the 20 MHz instantaneous bandwidth the front end can hold,
/// so every centre that could have seen the same air is included.
pub const MAX_LO_SPAN_HZ: f64 = 100e6;

/// Widest band the neighbourhood query may cover, Hz, whatever the LO span says.
pub const MAX_RETUNE_BAND_HZ: f64 = 500e6;

/// `?1` emitter id, `?2` row cap. Mirrors `EMITTER_DETECTION_EVIDENCE_SQL`: a row's detections
/// through its currently-linked tracks, or linked directly, most recent first — with the LO each
/// was measured under, read from the detection's own provenance.
const RETUNE_DETECTION_SQL: &str = "\
     SELECT detection_id, f_center, obw, lo FROM ( \
       SELECT d.detection_id AS detection_id, d.f_center AS f_center, d.obw AS obw, \
              d.t_start AS t_start, \
              json_extract(p.canonical, '$.tune.center_hz') AS lo \
       FROM emitter_link el \
       JOIN track_detection td ON td.track_id = el.target_id \
       JOIN detection d ON d.detection_id = td.detection_id \
       JOIN provenance p ON p.provenance_id = d.provenance_id \
       WHERE el.emitter_id = ?1 AND el.target_kind = 'track' AND el.superseded_by IS NULL \
       UNION ALL \
       SELECT d.detection_id AS detection_id, d.f_center AS f_center, d.obw AS obw, \
              d.t_start AS t_start, \
              json_extract(p.canonical, '$.tune.center_hz') AS lo \
       FROM emitter_link el \
       JOIN detection d ON d.detection_id = el.target_id \
       JOIN provenance p ON p.provenance_id = d.provenance_id \
       WHERE el.emitter_id = ?1 AND el.target_kind = 'detection' AND el.superseded_by IS NULL \
     ) WHERE lo IS NOT NULL ORDER BY t_start DESC LIMIT ?2";

/// Live, listed rows whose occupied band overlaps `?1..?2`, most recently seen first.
const RETUNE_ROWS_SQL: &str = "\
     SELECT emitter_id FROM emitter \
     WHERE merged_into IS NULL AND lifecycle_state != 'deleted' AND f_hi >= ?1 AND f_lo <= ?2 \
     ORDER BY last_seen DESC, emitter_id LIMIT ?3";

/// What the cross-centre test found about one detection.
///
/// [`RetuneSlope::Absolute`] is the **weak** claim — "not LO-relative", never "real": a
/// reference-clock harmonic is absolute-fixed and still an artefact, so it implies no flag bits
/// and clears nothing another mechanism found.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetuneVerdict {
    /// How the line's absolute frequency moved with the LO.
    pub slope: RetuneSlope,
    /// The invariant coordinate `f − slope·f_LO`, Hz.
    pub invariant_hz: f64,
    /// Distinct LOs that decided it. Never below [`RetuneTolerance::min_centres`].
    pub centres: usize,
    /// Spread of the invariant coordinate over the group, Hz.
    pub spread_hz: Option<f64>,
    /// The flag bits this slope implies ([`RetuneSlope::apply`] over empty flags).
    pub flag_bits: u32,
}

impl RetuneVerdict {
    /// The verdict a group gives its members.
    fn of(slope: RetuneSlope, invariant_hz: f64, centres: usize, spread_hz: f64) -> Self {
        let mut flags = DetectionFlags::default();
        slope.apply(&mut flags);
        Self {
            slope,
            invariant_hz,
            centres,
            spread_hz: Some(spread_hz),
            flag_bits: flags.bits(),
        }
    }
}

/// One sighting of one retune family: the rows that hold it and the one that represents them.
#[derive(Clone, Debug, PartialEq)]
pub struct RetuneFamily {
    /// The measured slope. Never [`RetuneSlope::Absolute`] — an absolute line defers to nothing.
    pub slope: RetuneSlope,
    /// The invariant coordinate `f − slope·f_LO`, Hz: the LO offset for an LO-locked family.
    pub invariant_hz: f64,
    /// The row that represents the family in the inventory.
    pub primary: EmitterId,
    /// The rows relating to it, which the inventory hides by default.
    pub deferred: Vec<EmitterId>,
    /// Distinct LOs the family was measured over.
    pub centres: usize,
}

/// What one [`Repository::resolve_retune`] pass compared and wrote.
///
/// The counts exist to be asserted on: a cross-centre check over one centre compares nothing and
/// would otherwise pass every assertion while testing nothing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RetuneOutcome {
    /// Inventory rows compared.
    pub rows: usize,
    /// Detections compared.
    pub observations: usize,
    /// Distinct LOs among them. Below [`RetuneTolerance::min_centres`] nothing is claimed.
    pub centres: usize,
    /// The LO-relative / image families found, each now one inventory row.
    pub families: Vec<RetuneFamily>,
    /// Detections whose stored verdict was written or changed.
    pub detections_marked: usize,
    /// Detections whose stored verdict was withdrawn (pre-verdict flags restored).
    pub detections_cleared: usize,
    /// Rows whose standing sibling relation was revoked by this pass.
    pub revoked: Vec<EmitterId>,
}

impl RetuneOutcome {
    /// Rows the inventory now hides as siblings of a represented artefact.
    pub fn deferred(&self) -> usize {
        self.families.iter().map(|f| f.deferred.len()).sum()
    }
}

/// Rule id written on every claim this module makes.
pub const RETUNE_RULE: &str = "hk-model/retune-diversity@1";

impl Repository {
    /// Classifies the rows around `seed` by how their absolute frequency tracks the LO, and
    /// **persists what it finds** (module docs for the rules and the bounds).
    ///
    /// Returns what it compared and wrote. It claims nothing below
    /// [`RetuneTolerance::min_centres`] distinct LOs, mutates no other row's measurements, and
    /// deletes nothing.
    pub fn resolve_retune(
        &mut self,
        seed: EmitterId,
        actor: &str,
        t: Timestamp,
        tol: &RetuneTolerance,
    ) -> Result<RetuneOutcome, RepoError> {
        let tx = self.write_tx()?;
        let out = resolve(&tx, seed, actor, t, tol)?;
        tx.commit()?;
        Ok(out)
    }

    /// How many distinct tuning centres the stored provenance holds — the cheap question "is
    /// there any retune diversity to reason about at all?", answered from
    /// `idx_provenance_tune_center` rather than by resolving anything.
    ///
    /// Below 2 no verdict is possible, so a caller can skip the whole pass; a change in this count
    /// is exactly the event that can change a verdict, which is what bounds how often the pass
    /// runs (`hk_pipeline::inventory`).
    pub fn tune_centre_count(&self) -> Result<usize, RepoError> {
        let n: i64 = self.conn.query_row(
            "SELECT count(DISTINCT json_extract(canonical, '$.tune.center_hz')) FROM provenance \
             WHERE json_extract(canonical, '$.tune.center_hz') IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        Ok(usize::try_from(n).unwrap_or(0))
    }

    /// The standing retune verdict about a detection, if one has been recorded.
    ///
    /// `None` means nothing has been claimed (or the claim was revoked) — never "it is real".
    pub fn detection_retune(&self, id: DetectionId) -> Result<Option<RetuneVerdict>, RepoError> {
        standing_verdict(&self.conn, id)
    }

    /// Every verdict ever claimed or revoked about a detection, oldest first: the audit behind a
    /// `lo-relative` flag, and the record of a claim later overturned. Nothing here is deleted.
    pub fn detection_retune_history(
        &self,
        id: DetectionId,
    ) -> Result<Vec<(RetuneVerdict, bool, Timestamp)>, RepoError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT slope, invariant_hz, centres, spread_hz, flag_bits, active, t \
             FROM detection_retune WHERE detection_id = ?1 ORDER BY verdict_id",
        )?;
        type Raw = (String, f64, i64, Option<f64>, i64, bool, i64);
        let raw: Vec<Raw> = stmt
            .query_map([blob(id)], |r| {
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
            .collect::<Result<_, _>>()?;
        raw.into_iter()
            .map(
                |(slope, invariant_hz, centres, spread_hz, bits, active, t)| {
                    Ok((
                        RetuneVerdict {
                            slope: slope_from(&slope)?,
                            invariant_hz,
                            centres: usize::try_from(centres).unwrap_or(0),
                            spread_hz,
                            flag_bits: u32::try_from(bits).unwrap_or(0),
                        },
                        active,
                        Timestamp::from_unix_nanos(t),
                    ))
                },
            )
            .collect()
    }
}

/// The band around `f_center` a retune neighbourhood must cover to hold this row's siblings.
///
/// An LO-locked line moves with the LO and an image moves at twice its rate, so the widest a
/// sibling can be from this sighting is `2·(lo_max − lo_min)` plus the room the measurement takes.
fn search_band(f_center: f64, bandwidth: f64, los: &[f64]) -> (f64, f64) {
    let (lo_min, lo_max) = los
        .iter()
        .fold((f64::MAX, f64::MIN), |(a, b), l| (a.min(*l), b.max(*l)));
    let span = if los.is_empty() {
        0.0
    } else {
        (lo_max - lo_min).max(0.0)
    };
    let half = (2.0 * span + bandwidth.max(0.0)).min(MAX_RETUNE_BAND_HZ / 2.0);
    (f_center - half, f_center + half)
}

/// Distinct tuning centres within [`MAX_LO_SPAN_HZ`] of `lo`, from the provenance the detections
/// already carry. Bounded and ascending.
fn nearby_los(conn: &Connection, lo: f64) -> Result<Vec<f64>, RepoError> {
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT json_extract(canonical, '$.tune.center_hz') AS lo FROM provenance \
         WHERE lo IS NOT NULL AND abs(lo - ?1) <= ?2 ORDER BY lo LIMIT ?3",
    )?;
    let los: Vec<f64> = stmt
        .query_map(params![lo, MAX_LO_SPAN_HZ, MAX_RETUNE_ROWS as i64], |r| {
            r.get(0)
        })?
        .collect::<Result<_, _>>()?;
    Ok(los)
}

/// One row's detections, with the LO each was measured under.
fn row_observations(
    conn: &Connection,
    id: EmitterId,
) -> Result<Vec<(DetectionId, RetuneObservation)>, RepoError> {
    let mut stmt = conn.prepare_cached(RETUNE_DETECTION_SQL)?;
    let raw: Vec<([u8; 16], f64, f64, f64)> = stmt
        .query_map(params![blob(id), MAX_RETUNE_DETECTIONS as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<Result<_, _>>()?;
    Ok(raw
        .into_iter()
        .filter(|(_, f, _, lo)| f.is_finite() && lo.is_finite())
        .map(|(d, f_center_hz, bandwidth_hz, lo_hz)| {
            (
                DetectionId::from_uuid(uuid::Uuid::from_bytes(d)),
                RetuneObservation {
                    lo_hz,
                    f_center_hz,
                    bandwidth_hz,
                },
            )
        })
        .collect())
}

/// The standing verdict about `id`: the highest `verdict_id` still in force, or `None`.
pub(super) fn standing_verdict(
    conn: &Connection,
    id: DetectionId,
) -> Result<Option<RetuneVerdict>, RepoError> {
    let row: Option<(String, f64, i64, Option<f64>, i64)> = conn
        .prepare_cached(
            "SELECT slope, invariant_hz, centres, spread_hz, flag_bits FROM detection_retune \
             WHERE detection_id = ?1 AND active = 1 \
               AND verdict_id = (SELECT max(verdict_id) FROM detection_retune \
                                 WHERE detection_id = ?1)",
        )?
        .query_row([blob(id)], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .optional()?;
    let Some((slope, invariant_hz, centres, spread_hz, bits)) = row else {
        return Ok(None);
    };
    Ok(Some(RetuneVerdict {
        slope: slope_from(&slope)?,
        invariant_hz,
        centres: usize::try_from(centres).unwrap_or(0),
        spread_hz,
        flag_bits: u32::try_from(bits).unwrap_or(0),
    }))
}

/// Applies the standing verdicts about `dets` to the flags a reader sees.
///
/// The stored row is untouched and untouchable: these bits are the verdict's, carried on the
/// verdict and OR'd in here, so a revocation takes them away again by construction.
pub(super) fn apply_verdicts(conn: &Connection, dets: &mut [Detection]) -> Result<(), RepoError> {
    for d in dets {
        let Some(v) = standing_verdict(conn, d.id)? else {
            continue;
        };
        v.slope.apply(&mut d.flags);
    }
    Ok(())
}

fn slope_from(text: &str) -> Result<RetuneSlope, RepoError> {
    Ok(match text {
        "absolute" => RetuneSlope::Absolute,
        "lo-locked" => RetuneSlope::LoLocked,
        "image" => RetuneSlope::Image,
        other => {
            return Err(RepoError::Invalid(format!(
                "unknown retune slope {other:?}"
            )));
        }
    })
}

/// Records (or withdraws) the verdict about one detection. Returns whether anything changed.
///
/// A verdict identical to the one standing is left alone: re-running the classifier over the same
/// evidence must not grow the record. Anything else appends — a revocation of what stood, then the
/// new claim if there is one.
fn write_verdict(
    conn: &Connection,
    id: DetectionId,
    verdict: Option<RetuneVerdict>,
    actor: &str,
    t: Timestamp,
) -> Result<bool, RepoError> {
    let standing = standing_verdict(conn, id)?;
    if standing.map(|v| v.slope) == verdict.map(|v| v.slope) {
        return Ok(false);
    }
    if let Some(prev) = standing {
        append_verdict(conn, id, prev, false, actor, t)?;
    }
    if let Some(v) = verdict {
        append_verdict(conn, id, v, true, actor, t)?;
    }
    Ok(true)
}

fn append_verdict(
    conn: &Connection,
    id: DetectionId,
    v: RetuneVerdict,
    active: bool,
    actor: &str,
    t: Timestamp,
) -> Result<(), RepoError> {
    conn.prepare_cached(
        "INSERT INTO detection_retune (detection_id, slope, invariant_hz, centres, spread_hz, \
         flag_bits, active, t, actor) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?
    .execute(params![
        blob(id),
        v.slope.as_str(),
        v.invariant_hz,
        v.centres as i64,
        v.spread_hz,
        i64::from(v.flag_bits),
        i64::from(active),
        t.as_unix_nanos(),
        actor,
    ])?;
    Ok(())
}

/// Whether a standing claim came from this rule.
fn is_retune_claim(r: &crate::relate::EmitterRelation) -> bool {
    r.kind == RelationKind::RetuneSiblingOf
}

/// Sort key for the row that represents a family: the one holding the most of its sightings, then
/// the first seen, then the id — deterministic on every replay.
fn family_primary(
    members: &BTreeMap<EmitterId, usize>,
    first_seen: &BTreeMap<EmitterId, i64>,
) -> EmitterId {
    let mut best: Option<(usize, i64, EmitterId)> = None;
    for (id, count) in members {
        let key = (
            *count,
            -first_seen.get(id).copied().unwrap_or(i64::MAX),
            *id,
        );
        if best.is_none_or(|b| key > (b.0, b.1, b.2)) {
            best = Some(key);
        }
    }
    best.map(|b| b.2).expect("a family has members")
}

fn resolve(
    conn: &Connection,
    seed: EmitterId,
    actor: &str,
    t: Timestamp,
    tol: &RetuneTolerance,
) -> Result<RetuneOutcome, RepoError> {
    let mut out = RetuneOutcome::default();
    let Some(live) = super::cluster::live_id(conn, seed)? else {
        return Ok(out);
    };
    let seed_row: Option<(f64, f64, String)> = conn
        .prepare_cached(
            "SELECT f_center, bandwidth, lifecycle_state FROM emitter \
             WHERE emitter_id = ?1 AND merged_into IS NULL",
        )?
        .query_row([blob(live)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    let Some((f_center, bandwidth, state)) = seed_row else {
        return Ok(out);
    };
    if state == "deleted" {
        return Ok(out);
    }
    let seed_obs = row_observations(conn, live)?;
    let Some(seed_lo) = seed_obs.first().map(|(_, o)| o.lo_hz) else {
        return Ok(out);
    };
    let los = nearby_los(conn, seed_lo)?;
    if los.len() < tol.min_centres {
        // No diversity has been recorded yet: the honest answer is silence, and a standing claim
        // is left alone rather than revoked on no evidence.
        return Ok(out);
    }
    let (f_lo, f_hi) = search_band(f_center, bandwidth, &los);

    // The neighbourhood: every live row whose band a sibling of this one could occupy.
    let rows: Vec<EmitterId> = {
        let mut stmt = conn.prepare_cached(RETUNE_ROWS_SQL)?;
        stmt.query_map(params![f_lo, f_hi, MAX_RETUNE_ROWS as i64], |r| {
            r.get::<_, [u8; 16]>(0)
        })?
        .map(|r| r.map(|b| EmitterId::from_uuid(uuid::Uuid::from_bytes(b))))
        .collect::<Result<_, _>>()?
    };
    out.rows = rows.len();

    let mut obs: Vec<RetuneObservation> = Vec::new();
    let mut owner: Vec<(EmitterId, DetectionId)> = Vec::new();
    let mut first_seen: BTreeMap<EmitterId, i64> = BTreeMap::new();
    let mut confirmed: BTreeMap<EmitterId, bool> = BTreeMap::new();
    for id in &rows {
        let meta: Option<(i64, String)> = conn
            .prepare_cached(
                "SELECT first_seen, lifecycle_state FROM emitter WHERE emitter_id = ?1",
            )?
            .query_row([blob(*id)], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        if let Some((seen, state)) = meta {
            first_seen.insert(*id, seen);
            confirmed.insert(*id, state == "confirmed");
        }
        for (det, o) in row_observations(conn, *id)? {
            obs.push(o);
            owner.push((*id, det));
        }
    }
    out.observations = obs.len();
    let summary = classify(&obs, tol);
    out.centres = summary.centres();
    if summary.centres() < tol.min_centres {
        return Ok(out);
    }

    // --- 1. The verdict on each detection row -------------------------------------------------
    let mut verdict: Vec<Option<RetuneVerdict>> = vec![None; obs.len()];
    for g in &summary.groups {
        for &i in &g.members {
            verdict[i] = Some(RetuneVerdict::of(
                g.slope,
                g.invariant_hz,
                g.centres(),
                g.spread_hz,
            ));
        }
    }
    for (i, (_, det)) in owner.iter().enumerate() {
        if write_verdict(conn, *det, verdict[i], actor, t)? {
            match verdict[i] {
                Some(_) => out.detections_marked += 1,
                None => out.detections_cleared += 1,
            }
        }
    }

    // --- 2. The relationship between the sightings --------------------------------------------
    let mut deferring: BTreeMap<EmitterId, (EmitterId, RetuneFamilyClaim)> = BTreeMap::new();
    for g in summary
        .groups
        .iter()
        .filter(|g| g.slope.is_receiver_artefact())
    {
        let mut members: BTreeMap<EmitterId, usize> = BTreeMap::new();
        for &i in &g.members {
            *members.entry(owner[i].0).or_default() += 1;
        }
        if members.len() < 2 {
            // One row already holds the whole family: it is one artefact and one row, which is
            // the state this rule exists to reach. Its detections carry the verdict.
            continue;
        }
        let primary = family_primary(&members, &first_seen);
        let mut family = RetuneFamily {
            slope: g.slope,
            invariant_hz: g.invariant_hz,
            primary,
            deferred: Vec::new(),
            centres: g.centres(),
        };
        for id in members.keys().filter(|id| **id != primary) {
            // A Confirmed row is never hidden by this rule: confirmation is a verified-emitter
            // decision a person can see and undo, and a measurement must not silently retract it.
            // Its detections still carry the verdict, which is what the review reads.
            if confirmed.get(id).copied().unwrap_or(false) {
                continue;
            }
            family.deferred.push(*id);
            deferring.insert(
                *id,
                (
                    primary,
                    RetuneFamilyClaim {
                        slope: g.slope,
                        invariant_hz: g.invariant_hz,
                        centres: g.centres(),
                        spread_hz: g.spread_hz,
                        los_hz: g.los_hz.clone(),
                    },
                ),
            );
        }
        if !family.deferred.is_empty() {
            out.families.push(family);
        }
    }

    // The sweep includes any row that currently *defers to* one of the rows examined, even if it
    // is no longer live (a deleted or merged sighting): its claim was made from this evidence, so
    // it is revoked from this evidence rather than left standing on a row nothing supports.
    let mut sweep = rows.clone();
    for id in &rows {
        let mut stmt = conn.prepare_cached(
            "SELECT DISTINCT emitter_id FROM emitter_relation \
             WHERE source_id = ?1 AND active = 1 AND kind = 'retune-sibling-of' LIMIT ?2",
        )?;
        let claimants: Vec<[u8; 16]> = stmt
            .query_map(params![blob(*id), MAX_RETUNE_ROWS as i64], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for c in claimants {
            let c = EmitterId::from_uuid(uuid::Uuid::from_bytes(c));
            if !sweep.contains(&c) {
                sweep.push(c);
            }
        }
    }

    for id in &sweep {
        let standing = read_relations(conn, CURRENT_RELATION_SQL, *id)?;
        match deferring.get(id) {
            Some((primary, claim)) => {
                let reason = format!(
                    "the same {} receiver artefact as emitter {}, seen from a different tuning \
                     centre: both sit on the invariant coordinate f - {:.0}*f_LO = {:.1} Hz over \
                     {} centres (spread {:.0} Hz), so this is one artefact measured more than \
                     once, not an emission of its own (this row is kept in full, with its own \
                     time extent, and revives if the evidence changes)",
                    claim.slope.as_str(),
                    primary,
                    claim.slope.factor(),
                    claim.invariant_hz,
                    claim.centres,
                    claim.spread_hz,
                );
                let already = standing
                    .iter()
                    .any(|r| is_retune_claim(r) && r.source_id == *primary);
                // A row defers to one sibling: supersede any other standing retune claim on it.
                for r in standing
                    .iter()
                    .filter(|r| is_retune_claim(r) && r.source_id != *primary)
                {
                    insert_relation(
                        conn,
                        &RelationClaim {
                            emitter_id: *id,
                            source_id: r.source_id,
                            kind: RelationKind::RetuneSiblingOf,
                            artifact: None,
                            active: false,
                            t,
                            author: RelationAuthor::System,
                            actor: actor.to_owned(),
                            reason: format!(
                                "superseded: this row now relates to emitter {primary} instead"
                            ),
                            score: None,
                            detail: None,
                        },
                    )?;
                    out.revoked.push(*id);
                }
                if already {
                    continue;
                }
                insert_relation(
                    conn,
                    &RelationClaim {
                        emitter_id: *id,
                        source_id: *primary,
                        kind: RelationKind::RetuneSiblingOf,
                        artifact: None,
                        active: true,
                        t,
                        author: RelationAuthor::System,
                        actor: actor.to_owned(),
                        reason,
                        score: None,
                        detail: Some(serde_json::json!({
                            "rule": "retune-diversity",
                            "slope": claim.slope.as_str(),
                            "slope_factor": claim.slope.factor(),
                            "invariant_hz": claim.invariant_hz,
                            "centres": claim.centres,
                            "spread_hz": claim.spread_hz,
                            "los_hz": claim.los_hz,
                        })),
                    },
                )?;
            }
            None => {
                // Revocation: the measurement no longer puts this row in an artefact family.
                for r in standing.iter().filter(|r| is_retune_claim(r)) {
                    insert_relation(
                        conn,
                        &RelationClaim {
                            emitter_id: *id,
                            source_id: r.source_id,
                            kind: RelationKind::RetuneSiblingOf,
                            artifact: None,
                            active: false,
                            t,
                            author: RelationAuthor::System,
                            actor: actor.to_owned(),
                            reason: "the retune measurement no longer places this row on an \
                                     LO-relative invariant coordinate with that row"
                                .to_owned(),
                            score: None,
                            detail: None,
                        },
                    )?;
                    out.revoked.push(*id);
                }
            }
        }
    }
    Ok(out)
}

/// The arithmetic behind one sibling claim, disclosed on the relation.
struct RetuneFamilyClaim {
    slope: RetuneSlope,
    invariant_hz: f64,
    centres: usize,
    spread_hz: f64,
    los_hz: Vec<f64>,
}
