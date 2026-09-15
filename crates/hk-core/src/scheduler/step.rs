//! Schedule output: one [`ScheduleStep`] per window the single radio points at.

use hk_model::Timestamp;
use hk_model::attention::observation::{LeaseKind, Reason, Tier, TrustTestKind};

use super::bandit::BanditKind;

use crate::source::Gains;

/// Caller-chosen key of a point of interest (e.g. a T-006/T-007 confirmation or track id).
pub type PoiKey = u64;

/// Which gain state of an interleaved gain-step pair (S4 rule 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GainSlot {
    /// The base state: the plan's gain table at the POI.
    A,
    /// The stepped state: LNA ± `gain_step_lna_db`.
    B,
}

/// Why the window points where it does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Purpose {
    /// A discovery sweep hop ([`super::CompiledPlan::hops`] index).
    Sweep {
        /// Hop index in pass order.
        hop: u32,
    },
    /// One window of a dwell-only region, visited in the discovery pass.
    RegionDwell {
        /// Hop index in pass order.
        hop: u32,
    },
    /// A dwell sized to a point of interest. Inside a verification group without gain-step
    /// stages it is the baseline capture for the retune comparison.
    Dwell {
        /// The POI.
        poi: PoiKey,
    },
    /// One block of an interleaved gain-step dwell (A/B × pairs).
    GainStep {
        /// The POI.
        poi: PoiKey,
        /// Pair index, 0-based.
        pair: u8,
        /// State A or B.
        slot: GainSlot,
    },
    /// A retune-test dwell: same gains, centre moved by `delta_hz` (S4 rule 7).
    Retune {
        /// The POI.
        poi: PoiKey,
        /// Centre offset from the POI dwell centre, Hz.
        delta_hz: f64,
    },
    /// A sample-rate-change dwell: same centre and gains, another rate (clock-harmonic test).
    RateChange {
        /// The POI.
        poi: PoiKey,
        /// The POI dwell's rate, Hz (the step's own rate is the changed one).
        base_rate_hz: f64,
    },
    /// Explicit user intent, which preempts everything else.
    UserIntent {
        /// Caller's intent id.
        intent: u64,
    },
    /// A bandit dwell on an arm window (T-120, ADR-0012 §5).
    Bandit {
        /// Arm table index.
        arm: u32,
        /// The arm's lead candidate (subject key), if any.
        lead: Option<PoiKey>,
        /// Exploit, explore or beacon-due.
        kind: BanditKind,
    },
    /// A slice of a pinned lease.
    Lease {
        /// Kind.
        kind: LeaseKind,
        /// Caller's lease id.
        lease: u64,
    },
    /// A scheduled-plan dwell (plan revisit target).
    Scheduled {
        /// Caller's id.
        target: u32,
    },
}

impl Purpose {
    /// The POI this step serves, if any.
    pub fn poi(&self) -> Option<PoiKey> {
        match *self {
            Purpose::Dwell { poi }
            | Purpose::GainStep { poi, .. }
            | Purpose::Retune { poi, .. }
            | Purpose::RateChange { poi, .. } => Some(poi),
            Purpose::Bandit { lead, .. } => lead,
            Purpose::Sweep { .. }
            | Purpose::RegionDwell { .. }
            | Purpose::UserIntent { .. }
            | Purpose::Lease { .. }
            | Purpose::Scheduled { .. } => None,
        }
    }

    /// A discovery-pass step (sweep hop or dwell-only region window).
    pub fn is_discovery(&self) -> bool {
        matches!(self, Purpose::Sweep { .. } | Purpose::RegionDwell { .. })
    }

    /// A trust-test step (gain step, retune or rate change).
    pub fn is_trust_test(&self) -> bool {
        matches!(
            self,
            Purpose::GainStep { .. } | Purpose::Retune { .. } | Purpose::RateChange { .. }
        )
    }

    /// Short stable name, e.g. for logs and golden files.
    pub fn name(&self) -> &'static str {
        match self {
            Purpose::Sweep { .. } => "sweep",
            Purpose::RegionDwell { .. } => "region-dwell",
            Purpose::Dwell { .. } => "dwell",
            Purpose::GainStep { .. } => "gain-step",
            Purpose::Retune { .. } => "retune",
            Purpose::RateChange { .. } => "rate-change",
            Purpose::UserIntent { .. } => "user-intent",
            Purpose::Bandit { .. } => "bandit",
            Purpose::Lease { .. } => "lease",
            Purpose::Scheduled { .. } => "scheduled",
        }
    }
}

impl Purpose {
    /// The ADR-0012 §1.2 reason code of this purpose, carried into the observation log. T-120 adds
    /// the bandit, lease and scheduled purposes.
    pub fn reason(&self) -> Reason {
        match *self {
            Purpose::Sweep { hop } => Reason::BackgroundSweep { hop },
            Purpose::RegionDwell { hop } => Reason::RegionDwell { hop },
            Purpose::Dwell { poi } => Reason::PoiDwell { poi },
            Purpose::GainStep { poi, .. } => Reason::Verification {
                poi,
                test: TrustTestKind::GainStep,
            },
            Purpose::Retune { poi, .. } => Reason::Verification {
                poi,
                test: TrustTestKind::Retune,
            },
            Purpose::RateChange { poi, .. } => Reason::Verification {
                poi,
                test: TrustTestKind::RateChange,
            },
            Purpose::UserIntent { intent } => Reason::Interactive { intent },
            Purpose::Bandit { arm, kind, .. } => match kind {
                BanditKind::Exploit { score } => Reason::Novelty { arm, score },
                BanditKind::Explore => Reason::Explore { arm },
                BanditKind::BeaconDue { eta_s } => Reason::BeaconDue { arm, eta_s },
            },
            Purpose::Lease { kind, lease } => Reason::Lease { kind, lease },
            Purpose::Scheduled { target } => Reason::RevisitDue { region: target },
        }
    }

    /// The ADR-0012 §5.4 preemption tier this purpose runs at.
    pub fn tier(&self) -> Tier {
        self.reason().tier()
    }
}

/// One scheduled window: where the radio points from `t_start` for `duration_ns`.
///
/// `Copy` and allocation-free, so emitting a step costs nothing in the steady state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScheduleStep {
    /// Monotonic step number within this scheduler.
    pub seq: u64,
    /// Start time (the scheduler clock's `now` when the step was emitted).
    pub t_start: Timestamp,
    /// Planned duration, ns. A preemption can cut it short.
    pub duration_ns: i64,
    /// Tuned centre, Hz. POI dwells are offset-tuned so the emitter clears DC.
    pub center_hz: f64,
    /// Sample rate, Hz.
    pub rate_hz: f64,
    /// Baseband filter bandwidth, Hz; `None` when the source has no selectable filter.
    pub baseband_filter_hz: Option<f64>,
    /// Front-end gains.
    pub gains: Gains,
    /// Index of the plan gain-table entry that set the gains (its `antenna_port`), if any.
    pub gain_entry: Option<u16>,
    /// The band uses an accessory or filter port (gain-table `antenna_port`): detection trust
    /// near notch / filter-bank edges is pending T-028, so don't rely on it here yet.
    pub accessory: bool,
    /// RF path of the tuned centre: the number of `rf_path_boundaries_hz` at or below it. The
    /// noise floor steps between paths, so floors are keyed by it and never pooled across it.
    pub rf_path: u8,
    /// Why.
    pub purpose: Purpose,
    /// ScanPlan version the step was planned under.
    pub plan_version: u32,
    /// Verification group of a trust-test or baseline step: the `seq` of the group's first step.
    /// A cut or plan update restarts a group under a new (larger) id, so captures of different
    /// groups never pair ([`super::Verification`]). `None` outside verification groups.
    pub verification_group: Option<u64>,
    /// T-181: a discovery step of a DC-dithered pass ([`super::Hop::dithered_on_pass`]), whether
    /// or not its own hop could move. Observation records pick the pass geometry by it, so a hop
    /// left undithered never splits a pass's record. `false` for every other purpose.
    pub dither_pass: bool,
}

impl ScheduleStep {
    /// Planned end time.
    pub fn t_end(&self) -> Timestamp {
        self.t_start.saturating_add_nanos(self.duration_ns)
    }
}
