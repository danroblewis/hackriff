//! A GPS LNAV ephemeris evaluated at a time: IS-GPS-200 Table 20-IV (position) and §20.3.3.3.3.1
//! (clock).

use super::rinex_nav::GpsEphemeris;
use super::{GpsTime, SECONDS_PER_WEEK};

/// WGS-84 gravitational constant used by the GPS control segment, m³/s².
pub const MU: f64 = 3.986_005e14;
/// WGS-84 Earth rotation rate, rad/s.
pub const OMEGA_E: f64 = 7.292_115_146_7e-5;
/// Relativistic clock constant F, s/√m.
pub const F_REL: f64 = -4.442_807_633e-10;

/// A satellite's Earth-fixed (WGS-84 ECEF) position and clock offset at a time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SvState {
    /// Position, m.
    pub ecef_m: [f64; 3],
    /// SV clock offset from GPS time, s — `af0 + af1·dt + af2·dt² + Δt_r`, **without** the L1
    /// group delay, so it is comparable with an ionosphere-free precise clock.
    pub clock_s: f64,
}

/// Seconds `t − t_ref`, wrapped into ±half a week (the IS-GPS-200 crossover rule).
fn wrap(dt: f64) -> f64 {
    if dt > SECONDS_PER_WEEK / 2.0 {
        dt - SECONDS_PER_WEEK
    } else if dt < -SECONDS_PER_WEEK / 2.0 {
        dt + SECONDS_PER_WEEK
    } else {
        dt
    }
}

/// Solves Kepler's equation `M = E − e·sin E` by Newton iteration.
pub fn eccentric_anomaly(m: f64, e: f64) -> f64 {
    // Danby's starter: converges from any M for e < 1.
    let mut ea = m + 0.85 * e * m.sin().signum();
    for _ in 0..50 {
        let d = (ea - e * ea.sin() - m) / (1.0 - e * ea.cos());
        ea -= d;
        if d.abs() < 1e-15 {
            break;
        }
    }
    ea
}

impl GpsEphemeris {
    /// Evaluates the ephemeris at GPS time `t`: satellite position in the Earth-fixed frame *of
    /// epoch `t`* (the frame an SP3 record uses) and its clock offset.
    ///
    /// No fit-interval check: callers decide whether `t` is in range ([`Self::valid_at`]).
    pub fn state_at(&self, t: GpsTime) -> SvState {
        let a = self.sqrt_a * self.sqrt_a;
        let n0 = (MU / (a * a * a)).sqrt();
        let tk = wrap(t.0 - self.toe().0);
        let n = n0 + self.delta_n;
        let mk = self.m0 + n * tk;
        let ek = eccentric_anomaly(mk, self.e);
        let (sin_e, cos_e) = ek.sin_cos();
        let nu = ((1.0 - self.e * self.e).sqrt() * sin_e).atan2(cos_e - self.e);
        let phi = nu + self.omega;
        let (s2, c2) = (2.0 * phi).sin_cos();
        let du = self.cus * s2 + self.cuc * c2;
        let dr = self.crs * s2 + self.crc * c2;
        let di = self.cis * s2 + self.cic * c2;
        let u = phi + du;
        let r = a * (1.0 - self.e * cos_e) + dr;
        let i = self.i0 + di + self.idot * tk;
        let (xp, yp) = (r * u.cos(), r * u.sin());
        let big_omega = self.omega0 + (self.omega_dot - OMEGA_E) * tk - OMEGA_E * self.toe_sow;
        let (so, co) = big_omega.sin_cos();
        let (si, ci) = i.sin_cos();
        let ecef_m = [xp * co - yp * ci * so, xp * so + yp * ci * co, yp * si];

        let dtc = wrap(t.0 - self.toc.0);
        let rel = F_REL * self.e * self.sqrt_a * sin_e;
        let clock_s = self.af0 + self.af1 * dtc + self.af2 * dtc * dtc + rel;
        SvState { ecef_m, clock_s }
    }
}

/// Euclidean distance, m.
pub fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ephemeris::parse_nav;

    fn circular(i0: f64) -> GpsEphemeris {
        GpsEphemeris {
            prn: 1,
            toc: GpsTime(0.0),
            af0: 1e-5,
            af1: 1e-11,
            af2: 0.0,
            iode: 1.0,
            crs: 0.0,
            delta_n: 0.0,
            m0: 0.0,
            cuc: 0.0,
            e: 0.0,
            cus: 0.0,
            sqrt_a: 5153.6,
            toe_sow: 0.0,
            cic: 0.0,
            omega0: 0.0,
            cis: 0.0,
            i0,
            crc: 0.0,
            omega: 0.0,
            omega_dot: 0.0,
            idot: 0.0,
            week: 0,
            sv_accuracy_m: 2.0,
            health: 0,
            tgd: 0.0,
            iodc: 1.0,
            fit_interval_h: 4.0,
        }
    }

    #[test]
    fn kepler_solution_satisfies_keplers_equation() {
        for &e in &[0.0, 0.01, 0.2, 0.7, 0.95] {
            for k in 0..20 {
                let m = -3.0 + 0.3 * f64::from(k);
                let ea = eccentric_anomaly(m, e);
                assert!((ea - e * ea.sin() - m).abs() < 1e-12, "e {e} M {m}");
            }
        }
    }

    /// Equatorial circular orbit: the satellite advances at n while the frame turns at Ω_E, so
    /// its Earth-fixed longitude is (n − Ω_E)·t — closed form.
    #[test]
    fn equatorial_circular_orbit_matches_closed_form() {
        let eph = circular(0.0);
        let a = eph.sqrt_a * eph.sqrt_a;
        let n = (MU / a.powi(3)).sqrt();
        for &t in &[0.0, 600.0, 3600.0, -5400.0] {
            let s = eph.state_at(GpsTime(t));
            let lon = (n - OMEGA_E) * t;
            assert!(distance(s.ecef_m, [a * lon.cos(), a * lon.sin(), 0.0]) < 1e-6);
            assert!((s.clock_s - (1e-5 + 1e-11 * t)).abs() < 1e-18);
        }
    }

    /// Polar circular orbit: z = a·sin(n·t) exactly, whatever the frame rotation.
    #[test]
    fn polar_orbit_z_follows_the_argument_of_latitude() {
        let eph = circular(std::f64::consts::FRAC_PI_2);
        let a = eph.sqrt_a * eph.sqrt_a;
        let n = (MU / a.powi(3)).sqrt();
        for &t in &[0.0, 1234.0, 7200.0] {
            let s = eph.state_at(GpsTime(t));
            assert!((s.ecef_m[2] - a * (n * t).sin()).abs() < 1e-6);
        }
    }

    /// A real-shaped ephemeris lands on a GPS orbit (radius ~26 600 km) at a GPS speed.
    #[test]
    fn realistic_ephemeris_has_gps_radius_and_speed() {
        let nav = parse_nav(crate::ephemeris::rinex_nav::tests::RINEX3).unwrap();
        let eph = &nav.ephemerides[0];
        let t = eph.toe();
        let s0 = eph.state_at(t);
        let s1 = eph.state_at(t.plus_s(1.0));
        let r = distance(s0.ecef_m, [0.0; 3]);
        assert!((25.9e6..27.2e6).contains(&r), "GPS orbit radius, got {r}");
        let v = distance(s0.ecef_m, s1.ecef_m);
        // Earth-fixed speed of a GPS satellite: ~1.5–4 km/s depending on geometry.
        assert!((1_000.0..4_500.0).contains(&v), "ECEF speed {v} m/s");
        // Clock at toc equals af0 plus the (small) relativistic term.
        assert!((s0.clock_s - eph.af0).abs() < 50e-9);
    }

    /// An ephemeris used across a week boundary: time runs continuously (week 10 → 11), and the
    /// node longitude uses the toe's seconds of week, per Table 20-IV.
    #[test]
    fn week_crossover_uses_continuous_time() {
        let mut eph = circular(0.0);
        eph.week = 10;
        eph.toe_sow = 604_000.0;
        eph.toc = eph.toe();
        let a = eph.sqrt_a * eph.sqrt_a;
        let n = (MU / a.powi(3)).sqrt();
        let later = GpsTime::from_week_sow(11, 200.0);
        let s = eph.state_at(later);
        let lon = (n - OMEGA_E) * 1000.0 - OMEGA_E * 604_000.0;
        assert!(distance(s.ecef_m, [a * lon.cos(), a * lon.sin(), 0.0]) < 1e-6);
        assert!((s.clock_s - (1e-5 + 1e-11 * 1000.0)).abs() < 1e-18);
    }
}
