//! Satellite passes from cached TLEs (C29), and the reservations they give the C04 attention
//! scheduler (T-276).
//!
//! C29 owns ephemerides: passes are computed on the device from cached element sets, offline
//! (docs/06 §5). The chain is
//! [`tle`] (parse a CelesTrak snapshot) → [`sgp4`] (near-earth SGP4) → [`look`] (TEME → Earth-fixed
//! → azimuth/elevation/range-rate from the [`crate::Site`]) → [`predict`] (AOS/TCA/LOS) →
//! [`plan`] (TLE-age verdict and margin, conflict resolution, `hk_core::scheduler::Reservation`s)
//! → `Scheduler::reserve`. The cached-feed entry point is [`crate::feeds::tle::plan_from_cache`].
//!
//! Each pass also becomes a docs/07 §2.17 [`ExternalEvent`] (`source` `tle-pass`, `event_type`
//! `pass`, [`Geo::OrbitPass`]) through [`pass_event`], so C30 can use pass windows as explanations.

pub mod look;
pub mod plan;
pub mod predict;
pub mod sgp4;
pub mod tle;

use hk_model::{ExternalEvent, FreqRange, Geo, TimeRange, Timestamp};
use serde_json::json;

pub use plan::{
    Disposition, Downlink, Freshness, PASS_LEASE_TAG, PassPlan, PassPlanConfig, PlannedPass,
    pass_lease_id, plan_passes,
};
pub use predict::{Pass, PredictConfig, Tracker, predict_all};
pub use sgp4::{Sgp4, Sgp4Error};
pub use tle::{Tle, parse_set};

/// ExternalEvent source of computed passes.
pub const SOURCE: &str = "tle-pass";
/// ExternalEvent type of a pass.
pub const EVENT_TYPE: &str = "pass";

/// A pass as a cached ExternalEvent: time AOS..LOS, [`Geo::OrbitPass`], the downlink ± Doppler as
/// its frequency extent (empty without one), and the element-set epoch and age in the payload.
/// `native_id` is `<norad>/<TCA unix minute>`; `valid_until` is LOS (a later prediction from a
/// fresher TLE supersedes it).
pub fn pass_event(
    pass: &Pass,
    downlink: Option<&Downlink>,
    computed_at: Timestamp,
) -> ExternalEvent {
    let minute = pass.tca.as_unix_nanos().div_euclid(60_000_000_000);
    let native_id = format!("{}/{minute}", pass.norad_id);
    let freq = downlink
        .map(|d| {
            let half = d.bandwidth_hz / 2.0 + pass.max_doppler_hz(d.center_hz);
            vec![FreqRange::new(d.center_hz - half, d.center_hz + half)]
        })
        .unwrap_or_default();
    ExternalEvent {
        id: crate::feeds::gpsjam::event_id(SOURCE, &native_id),
        source: SOURCE.to_owned(),
        native_id,
        event_type: EVENT_TYPE.to_owned(),
        time: TimeRange::new(pass.aos, pass.los),
        geo: Geo::OrbitPass {
            norad_id: pass.norad_id,
            max_elevation_deg: pass.max_elevation_deg,
        },
        freq,
        payload: json!({
            "name": pass.name,
            "aos_ns": pass.aos.as_unix_nanos(),
            "tca_ns": pass.tca.as_unix_nanos(),
            "los_ns": pass.los.as_unix_nanos(),
            "aos_azimuth_deg": pass.aos_azimuth_deg,
            "los_azimuth_deg": pass.los_azimuth_deg,
            "max_elevation_deg": pass.max_elevation_deg,
            "max_range_rate_km_s": pass.max_range_rate_km_s,
            "tle_epoch_ns": pass.tle_epoch.as_unix_nanos(),
            "tle_age_days": pass.tle_age_days(),
        }),
        fetched_at: computed_at,
        valid_until: Some(pass.los),
    }
}
