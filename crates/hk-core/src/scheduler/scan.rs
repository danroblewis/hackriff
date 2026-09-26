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
    ///
    /// The per-step cost **beyond** the dwell is unpriced here, and that is deliberate: nothing in
    /// a plan knows it. [`ScanBudget::with_step_overhead`] prices it once something has measured
    /// it; [`measured_step_overhead_ns`] is what measures it.
    pub fn budget(self, plan: &CompiledPlan) -> ScanBudget {
        ScanBudget::of(plan)
    }
}

/// The per-step cost a pass really pays **beyond its dwell**, from a measurement of a pass that
/// ran: `None` when nothing has been measured yet (T-965).
///
/// # Why a pass costs more than its dwells, and why only a measurement can say how much
///
/// [`ScanBudget::pass_ns`] used to be `Σ dwell`, which is the pass length **in capture time** — the
/// sample clock. A step is a **retune**, and a retune stops the sample stream: the front end is
/// reprogrammed, the stream restarted, and on a class or rate boundary the run re-plumbs. None of
/// that advances capture time, so all of it is invisible to a price computed from the dwells, and
/// all of it is paid by the user, on the wall clock, once per step.
///
/// The size of it is not a constant and must not be guessed (T-453's rule: *per-step cost must be
/// measured, not assumed*). Measured on the live HackRF (the explorer's `Scan everything (fast)`
/// pass of 2026-09-25, 418 steps × 0.3 s at 19.2 Msps): the pass was priced at **125.4 s** and
/// took **237 s**, a 268 ms per-step cost the price stated as zero — 1.9× the number the user was
/// shown before committing the radio. `source.source_dropped` rose by 3.5 G samples over that pass,
/// which at 19.2 Msps is 182 s of air the steps never heard: the same quantity, counted a second
/// way.
///
/// So this takes the one measurement that is always available while a pass runs — how long it has
/// been going on the wall clock, and how many steps it has finished — and returns what each step
/// cost over and above its dwell. `None` when no step has finished (there is nothing to divide by),
/// and floored at zero rather than going negative on a step cut short by preemption.
///
/// **`None` is not zero.** A caller with no measurement prices the dwells only and must say the
/// figure is a floor — [`ScanBudget::statement`] does — exactly as `Coverage::Unobserved` is not
/// quiet and `BiasTee::Unknown` is not off.
pub fn measured_step_overhead_ns(wall_ns: i64, steps_done: u64, dwell_ns: i64) -> Option<i64> {
    if steps_done == 0 || wall_ns <= 0 {
        return None;
    }
    let per_step = wall_ns / i64::try_from(steps_done).unwrap_or(i64::MAX);
    Some((per_step - dwell_ns).max(0))
}

/// How far one scan step advances (T-517): the **width of the window the pass is tiled at**, never
/// the resolution inside it.
///
/// # Where the old ~1.5 MHz step came from
///
/// [`CompiledPlan::compile`] tiles a range into slices of `sweep_rate_hz × usable_fraction` (capped
/// at `max_span_hz`). The in-app sweep (T-452) tiles at the sample rate **in force**, because a step
/// is one retune and nothing else — so a live run left at the HackRF's 2 Msps floor (where T-418's
/// narrow selection snaps it) steps `2 Msps × 0.75 = 1.5 MHz`, and ~4 000 steps cover 1 MHz–6 GHz.
/// `usable_fraction = 0.75` and `max_span_hz = 20 MHz` were never the limit; the rate was.
///
/// # Coarse widens the window, and the bins stay the same width
///
/// [`ScanStep::Coarse`] tiles the pass at the widest rate `rate × 2^k` the source supports **at
/// which the caller's bin width is unchanged** ([`coarse_step_rate`]). The pipeline sizes its
/// detection and history FFT as `next_pow2(fs / 5 kHz)`, so a power-of-two multiple of the rate
/// takes the same power-of-two multiple of bins and each bin is exactly as wide as before: coarse
/// means fewer, wider windows at identical frequency resolution, never blurrier data. A step that
/// cannot hold the bin width (a fixed FFT override, a rate list with no such multiple) is not
/// taken — coarse then degenerates to fine rather than degrade.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScanStep {
    /// Today's behaviour: tile at the rate in force, one retune per step and nothing else.
    #[default]
    Fine,
    /// Tile at the widest bin-width-preserving rate: ~10× fewer steps from a 2–2.4 Msps window.
    Coarse,
}

impl ScanStep {
    /// The name on the wire.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fine => "fine",
            Self::Coarse => "coarse",
        }
    }

    /// Parses the wire name.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "fine" => Some(Self::Fine),
            "coarse" => Some(Self::Coarse),
            _ => None,
        }
    }
}

/// The rate a [`ScanStep::Coarse`] pass is tiled at, starting from `rate_hz` (the rate in force).
///
/// The widest `rate_hz × 2^k`, `k ≥ 0`, that `caps` supports and at which `bin_hz` — the caller's
/// frequency resolution at a given rate, e.g. the pipeline's detection/history bin width — is the
/// same as at `rate_hz` (to 1 part in 10⁹). `k = 0` always qualifies, so the answer is never
/// narrower than the rate in force and never a rate that would coarsen a bin.
pub fn coarse_step_rate(
    rate_hz: f64,
    caps: &SourceCapabilities,
    bin_hz: impl Fn(f64) -> f64,
) -> f64 {
    let want = bin_hz(rate_hz);
    let mut best = rate_hz;
    if !(rate_hz.is_finite() && rate_hz > 0.0 && want.is_finite() && want > 0.0) {
        return best;
    }
    for k in 1..=10 {
        let r = rate_hz * f64::from(1u32 << k);
        if !caps.sample_rates.supports(r) {
            continue;
        }
        let b = bin_hz(r);
        if (b - want).abs() <= want * 1e-9 {
            best = r;
        }
    }
    best
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
    /// One whole pass, ns: the dwells **plus** the per-step cost when one has been measured. This
    /// is the **revisit** interval — how long a band waits between looks — and it is what the user
    /// is committing the radio for.
    ///
    /// With no measured [`ScanBudget::step_overhead_ns`] this is `Σ hop.duration_ns` and is a
    /// **floor**, not the answer; [`ScanBudget::statement`] says so in words.
    pub pass_ns: i64,
    /// The listening alone, ns: `Σ hop.duration_ns` over the dwell hops. Unaffected by the
    /// overhead, because a step's retune is not listening.
    pub dwell_total_ns: i64,
    /// Measured per-step cost beyond the dwell, ns — the retune and any re-plumb the step pays
    /// before it can listen (see [`measured_step_overhead_ns`]). `None` when nothing has measured
    /// it for this front end, which is **not** zero.
    pub step_overhead_ns: Option<i64>,
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
            dwell_total_ns: pass_ns,
            step_overhead_ns: None,
            span_hz,
            step_span_hz: plan.usable_span_hz,
        }
    }

    /// The same budget with a **measured** per-step cost priced in, so `pass_ns` and
    /// [`ScanBudget::statement`] state the wall-clock pass the user will wait for rather than the
    /// capture-time one (T-965). See [`measured_step_overhead_ns`].
    ///
    /// A negative or non-finite figure is refused by clamping to zero — this prices a cost, and a
    /// pass cannot be cheaper than its listening.
    pub fn with_step_overhead(self, overhead_ns: i64) -> Self {
        let overhead = overhead_ns.max(0);
        Self {
            pass_ns: self
                .dwell_total_ns
                .saturating_add(overhead.saturating_mul(self.steps as i64)),
            step_overhead_ns: Some(overhead),
            ..self
        }
    }

    /// Whether [`ScanBudget::pass_ns`] prices the per-step cost, or is the dwell-only floor.
    pub fn overhead_measured(&self) -> bool {
        self.step_overhead_ns.is_some()
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
        // T-965: the per-step cost, said out loud — either the measured figure that is already in
        // `pass_ns`, or the admission that the pass length below is only a floor. A price the user
        // commits the radio on must never read as measured when nothing measured it.
        let cost = match self.step_overhead_ns {
            Some(o) => format!(
                " Each step also pays a measured {:.2} s to retune before it can listen, which is \
                 in the pass length above.",
                o as f64 / NS_PER_S,
            ),
            None => " Nothing has measured what a step of this pass pays to retune before it can \
                 listen, so the pass length above is a floor, not the answer: expect longer."
                .to_owned(),
        };
        format!(
            "iterative scan: {} steps × {:.1} s = a {:.1} s pass over {:.3} MHz ({:.3} MHz per step); \
             each band is listened to {:.1} s in every {:.1} s (duty {:.3} %). A step catches \
             anything on the air during its own dwell; it catches nothing during the other {:.1} s, \
             and spectrum the pass has not reached is unobserved, never quiet.{}",
            self.steps,
            self.dwell_ns as f64 / NS_PER_S,
            self.revisit_s(),
            self.span_hz / 1e6,
            self.step_span_hz / 1e6,
            self.dwell_ns as f64 / NS_PER_S,
            self.revisit_s(),
            self.duty() * 100.0,
            (self.pass_ns - self.dwell_ns).max(0) as f64 / NS_PER_S,
            cost,
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

    /// The pipeline's detection/history bin width (`hk_pipeline::detection_resolution`'s rule:
    /// `fft = next_pow2(fs / 5 kHz)` clamped to 512..4096), restated so this crate can test against
    /// it without depending upward. hk-cli's contract test asserts the served `bin_hz` against the
    /// real function.
    fn pipeline_bin_hz(fs: f64) -> f64 {
        let fft = ((fs / 5_000.0).ceil().max(1.0) as usize)
            .next_power_of_two()
            .clamp(512, 4096);
        fs / fft as f64
    }

    /// Full-range step counts, fine vs coarse, compiled the way the in-app sweep compiles them.
    fn full_range_steps(rate_hz: f64) -> usize {
        let caps = SourceCapabilities::hackrf_one();
        let scan = IterativeScan::from_seconds(0.5).unwrap();
        let plan = scan.plan_over_capabilities(&caps, Timestamp::from_unix_nanos(0));
        let mut c = SchedulerConfig::from_plan(&plan).unwrap();
        c.sweep_rate_hz = rate_hz;
        c.max_span_hz = c.max_span_hz.max(rate_hz);
        c.validate(&caps).unwrap();
        let compiled = CompiledPlan::compile(&plan, &c, &caps).unwrap();
        scan.budget(&compiled).steps
    }

    /// T-517: WHERE THE ~1.5 MHz STEP CAME FROM, and what coarse does about it — measured, with
    /// the bin width held identical so "coarse" can never be read as permission to degrade.
    #[test]
    fn coarse_widens_the_window_by_a_power_of_two_and_keeps_the_bin_width() {
        let caps = SourceCapabilities::hackrf_one();
        // The HackRF floor (2 Msps) and the demo's 2.4 Msps: x8 is the widest multiple <= 20 Msps.
        for (fine, coarse) in [(2e6, 16e6), (2.4e6, 19.2e6), (8e6, 16e6), (20e6, 20e6)] {
            let r = coarse_step_rate(fine, &caps, pipeline_bin_hz);
            assert_eq!(r, coarse, "coarse rate from {fine}");
            assert_eq!(
                pipeline_bin_hz(r),
                pipeline_bin_hz(fine),
                "bin width must be identical coarse vs fine at {fine}"
            );
        }
        // A resolution that would coarsen with the rate (a fixed FFT) is never widened.
        assert_eq!(coarse_step_rate(2.4e6, &caps, |fs| fs / 4096.0), 2.4e6);
        // A discrete rate list with no power-of-two multiple stays put.
        let mut d = caps.clone();
        d.sample_rates = SampleRates::Discrete(vec![2.4e6, 10e6]);
        assert_eq!(coarse_step_rate(2.4e6, &d, pipeline_bin_hz), 2.4e6);

        // The measured step counts over the HackRF's whole 1 MHz-6 GHz range.
        assert_eq!(
            full_range_steps(2e6),
            4000,
            "fine at the 2 Msps floor: 1.5 MHz steps"
        );
        assert_eq!(
            full_range_steps(16e6),
            501,
            "coarse from 2 Msps: 12 MHz steps"
        );
        assert_eq!(
            full_range_steps(2.4e6),
            3334,
            "fine at 2.4 Msps: 1.8 MHz steps"
        );
        assert_eq!(
            full_range_steps(19.2e6),
            418,
            "coarse from 2.4 Msps: 14.4 MHz steps"
        );
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

    /// T-965: **a pass costs more than its dwells, and the price says so or says it cannot.**
    ///
    /// The live failure this is the unit half of: `Scan everything (fast)` priced a 418-step pass
    /// at 125.4 s and took 237 s, because the price counted the listening and not the retuning.
    #[test]
    fn the_pass_price_includes_the_measured_per_step_cost_and_admits_when_it_is_unmeasured() {
        let scan = IterativeScan::from_seconds(15.0).unwrap();
        let plan = scan.plan_over(
            "test",
            FreqRange::new(100e6, 200e6),
            Timestamp::from_unix_nanos(0),
        );
        let mut c = cfg();
        scan.apply(&mut c);
        let compiled = CompiledPlan::compile(&plan, &c, &caps()).unwrap();
        let bare = scan.budget(&compiled);

        // Unmeasured: the pass is the dwells, and the statement refuses to present that as the
        // answer. `None` is not zero.
        assert_eq!(bare.step_overhead_ns, None);
        assert!(!bare.overhead_measured());
        assert_eq!(bare.pass_ns, bare.dwell_total_ns);
        assert_eq!(bare.pass_ns, 15_000_000_000 * bare.steps as i64);
        let s = bare.statement();
        assert!(s.contains("a floor, not the answer"), "{s}");

        // Measured: the cost is in the pass length, once per step, and the listening is unchanged.
        let priced = bare.with_step_overhead(2_000_000_000);
        assert_eq!(priced.dwell_total_ns, bare.dwell_total_ns);
        assert_eq!(priced.step_overhead_ns, Some(2_000_000_000));
        assert!(priced.overhead_measured());
        assert_eq!(
            priced.pass_ns,
            bare.pass_ns + 2_000_000_000 * bare.steps as i64
        );
        // And the duty falls, because a retuning band is not a listened-to band.
        assert!(
            priced.duty() < bare.duty(),
            "{} vs {}",
            priced.duty(),
            bare.duty()
        );
        let s = priced.statement();
        assert!(s.contains("measured 2.00 s to retune"), "{s}");
        assert!(!s.contains("a floor, not the answer"), "{s}");

        // A cost cannot be negative: a pass is never cheaper than its listening.
        assert_eq!(bare.with_step_overhead(-5).pass_ns, bare.dwell_total_ns);
    }

    /// T-965: the measurement itself — the live pass's own numbers, and the cases with nothing to
    /// divide by.
    #[test]
    fn the_per_step_cost_is_measured_from_a_pass_that_ran() {
        // The explorer's pass: 418 steps of 0.3 s took 237 s.
        let dwell = 300_000_000;
        let o = measured_step_overhead_ns(237_000_000_000, 418, dwell).unwrap();
        // 237 s / 418 = 567 ms per step, of which 300 ms is the dwell.
        assert!(
            (o - 267_000_000).abs() < 2_000_000,
            "measured {o} ns per step beyond the dwell"
        );
        // Priced back onto the plan it explains the whole overrun, which is the point: the price
        // and the measurement must be the same number.
        let priced_pass = 418 * (dwell + o);
        assert!(
            (priced_pass - 237_000_000_000i64).abs() < 1_000_000_000,
            "{priced_pass} ns priced against 237 s measured"
        );

        // Nothing finished yet, or no wall time: no measurement, and `None` is not zero.
        assert_eq!(measured_step_overhead_ns(10_000_000_000, 0, dwell), None);
        assert_eq!(measured_step_overhead_ns(0, 5, dwell), None);
        // A step cut short by a higher tier never prices a negative cost.
        assert_eq!(measured_step_overhead_ns(1_000_000, 5, dwell), Some(0));
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
