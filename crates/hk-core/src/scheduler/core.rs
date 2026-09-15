//! The v1 attention scheduler: fixed sweep/dwell alternation (ADR-0005 "first version"). See the
//! [module docs](super) for the policy.

use std::sync::Arc;

use hk_model::attention::schedule::{ArmKey, BanditConfig, DwellOutcome};
use hk_model::attention::score::InterestingnessProvider;
use hk_model::{
    FreqRange, ScanPlan, Survey, SurveyId, SurveyState, SurveySummary, TimeRange, Timestamp,
};

use super::bandit::{
    ArmStatus, AttentionStatus, Bandit, BanditKind, Choice, CoverageRing, CoverageVisit, Lease,
    MAX_LEASES, MAX_SCHEDULED, RegionPoi, ScheduledDwell, Share, TierWindow, Undo, arm_key,
    region_poi,
};
use super::clock::{Clock, SyntheticClock};
use super::config::{MAX_GAIN_STEP_PAIRS, SchedulerConfig};
use super::plan::{
    CompiledPlan, Hop, HopKind, PlanError, check_gains, pick_baseband_filter, pick_rate, rf_path,
};
use super::step::{GainSlot, PoiKey, Purpose, ScheduleStep};
use super::survey::SurveyLog;
use crate::source::{Gains, SourceCapabilities};

/// POI scores (`interestingness × region priority`) are clamped to `[MIN_SCORE, MAX_SCORE]` (NaN
/// → `MIN_SCORE`), so an overflowing product cannot starve other POIs.
const MIN_SCORE: f64 = 1e-9;
const MAX_SCORE: f64 = 1e9;

/// A POI's dwell weight is at least this fraction of the strongest queued POI's score, so a tiny
/// weight (priority 0, interestingness 0) still gets about one dwell in 20 of the strongest's.
const MIN_DWELL_SHARE: f64 = 0.05;

/// A retune keeps the emitter at least this far from DC, Hz (beyond its half bandwidth).
const DC_GUARD_HZ: f64 = 10e3;

/// A shrunk retune must move at least the emitter bandwidth plus this, Hz, so an LO-relative
/// product separates from the emitter (twice the hk-detect retune frequency tolerance).
const RETUNE_SEPARATION_HZ: f64 = 20e3;

/// A point of interest to dwell on: a confirmed emitter from detection (T-006/T-007).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Poi {
    /// Caller's key; offering the same key again updates the POI.
    pub key: PoiKey,
    /// Emitter centre, Hz.
    pub center_hz: f64,
    /// Emitter bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// Interestingness (≥ 0). Placeholder for the C12 score; weights the dwell share.
    pub interestingness: f64,
    /// Expected burst interval, ns, if known; the dwell lasts `burst_intervals_per_dwell` of them.
    pub burst_interval_ns: Option<i64>,
    /// Schedule a verification group (gain-step A/B, ±Δ retune, optional rate change) on the
    /// POI's next dwell slot, once.
    pub verify: bool,
}

/// An explicit user request ("watch this"). It preempts everything until it ends or is released.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UserIntent {
    /// Caller's intent id.
    pub id: u64,
    /// Centre, Hz.
    pub center_hz: f64,
    /// Sample rate, Hz.
    pub rate_hz: f64,
    /// Gains; `None` takes the plan's gain table.
    pub gains: Option<Gains>,
    /// Duration, ns; `None` holds until [`Scheduler::release_intent`].
    pub duration_ns: Option<i64>,
}

/// A C37 transmit-slot request. Placeholder only: see "TX exclusivity" in the module docs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TxSlotRequest {
    /// Centre, Hz.
    pub center_hz: f64,
    /// Duration, ns.
    pub duration_ns: i64,
}

/// Why a retune test direction of a POI is not scheduled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetuneSkip {
    /// Retune tests are disabled (`retune_delta_hz` 0).
    Disabled,
    /// The moved centre is outside the source's frequency ranges.
    OutOfRange,
    /// The moved centre is on another RF path (the floor steps; the comparison is invalid).
    RfPathSwitch,
    /// The emitter cannot be both clear of DC and inside the usable span at any centre (it is
    /// wider than about half the usable span).
    NoRoom,
    /// The configured offset puts the emitter across DC and no offset large enough to separate
    /// LO-relative products (emitter bandwidth + 20 kHz, ≥ Δ/4) avoids it.
    CrossesDc,
    /// The configured offset pushes the emitter past the usable edge and no large enough smaller
    /// offset fits.
    PastUsableEdge,
}

/// One retune test direction as planned for a POI.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RetunePlan {
    /// At the configured offset.
    Full {
        /// Offset, Hz.
        delta_hz: f64,
    },
    /// Shrunk so the emitter stays clear of DC and inside the usable span.
    Shrunk {
        /// Offset used, Hz.
        delta_hz: f64,
        /// Configured offset, Hz.
        configured_hz: f64,
    },
    /// Not scheduled.
    Skipped(RetuneSkip),
}

impl RetunePlan {
    /// The scheduled offset, if any.
    pub fn delta_hz(&self) -> Option<f64> {
        match *self {
            RetunePlan::Full { delta_hz } | RetunePlan::Shrunk { delta_hz, .. } => Some(delta_hz),
            RetunePlan::Skipped(_) => None,
        }
    }
}

/// How a POI's dwell and verification fit the window, computed when it is offered or the plan
/// changes ([`Scheduler::poi_notes`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PoiNotes {
    /// The emitter is wider than the dwell's usable span: its edges fall outside the window.
    pub wider_than_span: bool,
    /// The emitter overlaps DC (±10 kHz) in its dwell: it is wider than half the usable span, or
    /// no off-DC centre fits the source range and RF path.
    pub on_dc: bool,
    /// Retune test directions `[+Δ, −Δ]`.
    pub retunes: [RetunePlan; 2],
}

/// Scheduler errors.
#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    /// The plan or settings cannot be scheduled.
    #[error(transparent)]
    Plan(#[from] PlanError),
    /// A request is outside the source's capabilities.
    #[error("{what} {value} is outside the source's capabilities")]
    OutOfCapability {
        /// Quantity.
        what: &'static str,
        /// Rejected value.
        value: f64,
    },
    /// A POI is malformed.
    #[error("invalid point of interest: {0}")]
    InvalidPoi(&'static str),
    /// The POI queue is full and the offered POI scores no higher than the weakest queued one.
    #[error("the POI queue is full")]
    QueueFull,
    /// Transmit is gated (C37) and not implemented.
    #[error("transmit slots are gated (C37) and not implemented")]
    TxGated,
    /// No Survey is open.
    #[error("no survey is open")]
    SurveyNotOpen,
    /// A Survey is already open.
    #[error("a survey is already open")]
    SurveyAlreadyOpen,
    /// A Survey can only end as closed or aborted.
    #[error("a survey can only end as closed or aborted")]
    InvalidSurveyState,
    /// The survey log failed.
    #[error(transparent)]
    Log(#[from] hk_model::RepoError),
    /// The lease or scheduled-dwell table is full.
    #[error("the {0} table is full")]
    TableFull(&'static str),
}

/// Counters since construction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScheduleStats {
    /// Steps emitted.
    pub steps: u64,
    /// Sweep hops.
    pub sweep_steps: u64,
    /// Dwell-only region windows.
    pub region_dwell_steps: u64,
    /// POI dwells (including verification baselines).
    pub dwell_steps: u64,
    /// Gain-step, retune and rate-change steps.
    pub trust_steps: u64,
    /// User-intent steps.
    pub intent_steps: u64,
    /// Verification groups completed.
    pub verifications_completed: u64,
    /// Slots cut by a preemption and rolled back (re-visited later).
    pub truncated_slots: u64,
    /// Bandit dwells (T-120).
    pub bandit_steps: u64,
    /// Pinned-lease slices.
    pub lease_steps: u64,
    /// Scheduled-plan dwells.
    pub scheduled_steps: u64,
    /// Steps above the bandit tier emitted while discovery was below the sweep floor (bandit
    /// enabled): the floor was unmeetable, disclosed rather than hidden.
    pub floor_violations: u64,
}

/// A step minus its sequence number, start time and plan version.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Template {
    duration_ns: i64,
    center_hz: f64,
    rate_hz: f64,
    baseband_filter_hz: Option<f64>,
    gains: Gains,
    gain_entry: Option<u16>,
    accessory: bool,
    rf_path: u8,
    purpose: Purpose,
    verification_group: Option<u64>,
}

impl Template {
    fn from_hop(index: usize, hop: &Hop) -> Self {
        let hop_index = index as u32;
        Self {
            duration_ns: hop.duration_ns,
            center_hz: hop.center_hz,
            rate_hz: hop.rate_hz,
            baseband_filter_hz: hop.baseband_filter_hz,
            gains: hop.gains,
            gain_entry: hop.gain_entry,
            accessory: hop.accessory,
            rf_path: hop.rf_path,
            purpose: match hop.kind {
                HopKind::Sweep => Purpose::Sweep { hop: hop_index },
                HopKind::RegionDwell => Purpose::RegionDwell { hop: hop_index },
            },
            verification_group: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Cursor {
    hop: usize,
    sweeps_in_cycle: u32,
    dwells_in_cycle: u32,
    passes: u64,
}

/// State before the running slot, restored when a preemption cuts it.
#[derive(Clone, Copy, Debug)]
struct Snapshot {
    cursor: Cursor,
    served: Option<(PoiKey, u64)>,
    verification: bool,
    bandit: Option<Undo>,
    scheduled: Option<(u32, i64, bool)>,
}

const MAX_STAGES: usize = 2 * MAX_GAIN_STEP_PAIRS as usize + 4;

#[derive(Clone, Copy, Debug)]
struct VerifyState {
    key: PoiKey,
    stages: [Option<Template>; MAX_STAGES],
    next: usize,
    bandit: bool,
}

impl VerifyState {
    fn remaining(&self) -> bool {
        self.stages[self.next..].iter().any(Option::is_some)
    }
}

#[derive(Clone, Copy, Debug)]
struct ActiveIntent {
    intent: UserIntent,
    until: Option<Timestamp>,
}

#[derive(Clone, Copy, Debug)]
struct ActiveLease {
    lease: Lease,
    until: Option<Timestamp>,
}

#[derive(Clone, Copy, Debug)]
struct ScheduledEntry {
    dwell: ScheduledDwell,
    due_ns: i64,
    done: bool,
}

/// Coverage ring capacity (recent windows kept for in-memory POI accounting).
const COVERAGE_VISITS: usize = 16_384;

/// Sweep floor under the low-power profile (ADR-0012 §5.8).
const LOW_POWER_SWEEP_FLOOR: f64 = 0.5;

fn tier_window(cfg: &BanditConfig) -> TierWindow {
    let window_ns = (cfg.sweep_floor_window_s * 1e9) as i64;
    TierWindow::new(
        window_ns,
        (cfg.max_dwell_s.max(60.0) * 1e9) as i64,
        window_ns / 60,
    )
}

#[derive(Clone, Copy, Debug)]
struct PoiEntry {
    poi: Poi,
    dwell: Template,
    notes: PoiNotes,
    score: f64,
    served: u64,
    verified: bool,
    order: u64,
}

#[derive(Clone, Debug)]
struct OpenSurvey {
    id: SurveyId,
    device_id: String,
}

/// The v1 attention scheduler. Deterministic for a given plan, settings, capabilities, clock
/// readings and call sequence; allocation-free per step.
pub struct Scheduler<C: Clock> {
    clock: C,
    caps: SourceCapabilities,
    cfg: SchedulerConfig,
    plan: CompiledPlan,
    seq: u64,
    cursor: Cursor,
    pois: Vec<PoiEntry>,
    next_order: u64,
    verify: Option<VerifyState>,
    intent: Option<ActiveIntent>,
    slot: Option<Snapshot>,
    current_end: Timestamp,
    last_now: Timestamp,
    stats: ScheduleStats,
    survey: Option<OpenSurvey>,
    bandit: Option<Box<Bandit>>,
    leases: Vec<ActiveLease>,
    lease_rr: usize,
    scheduled: Vec<ScheduledEntry>,
    tiers: TierWindow,
    current_share: Share,
    current_visits: usize,
    coverage: CoverageRing,
    low_power: bool,
}

impl<C: Clock> Scheduler<C> {
    /// Compiles `plan` for `caps` under `cfg`.
    pub fn new(
        plan: &ScanPlan,
        cfg: SchedulerConfig,
        caps: &SourceCapabilities,
        clock: C,
    ) -> Result<Self, SchedulerError> {
        cfg.validate(caps)?;
        let compiled = CompiledPlan::compile(plan, &cfg, caps)?;
        let now = clock.now();
        Ok(Self {
            pois: Vec::with_capacity(cfg.max_pois),
            clock,
            caps: caps.clone(),
            cfg,
            plan: compiled,
            seq: 0,
            cursor: Cursor::default(),
            next_order: 0,
            verify: None,
            intent: None,
            slot: None,
            current_end: now,
            last_now: now,
            stats: ScheduleStats::default(),
            survey: None,
            bandit: None,
            leases: Vec::with_capacity(MAX_LEASES),
            lease_rr: 0,
            scheduled: Vec::with_capacity(MAX_SCHEDULED),
            tiers: tier_window(&BanditConfig::default()),
            current_share: Share::Other,
            current_visits: 0,
            coverage: CoverageRing::new(COVERAGE_VISITS),
            low_power: false,
        })
    }

    /// The compiled plan.
    pub fn plan(&self) -> &CompiledPlan {
        &self.plan
    }

    /// The settings.
    pub fn config(&self) -> &SchedulerConfig {
        &self.cfg
    }

    /// The clock.
    pub fn clock(&self) -> &C {
        &self.clock
    }

    /// Counters.
    pub fn stats(&self) -> ScheduleStats {
        self.stats
    }

    /// Complete discovery passes so far (across plan updates).
    pub fn passes_completed(&self) -> u64 {
        self.cursor.passes
    }

    /// The open Survey, if any.
    pub fn survey_id(&self) -> Option<SurveyId> {
        self.survey.as_ref().map(|s| s.id)
    }

    /// Queued POIs.
    pub fn pois(&self) -> impl Iterator<Item = &Poi> {
        self.pois.iter().map(|e| &e.poi)
    }

    /// Whether a queued POI finished verification.
    pub fn is_verified(&self, key: PoiKey) -> Option<bool> {
        self.pois
            .iter()
            .find(|e| e.poi.key == key)
            .map(|e| e.verified)
    }

    /// How a queued POI fits its dwell window and which retune tests it gets (and why not).
    pub fn poi_notes(&self, key: PoiKey) -> Option<PoiNotes> {
        self.pois.iter().find(|e| e.poi.key == key).map(|e| e.notes)
    }

    /// The clock reading, never earlier than a previous one (a clock that steps backwards cannot
    /// reorder step start times; use a monotonic clock so cuts stay correct too).
    fn now(&mut self) -> Timestamp {
        let now = self.clock.now().max(self.last_now);
        self.last_now = now;
        now
    }

    /// The next step, starting now. Order (ADR-0012 §5.4): user intent, pinned leases, a running
    /// verification group, due scheduled-plan dwells, then the sweep/dwell cycle (bandit or WRR
    /// dwell slots and discovery).
    pub fn next_step(&mut self) -> ScheduleStep {
        let now = self.now();
        let now_ns = now.as_unix_nanos();
        if self
            .intent
            .is_some_and(|a| a.until.is_some_and(|until| now >= until))
        {
            self.intent = None;
        }
        self.leases
            .retain(|l| l.until.is_none_or(|until| now < until));
        let t = if let Some(active) = self.intent {
            self.slot = None;
            self.intent_template(now, &active)
        } else if !self.leases.is_empty() {
            self.slot = None;
            self.lease_template(now)
        } else if self.verify.is_some() {
            self.verify_template()
        } else if let Some(i) = self.due_scheduled(now_ns) {
            self.scheduled_template(i, now_ns)
        } else {
            self.slot_template()
        };
        let step = ScheduleStep {
            seq: self.seq,
            t_start: now,
            duration_ns: t.duration_ns,
            center_hz: t.center_hz,
            rate_hz: t.rate_hz,
            baseband_filter_hz: t.baseband_filter_hz,
            gains: t.gains,
            gain_entry: t.gain_entry,
            accessory: t.accessory,
            rf_path: t.rf_path,
            purpose: t.purpose,
            plan_version: self.plan.plan_version,
            verification_group: t.verification_group,
        };
        self.seq += 1;
        self.current_end = step.t_end();
        let share = match step.purpose {
            p if p.is_discovery() => Share::Discovery,
            Purpose::Bandit {
                kind: BanditKind::Explore,
                ..
            } => Share::Explore,
            Purpose::UserIntent { .. } | Purpose::Lease { .. } | Purpose::Scheduled { .. } => {
                Share::Other
            }
            _ => Share::Exploit,
        };
        // Interactive intent holds the radio by right (§5.3): only leases and scheduled plans
        // below the floor are violations.
        let interactive = matches!(step.purpose, Purpose::UserIntent { .. });
        if share == Share::Other && !interactive && !self.sweep_floor_met(now_ns) {
            self.stats.floor_violations += 1;
        }
        self.tiers.add(now_ns, step.duration_ns, share, 1);
        self.current_share = share;
        self.record_coverage(&step);
        let stats = &mut self.stats;
        stats.steps += 1;
        match step.purpose {
            Purpose::Sweep { .. } => stats.sweep_steps += 1,
            Purpose::RegionDwell { .. } => stats.region_dwell_steps += 1,
            Purpose::Dwell { .. } => stats.dwell_steps += 1,
            Purpose::GainStep { .. } | Purpose::Retune { .. } | Purpose::RateChange { .. } => {
                stats.trust_steps += 1;
            }
            Purpose::UserIntent { .. } => stats.intent_steps += 1,
            Purpose::Bandit { .. } => stats.bandit_steps += 1,
            Purpose::Lease { .. } => stats.lease_steps += 1,
            Purpose::Scheduled { .. } => stats.scheduled_steps += 1,
        }
        step
    }

    /// Covered extents of `step` (usable span; non-discovery windows minus the DC guard) into
    /// the coverage ring.
    fn record_coverage(&mut self, step: &ScheduleStep) {
        let usable = (step.rate_hz * self.cfg.usable_fraction).min(self.cfg.max_span_hz);
        let (lo, hi) = (step.center_hz - usable / 2.0, step.center_hz + usable / 2.0);
        let time = TimeRange::new(step.t_start, step.t_end());
        let guard = super::bandit::DC_GUARD_HZ;
        if step.purpose.is_discovery() {
            self.coverage.push(CoverageVisit {
                covered: FreqRange::new(lo, hi),
                time,
            });
            self.current_visits = 1;
        } else {
            for covered in [
                FreqRange::new(lo, step.center_hz - guard),
                FreqRange::new(step.center_hz + guard, hi),
            ] {
                self.coverage.push(CoverageVisit { covered, time });
            }
            self.current_visits = 2;
        }
    }

    fn sweep_floor(&self) -> Option<f64> {
        let floor = self.bandit.as_ref()?.cfg.sweep_floor;
        Some(if self.low_power {
            floor.max(LOW_POWER_SWEEP_FLOOR)
        } else {
            floor
        })
    }

    fn sweep_floor_met(&self, now_ns: i64) -> bool {
        let Some(floor) = self.sweep_floor() else {
            return true;
        };
        let t = self.tiers.totals(now_ns);
        let total: i64 = t.iter().sum();
        total == 0 || t[Share::Discovery as usize] as f64 >= floor * total as f64
    }

    /// Re-packs the bandit's arm table if the provider published a new snapshot version since the
    /// last packing (ADR-0012 §5.1). Packing allocates, so it is **not** done in
    /// [`Scheduler::next_step`]: the owner calls this at its decision boundaries (the pipeline's
    /// control loop before each step, the simulator before each decision). Returns whether it
    /// re-packed. Without the bandit, or with no new version, it is a cheap version compare.
    pub fn refresh_bandit(&mut self) -> bool {
        let now_ns = self.now().as_unix_nanos();
        match self.bandit.as_mut() {
            Some(b) if b.stale() => {
                b.repack(&self.plan, &self.cfg, &self.caps, now_ns);
                true
            }
            _ => false,
        }
    }

    /// Enables the bandit revisit policy (ADR-0012 §5): dwell slots are chosen by the bandit over
    /// `provider`'s candidate snapshots instead of WRR over offered POIs, under the exploration
    /// and sweep floors. Arms are packed at the next [`Scheduler::refresh_bandit`].
    pub fn enable_bandit(
        &mut self,
        cfg: BanditConfig,
        provider: Arc<dyn InterestingnessProvider>,
    ) -> Result<(), SchedulerError> {
        cfg.validate()
            .map_err(|e| PlanError::InvalidConfig(format!("bandit: {e}")))?;
        self.tiers = tier_window(&cfg);
        self.bandit = Some(Box::new(Bandit::new(cfg, provider)));
        Ok(())
    }

    /// The bandit is enabled.
    pub fn bandit_enabled(&self) -> bool {
        self.bandit.is_some()
    }

    /// The arm key of a step's window (the `arm` of its [`DwellOutcome`]).
    pub fn arm_key_of(&self, step: &ScheduleStep) -> ArmKey {
        let q = self.bandit.as_ref().map_or(1e6, |b| b.cfg.arm_quantum_hz);
        arm_key(step.rf_path, step.center_hz, step.rate_hz, q)
    }

    /// Feeds a processed dwell's outcome (detection, C12 and decoders done) to the bandit.
    /// Returns whether its arm is known. Allocation-free.
    pub fn record_outcome(&mut self, outcome: &DwellOutcome) -> bool {
        let now_ns = self.now().as_unix_nanos();
        self.bandit
            .as_mut()
            .is_some_and(|b| b.record_outcome(outcome, now_ns))
    }

    /// A trust-test verdict for a suspect candidate's verification group (C05): pass lets it be
    /// packed, fail bans it for `suspect_ban_s`. Returns whether the candidate was known.
    pub fn report_verification(&mut self, key: PoiKey, passed: bool) -> bool {
        let now_ns = self.now().as_unix_nanos();
        self.bandit
            .as_mut()
            .is_some_and(|b| b.report_verification(key, passed, now_ns))
    }

    /// Adds or updates a pinned lease. It preempts scheduled plans, the bandit and the sweep from
    /// the next step (a running slot is cut and rolled back, as for intent).
    pub fn add_lease(&mut self, lease: Lease) -> Result<(), SchedulerError> {
        self.check_window(
            "lease",
            lease.center_hz,
            lease.rate_hz,
            lease.gains.as_ref(),
        )?;
        if let Some(ns) = lease.duration_ns.filter(|&ns| ns <= 0) {
            return Err(SchedulerError::OutOfCapability {
                what: "lease duration (ns)",
                value: ns as f64,
            });
        }
        let now = self.now();
        let entry = ActiveLease {
            lease,
            until: lease.duration_ns.map(|ns| now.saturating_add_nanos(ns)),
        };
        if let Some(i) = self.leases.iter().position(|l| l.lease.id == lease.id) {
            // An update that changes the lease (or ends it sooner) trims a running lease step,
            // so the caller takes the next step with the new settings (T-127 review). A same
            // update (a renewal that does not shorten it) leaves the running step alone.
            let old = self.leases[i];
            let shortens = entry
                .until
                .is_some_and(|u| old.until.is_none_or(|o| u < o) && u < self.current_end);
            if self.intent.is_none() && (old.lease != lease || shortens) {
                self.trim_running(now);
            }
            self.leases[i] = entry;
            return Ok(());
        }
        if self.leases.len() >= MAX_LEASES {
            return Err(SchedulerError::TableFull("lease"));
        }
        self.cut(now);
        self.leases.push(entry);
        Ok(())
    }

    /// Releases a lease. Returns whether it was active.
    pub fn release_lease(&mut self, id: u64) -> bool {
        match self.leases.iter().position(|l| l.lease.id == id) {
            Some(i) => {
                self.leases.remove(i);
                let now = self.now();
                self.trim_running(now);
                true
            }
            None => false,
        }
    }

    /// The running step's end: its planned end, or earlier once a lease/intent change cut or
    /// trimmed it. A caller compares it across [`Self::add_lease`]/[`Self::release_lease`] to
    /// know whether the running step was abandoned (T-127 review).
    pub fn running_end(&self) -> Timestamp {
        self.current_end
    }

    /// Active leases.
    pub fn leases(&self) -> impl Iterator<Item = &Lease> {
        self.leases.iter().map(|l| &l.lease)
    }

    /// Adds or replaces a scheduled-plan dwell.
    pub fn schedule_dwell(&mut self, dwell: ScheduledDwell) -> Result<(), SchedulerError> {
        self.check_window(
            "scheduled dwell",
            dwell.center_hz,
            dwell.rate_hz,
            dwell.gains.as_ref(),
        )?;
        if dwell.duration_ns <= 0 || dwell.every_ns.is_some_and(|ns| ns <= 0) {
            return Err(SchedulerError::OutOfCapability {
                what: "scheduled dwell duration or period (ns)",
                value: dwell.duration_ns as f64,
            });
        }
        let entry = ScheduledEntry {
            dwell,
            due_ns: dwell.due.as_unix_nanos(),
            done: false,
        };
        self.scheduled.retain(|e| !e.done || e.dwell.id == dwell.id);
        if let Some(e) = self.scheduled.iter_mut().find(|e| e.dwell.id == dwell.id) {
            *e = entry;
        } else if self.scheduled.len() >= MAX_SCHEDULED {
            return Err(SchedulerError::TableFull("scheduled dwell"));
        } else {
            self.scheduled.push(entry);
        }
        Ok(())
    }

    /// Cancels a scheduled dwell. Returns whether it was pending.
    pub fn cancel_scheduled(&mut self, id: u32) -> bool {
        let before = self.scheduled.iter().filter(|e| !e.done).count();
        self.scheduled.retain(|e| e.dwell.id != id);
        self.scheduled.iter().filter(|e| !e.done).count() < before
    }

    /// Low-power profile (ADR-0012 §5.8): half the dwell slots per cycle and a sweep floor of at
    /// least 50 % (sweeping needs no demodulation or classification).
    pub fn set_low_power(&mut self, on: bool) {
        self.low_power = on;
    }

    /// Tier shares over the sweep-floor window, floor status and the bandit summary.
    pub fn attention_status(&self) -> AttentionStatus {
        let now = self.last_now;
        let now_ns = now.as_unix_nanos();
        let t = self.tiers.totals(now_ns);
        let s = |share: Share| t[share as usize] as f64 / 1e9;
        AttentionStatus {
            now,
            window_s: self.tiers.window_ns() as f64 / 1e9,
            discovery_s: s(Share::Discovery),
            exploit_s: s(Share::Exploit),
            explore_s: s(Share::Explore),
            other_s: s(Share::Other),
            sweep_floor: self.sweep_floor(),
            sweep_floor_met: self.sweep_floor_met(now_ns),
            floor_violations: self.stats.floor_violations,
            interactive: self.intent.is_some(),
            leases: self.leases.len(),
            scheduled: self.scheduled.iter().filter(|e| !e.done).count(),
            low_power: self.low_power,
            bandit: self.bandit.as_ref().map(|b| b.status(now_ns)),
        }
    }

    /// The bandit's arm table (empty without the bandit).
    pub fn arm_table(&self) -> Vec<ArmStatus> {
        self.bandit
            .as_ref()
            .map_or_else(Vec::new, |b| b.arm_table(self.last_now.as_unix_nanos()))
    }

    /// Recent planned windows (cut-corrected), oldest first: the in-memory coverage source.
    pub fn coverage_visits(&self) -> Vec<CoverageVisit> {
        self.coverage.to_vec()
    }

    /// Exact POI and coverage gaps of `region` over `span` from the recent windows (ADR-0012
    /// §5.5; 1 MHz cells, gaps longer than twice the measured revisit).
    pub fn region_poi(
        &self,
        region: FreqRange,
        span: TimeRange,
        taus_s: &[f64],
        rate_hz: Option<f64>,
    ) -> RegionPoi {
        region_poi(
            &self.coverage.to_vec(),
            region,
            span,
            1e6,
            taus_s,
            rate_hz,
            None,
        )
    }

    fn check_window(
        &self,
        what: &'static str,
        center_hz: f64,
        rate_hz: f64,
        gains: Option<&Gains>,
    ) -> Result<(), SchedulerError> {
        if !(center_hz.is_finite() && self.caps.supports_frequency(center_hz)) {
            return Err(SchedulerError::OutOfCapability {
                what,
                value: center_hz,
            });
        }
        if !(rate_hz.is_finite() && self.caps.sample_rates.supports(rate_hz)) {
            return Err(SchedulerError::OutOfCapability {
                what,
                value: rate_hz,
            });
        }
        if let Some(g) = gains {
            check_gains(&self.caps, g).map_err(|reason| PlanError::InvalidGain {
                index: None,
                reason,
            })?;
        }
        Ok(())
    }

    fn window_template(
        &self,
        center_hz: f64,
        rate_hz: f64,
        gains: Option<Gains>,
        duration_ns: i64,
        purpose: Purpose,
    ) -> Template {
        let (table_gains, gain_entry, accessory) = self.plan.gains_at(center_hz);
        Template {
            duration_ns,
            center_hz,
            rate_hz,
            baseband_filter_hz: pick_baseband_filter(&self.caps, rate_hz),
            gains: gains.unwrap_or(table_gains),
            gain_entry,
            accessory,
            rf_path: rf_path(self.cfg.rf_path_boundaries(&self.caps), center_hz),
            purpose,
            verification_group: None,
        }
    }

    /// The next lease slice, round robin over active leases.
    fn lease_template(&mut self, now: Timestamp) -> Template {
        self.lease_rr = (self.lease_rr + 1) % self.leases.len();
        let l = self.leases[self.lease_rr];
        let slice = self.cfg.intent_slice_ns;
        let duration_ns = l.until.map_or(slice, |until| {
            (until.as_unix_nanos() - now.as_unix_nanos()).clamp(1, slice)
        });
        self.window_template(
            l.lease.center_hz,
            l.lease.rate_hz,
            l.lease.gains,
            duration_ns,
            Purpose::Lease {
                kind: l.lease.kind,
                lease: l.lease.id,
            },
        )
    }

    fn due_scheduled(&self, now_ns: i64) -> Option<usize> {
        self.scheduled
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.done && e.due_ns <= now_ns)
            .min_by_key(|(_, e)| (e.due_ns, e.dwell.id))
            .map(|(i, _)| i)
    }

    fn scheduled_template(&mut self, i: usize, now_ns: i64) -> Template {
        let e = self.scheduled[i];
        self.slot = Some(Snapshot {
            cursor: self.cursor,
            served: None,
            verification: false,
            bandit: None,
            scheduled: Some((e.dwell.id, e.due_ns, e.done)),
        });
        let entry = &mut self.scheduled[i];
        match e.dwell.every_ns {
            Some(every) => {
                while entry.due_ns <= now_ns {
                    entry.due_ns = entry.due_ns.saturating_add(every);
                }
            }
            None => entry.done = true,
        }
        let d = e.dwell;
        self.window_template(
            d.center_hz,
            d.rate_hz,
            d.gains,
            d.duration_ns,
            Purpose::Scheduled { target: d.id },
        )
    }

    /// A bandit dwell slot: the bandit's choice unless it would push discovery below the sweep
    /// floor (then `None`: the cycle sweeps on).
    fn bandit_slot(&mut self) -> Option<Template> {
        let now_ns = self.last_now.as_unix_nanos();
        let shares = self.tiers.totals(now_ns);
        let floor = self.sweep_floor()?;
        let total: i64 = shares.iter().sum();
        let discovery = shares[Share::Discovery as usize] as f64;
        if discovery < floor * (total + self.cfg.dwell_min_ns) as f64 {
            if let Some(b) = self.bandit.as_mut() {
                b.counters.floor_deferrals += 1;
            }
            return None;
        }
        let choice = self.bandit.as_ref()?.pick(now_ns, &shares)?;
        let (template, verify) = match choice {
            Choice::Verify {
                key,
                center_hz,
                bandwidth_hz,
            } => {
                let poi = Poi {
                    key,
                    center_hz,
                    bandwidth_hz,
                    interestingness: 1.0,
                    burst_interval_ns: None,
                    verify: true,
                };
                let base = dwell_template(&self.plan, &self.cfg, &self.caps, &poi);
                let notes = poi_notes(&self.cfg, &self.caps, &poi, &base);
                (None, Some(self.verification_for(&poi, base, notes, true)))
            }
            Choice::Dwell { arm, kind } => {
                let a = self.bandit.as_ref()?.arm_view(arm);
                let duration_ns = a
                    .dwell_ns
                    .min(self.plan.dwell_cap_ns)
                    .max(self.cfg.dwell_min_ns);
                let t = self.window_template(
                    a.center_hz,
                    a.rate_hz,
                    None,
                    duration_ns,
                    Purpose::Bandit {
                        arm: arm as u32,
                        lead: a.lead,
                        kind,
                    },
                );
                (Some(t), None)
            }
        };
        let duration_ns = match (&template, &verify) {
            (Some(t), _) => t.duration_ns,
            (None, Some(v)) => v.stages.iter().flatten().map(|s| s.duration_ns).sum(),
            (None, None) => return None,
        };
        if discovery < floor * (total + duration_ns) as f64 {
            if let Some(b) = self.bandit.as_mut() {
                b.counters.floor_deferrals += 1;
            }
            return None;
        }
        let before = self.cursor;
        self.cursor.dwells_in_cycle += 1;
        let undo = self.bandit.as_mut()?.commit(choice, now_ns);
        self.slot = Some(Snapshot {
            cursor: before,
            served: None,
            verification: false,
            bandit: Some(undo),
            scheduled: None,
        });
        match (template, verify) {
            (Some(t), _) => Some(t),
            (None, v) => {
                self.verify = v;
                Some(self.verify_template())
            }
        }
    }

    /// Queues or updates a POI. Its dwell is sized now: rate ≥ `dwell_min_rate_hz` and wide
    /// enough that the emitter fits half the usable span, offset-tuned by a quarter span so the
    /// emitter clears DC, duration from its burst interval (else the default), clamped to the
    /// plan's dwell cap. Emitters too wide for that are noted ([`Scheduler::poi_notes`]). A full
    /// queue evicts the lowest-scoring POI if the new one scores higher.
    pub fn offer_poi(&mut self, poi: Poi) -> Result<(), SchedulerError> {
        self.check_poi(&poi)?;
        let dwell = dwell_template(&self.plan, &self.cfg, &self.caps, &poi);
        let notes = poi_notes(&self.cfg, &self.caps, &poi, &dwell);
        let score = self.score(&poi);
        if let Some(e) = self.pois.iter_mut().find(|e| e.poi.key == poi.key) {
            e.poi = poi;
            e.dwell = dwell;
            e.notes = notes;
            e.score = score;
            return Ok(());
        }
        // Start a new POI level with the least-served one, so it does not monopolise dwells.
        let floor = self.pois.iter().map(|e| e.score).fold(score, f64::max) * MIN_DWELL_SHARE;
        let min_ratio = self
            .pois
            .iter()
            .map(|e| e.served as f64 / e.score.max(floor))
            .fold(f64::INFINITY, f64::min);
        let served = if min_ratio.is_finite() {
            (min_ratio * score.max(floor)).floor() as u64
        } else {
            0
        };
        let entry = PoiEntry {
            poi,
            dwell,
            notes,
            score,
            served,
            verified: false,
            order: self.next_order,
        };
        if self.pois.len() < self.cfg.max_pois {
            self.pois.push(entry);
        } else {
            let weakest = self
                .pois
                .iter()
                .enumerate()
                .min_by(|a, b| {
                    a.1.score
                        .total_cmp(&b.1.score)
                        .then(b.1.order.cmp(&a.1.order))
                })
                .map(|(i, _)| i);
            match weakest {
                Some(i) if self.pois[i].score < score => self.pois[i] = entry,
                _ => return Err(SchedulerError::QueueFull),
            }
        }
        self.next_order += 1;
        Ok(())
    }

    /// Removes a POI. A verification group already running for it completes.
    pub fn remove_poi(&mut self, key: PoiKey) -> bool {
        match self.pois.iter().position(|e| e.poi.key == key) {
            Some(i) => {
                self.pois.swap_remove(i);
                true
            }
            None => false,
        }
    }

    /// Preempts with explicit user intent, effective from the next [`Scheduler::next_step`]
    /// (the executor should cut the running step). A cut discovery hop or dwell is rolled back
    /// and re-visited later; a cut verification group restarts from its first stage under a new
    /// verification group id.
    pub fn preempt(&mut self, intent: UserIntent) -> Result<(), SchedulerError> {
        if !(intent.center_hz.is_finite() && self.caps.supports_frequency(intent.center_hz)) {
            return Err(SchedulerError::OutOfCapability {
                what: "intent centre (Hz)",
                value: intent.center_hz,
            });
        }
        if !(intent.rate_hz.is_finite() && self.caps.sample_rates.supports(intent.rate_hz)) {
            return Err(SchedulerError::OutOfCapability {
                what: "intent sample rate (Hz)",
                value: intent.rate_hz,
            });
        }
        if let Some(ns) = intent.duration_ns.filter(|&ns| ns <= 0) {
            return Err(SchedulerError::OutOfCapability {
                what: "intent duration (ns)",
                value: ns as f64,
            });
        }
        if let Some(g) = &intent.gains {
            check_gains(&self.caps, g).map_err(|reason| PlanError::InvalidGain {
                index: None,
                reason,
            })?;
        }
        let now = self.now();
        self.cut(now);
        self.intent = Some(ActiveIntent {
            intent,
            until: intent.duration_ns.map(|ns| now.saturating_add_nanos(ns)),
        });
        Ok(())
    }

    /// Ends a user intent early. Returns whether one was active.
    pub fn release_intent(&mut self) -> bool {
        let had = self.intent.take().is_some();
        if had {
            let now = self.now();
            self.trim_running(now);
        }
        had
    }

    /// TX-exclusivity placeholder: always [`SchedulerError::TxGated`]. See the module docs.
    pub fn request_tx_slot(&mut self, _request: &TxSlotRequest) -> Result<(), SchedulerError> {
        Err(SchedulerError::TxGated)
    }

    /// Opens a Survey under the running plan version.
    pub fn open_survey(
        &mut self,
        log: &mut dyn SurveyLog,
        device_id: &str,
    ) -> Result<SurveyId, SchedulerError> {
        if self.survey.is_some() {
            return Err(SchedulerError::SurveyAlreadyOpen);
        }
        let survey = Survey {
            id: SurveyId::new(),
            plan_id: self.plan.plan_id,
            plan_version: self.plan.plan_version,
            device_id: device_id.into(),
            state: SurveyState::Open,
            t_start: self.now(),
            t_end: None,
            summary: None,
        };
        log.open(&survey)?;
        self.survey = Some(OpenSurvey {
            id: survey.id,
            device_id: survey.device_id,
        });
        Ok(survey.id)
    }

    /// Ends the open Survey as `closed` or `aborted`.
    pub fn close_survey(
        &mut self,
        log: &mut dyn SurveyLog,
        state: SurveyState,
        summary: &SurveySummary,
    ) -> Result<SurveyId, SchedulerError> {
        if state == SurveyState::Open {
            return Err(SchedulerError::InvalidSurveyState);
        }
        let id = self
            .survey
            .as_ref()
            .ok_or(SchedulerError::SurveyNotOpen)?
            .id;
        let now = self.now();
        log.close(id, state, now, summary)?;
        self.survey = None;
        Ok(id)
    }

    /// Switches to a new plan version (or another plan). The new plan is compiled first; on
    /// error the running plan is kept. If a Survey is open it is closed with `summary` and a new
    /// one opens under the new version (returned). The discovery pass resumes at the equivalent
    /// position of the new plan (the hop covering the next unvisited frequency), so frequent
    /// updates still reach the tail of the pass; queued POIs are kept and re-sized; a running
    /// verification group is dropped (it restarts later under a new id); a user intent stays in
    /// force.
    pub fn update_plan(
        &mut self,
        plan: &ScanPlan,
        cfg: SchedulerConfig,
        log: &mut dyn SurveyLog,
        summary: &SurveySummary,
    ) -> Result<Option<SurveyId>, SchedulerError> {
        if plan.id == self.plan.plan_id && plan.version <= self.plan.plan_version {
            return Err(PlanError::StaleVersion {
                id: plan.id,
                version: plan.version,
                running: self.plan.plan_version,
            }
            .into());
        }
        cfg.validate(&self.caps)?;
        let compiled = CompiledPlan::compile(plan, &cfg, &self.caps)?;
        let device = match &self.survey {
            Some(open) => {
                let id = open.id;
                let device = open.device_id.clone();
                let now = self.now();
                log.close(id, SurveyState::Closed, now, summary)?;
                self.survey = None;
                Some(device)
            }
            None => None,
        };
        if let Some(open) = self.verify.take() {
            if open.bandit {
                if let Some(b) = self.bandit.as_mut() {
                    b.undo(Undo::Verify { key: open.key });
                }
            } else if let Some(e) = self.pois.iter_mut().find(|e| e.poi.key == open.key) {
                e.verified = false;
            }
        }
        if let Some(b) = self.bandit.as_mut() {
            b.invalidate();
        }
        let hop = self.resume_hop(&compiled);
        self.cfg = cfg;
        self.plan = compiled;
        self.cursor.hop = hop;
        self.slot = None;
        if self.pois.capacity() < self.cfg.max_pois {
            self.pois.reserve(self.cfg.max_pois - self.pois.len());
        }
        for i in 0..self.pois.len() {
            let poi = self.pois[i].poi;
            let dwell = dwell_template(&self.plan, &self.cfg, &self.caps, &poi);
            self.pois[i].notes = poi_notes(&self.cfg, &self.caps, &poi, &dwell);
            self.pois[i].dwell = dwell;
            self.pois[i].score = self.score(&poi);
        }
        match device {
            Some(device) => Ok(Some(self.open_survey(log, &device)?)),
            None => Ok(None),
        }
    }

    /// The hop of `new` equivalent to the running pass position: the hop (of the same kind)
    /// covering the next old hop's lower edge, else the first hop after it in pass order
    /// (priority descending, then frequency), else the pass start.
    fn resume_hop(&self, new: &CompiledPlan) -> usize {
        if self.cursor.hop == 0 {
            return 0;
        }
        let Some(old) = self.plan.hops.get(self.cursor.hop) else {
            return 0;
        };
        let f = old.covers.lo_hz;
        new.hops
            .iter()
            .position(|h| h.kind == old.kind && h.covers.lo_hz <= f && f < h.covers.hi_hz)
            .or_else(|| {
                new.hops.iter().position(|h| {
                    h.priority < old.priority || (h.priority == old.priority && h.covers.lo_hz >= f)
                })
            })
            .unwrap_or(0)
    }

    fn check_poi(&self, poi: &Poi) -> Result<(), SchedulerError> {
        if !(poi.center_hz.is_finite() && poi.bandwidth_hz.is_finite() && poi.bandwidth_hz >= 0.0) {
            return Err(SchedulerError::InvalidPoi(
                "centre and bandwidth must be finite, bandwidth >= 0",
            ));
        }
        if !(poi.interestingness.is_finite() && poi.interestingness >= 0.0) {
            return Err(SchedulerError::InvalidPoi(
                "interestingness must be finite and >= 0",
            ));
        }
        if poi.burst_interval_ns.is_some_and(|ns| ns <= 0) {
            return Err(SchedulerError::InvalidPoi("burst interval must be > 0"));
        }
        if !self.caps.supports_frequency(poi.center_hz) {
            return Err(SchedulerError::OutOfCapability {
                what: "POI centre (Hz)",
                value: poi.center_hz,
            });
        }
        Ok(())
    }

    fn score(&self, poi: &Poi) -> f64 {
        let priority = self.plan.region_priority_at(poi.center_hz).unwrap_or(1.0);
        let score = poi.interestingness * priority;
        if score.is_nan() {
            MIN_SCORE
        } else {
            score.clamp(MIN_SCORE, MAX_SCORE)
        }
    }

    /// Ends the running step at `now`: its unrun planned time leaves the sweep-floor window and
    /// its coverage is cut (a released lease or intent, T-127; the first half of [`Self::cut`]).
    fn trim_running(&mut self, now: Timestamp) {
        if now < self.current_end {
            let now_ns = now.as_unix_nanos();
            let rest = self.current_end.as_unix_nanos() - now_ns;
            self.tiers.add(now_ns, rest, self.current_share, -1);
            self.coverage.cut_last(self.current_visits, now);
        }
        self.current_end = self.current_end.min(now);
    }

    /// Rolls back the running slot if `now` is inside it or a verification group is unfinished.
    fn cut(&mut self, now: Timestamp) {
        let running = now < self.current_end;
        self.trim_running(now);
        if running || self.verify.is_some() {
            if let Some(s) = self.slot.take() {
                self.cursor = s.cursor;
                if let (Some(u), Some(b)) = (s.bandit, self.bandit.as_mut()) {
                    b.undo(u);
                }
                if let Some((id, due_ns, done)) = s.scheduled {
                    if let Some(e) = self.scheduled.iter_mut().find(|e| e.dwell.id == id) {
                        e.due_ns = due_ns;
                        e.done = done;
                    }
                }
                if let Some((key, served)) = s.served {
                    if let Some(e) = self.pois.iter_mut().find(|e| e.poi.key == key) {
                        e.served = served;
                        if s.verification && e.verified {
                            e.verified = false;
                            self.stats.verifications_completed =
                                self.stats.verifications_completed.saturating_sub(1);
                        }
                    }
                }
                self.stats.truncated_slots += 1;
            }
            self.verify = None;
        }
        self.current_end = self.current_end.min(now);
    }

    fn intent_template(&self, now: Timestamp, active: &ActiveIntent) -> Template {
        let slice = self.cfg.intent_slice_ns;
        let duration_ns = active.until.map_or(slice, |until| {
            (until.as_unix_nanos() - now.as_unix_nanos()).clamp(1, slice)
        });
        let i = &active.intent;
        let (gains, gain_entry, accessory) = self.plan.gains_at(i.center_hz);
        Template {
            duration_ns,
            center_hz: i.center_hz,
            rate_hz: i.rate_hz,
            baseband_filter_hz: pick_baseband_filter(&self.caps, i.rate_hz),
            gains: i.gains.unwrap_or(gains),
            gain_entry,
            accessory,
            rf_path: rf_path(self.cfg.rf_path_boundaries(&self.caps), i.center_hz),
            purpose: Purpose::UserIntent { intent: i.id },
            verification_group: None,
        }
    }

    fn slot_template(&mut self) -> Template {
        let sweeps = self.cfg.sweeps_per_cycle;
        let dwells = if !self.plan.pois_allowed() {
            0
        } else if self.low_power {
            (self.cfg.dwells_per_cycle / 2)
                .max(1)
                .min(self.cfg.dwells_per_cycle)
        } else {
            self.cfg.dwells_per_cycle
        };
        if self.cursor.sweeps_in_cycle >= sweeps && self.cursor.dwells_in_cycle < dwells {
            if self.bandit.is_some() {
                if let Some(t) = self.bandit_slot() {
                    return t;
                }
            } else if let Some(i) = self.pick_poi() {
                let before = self.cursor;
                self.cursor.dwells_in_cycle += 1;
                let e = &mut self.pois[i];
                let starts_verification = e.poi.verify && !e.verified;
                self.slot = Some(Snapshot {
                    cursor: before,
                    served: Some((e.poi.key, e.served)),
                    verification: starts_verification,
                    bandit: None,
                    scheduled: None,
                });
                e.served += 1;
                if starts_verification {
                    let e = &self.pois[i];
                    let v = self.verification_for(&e.poi, e.dwell, e.notes, false);
                    if v.remaining() {
                        self.verify = Some(v);
                        return self.verify_template();
                    }
                    self.pois[i].verified = true;
                    self.stats.verifications_completed += 1;
                }
                return self.pois[i].dwell;
            }
        }
        if self.cursor.sweeps_in_cycle >= sweeps {
            self.cursor.sweeps_in_cycle = 0;
            self.cursor.dwells_in_cycle = 0;
        }
        self.slot = Some(Snapshot {
            cursor: self.cursor,
            served: None,
            verification: false,
            bandit: None,
            scheduled: None,
        });
        let index = self.cursor.hop;
        let hop = &self.plan.hops[index];
        let mut t = Template::from_hop(index, hop);
        // T-173: odd passes tune the hop's DC-dithered centre (same slice, path and gains).
        t.center_hz = hop.center_on_pass(self.cursor.passes);
        self.cursor.hop += 1;
        if self.cursor.hop == self.plan.hops.len() {
            self.cursor.hop = 0;
            self.cursor.passes += 1;
        }
        self.cursor.sweeps_in_cycle += 1;
        t
    }

    fn verify_template(&mut self) -> Template {
        let Some(mut v) = self.verify.take() else {
            return self.slot_template();
        };
        let Some(offset) = v.stages[v.next..].iter().position(Option::is_some) else {
            return self.slot_template();
        };
        let index = v.next + offset;
        let t = v.stages[index].expect("stage exists");
        v.next = index + 1;
        if v.remaining() {
            self.verify = Some(v);
        } else if v.bandit {
            let end_ns = self.last_now.as_unix_nanos().saturating_add(t.duration_ns);
            if let Some(b) = self.bandit.as_mut() {
                b.verification_finished(v.key, end_ns);
            }
            self.stats.verifications_completed += 1;
        } else if let Some(e) = self.pois.iter_mut().find(|e| e.poi.key == v.key) {
            e.verified = true;
            self.stats.verifications_completed += 1;
        }
        t
    }

    /// Weighted round robin: the POI with the lowest `served / weight`, where the weight is the
    /// score raised to at least `MIN_DWELL_SHARE` of the strongest score; ties go to the higher
    /// weight, then the earlier offer.
    fn pick_poi(&self) -> Option<usize> {
        let floor = self.pois.iter().map(|e| e.score).fold(0.0, f64::max) * MIN_DWELL_SHARE;
        let weight = |e: &PoiEntry| e.score.max(floor);
        let mut best: Option<usize> = None;
        for (i, e) in self.pois.iter().enumerate() {
            let better = match best {
                None => true,
                Some(b) => {
                    let bb = &self.pois[b];
                    let (we, wb) = (weight(e), weight(bb));
                    let lhs = e.served as f64 * wb;
                    let rhs = bb.served as f64 * we;
                    lhs < rhs || (lhs == rhs && (we > wb || (we == wb && e.order < bb.order)))
                }
            };
            if better {
                best = Some(i);
            }
        }
        best
    }

    /// The verification stages for `poi` around its dwell `base`; every stage carries the group
    /// id, the `seq` of the group's first step (the step about to be emitted).
    fn verification_for(
        &self,
        poi: &Poi,
        base: Template,
        notes: PoiNotes,
        bandit: bool,
    ) -> VerifyState {
        let key = poi.key;
        let group = Some(self.seq);
        let mut stages = [None; MAX_STAGES];
        let mut n = 0;
        let mut push = |t: Template| {
            stages[n] = Some(Template {
                verification_group: group,
                ..t
            });
            n += 1;
        };
        match self.alt_gains(base.gains) {
            Some(alt) => {
                for pair in 0..self.cfg.gain_step_pairs {
                    for (slot, gains) in [(GainSlot::A, base.gains), (GainSlot::B, alt)] {
                        push(Template {
                            duration_ns: self.cfg.gain_step_block_ns,
                            gains,
                            purpose: Purpose::GainStep {
                                poi: key,
                                pair,
                                slot,
                            },
                            ..base
                        });
                    }
                }
            }
            // No gain-step stages: a baseline dwell is the retune/rate-change reference.
            None => push(Template {
                duration_ns: self.cfg.retune_dwell_ns,
                ..base
            }),
        }
        for delta_hz in notes.retunes.iter().filter_map(RetunePlan::delta_hz) {
            push(Template {
                duration_ns: self.cfg.retune_dwell_ns,
                center_hz: base.center_hz + delta_hz,
                purpose: Purpose::Retune { poi: key, delta_hz },
                ..base
            });
        }
        if self.cfg.rate_change {
            if let Some(rate_hz) = self.alt_rate(poi, &base) {
                push(Template {
                    duration_ns: self.cfg.retune_dwell_ns,
                    rate_hz,
                    baseband_filter_hz: pick_baseband_filter(&self.caps, rate_hz),
                    purpose: Purpose::RateChange {
                        poi: key,
                        base_rate_hz: base.rate_hz,
                    },
                    ..base
                });
            }
        }
        VerifyState {
            key,
            stages,
            next: 0,
            bandit,
        }
    }

    /// State B: LNA up one step, or down when A is at the top; `None` if neither fits or the
    /// gain step is disabled.
    fn alt_gains(&self, a: Gains) -> Option<Gains> {
        let step = self.cfg.gain_step_lna_db;
        if step <= 0.0 || self.cfg.gain_step_pairs == 0 {
            return None;
        }
        let (up, down) = (a.lna_db + step, a.lna_db - step);
        let lna_db = match self.caps.gain_stages.iter().find(|s| s.name == "lna") {
            None => up,
            Some(s) if up <= s.max_db => up,
            Some(s) if down >= s.min_db => down,
            Some(_) => return None,
        };
        Some(Gains { lna_db, ..a })
    }

    /// Another supported rate at which the emitter still fits the usable span.
    fn alt_rate(&self, poi: &Poi, dwell: &Template) -> Option<f64> {
        let base = dwell.rate_hz;
        let offset = (poi.center_hz - dwell.center_hz).abs();
        let f = self.cfg.rate_change_factor;
        [base * f, base / f]
            .into_iter()
            .map(|want| pick_rate(&self.caps, want, self.cfg.rate_quantum_hz))
            .find(|&rate| {
                let usable = (rate * self.cfg.usable_fraction).min(self.cfg.max_span_hz);
                (rate - base).abs() > 0.5 && offset + poi.bandwidth_hz / 2.0 <= usable / 2.0
            })
    }
}

impl Scheduler<SyntheticClock> {
    /// Emits `steps` steps, advancing the synthetic clock by each step's duration.
    pub fn run_synthetic(&mut self, steps: usize, out: &mut Vec<ScheduleStep>) {
        for _ in 0..steps {
            self.refresh_bandit();
            let step = self.next_step();
            self.clock.advance_ns(step.duration_ns);
            out.push(step);
        }
    }
}

fn dwell_template(
    plan: &CompiledPlan,
    cfg: &SchedulerConfig,
    caps: &SourceCapabilities,
    poi: &Poi,
) -> Template {
    let bw = poi.bandwidth_hz;
    let needed = cfg.dwell_min_rate_hz.max(2.0 * bw / cfg.usable_fraction);
    let rate_hz = pick_rate(caps, needed, cfg.rate_quantum_hz);
    let usable = (rate_hz * cfg.usable_fraction).min(cfg.max_span_hz);
    let offset = if bw <= usable / 2.0 {
        usable / 4.0
    } else {
        ((usable - bw) / 2.0).max(0.0)
    };
    let bounds = cfg.rf_path_boundaries(caps);
    let path = rf_path(bounds, poi.center_hz);
    let center_hz = [poi.center_hz - offset, poi.center_hz + offset]
        .into_iter()
        .find(|&c| caps.supports_frequency(c) && rf_path(bounds, c) == path)
        .unwrap_or(poi.center_hz);
    let wanted = poi
        .burst_interval_ns
        .map_or(cfg.dwell_default_ns, |interval| {
            (interval as f64 * cfg.burst_intervals_per_dwell).min(i64::MAX as f64) as i64
        });
    let duration_ns = wanted
        .clamp(cfg.dwell_min_ns, cfg.dwell_max_ns)
        .min(plan.dwell_cap_ns)
        .max(cfg.dwell_min_ns);
    let (gains, gain_entry, accessory) = plan.gains_at(poi.center_hz);
    Template {
        duration_ns,
        center_hz,
        rate_hz,
        baseband_filter_hz: pick_baseband_filter(caps, rate_hz),
        gains,
        gain_entry,
        accessory,
        rf_path: rf_path(bounds, center_hz),
        purpose: Purpose::Dwell { poi: poi.key },
        verification_group: None,
    }
}

/// Dwell fit and retune plan for `poi` in `dwell`. A retune by `x` moves the emitter to `o0 − x`
/// from the LO (`o0` its dwell offset); it must stay at `|o0 − x|` in `[bw/2 + guard, usable/2 −
/// bw/2]`. Each direction takes the configured offset if that fits, else the largest fitting
/// smaller offset that still separates LO-relative products, else it is skipped with the reason.
fn poi_notes(
    cfg: &SchedulerConfig,
    caps: &SourceCapabilities,
    poi: &Poi,
    dwell: &Template,
) -> PoiNotes {
    let usable_half = (dwell.rate_hz * cfg.usable_fraction).min(cfg.max_span_hz) / 2.0;
    let half_bw = poi.bandwidth_hz / 2.0;
    let o0 = poi.center_hz - dwell.center_hz;
    let (near, far) = (half_bw + DC_GUARD_HZ, usable_half - half_bw);
    let bounds = cfg.rf_path_boundaries(caps);
    let delta = cfg.retune_delta_hz;
    let min_shrunk = (poi.bandwidth_hz + RETUNE_SEPARATION_HZ).max(0.25 * delta);
    let retune = |d: f64| -> RetunePlan {
        if d == 0.0 {
            return RetunePlan::Skipped(RetuneSkip::Disabled);
        }
        if near > far {
            return RetunePlan::Skipped(RetuneSkip::NoRoom);
        }
        let (sign, full) = (d.signum(), d.abs());
        // Allowed x: [o0 − far, o0 − near] ∪ [o0 + near, o0 + far]; in magnitude along `sign`,
        // clipped to (0, full], the largest.
        let best = [(o0 - far, o0 - near), (o0 + near, o0 + far)]
            .into_iter()
            .filter_map(|(a, b)| {
                let (lo, hi) = ((sign * a).min(sign * b), (sign * a).max(sign * b));
                let top = hi.min(full);
                (top > 0.0 && top >= lo).then_some(top)
            })
            .max_by(f64::total_cmp);
        let magnitude = match best {
            Some(m) if m == full || m >= min_shrunk => m,
            _ => {
                let moved = (o0 - d).abs();
                return RetunePlan::Skipped(if moved + half_bw > usable_half {
                    RetuneSkip::PastUsableEdge
                } else {
                    RetuneSkip::CrossesDc
                });
            }
        };
        let center_hz = dwell.center_hz + sign * magnitude;
        if !caps.supports_frequency(center_hz) {
            RetunePlan::Skipped(RetuneSkip::OutOfRange)
        } else if rf_path(bounds, center_hz) != dwell.rf_path {
            RetunePlan::Skipped(RetuneSkip::RfPathSwitch)
        } else if magnitude == full {
            RetunePlan::Full { delta_hz: d }
        } else {
            RetunePlan::Shrunk {
                delta_hz: sign * magnitude,
                configured_hz: d,
            }
        }
    };
    PoiNotes {
        wider_than_span: poi.bandwidth_hz > 2.0 * usable_half,
        on_dc: o0.abs() < near,
        retunes: [retune(delta), retune(-delta)],
    }
}
