//! SIGNAL-062 (broadcast FM with RDS), recorded: `fm_100p8M_2p4M_l32g30a1_t1p5_5s` (HackRF One,
//! 2.4 Msps, station at +500 kHz, 5 s).
//!
//! - **Station:** C13 on the station box → OBW99 consistent with broadcast FM (120–260 kHz).
//! - **Pilot → clock:** a test-local FM discriminator (demodulation proper is T-012) gives the
//!   MPX; [`tone_frequency`] measures the 19 kHz pilot and [`ppm_from_line`] gives the clock
//!   error: −6.8 ± 0.5 ppm (fixture: −6.77).
//! - **RDS CFO via x²:** the MPX is treated as a stream; a snippet mixed at a deliberately wrong
//!   56.7 kHz goes through C13 with the DSB hint. The x² line must give the subcarrier offset
//!   within 1 Hz of 3 × the fixture pilot − 56 700 Hz (S5 §3.4: exact; the centroid was 64 Hz
//!   off).

mod common;

use common::*;
use hk_dsp::{Ddc, DdcSpec};
use hk_e2e::Fixture;
use hk_estimate::clock::{ppm_from_line, tone_frequency};
use hk_estimate::{FamilyHint, Hints, Method, SnippetRequest};
use num_complex::Complex32;

const SIGNAL_062: &str = "SIGNAL-062";
const NAME: &str = "fm_100p8M_2p4M_l32g30a1_t1p5_5s";

#[test]
fn signal_062_station_obw_pilot_ppm_and_rds_cfo() {
    let (meta, data) = fixture_or_skip!(NAME);
    let fx = Fixture::load(&meta).unwrap();
    let fs = fx.sample_rate;
    let prov = meta_provenance(&meta);
    let center = prov.tune.center_hz;
    let station = fx.of_kind("wfm-broadcast")[0];
    let pilot_truth = station.expect_f64("/pilot/frequency_hz");
    let iq = read_ci8(&data);

    // Station parameters (first 2 s).
    let req = SnippetRequest {
        start_index: 0,
        end_index: (2.0 * fs) as u64,
        center_offset_hz: station.center_hz() - center,
        bandwidth_hz: station.bandwidth_hz(),
    };
    let mut ex = hk_estimate::SnippetExtractor::new(Default::default());
    let t = std::time::Instant::now();
    let snip = ex
        .extract(common::info(0, &prov), &iq, &req)
        .expect("station snippet");
    let extract_ms = t.elapsed().as_secs_f64() * 1e3;
    let mut est = hk_estimate::ParamEstimator::new(Default::default());
    let ps = est.estimate(&snip, &Hints::default());
    let carrier_truth = station.expect_f64("rf_center_hz");
    eprintln!(
        "[{SIGNAL_062}] station: OBW99 {:?} Hz, -26 dB {:?}, SNR box {:?}, noise {:?} ({:?}), \
         rf err (uncorrected) {:?} Hz, flags {:?}, {} samples at {:.0} Hz, extract {extract_ms:.0} \
         ms, estimate {:.1} ms",
        ps.obw99_hz.value(),
        ps.xdb(26.0).and_then(|e| e.value()),
        ps.snr_box_db.value(),
        ps.noise_density.value().map(db),
        ps.noise_density.method(),
        ps.rf_center_hz.value().map(|v| v - carrier_truth),
        ps.flags,
        snip.samples.len(),
        snip.sample_rate_hz,
        ps.cost_us as f64 / 1e3
    );
    let obw = ps.obw99_hz.value().expect("station OBW");
    assert!(
        (120e3..=260e3).contains(&obw),
        "[{SIGNAL_062}] station OBW99 {obw:.0} Hz is not broadcast FM"
    );

    // MPX via a test-local discriminator at 240 kS/s.
    let mpx_fs = 240e3;
    let mut ddc = Ddc::new(
        DdcSpec::new(station.center_hz() - center, 220e3).with_output_rate(mpx_fs),
        fs,
    )
    .unwrap();
    let block = ddc.process(common::info(0, &prov), &iq).unwrap();
    let scale = mpx_fs / std::f64::consts::TAU / 150e3;
    let mpx: Vec<f32> = block
        .samples
        .windows(2)
        .map(|w| (f64::from((w[1] * w[0].conj()).arg()) * scale) as f32)
        .collect();

    let pilot = tone_frequency(&mpx, mpx_fs, 18_900.0, 19_100.0);
    let ppm = ppm_from_line(&pilot, 19_000.0);
    let fixture_ppm = fx
        .scenario()
        .and_then(|s| s.f64("clock_ppm_this_capture"))
        .unwrap_or(-6.8);
    eprintln!(
        "[{SIGNAL_062}] pilot {:?} (truth {pilot_truth:.3}), clock {:?} ppm (fixture {fixture_ppm:.3})",
        pilot, ppm
    );
    let pilot_hz = pilot.value().expect("pilot");
    assert!(
        (pilot_hz - pilot_truth).abs() < 0.01,
        "[{SIGNAL_062}] pilot {pilot_hz}"
    );
    let ppm_v = ppm.value().expect("ppm");
    assert!(
        (ppm_v + 6.8).abs() <= 0.5,
        "[{SIGNAL_062}] clock {ppm_v:.2} ppm"
    );

    // RDS: the MPX as a 240 kS/s stream, snippet deliberately mixed at 56.7 kHz.
    let mpx_prov = provenance(0.0, mpx_fs);
    let mpx_iq: Vec<Complex32> = mpx.iter().map(|&v| Complex32::new(v, 0.0)).collect();
    let rds_req = SnippetRequest {
        start_index: 0,
        end_index: mpx_iq.len() as u64,
        center_offset_hz: 56_700.0,
        bandwidth_hz: 5_000.0,
    };
    let rds = ex
        .extract(common::info(0, &mpx_prov), &mpx_iq, &rds_req)
        .expect("RDS snippet");
    // The MPX noise rises with frequency (FM): a caller floor from the guard gaps either side
    // of the RDS band (53.2–54.4 kHz above L−R, 59.6–61.0 kHz), averaged, stands in for C08.
    let spec = hk_dsp::welch(&mpx_iq, mpx_fs, 0.0, &hk_dsp::WelchConfig::new(16_384)).unwrap();
    let gap = |lo: f64, hi: f64| {
        let bins: Vec<f64> = (0..spec.bins())
            .filter(|&k| (lo..=hi).contains(&spec.bin_offset_hz(k)))
            .map(|k| f64::from(spec.psd[k]))
            .collect();
        bins.iter().sum::<f64>() / bins.len() as f64
    };
    let floor = 0.5 * (gap(53_200.0, 54_400.0) + gap(59_600.0, 61_000.0));
    let hints = Hints {
        family: FamilyHint::Dsb,
        noise: Some(hk_estimate::NoiseReference {
            density: floor,
            sigma_db: 0.3,
        }),
        ..Default::default()
    };
    let rps = est.estimate(&rds, &hints);
    let truth = 3.0 * pilot_truth - 56_700.0;
    eprintln!(
        "[{SIGNAL_062}] RDS: x² {:?}, centroid {:?} (truth {truth:.3}), OBW99 {:?}, SNR {:?}, \
         noise {:?}, {} samples at {:.0} Hz, estimate {:.1} ms",
        rps.cfo.square_line,
        rps.cfo.centroid.value(),
        rps.obw99_hz.value(),
        rps.snr_box_db.value(),
        rps.noise_density.method(),
        rds.samples.len(),
        rds.sample_rate_hz,
        rps.cost_us as f64 / 1e3
    );
    assert_eq!(
        rps.cfo_hz.method(),
        Method::CfoSquareLine,
        "[{SIGNAL_062}] {:?}",
        rps.cfo
    );
    let cfo = rps.cfo_hz.value().unwrap();
    assert!(
        (cfo - truth).abs() <= 1.0,
        "[{SIGNAL_062}] RDS x² CFO {cfo:.3} vs {truth:.3} Hz"
    );
    assert!(rps.cfo_hz.sigma().unwrap() < 1.0);
}
