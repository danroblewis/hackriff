//! ScanPlan (docs/07 §2.1) and Survey (§2.2): what to watch, and one run of watching it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{ScanPlanId, SurveyId};
use crate::region::FreqRange;
use crate::time::Timestamp;

/// A region to watch, with its scheduling priority.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanRegion {
    /// Frequency extent.
    pub freq: FreqRange,
    /// Relative priority; higher is visited more. Scale owned by the scheduler (C04).
    pub priority: f64,
    /// Target revisit interval, ns, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revisit_ns: Option<i64>,
}

/// Sweep-versus-dwell policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScanPolicy {
    /// Wideband firmware sweep only.
    SweepOnly,
    /// Real-time dwell windows only.
    DwellOnly,
    /// Sweep to find where, dwell to find what (the default product behaviour).
    SweepThenDwell,
}

/// One per-band gain setting.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GainTableEntry {
    /// Band the setting applies to.
    pub freq: FreqRange,
    /// LNA gain, dB.
    pub lna_db: f64,
    /// VGA gain, dB.
    pub vga_db: f64,
    /// RF amplifier on.
    pub amp_on: bool,
    /// Antenna/filter port for the band, if switched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub antenna_port: Option<String>,
}

/// When the plan runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Schedule {
    /// Runs continuously while the device is on.
    Continuous,
    /// Runs on a cron-like expression (syntax owned by the scheduler, C04).
    Cron {
        /// The expression, e.g. `"0 */2 * * *"`.
        expr: String,
    },
    /// Runs once, on request.
    Manual,
}

/// A versioned, declarative scan plan (docs/07 §2.1).
///
/// Identity is `(id, version)`. Editing a plan inserts a new version with the same `id`;
/// earlier versions are never modified or deleted, so each Survey's plan is reproducible.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScanPlan {
    /// Stable plan id across versions.
    pub id: ScanPlanId,
    /// Version, starting at 1 and increasing by 1 per edit.
    pub version: u32,
    /// Human name.
    pub name: String,
    /// When this version was created.
    pub created_at: Timestamp,
    /// Regions to watch.
    pub regions: Vec<PlanRegion>,
    /// Sweep-vs-dwell policy.
    pub policy: ScanPolicy,
    /// Per-band gain table.
    #[serde(default)]
    pub gain_table: Vec<GainTableEntry>,
    /// Schedule.
    pub schedule: Schedule,
    /// Scheduler-specific settings not yet typed (C04 pins them).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub extra: Value,
}

/// Survey lifecycle state (docs/07 §2.2): `open → closed` or `open → aborted`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SurveyState {
    /// Running.
    Open,
    /// Finished normally.
    Closed,
    /// Stopped by an error, power loss or the user.
    Aborted,
}

/// Run summary written when a Survey closes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SurveySummary {
    /// Sweep frames processed.
    pub sweep_frames: u64,
    /// Spectrum frames processed.
    pub spectrum_frames: u64,
    /// Detections written.
    pub detections: u64,
    /// Recordings written.
    pub recordings: u64,
    /// Samples dropped by the source or ring buffer.
    pub dropped_samples: u64,
}

/// One execution of a ScanPlan version (docs/07 §2.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Survey {
    /// Time-sortable id.
    pub id: SurveyId,
    /// Plan run.
    pub plan_id: ScanPlanId,
    /// Plan version run.
    pub plan_version: u32,
    /// Source device, e.g. `hackrf:<serial>` or `synthetic:<generator>`.
    pub device_id: String,
    /// Lifecycle state.
    pub state: SurveyState,
    /// Start time.
    pub t_start: Timestamp,
    /// End time; `None` while open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t_end: Option<Timestamp>,
    /// Run summary; `None` while open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<SurveySummary>,
}
