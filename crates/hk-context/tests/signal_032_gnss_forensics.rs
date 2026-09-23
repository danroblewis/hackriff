//! T-324 — SIGNAL-032 (GNSS constellation forensics): the navigation messages a receiver logged
//! (RINEX nav) are compared against IGS precise orbits (SP3) and, failing those, against the
//! broadcast ephemerides the rest of the world received (BRDC) — both cached through the same
//! offline-first feed cache the TLE feed uses (T-276). A spoofed orbit and a clock jump are
//! flagged; healthy satellites agree; and an absent reference reads as not-yet-fetched, never as
//! agreement.
//!
//! The constellation is **synthetic** (six GPS-shaped ephemerides, hand-chosen). The "precise"
//! file is the truth propagated with IS-GPS-200 plus a fixed sub-metre offset and a 2 ns clock
//! bias, so it plays the role of an independent reference; the propagator itself is checked
//! against closed forms in `ephemeris::orbit`'s unit tests. Not yet checked against a real
//! IGS BRDC/SP3 pair (no network here) — that is the first thing to do with a real capture.

use std::fs;
use std::path::PathBuf;

use hk_context::ephemeris::{
    ForensicsConfig, GpsEphemeris, GpsTime, Reference, SvCheck, SvVerdict, parse_nav,
};
use hk_context::feeds::gnss_orbits::{
    BrdcAdapter, ForensicsReport, ReferenceAvailability, Sp3Adapter, compare_from_cache,
};
use hk_context::feeds::{DirectoryFetcher, FeedCache, OfflineFetcher, refresh};
use hk_model::{Repository, Timestamp};

const S: i64 = 1_000_000_000;
const PRNS: [u8; 6] = [3, 7, 12, 17, 24, 30];
/// Spoofed: its received orbit is wrong (M0 shifted → ~2.6 km along-track).
const SPOOFED: u8 = 7;
/// Its received clock jumped by 1 µs.
const CLOCK_JUMP: u8 = 12;
/// Absent from the precise-orbit file (as a newly launched or excluded satellite would be).
const NOT_IN_SP3: u8 = 30;
const SP3_KEY: &str = "igs-final-2026-09-20";
const BRDC_KEY: &str = "brdc-2026-09-20";

/// 2026-09-20T12:00:00 GPS time: week 2437, 43 200 s of week.
fn toe() -> GpsTime {
    GpsTime::from_calendar(2026, 9, 20, 12, 0, 0.0).unwrap()
}

fn truth() -> Vec<GpsEphemeris> {
    PRNS.iter()
        .enumerate()
        .map(|(k, &prn)| {
            let k = k as f64;
            GpsEphemeris {
                prn,
                toc: toe(),
                af0: -1.2e-4 + 3.0e-5 * k,
                af1: -1.1e-12,
                af2: 0.0,
                iode: 55.0 + k,
                crs: -80.0 + 10.0 * k,
                delta_n: 4.5e-9,
                m0: -3.0 + 1.1 * k,
                cuc: -4.0e-6,
                e: 0.002 + 0.003 * k,
                cus: 7.0e-6,
                sqrt_a: 5153.6 + 0.05 * k,
                toe_sow: toe().sow(),
                cic: 1.0e-8,
                omega0: -2.8 + 1.047 * k,
                cis: 4.0e-8,
                i0: 0.96 + 0.005 * k,
                crc: 220.0,
                omega: 0.5 + 0.9 * k,
                omega_dot: -8.1e-9,
                idot: 1.0e-10,
                week: toe().week(),
                sv_accuracy_m: 2.0,
                health: 0,
                tgd: -1.0e-8,
                iodc: 55.0 + k,
                fit_interval_h: 4.0,
            }
        })
        .collect()
}

/// What this receiver logged: the truth, except one spoofed orbit and one clock jump.
fn received() -> Vec<GpsEphemeris> {
    truth()
        .into_iter()
        .map(|mut e| {
            if e.prn == SPOOFED {
                e.m0 += 1.0e-4;
            }
            if e.prn == CLOCK_JUMP {
                e.af0 += 1.0e-6;
            }
            e
        })
        .collect()
}

/// A Fortran `D19.12` field.
fn d19(v: f64) -> String {
    if v == 0.0 {
        return " 0.000000000000D+00".to_owned();
    }
    let s = format!("{:.11e}", v.abs());
    let (m, e) = s.split_once('e').unwrap();
    let exp: i32 = e.parse::<i32>().unwrap() + 1;
    format!(
        "{}0.{}D{}{:02}",
        if v < 0.0 { '-' } else { ' ' },
        m.replace('.', ""),
        if exp < 0 { '-' } else { '+' },
        exp.abs()
    )
}

/// A RINEX 3.04 navigation file, as GNSS-SDR or the IGS BRDC merge would write it.
fn rinex(ephs: &[GpsEphemeris]) -> String {
    let mut s = String::from(
        "     3.04           N: GNSS NAV DATA    G: GPS              RINEX VERSION / TYPE\n\
         \x20                                                           END OF HEADER\n",
    );
    for e in ephs {
        let row =
            |v: [f64; 4]| format!("    {}{}{}{}\n", d19(v[0]), d19(v[1]), d19(v[2]), d19(v[3]));
        s += &format!(
            "G{:02} 2026 09 20 12 00 00{}{}{}\n",
            e.prn,
            d19(e.af0),
            d19(e.af1),
            d19(e.af2)
        );
        s += &row([e.iode, e.crs, e.delta_n, e.m0]);
        s += &row([e.cuc, e.e, e.cus, e.sqrt_a]);
        s += &row([e.toe_sow, e.cic, e.omega0, e.cis]);
        s += &row([e.i0, e.crc, e.omega, e.omega_dot]);
        s += &row([e.idot, 1.0, f64::from(e.week), 0.0]);
        s += &row([e.sv_accuracy_m, f64::from(e.health), e.tgd, e.iodc]);
        s += &format!("    {}{}\n", d19(e.toe_sow - 7200.0), d19(e.fit_interval_h));
    }
    s
}

/// An SP3-d file for one GPS day at 15 min: truth + (0.6, −0.4, 0.8) m and +2 ns, no PRN 30.
fn sp3(day: u32) -> String {
    let mut s = format!(
        "#dP2026  9 {day:2}  0  0  0.00000000      96 ORBIT IGS20 HLM  IGS\n\
         %c G  cc GPS ccc cccc cccc cccc cccc ccccc ccccc ccccc ccccc\n\
         /* synthetic precise orbits for T-324\n"
    );
    let day0 = GpsTime::from_calendar(2026, 9, day, 0, 0, 0.0).unwrap();
    for k in 0..96u32 {
        let t = day0.plus_s(900.0 * f64::from(k));
        s += &format!(
            "*  2026  9 {day:2} {:2} {:2}  0.00000000\n",
            k / 4,
            (k % 4) * 15
        );
        for e in truth().iter().filter(|e| e.prn != NOT_IN_SP3) {
            let st = e.state_at(t);
            s += &format!(
                "PG{:02}{:14.6}{:14.6}{:14.6}{:14.6}\n",
                e.prn,
                (st.ecef_m[0] + 0.6) / 1e3,
                (st.ecef_m[1] - 0.4) / 1e3,
                (st.ecef_m[2] + 0.8) / 1e3,
                (st.clock_s + 2e-9) * 1e6
            );
        }
    }
    s + "EOF\n"
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hk-context-t324-{tag}-{}-{}",
        std::process::id(),
        uuid::Uuid::now_v7()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// 2026-09-21T06:00:00Z: the day after, when a final product would be available.
fn fetched_at() -> Timestamp {
    hk_context::utc::parse_utc("2026-09-21T06:00:00Z").unwrap()
}

struct Env {
    dir: PathBuf,
    cache: FeedCache,
    repo: Repository,
}

impl Env {
    fn new(tag: &str) -> Self {
        let dir = temp_dir(tag);
        fs::write(dir.join(format!("{SP3_KEY}.sp3")), sp3(20)).unwrap();
        fs::write(dir.join(format!("{BRDC_KEY}.rnx")), rinex(&truth())).unwrap();
        Self {
            cache: FeedCache::open(&dir).unwrap(),
            repo: Repository::open_in_memory().unwrap(),
            dir,
        }
    }

    fn fetch_sp3(&mut self, key: &str) {
        let mut f = DirectoryFetcher {
            dir: self.dir.clone(),
            fetched_at: fetched_at(),
        };
        refresh(
            &self.cache,
            &mut self.repo,
            &mut f,
            &Sp3Adapter,
            key,
            fetched_at(),
        )
        .unwrap();
    }

    fn fetch_brdc(&mut self) {
        let mut f = DirectoryFetcher {
            dir: self.dir.clone(),
            fetched_at: fetched_at(),
        };
        refresh(
            &self.cache,
            &mut self.repo,
            &mut f,
            &BrdcAdapter,
            BRDC_KEY,
            fetched_at(),
        )
        .unwrap();
    }

    /// The receiver's nav log goes through the same RINEX parser as the reference.
    fn report(&self, now: Timestamp) -> ForensicsReport {
        let local = parse_nav(&rinex(&received())).unwrap().ephemerides;
        compare_from_cache(
            &self.cache,
            SP3_KEY,
            BRDC_KEY,
            &local,
            now,
            &ForensicsConfig::default(),
        )
        .unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.dir).ok();
    }
}

fn check(report: &ForensicsReport, prn: u8) -> &SvCheck {
    report.checks.iter().find(|c| c.prn == prn).unwrap()
}

/// The findings (spoofed orbit, clock jump) and the healthy satellites, against `reference`.
fn assert_findings(report: &ForensicsReport, reference_for: impl Fn(u8) -> Reference, tol_m: f64) {
    for c in &report.checks {
        let (residuals, flags) = match &c.verdict {
            SvVerdict::Agrees(r) => (r, None),
            SvVerdict::Disagrees {
                residuals,
                orbit,
                clock,
            } => (residuals, Some((*orbit, *clock))),
            other => panic!("PRN {} was not compared: {other:?}", c.prn),
        };
        assert_eq!(residuals.reference, reference_for(c.prn), "PRN {}", c.prn);
        assert!(residuals.samples >= 16, "PRN {}: {residuals:?}", c.prn);
        match c.prn {
            SPOOFED => {
                assert_eq!(flags, Some((true, false)), "PRN {SPOOFED}: {residuals:?}");
                assert!(residuals.max_orbit_m > 1_000.0, "{residuals:?}");
            }
            CLOCK_JUMP => {
                assert_eq!(
                    flags,
                    Some((false, true)),
                    "PRN {CLOCK_JUMP}: {residuals:?}"
                );
                let ns = residuals.max_clock_ns.unwrap();
                assert!((ns - 1000.0).abs() < 5.0, "clock step {ns} ns");
            }
            prn => {
                assert_eq!(flags, None, "PRN {prn} is healthy: {residuals:?}");
                assert!(residuals.max_orbit_m < tol_m, "PRN {prn}: {residuals:?}");
                assert!(
                    residuals.max_clock_ns.unwrap() < 5.0,
                    "PRN {prn}: {residuals:?}"
                );
            }
        }
    }
    let flagged: Vec<u8> = report.flagged().map(|c| c.prn).collect();
    assert_eq!(flagged, vec![SPOOFED, CLOCK_JUMP]);
}

#[test]
fn signal_032_nothing_cached_reads_not_yet_fetched_never_agreement() {
    let env = Env::new("empty");
    let report = env.report(fetched_at());
    assert!(matches!(
        &report.precise,
        ReferenceAvailability::NotYetFetched {
            last_attempt: None,
            ..
        }
    ));
    assert!(!report.broadcast.is_cached());
    assert_eq!(report.checks.len(), PRNS.len());
    for c in &report.checks {
        assert_eq!(c.verdict, SvVerdict::NotYetFetched, "PRN {}", c.prn);
        assert!(!c.verdict.is_agreement());
    }
    assert_eq!(report.flagged().count(), 0);
    assert_eq!(report.uncompared().count(), PRNS.len());
}

#[test]
fn signal_032_offline_refresh_records_the_attempt_and_still_compares_nothing() {
    let mut env = Env::new("offline");
    let now = fetched_at();
    for (adapter, key) in [
        (&Sp3Adapter as &dyn hk_context::FeedAdapter, SP3_KEY),
        (&BrdcAdapter, BRDC_KEY),
    ] {
        let err = refresh(
            &env.cache,
            &mut env.repo,
            &mut OfflineFetcher,
            adapter,
            key,
            now,
        );
        assert!(err.is_err());
    }
    let report = env.report(now);
    for avail in [&report.precise, &report.broadcast] {
        let ReferenceAvailability::NotYetFetched {
            last_attempt,
            last_error,
            ..
        } = avail
        else {
            panic!("{avail:?}")
        };
        assert_eq!(*last_attempt, Some(now));
        assert!(last_error.as_deref().unwrap().contains("offline"));
    }
    assert!(
        report
            .checks
            .iter()
            .all(|c| c.verdict == SvVerdict::NotYetFetched)
    );
}

#[test]
fn signal_032_precise_orbits_flag_the_spoofed_orbit_and_the_clock_jump() {
    let mut env = Env::new("precise");
    env.fetch_sp3(SP3_KEY);
    env.fetch_brdc();
    let now = fetched_at().saturating_add_nanos(3 * 3600 * S);
    let report = env.report(now);
    let ReferenceAvailability::Cached(status) = &report.precise else {
        panic!("{:?}", report.precise)
    };
    assert_eq!(status.cache_age_s, 3.0 * 3600.0);
    assert!(!status.refresh_failed);
    // The SP3 day, in UTC (GPS − 18 s).
    assert_eq!(
        status.coverage.start,
        hk_context::utc::parse_utc("2026-09-19T23:59:42Z").unwrap()
    );
    // PRN 30 is not in the precise file, so it falls back to the broadcast reference.
    assert_findings(
        &report,
        |prn| {
            if prn == NOT_IN_SP3 {
                Reference::Broadcast
            } else {
                Reference::Precise
            }
        },
        2.0,
    );
    // Healthy residuals are the metre-level CoM/APC-style offset, not zero.
    let SvVerdict::Agrees(r) = &check(&report, 3).verdict else {
        panic!()
    };
    assert!(
        (r.max_orbit_m - (0.36f64 + 0.16 + 0.64).sqrt()).abs() < 0.01,
        "{r:?}"
    );
}

#[test]
fn signal_032_broadcast_reference_alone_still_finds_both() {
    let mut env = Env::new("brdc");
    env.fetch_brdc();
    let report = env.report(fetched_at());
    assert!(!report.precise.is_cached());
    assert!(report.broadcast.is_cached());
    assert_findings(&report, |_| Reference::Broadcast, 0.01);
}

#[test]
fn signal_032_precise_only_names_the_missing_satellite() {
    let mut env = Env::new("sp3only");
    env.fetch_sp3(SP3_KEY);
    let report = env.report(fetched_at());
    assert_eq!(
        check(&report, NOT_IN_SP3).verdict,
        SvVerdict::NoReferenceForSv
    );
    assert!(check(&report, 3).verdict.is_agreement());
    assert_eq!(report.flagged().count(), 2);
}

#[test]
fn signal_032_a_reference_for_another_day_is_not_covered_not_agreement() {
    let mut env = Env::new("otherday");
    // The wrong day's product cached under the key asked for.
    fs::write(env.dir.join(format!("{SP3_KEY}.sp3")), sp3(21)).unwrap();
    env.fetch_sp3(SP3_KEY);
    let report = env.report(fetched_at());
    assert!(report.precise.is_cached());
    for c in &report.checks {
        assert_eq!(c.verdict, SvVerdict::NotCovered, "PRN {}", c.prn);
    }
    assert_eq!(report.flagged().count(), 0);
}

#[test]
fn signal_032_a_failed_refresh_keeps_the_cache_usable_and_says_so() {
    let mut env = Env::new("stale");
    env.fetch_sp3(SP3_KEY);
    env.fetch_brdc();
    let later = fetched_at().saturating_add_nanos(2 * 86_400 * S);
    assert!(
        refresh(
            &env.cache,
            &mut env.repo,
            &mut OfflineFetcher,
            &Sp3Adapter,
            SP3_KEY,
            later
        )
        .is_err()
    );
    let report = env.report(later);
    let ReferenceAvailability::Cached(status) = &report.precise else {
        panic!("{:?}", report.precise)
    };
    assert!(status.refresh_failed);
    assert!(status.last_error.as_deref().unwrap().contains("offline"));
    assert_eq!(status.cache_age_s, 2.0 * 86_400.0);
    assert_findings(
        &report,
        |prn| {
            if prn == NOT_IN_SP3 {
                Reference::Broadcast
            } else {
                Reference::Precise
            }
        },
        2.0,
    );
}

#[test]
fn signal_032_a_malformed_snapshot_fails_loudly_and_caches_nothing() {
    let mut env = Env::new("garbled");
    let garbled = sp3(20).replacen("PG03", "PG0x", 1);
    fs::write(env.dir.join(format!("{SP3_KEY}.sp3")), garbled).unwrap();
    let mut f = DirectoryFetcher {
        dir: env.dir.clone(),
        fetched_at: fetched_at(),
    };
    assert!(
        refresh(
            &env.cache,
            &mut env.repo,
            &mut f,
            &Sp3Adapter,
            SP3_KEY,
            fetched_at()
        )
        .is_err()
    );
    let report = env.report(fetched_at());
    let ReferenceAvailability::NotYetFetched { last_error, .. } = &report.precise else {
        panic!("{:?}", report.precise)
    };
    assert!(last_error.as_deref().unwrap().contains("satellite id"));
}
