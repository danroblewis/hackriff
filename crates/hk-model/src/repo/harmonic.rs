//! T-374 (C40): storage and resolution for **harmonic families** — several emitters against one
//! fundamental nobody can see. Rules and thresholds: [`crate::harmonic`]; schema: migration
//! `0014_harmonic_family.sql`.
//!
//! **Append-only, and never a mutation or a delete**, exactly like [`super::relate`]: a family is
//! a claim carrying its own reasoning, and a revocation is another row pointing at the one it
//! retires. Members keep their emitter rows, detections, tracks and history throughout, because a
//! relationship is ranked evidence and never an automatic delete.
//!
//! **Device-local** (T-302/T-259): the candidate rows offered to the search are partitioned by
//! [`ReceiveChain`] first, so no fit can ever span two front ends — the chain gate in
//! [`crate::harmonic::judge_family`] is then a second, redundant guard rather than the only one.

use rusqlite::{Connection, params};

use super::{RepoError, Repository, blob};
use crate::harmonic::{FamilyMember, HarmonicFamily, find_harmonic_families};
use crate::ids::EmitterId;
use crate::relate::{ReceiveChain, RelationAuthor};
use crate::time::Timestamp;

/// Live rows offered to the family search, per receive chain. Bounded because this runs on a
/// serving path, like [`super::relate::MAX_NEIGHBOURS`].
pub const MAX_FAMILY_CANDIDATES: usize = 64;

/// One stored harmonic-family claim.
#[derive(Clone, Debug, PartialEq)]
pub struct HarmonicFamilyRow {
    /// Row id.
    pub family_id: i64,
    /// The family as fitted.
    pub family: HarmonicFamily,
    /// `true` = in force, `false` = revoked.
    pub active: bool,
    /// The row this one retires.
    pub supersedes: Option<i64>,
    /// When it was claimed or revoked.
    pub t: Timestamp,
    /// Rule or person.
    pub author: RelationAuthor,
    /// Rule id or token fingerprint.
    pub actor: String,
    /// Backend-rendered reasoning, including the arithmetic. Always disclosed.
    pub reason: String,
}

const SELECT_FAMILY: &str = "\
     SELECT family_id, device_id, antenna_port, f0_hz, intercept_hz, intercept_se_hz, index_pin, \
            origin_sigmas, origin_fraction, residual_rms_hz, residual_tolerance_hz, \
            width_sigma0_hz, width_ratio, width_index_leverage, width_separates, line_shape_corr, \
            active, supersedes, t, author, actor, reason \
     FROM harmonic_family";

/// Families currently in force: `active = 1` and not retired by a later row.
const CURRENT_FAMILIES_SQL: &str = "\
     WHERE active = 1 AND family_id NOT IN \
       (SELECT supersedes FROM harmonic_family WHERE supersedes IS NOT NULL) \
     ORDER BY family_id";

/// Every row naming this emitter, newest first.
const FAMILIES_FOR_EMITTER_SQL: &str = "\
     WHERE family_id IN (SELECT family_id FROM harmonic_family_member WHERE emitter_id = ?1) \
     ORDER BY family_id DESC";

/// The single bound parameter a listing may carry. Bound, never interpolated.
enum Param {
    Emitter(EmitterId),
    Family(i64),
}

/// One `harmonic_family` row as SQL hands it back, before its members are attached.
type RawFamily = (
    i64,
    HarmonicFamily,
    bool,
    Option<i64>,
    i64,
    String,
    String,
    String,
);

fn read_rows(
    conn: &Connection,
    tail: &str,
    param: Option<Param>,
) -> Result<Vec<HarmonicFamilyRow>, RepoError> {
    let sql = format!("{SELECT_FAMILY} {tail}");
    let mut stmt = conn.prepare_cached(&sql)?;
    let map = |r: &rusqlite::Row<'_>| -> rusqlite::Result<RawFamily> {
        Ok((
            r.get(0)?,
            HarmonicFamily {
                device_id: r.get(1)?,
                antenna_port: r.get(2)?,
                f0_hz: r.get(3)?,
                intercept_hz: r.get(4)?,
                intercept_se_hz: r.get(5)?,
                index_pin: r.get(6)?,
                origin_sigmas: r.get(7)?,
                origin_fraction: r.get(8)?,
                residual_rms_hz: r.get(9)?,
                residual_tolerance_hz: r.get(10)?,
                members: Vec::new(),
                width: crate::harmonic::WidthEvidence {
                    sigma0_hz: r.get(11)?,
                    ratio: r.get(12)?,
                    index_leverage: r.get(13)?,
                    rss_harmonic: f64::NAN,
                    rss_flat: f64::NAN,
                    separates: r.get::<_, i64>(14)? != 0,
                },
                line_shape_corr: r.get(15)?,
            },
            r.get::<_, i64>(16)? != 0,
            r.get(17)?,
            r.get(18)?,
            r.get(19)?,
            r.get(20)?,
            r.get(21)?,
        ))
    };
    let raw: Vec<_> = match param {
        Some(Param::Emitter(id)) => stmt
            .query_map(params![blob(id)], map)?
            .collect::<Result<_, _>>()?,
        Some(Param::Family(n)) => stmt.query_map(params![n], map)?.collect::<Result<_, _>>()?,
        None => stmt.query_map([], map)?.collect::<Result<_, _>>()?,
    };
    let mut out = Vec::with_capacity(raw.len());
    for (family_id, mut family, active, supersedes, t, author, actor, reason) in raw {
        family.members = read_members(conn, family_id)?;
        out.push(HarmonicFamilyRow {
            family_id,
            family,
            active,
            supersedes,
            t: Timestamp::from_unix_nanos(t),
            author: match author.as_str() {
                "user" => RelationAuthor::User,
                _ => RelationAuthor::System,
            },
            actor,
            reason,
        });
    }
    Ok(out)
}

fn read_members(
    conn: &Connection,
    family_id: i64,
) -> Result<Vec<crate::harmonic::FamilyAssignment>, RepoError> {
    let mut stmt = conn.prepare_cached(
        "SELECT emitter_id, n, f_center_hz, residual_hz, width_hz \
         FROM harmonic_family_member WHERE family_id = ?1 ORDER BY n",
    )?;
    let rows: Vec<([u8; 16], i64, f64, f64, f64)> = stmt
        .query_map([family_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows
        .into_iter()
        .map(|(id, n, f, resid, w)| crate::harmonic::FamilyAssignment {
            emitter_id: EmitterId::from_uuid(uuid::Uuid::from_bytes(id)),
            index: n as u32,
            f_center_hz: f,
            residual_hz: resid,
            width_hz: w,
            sigma0_hz: w / n as f64,
        })
        .collect())
}

/// Who is making a claim and when — the four fields every appended row carries, kept together so
/// they cannot be transposed at a call site.
#[derive(Clone, Copy)]
struct Claimant<'a> {
    t: Timestamp,
    author: RelationAuthor,
    actor: &'a str,
    reason: &'a str,
}

fn insert_family(
    conn: &Connection,
    family: &HarmonicFamily,
    active: bool,
    supersedes: Option<i64>,
    by: Claimant<'_>,
) -> Result<HarmonicFamilyRow, RepoError> {
    let Claimant {
        t,
        author,
        actor,
        reason,
    } = by;
    if family.members.len() < crate::harmonic::MIN_MEMBERS {
        return Err(RepoError::Invalid(
            "a harmonic family carries at least three members".into(),
        ));
    }
    conn.prepare_cached(
        "INSERT INTO harmonic_family (device_id, antenna_port, f0_hz, intercept_hz, \
         intercept_se_hz, index_pin, origin_sigmas, origin_fraction, residual_rms_hz, \
         residual_tolerance_hz, width_sigma0_hz, width_ratio, width_index_leverage, \
         width_separates, line_shape_corr, member_count, active, supersedes, t, author, actor, \
         reason) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, \
         ?17, ?18, ?19, ?20, ?21, ?22)",
    )?
    .execute(params![
        family.device_id,
        family.antenna_port,
        family.f0_hz,
        family.intercept_hz,
        family.intercept_se_hz,
        family.index_pin,
        family.origin_sigmas,
        family.origin_fraction,
        family.residual_rms_hz,
        family.residual_tolerance_hz,
        family.width.sigma0_hz,
        family.width.ratio,
        family.width.index_leverage,
        i64::from(family.width.separates),
        family.line_shape_corr.filter(|v| v.is_finite()),
        family.members.len() as i64,
        active,
        supersedes,
        t.as_unix_nanos(),
        author.as_str(),
        actor,
        reason,
    ])?;
    let family_id = conn.last_insert_rowid();
    {
        let mut stmt = conn.prepare_cached(
            "INSERT INTO harmonic_family_member (family_id, emitter_id, n, f_center_hz, \
             residual_hz, width_hz) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for m in &family.members {
            stmt.execute(params![
                family_id,
                blob(m.emitter_id),
                i64::from(m.index),
                m.f_center_hz,
                m.residual_hz,
                m.width_hz,
            ])?;
        }
    }
    Ok(HarmonicFamilyRow {
        family_id,
        family: family.clone(),
        active,
        supersedes,
        t,
        author,
        actor: actor.to_owned(),
        reason: reason.to_owned(),
    })
}

/// The same set of emitters on the same fundamental, so a re-run does not append a duplicate.
fn same_claim(a: &HarmonicFamily, b: &HarmonicFamily) -> bool {
    let ids = |f: &HarmonicFamily| -> Vec<(EmitterId, u32)> {
        let mut v: Vec<(EmitterId, u32)> =
            f.members.iter().map(|m| (m.emitter_id, m.index)).collect();
        v.sort();
        v
    };
    a.device_id == b.device_id
        && a.antenna_port == b.antenna_port
        && ids(a) == ids(b)
        // The fundamental is re-fitted each run, so compare it at the tolerance the fit itself
        // reports rather than for equality.
        && (a.f0_hz - b.f0_hz).abs() <= a.residual_tolerance_hz.max(b.residual_tolerance_hz)
}

impl Repository {
    /// Record a harmonic-family claim. Append-only; nothing else is touched.
    pub fn record_harmonic_family(
        &mut self,
        family: &HarmonicFamily,
        author: RelationAuthor,
        actor: &str,
        t: Timestamp,
        reason: &str,
    ) -> Result<HarmonicFamilyRow, RepoError> {
        let tx = self.write_tx()?;
        let r = insert_family(
            &tx,
            family,
            true,
            None,
            Claimant {
                t,
                author,
                actor,
                reason,
            },
        )?;
        tx.commit()?;
        Ok(r)
    }

    /// Revoke a standing claim by appending a row that retires it. The retired row, its reasoning
    /// and its members all remain readable.
    pub fn revoke_harmonic_family(
        &mut self,
        family_id: i64,
        author: RelationAuthor,
        actor: &str,
        t: Timestamp,
        reason: &str,
    ) -> Result<HarmonicFamilyRow, RepoError> {
        let tx = self.write_tx()?;
        let prior = read_rows(
            &tx,
            "WHERE family_id = ?1 LIMIT 1",
            Some(Param::Family(family_id)),
        )?;
        let Some(prior) = prior.into_iter().next() else {
            return Err(RepoError::Invalid(format!(
                "no harmonic family {family_id} to revoke"
            )));
        };
        let r = insert_family(
            &tx,
            &prior.family,
            false,
            Some(family_id),
            Claimant {
                t,
                author,
                actor,
                reason,
            },
        )?;
        tx.commit()?;
        Ok(r)
    }

    /// Every harmonic family currently in force.
    pub fn harmonic_families(&self) -> Result<Vec<HarmonicFamilyRow>, RepoError> {
        read_rows(&self.conn, CURRENT_FAMILIES_SQL, None)
    }

    /// Every family row naming `id`, newest first — including revoked ones, because the reasoning
    /// of a retired claim is part of the record.
    pub fn harmonic_families_for_emitter(
        &self,
        id: EmitterId,
    ) -> Result<Vec<HarmonicFamilyRow>, RepoError> {
        read_rows(
            &self.conn,
            FAMILIES_FOR_EMITTER_SQL,
            Some(Param::Emitter(id)),
        )
    }

    /// **The resolution pass.** Gather the live inventory rows measured on each receive chain,
    /// search them for harmonic families, and record what is new — revoking any standing claim the
    /// evidence no longer supports.
    ///
    /// Partitioned by chain *before* the search (T-302), so no fit can ever span two front ends.
    /// Bounded by [`MAX_FAMILY_CANDIDATES`] per chain, because this runs on a serving path.
    pub fn resolve_harmonic_families(
        &mut self,
        actor: &str,
        t: Timestamp,
    ) -> Result<Vec<HarmonicFamilyRow>, RepoError> {
        let tx = self.write_tx()?;
        let standing = read_rows(&tx, CURRENT_FAMILIES_SQL, None)?;
        let mut found: Vec<HarmonicFamily> = Vec::new();
        for (chain, members) in candidates_by_chain(&tx)? {
            let _ = &chain;
            found.extend(find_harmonic_families(&members));
        }
        let mut out = Vec::new();
        // New claims.
        for f in &found {
            if standing.iter().any(|s| same_claim(&s.family, f)) {
                continue;
            }
            let reason = format!(
                "harmonics of one fundamental that was never detected: {}",
                f.arithmetic()
            );
            out.push(insert_family(
                &tx,
                f,
                true,
                None,
                Claimant {
                    t,
                    author: RelationAuthor::System,
                    actor,
                    reason: &reason,
                },
            )?);
        }
        // Claims the measurements no longer support.
        for s in &standing {
            if found.iter().any(|f| same_claim(&s.family, f)) {
                continue;
            }
            out.push(insert_family(
                &tx,
                &s.family,
                false,
                Some(s.family_id),
                Claimant {
                    t,
                    author: RelationAuthor::System,
                    actor,
                    reason: "the measurements no longer fit n x f0 through the origin with widths \
                             proportional to n, so this family is withdrawn; every member keeps \
                             its row, detections and history",
                },
            )?);
        }
        tx.commit()?;
        Ok(out)
    }
}

/// The live inventory rows that could be family members, grouped by the receive chain they were
/// measured on.
///
/// A row measured on several chains appears under each: a harmonic family is a property of one
/// chain, and which chain manufactured it is what the fit is being asked to decide.
fn candidates_by_chain(
    conn: &Connection,
) -> Result<Vec<(ReceiveChain, Vec<FamilyMember>)>, RepoError> {
    let ids: Vec<[u8; 16]> = conn
        .prepare_cached(
            "SELECT emitter_id FROM emitter WHERE merged_into IS NULL \
             AND lifecycle_state != 'deleted' ORDER BY last_seen DESC, emitter_id LIMIT ?1",
        )?
        .query_map([MAX_FAMILY_CANDIDATES as i64], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut out: Vec<(ReceiveChain, Vec<FamilyMember>)> = Vec::new();
    for raw in ids {
        let id = EmitterId::from_uuid(uuid::Uuid::from_bytes(raw));
        let Some(ev) = super::relate::evidence(conn, id)? else {
            continue;
        };
        // The width the corroboration is read from. **The same measure for every member** is what
        // makes `width/n` comparable — a -3 dB width next to an occupied bandwidth would put a
        // factor of two into the ratio for free — so this reads the occupied bandwidth for every
        // row rather than preferring a -3 dB extent where one happens to exist.
        let width = ev.bandwidth_hz;
        if !width.is_finite() || width <= 0.0 {
            continue;
        }
        let chains = ev.chains();
        for c in chains {
            let m = FamilyMember {
                emitter_id: id,
                f_center_hz: ev.f_center_hz,
                width_hz: width,
                chains: vec![c.clone()],
                line_shape: None,
            };
            match out.iter_mut().find(|(x, _)| x.same_chain(&c)) {
                Some((_, v)) => v.push(m),
                None => out.push((c, vec![m])),
            }
        }
    }
    Ok(out)
}
