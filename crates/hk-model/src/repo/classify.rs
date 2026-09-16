//! M3 classification rows (T-211, ADR-0016 §2): write, read and arbitration rank on
//! `emitter_classification` (migration 0007 columns `taxonomy`, `stage`, `arb_rank`, `detail`).
//! Pre-M3 rows keep NULLs there and read with a derived stage and rank
//! ([`crate::classify::rank`]).

use rusqlite::{Connection, OptionalExtension, Row, params};
use uuid::Uuid;

use super::inventory::{emitter_exists, link_kind, link_target};
use super::{RepoError, Repository, blob, enum_parse, enum_text, finite};
use crate::classify::{ArbRank, Classification as M3Classification, TaxonomyRef};
use crate::cluster::RecordedClassification;
use crate::emitter::Classification;
use crate::ids::EmitterId;
use crate::time::Timestamp;

/// A row's effective rank in SQL: the stored `arb_rank`, else the legacy derivation of
/// [`ArbRank::legacy`] (decoder prefix → 1, track input → 4, else chain at 3).
macro_rules! effective_rank_sql {
    () => {
        "coalesce(c.arb_rank, CASE WHEN substr(c.model_version, 1, 8) = 'decoder:' THEN 1 \
         WHEN c.input_kind IS 'track' THEN 4 ELSE 3 END)"
    };
}

/// [`effective_rank_sql`] as a constant (tests compare it with [`ArbRank::legacy`]).
#[cfg(test)]
pub(super) const EFFECTIVE_RANK_SQL: &str = effective_rank_sql!();

/// Order of an emitter's classifications for its current family (ADR-0016 §2, generalising
/// T-183): lowest arbitration rank (user > decoder > lock-verified > classifier > track shape),
/// latest among equals. Insert order alone would make the family depend on which pipeline thread
/// wrote last and on merge direction. Alias `c` is `emitter_classification`.
pub(super) const FAMILY_ORDER: &str = concat!(
    "ORDER BY ",
    effective_rank_sql!(),
    " ASC, c.classification_id DESC LIMIT 1"
);

/// Columns [`decode`] reads, alias `c`.
const COLUMNS: &str = "c.t, c.family, c.confidence, c.open_set_score, c.model_version, \
     c.input_kind, c.input_id, c.feature_set_version, c.taxonomy, c.stage, c.arb_rank, c.detail";

type Raw = (
    i64,
    String,
    f64,
    f64,
    String,
    Option<String>,
    Option<[u8; 16]>,
    Option<i64>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<String>,
);

fn raw(r: &Row<'_>) -> rusqlite::Result<Raw> {
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
}

fn decode(raw: Raw) -> Result<RecordedClassification, RepoError> {
    let (
        t,
        family,
        confidence,
        open_set_score,
        model_version,
        kind,
        input,
        v,
        tax,
        stage,
        rank,
        detail,
    ) = raw;
    let (stage, arb_rank) = match (stage, rank) {
        (Some(s), Some(r)) => (
            enum_parse(s)?,
            ArbRank::from_value(r)
                .ok_or_else(|| RepoError::Invalid(format!("arb_rank {r} out of range")))?,
        ),
        (None, None) => ArbRank::legacy(&model_version, kind.as_deref()),
        _ => {
            return Err(RepoError::Invalid(
                "classification row has only one of stage and arb_rank".into(),
            ));
        }
    };
    let taxonomy = tax
        .map(|s| s.parse::<TaxonomyRef>().map_err(RepoError::Invalid))
        .transpose()?;
    let detail = detail
        .map(|s| serde_json::from_str::<M3Classification>(&s))
        .transpose()?;
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
        taxonomy,
        stage,
        arb_rank,
        detail,
    })
}

/// An emitter's classifications, oldest first.
pub(super) fn history(
    conn: &Connection,
    id: EmitterId,
) -> Result<Vec<RecordedClassification>, RepoError> {
    let rows: Vec<Raw> = conn
        .prepare_cached(&format!(
            "SELECT {COLUMNS} FROM emitter_classification c WHERE c.emitter_id = ?1 \
             ORDER BY c.classification_id"
        ))?
        .query_map([blob(id)], raw)?
        .collect::<Result<_, _>>()?;
    rows.into_iter().map(decode).collect()
}

fn one(
    conn: &Connection,
    id: EmitterId,
    order: &str,
) -> Result<Option<RecordedClassification>, RepoError> {
    conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM emitter_classification c WHERE c.emitter_id = ?1 {order}"
    ))?
    .query_row([blob(id)], raw)
    .optional()?
    .map(decode)
    .transpose()
}

impl Repository {
    /// Appends an M3 [`M3Classification`] to an emitter's history with arbitration `rank`
    /// (ADR-0016 §2). The legacy columns carry `family`, `confidence`, `open_set_score` and
    /// [`M3Classification::legacy_model_version`], so existing readers keep working; `taxonomy`,
    /// `stage`, `arb_rank` and `detail` (the full classification as JSON) are the 0007 columns.
    ///
    /// Refused (`Invalid`): a classification breaking its contract
    /// ([`M3Classification::validate`]) or a `rank` its stage may not carry
    /// ([`ArbRank::allows`]). `NotFound` when the emitter does not exist.
    pub fn record_classification(
        &mut self,
        emitter_id: EmitterId,
        classification: &M3Classification,
        rank: ArbRank,
    ) -> Result<(), RepoError> {
        let c = classification;
        c.validate()
            .map_err(|e| RepoError::Invalid(e.to_string()))?;
        if !rank.allows(c.stage) {
            return Err(RepoError::Invalid(format!(
                "arb_rank {} is not allowed for stage {}",
                rank.value(),
                c.stage.as_str()
            )));
        }
        if !emitter_exists(&self.conn, emitter_id)? {
            return Err(RepoError::NotFound {
                kind: "emitter",
                id: emitter_id.to_string(),
            });
        }
        let (kind, input_id) = c.input.as_ref().map(link_kind).unzip();
        self.conn
            .prepare_cached(
                "INSERT INTO emitter_classification (emitter_id, t, family, confidence, \
                 open_set_score, model_version, input_kind, input_id, taxonomy, stage, arb_rank, \
                 detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            )?
            .execute(params![
                blob(emitter_id),
                c.t.as_unix_nanos(),
                c.family,
                finite(c.confidence, "confidence")?,
                finite(c.open_set_score, "open_set_score")?,
                c.legacy_model_version(),
                kind,
                input_id.map(Uuid::into_bytes),
                c.taxonomy.to_string(),
                enum_text(&c.stage)?,
                i64::from(rank.value()),
                serde_json::to_string(c)?,
            ])?;
        Ok(())
    }

    /// Appends a **legacy-shaped** classification (family, confidence, open-set score, model
    /// version) with an explicit `stage` and arbitration `rank` (ADR-0016 §2).
    ///
    /// This is the seam for a writer that knows *who* it is but has no distribution to offer, so
    /// the rank must not be derived from the columns: a user's explicit reclassification (rank 0,
    /// `hk_pipeline::family::reclassify`, T-218), and later a demodulator chain that knows it is
    /// locked (rank 2, ADR-0016 §2 — the follow-up `hk_pipeline::classify` documents). Writers
    /// with a full [`M3Classification`] use [`Repository::record_classification`]; writers with
    /// neither keep using `append_classification`, whose rows derive their rank.
    ///
    /// Refused (`Invalid`): a `rank` the `stage` may not carry ([`ArbRank::allows`]), a
    /// non-finite confidence or open-set score. `NotFound` when the emitter does not exist.
    pub fn append_classification_ranked(
        &mut self,
        emitter_id: EmitterId,
        classification: &Classification,
        stage: crate::classify::Stage,
        rank: ArbRank,
    ) -> Result<(), RepoError> {
        if !rank.allows(stage) {
            return Err(RepoError::Invalid(format!(
                "arb_rank {} is not allowed for stage {}",
                rank.value(),
                stage.as_str()
            )));
        }
        if !emitter_exists(&self.conn, emitter_id)? {
            return Err(RepoError::NotFound {
                kind: "emitter",
                id: emitter_id.to_string(),
            });
        }
        let c = classification;
        self.conn
            .prepare_cached(
                "INSERT INTO emitter_classification (emitter_id, t, family, confidence, \
                 open_set_score, model_version, stage, arb_rank) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?
            .execute(params![
                blob(emitter_id),
                c.t.as_unix_nanos(),
                c.family,
                finite(c.confidence, "confidence")?,
                finite(c.open_set_score, "open_set_score")?,
                c.model_version,
                enum_text(&stage)?,
                i64::from(rank.value()),
            ])?;
        Ok(())
    }

    /// The classification that sets an emitter's current family: lowest arbitration rank, latest
    /// among equals ([`crate::classify::rank`]). `None` without classifications. The emitter id
    /// is taken as given (not resolved through merges).
    pub fn current_classification(
        &self,
        emitter_id: EmitterId,
    ) -> Result<Option<RecordedClassification>, RepoError> {
        one(&self.conn, emitter_id, FAMILY_ORDER)
    }

    /// An emitter's most recently appended classification, whatever its rank.
    pub fn latest_classification(
        &self,
        emitter_id: EmitterId,
    ) -> Result<Option<RecordedClassification>, RepoError> {
        one(
            &self.conn,
            emitter_id,
            "ORDER BY c.classification_id DESC LIMIT 1",
        )
    }
}
