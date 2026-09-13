//! Acceptance on the T-023 synthetic scenarios, replayed as ci8 through STFT → floor tracker →
//! detector:
//! - AWARE-036 (`fsk_burst_train`): every burst detected, boxes within tolerance, no false alarms
//!   outside truth; repeats confirm the emitter candidate.
//! - AWARE-042 (`occupancy_multi_hour` render windows): per-channel detection counts match the
//!   schedule.
//!
//! Tests skip when `uv` is missing (`HK_E2E_REQUIRE_SYNTH=1` makes that a failure).

mod common;

use common::*;
use hk_detect::{Candidate, Detector, DetectorConfig};
use hk_e2e::{BoxTolerance, DetectionBox, SynthRequest, match_detections, synth_or_skip};
use hk_model::SurveyId;

const AWARE_036: &[&str] = &["AWARE-036"];
const AWARE_042: &[&str] = &["AWARE-042"];

#[test]
fn aware_036_fsk_burst_train_every_burst_detected_without_false_alarms() {
    for (seed, snr_db) in [(1u64, 20.0), (2, 12.0)] {
        let out = synth_or_skip!(
            SynthRequest::new("fsk_burst_train")
                .seed(seed)
                .param("snr_db", snr_db)
        );
        let fx = out.fixture(0).unwrap();
        let fs = fx.sample_rate;
        let iq = to_ci8(&fx.samples().unwrap());
        let chain = ChainConfig::new(512, 5);
        let mut det = Detector::new(DetectorConfig::new(SurveyId::new())).unwrap();
        let (got, frames) = replay_ci8(&iq, fs, &[(0, fixture_provenance(&fx))], &chain, &mut det);
        let boxes: Vec<DetectionBox> = got.detections.iter().map(|d| to_box(d, fs)).collect();
        let bursts = fx.of_kind("fsk-burst");
        let tol = BoxTolerance {
            time_s: 2.0 * chain.frame_period_s(fs),
            freq_hz: 5e3,
        };
        let report = match_detections(&boxes, &bursts, &fx.artefacts(), tol);
        eprintln!(
            "AWARE-036 seed {seed} snr {snr_db} dB: {} bursts, {} detections over {frames} frames; matched {}, explained {}, false {}",
            bursts.len(),
            boxes.len(),
            report.matches.len(),
            report.explained.len(),
            report.false_alarms.len()
        );
        for d in got.sorted() {
            eprintln!("  {}", describe(d, fs));
        }
        report.assert_all_found(AWARE_036, &bursts);
        report.assert_false_alarms_at_most(AWARE_036, &boxes, 0);
        // A wide-deviation (h = 4) rectangular 2-FSK burst is two tones plus spectral sidelobes:
        // S4 does no frequency merge at box level, so a burst may be several boxes (T-007
        // aggregates). The union of its non-marginal boxes must match the truth (Carson) box within
        // tolerance; weaker sidelobe boxes (< 10 dB, `marginal`) may extend beyond it and are
        // counted as explained, not false. None may carry an artefact flag.
        for t in &bursts {
            let near: Vec<_> = got
                .detections
                .iter()
                .filter(|d| {
                    d.t_start_s(fs) <= t.t_end_s + tol.time_s
                        && t.t_start_s - tol.time_s <= d.t_end_s(fs)
                        && d.f_lo_hz <= t.f_hi_hz + 2.0 * tol.freq_hz
                        && t.f_lo_hz - 2.0 * tol.freq_hz <= d.f_hi_hz
                })
                .collect();
            assert!(
                near.iter()
                    .all(|d| !d.detection.flags.spur_candidate && !d.detection.flags.impulsive)
            );
            let parts: Vec<_> = near
                .iter()
                .filter(|d| {
                    !d.detection.flags.marginal
                        && d.t_start_s(fs) <= t.t_end_s + tol.time_s
                        && t.t_start_s - tol.time_s <= d.t_end_s(fs)
                        && d.f_lo_hz <= t.f_hi_hz + tol.freq_hz
                        && t.f_lo_hz - tol.freq_hz <= d.f_hi_hz
                })
                .collect();
            // Occupied extent (docs/07: f_center ± OBW/2), not the region extent at T_off, which
            // also takes in sidelobe cells beyond the nominal Carson box.
            let f_lo = parts
                .iter()
                .map(|d| d.detection.freq().lo_hz)
                .fold(f64::INFINITY, f64::min);
            let f_hi = parts
                .iter()
                .map(|d| d.detection.freq().hi_hz)
                .fold(f64::NEG_INFINITY, f64::max);
            let t_lo = parts
                .iter()
                .map(|d| d.t_start_s(fs))
                .fold(f64::INFINITY, f64::min);
            let t_hi = parts
                .iter()
                .map(|d| d.t_end_s(fs))
                .fold(f64::NEG_INFINITY, f64::max);
            assert!(
                (f_lo - t.f_lo_hz).abs() <= tol.freq_hz && (f_hi - t.f_hi_hz).abs() <= tol.freq_hz,
                "[AWARE-036] burst {:?}: detected [{f_lo:.0}, {f_hi:.0}] Hz vs truth [{:.0}, {:.0}]",
                t.f64("burst_index"),
                t.f_lo_hz,
                t.f_hi_hz
            );
            assert!(
                (t_lo - t.t_start_s).abs() <= tol.time_s && (t_hi - t.t_end_s).abs() <= tol.time_s,
                "[AWARE-036] burst {:?}: detected [{t_lo:.4}, {t_hi:.4}] s vs truth [{:.4}, {:.4}]",
                t.f64("burst_index"),
                t.t_start_s,
                t.t_end_s
            );
            assert!(
                parts
                    .iter()
                    .all(|d| !d.detection.flags.spur_candidate && !d.detection.flags.impulsive)
            );
        }
        // The emitter repeats: every burst is an emitter candidate, i.e. at least one of its boxes
        // (tone lobes are separate boxes) is confirmed at emission or later.
        let confirmed = got.confirmed();
        let matched: Vec<_> = report
            .matches
            .iter()
            .map(|&(_, di)| &got.detections[di])
            .collect();
        let unconfirmed = bursts
            .iter()
            .filter(|t| {
                !got.detections.iter().any(|d| {
                    confirmed.contains(&d.detection.id)
                        && d.t_start_s(fs) <= t.t_end_s + tol.time_s
                        && t.t_start_s - tol.time_s <= d.t_end_s(fs)
                        && d.f_lo_hz <= t.f_hi_hz + tol.freq_hz
                        && t.f_lo_hz - tol.freq_hz <= d.f_hi_hz
                })
            })
            .count();
        assert_eq!(
            unconfirmed, 0,
            "[AWARE-036] {unconfirmed} bursts never confirmed"
        );
        // A burst split into tone lobes may only be confirmed by the next burst's lobes (a
        // `Confirmed` event), so the repeat-at-emission count is reported, not asserted.
        eprintln!(
            "AWARE-036 seed {seed}: {} of {} matched bursts were repeats at emission; all confirmed",
            matched
                .iter()
                .filter(|d| matches!(d.candidate, Candidate::Repeat { .. }))
                .count(),
            matched.len()
        );
    }
}

#[test]
fn aware_042_occupancy_windows_per_channel_counts_match_the_schedule() {
    let out = synth_or_skip!(
        SynthRequest::new("occupancy_multi_hour")
            .seed(5)
            .param("windows", 6)
    );
    let schedule = out.file_json("schedule.json").unwrap();
    let channels: Vec<f64> = schedule["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["center_hz"].as_f64().unwrap())
        .collect();
    let spacing = channels[1] - channels[0];
    let chain = ChainConfig::new(128, 10);
    let mut checked = 0;
    for fx in out.fixtures().unwrap() {
        let fs = fx.sample_rate;
        let frame_s = chain.frame_period_s(fs);
        let iq = to_ci8(&fx.samples().unwrap());
        let mut det = Detector::new(DetectorConfig::new(SurveyId::new())).unwrap();
        let (got, _) = replay_ci8(&iq, fs, &[(0, fixture_provenance(&fx))], &chain, &mut det);
        let truth = fx.of_kind("nbfm-burst");
        for (c, &ch) in channels.iter().enumerate() {
            let scheduled: Vec<_> = truth
                .iter()
                .filter(|t| t.f64("channel").map(|x| x as usize) == Some(c))
                .collect();
            // Bursts overlapping the window by fewer than min-duration + 1 frames need not show.
            let visible = scheduled
                .iter()
                .filter(|t| t.t_end_s - t.t_start_s >= 4.0 * frame_s)
                .count();
            let dets: Vec<_> = got
                .detections
                .iter()
                .filter(|d| (d.detection.f_center_hz - ch).abs() <= spacing / 2.0)
                .collect();
            eprintln!(
                "AWARE-042 {} ch{c}: scheduled {} (visible {visible}), detected {}",
                fx.meta_path.file_name().unwrap().to_string_lossy(),
                scheduled.len(),
                dets.len()
            );
            for d in &dets {
                eprintln!("    {}", describe(d, fs));
            }
            assert!(
                dets.len() >= visible && dets.len() <= scheduled.len(),
                "{AWARE_042:?} channel {c}: {} detections for {} scheduled ({visible} visible) bursts",
                dets.len(),
                scheduled.len()
            );
            checked += scheduled.len();
        }
        let stray: Vec<_> = got
            .detections
            .iter()
            .filter(|d| {
                channels
                    .iter()
                    .all(|&ch| (d.detection.f_center_hz - ch).abs() > spacing / 2.0)
            })
            .collect();
        assert!(
            stray.is_empty(),
            "{AWARE_042:?} detections off every channel: {:#?}",
            stray.iter().map(|d| describe(d, fs)).collect::<Vec<_>>()
        );
    }
    assert!(checked > 0, "no scheduled bursts in the rendered windows");
}
