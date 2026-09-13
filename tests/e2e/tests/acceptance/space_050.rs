//! SPACE-050 (T4 for the pipeline; the science claim is field-confirmed): natural noise-floor
//! survey. The `injected_floor` synth (six 1 s band segments from 10 MHz to 5.8 GHz at 1 Msps,
//! each a retune with a known floor and a known calibration constant) replayed once through the
//! composed pipeline. The history reader folds every frame into the SpectrumTile pyramid and the
//! `FloorProduct`, which answers floor-vs-time per segment.
//!
//! **Calibration path (T-037 not merged).** The pipeline opens its `FloorProduct` with no
//! calibration table, so its floor is uncalibrated dBFS/Hz. The calibrated floor is therefore
//! derived the way the T-021 tests build calibration (`hk_dsp::radiometry::
//! synthetic_calibration_state` from the scenario's `calibration_k_db`, looked up through
//! `PowerCalibrations::band` for the capture's gain) and applied to the pipeline's floor-vs-time
//! steps. Both the uncalibrated (dBFS/Hz) and calibrated (dBm/Hz) floors must land within ±1 dB.
//! When T-037 loads calibration into the pipeline, assert on the product's calibrated steps
//! directly and drop the post-hoc lookup.

use hk_dsp::radiometry::{
    PowerCalibrations, SyntheticCalSegment, gain_setting_of, synthetic_calibration_state,
};
use hk_e2e::{Role, SynthRequest, synth_or_skip};
use hk_model::{FreqRange, PowerUnit, TimeRange};
use hk_store::{RegionQuery, Resolution};
use serde_json::json;

use crate::common::*;

const SPACE_050: &str = "SPACE-050";
/// hk-core normalises ci8 by 1/128, the generator's dBFS by 1/127 (see
/// `synthetic_calibration_state`): the pipeline reads 20·log10(127/128) dB low.
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
    let (cfg, replay) = replay_config(&dir.0, &fx.meta_path, json!({}), hk_core::Pacing::Unpaced);
    let handle = start(cfg, replay);
    let product = handle.floor_product();
    let s = finish(handle);
    assert_eq!(s.always_on_lost_samples, 0);
    assert!(
        s.counter("/history/tiles_written") > 0,
        "[{SPACE_050}] no SpectrumTiles written"
    );

    let floors = fx.with_role(Role::Floor);
    assert_eq!(floors.len(), 6);
    let t_start = hk_core::source::sigmf_replay::parse_sigmf_datetime(
        fx.meta.captures[0].datetime.as_deref().unwrap(),
    )
    .unwrap();
    let segments: Vec<SyntheticCalSegment> = floors
        .iter()
        .map(|seg| {
            let cap = fx.capture_at(seg.sample_start).unwrap();
            SyntheticCalSegment {
                band: FreqRange::centered(cap.frequency.unwrap(), fs),
                gain: gain_setting_of(cap.provenance.as_ref().expect("capture provenance")),
                k_db: seg.expect_f64("calibration_k_db"),
            }
        })
        .collect();
    let state = synthetic_calibration_state("synthetic:t-024", &segments, 0.1, t_start);
    let cals = PowerCalibrations::from_states([&state], None);

    let p = product.lock().unwrap();
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

        // Folded into SpectrumTiles: the pyramid has observed cells over the segment.
        let tiles = p
            .uncalibrated_pyramid()
            .query(&RegionQuery {
                freq: region,
                time: TimeRange::new(t0, t1),
                resolution: Resolution::Level(0),
            })
            .unwrap();
        let observed_cells = tiles.cells.iter().filter(|c| c.observed()).count();
        assert!(
            observed_cells > 0,
            "[{SPACE_050}] segment {i}: no observed tile cells"
        );

        // Floor vs time.
        let fvt = p
            .floor_vs_time(region, t0, t1, Resolution::Level(0))
            .unwrap();
        let steps: Vec<_> = fvt.steps.iter().filter(|st| !st.is_gap()).collect();
        assert!(
            !steps.is_empty(),
            "[{SPACE_050}] segment {i}: no floor step"
        );
        let gain = gain_setting_of(cap.provenance.as_ref().unwrap());
        for st in steps {
            assert_eq!(
                st.unit,
                Some(PowerUnit::Dbfs),
                "[{SPACE_050}] the pipeline product is uncalibrated until T-037"
            );
            let got_fs = st.value_db_per_hz.expect("floor value");
            let bc = cals
                .band(Some(state.id), &gain, region, st.t)
                .unwrap_or_else(|e| panic!("[{SPACE_050}] segment {i}: uncalibrated {e:?}"));
            let got_dbm = got_fs + ci8_scale_db() + bc.k_center_db;
            worst_fs = worst_fs.max((got_fs - want_fs).abs());
            worst_m = worst_m.max((got_dbm - want_dbm).abs());
            assert!(
                (got_fs - want_fs).abs() <= 1.0,
                "[{SPACE_050}] segment {i}: {got_fs:.2} vs {want_fs:.2} dBFS/Hz"
            );
            assert!(
                (got_dbm - want_dbm).abs() <= 1.0,
                "[{SPACE_050}] segment {i}: calibrated {got_dbm:.2} vs {want_dbm:.2} dBm/Hz"
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
