//! From predicted passes to C04 reservations (T-276).
//!
//! # What a pass reservation says
//!
//! It says **where to point the radio and when**, nothing about what will be found there: the
//! dwell it books runs the ordinary blind detection chain, and a pass that turns out silent is a
//! finding, not a failure. The downlink band comes from the caller's [`Downlink`] list (the
//! satellites the user asked to catch, or a SatNOGS-DB extract later), never from a lookup that
//! pre-populates the inventory.
//!
//! # TLE age is reported, never silently trusted
//!
//! Each pass carries its element-set epoch; its age at AOS sets:
//! - a [`Freshness`] verdict — `Fresh` ≤ [`PassPlanConfig::aging_after_days`] <
//!   `Aging` ≤ [`PassPlanConfig::stale_after_days`] < `Stale`;
//! - a timing margin added before AOS and after LOS, growing with age
//!   (`margin_base_s + margin_per_day_s · age`), because along-track error grows with age;
//! - a `Stale` pass is listed but **not reserved** (disposition [`Disposition::StaleTle`], with
//!   its age) unless [`PassPlanConfig::reserve_stale`] is set, and even then it is reserved with
//!   its age-widened margin and still marked stale.
//!
//! The feed-level cache age (when the TLE snapshot was fetched, whether the last refresh failed)
//! is reported beside the plan by [`crate::feeds::tle::plan_from_cache`].
//!
//! # Overlapping passes (the pre-emption rule between passes)
//!
//! The scheduler shares the radio round-robin between concurrent leases, which would halve both
//! of two overlapping passes. So overlaps are resolved here, before reserving, greedily in
//! priority order — **higher maximum elevation first, then earlier AOS, then lower NORAD id**:
//! - a pass whose window overlaps nothing booked gets its own reservation;
//! - one that overlaps exactly one booked reservation and whose downlink (± Doppler) fits in that
//!   reservation's usable span with the others is **merged** into it: the window grows to the
//!   union and the centre moves to the middle of the downlinks, so both are received at once
//!   (e.g. several 137 MHz weather satellites in one 2.4 Msps window);
//! - otherwise it **loses** to the booked reservation and is reported as
//!   [`Disposition::LostTo`] — a conflict is never hidden.
//!
//! Above all pass reservations stands interactive intent (see `hk_core::scheduler::Reservation`).

use hk_core::scheduler::{Lease, Reservation};
use hk_model::attention::observation::LeaseKind;
use hk_model::{TimeRange, Timestamp};
use serde::{Deserialize, Serialize};

use super::predict::Pass;

/// Tag in the top bits of every pass lease id, so they never collide with API-assigned leases.
pub const PASS_LEASE_TAG: u64 = 0x7A55 << 48;

/// Lease id of the reservation led by `pass`: stable across re-planning with the same prediction
/// (TCA minute and NORAD id), so re-reserving replaces instead of duplicating.
pub fn pass_lease_id(pass: &Pass) -> u64 {
    let minute = pass.tca.as_unix_nanos().div_euclid(60_000_000_000) as u64;
    PASS_LEASE_TAG | (u64::from(pass.norad_id) & 0xFF_FFFF) << 24 | (minute & 0xFF_FFFF)
}

/// A satellite downlink the user wants received during its passes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Downlink {
    /// NORAD catalogue number.
    pub norad_id: u32,
    /// Carrier, Hz.
    pub center_hz: f64,
    /// Occupied bandwidth, Hz (before Doppler).
    pub bandwidth_hz: f64,
}

/// Planning settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PassPlanConfig {
    /// Passes culminating below this are not reserved, degrees.
    pub min_max_elevation_deg: f64,
    /// Timing margin at zero TLE age, s (before AOS and after LOS).
    pub margin_base_s: f64,
    /// Extra margin per day of TLE age, s.
    pub margin_per_day_s: f64,
    /// Older than this (days at AOS) is `Aging`.
    pub aging_after_days: f64,
    /// Older than this is `Stale`: listed, not reserved unless `reserve_stale`.
    pub stale_after_days: f64,
    /// Reserve stale passes anyway (with the widened margin, still marked stale).
    pub reserve_stale: bool,
    /// Sample rate of pass reservations, Hz.
    pub rate_hz: f64,
    /// Fraction of `rate_hz` usable for downlinks (the rest is filter roll-off).
    pub usable_fraction: f64,
}

impl Default for PassPlanConfig {
    fn default() -> Self {
        Self {
            min_max_elevation_deg: 10.0,
            margin_base_s: 15.0,
            margin_per_day_s: 3.0,
            aging_after_days: 3.0,
            stale_after_days: 14.0,
            reserve_stale: false,
            rate_hz: 2.4e6,
            usable_fraction: 0.8,
        }
    }
}

/// How far the element set can be trusted for this pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Freshness {
    /// Within `aging_after_days`.
    Fresh,
    /// Usable with a wider margin.
    Aging,
    /// Past `stale_after_days`: timing is not trusted.
    Stale,
}

/// What the planner did with a pass.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Disposition {
    /// It leads reservation `lease_id`.
    Reserved {
        /// Lease id of the reservation.
        lease_id: u64,
    },
    /// Received inside another pass's reservation (same window, downlinks fit together).
    SharesWindow {
        /// Lease id of the reservation.
        lease_id: u64,
    },
    /// Overlaps a higher-priority reservation it cannot share: not reserved.
    LostTo {
        /// The reservation that won.
        lease_id: u64,
        /// Its leading satellite.
        norad_id: u32,
    },
    /// Culminates below `min_max_elevation_deg`.
    BelowElevation,
    /// Element set older than `stale_after_days` at AOS (and `reserve_stale` is off).
    StaleTle,
    /// No downlink configured for the satellite.
    NoDownlink,
    /// The pass window ended before the plan's `now`.
    Past,
}

/// One pass and its verdict.
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedPass {
    /// The prediction.
    pub pass: Pass,
    /// Element-set age at AOS, days.
    pub tle_age_days: f64,
    /// Freshness verdict.
    pub freshness: Freshness,
    /// Margin added on both sides, s.
    pub margin_s: f64,
    /// `[AOS − margin, LOS + margin]`.
    pub window: TimeRange,
    /// The downlink it would be received on.
    pub downlink: Option<Downlink>,
    /// What was done with it.
    pub disposition: Disposition,
}

/// The plan: every pass with its verdict, and the reservations to hand the scheduler.
#[derive(Clone, Debug, PartialEq)]
pub struct PassPlan {
    /// All passes, in AOS order.
    pub passes: Vec<PlannedPass>,
    /// Reservations, in start order.
    pub reservations: Vec<Reservation>,
}

impl PassPlan {
    /// The oldest element set used by any reserved pass, days (`None` if nothing is reserved).
    pub fn oldest_reserved_tle_days(&self) -> Option<f64> {
        self.passes
            .iter()
            .filter(|p| {
                matches!(
                    p.disposition,
                    Disposition::Reserved { .. } | Disposition::SharesWindow { .. }
                )
            })
            .map(|p| p.tle_age_days)
            .reduce(f64::max)
    }
}

struct Booking {
    lease_id: u64,
    norad_id: u32,
    window: TimeRange,
    lo_hz: f64,
    hi_hz: f64,
}

fn overlaps(a: &TimeRange, b: &TimeRange) -> bool {
    a.start < b.end && b.start < a.end
}

/// Plans `passes` (any order) for reservation at or after `now`.
pub fn plan_passes(
    passes: &[Pass],
    downlinks: &[Downlink],
    now: Timestamp,
    cfg: &PassPlanConfig,
) -> PassPlan {
    let mut planned: Vec<PlannedPass> = passes
        .iter()
        .map(|pass| {
            let age = pass.tle_age_days();
            let freshness = if age > cfg.stale_after_days {
                Freshness::Stale
            } else if age > cfg.aging_after_days {
                Freshness::Aging
            } else {
                Freshness::Fresh
            };
            let margin_s = cfg.margin_base_s + cfg.margin_per_day_s * age;
            let margin_ns = (margin_s * 1e9) as i64;
            let window = TimeRange::new(
                pass.aos.saturating_add_nanos(-margin_ns),
                pass.los.saturating_add_nanos(margin_ns),
            );
            let downlink = downlinks
                .iter()
                .find(|d| d.norad_id == pass.norad_id)
                .cloned();
            let disposition = if window.end <= now {
                Disposition::Past
            } else if pass.max_elevation_deg < cfg.min_max_elevation_deg {
                Disposition::BelowElevation
            } else if downlink.is_none() {
                Disposition::NoDownlink
            } else if freshness == Freshness::Stale && !cfg.reserve_stale {
                Disposition::StaleTle
            } else {
                // Provisional; settled below.
                Disposition::Reserved { lease_id: 0 }
            };
            PlannedPass {
                pass: pass.clone(),
                tle_age_days: age,
                freshness,
                margin_s,
                window,
                downlink,
                disposition,
            }
        })
        .collect();
    planned.sort_by_key(|p| (p.pass.aos, p.pass.norad_id));

    let mut order: Vec<usize> = (0..planned.len())
        .filter(|&i| matches!(planned[i].disposition, Disposition::Reserved { .. }))
        .collect();
    order.sort_by(|&a, &b| {
        let (pa, pb) = (&planned[a].pass, &planned[b].pass);
        pb.max_elevation_deg
            .total_cmp(&pa.max_elevation_deg)
            .then(pa.aos.cmp(&pb.aos))
            .then(pa.norad_id.cmp(&pb.norad_id))
    });
    let usable = cfg.rate_hz * cfg.usable_fraction;
    let mut bookings: Vec<Booking> = Vec::new();
    for i in order {
        let p = &planned[i];
        let d = p.downlink.as_ref().expect("filtered on downlink");
        let half = d.bandwidth_hz / 2.0 + p.pass.max_doppler_hz(d.center_hz);
        let (lo, hi) = (d.center_hz - half, d.center_hz + half);
        let hits: Vec<usize> = (0..bookings.len())
            .filter(|&b| overlaps(&bookings[b].window, &p.window))
            .collect();
        let disposition = match hits.as_slice() {
            [] => {
                let lease_id = pass_lease_id(&p.pass);
                bookings.push(Booking {
                    lease_id,
                    norad_id: p.pass.norad_id,
                    window: p.window,
                    lo_hz: lo,
                    hi_hz: hi,
                });
                Disposition::Reserved { lease_id }
            }
            &[b] => {
                let bk = &bookings[b];
                let (nlo, nhi) = (bk.lo_hz.min(lo), bk.hi_hz.max(hi));
                let union = TimeRange::new(
                    bk.window.start.min(p.window.start),
                    bk.window.end.max(p.window.end),
                );
                let clear = bookings
                    .iter()
                    .enumerate()
                    .all(|(k, o)| k == b || !overlaps(&o.window, &union));
                if nhi - nlo <= usable && clear {
                    let bk = &mut bookings[b];
                    (bk.lo_hz, bk.hi_hz, bk.window) = (nlo, nhi, union);
                    Disposition::SharesWindow {
                        lease_id: bk.lease_id,
                    }
                } else {
                    Disposition::LostTo {
                        lease_id: bk.lease_id,
                        norad_id: bk.norad_id,
                    }
                }
            }
            many => {
                let bk = &bookings[many[0]];
                Disposition::LostTo {
                    lease_id: bk.lease_id,
                    norad_id: bk.norad_id,
                }
            }
        };
        planned[i].disposition = disposition;
    }

    let mut reservations: Vec<Reservation> = bookings
        .iter()
        .map(|b| {
            let start = b.window.start.max(now);
            Reservation {
                lease: Lease {
                    id: b.lease_id,
                    kind: LeaseKind::Pass,
                    center_hz: (b.lo_hz + b.hi_hz) / 2.0,
                    rate_hz: cfg.rate_hz,
                    gains: None,
                    duration_ns: Some(b.window.end.as_unix_nanos() - start.as_unix_nanos()),
                },
                start,
            }
        })
        .collect();
    reservations.sort_by_key(|r| (r.start, r.lease.id));
    PassPlan {
        passes: planned,
        reservations,
    }
}
