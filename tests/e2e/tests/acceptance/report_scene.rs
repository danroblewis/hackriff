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
//! top emitter; the baseline comparison is `unavailable`; CSV and PNG exports render.

use std::sync::{Arc, Mutex};

use hk_e2e::scene::SceneTruth;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::attention::baseline::SiteKey;
use hk_model::attention::report::{ComparisonStatus, ExportFormat};
use hk_model::{FreqRange, Repository, TimeRange};
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
            .param("revisit_mean_gap_s", 1800)
            .param("window_duration_s", WINDOW_S)
    );
    let dir = TempDir::new("t121");
    let scene = blind_scene(&dir.0, &out, json!({}));
    let rec = scene.recording.clone();
    let (centre, rate) = (
        scene.device.info.center_hz,
        scene.device.info.sample_rate_hz,
    );
    let handle = start(scene.cfg, scene.device);
    let product = handle.floor_product();
    let observations = handle.observation_store();
    let _ = finish(handle);

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
    let service = ReportService::new(None, Some(product), observations, db);
    let report = service.report(region, span, SiteKey::Unassigned).unwrap();
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
    assert_eq!(
        report.change_vs_baseline.status,
        ComparisonStatus::Unavailable,
        "[{T121}] no baselines on this server"
    );
    assert!(report.change_vs_baseline.changes.is_empty());

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
                e.fco.map(|f| (f * 1000.0).round() / 1000.0),
                e.top_suggestion.clone()
            ))
            .collect::<Vec<_>>()
    );
    assert!(!found.is_empty(), "[{T121}] no scene channel found blind");
    assert!(
        !report.top_emitters.is_empty() && report.top_emitters.iter().all(|e| e.sightings > 0),
        "[{T121}] top emitters"
    );
    if found.len() <= report.top_emitters.len() || report.top_emitters.len() < 20 {
        for ch in &found {
            assert!(
                listed(ch),
                "[{T121}] {} channel at {:.4} MHz was found blind but is not a top emitter",
                ch.kind,
                ch.center_hz / 1e6
            );
        }
    }
    assert!(
        report.occupancy.channels.iter().any(|s| s.fco.is_some()),
        "[{T121}] channel occupancy rows"
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
