//! Survey lifecycle records (docs/07 §2.2): the scheduler opens a Survey under a plan version
//! and closes it when the run ends or the plan version changes.

use hk_model::{RepoError, Repository, Survey, SurveyId, SurveyState, SurveySummary, Timestamp};

/// Where Survey open/close records go. Called only at run boundaries, never per step.
pub trait SurveyLog {
    /// Records an open Survey.
    fn open(&mut self, survey: &Survey) -> Result<(), RepoError>;

    /// Ends an open Survey as `closed` or `aborted`.
    fn close(
        &mut self,
        id: SurveyId,
        state: SurveyState,
        t_end: Timestamp,
        summary: &SurveySummary,
    ) -> Result<(), RepoError>;
}

/// The SQLite repository. The ScanPlan version must already be stored (foreign key).
impl SurveyLog for Repository {
    fn open(&mut self, survey: &Survey) -> Result<(), RepoError> {
        self.insert_survey(survey)
    }

    fn close(
        &mut self,
        id: SurveyId,
        state: SurveyState,
        t_end: Timestamp,
        summary: &SurveySummary,
    ) -> Result<(), RepoError> {
        self.finish_survey(id, state, t_end, summary)
    }
}

/// One lifecycle record.
#[derive(Clone, Debug, PartialEq)]
pub enum SurveyEvent {
    /// A Survey opened.
    Opened(Survey),
    /// A Survey ended.
    Closed {
        /// Survey.
        id: SurveyId,
        /// `closed` or `aborted`.
        state: SurveyState,
        /// End time.
        t_end: Timestamp,
        /// Run summary.
        summary: SurveySummary,
    },
}

/// An in-memory lifecycle log (tests, dry runs).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MemorySurveyLog {
    /// Records in order.
    pub events: Vec<SurveyEvent>,
}

impl SurveyLog for MemorySurveyLog {
    fn open(&mut self, survey: &Survey) -> Result<(), RepoError> {
        self.events.push(SurveyEvent::Opened(survey.clone()));
        Ok(())
    }

    fn close(
        &mut self,
        id: SurveyId,
        state: SurveyState,
        t_end: Timestamp,
        summary: &SurveySummary,
    ) -> Result<(), RepoError> {
        let opened = self
            .events
            .iter()
            .any(|e| matches!(e, SurveyEvent::Opened(s) if s.id == id));
        let closed = self
            .events
            .iter()
            .any(|e| matches!(e, SurveyEvent::Closed { id: c, .. } if *c == id));
        if !opened || closed {
            return Err(RepoError::Invalid(format!("survey {id} is not open")));
        }
        self.events.push(SurveyEvent::Closed {
            id,
            state,
            t_end,
            summary: summary.clone(),
        });
        Ok(())
    }
}
