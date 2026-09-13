//! Scheduler settings (C04 v1): defaults, validation, and the `extra.scheduler` object of a
//! [`ScanPlan`].

use hk_model::{ScanPlan, ScanPolicy};

use super::plan::{PlanError, check_gains};
use crate::source::{Gains, SourceCapabilities};

/// Most interleaved gain-step pairs in one verification group.
pub const MAX_GAIN_STEP_PAIRS: u8 = 4;

/// HackRF One RF-path switch frequencies, Hz: low-pass mixer path below 2170 MHz, bypass to
/// 2740 MHz, high-pass mixer path above (hackrf firmware `tuning.c` constants, not re-verified
/// here; S4 §3.7 measured the floor step at ~2.74 GHz).
pub const HACKRF_ONE_RF_PATH_BOUNDARIES_HZ: [f64; 2] = [2170e6, 2740e6];

const NS_PER_S: f64 = 1e9;

/// Scheduler settings. [`Default`] is the HackRF One v1 profile.
///
/// A plan can override a subset in `ScanPlan::extra` under `"scheduler"` (see
/// [`SchedulerConfig::from_plan`]); durations there are in seconds (`*_s`).
#[derive(Clone, Debug, PartialEq)]
pub struct SchedulerConfig {
    /// Sample rate of discovery hops (sweep and dwell-only region windows), Hz (20 Msps).
    pub sweep_rate_hz: f64,
    /// Fraction of the rate treated as usable span, excluding anti-alias skirts (0.75: 15 MHz of
    /// 20, docs/capabilities/C03).
    pub usable_fraction: f64,
    /// Largest usable span of one window, Hz (20 MHz: one HackRF window).
    pub max_span_hz: f64,
    /// One sweep hop, ns (50 ms ≈ 24 S4 frames of nfft 4096 × n_avg 10 at 20 Msps).
    pub sweep_step_ns: i64,
    /// One window of a dwell-only region, ns (5 s).
    pub region_dwell_ns: i64,
    /// Discovery steps per cycle (≥ 1, so discovery never starves).
    pub sweeps_per_cycle: u32,
    /// POI dwell slots per cycle (a whole verification group is one slot).
    pub dwells_per_cycle: u32,
    /// Lowest POI dwell rate, Hz (8 Msps: run the ADC fast and decimate on the host, C03).
    pub dwell_min_rate_hz: f64,
    /// Continuous-rate sources: POI dwell rates round up to a multiple of this, Hz (1 MHz).
    pub rate_quantum_hz: f64,
    /// POI dwell without a known burst interval, ns (2 s).
    pub dwell_default_ns: i64,
    /// Shortest POI dwell, ns (0.5 s).
    pub dwell_min_ns: i64,
    /// Longest POI dwell, ns (30 s).
    pub dwell_max_ns: i64,
    /// A POI with a known burst interval dwells this many intervals (3).
    pub burst_intervals_per_dwell: f64,
    /// User-intent step length, ns (1 s); an intent repeats until it ends or is released.
    pub intent_slice_ns: i64,
    /// Gains outside every gain-table band (LNA 24 / VGA 20 / amp off: the S4 mid gain).
    pub default_gains: Gains,
    /// Interleaved gain-step pairs per verification (3: S4 rule 6 "A/B × 3"; 0 disables).
    pub gain_step_pairs: u8,
    /// Length of each gain-step block, ns (0.5 s).
    pub gain_step_block_ns: i64,
    /// LNA step between states A and B, dB (8: one HackRF LNA step; S4 rule 6 requires the step
    /// to include the LNA or amp). B steps up, or down when A is at the LNA maximum.
    pub gain_step_lna_db: f64,
    /// Retune-test offset, Hz (1 MHz, S4 rule 7; 0 disables). Both +Δ and −Δ are scheduled
    /// when they stay inside the source range and on the same RF path.
    pub retune_delta_hz: f64,
    /// Length of each retune, rate-change and baseline dwell, ns (0.5 s).
    pub retune_dwell_ns: i64,
    /// Add a sample-rate-change dwell to verification (clock-harmonic confirmation: an `n × fs`
    /// line moves with the rate, a real emitter does not). Off by default.
    pub rate_change: bool,
    /// Rate-change candidates: rate × factor first, then rate ÷ factor (0.8).
    pub rate_change_factor: f64,
    /// RF-path switch frequencies, ascending, Hz (HackRF One defaults). Sweep hops never
    /// straddle one and every step carries its path.
    pub rf_path_boundaries_hz: Vec<f64>,
    /// POI queue capacity, preallocated (64).
    pub max_pois: usize,
    /// Per-region policy overrides by region index (`None` = the plan's policy).
    pub region_policy: Vec<Option<ScanPolicy>>,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            sweep_rate_hz: 20e6,
            usable_fraction: 0.75,
            max_span_hz: 20e6,
            sweep_step_ns: 50_000_000,
            region_dwell_ns: 5_000_000_000,
            sweeps_per_cycle: 4,
            dwells_per_cycle: 1,
            dwell_min_rate_hz: 8e6,
            rate_quantum_hz: 1e6,
            dwell_default_ns: 2_000_000_000,
            dwell_min_ns: 500_000_000,
            dwell_max_ns: 30_000_000_000,
            burst_intervals_per_dwell: 3.0,
            intent_slice_ns: 1_000_000_000,
            default_gains: Gains {
                lna_db: 24.0,
                vga_db: 20.0,
                amp_on: false,
            },
            gain_step_pairs: 3,
            gain_step_block_ns: 500_000_000,
            gain_step_lna_db: 8.0,
            retune_delta_hz: 1e6,
            retune_dwell_ns: 500_000_000,
            rate_change: false,
            rate_change_factor: 0.8,
            rf_path_boundaries_hz: HACKRF_ONE_RF_PATH_BOUNDARIES_HZ.to_vec(),
            max_pois: 64,
            region_policy: Vec::new(),
        }
    }
}

impl SchedulerConfig {
    /// Defaults overridden by `plan.extra["scheduler"]`, if present. Keys: `sweep_rate_hz`,
    /// `usable_fraction`, `sweep_step_s`, `region_dwell_s`, `sweeps_per_cycle`,
    /// `dwells_per_cycle`, `dwell_min_rate_hz`, `dwell_default_s`, `dwell_min_s`,
    /// `dwell_max_s`, `gain_step_pairs`, `gain_step_block_s`, `gain_step_lna_db`,
    /// `retune_delta_hz`, `retune_dwell_s`, `rate_change`, `region_policy` (array of
    /// `null` / `"sweep-only"` / `"dwell-only"` / `"sweep-then-dwell"`). Unknown keys are errors.
    pub fn from_plan(plan: &ScanPlan) -> Result<Self, PlanError> {
        let mut cfg = Self::default();
        let Some(settings) = plan.extra.get("scheduler") else {
            return Ok(cfg);
        };
        let settings = settings
            .as_object()
            .ok_or_else(|| PlanError::InvalidConfig("extra.scheduler must be an object".into()))?;
        for (key, value) in settings {
            let bad = || {
                PlanError::InvalidConfig(format!("extra.scheduler.{key}: invalid value {value}"))
            };
            let num = || value.as_f64().filter(|v| v.is_finite()).ok_or_else(bad);
            let secs = || num().map(|s| (s * NS_PER_S).round() as i64);
            let count = || {
                value
                    .as_u64()
                    .and_then(|v| u32::try_from(v).ok())
                    .ok_or_else(bad)
            };
            match key.as_str() {
                "sweep_rate_hz" => cfg.sweep_rate_hz = num()?,
                "usable_fraction" => cfg.usable_fraction = num()?,
                "sweep_step_s" => cfg.sweep_step_ns = secs()?,
                "region_dwell_s" => cfg.region_dwell_ns = secs()?,
                "sweeps_per_cycle" => cfg.sweeps_per_cycle = count()?,
                "dwells_per_cycle" => cfg.dwells_per_cycle = count()?,
                "dwell_min_rate_hz" => cfg.dwell_min_rate_hz = num()?,
                "dwell_default_s" => cfg.dwell_default_ns = secs()?,
                "dwell_min_s" => cfg.dwell_min_ns = secs()?,
                "dwell_max_s" => cfg.dwell_max_ns = secs()?,
                "gain_step_pairs" => {
                    cfg.gain_step_pairs = u8::try_from(count()?).map_err(|_| bad())?;
                }
                "gain_step_block_s" => cfg.gain_step_block_ns = secs()?,
                "gain_step_lna_db" => cfg.gain_step_lna_db = num()?,
                "retune_delta_hz" => cfg.retune_delta_hz = num()?,
                "retune_dwell_s" => cfg.retune_dwell_ns = secs()?,
                "rate_change" => cfg.rate_change = value.as_bool().ok_or_else(bad)?,
                "region_policy" => {
                    cfg.region_policy = serde_json::from_value(value.clone()).map_err(|_| bad())?;
                }
                _ => {
                    return Err(PlanError::InvalidConfig(format!(
                        "unknown setting extra.scheduler.{key}"
                    )));
                }
            }
        }
        Ok(cfg)
    }

    /// Checks the settings against the source's capabilities.
    pub fn validate(&self, caps: &SourceCapabilities) -> Result<(), PlanError> {
        let fail = |msg: String| Err(PlanError::InvalidConfig(msg));
        if !(self.sweep_rate_hz.is_finite() && caps.sample_rates.supports(self.sweep_rate_hz)) {
            return fail(format!(
                "sweep rate {} Hz is not supported by {}",
                self.sweep_rate_hz, caps.driver
            ));
        }
        if !(self.usable_fraction > 0.0 && self.usable_fraction <= 1.0) {
            return fail("usable_fraction must be in (0, 1]".into());
        }
        if !(self.max_span_hz.is_finite() && self.max_span_hz > 0.0) {
            return fail("max_span_hz must be finite and > 0".into());
        }
        for (name, ns) in [
            ("sweep_step", self.sweep_step_ns),
            ("region_dwell", self.region_dwell_ns),
            ("dwell_default", self.dwell_default_ns),
            ("dwell_min", self.dwell_min_ns),
            ("dwell_max", self.dwell_max_ns),
            ("intent_slice", self.intent_slice_ns),
            ("gain_step_block", self.gain_step_block_ns),
            ("retune_dwell", self.retune_dwell_ns),
        ] {
            if ns <= 0 {
                return fail(format!("{name} duration must be > 0"));
            }
        }
        if self.dwell_min_ns > self.dwell_max_ns {
            return fail("dwell_min must not exceed dwell_max".into());
        }
        if self.sweeps_per_cycle == 0 {
            return fail("sweeps_per_cycle must be >= 1 (discovery must not starve)".into());
        }
        if self.gain_step_pairs > MAX_GAIN_STEP_PAIRS {
            return fail(format!("gain_step_pairs must be <= {MAX_GAIN_STEP_PAIRS}"));
        }
        let positive = |v: f64| v.is_finite() && v > 0.0;
        if !(positive(self.dwell_min_rate_hz)
            && positive(self.rate_quantum_hz)
            && positive(self.burst_intervals_per_dwell))
        {
            return fail(
                "dwell_min_rate_hz, rate_quantum_hz and burst_intervals_per_dwell must be > 0"
                    .into(),
            );
        }
        let non_negative = |v: f64| v.is_finite() && v >= 0.0;
        if !(non_negative(self.gain_step_lna_db) && non_negative(self.retune_delta_hz)) {
            return fail("gain_step_lna_db and retune_delta_hz must be finite and >= 0".into());
        }
        if !(self.rate_change_factor > 0.0 && self.rate_change_factor < 1.0) {
            return fail("rate_change_factor must be in (0, 1)".into());
        }
        let b = &self.rf_path_boundaries_hz;
        if b.len() > usize::from(u8::MAX)
            || b.iter().any(|f| !f.is_finite())
            || b.windows(2).any(|w| w[0] >= w[1])
        {
            return fail("rf_path_boundaries_hz must be finite, ascending, <= 255 entries".into());
        }
        if self.max_pois == 0 {
            return fail("max_pois must be >= 1".into());
        }
        check_gains(caps, &self.default_gains).map_err(|reason| PlanError::InvalidGain {
            index: None,
            reason,
        })
    }
}
