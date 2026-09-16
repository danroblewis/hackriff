//! T-070 WFM objective on the recorded FM fixture, directly on the IQ (the pipeline e2e tests in
//! `tests/e2e/tests/refine.rs` drive the same loop through the mock SDR). Coarse starts around
//! the 101.3 MHz station refine to the fixture's private truth centre (receiver frame) within
//! 2 kHz, with a 150–220 kHz bandwidth and RDS PI decoded in the validation.

mod common;

use common::*;
use hk_demod::refine::{
    EvalDepth, IqWindow, LoopConfig, Objective, RefineStart, RefinementLoop, Tuning, WfmObjective,
    refine_wfm,
};
use hk_e2e::Fixture;

const NAME: &str = "fm_100p8M_2p4M_l32g30a1_t1p5_5s";

#[test]
fn wfm_refinement_on_the_recorded_station() {
    let (meta, data) = fixture_or_skip!(NAME);
    let fx = Fixture::load(&meta).unwrap();
    let prov = meta_provenance(&meta);
    let station = fx.of_kind("wfm-broadcast")[0];
    let truth = station.expect_f64("/center_hz");
    let pi_truth = station.str("/rds/pi_hex").unwrap().to_owned();
    let iq = read_ci8(&data);
    let n = (1.0 * prov.tune.sample_rate_hz) as usize;
    let window = IqWindow::new(info(0, &prov), &iq[..n]);

    // `HK_REFINE_SCAN=1` prints the objective's measurements around the station (how the
    // defaults were chosen: MPX mean tracks the offset to ±100 kHz; pilot C/N0 is flat in
    // bandwidth; the ITU 99 % occupied bandwidth is about 200 kHz).
    if std::env::var_os("HK_REFINE_SCAN").is_some() {
        let mut obj = WfmObjective::default();
        let at = |center_hz: f64, bandwidth_hz: f64| Tuning {
            center_hz,
            bandwidth_hz,
            ..Tuning::default()
        };
        for off in [-100e3, -60e3, -30e3, -10e3, 0.0, 10e3, 30e3, 60e3, 100e3] {
            let m = obj.measure(window.leading(0.25), &at(truth + off, 200e3), None);
            eprintln!("offset {off:>8}: {:?}", m.unwrap());
        }
        for bw in [80e3, 120e3, 160e3, 200e3, 220e3] {
            let m = obj.measure(window.leading(0.25), &at(truth, bw), None);
            eprintln!("bandwidth {bw:>7}: {:?}", m.unwrap());
        }
        for secs in [0.1, 0.25, 1.0] {
            for off in [-5e3, 0.0, 5e3] {
                let o = obj.measure_obw(window.leading(secs), truth + off).unwrap();
                eprintln!("occupied bandwidth {secs} s, offset {off}: {o:?}");
            }
        }
    }

    for width in [60e3, 400e3] {
        for off in [-100e3, -50e3, 50e3, 100e3] {
            let start = RefineStart {
                center_hz: truth + off,
                bandwidth_hz: width,
                warm: false,
            };
            let o = refine_wfm(window, &start);
            let err = o.tuning.center_hz - truth;
            eprintln!(
                "start {off:+.0} Hz / {width:.0} Hz -> centre error {err:+.0} Hz, bw {:.0}, \
                 quality {:.1}, PI {:?}, {} iterations, {} evaluations, {:.2} s, stop {:?}, \
                 params {:?}",
                o.tuning.bandwidth_hz,
                o.quality,
                o.labels.get("rds_pi"),
                o.iterations,
                o.evaluations,
                o.elapsed_s,
                o.stop,
                o.mode_params
            );
            if std::env::var_os("HK_REFINE_TRACE").is_some() {
                for s in &o.trace {
                    eprintln!("   {s:?}");
                }
            }
            assert!(o.locked && o.converged, "{o:?}");
            assert!(err.abs() <= 2_000.0, "centre error {err} Hz");
            assert!(
                (150e3..=220e3).contains(&o.tuning.bandwidth_hz),
                "bandwidth {}",
                o.tuning.bandwidth_hz
            );
            assert_eq!(o.labels.get("rds_pi"), Some(&pi_truth));
        }
    }

    // Warm start from the result: stays put (hysteresis keeps it).
    let first = refine_wfm(
        window,
        &RefineStart {
            center_hz: truth + 30e3,
            bandwidth_hz: 200e3,
            warm: false,
        },
    );
    let mut l = RefinementLoop::new(WfmObjective::default(), LoopConfig::default());
    let again = l.run(
        IqWindow::new(info(n as u64, &prov), &iq[n..2 * n]),
        &RefineStart {
            center_hz: first.tuning.center_hz,
            bandwidth_hz: first.tuning.bandwidth_hz,
            warm: true,
        },
    );
    eprintln!(
        "warm: {:+.0} Hz, {} evaluations, accept {}",
        again.tuning.center_hz - truth,
        again.evaluations,
        l.accept(Some(&first), &again)
    );
    assert!(again.locked && (again.tuning.center_hz - truth).abs() <= 2_000.0);
    assert!(
        !l.accept(Some(&first), &again),
        "no retune on estimation noise"
    );
}

/// T-226, the T-188 case: a probe centre a few kHz off the carrier puts an acquisition grid point
/// one 50 kHz step away, where the WFM channel filter sits beside the carrier — it cuts MPX noise
/// while the 19 kHz pilot survives, so the 0.1 s acquisition window can read a *higher* pilot C/N0
/// there than at the station itself, and that decoy's own MPX mean points back at the station by
/// more than one step (−52.9 kHz). Acquisition used to discard such a correction, leaving the
/// search one step off the carrier; it is now clamped to one step and re-measured, so the search
/// reaches the station instead of merely refusing the decoy.
#[test]
fn an_acquisition_decoy_one_step_off_the_carrier_still_reaches_the_station() {
    let (meta, data) = fixture_or_skip!(NAME);
    let fx = Fixture::load(&meta).unwrap();
    let prov = meta_provenance(&meta);
    let station = fx.of_kind("wfm-broadcast")[0];
    let truth = station.expect_f64("/center_hz");
    let iq = read_ci8(&data);
    let n = (1.0 * prov.tune.sample_rate_hz) as usize;
    let window = IqWindow::new(info(0, &prov), &iq[..n]);

    // What the shallow acquisition window sees at the carrier and one grid step either side.
    let mut obj = WfmObjective::default();
    for off in [-50e3, 0.0, 50e3] {
        let t = Tuning {
            center_hz: truth + off,
            bandwidth_hz: 200e3,
            ..Tuning::default()
        };
        let m = obj
            .evaluate(window.leading(0.1), &t, EvalDepth::Acquire)
            .unwrap();
        eprintln!(
            "acquire {off:+.0} Hz: locked {}, quality {:.1} dB-Hz, correction {:?}",
            m.locked, m.quality, m.center_correction_hz
        );
    }

    // The probe centre of the T-188 failure: 2.8 kHz above the carrier, so the grid offers the
    // station at +2.8 kHz and the decoy at +52.8 kHz.
    let start = RefineStart {
        center_hz: truth + 2_800.0,
        bandwidth_hz: 200e3,
        warm: false,
    };
    let o = refine_wfm(window, &start);
    let err = o.tuning.center_hz - truth;
    eprintln!(
        "T-226: centre error {err:+.0} Hz, bandwidth {:.0} Hz, locked {}, validated {}, converged \
         {}, stop {:?}, {} evaluations",
        o.tuning.bandwidth_hz, o.locked, o.validated, o.converged, o.stop, o.evaluations
    );
    assert!(o.locked && o.validated, "{o:?}");
    assert!(err.abs() <= 2_000.0, "centre error {err} Hz");
}
