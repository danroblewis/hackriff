//! Received ephemerides against a reference: per-ephemeris orbit and clock residuals, and a
//! verdict that keeps *no data* apart from *agreement*.
//!
//! # Reference order
//!
//! For each local ephemeris the check samples its fit interval (`toe ± fit/2`) and compares:
//!
//! 1. against **precise orbits** (SP3) at every SP3 epoch in the interval where the satellite has
//!    a good position, if any;
//! 2. otherwise against the **reference broadcast ephemeris** for the same satellite valid at each
//!    sample time (every [`ForensicsConfig::broadcast_step_s`]).
//!
//! If neither yields a single sample, the verdict says *why*, in order of how much we know:
//! a reference spans the interval but lacks the satellite ([`SvVerdict::NoReferenceForSv`]), a
//! reference is cached but does not span the interval ([`SvVerdict::NotCovered`]), or nothing is
//! cached at all ([`SvVerdict::NotYetFetched`]). None of those is agreement.
//!
//! # Tolerances (defaults; measured on nothing yet — unverified)
//!
//! GPS broadcast orbits are good to ~1–2 m RMS against IGS finals, SP3 is centre of mass against
//! the broadcast antenna phase centre (~1 m), and broadcast vs precise clocks differ by a few ns
//! (TGD, time-scale alignment). Defaults are therefore 25 m and 60 ns — loose enough that a
//! healthy satellite never trips, tight enough that a spoofed orbit (km) or a clock step (µs)
//! always does.

use super::GpsTime;
use super::orbit::distance;
use super::rinex_nav::GpsEphemeris;
use super::sp3::Sp3;

/// Tolerances and sampling.
#[derive(Clone, Debug, PartialEq)]
pub struct ForensicsConfig {
    /// Largest 3-D orbit residual still counted as agreement, m.
    pub orbit_tolerance_m: f64,
    /// Largest clock residual still counted as agreement, ns.
    pub clock_tolerance_ns: f64,
    /// Sample spacing against a broadcast reference, s.
    pub broadcast_step_s: f64,
}

impl Default for ForensicsConfig {
    fn default() -> Self {
        Self {
            orbit_tolerance_m: 25.0,
            clock_tolerance_ns: 60.0,
            broadcast_step_s: 900.0,
        }
    }
}

/// Which reference a residual was measured against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reference {
    /// IGS precise orbits (SP3).
    Precise,
    /// Broadcast ephemerides received elsewhere (IGS merged BRDC).
    Broadcast,
}

/// Residuals over the samples compared.
#[derive(Clone, Debug, PartialEq)]
pub struct Residuals {
    /// The reference used.
    pub reference: Reference,
    /// Samples compared.
    pub samples: usize,
    /// Largest 3-D orbit residual, m.
    pub max_orbit_m: f64,
    /// RMS 3-D orbit residual, m.
    pub rms_orbit_m: f64,
    /// Largest |clock residual|, ns; `None` if the reference had no clock at any sample.
    pub max_clock_ns: Option<f64>,
    /// Time of the largest orbit residual.
    pub worst_at: GpsTime,
}

/// The verdict for one received ephemeris.
#[derive(Clone, Debug, PartialEq)]
pub enum SvVerdict {
    /// Compared, and inside both tolerances.
    Agrees(Residuals),
    /// Compared, and outside at least one tolerance.
    Disagrees {
        /// The residuals.
        residuals: Residuals,
        /// The orbit residual exceeded its tolerance (a wrong orbit: spoofing or a bad upload).
        orbit: bool,
        /// The clock residual exceeded its tolerance (a clock jump).
        clock: bool,
    },
    /// No reference data is cached at all. Not agreement: nothing was compared.
    NotYetFetched,
    /// Reference data is cached but does not span this ephemeris' fit interval.
    NotCovered,
    /// A reference spans the interval but has no usable data for this satellite.
    NoReferenceForSv,
}

impl SvVerdict {
    /// Only [`SvVerdict::Agrees`] is agreement.
    pub fn is_agreement(&self) -> bool {
        matches!(self, SvVerdict::Agrees(_))
    }

    /// Whether samples were actually compared.
    pub fn was_compared(&self) -> bool {
        matches!(self, SvVerdict::Agrees(_) | SvVerdict::Disagrees { .. })
    }

    /// Stable label.
    pub fn label(&self) -> &'static str {
        match self {
            SvVerdict::Agrees(_) => "agrees",
            SvVerdict::Disagrees { .. } => "disagrees",
            SvVerdict::NotYetFetched => "not-yet-fetched",
            SvVerdict::NotCovered => "not-covered",
            SvVerdict::NoReferenceForSv => "no-reference-for-sv",
        }
    }
}

/// One received ephemeris and its verdict.
#[derive(Clone, Debug, PartialEq)]
pub struct SvCheck {
    /// PRN.
    pub prn: u8,
    /// Its time of ephemeris.
    pub toe: GpsTime,
    /// Its IODE.
    pub iode: f64,
    /// The satellite flagged itself unhealthy.
    pub unhealthy: bool,
    /// The verdict.
    pub verdict: SvVerdict,
}

/// Why a single reference produced no samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Miss {
    Absent,
    NotCovered,
    NoSv,
}

fn accumulate(
    reference: Reference,
    pairs: impl Iterator<Item = (GpsTime, [f64; 3], Option<f64>)>,
    local: &GpsEphemeris,
) -> Option<Residuals> {
    let mut samples = 0usize;
    let mut max_orbit = -1.0f64;
    let mut sum_sq = 0.0;
    let mut max_clock: Option<f64> = None;
    let mut worst_at = local.toe();
    for (t, pos, clock) in pairs {
        let s = local.state_at(t);
        let d = distance(s.ecef_m, pos);
        samples += 1;
        sum_sq += d * d;
        if d > max_orbit {
            max_orbit = d;
            worst_at = t;
        }
        if let Some(c) = clock {
            let dc = ((s.clock_s - c) * 1e9).abs();
            max_clock = Some(max_clock.map_or(dc, |m: f64| m.max(dc)));
        }
    }
    (samples > 0).then(|| Residuals {
        reference,
        samples,
        max_orbit_m: max_orbit,
        rms_orbit_m: (sum_sq / samples as f64).sqrt(),
        max_clock_ns: max_clock,
        worst_at,
    })
}

fn against_precise(local: &GpsEphemeris, sp3: Option<&Sp3>) -> Result<Residuals, Miss> {
    let sp3 = sp3.ok_or(Miss::Absent)?;
    let (from, to) = local.fit_span();
    let (a, b) = sp3.span();
    if b < from || a > to {
        return Err(Miss::NotCovered);
    }
    let samples = sp3.samples(local.prn, from, to);
    accumulate(
        Reference::Precise,
        samples
            .into_iter()
            .map(|(t, r)| (t, r.ecef_m.expect("samples() keeps positions"), r.clock_s)),
        local,
    )
    .ok_or(Miss::NoSv)
}

fn against_broadcast(
    local: &GpsEphemeris,
    reference: Option<&[GpsEphemeris]>,
    cfg: &ForensicsConfig,
) -> Result<Residuals, Miss> {
    let reference = reference.ok_or(Miss::Absent)?;
    let (from, to) = local.fit_span();
    let step = cfg.broadcast_step_s.max(1.0);
    let times: Vec<GpsTime> = (0..)
        .map(|k| from.plus_s(step * f64::from(k)))
        .take_while(|t| *t <= to)
        .collect();
    let covered = times
        .iter()
        .any(|t| reference.iter().any(|r| r.valid_at(*t)));
    if !covered {
        return Err(Miss::NotCovered);
    }
    let pairs = times.into_iter().filter_map(|t| {
        reference
            .iter()
            .filter(|r| r.prn == local.prn && r.valid_at(t))
            .min_by(|a, b| (a.toe().0 - t.0).abs().total_cmp(&(b.toe().0 - t.0).abs()))
            .map(|r| {
                let s = r.state_at(t);
                (t, s.ecef_m, Some(s.clock_s))
            })
    });
    accumulate(Reference::Broadcast, pairs, local).ok_or(Miss::NoSv)
}

/// Compares each received ephemeris in `local` against the precise orbits, falling back to the
/// reference broadcast ephemerides (see the [module docs](self)). `None` for a reference means
/// it is not cached.
pub fn compare(
    local: &[GpsEphemeris],
    precise: Option<&Sp3>,
    broadcast: Option<&[GpsEphemeris]>,
    cfg: &ForensicsConfig,
) -> Vec<SvCheck> {
    local
        .iter()
        .map(|eph| {
            let verdict = match against_precise(eph, precise) {
                Ok(r) => judge(r, cfg),
                Err(p) => match against_broadcast(eph, broadcast, cfg) {
                    Ok(r) => judge(r, cfg),
                    Err(b) => match p.max(b) {
                        Miss::Absent => SvVerdict::NotYetFetched,
                        Miss::NotCovered => SvVerdict::NotCovered,
                        Miss::NoSv => SvVerdict::NoReferenceForSv,
                    },
                },
            };
            SvCheck {
                prn: eph.prn,
                toe: eph.toe(),
                iode: eph.iode,
                unhealthy: eph.health != 0,
                verdict,
            }
        })
        .collect()
}

fn judge(residuals: Residuals, cfg: &ForensicsConfig) -> SvVerdict {
    let orbit = residuals.max_orbit_m > cfg.orbit_tolerance_m;
    let clock = residuals
        .max_clock_ns
        .is_some_and(|c| c > cfg.clock_tolerance_ns);
    if orbit || clock {
        SvVerdict::Disagrees {
            residuals,
            orbit,
            clock,
        }
    } else {
        SvVerdict::Agrees(residuals)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ephemeris::parse_nav;
    use crate::ephemeris::rinex_nav::tests::RINEX3;
    use crate::ephemeris::sp3::{Sp3Epoch, Sp3Record};

    fn eph() -> GpsEphemeris {
        parse_nav(RINEX3).unwrap().ephemerides.remove(0)
    }

    fn sp3_from(e: &GpsEphemeris, offset_m: f64, prn: u8) -> Sp3 {
        let (a, b) = e.fit_span();
        let epochs = (0..)
            .map(|k| a.plus_s(900.0 * f64::from(k)))
            .take_while(|t| *t <= b)
            .map(|t| {
                let s = e.state_at(t);
                Sp3Epoch {
                    t,
                    records: vec![Sp3Record {
                        prn,
                        ecef_m: Some([s.ecef_m[0] + offset_m, s.ecef_m[1], s.ecef_m[2]]),
                        clock_s: Some(s.clock_s + 3e-9),
                    }],
                }
            })
            .collect();
        Sp3 {
            version: 'd',
            epochs,
            skipped_other_systems: 0,
        }
    }

    #[test]
    fn nothing_cached_is_not_yet_fetched_never_agreement() {
        let checks = compare(&[eph()], None, None, &ForensicsConfig::default());
        assert_eq!(checks[0].verdict, SvVerdict::NotYetFetched);
        assert!(!checks[0].verdict.is_agreement());
        assert!(!checks[0].verdict.was_compared());
    }

    #[test]
    fn precise_agreement_and_orbit_disagreement() {
        let e = eph();
        let cfg = ForensicsConfig::default();
        let ok = compare(
            std::slice::from_ref(&e),
            Some(&sp3_from(&e, 1.0, 5)),
            None,
            &cfg,
        );
        let SvVerdict::Agrees(r) = &ok[0].verdict else {
            panic!("{:?}", ok[0].verdict)
        };
        assert_eq!(r.reference, Reference::Precise);
        assert_eq!(r.samples, 17, "4 h at 15 min, both ends");
        assert!((r.max_orbit_m - 1.0).abs() < 1e-6);
        assert!((r.max_clock_ns.unwrap() - 3.0).abs() < 1e-6);
        let bad = compare(
            std::slice::from_ref(&e),
            Some(&sp3_from(&e, 2_000.0, 5)),
            None,
            &cfg,
        );
        assert!(matches!(
            bad[0].verdict,
            SvVerdict::Disagrees {
                orbit: true,
                clock: false,
                ..
            }
        ));
    }

    #[test]
    fn missing_satellite_and_uncovered_span_are_named() {
        let e = eph();
        let cfg = ForensicsConfig::default();
        let other_sv = sp3_from(&e, 0.0, 9);
        assert_eq!(
            compare(std::slice::from_ref(&e), Some(&other_sv), None, &cfg)[0].verdict,
            SvVerdict::NoReferenceForSv
        );
        let mut later = e.clone();
        later.toe_sow += 86_400.0;
        let far = sp3_from(&later, 0.0, 5);
        assert_eq!(
            compare(std::slice::from_ref(&e), Some(&far), None, &cfg)[0].verdict,
            SvVerdict::NotCovered
        );
        // Precise misses; the broadcast reference covers it.
        let v = &compare(
            std::slice::from_ref(&e),
            Some(&far),
            Some(std::slice::from_ref(&e)),
            &cfg,
        )[0]
        .verdict;
        let SvVerdict::Agrees(r) = v else {
            panic!("{v:?}")
        };
        assert_eq!(r.reference, Reference::Broadcast);
        assert!(r.max_orbit_m < 1e-9 && r.max_clock_ns.unwrap() < 1e-9);
    }

    #[test]
    fn clock_jump_is_flagged_on_its_own() {
        let e = eph();
        let mut jumped = e.clone();
        jumped.af0 += 1e-6;
        let v = &compare(&[jumped], None, Some(&[e]), &ForensicsConfig::default())[0].verdict;
        let SvVerdict::Disagrees {
            orbit,
            clock,
            residuals,
        } = v
        else {
            panic!("{v:?}")
        };
        assert!(!orbit && *clock);
        assert!((residuals.max_clock_ns.unwrap() - 1000.0).abs() < 1e-6);
    }
}
