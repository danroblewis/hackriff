//! SPACE-050 (T4 for the pipeline; the science claim is field-confirmed): natural noise-floor
//! survey. The `injected_floor` synth (six 1 s band segments from 10 MHz to 5.8 GHz at 1 Msps,
//! each a retune with a known floor and a known calibration constant) replayed once through the
//! composed pipeline. The history reader folds every frame into the SpectrumTile pyramid and the
//! `FloorProduct`, which answers floor-vs-time per segment.
//!
//! **Calibrated in the pipeline (T-037a).** The scenario's calibration (`calibration_k_db` per
//! segment and gain, as the T-021 tests build it with `hk_dsp::radiometry::
//! synthetic_calibration_state`) is written as T-021 `CalibrationState` JSON, loaded with
//! `hk_pipeline::load_calibrations` into `PipelineConfig::calibrations`, and pinned by the
//! pipeline on the capture provenance of its device. The product's floor-vs-time steps are then
//! dBm/Hz straight from the run: no post-run lookup. Calibrated floors must land within ±1 dB
//! of truth, and so must the dBFS/Hz floor they imply.

use hk_dsp::radiometry::{SyntheticCalSegment, gain_setting_of, synthetic_calibration_state};
use hk_e2e::{Role, SynthRequest, synth_or_skip};
use hk_model::{FreqRange, PowerUnit, TimeRange};
use hk_store::{RegionQuery, Resolution};
use serde_json::json;

use crate::blind::{assert_truth_found, replay_config, start};
use crate::common::*;

const SPACE_050: &str = "SPACE-050";
/// hk-core normalises ci8 by 1/128, the generator's dBFS by 1/127 (see
/// `synthetic_calibration_state`): the pipeline reads 20·log10(127/128) dB low, so the pipeline's
/// calibration constant is the scenario's plus this.
fn ci8_scale_db() -> f64 {
    20.0 * (128.0f64 / 127.0).log10()
}

#[test]
fn space_050_injected_floor_calibrated_floor_vs_time_from_pipeline_tiles() {
    let out = synth_or_skip!(
        SynthRequest::new("injected_floor")
            .seed(4)
            .param("segment_duration_s", 1.0)
    );
    let fx = out.fixture(0).unwrap();
    let fs = fx.sample_rate;
    let dir = TempDir::new("s050");

    let floors = fx.with_role(Role::Floor);
    assert_eq!(floors.len(), 6);
    let t_start = hk_core::source::sigmf_replay::parse_sigmf_datetime(
        fx.meta.captures[0].datetime.as_deref().unwrap(),
    )
    .unwrap();
    // The six segments are a survey through the mock SDR (the device retuned to each recorded
    // centre in turn, `blind::SurveyDevice`); the calibration is the device-under-test's.
    let (mut cfg, replay) = replay_config(
        &dir.0.join("run"),
        &fx.meta_path,
        json!({}),
        hk_core::Pacing::Unpaced,
    );
    let device_id = replay.device.device_id.clone();
    let recorded_device = fx.meta.captures[0]
        .provenance
        .as_ref()
        .expect("capture provenance")
        .device_id
        .clone();
    let segments: Vec<SyntheticCalSegment> = floors
        .iter()
        .map(|seg| {
            let cap = fx.capture_at(seg.sample_start).unwrap();
            let prov = cap.provenance.as_ref().expect("capture provenance");
            assert_eq!(prov.device_id, recorded_device, "one device");
            SyntheticCalSegment {
                band: FreqRange::centered(cap.frequency.unwrap(), fs),
                gain: gain_setting_of(prov),
                k_db: seg.expect_f64("calibration_k_db") + ci8_scale_db(),
            }
        })
        .collect();
    let state = synthetic_calibration_state(&device_id, &segments, 0.1, t_start);
    let cal_path = dir.0.join("calibration.json");
    std::fs::write(&cal_path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
    cfg.calibrations = hk_pipeline::load_calibrations(&cal_path).unwrap();
    let handle = start(cfg, replay);
    let product = handle.floor_product();
    let s = finish(handle);
    assert_eq!(s.always_on_lost_samples, 0);
    assert!(
        s.counter("/history/tiles_written") > 0,
        "[{SPACE_050}] no SpectrumTiles written"
    );
    // Each segment's reference carrier, matched blind across the survey (T-047) against the
    // private truth: detected, and stored as an inventory emitter. Before T-072 the six carriers
    // were detected but nothing was stored: the pipeline pinned the loaded calibration on the
    // provenance without storing the calibration version the Provenance row references, so every
    // detection write failed (`/detect/db_errors`) and no track reached the inventory.
    assert_eq!(
        s.counter("/detect/db_errors"),
        0,
        "[{SPACE_050}] store errors"
    );
    for t in assert_truth_found(SPACE_050, &dir.0.join("run"), &fx, 0.0, false) {
        assert!(
            !t.emitters.is_empty(),
            "[{SPACE_050}] carrier detected but not stored as an emitter: {t:?}"
        );
    }

    let p = product.lock().unwrap();
    assert!(
        p.stats().calibrated_frames > 0 && p.stats().uncalibrated_frames == 0,
        "[{SPACE_050}] every frame calibrated in the pipeline: {:?}",
        p.stats()
    );
    let (mut worst_fs, mut worst_m) = (0.0f64, 0.0f64);
    for (i, seg) in floors.iter().enumerate() {
        let cap = fx.capture_at(seg.sample_start).unwrap();
        let fc = cap.frequency.unwrap();
        let t0 =
            hk_core::source::sigmf_replay::parse_sigmf_datetime(cap.datetime.as_deref().unwrap())
                .unwrap();
        let t1 = t0.saturating_add_nanos((seg.sample_count as f64 * 1e9 / fs) as i64);
        let region = FreqRange::centered(fc, 0.8 * fs);
        let k = seg.expect_f64("calibration_k_db");
        let want_fs = seg.expect_f64("expected_floor_dbfs") - 10.0 * seg.bandwidth_hz().log10();
        let want_dbm = want_fs + k;

        // Folded into calibrated SpectrumTiles: the dBm pyramid has observed cells.
        let tiles = p
            .calibrated_pyramid()
            .query(&RegionQuery {
                freq: region,
                time: TimeRange::new(t0, t1),
                resolution: Resolution::Level(0),
            })
            .unwrap();
        let observed_cells = tiles.cells.iter().filter(|c| c.observed()).count();
        assert!(
            observed_cells > 0,
            "[{SPACE_050}] segment {i}: no observed calibrated tile cells"
        );

        // Floor vs time, calibrated by the pipeline.
        let fvt = p
            .floor_vs_time(region, t0, t1, Resolution::Level(0))
            .unwrap();
        let steps: Vec<_> = fvt.steps.iter().filter(|st| !st.is_gap()).collect();
        assert!(
            !steps.is_empty(),
            "[{SPACE_050}] segment {i}: no floor step"
        );
        for st in steps {
            assert_eq!(
                st.unit,
                Some(PowerUnit::Dbm),
                "[{SPACE_050}] segment {i}: the pipeline calibrates the floor"
            );
            let got_dbm = st.dbm_per_hz().expect("calibrated floor value");
            let got_fs = got_dbm - k - ci8_scale_db();
            worst_fs = worst_fs.max((got_fs - want_fs).abs());
            worst_m = worst_m.max((got_dbm - want_dbm).abs());
            assert!(
                (got_dbm - want_dbm).abs() <= 1.0,
                "[{SPACE_050}] segment {i}: calibrated {got_dbm:.2} vs {want_dbm:.2} dBm/Hz"
            );
            assert!(
                (got_fs - want_fs).abs() <= 1.0,
                "[{SPACE_050}] segment {i}: {got_fs:.2} vs {want_fs:.2} dBFS/Hz"
            );
        }
        eprintln!(
            "[{SPACE_050}] segment {i} ({:.2} MHz): {observed_cells} tile cells, {} floor steps, \
             want {want_dbm:.2} dBm/Hz",
            fc / 1e6,
            fvt.steps.len()
        );
    }
    eprintln!(
        "[{SPACE_050}] worst floor error: {worst_fs:.2} dB (dBFS/Hz), {worst_m:.2} dB (dBm/Hz)"
    );
}
