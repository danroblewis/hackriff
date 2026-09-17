//! The iterative scan (T-406, `docs/16` §7 step 6): **a dwell policy over the scheduler that
//! already exists**, not a second scheduler.
//!
//! The user's ask: *step the tune to the next region after sampling enough per step (~10–30 s,
//! configurable), and populate and retain the full-spectrum survey so probable signals across the
//! whole spectrum become visible.*
//!
//! # It is three lines of policy, and deliberately nothing else
//!
//! Everything the iterative scan needs is already here. [`CompiledPlan`] tiles a frequency range
//! into hops no wider than the usable span and visits them in order, wrapping at the end of a pass;
//! [`ScanPolicy::DwellOnly`] makes those hops [`HopKind::RegionDwell`] instead of sweep hops; and
//! [`SchedulerConfig::region_dwell_ns`] is how long each one holds. So an iterative scan **is** a
//! `DwellOnly` plan over the device's tunable range with a long `region_dwell_ns`, and this module
//! is the one place that says so, validates the dwell and prices the pass.
//!
//! [`IterativeScan::plan`] writes the dwell into `extra.scheduler.region_dwell_s`, so the plan
//! carries its own policy: it survives serialisation to a `--plan` JSON file and
//! [`SchedulerConfig::from_plan`] reads it back with no second code path.
//!
//! # Why a dwell and not a sweep hop, which is the whole of `docs/16` §7 step 6
//!
//! A `DwellOnly` hop emits [`crate::scheduler::Purpose::RegionDwell`], and
//! [`crate::scheduler::observe::ObservationRecorder`] writes **one [`DwellRecord`] per step**, with
//! that step's own analysed band in `window` and its own settled interval in `observed`. Only
//! [`crate::scheduler::Purpose::Sweep`] aggregates, and it aggregates into a record that spans a
//! whole pass.
//!
//! That is not a detail of bookkeeping. `hk_store::coverage::grid_over` rasterises coverage records
//! onto a time–frequency grid, and it can only tell the truth about a dwell pattern if the records
//! it reads **have the dwell's own shape**. One coarse record spanning a whole pass rasterises as
//! *the whole band, the whole time* — the overclaim the coverage map exists to prevent. Choosing
//! the dwell purpose is therefore the load-bearing decision of this module, and
//! `hk-pipeline/tests/iterative_scan.rs` asserts it end to end: scheduler → recorder → observation
//! log → coverage grid, and no [`hk_model::attention::observation::SweepRecord`] anywhere in it.
//!
//! # What a step of this length can and cannot catch
//!
//! The project's own finding (`docs/02`) is that *a HackRF sweeps 0–6 GHz in under a second but
//! misses short bursts*. 10–30 s per step is a **dwell, not a sweep**, and the trade it makes is
//! exactly measurable — [`ScanBudget`] computes it from the compiled plan:
//!
//! - **Inside its own dwell** a step catches anything present for at least one analysis frame
//!   (≈ 0.2 ms at 20 Msps / 4096 bins), at the full resolution of a live window. A 50 ms sweep hop
//!   catches only what is on the air during those 50 ms, so a 15 s dwell is **300× more listening
//!   per visit** and is what makes an ISM-band burst, a pager page or a one-off chirp findable at
//!   all.
//! - **Outside its own dwell it catches nothing**, and the pass is long: the HackRF's 1 MHz–6 GHz
//!   range is ~400 hops at a 15 MHz usable span, so a 15 s dwell is a **100-minute** pass and each
//!   band is listened to for 15 s in every 6 000 — a duty of about **0.25 %**. An emission whose
//!   total on-air time is short and whose timing is uncorrelated with the pass is found with
//!   roughly that probability per occurrence.
//! - **So the dwell length buys sensitivity to short bursts and pays for it in revisit latency**,
//!   one for one: [`ScanBudget::pass_ns`] is linear in the dwell. Narrow the plan's range and the
//!   pass shortens in proportion — surveying one 100 MHz band at 15 s is a 105-second pass, which
//!   is why the range is a parameter and the dwell is not tuned to one band.
//! - **Nothing here changes what a step cannot see at all:** anything outside the tuned window for
//!   that step's duration is `Unobserved` in the coverage map, never "quiet". That distinction is
//!   `hk_store::Coverage`'s and this policy must not blur it — a sweep that greys what it has
//!   already cleared is useless, and one that paints unreached spectrum as quiet is dishonest.
//!
//! # It does not own the radio
//!
//! An iterative scan runs at [`hk_model::attention::observation::Tier::ScheduledPlan`], below
//! pinned leases and interactive intent, so a user tune, a decoder lease or a verification group
//! still preempts it and the cut step is recorded as `preempted` with the interval it actually got.
//! With several front ends the scheduler arbitrates as it already does; this module adds no
//! assumption that one radio is free.

use hk_model::{
    FreqRange, PlanRegion, ScanPlan, ScanPlanId, ScanPolicy, Schedule, Timestamp,
    attention::observation::DwellRecord,
};

use super::config::SchedulerConfig;
use super::plan::{CompiledPlan, HopKind, PlanError};
use crate::source::SourceCapabilities;

/// Nanoseconds in a second.
const NS_PER_S: f64 = 1e9;

/// The dwell an iterative scan uses when the caller names none: 15 s, the middle of the range the
/// user asked for.
pub const DEFAULT_DWELL_NS: i64 = 15_000_000_000;

/// Shortest dwell the user's range recommends, ns (10 s). Not a hard limit — see
/// [`IterativeScan::recommended`].
pub const RECOMMENDED_MIN_DWELL_NS: i64 = 10_000_000_000;

/// Longest dwell the user's range recommends, ns (30 s).
pub const RECOMMENDED_MAX_DWELL_NS: i64 = 30_000_000_000;

/// Hard bound on a configured dwell, ns (5 minutes).
///
/// The bound exists because the dwell is linear in the pass: at 400 hops a 5-minute dwell is a
/// 33-hour pass, past which "iterative scan" stops describing anything. A survey that wants to sit
/// on one band for longer wants a narrower range, not a longer step.
pub const MAX_DWELL_NS: i64 = 300_000_000_000;

/// A dwell policy: how long the tune holds before it steps to the next region.
///
/// Build a plan with [`IterativeScan::plan`] (or [`IterativeScan::plan_over`] for one range), and
/// price it with [`IterativeScan::budget`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IterativeScan {
    dwell_ns: i64,
}

impl Default for IterativeScan {
    fn default() -> Self {
        Self {
            dwell_ns: DEFAULT_DWELL_NS,
        }
    }
}

impl IterativeScan {
    /// A policy with this dwell, ns.
    ///
    /// Refuses a non-positive dwell and one past [`MAX_DWELL_NS`]. A dwell outside the
    /// recommended 10–30 s is **allowed** — the user asked for the number to be configurable, not
    /// clamped to one band's taste — and [`IterativeScan::recommended`] reports whether it is
    /// inside the range so a caller can say so.
    pub fn new(dwell_ns: i64) -> Result<Self, PlanError> {
        if dwell_ns <= 0 || dwell_ns > MAX_DWELL_NS {
            return Err(PlanError::InvalidConfig(format!(
                "iterative-scan dwell must be > 0 and <= {} s, got {} s",
                MAX_DWELL_NS as f64 / NS_PER_S,
                dwell_ns as f64 / NS_PER_S,
            )));
        }
        Ok(Self { dwell_ns })
    }

    /// [`IterativeScan::new`] from seconds (the CLI and `extra.scheduler` unit).
    pub fn from_seconds(dwell_s: f64) -> Result<Self, PlanError> {
        if !dwell_s.is_finite() {
            return Err(PlanError::InvalidConfig(
                "iterative-scan dwell must be a finite number of seconds".into(),
            ));
        }
        Self::new((dwell_s * NS_PER_S).round() as i64)
    }

    /// The dwell, ns.
    pub fn dwell_ns(self) -> i64 {
        self.dwell_ns
    }

    /// The dwell, s.
    pub fn dwell_s(self) -> f64 {
        self.dwell_ns as f64 / NS_PER_S
    }

    /// Whether the dwell is inside the 10–30 s range the user asked for. A policy outside it still
    /// runs; this is what lets a caller report that it is unusual rather than refuse it.
    pub fn recommended(self) -> bool {
        (RECOMMENDED_MIN_DWELL_NS..=RECOMMENDED_MAX_DWELL_NS).contains(&self.dwell_ns)
    }

    /// Writes the dwell into a scheduler config (the same field `extra.scheduler.region_dwell_s`
    /// sets).
    pub fn apply(self, cfg: &mut SchedulerConfig) {
        cfg.region_dwell_ns = self.dwell_ns;
    }

    /// A `DwellOnly` plan over `ranges`, carrying this dwell in `extra.scheduler.region_dwell_s`.
    ///
    /// Ranges outside the source are clipped by [`CompiledPlan::compile`] and reported as
    /// [`super::PlanWarning::ClippedToCapabilities`] — this does not pre-filter them, so a plan
    /// says what was asked for and the compilation says what the device can do.
    pub fn plan(
        self,
        name: impl Into<String>,
        ranges: &[FreqRange],
        created_at: Timestamp,
    ) -> ScanPlan {
        ScanPlan {
            id: ScanPlanId::new(),
            version: 1,
            name: name.into(),
            created_at,
            regions: ranges
                .iter()
                .map(|freq| PlanRegion {
                    freq: *freq,
                    priority: 1.0,
                    // No revisit target: the pass length IS the revisit here, and asserting a
                    // target the dwell cannot meet would only produce a warning per compilation.
                    revisit_ns: None,
                })
                .collect(),
            policy: ScanPolicy::DwellOnly,
            gain_table: Vec::new(),
            schedule: Schedule::Continuous,
            extra: serde_json::json!({
                "scheduler": { "region_dwell_s": self.dwell_s() }
            }),
        }
    }

    /// [`IterativeScan::plan`] over one range.
    pub fn plan_over(
        self,
        name: impl Into<String>,
        range: FreqRange,
        created_at: Timestamp,
    ) -> ScanPlan {
        self.plan(name, &[range], created_at)
    }

    /// A plan over **everything this front end can tune** — the full-spectrum survey.
    ///
    /// The ranges are the source's own [`SourceCapabilities::frequency_ranges`], so a device that
    /// reaches 1 MHz–6 GHz gets that and a replay gets its recording's band. A source that states
    /// no range yields a plan with no regions, which [`CompiledPlan::compile`] refuses with
    /// [`PlanError::Empty`] rather than inventing a range to scan.
    pub fn plan_over_capabilities(
        self,
        caps: &SourceCapabilities,
        created_at: Timestamp,
    ) -> ScanPlan {
        let ranges: Vec<FreqRange> = caps
            .frequency_ranges
            .iter()
            .map(|r| FreqRange::new(r.min_hz, r.max_hz))
            .collect();
        self.plan("iterative scan", &ranges, created_at)
    }

    /// What this policy costs and buys on a compiled plan (see the module docs).
    pub fn budget(self, plan: &CompiledPlan) -> ScanBudget {
        ScanBudget::of(plan)
    }
}

/// The price of one iterative-scan pass: how many steps, how long each, how long until the tune
/// comes back.
///
/// Computed from the **compiled** plan, so it counts the hops the device will really visit after
/// clipping to its tunable ranges, RF-path cuts and gain-table edges — not the hops a plan asked
/// for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScanBudget {
    /// Region-dwell steps in one pass.
    pub steps: usize,
    /// Planned length of one step, ns (the longest, when hops differ).
    pub dwell_ns: i64,
    /// One whole pass, ns: `Σ hop.duration_ns` over the dwell hops. This is the **revisit**
    /// interval — how long a band waits between looks.
    pub pass_ns: i64,
    /// Total frequency extent the dwell hops cover, Hz.
    pub span_hz: f64,
    /// Extent one step covers, Hz (the usable span of one window).
    pub step_span_hz: f64,
}

impl ScanBudget {
    /// The budget of `plan`'s region-dwell hops. Sweep hops are not counted: they are a different
    /// policy, and a plan that mixes them is priced on its dwell half only.
    pub fn of(plan: &CompiledPlan) -> Self {
        let dwells = plan.hops.iter().filter(|h| h.kind == HopKind::RegionDwell);
        let mut steps = 0usize;
        let mut pass_ns = 0i64;
        let mut dwell_ns = 0i64;
        let mut span_hz = 0.0;
        for h in dwells {
            steps += 1;
            pass_ns = pass_ns.saturating_add(h.duration_ns);
            dwell_ns = dwell_ns.max(h.duration_ns);
            span_hz += h.covers.width_hz();
        }
        ScanBudget {
            steps,
            dwell_ns,
            pass_ns,
            span_hz,
            step_span_hz: plan.usable_span_hz,
        }
    }

    /// Fraction of wall time any one band is actually being listened to: `dwell / pass`.
    ///
    /// This is the honest headline of the trade. It is **not** a detection probability: an
    /// emission that is on the air continuously is caught on every visit whatever the duty, and an
    /// emission whose occurrences are uncorrelated with the pass is caught with roughly this
    /// probability per occurrence. `0.0` for a plan with no dwell hops.
    pub fn duty(&self) -> f64 {
        if self.pass_ns <= 0 {
            return 0.0;
        }
        (self.dwell_ns as f64 / self.pass_ns as f64).clamp(0.0, 1.0)
    }

    /// How long a band waits between looks, s — the same number as [`ScanBudget::pass_ns`], which
    /// is what "revisit" means for a policy that visits every hop once per pass.
    pub fn revisit_s(&self) -> f64 {
        self.pass_ns as f64 / NS_PER_S
    }

    /// One line a log or a status page can print, stating what this pass catches and what it does
    /// not. The wording is the module docs' trade, with this plan's numbers in it.
    pub fn statement(&self) -> String {
        format!(
            "iterative scan: {} steps × {:.1} s = a {:.1} s pass over {:.3} MHz ({:.3} MHz per step); \
             each band is listened to {:.1} s in every {:.1} s (duty {:.3} %). A step catches \
             anything on the air during its own dwell; it catches nothing during the other {:.1} s, \
             and spectrum the pass has not reached is unobserved, never quiet.",
            self.steps,
            self.dwell_ns as f64 / NS_PER_S,
            self.revisit_s(),
            self.span_hz / 1e6,
            self.step_span_hz / 1e6,
            self.dwell_ns as f64 / NS_PER_S,
            self.revisit_s(),
            self.duty() * 100.0,
            (self.pass_ns - self.dwell_ns).max(0) as f64 / NS_PER_S,
        )
    }
}

/// Whether `rec` is the record an iterative-scan step writes: a dwell record whose reason is a
/// region-dwell.
///
/// This is the predicate `docs/16` §7 step 6 turns on — *one record per step with its true band and
/// interval* — exposed so a test, a report or a status page can check the shape of what was written
/// rather than assume it.
pub fn is_scan_step(rec: &DwellRecord) -> bool {
    matches!(
        rec.reason,
        hk_model::attention::observation::Reason::RegionDwell { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{SampleRates, SourceCapabilities};

    fn caps() -> SourceCapabilities {
        let mut c = SourceCapabilities::hackrf_one();
        c.sample_rates = SampleRates::Discrete(vec![20e6]);
        c
    }

    fn cfg() -> SchedulerConfig {
        SchedulerConfig {
            sweep_rate_hz: 20e6,
            ..SchedulerConfig::default()
        }
    }

    #[test]
    fn a_dwell_outside_the_recommended_range_is_allowed_but_says_so() {
        let mid = IterativeScan::new(DEFAULT_DWELL_NS).unwrap();
        assert!(mid.recommended());
        assert!(IterativeScan::from_seconds(10.0).unwrap().recommended());
        assert!(IterativeScan::from_seconds(30.0).unwrap().recommended());
        // Configurable, not clamped: a 2 s or a 120 s dwell is a legal policy that simply is not
        // the one the user named.
        let short = IterativeScan::from_seconds(2.0).unwrap();
        assert!(!short.recommended());
        assert_eq!(short.dwell_ns(), 2_000_000_000);
        assert!(!IterativeScan::from_seconds(120.0).unwrap().recommended());
        // The two ends that are not policies at all.
        assert!(IterativeScan::new(0).is_err());
        assert!(IterativeScan::new(-1).is_err());
        assert!(IterativeScan::new(MAX_DWELL_NS + 1).is_err());
        assert!(IterativeScan::from_seconds(f64::NAN).is_err());
        assert!(IterativeScan::from_seconds(f64::INFINITY).is_err());
    }

    /// The load-bearing property: the plan an iterative scan builds compiles to **region-dwell**
    /// hops, each planned for the configured dwell — so every step writes its own dwell record
    /// (`docs/16` §7 step 6) instead of folding into one coarse sweep record.
    #[test]
    fn the_plan_compiles_to_region_dwell_hops_of_the_configured_length() {
        let scan = IterativeScan::from_seconds(12.0).unwrap();
        let plan = scan.plan_over(
            "test",
            FreqRange::new(100e6, 200e6),
            Timestamp::from_unix_nanos(0),
        );
        assert_eq!(plan.policy, ScanPolicy::DwellOnly);
        // The dwell travels with the plan: a `--plan` JSON round trip keeps the policy.
        let json = serde_json::to_string(&plan).unwrap();
        let back: ScanPlan = serde_json::from_str(&json).unwrap();
        let cfg = SchedulerConfig::from_plan(&back).unwrap();
        assert_eq!(cfg.region_dwell_ns, 12_000_000_000);

        let compiled = CompiledPlan::compile(&back, &cfg, &caps()).unwrap();
        assert!(!compiled.hops.is_empty());
        assert!(
            compiled.hops.iter().all(|h| h.kind == HopKind::RegionDwell),
            "an iterative scan emits no sweep hops"
        );
        assert!(
            compiled
                .hops
                .iter()
                .all(|h| h.duration_ns == 12_000_000_000),
            "every hop holds the configured dwell"
        );
    }

    /// The budget is the "what it can and cannot catch" statement as data, and it is linear in the
    /// dwell: doubling the dwell doubles the pass and leaves the duty alone.
    #[test]
    fn the_budget_prices_the_pass_and_is_linear_in_the_dwell() {
        let range = FreqRange::new(100e6, 200e6);
        let t = Timestamp::from_unix_nanos(0);
        let build = |dwell_s: f64| {
            let scan = IterativeScan::from_seconds(dwell_s).unwrap();
            let plan = scan.plan_over("test", range, t);
            let mut c = cfg();
            scan.apply(&mut c);
            let compiled = CompiledPlan::compile(&plan, &c, &caps()).unwrap();
            (scan.budget(&compiled), compiled)
        };
        let (b15, compiled) = build(15.0);
        // 100 MHz at a 15 MHz usable span: seven windows.
        assert_eq!(b15.steps, compiled.hops.len());
        assert!(b15.steps >= 7, "{} steps for 100 MHz", b15.steps);
        assert_eq!(b15.dwell_ns, 15_000_000_000);
        assert_eq!(b15.pass_ns, 15_000_000_000 * b15.steps as i64);
        assert!((b15.duty() - 1.0 / b15.steps as f64).abs() < 1e-12);
        assert!((b15.span_hz - 100e6).abs() < 1e-3, "{}", b15.span_hz);

        let (b30, _) = build(30.0);
        assert_eq!(b30.steps, b15.steps);
        assert_eq!(
            b30.pass_ns,
            2 * b15.pass_ns,
            "the pass is linear in the dwell"
        );
        assert!(
            (b30.duty() - b15.duty()).abs() < 1e-12,
            "the duty is not: it is 1/steps whatever the dwell"
        );
        // And the statement says the two halves out loud.
        let s = b15.statement();
        assert!(s.contains("unobserved, never quiet"), "{s}");
        assert!(s.contains("duty"), "{s}");
    }

    /// The full-spectrum plan takes its range from the device, never from a constant.
    #[test]
    fn the_full_spectrum_plan_asks_the_device_what_it_can_tune() {
        let caps = caps();
        let plan =
            IterativeScan::default().plan_over_capabilities(&caps, Timestamp::from_unix_nanos(0));
        assert_eq!(plan.regions.len(), caps.frequency_ranges.len());
        for (r, c) in plan.regions.iter().zip(&caps.frequency_ranges) {
            assert_eq!(r.freq.lo_hz, c.min_hz);
            assert_eq!(r.freq.hi_hz, c.max_hz);
        }
        // A source that states no tunable range gets no regions, and compiling refuses rather
        // than inventing a spectrum to scan.
        let mut blind = caps.clone();
        blind.frequency_ranges.clear();
        let empty =
            IterativeScan::default().plan_over_capabilities(&blind, Timestamp::from_unix_nanos(0));
        assert!(empty.regions.is_empty());
        assert!(matches!(
            CompiledPlan::compile(&empty, &cfg(), &blind),
            Err(PlanError::Empty)
        ));
    }
}
