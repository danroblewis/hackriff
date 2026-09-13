//! T-047 perturbed variants driven **through the mock SDR's control interface** on the real FM
//! fixture, blind (truth stripped and sealed; everything produced is matched against the private
//! truth after the run). The +150 kHz off-raster variant lives in AWARE-053 (an IQ shift: a device
//! retune preserves absolute frequencies, so it cannot move a station off its raster).
//!
//! - **Gain step:** VGA raised in two steps (`set_gain`, +6 dB then +26 dB over the recorded
//!   30 dB). The strong station already drives the 8-bit ADC model into clipping at +6 dB. The
//!   station stays detected at every gain; detections taken while the mock clips carry an
//!   overloaded Provenance and the `clipped` flag, and none at the recorded gain do.
//! - **Retune outside coverage:** `tune()` to a window half outside the recording, then to one
//!   entirely outside it. The station stays detected with the partial-coverage flag in its stored
//!   Provenance; the noise-filled spectrum yields no FM-wide detection or emitter (no phantoms).
//!   Detections in the recorded window before the retunes (including real stations between the
//!   baseband filter edge and the recorded rate) are not phantoms.
//! - **HIL switch:** `HK_DEVICE=hackrf` would run these against the real radio (T-053); ignored by
//!   default and never opens the device.

use hk_core::Coverage;
use hk_e2e::TruthItem;
use hk_e2e::blind::matches_truth;
use hk_model::sigmf::SigmfMeta;
use hk_model::{Detection, FreqRange, InventoryQuery, Provenance, Region};

use crate::blind::{
    BlindSource, DeviceAction, DeviceStep, assert_truth_found, blind_replay, center_tol_hz,
    private_truth,
};
use crate::common::*;
use crate::signal_062::FM_FIXTURE;

const TAG: &str = "T-047/device";
/// Detections this wide in noise would be phantom FM-like emissions.
const PHANTOM_OBW_HZ: f64 = 20e3;

fn station(fx: &hk_e2e::Fixture) -> TruthItem {
    fx.of_kind("wfm-broadcast")
        .first()
        .map(|t| (*t).clone())
        .expect("fixture truth has a wfm-broadcast station")
}

/// Every detection of a finished run with its stored Provenance.
fn detections(dir: &TempDir) -> Vec<(Detection, Provenance)> {
    let repo = repo(&dir.0);
    repo.detections_in_region(&Region::new(FreqRange::new(0.0, 7.0e9), ever()))
        .unwrap()
        .into_iter()
        .map(|d| {
            let p = repo.provenance(d.provenance_ref).unwrap();
            (d, p)
        })
        .collect()
}

#[test]
fn device_gain_step_keeps_the_station_detected_and_flags_overload_when_clipping() {
    const RECORDED_VGA_DB: f64 = 30.0;
    static STEPS: [DeviceStep; 2] = [
        DeviceStep {
            at_block: 300,
            action: DeviceAction::Gain("vga", 36.0),
        },
        DeviceStep {
            at_block: 650,
            action: DeviceAction::Gain("vga", 56.0),
        },
    ];
    let Some((meta, fx)) = private_truth(FM_FIXTURE) else {
        return;
    };
    let truth = station(&fx);
    let run = blind_replay(
        &meta,
        "gain",
        BlindSource {
            steps: &STEPS,
            ..BlindSource::default()
        },
    );
    let stats = run.mock[0];
    eprintln!("[{TAG}] gain step: mock {stats:?}");
    assert!(
        stats.control_changes >= 2,
        "[{TAG}] both gain steps applied"
    );
    assert!(
        stats.clipped_components > 0 && stats.overload_blocks > 0,
        "[{TAG}] the +26 dB step drives the 8-bit ADC model into clipping"
    );
    assert_truth_found(TAG, &run.dir.0, &fx, 0.0, true);

    let tol = center_tol_hz(&truth);
    let at_station: Vec<_> = detections(&run.dir)
        .into_iter()
        .filter(|(d, _)| matches_truth(&truth, 0.0, d.f_center_hz, d.obw_hz, tol))
        .collect();
    for vga in [RECORDED_VGA_DB, 36.0, 56.0] {
        let at: Vec<_> = at_station
            .iter()
            .filter(|(_, p)| p.tune.vga_db == vga)
            .collect();
        let overloaded = at.iter().filter(|(_, p)| p.overload).count();
        eprintln!(
            "[{TAG}] VGA {vga} dB: {} station detections, {overloaded} with an overloaded \
             provenance, {} flagged clipped",
            at.len(),
            at.iter().filter(|(d, _)| d.flags.clipped).count()
        );
        assert!(
            !at.is_empty(),
            "[{TAG}] the station is not detected at VGA {vga} dB"
        );
        for (d, p) in &at {
            assert!(
                !p.overload || d.flags.clipped,
                "[{TAG}] an overloaded detection lacks the clipped flag"
            );
        }
        if vga == RECORDED_VGA_DB {
            assert_eq!(overloaded, 0, "[{TAG}] overload before any clipping");
        }
    }
    assert!(
        at_station
            .iter()
            .any(|(d, p)| p.tune.vga_db == 56.0 && p.overload && d.flags.clipped),
        "[{TAG}] no station detection flagged overloaded/clipped while the device clipped"
    );
}

#[test]
fn device_retune_outside_coverage_serves_noise_flagged_and_yields_no_phantoms() {
    static STEPS: [DeviceStep; 2] = [
        // Half the window outside the recording (the station stays inside).
        DeviceStep {
            at_block: 300,
            action: DeviceAction::Tune(101.8e6),
        },
        // Entirely outside it.
        DeviceStep {
            at_block: 650,
            action: DeviceAction::Tune(400.0e6),
        },
    ];
    let Some((meta, fx)) = private_truth(FM_FIXTURE) else {
        return;
    };
    let truth = station(&fx);
    // The recorded band (capture metadata, not truth): the device's coverage.
    let tune = SigmfMeta::read(&meta)
        .unwrap()
        .global
        .provenance
        .expect("recorded provenance")
        .tune;
    let (lo, hi) = (
        tune.center_hz - tune.bandwidth_hz / 2.0,
        tune.center_hz + tune.bandwidth_hz / 2.0,
    );
    let run = blind_replay(
        &meta,
        "retune",
        BlindSource {
            steps: &STEPS,
            ..BlindSource::default()
        },
    );
    let stats = run.mock[0];
    eprintln!("[{TAG}] retune: mock {stats:?}");
    assert!(
        stats.control_changes >= 2 && stats.uncovered_samples > 0,
        "[{TAG}] the device served noise outside its recording"
    );
    assert_truth_found(TAG, &run.dir.0, &fx, 0.0, true);

    let all = detections(&run.dir);
    let tol = center_tol_hz(&truth);
    let mut by_coverage = [0usize; 3];
    for (_, p) in &all {
        match Coverage::from_provenance(p) {
            Some(Coverage::Recorded) => by_coverage[0] += 1,
            Some(Coverage::Partial) => by_coverage[1] += 1,
            Some(Coverage::Noise) => by_coverage[2] += 1,
            None => panic!("[{TAG}] a mock detection without a coverage flag: {p:?}"),
        }
    }
    eprintln!(
        "[{TAG}] detections by coverage (recorded, partial, noise): {by_coverage:?} of {}",
        all.len()
    );
    assert!(
        all.iter().any(|(d, p)| {
            Coverage::from_provenance(p) == Some(Coverage::Partial)
                && p.tune.center_hz == 101.8e6
                && matches_truth(&truth, 0.0, d.f_center_hz, d.obw_hz, tol)
        }),
        "[{TAG}] the station was not detected with the partial-coverage flag after the retune"
    );
    // Phantoms: FM-wide detections taken while the device served noise, outside the recording.
    let phantoms: Vec<(f64, f64, Option<Coverage>)> = all
        .iter()
        .filter(|(d, p)| {
            Coverage::from_provenance(p) != Some(Coverage::Recorded)
                && (d.f_center_hz < lo || d.f_center_hz > hi)
                && d.obw_hz > PHANTOM_OBW_HZ
        })
        .map(|(d, p)| (d.f_center_hz, d.obw_hz, Coverage::from_provenance(p)))
        .collect();
    assert!(
        phantoms.is_empty(),
        "[{TAG}] FM-wide detections in the noise outside {lo}..{hi} Hz: {phantoms:?}"
    );
    // No emitter anywhere the recording never held samples (beyond its recorded rate).
    let (rec_lo, rec_hi) = (
        tune.center_hz - tune.sample_rate_hz / 2.0,
        tune.center_hz + tune.sample_rate_hz / 2.0,
    );
    let phantom_emitters: Vec<(f64, f64)> = inventory(&repo(&run.dir.0), InventoryQuery::default())
        .into_iter()
        .filter(|e| {
            (e.emitter.f_center_hz < rec_lo || e.emitter.f_center_hz > rec_hi)
                && e.emitter.bandwidth_hz > PHANTOM_OBW_HZ
        })
        .map(|e| (e.emitter.f_center_hz, e.emitter.bandwidth_hz))
        .collect();
    assert!(
        phantom_emitters.is_empty(),
        "[{TAG}] emitters in the noise outside the recording: {phantom_emitters:?}"
    );
}

/// The HIL switch (T-053): the same blind device tests against the real HackRF, with truth from a
/// live survey of an always-occupied band. Only the switch exists; the device is never opened.
#[test]
#[ignore = "HIL (T-053): run with HK_DEVICE=hackrf on the bench rig, receive-only"]
fn hil_blind_fm_survey_on_the_hackrf() {
    if !hardware_skip("hil_blind_fm_survey_on_the_hackrf") {
        eprintln!(
            "SKIP hil_blind_fm_survey_on_the_hackrf: set HK_DEVICE=hackrf to select the real \
             HackRF (T-053)"
        );
    }
}
