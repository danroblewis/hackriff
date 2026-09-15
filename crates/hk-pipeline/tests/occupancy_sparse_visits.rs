//! T-147: sparse-visit occupancy through the mock SDR under the default scheduler + bandit (T-124's
//! visit pattern: two 1 s IQ windows per 15-min interval, 6 h). The default sweep's first hop is
//! tuned 12.5 kHz from two Markov channels, inside the detector's DC rule, so their detections
//! there are DC-flagged; the same carriers are detected clean from the other hop and the bandit.
//! A DC flag is per tuning (ADR-0012 §2.6), so those visits count towards FCO and the channels are
//! learned. Blind: the pipeline sees only the device; the hidden truth is read in assertions only.

mod common;

use common::*;
use hk_core::{MockEnd, Pacing};
use hk_e2e::scene::SceneTruth;
use hk_e2e::{SynthOutput, SynthRequest};
use hk_model::attention::occupancy::OccupancySubject;
use hk_model::{FreqRange, ScanPolicy, TimeRange};
use hk_pipeline::{Pipeline, PipelineConfig, TrackInventory, open_mock_replay, replay_plan};
use serde_json::json;

const SAMPLE_RATE: f64 = 500e3;
const SPAN_H: f64 = 6.0;
const INTERVAL_S: f64 = 900.0;
const EDGE_S: f64 = 5.0;
const WINDOW_S: f64 = 1.0;

// A-priori thresholds (set before the fixed run). The truth must lie in the row's Wilson CI from
// its reported n_eff (ADR-0012 §2.4), over at least `MIN_REVISITS` activity-independent visits.
/// §2.5 rule 4's 30 visits. (Corrected after the first run from `n_eff >= 30`, a derivation
/// error: §2.5's 30 counts visits n, while §2.4's n_eff = n(1−ρ)/(1+ρ) with ρ the lag-1
/// autocorrelation of consecutive visit states (T-118 amendment) is ≈ state changes / (4p(1−p));
/// visits bunch in 48 one-second windows, so it is bounded by the ≤ 47 window-to-window changes,
/// not by n. Measured at 433.400: n 475, n_eff 13.5.)
const MIN_REVISITS: u64 = 30;
/// At most this share of a clean carrier's visits may stay suspect (a DC flag without a twin).
const MAX_SUSPECT_SHARE: f64 = 0.05;

fn scene() -> Option<SynthOutput> {
    let n = (SPAN_H * 3600.0 / INTERVAL_S).round() as usize;
    let times: Vec<String> = (0..n)
        .flat_map(|k| {
            let t0 = k as f64 * INTERVAL_S;
            [t0 + EDGE_S, t0 + INTERVAL_S - EDGE_S - WINDOW_S]
        })
        .map(|t| format!("{t}"))
        .collect();
    let request = SynthRequest::new("occupancy_markov_scene")
        .seed(7)
        .param("span_hours", SPAN_H)
        .param("sample_rate", SAMPLE_RATE)
        .param("iq_windows_at_revisits", "true")
        .param("revisit_mode", "given")
        .param("revisit_times_s", times.join(","))
        .param("window_duration_s", WINDOW_S);
    match request.generate() {
        Ok(out) => Some(out),
        Err(err) if err.is_unavailable() && !hk_e2e::synth::require_synth() => {
            eprintln!("SKIP {}: {err}", module_path!());
            None
        }
        Err(err) => panic!("synthetic scenario generation failed: {err}"),
    }
}

#[test]
fn occupancy_sparse_visits_dc_flag_is_per_tuning() {
    let Some(out) = scene() else { return };
    let dir = TempDir::new("t147");
    let src = TempDir::new("t147-src");
    let rec = hk_e2e::scene::join_scene_windows(&out, &src.0, "scene").unwrap();
    let blind = blind_meta(&rec.meta, &src.0);
    let replay = open_mock_replay(&blind, Pacing::Unpaced, MockEnd::Stop).unwrap();
    let (centre, rate, t_first) = (
        replay.info.center_hz,
        replay.info.sample_rate_hz,
        replay.info.start_time,
    );
    let mut plan = replay_plan(centre, rate, t_first);
    plan.policy = ScanPolicy::SweepThenDwell;
    plan.extra = json!({
        "bandit": { "min_dwell_s": 0.5, "max_dwell_s": 2.0, "sweep_floor_window_s": 10.0 }
    });
    let mut cfg = PipelineConfig::new(dir.0.join("data"), plan).unwrap();
    cfg.source_class = replay.class;
    cfg.lossless = true;
    cfg.drive_scheduler = true;
    cfg.device_id = replay.device.device_id.clone();
    let handle = Pipeline::start(
        cfg,
        Box::new(replay.source),
        replay.info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let occ = handle.occupancy();
    let product = handle.floor_product();
    let summary = handle.wait().unwrap();
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    let t_end = product
        .lock()
        .unwrap()
        .uncalibrated_pyramid()
        .latest_frame_end()
        .unwrap();
    let span = TimeRange::new(t_first, t_end.saturating_add_nanos(1_000_000_000));
    let band = FreqRange::centered(centre, 0.9 * rate);
    let (_, f_cell) = occ.plan_info();
    let (_, _, _, channels) = occ.channels(band);
    let rows = occ.span_stats(band, span).unwrap();

    // Hidden truth, read only from here on.
    let truth = SceneTruth::load(&out).unwrap();
    let windowed_fco = |ch: u64| {
        truth
            .intervals(ch)
            .iter()
            .map(|(s, e)| {
                truth
                    .window_starts_s
                    .iter()
                    .map(|w| (e.min(w + WINDOW_S) - s.max(*w)).max(0.0))
                    .sum::<f64>()
            })
            .sum::<f64>()
            / (truth.window_starts_s.len() as f64 * WINDOW_S)
    };
    let check = |f_hz: f64| {
        let c = truth
            .channels
            .iter()
            .find(|c| (c.center_hz - f_hz).abs() < 1e3)
            .unwrap_or_else(|| panic!("scene has no channel at {f_hz}"));
        let fco_truth = windowed_fco(c.channel);
        let learned = channels.iter().any(|ch| {
            let f = ch.key.freq(f_cell);
            f.lo_hz <= c.center_hz && c.center_hz <= f.hi_hz
        });
        assert!(learned, "{f_hz}: not learned; channels {channels:?}");
        let row = rows
            .iter()
            .find(|r| match r.subject {
                OccupancySubject::Channel { key } => {
                    let f = key.freq(f_cell);
                    f.lo_hz <= c.center_hz && c.center_hz <= f.hi_hz
                }
                OccupancySubject::Band { .. } => false,
            })
            .unwrap_or_else(|| panic!("{f_hz}: no channel row"));
        eprintln!(
            "T147 {f_hz}: truth {fco_truth:.3} fco {:?} upper {:?} n {} occupied {} suspect {} ci {:?}",
            row.fco,
            row.fco_suspect_upper,
            row.n_revisits,
            row.n_occupied,
            row.n_suspect,
            row.confidence
        );
        let ci = row
            .confidence
            .unwrap_or_else(|| panic!("{f_hz}: no CI: {row:?}"));
        assert!(row.fco.is_some(), "{f_hz}: no fco: {row:?}");
        assert!(row.n_revisits >= MIN_REVISITS, "{f_hz}: {row:?}");
        assert!(
            ci.lo <= fco_truth && fco_truth <= ci.hi,
            "{f_hz}: truth {fco_truth:.3} outside {ci:?}: {row:?}"
        );
        (fco_truth, row.clone())
    };
    for c in &channels {
        let f = c.key.freq(f_cell);
        eprintln!(
            "T147 channel {:.4}-{:.4} MHz obw {:.0} ev {}",
            f.lo_hz / 1e6,
            f.hi_hz / 1e6,
            c.obw_hz,
            c.evidence
        );
    }
    for c in &truth.channels {
        let row = rows.iter().find(|r| match r.subject {
            OccupancySubject::Channel { key } => {
                let f = key.freq(f_cell);
                f.lo_hz <= c.center_hz && c.center_hz <= f.hi_hz
            }
            OccupancySubject::Band { .. } => false,
        });
        eprintln!(
            "T147 truth {} {:.4} MHz windowed fco {:.3}: {:?}",
            c.kind,
            c.center_hz / 1e6,
            windowed_fco(c.channel),
            row.map(|r| (r.fco, r.n_revisits, r.n_suspect, r.confidence))
        );
    }
    for f in [433.400e6, 433.375e6] {
        let (_, row) = check(f);
        assert!(
            row.n_suspect as f64 <= MAX_SUSPECT_SHARE * row.n_revisits as f64,
            "{f}: {} of {} visits suspect",
            row.n_suspect,
            row.n_revisits
        );
    }
    // The always-on channel far from any LO is unchanged.
    let (t425, row) = check(433.425e6);
    assert_eq!(t425, 1.0);
    assert_eq!((row.fco, row.n_suspect), (Some(1.0), 0), "{row:?}");
}
