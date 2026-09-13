//! SIGNAL-062 (broadcast FM with RDS): the `fm_broadcast_rds` synthetic, offset by 400 kHz
//! inside a 2 Msps window, is down-converted straight from native `Complex<i8>` to 250 kS/s by
//! a DDC built from a JSON data spec. The WFM signal fills the channel (power and occupied
//! bandwidth), and a quadrature discriminator on the DDC output shows the 19 kHz stereo pilot
//! at its deviation. The discriminator is test-local; demodulation proper belongs to T-012.

mod chan_common;
mod common;

use chan_common::*;
use common::provenance;
use hk_dsp::{Ddc, DdcSpec};
use hk_e2e::{SynthRequest, synth_or_skip};

const SIGNAL_062: &str = "SIGNAL-062";

/// 99% occupied bandwidth of a DC-centred PSD with bins `bin_hz` wide: the span left after
/// trimming 0.5% of the power from each side.
fn obw99(s: &[f64], bin_hz: f64) -> f64 {
    let total: f64 = s.iter().sum();
    let (mut lo, mut acc) = (0, 0.0);
    while acc + s[lo] < 0.005 * total {
        acc += s[lo];
        lo += 1;
    }
    let (mut hi, mut acc) = (s.len() - 1, 0.0);
    while acc + s[hi] < 0.005 * total {
        acc += s[hi];
        hi -= 1;
    }
    (hi - lo + 1) as f64 * bin_hz
}

#[test]
fn signal_062_wfm_ddc_to_250k_fills_channel_and_shows_pilot() {
    let fs = 2e6;
    let offset = 400e3;
    let out = synth_or_skip!(
        SynthRequest::new("fm_broadcast_rds")
            .seed(62)
            .param("sample_rate", fs)
            .param("offset_hz", offset)
            .param("duration_s", 0.25)
    );
    let fx = out.fixture(0).unwrap();
    let wfm = fx.of_kind("wfm-broadcast");
    assert_eq!(wfm.len(), 1);
    let wfm = wfm[0];
    let pilot_dev = wfm.expect_f64("/pilot/deviation_hz");
    let mono_dev = wfm.expect_f64("/audio/mono_deviation_hz");
    let power_dbfs = wfm.expect_f64("power_dbfs");
    let iq = read_ci8(&fx);
    let prov = provenance(fx.center_hz_at(0).unwrap(), fs);

    let spec: DdcSpec = serde_json::from_value(serde_json::json!({
        "center_offset_hz": offset, "bandwidth_hz": 220e3, "output_rate_hz": 250e3
    }))
    .unwrap();
    let mut ddc = Ddc::new(spec, fs).unwrap();
    let (y, t0) = run_ddc(&mut ddc, &iq, &prov, 65_536);
    let ofs = ddc.output_rate_hz();
    assert_eq!(ofs, 250e3);
    assert_eq!(t0.source_per_output, 8.0);

    // The channel holds the station's power (ci8 is scaled by 127 in the generator, 128 in
    // hk-core).
    let p = db10(power(&y)) + 20.0 * (128.0f64 / 127.0).log10();
    assert!(
        (p - power_dbfs).abs() < 0.5,
        "[{SIGNAL_062}] channel power {p:.2} dBFS vs truth {power_dbfs:.2}"
    );

    // Occupied bandwidth (99%): the channel carries the station's whole occupied bandwidth,
    // measured the same way (same 244 Hz bins) on the wideband input around the station.
    let n_out = 1024;
    let obw = obw99(&psd(&y, n_out, ofs), ofs / n_out as f64);
    let n_in = 8192;
    let x: Vec<num_complex::Complex32> = iq
        .iter()
        .map(|s| hk_dsp::IqSample::to_complex32(*s))
        .collect();
    let s_in = psd(&x, n_in, fs);
    let around: Vec<f64> = (0..n_in)
        .filter(|&k| (bin_hz(k, n_in, fs) - offset).abs() <= ofs / 2.0)
        .map(|k| s_in[k])
        .collect();
    let obw_in = obw99(&around, fs / n_in as f64);
    assert!(
        obw >= 60e3,
        "[{SIGNAL_062}] 99% occupied bandwidth {obw:.0} Hz is not wideband FM"
    );
    assert!(
        (obw - obw_in).abs() <= 2e3,
        "[{SIGNAL_062}] channel OBW {obw:.0} Hz vs station OBW at input {obw_in:.0} Hz"
    );

    // Discriminator: the 19 kHz pilot at its deviation, the 1 kHz mono tone at half the mono
    // deviation, and quiet neighbours.
    let d = discriminator(&y, ofs);
    let pilot = sine_amplitude(&d, 19_000.0, ofs);
    let mono = sine_amplitude(&d, 1_000.0, ofs);
    let quiet = sine_amplitude(&d, 17_000.0, ofs).max(sine_amplitude(&d, 21_000.0, ofs));
    let pilot_err = 20.0 * (pilot / pilot_dev).log10();
    assert!(
        pilot_err.abs() < 1.0,
        "[{SIGNAL_062}] pilot deviation {pilot:.0} Hz vs {pilot_dev:.0} Hz"
    );
    assert!(
        (20.0 * (mono / (mono_dev / 2.0)).log10()).abs() < 1.0,
        "[{SIGNAL_062}] mono 1 kHz deviation {mono:.0} Hz vs {:.0} Hz",
        mono_dev / 2.0
    );
    let pilot_snr = 20.0 * (pilot / quiet).log10();
    assert!(
        pilot_snr > 20.0,
        "[{SIGNAL_062}] pilot only {pilot_snr:.1} dB above neighbouring MPX"
    );
    eprintln!(
        "[{SIGNAL_062}] 2 Msps ci8 -> 250 kS/s: power {p:.2} dBFS (truth {power_dbfs:.2}), \
         OBW99 {:.0} kHz, pilot {pilot:.0} Hz ({pilot_err:+.2} dB, {pilot_snr:.1} dB over MPX \
         floor), mono {mono:.0} Hz",
        obw / 1e3
    );
}
