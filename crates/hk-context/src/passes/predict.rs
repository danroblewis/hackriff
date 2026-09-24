//! Pass prediction: when a satellite is above the site's horizon, from a cached element set.
//!
//! Elevation is sampled every [`PredictConfig::step_s`]; each horizon crossing is refined by
//! bisection to [`PredictConfig::tolerance_s`], and the culmination by golden-section search. A
//! pass shorter than the coarse step can be missed; such a pass never clears a useful elevation.
//! Every [`Pass`] carries its element set's epoch, so its age is always reportable
//! ([`super::plan`] turns that age into a freshness verdict and a timing margin).

use hk_model::Timestamp;

use super::look::{Look, look, site_ecef, teme_to_ecef};
use super::sgp4::{Sgp4, Sgp4Error};
use super::tle::Tle;
use crate::geo::Site;

const NS_PER_S: f64 = 1e9;

/// Prediction settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PredictConfig {
    /// Horizon (AOS/LOS) elevation, degrees. 0 is the geometric horizon; raise it for a site with
    /// obstructions.
    pub horizon_deg: f64,
    /// Coarse sampling step, s.
    pub step_s: f64,
    /// AOS/LOS/TCA refinement tolerance, s.
    pub tolerance_s: f64,
}

impl Default for PredictConfig {
    fn default() -> Self {
        Self {
            horizon_deg: 0.0,
            step_s: 20.0,
            tolerance_s: 0.05,
        }
    }
}

/// One predicted pass over the site.
#[derive(Clone, Debug, PartialEq)]
pub struct Pass {
    /// NORAD catalogue number.
    pub norad_id: u32,
    /// Satellite name from the element set.
    pub name: String,
    /// Acquisition of signal: rises through the horizon.
    pub aos: Timestamp,
    /// Time of closest approach (maximum elevation).
    pub tca: Timestamp,
    /// Loss of signal: sets through the horizon.
    pub los: Timestamp,
    /// Elevation at TCA, degrees.
    pub max_elevation_deg: f64,
    /// Azimuth at AOS, degrees.
    pub aos_azimuth_deg: f64,
    /// Azimuth at LOS, degrees.
    pub los_azimuth_deg: f64,
    /// Largest |range rate| over the pass, km/s (bounds the Doppler shift).
    pub max_range_rate_km_s: f64,
    /// Epoch of the element set it was computed from (its age is never implicit).
    pub tle_epoch: Timestamp,
}

impl Pass {
    /// Duration AOS → LOS, s.
    pub fn duration_s(&self) -> f64 {
        (self.los.as_unix_nanos() - self.aos.as_unix_nanos()) as f64 / NS_PER_S
    }

    /// Largest Doppler shift magnitude over the pass at carrier `hz`, Hz.
    pub fn max_doppler_hz(&self, hz: f64) -> f64 {
        hz * self.max_range_rate_km_s / super::look::C_KM_S
    }

    /// Element-set age at AOS, days (absolute: an epoch after the pass is as untrusted as one
    /// before it).
    pub fn tle_age_days(&self) -> f64 {
        ((self.aos.as_unix_nanos() - self.tle_epoch.as_unix_nanos()) as f64 / 86_400e9).abs()
    }
}

/// A satellite ready for look-angle queries from one site.
#[derive(Clone, Debug)]
pub struct Tracker {
    tle: Tle,
    sat: Sgp4,
    site: Site,
    obs: [f64; 3],
}

impl Tracker {
    /// Initialises SGP4 for `tle` as seen from `site`.
    pub fn new(tle: &Tle, site: Site) -> Result<Self, Sgp4Error> {
        Ok(Self {
            sat: Sgp4::new(tle)?,
            tle: tle.clone(),
            obs: site_ecef(&site),
            site,
        })
    }

    /// The element set.
    pub fn tle(&self) -> &Tle {
        &self.tle
    }

    /// Look angles at `t`.
    pub fn look_at(&self, t: Timestamp) -> Result<Look, Sgp4Error> {
        let tsince = (t.as_unix_nanos() - self.tle.epoch.as_unix_nanos()) as f64 / 60e9;
        let state = self.sat.propagate(tsince)?;
        let (r, v) = teme_to_ecef(&state, t);
        Ok(look(&self.site, self.obs, r, v))
    }

    fn elevation(&self, t_ns: i64) -> Result<f64, Sgp4Error> {
        Ok(self
            .look_at(Timestamp::from_unix_nanos(t_ns))?
            .elevation_deg)
    }

    /// Bisects the horizon crossing in `(lo, hi]`, where `above(lo) != above(hi)`.
    fn crossing(
        &self,
        mut lo: i64,
        mut hi: i64,
        horizon: f64,
        tol_ns: i64,
    ) -> Result<i64, Sgp4Error> {
        let rising = self.elevation(lo)? < horizon;
        while hi - lo > tol_ns {
            let mid = lo + (hi - lo) / 2;
            if (self.elevation(mid)? >= horizon) == rising {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        Ok(hi)
    }

    /// Culmination in `[a, b]` by golden-section search on elevation.
    fn culmination(&self, a: i64, b: i64, tol_ns: i64) -> Result<(i64, f64), Sgp4Error> {
        let g = (5f64.sqrt() - 1.0) / 2.0;
        let (mut a, mut b) = (a as f64, b as f64);
        let mut c = b - g * (b - a);
        let mut d = a + g * (b - a);
        let (mut fc, mut fd) = (self.elevation(c as i64)?, self.elevation(d as i64)?);
        while b - a > tol_ns as f64 {
            if fc > fd {
                b = d;
                d = c;
                fd = fc;
                c = b - g * (b - a);
                fc = self.elevation(c as i64)?;
            } else {
                a = c;
                c = d;
                fc = fd;
                d = a + g * (b - a);
                fd = self.elevation(d as i64)?;
            }
        }
        let t = ((a + b) / 2.0) as i64;
        Ok((t, self.elevation(t)?))
    }

    /// Passes with LOS after `from` and AOS before `to`, in time order. A pass already in
    /// progress at `from` is found by searching back up to one orbit and keeps its true AOS.
    pub fn passes(
        &self,
        from: Timestamp,
        to: Timestamp,
        cfg: &PredictConfig,
    ) -> Result<Vec<Pass>, Sgp4Error> {
        let step = (cfg.step_s * NS_PER_S) as i64;
        let tol = ((cfg.tolerance_s * NS_PER_S) as i64).max(1);
        let period_ns = (std::f64::consts::TAU / self.sat.mean_motion_rad_min() * 60e9) as i64;
        let (from_ns, to_ns) = (from.as_unix_nanos(), to.as_unix_nanos());
        let mut t = from_ns - period_ns;
        let mut prev = self.elevation(t)?;
        let mut aos: Option<i64> = None;
        let mut out = Vec::new();
        // Past `to`, only a pass already risen is followed to its LOS.
        while t < to_ns || aos.is_some() {
            let next = t + step;
            let el = self.elevation(next)?;
            let (was, is) = (prev >= cfg.horizon_deg, el >= cfg.horizon_deg);
            if !was && is {
                aos = Some(self.crossing(t, next, cfg.horizon_deg, tol)?);
            } else if was && !is {
                let los = self.crossing(t, next, cfg.horizon_deg, tol)?;
                if let Some(a) = aos.take() {
                    if los > from_ns && a < to_ns {
                        out.push(self.pass(a, los, tol)?);
                    }
                }
            }
            prev = el;
            t = next;
        }
        Ok(out)
    }

    fn pass(&self, aos: i64, los: i64, tol: i64) -> Result<Pass, Sgp4Error> {
        let (tca, max_el) = self.culmination(aos, los, tol)?;
        let mut max_rr: f64 = 0.0;
        let n = 64;
        for k in 0..=n {
            let t = aos + (los - aos) * k / n;
            let l = self.look_at(Timestamp::from_unix_nanos(t))?;
            max_rr = max_rr.max(l.range_rate_km_s.abs());
        }
        let (a, l) = (
            self.look_at(Timestamp::from_unix_nanos(aos))?,
            self.look_at(Timestamp::from_unix_nanos(los))?,
        );
        Ok(Pass {
            norad_id: self.tle.norad_id,
            name: self.tle.name.clone(),
            aos: Timestamp::from_unix_nanos(aos),
            tca: Timestamp::from_unix_nanos(tca),
            los: Timestamp::from_unix_nanos(los),
            max_elevation_deg: max_el,
            aos_azimuth_deg: a.azimuth_deg,
            los_azimuth_deg: l.azimuth_deg,
            max_range_rate_km_s: max_rr,
            tle_epoch: self.tle.epoch,
        })
    }
}

/// Passes of every satellite in `tles` over `site` in `[from, to]`, sorted by AOS then NORAD id.
/// A set SGP4 refuses (deep space, decayed, diverged) is returned in the second list with its
/// reason — reported, never silently dropped.
pub fn predict_all(
    tles: &[Tle],
    site: Site,
    from: Timestamp,
    to: Timestamp,
    cfg: &PredictConfig,
) -> (Vec<Pass>, Vec<(u32, Sgp4Error)>) {
    let mut passes = Vec::new();
    let mut refused = Vec::new();
    for tle in tles {
        match Tracker::new(tle, site).and_then(|t| t.passes(from, to, cfg)) {
            Ok(p) => passes.extend(p),
            Err(e) => refused.push((tle.norad_id, e)),
        }
    }
    passes.sort_by_key(|p| (p.aos, p.norad_id));
    (passes, refused)
}
