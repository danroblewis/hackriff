//! T-173: a carrier exactly at a default sweep hop's LO is learned through the mock SDR under a
//! sweep-only schedule (no dwells, so no other tuning can rescue it; T-124's 433.375 MHz channel
//! had only its hop's DC views and the other hop's edge zone). Even passes see it as a DC hit; odd
//! passes tune the hop 80 kHz away (ADR-0005, `SchedulerConfig::dc_dither_hz`), so it gets clean
//! views, a non-suspect FCO and a channel. T-147's sparse visit pattern (two 1 s IQ windows per
//! 15-min interval, 6 h). Blind: the pipeline sees only the device; the hidden truth is read in
//! assertions only.

mod common;

use common::*;
use hk_core::scheduler::{Scheduler, SchedulerConfig, SyntheticClock};
use hk_core::{MockEnd, Pacing, SourceControl};
use hk_e2e::scene::SceneTruth;
use hk_e2e::{SynthOutput, SynthRequest};
use hk_model::attention::occupancy::OccupancySubject;
use hk_model::{FreqRange, ScanPolicy, TimeRange};
use hk_pipeline::{Pipeline, PipelineConfig, TrackInventory, open_mock_replay, replay_plan};

const SAMPLE_RATE: f64 = 500e3;
const SPAN_H: f64 = 6.0;
const INTERVAL_S: f64 = 900.0;
const EDGE_S: f64 = 5.0;
const WINDOW_S: f64 = 1.0;
/// The scene's Markov channels sit at −6, −5, −4, −3 spacings from its centre. The default replay
/// plan at 500 kS/s tiles centre ± 225 kHz with two 225 kHz hops, so the lower hop's LO is at
/// −112.5 kHz = −4 spacings: the FCO 0.5 channel (asserted a priori below, from the compiled plan).
const SPACING_HZ: f64 = 28_125.0;

// A-priori thresholds, as in `occupancy_sparse_visits` (T-147): the truth inside the row's Wilson
// CI from its n_eff (ADR-0012 §2.4), over at least §2.5 rule 4's 30 visits, with at most 5 % of
// the visits suspect.
const MIN_REVISITS: u64 = 30;
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
        .seed(11)
        .param("span_hours", SPAN_H)
        .param("sample_rate", SAMPLE_RATE)
        .param("channel_spacing_hz", SPACING_HZ)
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
fn occupancy_carrier_at_a_hop_lo_is_learned_with_a_clean_fco() {
    let Some(out) = scene() else { return };
    let dir = TempDir::new("t173");
    let src = TempDir::new("t173-src");
    let rec = hk_e2e::scene::join_scene_windows(&out, &src.0, "scene").unwrap();
    let blind = blind_meta(&rec.meta, &src.0);
    let replay = open_mock_replay(&blind, Pacing::Unpaced, MockEnd::Stop).unwrap();
    let (centre, rate, t_first) = (
        replay.info.center_hz,
        replay.info.sample_rate_hz,
        replay.info.start_time,
    );
    let mut plan = replay_plan(centre, rate, t_first);
    plan.policy = ScanPolicy::SweepOnly;

    // The schedule, compiled independently as the pipeline's scheduler compiles it (a priori).
    let caps = replay.source.mock_control().capabilities().clone();
    let mut scfg = SchedulerConfig::from_plan(&plan).unwrap();
    scfg.sweep_rate_hz = rate;
    scfg.dwell_min_rate_hz = rate;
    scfg.max_span_hz = rate;
    let expected = Scheduler::new(&plan, scfg, &caps, SyntheticClock::new(t_first))
        .unwrap()
        .plan()
        .clone();
    let lo = expected
        .hops
        .iter()
        .map(|h| h.center_hz)
        .min_by(f64::total_cmp)
        .unwrap();
    assert!(
        (lo - (centre - 4.0 * SPACING_HZ)).abs() < 1.0,
        "hop LOs {:?}",
        expected.hops
    );
    assert!(expected.hops.iter().all(|h| h.dither_hz.abs() == 80e3));

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
    assert_eq!(summary.counter("/scheduler/dwell_steps"), 0, "sweep only");
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
    let c = truth
        .channels
        .iter()
        .find(|c| (c.center_hz - lo).abs() < 1.0)
        .unwrap_or_else(|| panic!("scene has no channel at the hop LO {lo}"));
    let fco_truth = truth
        .intervals(c.channel)
        .iter()
        .map(|(s, e)| {
            truth
                .window_starts_s
                .iter()
                .map(|w| (e.min(w + WINDOW_S) - s.max(*w)).max(0.0))
                .sum::<f64>()
        })
        .sum::<f64>()
        / (truth.window_starts_s.len() as f64 * WINDOW_S);
    let holds = |key: &hk_model::attention::occupancy::ChannelKey| {
        let f = key.freq(f_cell);
        f.lo_hz <= c.center_hz && c.center_hz <= f.hi_hz
    };
    for ch in &channels {
        let f = ch.key.freq(f_cell);
        eprintln!(
            "T173 channel {:.4}-{:.4} MHz obw {:.0} ev {}",
            f.lo_hz / 1e6,
            f.hi_hz / 1e6,
            ch.obw_hz,
            ch.evidence
        );
    }
    assert!(
        channels.iter().any(|ch| holds(&ch.key)),
        "{} at the hop LO: not learned; channels {channels:?}",
        c.center_hz
    );
    let row = rows
        .iter()
        .find(|r| matches!(r.subject, OccupancySubject::Channel { key } if holds(&key)))
        .unwrap_or_else(|| panic!("no channel row at {}", c.center_hz));
    eprintln!(
        "T173 {} {:.4} MHz: truth {fco_truth:.3} fco {:?} upper {:?} n {} occupied {} suspect {} ci {:?}",
        c.kind,
        c.center_hz / 1e6,
        row.fco,
        row.fco_suspect_upper,
        row.n_revisits,
        row.n_occupied,
        row.n_suspect,
        row.confidence
    );
    assert!(row.fco.is_some(), "no fco: {row:?}");
    assert!(row.n_revisits >= MIN_REVISITS, "{row:?}");
    assert!(
        row.n_suspect as f64 <= MAX_SUSPECT_SHARE * row.n_revisits as f64,
        "{} of {} visits suspect",
        row.n_suspect,
        row.n_revisits
    );
    let ci = row.confidence.unwrap_or_else(|| panic!("no CI: {row:?}"));
    assert!(
        ci.lo <= fco_truth && fco_truth <= ci.hi,
        "truth {fco_truth:.3} outside {ci:?}: {row:?}"
    );
}
