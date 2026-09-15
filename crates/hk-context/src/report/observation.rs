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

    fn observed(
        &self,
        cells: &[FreqRange],
        span: TimeRange,
    ) -> Result<Option<Vec<Vec<TimeRange>>>, ReportError> {
        let mut any = false;
        let out: Vec<Vec<TimeRange>> = cells
            .iter()
            .map(|&cell| {
                let visits = self.0.observations_of(cell, span);
                any |= !visits.is_empty();
                visits.iter().map(|v| v.observed).collect()
            })
            .collect();
        Ok(any.then_some(out))
    }
}
