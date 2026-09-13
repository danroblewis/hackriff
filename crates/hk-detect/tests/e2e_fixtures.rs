//! Acceptance on the real HackRF One fixtures (fixtures/hackrf/2026-09-13), replayed as ci8 codes
//! through STFT → floor tracker → detector with per-frame clip counts:
//! - SIGNAL-062: FM 101.3 MHz detected; the 100 MHz reference harmonic flagged `ref-harmonic`;
//!   DC flagged `dc`.
//! - AWARE-036 (recorded): the ≥ 12 dB truth bursts detected, each ±fs/2 edge-alias pair counted
//!   once; false alarms outside truth are a monitored metric (real scene), not asserted.
//! - AWARE-042 overload pair: all 7 known stations detected at the clipped high gain (clipped on
//!   100 %, comb flagged, repository accepts) and at mid gain (not clipped); the gain-step test
//!   refuses the clipped pair.
//! - Negative control (433.62 MHz): no confirmed emitter candidate off the annotated artefacts;
//!   what the 434.000 MHz line looks like is printed.
//!
//! Tests skip when the LFS data is not fetched (`HK_REQUIRE_FIXTURES=1` makes that a failure).

mod common;

use common::*;
use hk_detect::{
    BandProfile, DetectionProfile, DetectionRecord, DetectionWriter, Detector, DetectorConfig,
    GainStepConfig, GainStepSkip, gain_step,
};
use hk_e2e::{Fixture, TruthItem};
use hk_model::detection::SpurReason;
use hk_model::{FreqRange, Repository, SurveyId};

const SIGNAL_062: &[&str] = &["SIGNAL-062"];
const AWARE_036: &[&str] = &["AWARE-036"];
const AWARE_042: &[&str] = &["AWARE-042"];

struct Run {
    fx: Fixture,
    got: Collected,
    det: Detector,
    fs: f64,
    frame_s: f64,
}

fn run(name: &str, chain: ChainConfig, config: DetectorConfig) -> Option<Run> {
    let (fx, iq) = load_ci8_fixture(name)?;
    let fs = fx.sample_rate;
    let mut det = Detector::new(config).unwrap();
    det.retain_capture_results(true);
    let start = std::time::Instant::now();
    let (got, frames) = replay_ci8(&iq, fs, &[(0, fixture_provenance(&fx))], &chain, &mut det);
    eprintln!(
        "{name}: {frames} frames, {} detections, {} confirmations, {} evaluations in {:.1} s",
        got.detections.len(),
        got.confirmations.len(),
        got.evaluations.len(),
        start.elapsed().as_secs_f64()
    );
    Some(Run {
        frame_s: chain.frame_period_s(fs),
        fx,
        got,
        det,
        fs,
    })
}

fn overlapping(got: &Collected, lo: f64, hi: f64) -> Vec<&DetectionRecord> {
    got.detections
        .iter()
        .filter(|d| d.f_lo_hz <= hi && lo <= d.f_hi_hz)
        .collect()
}

fn config() -> DetectorConfig {
    DetectorConfig::new(SurveyId::new())
}

#[test]
fn signal_062_fm_station_detected_ref_harmonic_and_dc_flagged() {
    let Some(r) = run(
        "fm_100p8M_2p4M_l32g30a1_t1p5_5s",
        ChainConfig::new(512, 10),
        config(),
    ) else {
        return;
    };
    let fc = r.fx.center_hz_at(0).unwrap();
    let station = r.fx.of_kind("wfm-broadcast")[0];
    let hits: Vec<_> = overlapping(&r.got, station.f_lo_hz, station.f_hi_hz)
        .into_iter()
        .filter(|d| !d.detection.flags.spur_candidate)
        .collect();
    eprintln!(
        "SIGNAL-062 station {:.4} MHz: {} detections",
        station.center_hz() / 1e6,
        hits.len()
    );
    for d in hits.iter().take(5) {
        eprintln!("  {}", describe(d, r.fs));
    }
    assert!(
        !hits.is_empty(),
        "[SIGNAL-062] 101.3 MHz station not detected"
    );
    let confirmed = r.got.confirmed();
    assert!(
        hits.iter().any(|d| confirmed.contains(&d.detection.id)),
        "[SIGNAL-062] station never an emitter candidate"
    );

    let spur = r.fx.of_kind("spur")[0];
    let spurs: Vec<_> = r
        .got
        .detections
        .iter()
        .filter(|d| (d.detection.f_center_hz - spur.center_hz()).abs() <= 10e3)
        .collect();
    assert!(!spurs.is_empty(), "[SIGNAL-062] 100 MHz spur not detected");
    for d in &spurs {
        assert_eq!(
            d.detection.flags.spur_reason,
            Some(SpurReason::RefHarmonic),
            "[SIGNAL-062] {}",
            describe(d, r.fs)
        );
    }
    let dcs: Vec<_> = r
        .got
        .detections
        .iter()
        .filter(|d| {
            (d.detection.f_center_hz - fc).abs() <= 15e3
                && d.detection.xdb_bandwidth_hz.unwrap() <= 40e3
        })
        .collect();
    assert!(!dcs.is_empty(), "[SIGNAL-062] DC not detected");
    for d in &dcs {
        assert_eq!(
            d.detection.flags.spur_reason,
            Some(SpurReason::Dc),
            "[SIGNAL-062] {}",
            describe(d, r.fs)
        );
    }
    let _ = SIGNAL_062;
}

/// Truth boxes of one transmission: a burst and, for ±fs/2 aliases, its partner.
fn transmissions<'a>(truth: &[&'a TruthItem]) -> Vec<Vec<&'a TruthItem>> {
    let mut groups: Vec<Vec<&TruthItem>> = Vec::new();
    for t in truth {
        let idx = t.f64("burst_index").map(|x| x as i64);
        let alias = t.f64("alias_of_burst_index").map(|x| x as i64);
        if let Some(g) = groups.iter_mut().find(|g| {
            g.iter().any(|o| {
                let oi = o.f64("burst_index").map(|x| x as i64);
                alias.is_some() && oi == alias
                    || o.f64("alias_of_burst_index").map(|x| x as i64) == idx && idx.is_some()
            })
        }) {
            g.push(t);
        } else {
            groups.push(vec![t]);
        }
    }
    groups
}

#[test]
fn aware_036_recorded_ism_bursts_detected_with_edge_aliases_counted_once() {
    let mut cfg = config();
    // S4: the short-burst profile where 2–4 ms bursts matter (ISM), selected per band.
    cfg.band_profiles.push(BandProfile {
        freq: FreqRange::new(902e6, 928e6),
        profile: DetectionProfile::short_burst(),
    });
    let Some(r) = run(
        "ism_915M_10M_l24g30a1_t42p3_1p2s",
        ChainConfig::new(1024, 4),
        cfg,
    ) else {
        return;
    };
    let truth: Vec<&TruthItem> =
        r.fx.emissions()
            .into_iter()
            .filter(|t| t.f64("detector_peak_db").is_some_and(|p| p >= 12.0))
            .collect();
    let groups = transmissions(&truth);
    let (tt, tf) = (2.0 * r.frame_s, 20e3);
    let hit = |t: &TruthItem| {
        r.got.detections.iter().any(|d| {
            d.t_start_s(r.fs) <= t.t_end_s + tt
                && t.t_start_s - tt <= d.t_end_s(r.fs)
                && d.f_lo_hz <= t.f_hi_hz + tf
                && t.f_lo_hz - tf <= d.f_hi_hz
        })
    };
    let mut missed = Vec::new();
    for g in &groups {
        let found = g.iter().any(|t| hit(t));
        let t = g[0];
        eprintln!(
            "  burst {:?}{} {:.3} ms @ {:.4} MHz peak {:.1} dB: {}",
            t.f64("burst_index"),
            if g.len() > 1 { " (+alias)" } else { "" },
            (t.t_end_s - t.t_start_s) * 1e3,
            t.center_hz() / 1e6,
            t.f64("detector_peak_db").unwrap(),
            if found { "detected" } else { "MISSED" }
        );
        if !found {
            missed.push(t.f64("burst_index"));
        }
    }
    let outside = r
        .got
        .detections
        .iter()
        .filter(|d| {
            !r.fx
                .truth
                .iter()
                .filter(|t| t.role != hk_e2e::Role::Scenario)
                .any(|t| {
                    d.t_start_s(r.fs) <= t.t_end_s + tt
                        && t.t_start_s - tt <= d.t_end_s(r.fs)
                        && d.f_lo_hz <= t.f_hi_hz + tf
                        && t.f_lo_hz - tf <= d.f_hi_hz
                })
        })
        .count();
    let exposure = 1.2 * 7.0 / 3600.0;
    eprintln!(
        "AWARE-036 recorded: {} transmissions ({} truth boxes ≥ 12 dB), missed {:?}; {} detections outside truth ({:.0} /MHz/h over {:.4} MHz·h, monitored)",
        groups.len(),
        truth.len(),
        missed,
        outside,
        outside as f64 / exposure,
        exposure
    );
    assert!(
        missed.is_empty(),
        "{AWARE_036:?} missed transmissions {missed:?}"
    );
}

const KNOWN_FM: [f64; 7] = [93.3e6, 94.9e6, 96.5e6, 98.9e6, 101.3e6, 104.5e6, 106.9e6];

fn known_stations_detected(r: &Run) {
    for f in KNOWN_FM {
        let t =
            r.fx.of_kind("wfm-broadcast")
                .into_iter()
                .find(|t| t.f64("channel_hz") == Some(f))
                .unwrap_or_else(|| panic!("no truth for {f}"));
        let hits: Vec<_> = overlapping(&r.got, t.f_lo_hz, t.f_hi_hz)
            .into_iter()
            .filter(|d| !d.detection.flags.spur_candidate)
            .collect();
        eprintln!("  {:.1} MHz: {} detections", f / 1e6, hits.len());
        assert!(
            !hits.is_empty(),
            "{AWARE_042:?} known station {f} not detected"
        );
    }
}

#[test]
fn aware_042_clipped_overload_keeps_known_stations_flags_clipping_and_the_comb() {
    let Some(r) = run(
        "urban_98M_20M_l32g30a1_t1p0_0p6s",
        ChainConfig::new(4096, 10),
        config(),
    ) else {
        return;
    };
    known_stations_detected(&r);
    let all = r.got.detections.len();
    let clipped = r
        .got
        .detections
        .iter()
        .filter(|d| d.detection.flags.clipped)
        .count();
    eprintln!("clipped on {clipped} of {all} detections");
    assert!(all > 0);
    assert_eq!(clipped, all, "{AWARE_042:?} clipped must be set on 100 %");
    assert!(r.got.detections.iter().all(|d| d.detection.clip_count > 0));

    let eval = r.got.evaluations.iter().rev().find(|e| e.comb.flagged);
    let comb_truth = r.fx.of_kind("comb");
    let comb_flagged = comb_truth
        .iter()
        .filter(|t| {
            r.got.detections.iter().any(|d| {
                (d.detection.f_center_hz - t.center_hz()).abs() <= 7e3
                    && d.detection.flags.spur_reason == Some(SpurReason::Comb)
            })
        })
        .count();
    eprintln!(
        "comb: evaluation {:?}; {comb_flagged} of {} comb lines have a comb-flagged detection",
        eval.map(|e| (e.comb.members, e.comb.spacing_hz, e.comb.chance)),
        comb_truth.len()
    );
    let eval = eval.expect("[AWARE-042] comb not found in the integrated spectrum");
    let m = (209_478.6 / eval.comb.spacing_hz).round();
    assert!((1.0..=2.0).contains(&m) && (eval.comb.spacing_hz * m - 209_478.6).abs() < 1e3);
    assert!(
        comb_flagged * 4 >= comb_truth.len() * 3,
        "{AWARE_042:?} only {comb_flagged} comb lines flagged"
    );
    // The 100 MHz spur is detected and flagged. DC is compressed at this gain (S4: −30 dB relative
    // to linear); whatever is detected at DC must be flagged, but it need not be detected.
    for (kind, reason, required) in [
        ("spur", SpurReason::RefHarmonic, true),
        ("dc-offset", SpurReason::Dc, false),
    ] {
        let t = r.fx.of_kind(kind)[0];
        let d: Vec<_> = r
            .got
            .detections
            .iter()
            .filter(|d| {
                (d.detection.f_center_hz - t.center_hz()).abs() <= 10e3
                    && d.detection.xdb_bandwidth_hz.unwrap() <= 40e3
            })
            .collect();
        let text: Vec<_> = d.iter().map(|d| describe(d, r.fs)).collect();
        eprintln!("{kind} at high gain: {text:#?}");
        assert!(!required || !d.is_empty(), "{kind} not detected");
        assert!(
            d.iter()
                .all(|d| d.detection.flags.spur_reason == Some(reason)),
            "{kind}: {text:#?}"
        );
    }

    // The repository accepts every clipped / overloaded detection.
    let mut repo = Repository::open_in_memory().unwrap();
    let survey = r.got.detections[0].detection.survey_id;
    insert_survey(&mut repo, survey);
    let mut writer = DetectionWriter::new(256);
    for d in &r.got.detections {
        writer.push(&mut repo, d).unwrap();
    }
    writer.flush(&mut repo).unwrap();
    assert_eq!(repo.detection_count().unwrap(), all as u64);

    // Mid-gain pair: same stations, nothing clipped; the gain step refuses clipped blocks.
    let Some(mid) = run(
        "urban_98M_20M_l24g20a0_t1p0_0p6s",
        ChainConfig::new(4096, 10),
        config(),
    ) else {
        return;
    };
    known_stations_detected(&mid);
    assert!(
        mid.got
            .detections
            .iter()
            .all(|d| !d.detection.flags.clipped && d.detection.clip_count == 0)
    );
    let (lo, hi) = (
        mid.det.last_capture().unwrap(),
        r.det.last_capture().unwrap(),
    );
    let step = gain_step(lo, hi, &GainStepConfig::default());
    eprintln!("gain step mid → high: {:?}", step.skipped);
    assert_eq!(step.skipped, Some(GainStepSkip::Clipped));
}

fn insert_survey(repo: &mut Repository, id: SurveyId) {
    use hk_model::{ScanPlan, ScanPlanId, ScanPolicy, Schedule, Survey, SurveyState, Timestamp};
    let plan = ScanPlan {
        id: ScanPlanId::new(),
        version: 1,
        name: "fixtures".into(),
        created_at: Timestamp::UNIX_EPOCH,
        regions: Vec::new(),
        policy: ScanPolicy::SweepThenDwell,
        gain_table: Vec::new(),
        schedule: Schedule::Cron {
            expr: "0 * * * *".into(),
        },
        extra: serde_json::json!({}),
    };
    repo.insert_scan_plan(&plan).unwrap();
    repo.insert_survey(&Survey {
        id,
        plan_id: plan.id,
        plan_version: 1,
        device_id: "hackrf".into(),
        state: SurveyState::Open,
        t_start: Timestamp::UNIX_EPOCH,
        t_end: None,
        summary: None,
    })
    .unwrap();
}

#[test]
fn negative_control_433_has_no_confirmed_non_artefact_emitter_candidates() {
    let Some(r) = run(
        "ism_433p62M_2M_l24g30a1_t162p0_6s",
        ChainConfig::new(4096, 10),
        config(),
    ) else {
        return;
    };
    let df = r.fs / 4096.0;
    let artefacts = r.fx.artefacts();
    // The annotations (floor and narrowband lines) cover the floor span only (|f − fc| ≤ 0.7 MHz);
    // lines beyond it are unannotated, so they are reported, not judged.
    let span = r.fx.with_role(hk_e2e::Role::Floor)[0];
    let confirmed = r.got.confirmed();
    let mut offenders = Vec::new();
    let mut outside = Vec::new();
    for d in r
        .got
        .detections
        .iter()
        .filter(|d| confirmed.contains(&d.detection.id))
    {
        if d.f_lo_hz < span.f_lo_hz || d.f_hi_hz > span.f_hi_hz {
            outside.push(d.detection.f_center_hz);
            continue;
        }
        let on_artefact = artefacts
            .iter()
            .any(|t| d.f_lo_hz <= t.f_hi_hz + 2.0 * df && t.f_lo_hz - 2.0 * df <= d.f_hi_hz);
        if !on_artefact && !d.detection.flags.spur_candidate {
            offenders.push(describe(d, r.fs));
        }
    }
    let mut lines: Vec<i64> = outside.iter().map(|f| (f / 1e3).round() as i64).collect();
    lines.sort_unstable();
    lines.dedup_by(|a, b| (*a - *b).abs() <= 2);
    eprintln!(
        "outside the annotated span [{:.3}, {:.3}] MHz: {} confirmed detections on {} persistent lines (kHz): {lines:?}",
        span.f_lo_hz / 1e6,
        span.f_hi_hz / 1e6,
        outside.len(),
        lines.len()
    );
    let line: Vec<_> = r
        .got
        .detections
        .iter()
        .filter(|d| (d.detection.f_center_hz - 434.0e6).abs() <= 5e3)
        .collect();
    eprintln!(
        "434.000 MHz line: {} detections, {} confirmed",
        line.len(),
        line.iter()
            .filter(|d| confirmed.contains(&d.detection.id))
            .count()
    );
    for d in &line {
        eprintln!("  {}", describe(d, r.fs));
    }
    if let Some(e) = r.got.evaluations.iter().rev().find(|e| e.confirming) {
        for em in e
            .emitters
            .iter()
            .filter(|em| (em.f_center_hz - 434.0e6).abs() <= 5e3)
        {
            eprintln!(
                "  integrated: {:.6} MHz bw {:.0} Hz peak {:.1} dB level {:.1} dBFS flags {:?}",
                em.f_center_hz / 1e6,
                em.bandwidth_hz,
                em.peak_snr_db,
                em.level_dbfs,
                em.flags
            );
        }
    }
    eprintln!(
        "negative control: {} detections, {} confirmed, {} confirmed off the annotated artefacts",
        r.got.detections.len(),
        confirmed.len(),
        offenders.len()
    );
    assert!(
        offenders.is_empty(),
        "{AWARE_036:?} confirmed non-artefact candidates: {offenders:#?}"
    );
}
