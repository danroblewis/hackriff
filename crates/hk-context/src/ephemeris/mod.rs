//! GNSS constellation forensics (SIGNAL-032; C36 × C29; T-324): broadcast ephemerides compared
//! against precise orbits.
//!
//! # What this is for
//!
//! A GNSS receiver trusts the ephemeris the satellite broadcasts. A spoofer (or a faulty
//! satellite) can broadcast a navigation message whose orbit or clock is wrong, and the receiver
//! will happily compute a wrong position from it. The forensic check galmon popularised is to
//! evaluate the ephemeris the device *received* and compare it, epoch by epoch, with an
//! independent reference: IGS **precise orbits** (SP3, centimetre-level) or, failing that, the
//! **broadcast ephemerides the rest of the world received** (IGS merged BRDC navigation files).
//! A metre-level residual is normal (broadcast orbits are good to ~1 m, and SP3 is centre-of-mass
//! while broadcast is antenna phase centre); a kilometre residual or a microsecond clock step is
//! a finding.
//!
//! # The chain
//!
//! - [`rinex_nav`]: GPS LNAV ephemerides from a RINEX 2.x/3.x navigation file — the reference
//!   BRDC snapshot and the local receiver's own nav log (GNSS-SDR writes RINEX nav) share one
//!   parser.
//! - [`sp3`]: precise orbits and clocks from an SP3-c/d file (GPS time only).
//! - [`orbit`]: IS-GPS-200 Table 20-IV — an ephemeris to an Earth-fixed position and a clock
//!   offset at a time.
//! - [`forensics`]: per-ephemeris residuals and a verdict, with *no data* kept distinct from
//!   *agreement*.
//! - The cached feeds are [`crate::feeds::gnss_orbits`], the same offline-first cache and
//!   [`crate::feeds::FeedFetcher`] seam the TLE feed (T-276) uses — not a second feed path.
//!
//! # Honesty rule
//!
//! An absent reference reads as [`forensics::SvVerdict::NotYetFetched`], a reference that does
//! not span the ephemeris as [`forensics::SvVerdict::NotCovered`], a reference lacking the
//! satellite as [`forensics::SvVerdict::NoReferenceForSv`]. None of them is agreement; only
//! [`forensics::SvVerdict::Agrees`] is, and only after samples were actually compared.
//!
//! # Not the known-code exception
//!
//! Nothing here acquires or detects anything. It compares navigation *data* with reference data,
//! so it cannot seed a detector (ADR-0018's leak rule is unaffected).

pub mod forensics;
pub mod orbit;
pub mod rinex_nav;
pub mod sp3;

use hk_model::Timestamp;

pub use forensics::{ForensicsConfig, Reference, SvCheck, SvVerdict, compare};
pub use orbit::SvState;
pub use rinex_nav::{GpsEphemeris, NavFile, parse_nav};
pub use sp3::{Sp3, Sp3Epoch, Sp3Record, parse_sp3};

/// Seconds per GPS week.
pub const SECONDS_PER_WEEK: f64 = 604_800.0;

/// Unix time of the GPS epoch, 1980-01-06T00:00:00Z.
pub const GPS_EPOCH_UNIX_S: i64 = 315_964_800;

/// GPS − UTC, seconds. 18 s since 2017-01-01 and unchanged at the time of writing; a future leap
/// second changes it (no leap-second table is carried — unverified beyond 2026).
pub const GPS_MINUS_UTC_S: i64 = 18;

/// A GPS-system time: seconds since the GPS epoch (no leap seconds). f64 keeps ~0.2 µs at today's
/// magnitudes, i.e. under a millimetre of satellite motion.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct GpsTime(pub f64);

impl GpsTime {
    /// From a GPS-time calendar epoch (RINEX and SP3 write GPS time as a calendar date).
    pub fn from_calendar(y: i64, mo: u32, d: u32, h: u32, mi: u32, s: f64) -> Option<Self> {
        if h > 23 || mi > 59 || !(0.0..61.0).contains(&s) {
            return None;
        }
        let day = crate::utc::day_start(&format!("{y:04}-{mo:02}-{d:02}"))?;
        let day_s = day.as_unix_nanos().div_euclid(1_000_000_000) - GPS_EPOCH_UNIX_S;
        Some(Self(
            day_s as f64 + f64::from(h) * 3600.0 + f64::from(mi) * 60.0 + s,
        ))
    }

    /// From a GPS week (continuous, not mod 1024) and seconds of week.
    pub fn from_week_sow(week: u32, sow: f64) -> Self {
        Self(f64::from(week) * SECONDS_PER_WEEK + sow)
    }

    /// GPS week.
    pub fn week(self) -> u32 {
        (self.0 / SECONDS_PER_WEEK).floor() as u32
    }

    /// Seconds of week.
    pub fn sow(self) -> f64 {
        self.0 - f64::from(self.week()) * SECONDS_PER_WEEK
    }

    /// Plus `s` seconds.
    pub fn plus_s(self, s: f64) -> Self {
        Self(self.0 + s)
    }

    /// The UTC instant (applies [`GPS_MINUS_UTC_S`]).
    pub fn to_utc(self) -> Timestamp {
        let unix_s = self.0 + (GPS_EPOCH_UNIX_S - GPS_MINUS_UTC_S) as f64;
        Timestamp::from_unix_nanos((unix_s * 1e9).round() as i64)
    }

    /// From a UTC instant.
    pub fn from_utc(t: Timestamp) -> Self {
        Self(t.as_unix_nanos() as f64 / 1e9 - (GPS_EPOCH_UNIX_S - GPS_MINUS_UTC_S) as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gps_epoch_is_week_zero_and_utc_offset_applies() {
        let t = GpsTime::from_calendar(1980, 1, 6, 0, 0, 0.0).unwrap();
        assert_eq!(t.0, 0.0);
        // 2026-09-20 is a Sunday: the start of a GPS week.
        let t = GpsTime::from_calendar(2026, 9, 20, 0, 0, 0.0).unwrap();
        assert_eq!(t.sow(), 0.0);
        assert_eq!(t.week(), 2437);
        let utc = t.to_utc();
        assert_eq!(
            utc,
            crate::utc::parse_utc("2026-09-19T23:59:42Z").unwrap(),
            "GPS runs 18 s ahead of UTC"
        );
        assert_eq!(GpsTime::from_utc(utc), t);
    }
}
