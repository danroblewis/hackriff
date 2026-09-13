//! Trust verdicts (T-037b): what the scheduler's cross-capture trust tests concluded (S4 rule 6
//! gain step, rule 7 retune, and the clock-harmonic rate-change test), one row per classified
//! emitter, or one row per comparison that was skipped. Insert-only.
//!
//! Detections stay immutable: a verdict names the emitter's frequency span, the Survey and (when
//! the scheduler still knew it) the POI's Track. `track_id` is a soft reference with no foreign
//! key, because a verdict can be written before the batched Track row.

use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::{RepoError, Repository, blob, finite, opt_blob};
use crate::ids::{SurveyId, TrackId};
use crate::region::FreqRange;
use crate::time::Timestamp;

/// Which cross-capture trust test produced a verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrustTest {
    /// S4 rule 6: SNR invariance across a gain step.
    GainStep,
    /// S4 rule 7: the same emitter after a retune.
    Retune,
    /// Clock-harmonic test: the same centre at another sample rate.
    RateChange,
}

impl TrustTest {
    /// Stored name.
    pub fn as_str(self) -> &'static str {
        match self {
            TrustTest::GainStep => "gain-step",
            TrustTest::Retune => "retune",
            TrustTest::RateChange => "rate-change",
        }
    }
}

/// One trust-test verdict.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrustVerdict {
    /// Survey the captures belong to.
    pub survey_id: SurveyId,
    /// The POI's track, when known (soft reference, see the module docs).
    pub track: Option<TrackId>,
    /// Test.
    pub test: TrustTest,
    /// Verdict, kebab-case (`linear`, `suspect-imd`, `stays`, `moves-with-lo`, `clock-harmonic`,
    /// `not-reproduced`, …), or `skipped-<reason>` for a comparison that was not run.
    pub label: String,
    /// The classified emitter's span (a skipped comparison: the common usable span), Hz.
    pub freq: FreqRange,
    /// Evaluation time (stream time).
    pub t: Timestamp,
    /// Test-specific numbers (ΔSNR and bound, Δ, rates, capture side, …).
    pub detail: serde_json::Value,
}

impl Repository {
    /// Appends verdicts in one transaction; returns how many.
    pub fn insert_trust_verdicts(&mut self, verdicts: &[TrustVerdict]) -> Result<usize, RepoError> {
        if verdicts.is_empty() {
            return Ok(0);
        }
        let tx = self.write_tx()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO trust_verdict (survey_id, track_id, test, label, f_lo, f_hi, t, body) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for v in verdicts {
                stmt.execute(params![
                    blob(v.survey_id),
                    opt_blob(v.track),
                    v.test.as_str(),
                    v.label,
                    finite(v.freq.lo_hz, "trust verdict f_lo")?,
                    finite(v.freq.hi_hz, "trust verdict f_hi")?,
                    v.t.as_unix_nanos(),
                    serde_json::to_string(v)?
                ])?;
            }
        }
        tx.commit()?;
        Ok(verdicts.len())
    }

    /// A Survey's verdicts in time order (then insertion order).
    pub fn trust_verdicts(&self, survey: SurveyId) -> Result<Vec<TrustVerdict>, RepoError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT body FROM trust_verdict WHERE survey_id = ?1 ORDER BY t, verdict_seq",
        )?;
        let bodies: Vec<String> = stmt
            .query_map(params![blob(survey)], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        bodies
            .iter()
            .map(|b| serde_json::from_str(b).map_err(RepoError::from))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ids::ScanPlanId;
    use crate::plan::{ScanPlan, ScanPolicy, Schedule, Survey, SurveyState};

    #[test]
    fn trust_verdicts_round_trip_per_survey_in_time_order() {
        let mut repo = Repository::open_in_memory().unwrap();
        let t0 = Timestamp::from_unix_nanos(1_789_300_800_000_000_000);
        let plan = ScanPlan {
            id: ScanPlanId::new(),
            version: 1,
            name: "verdicts".into(),
            created_at: t0,
            regions: Vec::new(),
            policy: ScanPolicy::SweepThenDwell,
            gain_table: Vec::new(),
            schedule: Schedule::Continuous,
            extra: json!({}),
        };
        repo.insert_scan_plan(&plan).unwrap();
        let survey = Survey {
            id: SurveyId::new(),
            plan_id: plan.id,
            plan_version: 1,
            device_id: "test".into(),
            state: SurveyState::Open,
            t_start: t0,
            t_end: None,
            summary: None,
        };
        repo.insert_survey(&survey).unwrap();
        let verdict = |test, label: &str, dt: i64| TrustVerdict {
            survey_id: survey.id,
            track: Some(TrackId::new()),
            test,
            label: label.into(),
            freq: FreqRange::centered(915e6, 100e3),
            t: t0.saturating_add_nanos(dt),
            detail: json!({ "side": "a" }),
        };
        let rows = vec![
            verdict(TrustTest::RateChange, "clock-harmonic", 2),
            verdict(TrustTest::GainStep, "linear", 1),
            verdict(TrustTest::Retune, "skipped-centre-mismatch", 2),
        ];
        assert_eq!(repo.insert_trust_verdicts(&rows).unwrap(), 3);
        assert_eq!(repo.insert_trust_verdicts(&[]).unwrap(), 0);
        let back = repo.trust_verdicts(survey.id).unwrap();
        assert_eq!(
            back,
            vec![rows[1].clone(), rows[0].clone(), rows[2].clone()]
        );
        assert!(repo.trust_verdicts(SurveyId::new()).unwrap().is_empty());
        let mut bad = rows[0].clone();
        bad.survey_id = SurveyId::new();
        assert!(
            repo.insert_trust_verdicts(&[bad]).is_err(),
            "the Survey must exist"
        );
    }
}
