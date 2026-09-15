//! JSON report types. Everything here is deterministic for a seed (no wall-clock values).

use serde::Serialize;
use serde_json::Value;

use crate::radio::RadioModel;
use crate::scenario::EmitterClass;

/// Report schema id.
pub const SCHEMA: &str = "hk-sim/comparison/v1";

/// A comparison of policies on one scenario.
#[derive(Clone, Debug, Serialize)]
pub struct ComparisonReport {
    /// [`SCHEMA`].
    pub schema: &'static str,
    /// Scenario summary.
    pub scenario: ScenarioSummary,
    /// Radio model.
    pub radio: RadioModel,
    /// One report per policy, in run order.
    pub policies: Vec<PolicyReport>,
}

/// Scenario summary.
#[derive(Clone, Debug, Serialize)]
pub struct ScenarioSummary {
    /// Name.
    pub name: String,
    /// Seed.
    pub seed: u64,
    /// Simulated duration, s.
    pub duration_s: f64,
    /// Plan regions `[lo, hi]`, Hz.
    pub regions_hz: Vec<[f64; 2]>,
    /// Emitters per class.
    pub emitters: Vec<ClassCount>,
    /// Emitters injected mid-run.
    pub injected: usize,
}

/// A count per class.
#[derive(Clone, Debug, Serialize)]
pub struct ClassCount {
    /// Class.
    pub class: EmitterClass,
    /// Count.
    pub count: usize,
}

/// One policy's run.
#[derive(Clone, Debug, Serialize)]
pub struct PolicyReport {
    /// Policy name.
    pub name: String,
    /// Policy parameters.
    pub params: Value,
    /// Radio time budget.
    pub time: TimeBudget,
    /// Discovery.
    pub discovery: DiscoveryReport,
    /// Per class.
    pub classes: Vec<ClassReport>,
    /// Time to first detection.
    pub ttfd: TtfdReport,
    /// Per plan region: revisit and POI.
    pub regions: Vec<RegionReport>,
    /// Dwell spent on suspect emitters.
    pub suspect: SuspectReport,
}

/// Where the radio's time went.
#[derive(Clone, Debug, Default, Serialize)]
pub struct TimeBudget {
    /// Steps executed.
    pub steps: u64,
    /// Sweep-mode step time, s.
    pub sweep_s: f64,
    /// Stream-mode (dwell) step time, s.
    pub stream_s: f64,
    /// Dead time (retunes, settles, mode switches), s.
    pub dead_s: f64,
    /// Sweep↔stream switches.
    pub mode_switches: u64,
}

/// Emitters discovered.
#[derive(Clone, Debug, Serialize)]
pub struct DiscoveryReport {
    /// Non-suspect emitters in the population.
    pub emitters: usize,
    /// Non-suspect emitters detected at least once.
    pub discovered: usize,
    /// Suspect emitters detected at least once.
    pub suspect_discovered: usize,
    /// Discovered (non-suspect) vs time.
    pub curve: Vec<CurvePoint>,
}

/// One point of the discovery curve.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct CurvePoint {
    /// Time, s.
    pub t_s: f64,
    /// Non-suspect emitters present by then.
    pub present: usize,
    /// Of them, discovered.
    pub discovered: usize,
}

/// Per-class capture.
#[derive(Clone, Debug, Serialize)]
pub struct ClassReport {
    /// Class.
    pub class: EmitterClass,
    /// Emitters.
    pub emitters: usize,
    /// Emitters detected at least once.
    pub discovered: usize,
    /// Transmissions (a continuous emitter is one).
    pub transmissions: u64,
    /// Transmissions detected at least once.
    pub captured: u64,
    /// Captured per simulated hour.
    pub captured_per_hour: f64,
    /// Captured / transmissions.
    pub capture_fraction: Option<f64>,
}

/// Time to first detection.
#[derive(Clone, Debug, Serialize)]
pub struct TtfdReport {
    /// Emitters injected mid-run.
    pub injected: Vec<InjectedTtfd>,
    /// Median over discovered non-suspect emitters, s (from their appearance).
    pub median_s: Option<f64>,
    /// 90th percentile, s.
    pub p90_s: Option<f64>,
    /// Non-suspect emitters never detected.
    pub undiscovered: usize,
}

/// One injected emitter.
#[derive(Clone, Debug, Serialize)]
pub struct InjectedTtfd {
    /// Emitter id.
    pub emitter: u64,
    /// Class.
    pub class: EmitterClass,
    /// Centre, Hz.
    pub center_hz: f64,
    /// Appearance, s.
    pub appears_s: f64,
    /// Time to first detection, s; `None` if never detected.
    pub ttfd_s: Option<f64>,
}

/// Revisit and POI of one plan region.
#[derive(Clone, Debug, Serialize)]
pub struct RegionReport {
    /// Region index.
    pub region: usize,
    /// `[lo, hi]`, Hz.
    pub freq_hz: [f64; 2],
    /// Revisit bins in the region.
    pub bins: usize,
    /// Bins never observed.
    pub bins_unvisited: usize,
    /// Mean over bins of the mean interval between visit starts, s (contiguous windows merge
    /// into one visit).
    pub revisit_mean_s: Option<f64>,
    /// Longest interval between visits of any bin, s.
    pub revisit_max_s: Option<f64>,
    /// Mean live time per visit, s (T_d).
    pub visit_mean_s: Option<f64>,
    /// Fraction of region-time observed.
    pub observed_fraction: f64,
    /// Short-burst POI (non-suspect bursts and beacons).
    pub poi: PoiReport,
}

/// Measured POI against P_POI ≈ min(1, (τ + T_d)/T_R) (docs/04 §3.8).
#[derive(Clone, Debug, Serialize)]
pub struct PoiReport {
    /// Burst and beacon emitters in the region.
    pub emitters: usize,
    /// Their transmissions.
    pub transmissions: u64,
    /// Detected.
    pub captured: u64,
    /// captured / transmissions.
    pub measured: Option<f64>,
    /// Transmission-weighted formula prediction from each emitter's τ and its bin's measured
    /// T_d and T_R.
    pub predicted: Option<f64>,
    /// Standard error of `measured` under the prediction (Poisson-binomial).
    pub std_err: Option<f64>,
    /// (measured − predicted) / std_err.
    pub z: Option<f64>,
}

/// Dwell spent on suspect emitters.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SuspectReport {
    /// Stream-mode step time aimed at a POI that is a suspect emitter, s.
    pub dwell_s_on_suspect_pois: f64,
    /// Live stream-mode time whose detections were all suspect, s.
    pub dwell_s_suspect_only: f64,
}
