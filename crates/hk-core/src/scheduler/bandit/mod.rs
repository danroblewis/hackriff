//! Bandit revisit policy (T-120, ADR-0012 §5): arms packed from
//! `hk_model::attention::score::CandidateSet` snapshots, discounted cost-normalised UCB,
//! exploration and sweep floors, suspect verification, preemption tiers and POI accounting.
//!
//! # Decisions (in order, at each bandit dwell slot of the sweep/dwell cycle)
//!
//! 1. **Verification:** a `needs_verification` candidate gets exactly one verification group (the
//!    S4 gain-step/retune machinery). Pass (C12 clears the flag, or
//!    [`super::Scheduler::report_verification`]): packed normally from then on. Fail (still
//!    flagged in a snapshot scored after the group, or reported failed): banned for
//!    `suspect_ban_s`, after which a still-flagged candidate is verified once more.
//! 2. **Complete capture / beacon due:** an arm with a candidate `min_on_off_s` is revisited every
//!    ½ of it when that is feasible (at least `min_dwell_s`, and the summed demand fits beside the
//!    sweep floor); a candidate `next_burst_eta` inside the next dwell also makes its arm due.
//! 3. **Starvation bound:** a candidate arm unvisited for `max_arm_staleness_s` is revisited.
//!    Hop exploration arms (prior 0) are served by the exploration floor and UCB instead: tiling
//!    0–6 GHz with 8 s exploration dwells cannot meet a 30-minute bound (disclosed as a deviation
//!    in the T-120 report).
//! 4. **Exploration floor:** exploration below `exploration_floor` of bandit time picks the
//!    stalest arm.
//! 5. **UCB:** `ucb_index(ū, arm_dwell_s, Σ dwell_s, c) × (1 − suspect_fraction)`, `ū` seeded with
//!    the arm's best `score_norm` over `prior_pseudo_dwell_s`, statistics discounted with a
//!    `discount_half_life_s` half-life of device-clock time. Ties go to the lower `ArmKey`; no RNG.
//!
//! The scheduler then applies the **sweep floor** (discovery ≥ `sweep_floor` of radio time over
//! `sweep_floor_window_s`): a dwell that would push discovery below it is deferred and the cycle
//! sweeps on. All time is the scheduler clock (device/sample time, ADR-0012 §0).
//!
//! Allocation: packing (`repack`) runs only when the provider's version moved and may allocate;
//! picking, committing, undoing and recording outcomes use the preallocated tables.

mod pack;
mod poi;
mod tiers;
mod window;

pub(crate) use poi::CoverageRing;
pub use poi::{
    CoverageVisit, DEFAULT_POI_TAUS_S, MAX_CELLS, MAX_GAPS, RegionPoi, region_poi,
    visits_from_records,
};
pub use tiers::{AttentionStatus, Lease, MAX_LEASES, MAX_SCHEDULED, ScheduledDwell};
pub(crate) use window::{Share, ShareNs, TierWindow};

use std::sync::Arc;

use hk_model::attention::schedule::{ArmKey, BanditConfig, DwellOutcome, ucb_index};
use hk_model::attention::score::{CandidateSubject, InterestingnessProvider};

use super::config::SchedulerConfig;
use super::plan::{CompiledPlan, HopKind, pick_rate, rf_path};
use crate::source::SourceCapabilities;
use pack::{Geometry, PackItem};

/// Most candidates awaiting or holding a verification record.
pub const MAX_VERIFICATIONS: usize = 64;

/// Candidates are placed at least this far (beyond their half bandwidth) from DC, Hz.
pub const DC_GUARD_HZ: f64 = 10e3;

const NS: f64 = 1e9;

/// What kind of bandit dwell a step is (its ADR-0012 §1.2 reason).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BanditKind {
    /// UCB exploitation (`novelty <score_norm>`).
    Exploit {
        /// The arm's prior (best `score_norm`).
        score: f32,
    },
    /// Exploration floor or starvation bound.
    Explore,
    /// Complete-capture revisit or a predicted burst.
    BeaconDue {
        /// Required revisit, or seconds to the predicted burst, s.
        eta_s: f32,
    },
}

/// The scheduler's `u64` key of a candidate subject: the low 64 bits of an emitter or track id,
/// or a mix of a cell range.
pub fn subject_key(s: &CandidateSubject) -> u64 {
    match s {
        CandidateSubject::Emitter { id } => id.as_uuid().as_u128() as u64,
        CandidateSubject::Track { id } => id.as_uuid().as_u128() as u64,
        CandidateSubject::Cells {
            scheme,
            lo_cell,
            hi_cell,
        } => {
            let mut z = u64::from(*scheme)
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add(*lo_cell as u64)
                .rotate_left(29)
                .wrapping_add(*hi_cell as u64);
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
    }
}

/// The arm key of a window.
pub fn arm_key(rf_path: u8, center_hz: f64, rate_hz: f64, quantum_hz: f64) -> ArmKey {
    ArmKey {
        rf_path,
        center_q: (center_hz / quantum_hz).round() as i64,
        rate_hz: rate_hz.round().clamp(0.0, u32::MAX as f64) as u32,
    }
}

#[derive(Clone, Copy, Debug)]
struct Arm {
    key: ArmKey,
    center_hz: f64,
    rate_hz: f64,
    rf_path: u8,
    active: bool,
    exploration: bool,
    on_dc: bool,
    prior: f64,
    suspect_fraction: f64,
    lead: Option<u64>,
    members: u16,
    required_revisit_s: Option<f64>,
    complete_capture: bool,
    next_eta_ns: Option<i64>,
    dwell_ns: i64,
    reward_sum: f64,
    dwell_s: f64,
    t_stats_ns: i64,
    visits: u64,
    last_visit_ns: Option<i64>,
    added_ns: i64,
    last_reward: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum VerifyState {
    Pending,
    Running,
    Done { at_ns: i64 },
    Passed,
    Banned { until_ns: i64 },
}

#[derive(Clone, Copy, Debug)]
struct VerifyRec {
    key: u64,
    center_hz: f64,
    bandwidth_hz: f64,
    state: VerifyState,
    seen: bool,
}

/// A bandit decision before the sweep floor is checked.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Choice {
    Verify {
        key: u64,
        center_hz: f64,
        bandwidth_hz: f64,
    },
    Dwell {
        arm: usize,
        kind: BanditKind,
    },
}

/// What a committed choice changed, restored when a preemption cuts the slot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Undo {
    Arm {
        arm: usize,
        last_visit_ns: Option<i64>,
        visits: u64,
    },
    Verify {
        key: u64,
    },
}

/// The window of one arm, for building its step.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ArmView {
    pub center_hz: f64,
    pub rate_hz: f64,
    pub rf_path: u8,
    pub dwell_ns: i64,
    pub lead: Option<u64>,
}

/// One row of the arm table (`GET /api/scheduler/arms`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ArmStatus {
    /// Table index (the `arm` of bandit reasons).
    pub index: u32,
    /// Key.
    pub key: ArmKey,
    /// Tuned centre, Hz.
    pub center_hz: f64,
    /// Rate, Hz.
    pub rate_hz: f64,
    /// In the latest packing.
    pub active: bool,
    /// A hop exploration arm (prior 0) rather than a candidate window.
    pub exploration: bool,
    /// The lead candidate could not be placed clear of DC.
    pub on_dc: bool,
    /// Best `score_norm` of its candidates.
    pub prior: f64,
    /// Discounted mean reward including the prior pseudo-dwell.
    pub mean_reward: f64,
    /// Discounted observed dwell-seconds.
    pub dwell_s: f64,
    /// UCB index now (after the suspect scaling).
    pub ucb: f64,
    /// Visits.
    pub visits: u64,
    /// Seconds since the last visit (or since added).
    pub staleness_s: f64,
    /// Score-weighted suspect fraction of its candidates.
    pub suspect_fraction: f64,
    /// Lead candidate key.
    pub lead: Option<u64>,
    /// Candidates packed.
    pub members: u16,
    /// Planned dwell, s.
    pub dwell_planned_s: f64,
    /// Required revisit for complete capture, s.
    pub required_revisit_s: Option<f64>,
    /// The required revisit is being scheduled (else occupancy there is statistical).
    pub complete_capture: bool,
    /// Last bounded reward.
    pub last_reward: Option<f64>,
}

/// Bandit counters.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BanditCounters {
    /// Snapshots packed.
    pub repacks: u64,
    /// Outcomes recorded.
    pub outcomes: u64,
    /// Outcomes for unknown arms.
    pub outcomes_unmatched: u64,
    /// Exploitation dwells.
    pub exploit_dwells: u64,
    /// Exploration dwells (floor and starvation bound).
    pub explore_dwells: u64,
    /// Of which forced by the starvation bound.
    pub stale_forced: u64,
    /// Complete-capture / beacon dwells.
    pub beacon_dwells: u64,
    /// Verification groups started.
    pub verifications_started: u64,
    /// Passed.
    pub verifications_passed: u64,
    /// Failed (banned).
    pub verifications_failed: u64,
    /// Bandit slots deferred by the sweep floor.
    pub floor_deferrals: u64,
    /// Packed windows dropped because the arm table was full of active arms.
    pub arms_dropped: u64,
    /// Dwell-seconds whose outcome had only suspect detections.
    pub suspect_wasted_s: f64,
}

/// Bandit summary (`GET /api/scheduler`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BanditStatus {
    /// Provider version last packed.
    pub provider_version: Option<u64>,
    /// Arms in the table.
    pub arms: usize,
    /// Of which active.
    pub active_arms: usize,
    /// Candidates awaiting verification.
    pub pending_verifications: usize,
    /// Candidates banned now.
    pub banned: usize,
    /// Discounted dwell-seconds over all arms.
    pub total_dwell_s: f64,
    /// Settings.
    pub config: BanditConfig,
    /// Counters.
    pub counters: BanditCounters,
}

/// The bandit state inside the scheduler.
pub(crate) struct Bandit {
    pub(crate) cfg: BanditConfig,
    provider: Arc<dyn InterestingnessProvider>,
    seen_version: Option<u64>,
    arms: Vec<Arm>,
    verifs: Vec<VerifyRec>,
    total_dwell_s: f64,
    total_t_ns: i64,
    explore_cursor: usize,
    pub(crate) counters: BanditCounters,
}

impl Bandit {
    pub(crate) fn new(cfg: BanditConfig, provider: Arc<dyn InterestingnessProvider>) -> Self {
        Self {
            arms: Vec::with_capacity(usize::from(cfg.max_arms)),
            verifs: Vec::with_capacity(MAX_VERIFICATIONS),
            cfg,
            provider,
            seen_version: None,
            total_dwell_s: 0.0,
            total_t_ns: 0,
            explore_cursor: 0,
            counters: BanditCounters::default(),
        }
    }

    /// The provider's version moved since the last packing (cheap, lock-free).
    pub(crate) fn stale(&self) -> bool {
        self.seen_version != Some(self.provider.version())
    }

    /// Forces a repack at the next decision boundary (plan or settings changed).
    pub(crate) fn invalidate(&mut self) {
        self.seen_version = None;
    }

    fn decay(&self, from_ns: i64, now_ns: i64) -> f64 {
        let dt = (now_ns - from_ns).max(0) as f64 / NS;
        0.5f64.powf(dt / self.cfg.discount_half_life_s)
    }

    fn mean_and_dwell(&self, a: &Arm, now_ns: i64) -> (f64, f64) {
        let f = self.decay(a.t_stats_ns, now_ns);
        let pseudo = self.cfg.prior_pseudo_dwell_s;
        let dwell = a.dwell_s * f;
        let n = pseudo + dwell;
        let mean = if n > 0.0 {
            (a.prior * pseudo + a.reward_sum * f) / n
        } else {
            a.prior
        };
        (mean, n)
    }

    fn index(&self, a: &Arm, now_ns: i64) -> f64 {
        let (mean, n) = self.mean_and_dwell(a, now_ns);
        let total = self.total_dwell_s * self.decay(self.total_t_ns, now_ns) + n;
        ucb_index(mean, n, total, self.cfg.ucb_c) * (1.0 - a.suspect_fraction)
    }

    fn since_ns(a: &Arm, now_ns: i64) -> i64 {
        now_ns - a.last_visit_ns.unwrap_or(a.added_ns)
    }

    /// `a` is staler than `b`: never visited first (earlier added), then the older visit, then the
    /// lower key.
    fn staler(a: &Arm, b: &Arm) -> bool {
        match (a.last_visit_ns, b.last_visit_ns) {
            (None, Some(_)) => true,
            (Some(_), None) => false,
            (None, None) => (a.added_ns, a.key) < (b.added_ns, b.key),
            (Some(x), Some(y)) => (x, a.key) < (y, b.key),
        }
    }

    /// The next bandit decision (no side effects).
    pub(crate) fn pick(&self, now_ns: i64, shares: &ShareNs) -> Option<Choice> {
        if let Some(v) = self.verifs.iter().find(|v| v.state == VerifyState::Pending) {
            return Some(Choice::Verify {
                key: v.key,
                center_hz: v.center_hz,
                bandwidth_hz: v.bandwidth_hz,
            });
        }
        let active = || self.arms.iter().enumerate().filter(|(_, a)| a.active);
        // Complete capture and predicted bursts: the most overdue.
        let mut due: Option<(usize, i64, f32)> = None;
        for (i, a) in active().filter(|(_, a)| !a.exploration) {
            let mut overdue: Option<(i64, f32)> = None;
            if let (Some(r), true) = (a.required_revisit_s, a.complete_capture) {
                let o = Self::since_ns(a, now_ns) - (r * NS) as i64;
                if o >= 0 {
                    overdue = Some((o, r as f32));
                }
            }
            if let Some(eta) = a.next_eta_ns {
                let fresh = a.last_visit_ns.is_none_or(|l| l + a.dwell_ns <= eta);
                if fresh && eta <= now_ns + a.dwell_ns / 3 && eta + a.dwell_ns >= now_ns {
                    let o = now_ns - eta;
                    if overdue.is_none_or(|(x, _)| o > x) {
                        overdue = Some((o, ((eta - now_ns) as f64 / NS) as f32));
                    }
                }
            }
            if let Some((o, eta_s)) = overdue {
                if due.is_none_or(|(j, d, _)| o > d || (o == d && a.key < self.arms[j].key)) {
                    due = Some((i, o, eta_s));
                }
            }
        }
        if let Some((arm, _, eta_s)) = due {
            return Some(Choice::Dwell {
                arm,
                kind: BanditKind::BeaconDue { eta_s },
            });
        }
        // Starvation bound over candidate arms.
        let limit = (self.cfg.max_arm_staleness_s * NS) as i64;
        let stale = active()
            .filter(|(_, a)| !a.exploration && Self::since_ns(a, now_ns) >= limit)
            .fold(None::<usize>, |best, (i, a)| match best {
                Some(b) if !Self::staler(a, &self.arms[b]) => Some(b),
                _ => Some(i),
            });
        if let Some(arm) = stale {
            return Some(Choice::Dwell {
                arm,
                kind: BanditKind::Explore,
            });
        }
        // Exploration floor.
        let stalest = active().fold(None::<usize>, |best, (i, a)| match best {
            Some(b) if !Self::staler(a, &self.arms[b]) => Some(b),
            _ => Some(i),
        })?;
        let bandit_ns = shares[Share::Exploit as usize] + shares[Share::Explore as usize];
        let explore_ns = shares[Share::Explore as usize] as f64;
        if explore_ns
            < self.cfg.exploration_floor * (bandit_ns + self.arms[stalest].dwell_ns) as f64
        {
            return Some(Choice::Dwell {
                arm: stalest,
                kind: BanditKind::Explore,
            });
        }
        // UCB.
        let mut best: Option<(usize, f64)> = None;
        for (i, a) in active() {
            let idx = self.index(a, now_ns);
            let better = match best {
                None => true,
                Some((b, bi)) => idx > bi || (idx == bi && a.key < self.arms[b].key),
            };
            if better {
                best = Some((i, idx));
            }
        }
        best.map(|(arm, _)| Choice::Dwell {
            arm,
            kind: BanditKind::Exploit {
                score: self.arms[arm].prior as f32,
            },
        })
    }

    pub(crate) fn arm_view(&self, i: usize) -> ArmView {
        let a = &self.arms[i];
        ArmView {
            center_hz: a.center_hz,
            rate_hz: a.rate_hz,
            rf_path: a.rf_path,
            dwell_ns: a.dwell_ns,
            lead: a.lead,
        }
    }

    pub(crate) fn commit(&mut self, choice: Choice, now_ns: i64) -> Undo {
        match choice {
            Choice::Verify { key, .. } => {
                if let Some(v) = self.verifs.iter_mut().find(|v| v.key == key) {
                    v.state = VerifyState::Running;
                }
                self.counters.verifications_started += 1;
                Undo::Verify { key }
            }
            Choice::Dwell { arm, kind } => {
                let limit = (self.cfg.max_arm_staleness_s * NS) as i64;
                let a = &mut self.arms[arm];
                let undo = Undo::Arm {
                    arm,
                    last_visit_ns: a.last_visit_ns,
                    visits: a.visits,
                };
                match kind {
                    BanditKind::Exploit { .. } => self.counters.exploit_dwells += 1,
                    BanditKind::Explore => {
                        self.counters.explore_dwells += 1;
                        if !a.exploration && Self::since_ns(a, now_ns) >= limit {
                            self.counters.stale_forced += 1;
                        }
                    }
                    BanditKind::BeaconDue { .. } => self.counters.beacon_dwells += 1,
                }
                a.last_visit_ns = Some(now_ns);
                a.visits += 1;
                undo
            }
        }
    }

    pub(crate) fn undo(&mut self, undo: Undo) {
        match undo {
            Undo::Verify { key } => {
                if let Some(v) = self.verifs.iter_mut().find(|v| v.key == key) {
                    if matches!(v.state, VerifyState::Running | VerifyState::Done { .. }) {
                        v.state = VerifyState::Pending;
                        self.counters.verifications_started =
                            self.counters.verifications_started.saturating_sub(1);
                    }
                }
            }
            Undo::Arm {
                arm,
                last_visit_ns,
                visits,
            } => {
                if let Some(a) = self.arms.get_mut(arm) {
                    a.last_visit_ns = last_visit_ns;
                    a.visits = visits;
                }
            }
        }
    }

    /// The verification group of `key` emitted its last stage.
    pub(crate) fn verification_finished(&mut self, key: u64, now_ns: i64) {
        if let Some(v) = self.verifs.iter_mut().find(|v| v.key == key) {
            if v.state == VerifyState::Running {
                v.state = VerifyState::Done { at_ns: now_ns };
            }
        }
    }

    /// An explicit trust-test verdict for `key`.
    pub(crate) fn report_verification(&mut self, key: u64, passed: bool, now_ns: i64) -> bool {
        let Some(v) = self.verifs.iter_mut().find(|v| v.key == key) else {
            return false;
        };
        if passed {
            v.state = VerifyState::Passed;
            self.counters.verifications_passed += 1;
        } else {
            v.state = VerifyState::Banned {
                until_ns: now_ns + (self.cfg.suspect_ban_s * NS) as i64,
            };
            self.counters.verifications_failed += 1;
        }
        true
    }

    /// Records a dwell outcome. Returns whether its arm is known.
    pub(crate) fn record_outcome(&mut self, o: &DwellOutcome, now_ns: i64) -> bool {
        let Some(i) = self.arms.iter().position(|a| a.key == o.arm) else {
            self.counters.outcomes_unmatched += 1;
            return false;
        };
        let dwell_s = if o.dwell_s.is_finite() {
            o.dwell_s.max(0.0)
        } else {
            0.0
        };
        let only_suspect = o.suspect_detections > 0
            && o.new_detections == 0
            && o.bursts == 0
            && o.valid_decodes == 0
            && o.novelty_sum <= 0.0;
        let u = if only_suspect {
            self.counters.suspect_wasted_s += dwell_s;
            0.0
        } else {
            o.reward(&self.cfg.reward)
        };
        let ft = self.decay(self.total_t_ns, now_ns);
        self.total_dwell_s = self.total_dwell_s * ft + dwell_s;
        self.total_t_ns = now_ns;
        let f = self.decay(self.arms[i].t_stats_ns, now_ns);
        let a = &mut self.arms[i];
        a.reward_sum = a.reward_sum * f + u * dwell_s;
        a.dwell_s = a.dwell_s * f + dwell_s;
        a.t_stats_ns = now_ns;
        a.last_reward = Some(u);
        self.counters.outcomes += 1;
        true
    }

    /// Takes the provider's snapshot and re-packs the arm table (allocates).
    pub(crate) fn repack(
        &mut self,
        plan: &CompiledPlan,
        cfg: &SchedulerConfig,
        caps: &SourceCapabilities,
        now_ns: i64,
    ) {
        let set = self.provider.snapshot();
        self.seen_version = Some(set.version);
        self.counters.repacks += 1;
        let ban_ns = (self.cfg.suspect_ban_s * NS) as i64;
        for v in &mut self.verifs {
            v.seen = false;
        }
        let mut items = Vec::with_capacity(set.candidates.len());
        for c in &set.candidates {
            let key = subject_key(&c.subject);
            let flagged = c.needs_verification;
            let (center_hz, bandwidth_hz) = (c.freq.center_hz(), c.freq.width_hz());
            match self.verifs.iter().position(|v| v.key == key) {
                Some(i) => {
                    let v = &mut self.verifs[i];
                    v.seen = true;
                    v.center_hz = center_hz;
                    v.bandwidth_hz = bandwidth_hz;
                    match v.state {
                        VerifyState::Banned { until_ns } if until_ns > now_ns => continue,
                        VerifyState::Banned { .. } if flagged => {
                            v.state = VerifyState::Pending;
                            continue;
                        }
                        VerifyState::Banned { .. } => v.state = VerifyState::Passed,
                        VerifyState::Done { at_ns } if flagged => {
                            if set.t.as_unix_nanos() > at_ns {
                                v.state = VerifyState::Banned {
                                    until_ns: now_ns + ban_ns,
                                };
                                self.counters.verifications_failed += 1;
                            }
                            continue;
                        }
                        VerifyState::Pending | VerifyState::Running if flagged => continue,
                        VerifyState::Done { .. } => {
                            v.state = VerifyState::Passed;
                            self.counters.verifications_passed += 1;
                        }
                        VerifyState::Pending | VerifyState::Running => {
                            v.state = VerifyState::Passed;
                        }
                        VerifyState::Passed => {}
                    }
                }
                None if flagged => {
                    if self.verifs.len() < MAX_VERIFICATIONS {
                        self.verifs.push(VerifyRec {
                            key,
                            center_hz,
                            bandwidth_hz,
                            state: VerifyState::Pending,
                            seen: true,
                        });
                    }
                    continue;
                }
                None => {}
            }
            if !(c.freq.hi_hz > c.freq.lo_hz && caps.supports_frequency(center_hz)) {
                continue;
            }
            items.push(PackItem {
                key,
                lo_hz: c.freq.lo_hz,
                hi_hz: c.freq.hi_hz,
                score_norm: c.score_norm,
                suspect_fraction: c.suspect_fraction,
                expected_interval_s: c.expected_interval_s,
                min_on_off_s: c.min_on_off_s,
                next_eta_ns: c.next_burst_eta.map(|t| t.as_unix_nanos()),
            });
        }
        self.verifs.retain(|v| {
            v.seen
                || matches!(v.state, VerifyState::Banned { until_ns } if until_ns > now_ns)
                || v.state == VerifyState::Running
        });

        let rate_hz = pick_rate(
            caps,
            cfg.sweep_rate_hz.max(cfg.dwell_min_rate_hz),
            cfg.rate_quantum_hz,
        );
        let usable_hz = (rate_hz * cfg.usable_fraction).min(cfg.max_span_hz);
        let bounds = cfg.rf_path_boundaries(caps);
        let q = self.cfg.arm_quantum_hz;
        let geo = Geometry {
            usable_hz,
            quantum_hz: q,
            dc_guard_hz: DC_GUARD_HZ,
            bounds,
            caps,
        };
        for a in &mut self.arms {
            a.active = false;
        }
        let max_arms = usize::from(self.cfg.max_arms);
        let bcfg = self.cfg;
        let candidate_dwell_ns = |interval: Option<f64>| (bcfg.dwell_s(interval) * NS) as i64;
        for p in pack::pack(&items, &geo) {
            let key = arm_key(p.rf_path, p.center_hz, rate_hz, q);
            let dwell_ns = candidate_dwell_ns(p.expected_interval_s);
            let fields = |a: &mut Arm| {
                a.center_hz = p.center_hz;
                a.rate_hz = rate_hz;
                a.rf_path = p.rf_path;
                a.exploration = false;
                a.on_dc = p.on_dc;
                a.prior = p.prior;
                a.suspect_fraction = p.suspect_fraction;
                a.lead = Some(p.lead);
                a.members = p.members;
                a.required_revisit_s = p.required_revisit_s;
                a.next_eta_ns = p.next_eta_ns;
                a.dwell_ns = dwell_ns;
            };
            if !self.upsert(key, now_ns, max_arms, fields) {
                self.counters.arms_dropped += 1;
            }
        }
        // One exploration arm per discovery hop not covered by a candidate window.
        let n = plan.hops.len();
        let explore_dwell_ns = candidate_dwell_ns(None);
        let half = usable_hz / 2.0;
        let windows: Vec<(u8, f64)> = self
            .arms
            .iter()
            .filter(|a| a.active)
            .map(|a| (a.rf_path, a.center_hz))
            .collect();
        let mut active = windows.len();
        let mut k = 0;
        while k < n {
            let i = (self.explore_cursor + k) % n;
            k += 1;
            let hop = &plan.hops[i];
            if hop.kind != HopKind::Sweep {
                continue;
            }
            if active >= max_arms {
                self.explore_cursor = i;
                break;
            }
            let covered = windows.iter().any(|&(path, c)| {
                path == hop.rf_path && c - half <= hop.covers.lo_hz && hop.covers.hi_hz <= c + half
            });
            if covered {
                continue;
            }
            let key = arm_key(hop.rf_path, hop.center_hz, rate_hz, q);
            if windows
                .iter()
                .any(|&(path, c)| arm_key(path, c, rate_hz, q) == key)
            {
                continue;
            }
            let path = rf_path(bounds, hop.center_hz);
            active += 1;
            self.upsert(key, now_ns, max_arms, |a| {
                a.center_hz = hop.center_hz;
                a.rate_hz = rate_hz;
                a.rf_path = path;
                a.exploration = true;
                a.on_dc = false;
                a.prior = 0.0;
                a.suspect_fraction = 0.0;
                a.lead = None;
                a.members = 0;
                a.required_revisit_s = None;
                a.next_eta_ns = None;
                a.dwell_ns = explore_dwell_ns;
            });
        }
        // Complete capture: feasible revisits, tightest first dropped until the demand fits.
        let budget = 1.0 - self.cfg.sweep_floor;
        let min_dwell = self.cfg.min_dwell_s;
        let mut demand = 0.0;
        for a in &mut self.arms {
            a.complete_capture =
                a.active && a.required_revisit_s.is_some_and(|r| r >= min_dwell) && !a.exploration;
            if a.complete_capture {
                demand += a.dwell_ns as f64 / NS / a.required_revisit_s.unwrap_or(f64::INFINITY);
            }
        }
        while demand > budget {
            let Some(i) = self
                .arms
                .iter()
                .enumerate()
                .filter(|(_, a)| a.complete_capture)
                .min_by(|x, y| {
                    x.1.required_revisit_s
                        .partial_cmp(&y.1.required_revisit_s)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(x.1.key.cmp(&y.1.key))
                })
                .map(|(i, _)| i)
            else {
                break;
            };
            let a = &mut self.arms[i];
            a.complete_capture = false;
            demand -= a.dwell_ns as f64 / NS / a.required_revisit_s.unwrap_or(f64::INFINITY);
        }
    }

    /// Updates the arm with `key` or inserts it (evicting the stalest inactive arm when full).
    fn upsert(
        &mut self,
        key: ArmKey,
        now_ns: i64,
        max_arms: usize,
        set: impl FnOnce(&mut Arm),
    ) -> bool {
        let i = match self.arms.iter().position(|a| a.key == key) {
            Some(i) => i,
            None => {
                let fresh = Arm {
                    key,
                    center_hz: 0.0,
                    rate_hz: 0.0,
                    rf_path: 0,
                    active: false,
                    exploration: false,
                    on_dc: false,
                    prior: 0.0,
                    suspect_fraction: 0.0,
                    lead: None,
                    members: 0,
                    required_revisit_s: None,
                    complete_capture: false,
                    next_eta_ns: None,
                    dwell_ns: 0,
                    reward_sum: 0.0,
                    dwell_s: 0.0,
                    t_stats_ns: now_ns,
                    visits: 0,
                    last_visit_ns: None,
                    added_ns: now_ns,
                    last_reward: None,
                };
                if self.arms.len() < max_arms {
                    self.arms.push(fresh);
                    self.arms.len() - 1
                } else {
                    let victim = self
                        .arms
                        .iter()
                        .enumerate()
                        .filter(|(_, a)| !a.active)
                        .fold(None::<usize>, |best, (i, a)| match best {
                            Some(b) if !Self::staler(a, &self.arms[b]) => Some(b),
                            _ => Some(i),
                        });
                    let Some(v) = victim else {
                        return false;
                    };
                    self.arms[v] = fresh;
                    v
                }
            }
        };
        let a = &mut self.arms[i];
        set(a);
        a.active = true;
        true
    }

    pub(crate) fn status(&self, now_ns: i64) -> BanditStatus {
        BanditStatus {
            provider_version: self.seen_version,
            arms: self.arms.len(),
            active_arms: self.arms.iter().filter(|a| a.active).count(),
            pending_verifications: self
                .verifs
                .iter()
                .filter(|v| v.state == VerifyState::Pending)
                .count(),
            banned: self
                .verifs
                .iter()
                .filter(
                    |v| matches!(v.state, VerifyState::Banned { until_ns } if until_ns > now_ns),
                )
                .count(),
            total_dwell_s: self.total_dwell_s * self.decay(self.total_t_ns, now_ns),
            config: self.cfg,
            counters: self.counters,
        }
    }

    pub(crate) fn arm_table(&self, now_ns: i64) -> Vec<ArmStatus> {
        self.arms
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let (mean, _) = self.mean_and_dwell(a, now_ns);
                ArmStatus {
                    index: i as u32,
                    key: a.key,
                    center_hz: a.center_hz,
                    rate_hz: a.rate_hz,
                    active: a.active,
                    exploration: a.exploration,
                    on_dc: a.on_dc,
                    prior: a.prior,
                    mean_reward: mean,
                    dwell_s: a.dwell_s * self.decay(a.t_stats_ns, now_ns),
                    ucb: self.index(a, now_ns),
                    visits: a.visits,
                    staleness_s: Self::since_ns(a, now_ns) as f64 / NS,
                    suspect_fraction: a.suspect_fraction,
                    lead: a.lead,
                    members: a.members,
                    dwell_planned_s: a.dwell_ns as f64 / NS,
                    required_revisit_s: a.required_revisit_s,
                    complete_capture: a.complete_capture,
                    last_reward: a.last_reward,
                }
            })
            .collect()
    }
}
