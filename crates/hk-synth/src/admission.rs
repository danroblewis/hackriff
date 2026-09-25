//! Admission, power and throttle (ADR-0015 §3.3, with U2 = B, user 2026-09-23).
//!
//! Three things live here, all pure or lock-free so the job manager (`hk-pipeline::synth`, M-8)
//! and the engine ([`crate::engine`]) share one statement of the rules:
//!
//! - [`PowerPolicy`] — the **minimal** power-policy input §3.3 says M-3 adds (no policy object
//!   existed in code): `mains` / `battery` / `low`. `battery` refuses `deep` and halves threads,
//!   `low` allows only `quick`.
//! - [`admit`] — one running job, a queue of ≤ 4, `503 busy` beyond; the auto-analyze rule
//!   (U2 = B: `quick` only, mains only, one auto job at a time, never displacing a user job).
//! - [`Control`] — the live inputs a running search polls between work units: cancel, the run's
//!   `lost_samples` counter (a rise throttles the job — threads halved — before capture is hurt),
//!   the thermal-throttle flag (pauses expansion) and the current power policy (a change to one
//!   that refuses the job's profile stops expansion, recorded as `refused_power`).
//!
//! **What is not here:** the `synth` chain kind and lower OS priority for search threads are the
//! runtime's (`hk-pipeline::synth`, ADR-0015 §9); this crate spawns scoped threads and cannot
//! reach the scheduler portably.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::search::{Profile, SynthBudget};

/// The run's power policy (ADR-0007/0009 input, minimal form; ADR-0015 §3.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerPolicy {
    /// External power: every profile, full threads.
    #[default]
    Mains,
    /// On battery: `deep` refused (`422 power`), threads halved.
    Battery,
    /// Low-power mode: `quick` only.
    Low,
}

impl PowerPolicy {
    /// Whether this policy admits `profile` at all.
    pub const fn allows(self, profile: Profile) -> bool {
        match self {
            PowerPolicy::Mains => true,
            PowerPolicy::Battery => !matches!(profile, Profile::Deep),
            PowerPolicy::Low => matches!(profile, Profile::Quick),
        }
    }

    /// The wire name (`refused_power.policy`).
    pub const fn as_str(self) -> &'static str {
        match self {
            PowerPolicy::Mains => "mains",
            PowerPolicy::Battery => "battery",
            PowerPolicy::Low => "low",
        }
    }

    /// `profile`'s budget under this policy, or `None` when refused. Battery halves threads
    /// (never below 1); `low` runs `quick` unchanged (it already has one thread).
    pub fn budget(self, profile: Profile) -> Option<SynthBudget> {
        if !self.allows(profile) {
            return None;
        }
        let mut b = profile.budget();
        if self == PowerPolicy::Battery {
            b.threads = halve(b.threads);
        }
        Some(b)
    }

    const fn to_u8(self) -> u8 {
        match self {
            PowerPolicy::Mains => 0,
            PowerPolicy::Battery => 1,
            PowerPolicy::Low => 2,
        }
    }

    const fn from_u8(v: u8) -> Self {
        match v {
            0 => PowerPolicy::Mains,
            1 => PowerPolicy::Battery,
            _ => PowerPolicy::Low,
        }
    }
}

/// Threads halved, never below one.
pub const fn halve(threads: u32) -> u32 {
    if threads <= 2 { 1 } else { threads / 2 }
}

/// Who asked for a job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// The user pressed Analyze (or the context menu).
    User,
    /// The attention scheduler auto-queued an unexplained candidate (U2).
    Auto,
}

/// The auto-analyze policy, `auto_profile: quick | none` (ADR-0015 §3.3, U2 = B).
///
/// The product default is `quick`; **this crate ships `none`** until the attention → queue
/// wiring exists (§3.3: "M-3 may ship it `none` until the attention→queue wiring and the
/// power-policy object exist, and flipping it is configuration, not code"; §16.7: "M-3's
/// `auto_profile` default, which starts off").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoProfile {
    /// Auto jobs run at `quick`.
    Quick,
    /// No auto jobs.
    #[default]
    None,
}

/// Most queued jobs behind the running one (§3.3).
pub const MAX_QUEUED: usize = 4;

/// What the job manager knows when a request arrives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Slots {
    /// The running job's origin, if one runs.
    pub running: Option<Origin>,
    /// Jobs waiting.
    pub queued: usize,
    /// An auto job is running or queued.
    pub auto_in_flight: bool,
}

/// Where an admitted job goes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Admitted {
    /// Its budget, already adjusted for the power policy.
    pub budget: SynthBudget,
    /// `true`: start now; `false`: queued. A user job queues **ahead of** every auto job (an auto
    /// job never displaces a user job) — the job manager keeps that order.
    pub start_now: bool,
}

/// Why a request was refused, with the HTTP status the API maps it to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    /// One running job and a full queue (`503 busy`).
    Busy,
    /// The power policy refuses this profile (`422 power`).
    Power,
    /// Auto-analyze is off (`auto_profile: none`).
    AutoDisabled,
    /// Auto jobs run at `quick` only.
    AutoProfile,
    /// Auto jobs run on mains only.
    AutoNotOnMains,
    /// One auto job at a time.
    AutoInFlight,
}

impl Refusal {
    /// The HTTP status (ADR-0015 §3.3 / §5.1). Auto refusals never reach HTTP (the scheduler
    /// asked, not a client); they map to `409` if a caller surfaces them.
    pub const fn http_status(self) -> u16 {
        match self {
            Refusal::Busy => 503,
            Refusal::Power => 422,
            Refusal::AutoDisabled
            | Refusal::AutoProfile
            | Refusal::AutoNotOnMains
            | Refusal::AutoInFlight => 409,
        }
    }
}

/// Admission (ADR-0015 §3.3): the power policy first, then the auto rules, then the slots.
pub fn admit(
    profile: Profile,
    origin: Origin,
    power: PowerPolicy,
    auto_profile: AutoProfile,
    slots: Slots,
) -> Result<Admitted, Refusal> {
    if origin == Origin::Auto {
        match auto_profile {
            AutoProfile::None => return Err(Refusal::AutoDisabled),
            AutoProfile::Quick if profile != Profile::Quick => return Err(Refusal::AutoProfile),
            AutoProfile::Quick => {}
        }
        if power != PowerPolicy::Mains {
            return Err(Refusal::AutoNotOnMains);
        }
        if slots.auto_in_flight {
            return Err(Refusal::AutoInFlight);
        }
    }
    let budget = power.budget(profile).ok_or(Refusal::Power)?;
    if slots.running.is_none() {
        return Ok(Admitted {
            budget,
            start_now: true,
        });
    }
    if slots.queued >= MAX_QUEUED {
        return Err(Refusal::Busy);
    }
    Ok(Admitted {
        budget,
        start_now: false,
    })
}

/// The live inputs a running search polls (shared with the runtime by `Arc`). All lock-free.
#[derive(Debug, Default)]
pub struct Control {
    cancel: AtomicBool,
    lost_samples: AtomicU64,
    thermal: AtomicBool,
    power: AtomicU8,
}

impl Control {
    /// A control on mains, nothing lost, not cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// A control starting under `power`.
    pub fn with_power(power: PowerPolicy) -> Self {
        let c = Self::default();
        c.set_power(power);
        c
    }

    /// Asks the search to stop; partial results are kept.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Whether cancel was asked.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Publishes the run's cumulative `lost_samples` counter. A rise while a job runs throttles
    /// it (threads halved).
    pub fn set_lost_samples(&self, total: u64) {
        self.lost_samples.fetch_max(total, Ordering::Relaxed);
    }

    /// The run's cumulative `lost_samples`, as last published.
    pub fn lost_samples(&self) -> u64 {
        self.lost_samples.load(Ordering::Relaxed)
    }

    /// Sets or clears the thermal-throttle flag; while set, expansion pauses.
    pub fn set_thermal(&self, throttled: bool) {
        self.thermal.store(throttled, Ordering::Relaxed);
    }

    /// Whether the thermal flag is set.
    pub fn thermal(&self) -> bool {
        self.thermal.load(Ordering::Relaxed)
    }

    /// Publishes the current power policy.
    pub fn set_power(&self, power: PowerPolicy) {
        self.power.store(power.to_u8(), Ordering::Relaxed);
    }

    /// The current power policy.
    pub fn power(&self) -> PowerPolicy {
        PowerPolicy::from_u8(self.power.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDLE: Slots = Slots {
        running: None,
        queued: 0,
        auto_in_flight: false,
    };

    #[test]
    fn battery_refuses_deep_and_halves_threads_low_allows_only_quick() {
        assert_eq!(
            admit(
                Profile::Deep,
                Origin::User,
                PowerPolicy::Battery,
                AutoProfile::None,
                IDLE
            ),
            Err(Refusal::Power)
        );
        assert_eq!(Refusal::Power.http_status(), 422);
        let a = admit(
            Profile::Standard,
            Origin::User,
            PowerPolicy::Battery,
            AutoProfile::None,
            IDLE,
        )
        .unwrap();
        assert_eq!(a.budget.threads, 1);
        assert!(a.start_now);
        for p in [Profile::Standard, Profile::Deep] {
            assert_eq!(
                admit(p, Origin::User, PowerPolicy::Low, AutoProfile::None, IDLE),
                Err(Refusal::Power)
            );
        }
        let q = admit(
            Profile::Quick,
            Origin::User,
            PowerPolicy::Low,
            AutoProfile::None,
            IDLE,
        )
        .unwrap();
        assert_eq!(q.budget, Profile::Quick.budget());
        assert_eq!(
            PowerPolicy::Mains.budget(Profile::Deep),
            Some(Profile::Deep.budget())
        );
        assert_eq!(halve(4), 2);
        assert_eq!(halve(1), 1);
    }

    #[test]
    fn one_running_job_a_queue_of_four_then_busy() {
        let mut slots = Slots {
            running: Some(Origin::User),
            ..IDLE
        };
        for queued in 0..MAX_QUEUED {
            slots.queued = queued;
            let a = admit(
                Profile::Quick,
                Origin::User,
                PowerPolicy::Mains,
                AutoProfile::None,
                slots,
            )
            .unwrap();
            assert!(!a.start_now);
        }
        slots.queued = MAX_QUEUED;
        let busy = admit(
            Profile::Quick,
            Origin::User,
            PowerPolicy::Mains,
            AutoProfile::None,
            slots,
        );
        assert_eq!(busy, Err(Refusal::Busy));
        assert_eq!(Refusal::Busy.http_status(), 503);
    }

    #[test]
    fn auto_jobs_are_quick_only_mains_only_one_at_a_time_and_off_by_default() {
        assert_eq!(AutoProfile::default(), AutoProfile::None);
        let auto = |p, power, ap, slots| admit(p, Origin::Auto, power, ap, slots);
        assert_eq!(
            auto(Profile::Quick, PowerPolicy::Mains, AutoProfile::None, IDLE),
            Err(Refusal::AutoDisabled)
        );
        assert_eq!(
            auto(
                Profile::Standard,
                PowerPolicy::Mains,
                AutoProfile::Quick,
                IDLE
            ),
            Err(Refusal::AutoProfile)
        );
        assert_eq!(
            auto(
                Profile::Quick,
                PowerPolicy::Battery,
                AutoProfile::Quick,
                IDLE
            ),
            Err(Refusal::AutoNotOnMains)
        );
        let in_flight = Slots {
            auto_in_flight: true,
            ..IDLE
        };
        assert_eq!(
            auto(
                Profile::Quick,
                PowerPolicy::Mains,
                AutoProfile::Quick,
                in_flight
            ),
            Err(Refusal::AutoInFlight)
        );
        assert!(auto(Profile::Quick, PowerPolicy::Mains, AutoProfile::Quick, IDLE).is_ok());
    }

    #[test]
    fn control_round_trips_and_lost_samples_only_rise() {
        let c = Control::with_power(PowerPolicy::Battery);
        assert_eq!(c.power(), PowerPolicy::Battery);
        c.set_lost_samples(10);
        c.set_lost_samples(4);
        assert_eq!(c.lost_samples(), 10);
        c.set_thermal(true);
        assert!(c.thermal());
        assert!(!c.is_cancelled());
        c.cancel();
        assert!(c.is_cancelled());
        for p in [PowerPolicy::Mains, PowerPolicy::Battery, PowerPolicy::Low] {
            assert_eq!(PowerPolicy::from_u8(p.to_u8()), p);
        }
    }
}
