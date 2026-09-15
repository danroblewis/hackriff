//! Refined tuning of an emitter (T-070): the centre, bandwidth and mode parameters a demodulator
//! chain found by analysing its own output (`hk_demod::refine`), stored beside the emitter's
//! detected values, which never change because of it.
//!
//! - **Interpretation, append-only.** Every refinement is a new row (an `UPDATE` aborts); the
//!   latest row is current. Provenance is always [`REFINED_BY_OUTPUT_ANALYSIS`].
//! - **Originals kept.** The row snapshots the emitter's detected centre and bandwidth at the time
//!   (`detected_*`) and the coarse start the search began from (`start_*`); the emitter aggregate
//!   itself is not touched.
//! - **Merges.** Rows are written under the live (merged-into) emitter; reads also see rows
//!   written under emitters merged into it.
//! - **Only locked results are stored** (an unlocked search refined nothing), with finite numbers
//!   only.
//! - Databases created before T-070 (same pre-release schema version) get the table on first use.
//!
//! No content: centre, bandwidth, quality figures and search statistics are metadata.

use std::collections::BTreeMap;

use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::{RepoError, Repository, blob};
use crate::ids::EmitterId;
use crate::time::Timestamp;

/// Provenance of every refined tuning.
pub const REFINED_BY_OUTPUT_ANALYSIS: &str = "refined by output analysis";

/// Most rows [`Repository::refined_tuning_history`] returns.
pub const REFINED_HISTORY_MAX: usize = 1000;

const ENSURE_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS emitter_refined_tuning (
    refined_id  INTEGER PRIMARY KEY,
    emitter_id  BLOB    NOT NULL REFERENCES emitter (emitter_id),
    t           INTEGER NOT NULL,
    f_center    REAL    NOT NULL CHECK (f_center > 0),
    bandwidth   REAL    NOT NULL CHECK (bandwidth > 0),
    body        TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_emitter_refined_tuning_emitter
    ON emitter_refined_tuning (emitter_id, t, refined_id);
CREATE TRIGGER IF NOT EXISTS emitter_refined_tuning_append_only
    BEFORE UPDATE ON emitter_refined_tuning
    BEGIN SELECT RAISE(ABORT, 'refined tunings are append-only'); END;";

/// One output-driven refinement of an emitter's tuning.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RefinedTuning {
    /// Emitter (the live id when written).
    pub emitter_id: EmitterId,
    /// Always [`REFINED_BY_OUTPUT_ANALYSIS`].
    pub provenance: String,
    /// Objective name and version, e.g. `hk-demod/wfm-output@1`.
    pub objective: String,
    /// Mode refined, e.g. `wfm`.
    pub mode: String,
    /// Chain that refined, e.g. `listen` or `analog-chain`.
    pub source: String,
    /// Refined centre, RF Hz (receiver frame).
    pub center_hz: f64,
    /// Refined channel bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// Where the search started (the selection or detection box centre), Hz.
    pub start_center_hz: f64,
    /// The start box width, Hz.
    pub start_bandwidth_hz: f64,
    /// The emitter's detected centre when this was written (filled by the repository), Hz.
    #[serde(default)]
    pub detected_center_hz: f64,
    /// The emitter's detected bandwidth when this was written (filled by the repository), Hz.
    #[serde(default)]
    pub detected_bandwidth_hz: f64,
    /// Objective value (quality) at the result, in the objective's unit.
    pub objective_value: f64,
    /// Output locked at the result (always true when stored).
    pub locked: bool,
    /// The centre converged within the loop's tolerance.
    pub converged: bool,
    /// Search iterations.
    pub iterations: u32,
    /// Measurements made.
    pub evaluations: u32,
    /// Wall-clock time of the search, s.
    pub elapsed_s: f64,
    /// Mode parameters measured at the result (e.g. `pilot_hz`, `clock_ppm`).
    #[serde(default)]
    pub mode_params: BTreeMap<String, f64>,
    /// Time of the IQ the refinement measured.
    pub t: Timestamp,
}

impl RefinedTuning {
    /// Checks the rules in the module docs.
    pub fn validate(&self) -> Result<(), RepoError> {
        let bad = |why: String| Err(RepoError::Invalid(format!("refined tuning: {why}")));
        if self.provenance != REFINED_BY_OUTPUT_ANALYSIS {
            return bad(format!("provenance must be {REFINED_BY_OUTPUT_ANALYSIS:?}"));
        }
        if !self.locked {
            return bad("an unlocked search refined nothing".into());
        }
        if !(self.center_hz.is_finite() && self.center_hz > 0.0) {
            return bad(format!(
                "centre {} must be finite and positive",
                self.center_hz
            ));
        }
        if !(self.bandwidth_hz.is_finite() && self.bandwidth_hz > 0.0) {
            return bad(format!(
                "bandwidth {} must be finite and positive",
                self.bandwidth_hz
            ));
        }
        let finite = [
            self.start_center_hz,
            self.start_bandwidth_hz,
            self.objective_value,
            self.elapsed_s,
        ];
        if finite.iter().any(|v| !v.is_finite())
            || self.mode_params.values().any(|v| !v.is_finite())
        {
            return bad("non-finite number".into());
        }
        Ok(())
    }
}

impl Repository {
    fn ensure_refined_table(&self) -> Result<(), RepoError> {
        self.conn.execute_batch(ENSURE_TABLE)?;
        Ok(())
    }

    /// Stores a refinement under the live emitter, snapshotting its detected centre and
    /// bandwidth; returns the stored row.
    pub fn insert_refined_tuning(
        &mut self,
        refined: &RefinedTuning,
    ) -> Result<RefinedTuning, RepoError> {
        refined.validate()?;
        self.ensure_refined_table()?;
        let id = self.live_emitter_id(refined.emitter_id)?;
        let e = self.emitter(id)?;
        let row = RefinedTuning {
            emitter_id: id,
            detected_center_hz: e.f_center_hz,
            detected_bandwidth_hz: e.bandwidth_hz,
            ..refined.clone()
        };
        let tx = self.write_tx()?;
        tx.execute(
            "INSERT INTO emitter_refined_tuning (emitter_id, t, f_center, bandwidth, body) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                blob(id),
                row.t.as_unix_nanos(),
                row.center_hz,
                row.bandwidth_hz,
                serde_json::to_string(&row)?
            ],
        )?;
        tx.commit()?;
        Ok(row)
    }

    /// Every refinement of `emitter` (its live id and emitters merged into it), newest first (at
    /// most [`REFINED_HISTORY_MAX`]).
    pub fn refined_tuning_history(
        &self,
        emitter: EmitterId,
    ) -> Result<Vec<RefinedTuning>, RepoError> {
        self.ensure_refined_table()?;
        let id = self.live_emitter_id(emitter)?;
        let mut stmt = self.conn.prepare_cached(
            "WITH RECURSIVE absorbed(id) AS ( \
                 SELECT ?1 \
                 UNION SELECT e.emitter_id FROM emitter e JOIN absorbed a ON e.merged_into = a.id \
             ) \
             SELECT body FROM emitter_refined_tuning \
             WHERE emitter_id IN (SELECT id FROM absorbed) \
             ORDER BY t DESC, refined_id DESC LIMIT ?2",
        )?;
        let texts = stmt
            .query_map(params![blob(id), REFINED_HISTORY_MAX as i64], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        texts
            .iter()
            .map(|t| serde_json::from_str(t).map_err(RepoError::from))
            .collect()
    }

    /// The current (latest) refinement of `emitter`, if any.
    pub fn refined_tuning(&self, emitter: EmitterId) -> Result<Option<RefinedTuning>, RepoError> {
        Ok(self.refined_tuning_history(emitter)?.into_iter().next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::{Fingerprint, Sighting};
    use crate::emitter::LinkTarget;
    use crate::ids::TrackId;
    use crate::region::TimeRange;

    fn t(s: i64) -> Timestamp {
        Timestamp::from_unix_nanos(s * 1_000_000_000)
    }

    fn sighting(f: f64, bw: f64) -> Sighting {
        Sighting {
            source: LinkTarget::Track(TrackId::new()),
            seen: TimeRange::new(t(0), t(1)),
            count: 1,
            f_center_hz: f,
            bandwidth_hz: bw,
            fingerprint: Some(Fingerprint::new(f, bw)),
            identity: None,
            context: None,
            classification: None,
            tags: Vec::new(),
        }
    }

    fn refined(e: EmitterId, center: f64, at: i64) -> RefinedTuning {
        RefinedTuning {
            emitter_id: e,
            provenance: REFINED_BY_OUTPUT_ANALYSIS.into(),
            objective: "hk-demod/wfm-output@1".into(),
            mode: "wfm".into(),
            source: "test".into(),
            center_hz: center,
            bandwidth_hz: 180e3,
            start_center_hz: 101.2e6,
            start_bandwidth_hz: 60e3,
            detected_center_hz: 0.0,
            detected_bandwidth_hz: 0.0,
            objective_value: 62.5,
            locked: true,
            converged: true,
            iterations: 7,
            evaluations: 20,
            elapsed_s: 0.8,
            mode_params: BTreeMap::from([("pilot_hz".to_owned(), 18_999.87)]),
            t: t(at),
        }
    }

    #[test]
    fn refinements_append_keep_the_detected_values_and_follow_merges() {
        let mut repo = Repository::open_in_memory().unwrap();
        let a = repo
            .record_sighting(&sighting(101.303e6, 333e3), None)
            .unwrap();
        assert_eq!(repo.refined_tuning(a.emitter_id).unwrap(), None);
        let stored = repo
            .insert_refined_tuning(&refined(a.emitter_id, 101.2995e6, 2))
            .unwrap();
        assert_eq!(
            (stored.detected_center_hz, stored.detected_bandwidth_hz),
            (101.303e6, 333e3)
        );
        repo.insert_refined_tuning(&refined(a.emitter_id, 101.2996e6, 3))
            .unwrap();
        let cur = repo.refined_tuning(a.emitter_id).unwrap().unwrap();
        assert_eq!(cur.center_hz, 101.2996e6);
        assert_eq!(repo.refined_tuning_history(a.emitter_id).unwrap().len(), 2);
        let e = repo.emitter(a.emitter_id).unwrap();
        assert_eq!((e.f_center_hz, e.bandwidth_hz), (101.303e6, 333e3));

        // Append-only.
        assert!(
            repo.conn
                .execute("UPDATE emitter_refined_tuning SET f_center = 1", [])
                .is_err()
        );

        // Merged: the survivor sees the absorbed emitter's rows.
        let b = repo.record_sighting(&sighting(900e6, 20e3), None).unwrap();
        repo.merge_emitters(a.emitter_id, b.emitter_id, t(4), "test")
            .unwrap();
        assert_eq!(
            repo.refined_tuning(b.emitter_id)
                .unwrap()
                .unwrap()
                .center_hz,
            101.2996e6
        );
        let via_old = repo
            .insert_refined_tuning(&refined(a.emitter_id, 101.2997e6, 5))
            .unwrap();
        assert_eq!(
            via_old.emitter_id, b.emitter_id,
            "written under the live id"
        );
    }

    #[test]
    fn invalid_or_unlocked_refinements_are_refused() {
        let mut repo = Repository::open_in_memory().unwrap();
        let a = repo
            .record_sighting(&sighting(101.3e6, 200e3), None)
            .unwrap();
        let ok = refined(a.emitter_id, 101.3e6, 1);
        for bad in [
            RefinedTuning {
                locked: false,
                ..ok.clone()
            },
            RefinedTuning {
                provenance: "snapped to the raster".into(),
                ..ok.clone()
            },
            RefinedTuning {
                center_hz: f64::NAN,
                ..ok.clone()
            },
            RefinedTuning {
                bandwidth_hz: 0.0,
                ..ok.clone()
            },
            RefinedTuning {
                mode_params: BTreeMap::from([("x".to_owned(), f64::INFINITY)]),
                ..ok.clone()
            },
        ] {
            assert!(
                matches!(repo.insert_refined_tuning(&bad), Err(RepoError::Invalid(_))),
                "{bad:?}"
            );
        }
        assert!(repo.refined_tuning(a.emitter_id).unwrap().is_none());
    }
}
