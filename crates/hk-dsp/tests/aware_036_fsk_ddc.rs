//! AWARE-036 (periodic 2-FSK sensor bursts): the `fsk_burst_train` synthetic, placed 300 kHz
//! off centre in a 2 Msps window, is down-converted to a narrow 50 kS/s channel. The two FSK
//! tones at ±deviation (around the carrier offset) are present, and the per-Hz SNR of the
//! bursts is preserved within 1 dB (the DDC neither loses signal nor folds in noise).

mod chan_common;
mod common;

use chan_common::*;
use common::provenance;
use hk_dsp::{Ddc, DdcSpec};
use hk_e2e::{SynthRequest, synth_or_skip};
use num_complex::{Complex, Complex32};

const AWARE_036: &str = "AWARE-036";

fn mean_over<T: hk_dsp::IqSample>(x: &[T], spans: &[(usize, usize)]) -> f64 {
    let (mut sum, mut count) = (0.0, 0usize);
    for &(a, b) in spans {
        sum += power(&x[a..b]) * (b - a) as f64;
        count += b - a;
    }
    sum / count as f64
}

#[test]
fn aware_036_fsk_burst_ddc_keeps_tones_and_snr() {
    let fs = 2e6;
    let chan = 300e3;
    let snr_db = 20.0;
    let out = synth_or_skip!(
        SynthRequest::new("fsk_burst_train")
            .seed(36)
            .param("sample_rate", fs)
            .param("channel_offset_hz", chan)
            .param("snr_db", snr_db)
            .param("duration_s", 0.4)
    );
    let fx = out.fixture(0).unwrap();
    let bursts = fx.of_kind("fsk-burst");
    assert!(bursts.len() >= 2, "[{AWARE_036}] need bursts");
    let dev = bursts[0].expect_f64("deviation_hz");
    let cfo = bursts[0].expect_f64("cfo_hz");
    let bw = bursts[0].bandwidth_hz();
    let iq: Vec<Complex<i8>> = read_ci8(&fx);
    let prov = provenance(fx.center_hz_at(0).unwrap(), fs);

    let mut ddc = Ddc::new(DdcSpec::new(chan, 40e3).with_output_rate(50e3), fs).unwrap();
    let (y, t0) = run_ddc(&mut ddc, &iq, &prov, 65_536);
    let ofs = ddc.output_rate_hz();

    // Source-index spans: burst interiors and quiet gaps, with guards well beyond filter spans.
    let guard = (2e-3 * fs) as usize;
    let quiet_guard = (5e-3 * fs) as usize;
    let mut sig = Vec::new();
    let mut quiet = Vec::new();
    let mut prev_end = (t0.source_index as usize) + quiet_guard;
    for b in &bursts {
        let (s, e) = (
            b.sample_start as usize,
            (b.sample_start + b.sample_count) as usize,
        );
        sig.push((s + guard, e - guard));
        if s > prev_end + quiet_guard {
            quiet.push((prev_end, s - quiet_guard));
        }
        prev_end = e + quiet_guard;
    }
    if iq.len() > prev_end + quiet_guard {
        quiet.push((prev_end, iq.len() - quiet_guard));
    }
    let to_out = |spans: &[(usize, usize)]| -> Vec<(usize, usize)> {
        spans
            .iter()
            .map(|&(a, b)| {
                let lo = t0.output_position_of(a as f64).ceil().max(0.0) as usize;
                let hi = (t0.output_position_of(b as f64).floor() as usize).min(y.len());
                (lo, hi)
            })
            .filter(|(a, b)| b > a)
            .collect()
    };
    let (sig_out, quiet_out) = (to_out(&sig), to_out(&quiet));

    // Input: per-Hz SNR from burst and quiet powers (white noise: N0 = N / fs).
    let (pin_b, pin_n) = (mean_over(&iq, &sig), mean_over(&iq, &quiet));
    let n0_in = pin_n / fs;
    let snr_in = db10((pin_b - pin_n) / n0_in);
    // Output: noise density measured in the passband.
    let (pout_b, pout_n) = (mean_over(&y, &sig_out), mean_over(&y, &quiet_out));
    let quiet_samples: Vec<Complex32> = quiet_out
        .iter()
        .flat_map(|&(a, b)| y[a..b].iter().copied())
        .collect();
    let n = 512;
    let s = psd(&quiet_samples, n, ofs);
    let inband: Vec<f64> = (0..n)
        .filter(|&k| bin_hz(k, n, ofs).abs() <= 15e3)
        .map(|k| s[k])
        .collect();
    let n0_out = inband.iter().sum::<f64>() / inband.len() as f64;
    let snr_out = db10((pout_b - pout_n) / n0_out);
    let snr_in_bw = snr_in - db10(bw);
    assert!(
        (snr_in_bw - snr_db).abs() < 1.5,
        "[{AWARE_036}] input SNR in {bw:.0} Hz is {snr_in_bw:.2} dB, generator says {snr_db}"
    );
    assert!(
        (snr_out - snr_in).abs() <= 1.0,
        "[{AWARE_036}] SNR changed through the DDC: {snr_in:.2} -> {snr_out:.2} dB-Hz"
    );
    assert!(
        db10(n0_out / n0_in).abs() < 0.5,
        "[{AWARE_036}] passband noise density changed by {:.2} dB",
        db10(n0_out / n0_in)
    );

    // Tones: instantaneous frequency clusters at cfo ± deviation.
    let mut upper = Vec::new();
    let mut lower = Vec::new();
    let mut total = 0usize;
    for &(a, b) in &sig_out {
        for f in discriminator(&y[a..b], ofs) {
            total += 1;
            if (f - (cfo + dev)).abs() < 0.25 * dev {
                upper.push(f);
            } else if (f - (cfo - dev)).abs() < 0.25 * dev {
                lower.push(f);
            }
        }
    }
    let near = (upper.len() + lower.len()) as f64 / total as f64;
    assert!(
        near > 0.6,
        "[{AWARE_036}] only {:.0}% of burst samples at ±deviation",
        near * 100.0
    );
    for (name, v, want) in [
        ("+dev", &mut upper, cfo + dev),
        ("-dev", &mut lower, cfo - dev),
    ] {
        assert!(
            v.len() as f64 > 0.2 * total as f64,
            "[{AWARE_036}] {name} tone missing ({} of {total} samples)",
            v.len()
        );
        v.sort_by(f64::total_cmp);
        let median = v[v.len() / 2];
        assert!(
            (median - want).abs() < 0.05 * dev,
            "[{AWARE_036}] {name} tone at {median:.0} Hz, expected {want:.0} Hz"
        );
    }
    eprintln!(
        "[{AWARE_036}] 2 Msps -> 50 kS/s ({} bursts): SNR {snr_in:.2} -> {snr_out:.2} dB-Hz \
         ({snr_in_bw:.2} dB in {bw:.0} Hz), N0 change {:.2} dB, {:.0}% of samples at \
         cfo±dev ({:.0}/{:.0} Hz)",
        bursts.len(),
        db10(n0_out / n0_in),
        near * 100.0,
        cfo + dev,
        cfo - dev
    );
}
