//! T-276 — SIGNAL-033 (schedule automated passes), SIGNAL-027 (Meteor-M scheduled revisit): a
//! cached TLE feed (C29) predicts pass windows and the C04 attention scheduler reserves dwell for
//! them, with pre-emption stated. Offline-first: the element sets come from the feed cache after
//! the network is gone, and TLE age is reported with every pass rather than silently trusted.
//!
//! The element sets are **synthetic** (hand-made sun-synchronous and ISS-like orbits, valid
//! checksums), so no feed terms apply. SGP4 itself is checked against the published verification
//! vectors in `passes::sgp4`'s unit tests; here the pass finder is checked against a brute-force
//! elevation scan, and the geometry against the sub-satellite point.

use std::fs;
use std::path::PathBuf;

use hk_context::Site;
use hk_context::feeds::tle::{self, PassRequest, TleAdapter, plan_from_cache};
use hk_context::feeds::{DirectoryFetcher, FeedCache, FeedError, OfflineFetcher, refresh};
use hk_context::passes::look::{ecef_to_geodetic, teme_to_ecef};
use hk_context::passes::tle::checksum;
use hk_context::passes::{
    Disposition, Downlink, Freshness, Pass, PassPlanConfig, PredictConfig, Sgp4, Tle, Tracker,
    parse_set, pass_event, plan_passes,
};
use hk_core::SourceCapabilities;
use hk_core::scheduler::{Clock, Purpose, Scheduler, SchedulerConfig, SyntheticClock};
use hk_model::attention::observation::{LeaseKind, Tier};
use hk_model::{FreqRange, Geo, PlanRegion, Repository, ScanPlan, ScanPolicy, Schedule, Timestamp};
use serde_json::Value;

const S: i64 = 1_000_000_000;
const DAY: i64 = 86_400 * S;

/// 2026-09-20T12:00:00Z, the synthetic element epoch (day 263.5 of 2026).
fn epoch() -> Timestamp {
    hk_context::utc::parse_utc("2026-09-20T12:00:00Z").unwrap()
}

fn site() -> Site {
    Site {
        lat_deg: 51.48,
        lon_deg: -0.01,
        alt_m: Some(20.0),
    }
}

fn with_checksum(line: String) -> String {
    assert_eq!(line.len(), 68, "{line:?}");
    let c = checksum(&line);
    format!("{line}{c}")
}

/// A 3-line element set with valid checksums (epoch day 263.5 of 2026); `elements` are
/// inclination, RAAN, eccentricity, argument of perigee, mean anomaly (degrees) and mean motion
/// (rev/day).
fn tle_text(name: &str, norad: u32, elements: [f64; 6]) -> String {
    let [inc, raan, ecc, argp, ma, mm] = elements;
    let l1 = with_checksum(format!(
        "1 {norad:05}U 26001A   26{:012.8}  .00000000  00000-0  10000-3 0  999",
        263.5
    ));
    let l2 = with_checksum(format!(
        "2 {norad:05} {inc:8.4} {raan:8.4} {:07} {argp:8.4} {ma:8.4} {mm:11.8}{:5}",
        (ecc * 1e7).round() as u64,
        1234
    ));
    format!("{name}\n{l1}\n{l2}\n")
}

const METEOR: u32 = 90_001;
const APT: u32 = 90_002;
const STATION: u32 = 90_003;

fn snapshot() -> String {
    [
        tle_text(
            "SYNTH METEOR",
            METEOR,
            [98.62, 300.0, 0.0004, 90.0, 270.0, 14.2388],
        ),
        tle_text(
            "SYNTH APT",
            APT,
            [99.05, 260.0, 0.0013, 120.0, 240.0, 14.1256],
        ),
        tle_text(
            "SYNTH STATION",
            STATION,
            [51.64, 40.0, 0.0006, 30.0, 330.0, 15.4960],
        ),
    ]
    .concat()
}

fn downlinks() -> Vec<Downlink> {
    vec![
        Downlink {
            norad_id: METEOR,
            center_hz: 137.9e6,
            bandwidth_hz: 150e3,
        },
        Downlink {
            norad_id: APT,
            center_hz: 137.1e6,
            bandwidth_hz: 40e3,
        },
        Downlink {
            norad_id: STATION,
            center_hz: 145.8e6,
            bandwidth_hz: 16e3,
        },
    ]
}

fn tles() -> Vec<Tle> {
    parse_set(&snapshot()).unwrap()
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hk-context-t276-{tag}-{}-{}",
        std::process::id(),
        uuid::Uuid::now_v7()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn element_sets_parse_and_malformed_ones_fail_loudly_with_the_line() {
    let set = tles();
    assert_eq!(set.len(), 3);
    assert_eq!(set[0].name, "SYNTH METEOR");
    assert_eq!(set[0].epoch, epoch());
    assert!((set[0].bstar - 1e-4).abs() < 1e-12);
    // A flipped digit breaks the checksum: refused, naming the line, not skipped.
    let bad = snapshot().replacen("98.6200", "98.6201", 1);
    let e = parse_set(&bad).unwrap_err();
    assert_eq!(e.line, 3, "{e}");
    assert!(e.message.contains("checksum"), "{e}");
    let truncated: String = snapshot().lines().take(2).collect::<Vec<_>>().join("\n");
    assert!(parse_set(&truncated).is_err());
}

#[test]
fn the_sub_satellite_point_sees_the_satellite_overhead() {
    for tle in tles() {
        let sat = Sgp4::new(&tle).unwrap();
        for minutes in [0.0, 97.0, 1440.0] {
            let t = tle.epoch.saturating_add_nanos((minutes * 60e9) as i64);
            let (r, _) = teme_to_ecef(&sat.propagate(minutes).unwrap(), t);
            let (lat, lon, h) = ecef_to_geodetic(r);
            assert!((300.0..1000.0).contains(&h), "LEO altitude {h} km");
            let under = Tracker::new(&tle, Site::new(lat, lon)).unwrap();
            let l = under.look_at(t).unwrap();
            assert!(l.elevation_deg > 89.9, "{} overhead: {l:?}", tle.name);
            assert!(
                (l.range_km - h).abs() < 0.5,
                "slant range = altitude overhead"
            );
            let antipode = Tracker::new(
                &tle,
                Site::new(-lat, lon + 180.0 - 360.0 * f64::from(lon > 0.0)),
            )
            .unwrap();
            assert!(antipode.look_at(t).unwrap().elevation_deg < -60.0);
        }
    }
}

#[test]
fn the_pass_finder_agrees_with_a_brute_force_elevation_scan() {
    let cfg = PredictConfig::default();
    let (from, to) = (epoch(), epoch().saturating_add_nanos(2 * DAY));
    for tle in tles() {
        let tr = Tracker::new(&tle, site()).unwrap();
        let passes = tr.passes(from, to, &cfg).unwrap();
        // Brute force: 1 s samples over the same span.
        let mut brute: Vec<(i64, i64, f64)> = Vec::new();
        let mut open: Option<(i64, f64)> = None;
        let mut t = from.as_unix_nanos();
        while t <= to.as_unix_nanos() {
            let el = tr
                .look_at(Timestamp::from_unix_nanos(t))
                .unwrap()
                .elevation_deg;
            match (&mut open, el >= 0.0) {
                (None, true) => open = Some((t, el)),
                (Some((_, max)), true) => *max = max.max(el),
                (Some((aos, max)), false) => {
                    brute.push((*aos, t, *max));
                    open = None;
                }
                (None, false) => {}
            }
            t += S;
        }
        // Drop a pass the brute force caught mid-way at `from` (the finder keeps its true AOS).
        if brute.first().is_some_and(|b| b.0 == from.as_unix_nanos()) {
            brute.remove(0);
        }
        let found: Vec<&Pass> = passes
            .iter()
            .filter(|p| p.aos >= from && p.los <= to)
            .collect();
        assert!(
            found.len() >= 4,
            "{}: {} passes in 2 days",
            tle.name,
            found.len()
        );
        assert_eq!(found.len(), brute.len(), "{}", tle.name);
        for (p, (aos, los, max)) in found.iter().zip(&brute) {
            assert!((p.aos.as_unix_nanos() - aos).abs() <= S, "{} AOS", tle.name);
            assert!((p.los.as_unix_nanos() - los).abs() <= S, "{} LOS", tle.name);
            assert!(p.max_elevation_deg >= max - 1e-9 && p.max_elevation_deg - max < 0.05);
            assert!(p.aos < p.tca && p.tca < p.los);
            assert!(p.duration_s() < 20.0 * 60.0, "a LEO pass is minutes long");
            assert_eq!(
                p.tle_epoch, tle.epoch,
                "every pass carries its element epoch"
            );
            // 137 MHz Doppler from LEO is a few kHz (a grazing pass has little radial motion).
            let doppler = p.max_doppler_hz(137.5e6);
            assert!(doppler < 4e3, "{p:?}");
            if p.max_elevation_deg > 10.0 {
                assert!(doppler > 2e3, "{p:?}");
            }
        }
    }
}

/// Planning time an hour before the synthetic passes.
fn before() -> Timestamp {
    epoch().saturating_add_nanos(-3600 * S)
}

fn synthetic_pass(norad: u32, aos_s: i64, dur_s: i64, max_el: f64, tle_age_days: f64) -> Pass {
    let aos = epoch().saturating_add_nanos(aos_s * S);
    Pass {
        norad_id: norad,
        name: format!("SAT {norad}"),
        aos,
        tca: aos.saturating_add_nanos(dur_s * S / 2),
        los: aos.saturating_add_nanos(dur_s * S),
        max_elevation_deg: max_el,
        aos_azimuth_deg: 10.0,
        los_azimuth_deg: 190.0,
        max_range_rate_km_s: 6.5,
        tle_epoch: aos.saturating_add_nanos(-(tle_age_days * 86_400e9) as i64),
    }
}

#[test]
fn tle_age_is_reported_widens_the_margin_and_a_stale_set_is_not_trusted() {
    let cfg = PassPlanConfig::default();
    let dl = downlinks();
    let passes = [
        synthetic_pass(METEOR, 0, 600, 40.0, 1.0),
        synthetic_pass(METEOR, 10_000, 600, 40.0, 7.0),
        synthetic_pass(METEOR, 20_000, 600, 40.0, 30.0),
        synthetic_pass(APT, 30_000, 600, 5.0, 1.0),
    ];
    let plan = plan_passes(&passes, &dl, before(), &cfg);
    let got: Vec<_> = plan
        .passes
        .iter()
        .map(|p| {
            (
                p.freshness,
                p.disposition,
                (p.tle_age_days * 10.0).round() / 10.0,
            )
        })
        .collect();
    assert!(matches!(
        got[0],
        (Freshness::Fresh, Disposition::Reserved { .. }, 1.0)
    ));
    assert!(matches!(
        got[1],
        (Freshness::Aging, Disposition::Reserved { .. }, 7.0)
    ));
    assert!(matches!(
        got[2],
        (Freshness::Stale, Disposition::StaleTle, 30.0)
    ));
    assert!(matches!(
        got[3],
        (Freshness::Fresh, Disposition::BelowElevation, 1.0)
    ));
    assert!(plan.passes[1].margin_s > plan.passes[0].margin_s);
    assert_eq!(plan.reservations.len(), 2);
    assert_eq!(
        plan.oldest_reserved_tle_days(),
        Some(plan.passes[1].tle_age_days)
    );
    // Each reservation covers AOS − margin .. LOS + margin.
    for (r, p) in plan.reservations.iter().zip(&plan.passes) {
        assert_eq!(r.start, p.window.start);
        assert_eq!(r.end(), p.window.end);
        assert_eq!(r.lease.kind, LeaseKind::Pass);
    }

    // Asked to, a stale pass is reserved — still marked stale, with its wide margin.
    let lax = PassPlanConfig {
        reserve_stale: true,
        ..cfg
    };
    let plan = plan_passes(&passes, &dl, before(), &lax);
    assert!(matches!(
        (plan.passes[2].freshness, plan.passes[2].disposition),
        (Freshness::Stale, Disposition::Reserved { .. })
    ));
    assert!(plan.passes[2].margin_s >= 15.0 + 3.0 * 30.0 - 1e-6);
}

#[test]
fn overlapping_passes_share_a_window_when_they_fit_and_otherwise_the_lower_one_loses() {
    let cfg = PassPlanConfig::default();
    let dl = downlinks();
    // APT and METEOR overlap in time and fit one 2.4 Msps window (137.1 + 137.9 MHz); STATION
    // overlaps too but 145.8 MHz cannot share it, and culminates lower.
    let passes = [
        synthetic_pass(APT, 0, 700, 55.0, 0.5),
        synthetic_pass(METEOR, 300, 800, 35.0, 0.5),
        synthetic_pass(STATION, 900, 400, 20.0, 0.5),
    ];
    let plan = plan_passes(&passes, &dl, before(), &cfg);
    let Disposition::Reserved { lease_id } = plan.passes[0].disposition else {
        panic!("{:?}", plan.passes[0].disposition)
    };
    assert_eq!(
        plan.passes[1].disposition,
        Disposition::SharesWindow { lease_id }
    );
    assert_eq!(
        plan.passes[2].disposition,
        Disposition::LostTo {
            lease_id,
            norad_id: APT
        }
    );
    assert_eq!(plan.reservations.len(), 1);
    let r = plan.reservations[0];
    assert_eq!(r.lease.id, lease_id);
    assert_eq!(r.start, plan.passes[0].window.start);
    assert_eq!(
        r.end(),
        plan.passes[1].window.end,
        "the window grew to the union"
    );
    let usable = cfg.rate_hz * cfg.usable_fraction;
    for d in &dl[..2] {
        assert!((d.center_hz - r.lease.center_hz).abs() + d.bandwidth_hz / 2.0 < usable / 2.0);
    }

    // With STATION culminating highest, it wins the time and the 137 MHz pair loses to it.
    let passes = [
        synthetic_pass(APT, 0, 700, 55.0, 0.5),
        synthetic_pass(STATION, 400, 400, 80.0, 0.5),
    ];
    let plan = plan_passes(&passes, &dl, before(), &cfg);
    assert!(matches!(
        plan.passes[1].disposition,
        Disposition::Reserved { .. }
    ));
    assert!(matches!(
        plan.passes[0].disposition,
        Disposition::LostTo {
            norad_id: STATION,
            ..
        }
    ));
}

fn scan_plan() -> ScanPlan {
    ScanPlan {
        id: "01926f3a-0000-7000-8000-000000000276".parse().unwrap(),
        version: 1,
        name: "survey".into(),
        created_at: epoch(),
        regions: vec![PlanRegion {
            freq: FreqRange::new(400e6, 480e6),
            priority: 1.0,
            revisit_ns: None,
        }],
        policy: ScanPolicy::SweepThenDwell,
        gain_table: vec![],
        schedule: Schedule::Continuous,
        extra: Value::Null,
    }
}

#[test]
fn signal_033_cached_tles_drive_scheduler_reservations_offline() {
    let dir = temp_dir("offline");
    fs::write(dir.join("weather.tle"), snapshot()).unwrap();
    let cache = FeedCache::open(&dir).unwrap();
    let mut repo = Repository::open_in_memory().unwrap();
    let fetched = epoch().saturating_add_nanos(6 * 3600 * S);
    let mut online = DirectoryFetcher {
        dir: dir.clone(),
        fetched_at: fetched,
    };
    refresh(
        &cache,
        &mut repo,
        &mut online,
        &TleAdapter,
        "weather",
        fetched,
    )
    .unwrap();

    // Two days later the network is gone: the refresh fails loudly, the cache stays usable.
    let now = fetched.saturating_add_nanos(2 * DAY);
    let err = refresh(
        &cache,
        &mut repo,
        &mut OfflineFetcher,
        &TleAdapter,
        "weather",
        now,
    );
    assert!(matches!(err, Err(FeedError::Fetch { .. })));
    let req = PassRequest {
        site: site(),
        now,
        horizon_s: 86_400.0,
        downlinks: downlinks(),
        predict: PredictConfig::default(),
        plan: PassPlanConfig::default(),
    };
    let sched = plan_from_cache(&cache, "weather", &req).unwrap().unwrap();
    assert!(sched.feed.refresh_failed, "the failed refresh is reported");
    assert!(
        sched
            .feed
            .last_error
            .as_deref()
            .unwrap()
            .contains("offline")
    );
    assert!((sched.feed.cache_age_s - 2.0 * 86_400.0).abs() < 1.0);
    assert_eq!(sched.feed.epochs.start, epoch());
    assert!(sched.refused.is_empty());
    let plan = &sched.plan;
    assert!(
        plan.passes.len() >= 8,
        "{} passes in a day",
        plan.passes.len()
    );
    for p in &plan.passes {
        assert!(
            p.tle_age_days > 2.2 && p.tle_age_days < 3.3,
            "{}",
            p.tle_age_days
        );
        assert_eq!(p.pass.tle_epoch, epoch());
    }
    assert!(!plan.reservations.is_empty());
    // Every reserved or shared pass lies inside its reservation's window.
    for p in &plan.passes {
        let id = match p.disposition {
            Disposition::Reserved { lease_id } | Disposition::SharesWindow { lease_id } => lease_id,
            _ => continue,
        };
        let r = plan.reservations.iter().find(|r| r.lease.id == id).unwrap();
        assert!(r.start <= p.pass.aos && r.end() >= p.pass.los);
    }

    // The scheduler reserves them all and keeps the first window free for the pass.
    let clock = SyntheticClock::new(now);
    let mut s = Scheduler::new(
        &scan_plan(),
        SchedulerConfig::from_plan(&scan_plan()).unwrap(),
        &SourceCapabilities::hackrf_one(),
        clock.clone(),
    )
    .unwrap();
    for r in &plan.reservations {
        s.reserve(*r).unwrap();
    }
    assert_eq!(s.attention_status().reservations, plan.reservations.len());
    let first = plan.reservations[0];
    clock.set(first.start.saturating_add_nanos(-3 * S));
    let mut t = clock.now().as_unix_nanos();
    let mut in_window = 0;
    while t < first.end().as_unix_nanos() {
        let st = s.next_step();
        assert_eq!(st.t_start.as_unix_nanos(), t);
        if st.t_start < first.start {
            assert!(st.purpose.is_discovery(), "{st:?}");
            assert!(st.t_end() <= first.start, "nothing runs into the window");
        } else {
            assert_eq!(
                st.purpose,
                Purpose::Lease {
                    kind: LeaseKind::Pass,
                    lease: first.lease.id
                }
            );
            assert_eq!(st.purpose.tier(), Tier::PinnedLease);
            assert_eq!(st.center_hz, first.lease.center_hz);
            assert!(st.t_end() <= first.end());
            in_window += st.duration_ns;
        }
        clock.advance_ns(st.duration_ns);
        t += st.duration_ns;
    }
    assert_eq!(
        in_window,
        first.lease.duration_ns.unwrap(),
        "the whole window was the pass"
    );
    assert!(s.next_step().purpose.is_discovery());
    assert_eq!(s.stats().reservations_started, 1);
    assert_eq!(s.stats().reservations_late, 0);

    // Much later, still offline: every pass is listed with its age, none trusted enough to book.
    let req = PassRequest {
        now: epoch().saturating_add_nanos(20 * DAY),
        ..req
    };
    let late = plan_from_cache(&cache, "weather", &req).unwrap().unwrap();
    assert!(!late.plan.passes.is_empty());
    assert!(
        late.plan
            .passes
            .iter()
            .all(|p| p.freshness == Freshness::Stale
                && matches!(
                    p.disposition,
                    Disposition::StaleTle | Disposition::BelowElevation
                )
                && p.tle_age_days > 19.0)
    );
    assert!(
        late.plan
            .passes
            .iter()
            .any(|p| p.disposition == Disposition::StaleTle)
    );
    assert!(late.plan.reservations.is_empty());

    // Nothing cached for another group: `None`, never an empty sky.
    assert!(plan_from_cache(&cache, "amateur", &req).unwrap().is_none());
    assert!(tle::load(&cache, "amateur", now).unwrap().is_none());
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn aware_064_passes_become_idempotent_tle_pass_external_events() {
    let tr = Tracker::new(&tles()[0], site()).unwrap();
    let pass = tr
        .passes(
            epoch(),
            epoch().saturating_add_nanos(DAY),
            &PredictConfig::default(),
        )
        .unwrap()
        .remove(0);
    let dl = &downlinks()[0];
    let ev = pass_event(&pass, Some(dl), epoch());
    assert_eq!(
        (ev.source.as_str(), ev.event_type.as_str()),
        ("tle-pass", "pass")
    );
    assert_eq!((ev.time.start, ev.time.end), (pass.aos, pass.los));
    assert!(matches!(
        ev.geo,
        Geo::OrbitPass {
            norad_id: METEOR,
            ..
        }
    ));
    assert!(ev.freq[0].lo_hz < dl.center_hz && ev.freq[0].hi_hz > dl.center_hz);
    assert_eq!(ev.payload["tle_epoch_ns"], epoch().as_unix_nanos());
    let mut repo = Repository::open_in_memory().unwrap();
    let a = repo.upsert_external_event(&ev).unwrap();
    let b = repo
        .upsert_external_event(&pass_event(&pass, Some(dl), epoch()))
        .unwrap();
    assert_eq!(a, b);
}
