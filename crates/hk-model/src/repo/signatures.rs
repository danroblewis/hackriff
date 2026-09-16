//! C18 signature storage (T-201, ADR-0016 §5/§9): the catalogue, the measured features and the
//! append-only match log, over T-218's migration 0009 plus 0010 for the measured side.
//!
//! The engine-level guarantees (a version is immutable but retirable, a match and a features
//! snapshot are append-only) are the migrations' triggers and are tested in
//! [`super::signature_tests`]. This module is the typed access on top of them, and adds the two
//! rules the matcher depends on:
//!
//! - **A read of the catalogue is a read of *current* entries.** [`Repository::signatures`]
//!   returns the highest non-retired version of each id, which is what a match is computed
//!   against; every version stays readable by id for re-deriving an old match.
//! - **[`Repository::signatures_rev`] changes on every catalogue write** (a new version or a
//!   retirement), so a stored match records exactly which catalogue produced it.
//!
//! Nothing here writes identity, `known_status` or lifecycle. A [`SignatureMatch`] is ranked
//! evidence with its arithmetic disclosed (ADR-0016 §5); the only column it shares with the
//! inventory is the emitter it is *about*.

use rusqlite::{OptionalExtension, params};

use super::{RepoError, Repository, blob, bodies};
use crate::ids::EmitterId;
use crate::signature::{EmissionFeatures, Signature, SignatureMatch};
use crate::time::Timestamp;

impl Repository {
    /// Inserts one signature version. The version is validated first: a stored entry that breaks
    /// its own contract would silently mis-rank every later match. An id/version already present
    /// is refused by the primary key (a version is immutable — write a new one).
    pub fn insert_signature(&mut self, s: &Signature) -> Result<(), RepoError> {
        s.validate().map_err(|e| RepoError::Invalid(e.0))?;
        self.conn.execute(
            "INSERT INTO signature (signature_id, version, name, kind, taxonomy, family, \
             provenance, author, created_at, supersedes, body) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                s.id,
                s.version,
                s.name,
                s.kind.as_str(),
                s.taxonomy.as_ref().map(ToString::to_string),
                s.family,
                s.provenance.as_str(),
                s.author,
                s.created_at.as_unix_nanos(),
                s.supersedes,
                serde_json::to_string(s)?,
            ],
        )?;
        Ok(())
    }

    /// One signature version, retired or not (so an old match can be re-derived exactly).
    pub fn signature(&self, id: &str, version: u32) -> Result<Signature, RepoError> {
        let body: Option<String> = self
            .conn
            .prepare_cached("SELECT body FROM signature WHERE signature_id = ?1 AND version = ?2")?
            .query_row(params![id, version], |r| r.get(0))
            .optional()?;
        match body {
            Some(b) => Ok(serde_json::from_str(&b)?),
            None => Err(RepoError::NotFound {
                kind: "signature",
                id: format!("{id}@{version}"),
            }),
        }
    }

    /// Every version of one id, oldest first, retired ones included.
    pub fn signature_versions(&self, id: &str) -> Result<Vec<Signature>, RepoError> {
        bodies(
            &self.conn,
            "SELECT body FROM signature WHERE signature_id = ?1 ORDER BY version",
            params![id],
        )
    }

    /// The current catalogue: the highest **non-retired** version of every id, by id. This is what
    /// a match is computed against.
    pub fn signatures(&self) -> Result<Vec<Signature>, RepoError> {
        bodies(
            &self.conn,
            "SELECT body FROM signature WHERE retired_at IS NULL AND version = ( \
               SELECT max(version) FROM signature AS s2 \
               WHERE s2.signature_id = signature.signature_id AND s2.retired_at IS NULL) \
             ORDER BY signature_id",
            [],
        )
    }

    /// The current (highest non-retired) version of one id, or `None` when every version is
    /// retired or the id is unknown.
    pub fn current_signature(&self, id: &str) -> Result<Option<Signature>, RepoError> {
        Ok(self.signatures()?.into_iter().find(|s| s.id == id))
    }

    /// Retires one version. Retiring is not an edit and not a delete: the row stays readable, and
    /// a match that already cited it can still be explained.
    pub fn retire_signature(
        &mut self,
        id: &str,
        version: u32,
        t: Timestamp,
    ) -> Result<(), RepoError> {
        let n = self.conn.execute(
            "UPDATE signature SET retired_at = ?3 \
             WHERE signature_id = ?1 AND version = ?2 AND retired_at IS NULL",
            params![id, version, t.as_unix_nanos()],
        )?;
        if n == 0 {
            return Err(RepoError::NotFound {
                kind: "signature",
                id: format!("{id}@{version}"),
            });
        }
        Ok(())
    }

    /// Catalogue revision: it changes on every write (a new version, a retirement), so a stored
    /// match names exactly the catalogue it was computed against.
    pub fn signatures_rev(&self) -> Result<u64, RepoError> {
        let rev: i64 = self.conn.query_row(
            "SELECT count(*) + count(retired_at) FROM signature",
            [],
            |r| r.get(0),
        )?;
        Ok(rev.max(0) as u64)
    }

    /// Appends one features snapshot. Snapshots are append-only: the emitter's measured history is
    /// kept, never overwritten.
    pub fn put_emission_features(&mut self, f: &EmissionFeatures) -> Result<(), RepoError> {
        f.validate().map_err(|e| RepoError::Invalid(e.0))?;
        self.conn.execute(
            "INSERT INTO emission_features (features_id, emitter_id, t, version, observations, \
             suspect_fraction, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                f.id,
                blob(f.emitter_id),
                f.t.as_unix_nanos(),
                f.version,
                f.observations,
                f.suspect_fraction,
                serde_json::to_string(f)?,
            ],
        )?;
        Ok(())
    }

    /// One features snapshot by id (what a [`SignatureMatch::features_ref`] names).
    pub fn emission_features(&self, id: &str) -> Result<EmissionFeatures, RepoError> {
        let body: Option<String> = self
            .conn
            .prepare_cached("SELECT body FROM emission_features WHERE features_id = ?1")?
            .query_row(params![id], |r| r.get(0))
            .optional()?;
        match body {
            Some(b) => Ok(serde_json::from_str(&b)?),
            None => Err(RepoError::NotFound {
                kind: "emission features",
                id: id.to_owned(),
            }),
        }
    }

    /// The emitter's latest features snapshot, or `None` when it has none.
    pub fn emitter_features(
        &self,
        emitter_id: EmitterId,
    ) -> Result<Option<EmissionFeatures>, RepoError> {
        let rows: Vec<EmissionFeatures> = bodies(
            &self.conn,
            "SELECT body FROM emission_features WHERE emitter_id = ?1 ORDER BY t DESC, rowid DESC \
             LIMIT 1",
            params![blob(emitter_id)],
        )?;
        Ok(rows.into_iter().next())
    }

    /// The emitter's features snapshots, newest first.
    pub fn emitter_features_history(
        &self,
        emitter_id: EmitterId,
        limit: u32,
    ) -> Result<Vec<EmissionFeatures>, RepoError> {
        bodies(
            &self.conn,
            "SELECT body FROM emission_features WHERE emitter_id = ?1 \
             ORDER BY t DESC, rowid DESC LIMIT ?2",
            params![blob(emitter_id), limit],
        )
    }

    /// Appends one match. It is validated first, and it sets nothing on the emitter: the row is
    /// evidence about an emitter, never a change to it (ADR-0016 §5).
    pub fn append_signature_match(&mut self, m: &SignatureMatch) -> Result<(), RepoError> {
        m.validate().map_err(|e| RepoError::Invalid(e.0))?;
        let top = m.top();
        self.conn.execute(
            "INSERT INTO signature_match (emitter_id, t, outcome, signature_id, version, score, \
             features_ref, signatures_rev, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                blob(m.emitter_id),
                m.t.as_unix_nanos(),
                m.outcome.as_str(),
                top.map(|c| c.signature.id.clone()),
                top.map(|c| c.signature.version),
                top.map(|c| c.score),
                m.features_ref,
                super::int(m.signatures_rev, "signatures_rev")?,
                serde_json::to_string(m)?,
            ],
        )?;
        Ok(())
    }

    /// The emitter's current match: the most recently appended one, or `None`.
    pub fn current_signature_match(
        &self,
        emitter_id: EmitterId,
    ) -> Result<Option<SignatureMatch>, RepoError> {
        let rows: Vec<SignatureMatch> = bodies(
            &self.conn,
            "SELECT body FROM signature_match WHERE emitter_id = ?1 ORDER BY match_id DESC LIMIT 1",
            params![blob(emitter_id)],
        )?;
        Ok(rows.into_iter().next())
    }

    /// The emitter's match history, newest first.
    pub fn signature_matches(
        &self,
        emitter_id: EmitterId,
        limit: u32,
    ) -> Result<Vec<SignatureMatch>, RepoError> {
        bodies(
            &self.conn,
            "SELECT body FROM signature_match WHERE emitter_id = ?1 ORDER BY match_id DESC \
             LIMIT ?2",
            params![blob(emitter_id), limit],
        )
    }
}
