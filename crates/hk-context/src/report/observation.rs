//! Observation-log coverage provider (T-115): the exact observed intervals of each frequency
//! cell, one per dwell or hop visit. It declines (so history tiles answer) when the log holds
//! nothing for the box, e.g. a store created before the log existed.

use hk_model::{FreqRange, TimeRange};
use hk_store::observation::ObservationStore;

use super::{CoverageProvider, ReportError};

/// [`CoverageProvider`] over the observation log.
pub struct ObservationCoverage<'a>(pub &'a ObservationStore);

impl CoverageProvider for ObservationCoverage<'_> {
    fn name(&self) -> &'static str {
        "the observation log"
    }

    /// One pass over the log's segments for all cells.
    fn observed(
        &self,
        cells: &[FreqRange],
        span: TimeRange,
    ) -> Result<Option<Vec<Vec<TimeRange>>>, ReportError> {
        let visits = self.0.observations_of_each(cells, span);
        let any = visits.iter().any(|v| !v.is_empty());
        Ok(any.then(|| {
            visits
                .iter()
                .map(|v| v.iter().map(|x| x.observed).collect())
                .collect()
        }))
    }
}
