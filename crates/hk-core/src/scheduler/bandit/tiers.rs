//! Preemption tiers above the bandit (ADR-0012 §5.4): pinned leases and scheduled-plan dwells,
//! plus the attention status the API discloses.

use hk_model::Timestamp;
use hk_model::attention::observation::LeaseKind;

use super::BanditStatus;
use crate::source::Gains;

/// Most concurrent leases.
pub const MAX_LEASES: usize = 16;

/// Most scheduled-plan dwells.
pub const MAX_SCHEDULED: usize = 32;

/// Most pending reservations.
pub const MAX_RESERVATIONS: usize = 32;

/// A pinned lease: a user "watch this" pin, a decoder/trunking lease or a pass/launch window. It
/// preempts scheduled plans, the bandit and the sweep until it ends or is released; only
/// interactive intent ranks above it. Concurrent leases share the radio in round-robin slices.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Lease {
    /// Caller's lease id; adding the same id again updates it.
    pub id: u64,
    /// Kind.
    pub kind: LeaseKind,
    /// Centre, Hz.
    pub center_hz: f64,
    /// Sample rate, Hz.
    pub rate_hz: f64,
    /// Gains; `None` takes the plan's gain table.
    pub gains: Option<Gains>,
    /// Duration, ns; `None` holds until released.
    pub duration_ns: Option<i64>,
}

/// A scheduled-plan dwell (a plan revisit target or a cron-like action): due at `due`, repeating
/// every `every_ns` if set. It takes the next step boundary once due (it never cuts a running
/// step or verification group) and ranks above the bandit and the sweep.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScheduledDwell {
    /// Caller's id (the `region` of its `revisit-due` reason); adding it again replaces it.
    pub id: u32,
    /// Centre, Hz.
    pub center_hz: f64,
    /// Sample rate, Hz.
    pub rate_hz: f64,
    /// Gains; `None` takes the plan's gain table.
    pub gains: Option<Gains>,
    /// Dwell length, ns.
    pub duration_ns: i64,
    /// First due time (scheduler clock).
    pub due: Timestamp,
    /// Repeat period, ns.
    pub every_ns: Option<i64>,
}

/// A pinned lease reserved ahead of time for a predicted window (T-276): a satellite pass from
/// cached TLEs or a 00Z/12Z launch window (C29 → C04). Pre-emption, stated:
///
/// - **Before `start` the window is kept free.** Every step below interactive intent that would
///   run past `start` is clipped to end there. A clipped slot (discovery hop, bandit/POI dwell,
///   scheduled dwell, verification group) is rolled back when the reservation begins, exactly as a
///   cut by [`crate::scheduler::Scheduler::add_lease`] is, so it is revisited in full later.
///   Reserving a window that starts inside the running step trims that step
///   ([`crate::scheduler::Scheduler::running_end`] moves).
/// - **From `start` to `start + duration` it is a pinned lease**: above scheduled plans, the bandit
///   and the sweep; a running verification group is cut. It ends at `start + duration` however
///   late it began, so a late start never pushes the window past LOS.
/// - **Equal tier with other leases**: user pins, decoder/trunking leases and other reservations
///   active at once share the radio in round-robin slices. A planner that must not split a pass
///   resolves overlapping windows before reserving them.
/// - **Interactive intent outranks it.** Intent steps are never clipped; the reservation begins at
///   the first step boundary after `start` (at most one intent slice late) and the pass time the
///   user took is lost, counted in `ScheduleStats::reservations_late`.
/// - A reservation that cannot begin (lease table full) stays pending and is retried every step;
///   one whose window elapses unstarted is dropped and counted in
///   `ScheduleStats::reservations_missed`, never silently.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reservation {
    /// The lease it becomes at `start`; `duration_ns` is required. Lease ids are one space:
    /// reserving the id of an active lease replaces that lease when the window begins.
    pub lease: Lease,
    /// Window start (scheduler clock), e.g. AOS minus the prediction margin.
    pub start: Timestamp,
}

impl Reservation {
    /// Window end, `start + duration` (`start` if the lease has no duration, which
    /// [`crate::scheduler::Scheduler::reserve`] refuses).
    pub fn end(&self) -> Timestamp {
        self.start
            .saturating_add_nanos(self.lease.duration_ns.unwrap_or(0))
    }
}

/// Radio-time shares over the sweep-floor window and what holds the radio (`GET /api/scheduler`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AttentionStatus {
    /// Scheduler clock.
    pub now: Timestamp,
    /// Window, s.
    pub window_s: f64,
    /// Discovery (sweep hops and dwell-only region windows), s.
    pub discovery_s: f64,
    /// Bandit exploitation, POI dwells and verification, s.
    pub exploit_s: f64,
    /// Bandit exploration, s.
    pub explore_s: f64,
    /// Scheduled plans, leases and interactive intent, s.
    pub other_s: f64,
    /// Sweep floor in force (with the bandit enabled; raised in low-power mode).
    pub sweep_floor: Option<f64>,
    /// Discovery holds at least the floor over the window (or no floor applies).
    pub sweep_floor_met: bool,
    /// Steps emitted by higher tiers while discovery was below the floor (disclosed, not hidden).
    pub floor_violations: u64,
    /// Interactive intent holds the radio.
    pub interactive: bool,
    /// Active leases.
    pub leases: usize,
    /// Pending scheduled dwells.
    pub scheduled: usize,
    /// Pending reservations (predicted pass/launch windows not yet begun, T-276).
    pub reservations: usize,
    /// Low-power profile.
    pub low_power: bool,
    /// Bandit summary, when enabled.
    pub bandit: Option<BanditStatus>,
}
