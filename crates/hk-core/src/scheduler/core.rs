//! The v1 attention scheduler: fixed sweep/dwell alternation (ADR-0005 "first version"). See the
//! [module docs](super) for the policy.

use hk_model::{ScanPlan, Survey, SurveyId, SurveyState, SurveySummary, Timestamp};

use super::clock::{Clock, SyntheticClock};
use super::config::{MAX_GAIN_STEP_PAIRS, SchedulerConfig};
use super::plan::{
    CompiledPlan, Hop, HopKind, PlanError, check_gains, pick_baseband_filter, pick_rate, rf_path,
};
use super::step::{GainSlot, PoiKey, Purpose, ScheduleStep};
use super::survey::SurveyLog;
use crate::source::{Gains, SourceCapabilities};

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
}

const MAX_STAGES: usize = 2 * MAX_GAIN_STEP_PAIRS as usize + 4;

#[derive(Clone, Copy, Debug)]
struct VerifyState {
    key: PoiKey,
    stages: [Option<Template>; MAX_STAGES],
    next: usize,
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
struct PoiEntry {
    poi: Poi,
    dwell: Template,
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
    stats: ScheduleStats,
    survey: Option<OpenSurvey>,
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
            stats: ScheduleStats::default(),
            survey: None,
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

    /// Complete discovery passes so far.
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

    /// The next step, starting now. Order: user intent, then a running verification group, then
    /// the sweep/dwell cycle.
    pub fn next_step(&mut self) -> ScheduleStep {
        let now = self.clock.now();
        if self
            .intent
            .is_some_and(|a| a.until.is_some_and(|until| now >= until))
        {
            self.intent = None;
        }
        let t = if let Some(active) = self.intent {
            self.slot = None;
            self.intent_template(now, &active)
        } else if self.verify.is_some() {
            self.verify_template()
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
        };
        self.seq += 1;
        self.current_end = step.t_end();
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
        }
        step
    }

    /// Queues or updates a POI. Its dwell is sized now: rate ≥ `dwell_min_rate_hz` and wide
    /// enough that the emitter fits half the usable span, offset-tuned by a quarter span so the
    /// emitter clears DC, duration from its burst interval (else the default), clamped to the
    /// plan's dwell cap. A full queue evicts the lowest-scoring POI if the new one scores higher.
    pub fn offer_poi(&mut self, poi: Poi) -> Result<(), SchedulerError> {
        self.check_poi(&poi)?;
        let dwell = dwell_template(&self.plan, &self.cfg, &self.caps, &poi);
        let score = self.score(&poi);
        if let Some(e) = self.pois.iter_mut().find(|e| e.poi.key == poi.key) {
            e.poi = poi;
            e.dwell = dwell;
            e.score = score;
            return Ok(());
        }
        // Start a new POI level with the least-served one, so it does not monopolise dwells.
        let min_ratio = self
            .pois
            .iter()
            .map(|e| e.served as f64 / e.score)
            .fold(f64::INFINITY, f64::min);
        let served = if min_ratio.is_finite() {
            (min_ratio * score).floor() as u64
        } else {
            0
        };
        let entry = PoiEntry {
            poi,
            dwell,
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
    /// and re-visited later; a cut verification group restarts from its first stage.
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
        let now = self.clock.now();
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
            let now = self.clock.now();
            self.current_end = self.current_end.min(now);
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
            t_start: self.clock.now(),
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
        log.close(id, state, self.clock.now(), summary)?;
        self.survey = None;
        Ok(id)
    }

    /// Switches to a new plan version (or another plan). The new plan is compiled first; on
    /// error the running plan is kept. If a Survey is open it is closed with `summary` and a new
    /// one opens under the new version (returned). The discovery pass restarts; queued POIs are
    /// kept and re-sized; a running verification group is dropped (it restarts later); a user
    /// intent stays in force.
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
                log.close(id, SurveyState::Closed, self.clock.now(), summary)?;
                self.survey = None;
                Some(device)
            }
            None => None,
        };
        if let Some(open) = self.verify.take() {
            if let Some(e) = self.pois.iter_mut().find(|e| e.poi.key == open.key) {
                e.verified = false;
            }
        }
        self.cfg = cfg;
        self.plan = compiled;
        self.cursor = Cursor::default();
        self.slot = None;
        if self.pois.capacity() < self.cfg.max_pois {
            self.pois.reserve(self.cfg.max_pois - self.pois.len());
        }
        for i in 0..self.pois.len() {
            let poi = self.pois[i].poi;
            self.pois[i].dwell = dwell_template(&self.plan, &self.cfg, &self.caps, &poi);
            self.pois[i].score = self.score(&poi);
        }
        match device {
            Some(device) => Ok(Some(self.open_survey(log, &device)?)),
            None => Ok(None),
        }
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
        poi.interestingness.max(1e-6) * priority.max(1e-6)
    }

    /// Rolls back the running slot if `now` is inside it or a verification group is unfinished.
    fn cut(&mut self, now: Timestamp) {
        if now < self.current_end || self.verify.is_some() {
            if let Some(s) = self.slot.take() {
                self.cursor = s.cursor;
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
            rf_path: rf_path(&self.cfg.rf_path_boundaries_hz, i.center_hz),
            purpose: Purpose::UserIntent { intent: i.id },
        }
    }

    fn slot_template(&mut self) -> Template {
        let sweeps = self.cfg.sweeps_per_cycle;
        let dwells = if self.plan.pois_allowed() {
            self.cfg.dwells_per_cycle
        } else {
            0
        };
        if self.cursor.sweeps_in_cycle >= sweeps && self.cursor.dwells_in_cycle < dwells {
            if let Some(i) = self.pick_poi() {
                let before = self.cursor;
                self.cursor.dwells_in_cycle += 1;
                let e = &mut self.pois[i];
                let starts_verification = e.poi.verify && !e.verified;
                self.slot = Some(Snapshot {
                    cursor: before,
                    served: Some((e.poi.key, e.served)),
                    verification: starts_verification,
                });
                e.served += 1;
                if starts_verification {
                    let v = self.verification_for(i);
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
        });
        let index = self.cursor.hop;
        let t = Template::from_hop(index, &self.plan.hops[index]);
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
        } else if let Some(e) = self.pois.iter_mut().find(|e| e.poi.key == v.key) {
            e.verified = true;
            self.stats.verifications_completed += 1;
        }
        t
    }

    /// Weighted round robin: the POI with the lowest `served / score`; ties go to the higher
    /// score, then the earlier offer.
    fn pick_poi(&self) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (i, e) in self.pois.iter().enumerate() {
            let better = match best {
                None => true,
                Some(b) => {
                    let bb = &self.pois[b];
                    let lhs = e.served as f64 * bb.score;
                    let rhs = bb.served as f64 * e.score;
                    lhs < rhs
                        || (lhs == rhs
                            && (e.score > bb.score || (e.score == bb.score && e.order < bb.order)))
                }
            };
            if better {
                best = Some(i);
            }
        }
        best
    }

    fn verification_for(&self, i: usize) -> VerifyState {
        let e = &self.pois[i];
        let key = e.poi.key;
        let base = e.dwell;
        let mut stages = [None; MAX_STAGES];
        let mut n = 0;
        let mut push = |t: Template| {
            stages[n] = Some(t);
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
        if self.cfg.retune_delta_hz > 0.0 {
            for delta_hz in [self.cfg.retune_delta_hz, -self.cfg.retune_delta_hz] {
                let center_hz = base.center_hz + delta_hz;
                if self.caps.supports_frequency(center_hz)
                    && rf_path(&self.cfg.rf_path_boundaries_hz, center_hz) == base.rf_path
                {
                    push(Template {
                        duration_ns: self.cfg.retune_dwell_ns,
                        center_hz,
                        purpose: Purpose::Retune { poi: key, delta_hz },
                        ..base
                    });
                }
            }
        }
        if self.cfg.rate_change {
            if let Some(rate_hz) = self.alt_rate(e) {
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
    fn alt_rate(&self, e: &PoiEntry) -> Option<f64> {
        let base = e.dwell.rate_hz;
        let offset = (e.poi.center_hz - e.dwell.center_hz).abs();
        let f = self.cfg.rate_change_factor;
        [base * f, base / f]
            .into_iter()
            .map(|want| pick_rate(&self.caps, want, self.cfg.rate_quantum_hz))
            .find(|&rate| {
                let usable = (rate * self.cfg.usable_fraction).min(self.cfg.max_span_hz);
                (rate - base).abs() > 0.5 && offset + e.poi.bandwidth_hz / 2.0 <= usable / 2.0
            })
    }
}

impl Scheduler<SyntheticClock> {
    /// Emits `steps` steps, advancing the synthetic clock by each step's duration.
    pub fn run_synthetic(&mut self, steps: usize, out: &mut Vec<ScheduleStep>) {
        for _ in 0..steps {
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
    let bounds = &cfg.rf_path_boundaries_hz;
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
    }
}
