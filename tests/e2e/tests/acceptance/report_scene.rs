//! T-121 (AWARE-033, AWARE-042, RESEARCH-050): a survey report over a blind, time-compressed 48 h
//! `occupancy_markov_scene` (T-117) replayed through one mock SDR (T-125 `blind_scene`).
//!
//! The report's region is the device's sampled band and its span runs from the recording's first
//! sample to the stream time history reached: both come from the device and the run, never from
//! the scene schedule. The schedule (truth) is read only in the assertions below.
//!
//! Asserted: the report validates; coverage is disclosed with POI and says unobserved is not
//! quiet; the scene's longest gap between two observation windows is listed as a coverage gap
//! across the band; every scene channel the run found blind (an inventory row at its centre) is a
//! top emitter; CSV and PNG exports render.
//!
//! **T-131 full path (fixed before the first run).** The run's own services are used, as `hk serve`
//! wires them: the user pins a site through the attention API right after start (a parked device;
//! no truth), occupancy rows carry it, baselines fold, candidates publish read-only (bandit off)
//! and the occupancy thread steps the novelty alarms. Asserted:
//! 1. baselines populate (subjects under the pinned site);
//! 2. candidates populate (sets observed as published during the run, as `/api/candidates` would
//!    serve them: every track closes when the run ends, so the final set is empty), and in the
//!    latest published set holding the hour-30 novelty emitter its candidate ranks in the top
//!    max(3, n/4);
//! 3. the report over the pinned site carries true `fco` on channel rows (activity-independent
//!    visits) and a baseline comparison that is `available` or `immature` (printed);
//! 4. the run's alarm service is fed without write errors; alarms and suppressions are printed.
//!    No alarm for the novelty emitter is expected within 48 h: its channel is learned at hour ~30,
//!    so its baseline subject is immature (maturity is never loosened).

use std::sync::{Arc, Mutex};
use std::time::Instant;

use hk_context::report::DEFAULT_MAX_EMITTERS;
use hk_e2e::scene::SceneTruth;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::attention::baseline::SiteKey;
use hk_model::attention::report::{ComparisonStatus, ExportFormat};
use hk_model::attention::score::InterestingnessProvider as _;
use hk_model::repo::alarms::AnomalyQuery;
use hk_model::{FreqRange, Repository, TimeRange};
use hk_pipeline::attention::SiteSelect;
use hk_pipeline::reports::ReportService;
use serde_json::json;

use crate::blind::{blind_scene, inventory_rows, start};
use crate::common::*;

const T121: &str = "T-121";
const HOUR_S: f64 = 3600.0;
const WINDOW_S: f64 = 0.5;

#[test]
fn report_over_48h_scene_discloses_longest_gap_and_blind_top_emitters() {
    let out = synth_or_skip!(
        SynthRequest::new("occupancy_markov_scene")
            .seed(7)
            .param("span_hours", 48)
            .param("iq_windows_at_revisits", "true")
            .param("revisit_mean_gap_s", 900)
            .param("window_duration_s", WINDOW_S)
    );
    let dir = TempDir::new("t121");
    let wall = Instant::now();
    let scene = blind_scene(&dir.0, &out, json!({}));
    let rec = scene.recording.clone();
    let (centre, rate) = (
        scene.device.info.center_hz,
        scene.device.info.sample_rate_hz,
    );
    let handle = start(scene.cfg, scene.device);
    // T-131: the user pins the parked device's site (`PUT /api/sites/current`).
    let attention = handle.attention().expect("the run's attention service");
    attention
        .set_current_site(SiteSelect {
            id: None,
            name: Some("bench".into()),
            lat_deg: Some(51.5),
            lon_deg: Some(-0.1),
            radius_m: None,
            utc_offset_min: Some(0),
            release: false,
        })
        .unwrap();
    let alarms = handle.alarms().expect("the run's alarm service");
    // Published candidate sets, polled like the API (version first, snapshot when it moved).
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let poller = {
        let (provider, stop) = (attention.provider(), Arc::clone(&stop));
        std::thread::spawn(move || {
            let (mut seen, mut sets) = (0, Vec::new());
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let v = provider.version();
                if v != seen {
                    seen = v;
                    let set = provider.snapshot();
                    if !set.candidates.is_empty() {
                        sets.push(set);
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            sets
        })
    };
    let occupancy = handle.occupancy();
    let product = handle.floor_product();
    let observations = handle.observation_store();
    let _ = finish(handle);
    let wall_s = wall.elapsed().as_secs_f64();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let published = poller.join().unwrap();

    // ---- The report, from what the device and the run know. ----
    let meta = hk_model::sigmf::SigmfMeta::read(&rec.meta).unwrap();
    let t_first = hk_core::source::sigmf_replay::parse_sigmf_datetime(
        meta.captures[0].datetime.as_deref().unwrap(),
    )
    .unwrap();
    let t_end = product
        .lock()
        .unwrap()
        .uncalibrated_pyramid()
        .latest_frame_end()
        .unwrap();
    let region = FreqRange::new(centre - rate / 2.0, centre + rate / 2.0);
    let span = TimeRange::new(t_first, t_end);
    let db = Arc::new(Mutex::new(
        Repository::open(dir.0.join("hackriff.db")).unwrap(),
    ));
    let (site, _) = attention.site_at(t_end);
    assert!(
        matches!(site, SiteKey::Site(_)),
        "[{T121}] pinned site {site:?}"
    );
    let service = ReportService::new(None, Some(product), observations, db)
        .with_attention(Some(occupancy), Some(Arc::clone(&attention)));
    let report = service.report(region, span, site).unwrap();
    report.validate().unwrap();
    let c = &report.coverage;
    eprintln!(
        "[{T121}] report: observed {:.4} % ({:.1} s), {} gaps{}, {} never-observed, poi {:?}, \
         {} channels, {} top emitters, {} provenance steps; warnings {:?}",
        c.observed_fraction * 100.0,
        c.observed_s,
        c.gaps.len(),
        if c.gaps_truncated { " (truncated)" } else { "" },
        c.never_observed.len(),
        c.poi.iter().map(|p| (p.tau_s, p.p_poi)).collect::<Vec<_>>(),
        report.occupancy.channels.len(),
        report.top_emitters.len(),
        report.provenance_steps.len(),
        report.warnings
    );
    assert!(
        c.statement.contains("not quiet"),
        "[{T121}] {}",
        c.statement
    );
    assert_eq!(c.poi.len(), 4, "[{T121}] POI rows");
    assert!(
        c.observed_fraction > 0.0 && c.observed_fraction < 0.05,
        "[{T121}] 0.5 s windows every ~30 min: {}",
        c.observed_fraction
    );
    let cmp = &report.change_vs_baseline;
    eprintln!(
        "[T-131] run wall {wall_s:.1} s; change vs baseline: {:?} (resolution {:?}), {} changes: \
         {:?}",
        cmp.status,
        cmp.resolution,
        cmp.changes.len(),
        cmp.changes
            .iter()
            .take(6)
            .map(|c| (c.kind, c.subject, (c.z * 10.0).round() / 10.0))
            .collect::<Vec<_>>()
    );
    assert!(
        matches!(
            cmp.status,
            ComparisonStatus::Available | ComparisonStatus::Immature
        ),
        "[T-131] the pinned site's baselines are compared: {:?}",
        cmp.status
    );

    // ---- Truth, for assertions only. ----
    let truth = SceneTruth::load(&out).unwrap();
    let w = &truth.window_starts_s;
    let t_scene = t_first.saturating_add_nanos(-((w[0] * 1e9).round() as i64));
    let at = |s: f64| t_scene.saturating_add_nanos((s * 1e9).round() as i64);
    let scene_h = |t: hk_model::Timestamp| {
        (t.as_unix_nanos() - t_scene.as_unix_nanos()) as f64 / 1e9 / HOUR_S
    };

    // The longest gap between two windows is disclosed as unobserved across the band.
    let (ga, gb) = w
        .windows(2)
        .map(|p| (p[0] + WINDOW_S, p[1]))
        .max_by(|x, y| (x.1 - x.0).total_cmp(&(y.1 - y.0)))
        .unwrap();
    // Grid rows are coarser than a window: allow two rows of the finest level used (≤ 2 min).
    let tol_ns = 120_000_000_000i64;
    let covering = c.gaps.iter().find(|g| {
        g.time.start.as_unix_nanos() <= at(ga).as_unix_nanos() + tol_ns
            && g.time.end.as_unix_nanos() >= at(gb).as_unix_nanos() - tol_ns
            && g.freq.width_hz() >= 0.5 * region.width_hz()
    });
    eprintln!(
        "[{T121}] longest scene gap {:.2} h..{:.2} h ({:.2} h); report gaps (h): {:?}",
        ga / HOUR_S,
        gb / HOUR_S,
        (gb - ga) / HOUR_S,
        c.gaps
            .iter()
            .take(8)
            .map(|g| (
                (scene_h(g.time.start) * 100.0).round() / 100.0,
                (scene_h(g.time.end) * 100.0).round() / 100.0,
                (g.freq.width_hz() / 1e3).round()
            ))
            .collect::<Vec<_>>()
    );
    assert!(
        covering.is_some(),
        "[{T121}] the longest scene gap ({:.2} h..{:.2} h) is not disclosed as unobserved",
        ga / HOUR_S,
        gb / HOUR_S
    );

    // Every scene channel found blind is a top emitter.
    let rows = inventory_rows(&dir.0);
    let found: Vec<_> = truth
        .channels
        .iter()
        .filter(|ch| {
            let tol = (ch.bandwidth_hz / 2.0).max(10e3);
            rows.iter().any(|r| {
                (r["f_center_hz"].as_f64().unwrap_or(f64::NAN) - ch.center_hz).abs() <= tol
            })
        })
        .collect();
    let listed = |ch: &hk_e2e::scene::SceneChannel| {
        let tol = (ch.bandwidth_hz / 2.0).max(10e3);
        report
            .top_emitters
            .iter()
            .any(|e| (e.freq.center_hz() - ch.center_hz).abs() <= tol)
    };
    let listed_n = found.iter().filter(|ch| listed(ch)).count();
    eprintln!(
        "[{T121}] scene channels: {} in truth, {} found blind in the inventory ({} rows), {} of \
         them top emitters; top: {:?}",
        truth.channels.len(),
        found.len(),
        rows.len(),
        listed_n,
        report
            .top_emitters
            .iter()
            .map(|e| (
                (e.freq.center_hz() / 1e3).round(),
                e.sightings,
                e.fco_all_visits.map(|f| (f * 1000.0).round() / 1000.0),
                e.top_suggestion.clone()
            ))
            .collect::<Vec<_>>()
    );
    assert!(!found.is_empty(), "[{T121}] no scene channel found blind");
    assert!(
        !report.top_emitters.is_empty() && report.top_emitters.iter().all(|e| e.sightings > 0),
        "[{T121}] top emitters"
    );
    assert!(
        found.len() <= DEFAULT_MAX_EMITTERS,
        "[{T121}] cannot evaluate the blind top-emitter assertion: {} scene channels found blind \
         exceed the {DEFAULT_MAX_EMITTERS}-emitter cap (shrink the scene)",
        found.len()
    );
    for ch in &found {
        assert!(
            listed(ch),
            "[{T121}] {} channel at {:.4} MHz was found blind but is not a top emitter",
            ch.kind,
            ch.center_hz / 1e6
        );
    }
    assert!(
        report
            .occupancy
            .channels
            .iter()
            .any(|s| s.fco_all_visits.is_some()),
        "[{T121}] channel occupancy rows"
    );
    // T-131: the series rows carry true `fco` (activity-independent visits, §2.5).
    let with_fco = report
        .occupancy
        .channels
        .iter()
        .filter(|s| s.fco.is_some())
        .count();
    eprintln!(
        "[T-131] report channels with true fco: {with_fco} of {}; top emitters with fco: {}",
        report.occupancy.channels.len(),
        report
            .top_emitters
            .iter()
            .filter(|e| e.fco.is_some())
            .count()
    );
    assert!(with_fco > 0, "[T-131] no channel row carries a true fco");

    // ---- T-131: baselines, candidates and alarms from the live run. ----
    let baselines = attention.baselines_json(None).unwrap();
    let subjects: u64 = baselines["baselines"]
        .as_array()
        .map(|b| b.iter().filter_map(|k| k["subjects"].as_u64()).sum())
        .unwrap_or(0);
    let mature: u64 = baselines["baselines"]
        .as_array()
        .map(|b| b.iter().filter_map(|k| k["mature_subjects"].as_u64()).sum())
        .unwrap_or(0);
    eprintln!("[T-131] baselines: {subjects} subjects, {mature} mature: {baselines}");
    assert!(subjects > 0, "[T-131] baselines did not populate");

    let novelty = truth.channel("novelty").unwrap();
    let at_novelty = |f: &FreqRange| {
        f.lo_hz <= novelty.center_hz + novelty.bandwidth_hz / 2.0
            && f.hi_hz >= novelty.center_hz - novelty.bandwidth_hz / 2.0
    };
    let (anomalies, _) = alarms
        .list(&AnomalyQuery {
            limit: 1000,
            ..Default::default()
        })
        .unwrap();
    let novelty_alarm = anomalies
        .iter()
        .find(|a| at_novelty(&a.listing.anomaly.region.freq));
    let suppressions = alarms.suppressions();
    eprintln!(
        "[T-131] alarms: {} anomalies ({:?}); novelty alarm {:?}; suppressions {suppressions}; \
         status {}",
        anomalies.len(),
        anomalies
            .iter()
            .take(8)
            .map(|a| (
                a.listing.anomaly.kind,
                (a.listing.anomaly.region.freq.center_hz() / 1e3).round()
            ))
            .collect::<Vec<_>>(),
        novelty_alarm.map(|a| (a.listing.anomaly.kind, a.listing.status)),
        alarms.status_json()
    );
    // No alarm is asserted: the novelty emitter's channel is first learned at scene hour ~30, so
    // its baseline subject has < 24 h of observation by hour 48 (immature, novelty 0). Raising one
    // needs the subject observed ≥ 24 h before the change (a ≥ ~56 h scene with the channel learned
    // early). The live wiring itself is covered by `attention_interval_folds_raise_busier_alarm`.
    let status = alarms.status_json();
    assert_eq!(status["errors"], 0, "[T-131] alarm writes failed");
    // The folds reach the alarms, and the immature ones are counted, not silently dropped (§7.3).
    let observed = status["inputs_observed"].as_u64().unwrap_or(0);
    assert!(
        observed > 0,
        "[T-131] no fold evidence reached the alarms: {status}"
    );
    let immature: u64 = suppressions
        .as_object()
        .map(|by_kind| {
            by_kind
                .values()
                .filter_map(|s| s["immature-baseline"].as_u64())
                .sum()
        })
        .unwrap_or(0);
    eprintln!("[T-131] inputs observed {observed}, immature-baseline suppressions {immature}");
    assert!(
        immature > 0,
        "[T-131] immature folds were not counted as suppressions: {suppressions}"
    );
    let holding: Vec<_> = published
        .iter()
        .filter(|set| set.candidates.iter().any(|c| at_novelty(&c.freq)))
        .collect();
    let ranks: Vec<(f64, usize, usize)> = holding
        .iter()
        .map(|set| {
            let r = set
                .candidates
                .iter()
                .position(|c| at_novelty(&c.freq))
                .unwrap();
            (scene_h(set.t), r + 1, set.candidates.len())
        })
        .collect();
    let latest = holding.last().copied();
    eprintln!(
        "[T-131] candidates: {} non-empty sets published (polled), {} hold the novelty emitter; \
         (scene h, rank, n) latest 6: {:?}; latest top: {:?}",
        published.len(),
        holding.len(),
        &ranks[ranks.len().saturating_sub(6)..],
        latest.map(|set| set
            .candidates
            .iter()
            .take(6)
            .map(|c| (
                (c.freq.center_hz() / 1e3).round(),
                (c.score * 100.0).round() / 100.0,
                (c.novelty.novelty * 100.0).round() / 100.0,
                (c.suspect_fraction * 100.0).round() / 100.0
            ))
            .collect::<Vec<_>>())
    );
    assert!(!published.is_empty(), "[T-131] no candidates published");
    let (_, rank, n) = *ranks
        .last()
        .expect("[T-131] no published candidate set holds the novelty emitter");
    let top = 3.max(n / 4);
    assert!(
        rank <= top,
        "[T-131] the novelty emitter ranks {rank} of {n} (top {top} required)"
    );

    // ---- Exports render in the backend. ----
    let (ct, csv) = service
        .export(region, span, SiteKey::Unassigned, ExportFormat::Csv)
        .unwrap();
    assert_eq!(ct, "text/csv; charset=utf-8");
    let csv = String::from_utf8(csv).unwrap();
    assert!(
        csv.lines()
            .any(|l| l.starts_with("# coverage:") && l.contains("not quiet"))
    );
    assert!(csv.lines().any(|l| l.starts_with("gap,")));
    let (ct, png) = service
        .export(region, span, SiteKey::Unassigned, ExportFormat::Png)
        .unwrap();
    assert_eq!(ct, "image/png");
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
}
