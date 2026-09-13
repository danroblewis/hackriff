//! Applying schedule steps to a source through its [`SourceControl`].

use std::sync::Arc;

use super::step::ScheduleStep;
use crate::source::{Gains, SourceControl, SourceError};

/// Which controls a step changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AppliedChanges {
    /// Sample rate set.
    pub rate: bool,
    /// Baseband filter set.
    pub baseband_filter: bool,
    /// Gains set.
    pub gains: bool,
    /// Centre retuned.
    pub tune: bool,
}

impl AppliedChanges {
    /// Anything was sent.
    pub fn any(&self) -> bool {
        self.rate || self.baseband_filter || self.gains || self.tune
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Applied {
    center_hz: f64,
    rate_hz: f64,
    baseband_filter_hz: Option<f64>,
    gains: Gains,
}

/// Sends each step's settings to a source, only those that changed since the last step, in the
/// order rate → filter → gains → tune. A live source applies them together at its next block
/// boundary (one block carries all the discontinuity flags). No allocation.
pub struct StepApplier {
    control: Arc<dyn SourceControl>,
    last: Option<Applied>,
}

impl StepApplier {
    /// Applies to `control`. The first step sends every setting.
    pub fn new(control: Arc<dyn SourceControl>) -> Self {
        Self {
            control,
            last: None,
        }
    }

    /// The control handle.
    pub fn control(&self) -> &Arc<dyn SourceControl> {
        &self.control
    }

    /// Forgets the applied state, so the next step sends every setting (e.g. after the source
    /// restarted).
    pub fn invalidate(&mut self) {
        self.last = None;
    }

    /// Applies `step`. On error the applied state is forgotten, so the next step resends all.
    pub fn apply(&mut self, step: &ScheduleStep) -> Result<AppliedChanges, SourceError> {
        let prev = self.last.take();
        let mut changes = AppliedChanges::default();
        if prev.is_none_or(|p| p.rate_hz != step.rate_hz) {
            self.control.set_sample_rate(step.rate_hz)?;
            changes.rate = true;
        }
        if let Some(bw) = step.baseband_filter_hz {
            if prev.is_none_or(|p| p.baseband_filter_hz != Some(bw)) {
                self.control.set_baseband_filter(bw)?;
                changes.baseband_filter = true;
            }
        }
        if prev.is_none_or(|p| p.gains != step.gains) {
            self.control.set_gains(&step.gains)?;
            changes.gains = true;
        }
        if prev.is_none_or(|p| p.center_hz != step.center_hz) {
            self.control.tune(step.center_hz)?;
            changes.tune = true;
        }
        self.last = Some(Applied {
            center_hz: step.center_hz,
            rate_hz: step.rate_hz,
            baseband_filter_hz: step.baseband_filter_hz,
            gains: step.gains,
        });
        Ok(changes)
    }
}
