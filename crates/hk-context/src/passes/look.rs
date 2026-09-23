//! Frames and look angles: TEME → Earth-fixed through Greenwich mean sidereal time (IAU-82, as
//! SGP4 output is defined; polar motion ignored — metres, far below what a pass window needs), the
//! observer on the WGS-84 ellipsoid, and topocentric azimuth / elevation / range / range rate.

use hk_model::Timestamp;

use super::sgp4::StateTeme;
use crate::geo::Site;

/// Earth rotation rate, rad/s.
const OMEGA_EARTH: f64 = 7.292_115_146_706_979e-5;
/// WGS-84 equatorial radius, km.
const WGS84_A_KM: f64 = 6378.137;
/// WGS-84 flattening.
const WGS84_F: f64 = 1.0 / 298.257_223_563;
/// Speed of light, km/s.
pub const C_KM_S: f64 = 299_792.458;

/// Julian date (UT1 ≈ UTC) of a timestamp.
pub fn julian_date(t: Timestamp) -> f64 {
    t.as_unix_nanos() as f64 / 86_400e9 + 2_440_587.5
}

/// Greenwich mean sidereal time, rad in [0, 2π) (IAU-82, Vallado `gstime`).
pub fn gmst(t: Timestamp) -> f64 {
    let tut1 = (julian_date(t) - 2_451_545.0) / 36_525.0;
    let secs = -6.2e-6 * tut1.powi(3)
        + 0.093_104 * tut1 * tut1
        + (876_600.0 * 3600.0 + 8_640_184.812_866) * tut1
        + 67_310.548_41;
    (secs.to_radians() / 240.0).rem_euclid(std::f64::consts::TAU)
}

/// A TEME state rotated into the Earth-fixed frame (position km, velocity km/s relative to the
/// rotating Earth).
pub fn teme_to_ecef(state: &StateTeme, t: Timestamp) -> ([f64; 3], [f64; 3]) {
    let (s, c) = gmst(t).sin_cos();
    let [x, y, z] = state.r_km;
    let [vx, vy, vz] = state.v_km_s;
    let r = [c * x + s * y, -s * x + c * y, z];
    let v = [
        c * vx + s * vy + OMEGA_EARTH * r[1],
        -s * vx + c * vy - OMEGA_EARTH * r[0],
        vz,
    ];
    (r, v)
}

/// A site on the WGS-84 ellipsoid, Earth-fixed, km (altitude 0 when unknown).
pub fn site_ecef(site: &Site) -> [f64; 3] {
    let (lat, lon) = (site.lat_deg.to_radians(), site.lon_deg.to_radians());
    let h = site.alt_m.unwrap_or(0.0) / 1000.0;
    let e2 = WGS84_F * (2.0 - WGS84_F);
    let n = WGS84_A_KM / (1.0 - e2 * lat.sin().powi(2)).sqrt();
    [
        (n + h) * lat.cos() * lon.cos(),
        (n + h) * lat.cos() * lon.sin(),
        (n * (1.0 - e2) + h) * lat.sin(),
    ]
}

/// Geodetic latitude/longitude (degrees) and height (km) of an Earth-fixed point (Bowring's
/// iteration, converged to far below a metre).
pub fn ecef_to_geodetic(r: [f64; 3]) -> (f64, f64, f64) {
    let e2 = WGS84_F * (2.0 - WGS84_F);
    let p = r[0].hypot(r[1]);
    let lon = r[1].atan2(r[0]);
    let mut lat = r[2].atan2(p * (1.0 - e2));
    let mut h = 0.0;
    for _ in 0..8 {
        let n = WGS84_A_KM / (1.0 - e2 * lat.sin().powi(2)).sqrt();
        h = p / lat.cos() - n;
        lat = r[2].atan2(p * (1.0 - e2 * n / (n + h)));
    }
    (lat.to_degrees(), lon.to_degrees(), h)
}

/// Where a satellite appears from the site.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    /// Azimuth from true north, clockwise, degrees in [0, 360).
    pub azimuth_deg: f64,
    /// Elevation above the ellipsoid horizon, degrees.
    pub elevation_deg: f64,
    /// Slant range, km.
    pub range_km: f64,
    /// Range rate, km/s (positive receding). Doppler at `f` is `−f · rate / c`.
    pub range_rate_km_s: f64,
}

/// Look angles of an Earth-fixed satellite state from `site` (Earth-fixed position `obs`).
pub fn look(site: &Site, obs: [f64; 3], r: [f64; 3], v: [f64; 3]) -> Look {
    let rho = [r[0] - obs[0], r[1] - obs[1], r[2] - obs[2]];
    let range = (rho[0] * rho[0] + rho[1] * rho[1] + rho[2] * rho[2]).sqrt();
    let (sl, cl) = site.lat_deg.to_radians().sin_cos();
    let (so, co) = site.lon_deg.to_radians().sin_cos();
    let south = sl * co * rho[0] + sl * so * rho[1] - cl * rho[2];
    let east = -so * rho[0] + co * rho[1];
    let zenith = cl * co * rho[0] + cl * so * rho[1] + sl * rho[2];
    Look {
        azimuth_deg: east.atan2(-south).to_degrees().rem_euclid(360.0),
        elevation_deg: (zenith / range).clamp(-1.0, 1.0).asin().to_degrees(),
        range_km: range,
        range_rate_km_s: (rho[0] * v[0] + rho[1] * v[1] + rho[2] * v[2]) / range,
    }
}
