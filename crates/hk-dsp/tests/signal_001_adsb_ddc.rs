//! SIGNAL-001 (ADS-B / Mode S): the `adsb_squitter` synthetic, generated at 20 Msps, is placed
//! 4 MHz off centre next to a strong CW interferer and down-converted to 2.4 Msps (rational
//! resampling) and 2 Msps (integer decimation), the rates ADS-B decoders expect. At every
//! truth message the pulse envelope is preserved: a preamble pulse-position correlation peaks
//! where the DDC time map places the message (within half an output sample), and pulses stand
//! well above the gaps. Decoding belongs to the T-015 plugin.

mod chan_common;
mod common;

use std::f64::consts::PI;

use chan_common::*;
use common::provenance;
use hk_dsp::{Ddc, DdcSpec};
use hk_e2e::{SynthRequest, synth_or_skip};
use num_complex::Complex32;

const SIGNAL_001: &str = "SIGNAL-001";
const PREAMBLE_US: [f64; 4] = [0.0, 1.0, 3.5, 4.5];

/// Fraction of the output sample cell centred at `t_us` covered by preamble pulses.
fn preamble_cell(t_us: f64, cell_us: f64) -> f64 {
    let (a, b) = (t_us - cell_us / 2.0, t_us + cell_us / 2.0);
    PREAMBLE_US
        .iter()
        .map(|&p| (b.min(p + 0.5) - a.max(p)).max(0.0))
        .sum::<f64>()
        / cell_us
}

#[test]
fn signal_001_adsb_ddc_preserves_pulse_positions() {
    let fs = 20e6;
    let offset = 4e6;
    let out = synth_or_skip!(
        SynthRequest::new("adsb_squitter")
            .seed(1)
            .param("sample_rate", fs)
            .param("duration_s", 0.06)
            .param("messages_per_aircraft", 3)
    );
    let fx = out.fixture(0).unwrap();
    let msgs = fx.of_kind("adsb-df17");
    assert!(msgs.len() >= 8, "[{SIGNAL_001}] need messages");
    let base = fx.samples().unwrap();
    let x: Vec<Complex32> = base
        .iter()
        .enumerate()
        .map(|(n, s)| {
            let mix = 2.0 * PI * offset * n as f64 / fs;
            let jam = 2.0 * PI * -3.3e6 * n as f64 / fs;
            Complex32::new(s.re, s.im) * Complex32::from_polar(1.0, mix as f32)
                + Complex32::from_polar(0.3, jam as f32)
        })
        .collect();
    let prov = provenance(fx.center_hz_at(0).unwrap(), fs);

    for (bw, rate) in [(2.0e6, 2.4e6), (1.6e6, 2.0e6)] {
        let mut ddc = Ddc::new(DdcSpec::new(offset, bw).with_output_rate(rate), fs).unwrap();
        let (y, t0) = run_ddc(&mut ddc, &x, &prov, 262_144);
        let env: Vec<f64> = y.iter().map(|s| f64::from(s.norm())).collect();
        let cell_us = 1e6 / rate;
        let k_len = (8.0 / cell_us).ceil() as usize;
        let template: Vec<f64> = (0..k_len)
            .map(|k| preamble_cell(k as f64 * cell_us, cell_us))
            .collect();
        let t_mean = template.iter().sum::<f64>() / k_len as f64;
        let corr = |at: isize| -> f64 {
            (0..k_len)
                .map(|k| env[(at + k as isize) as usize] * (template[k] - t_mean))
                .sum()
        };
        let mut worst_offset = 0.0f64;
        let mut worst_contrast = f64::INFINITY;
        for m in &msgs {
            let pos = t0.output_position_of(m.sample_start as f64);
            let base_k = pos.round() as isize;
            let (best, best_c) = (-6..=6)
                .map(|l| (base_k + l, corr(base_k + l)))
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .unwrap();
            // Sub-sample peak by parabolic interpolation.
            let (cm, cp) = (corr(best - 1), corr(best + 1));
            let denom = cm - 2.0 * best_c + cp;
            let peak = best as f64
                + if denom < 0.0 {
                    0.5 * (cm - cp) / denom
                } else {
                    0.0
                };
            let offset_samples = peak - pos;
            worst_offset = worst_offset.max(offset_samples.abs());
            assert!(
                offset_samples.abs() <= 0.5,
                "[{SIGNAL_001}] {rate} S/s: preamble at sample {} peaks {offset_samples:+.2} \
                 output samples from its mapped position",
                m.sample_start
            );
            let quiet = corr(base_k - (150e-6 * rate) as isize).abs();
            assert!(
                best_c > 5.0 * quiet,
                "[{SIGNAL_001}] {rate} S/s: correlation peak {best_c:.3} vs quiet {quiet:.3}"
            );
            let at = |us: f64| {
                let p = pos + us / cell_us;
                let i = p.floor() as usize;
                let f = p - p.floor();
                env[i] * (1.0 - f) + env[i + 1] * f
            };
            let pulses = [0.25, 1.25, 3.75, 4.75].map(at).iter().sum::<f64>() / 4.0;
            let gaps = [2.25, 2.75, 5.75, 6.75].map(at).iter().sum::<f64>() / 4.0;
            worst_contrast = worst_contrast.min(pulses / gaps);
            assert!(
                pulses > 3.0 * gaps,
                "[{SIGNAL_001}] {rate} S/s: pulse envelope smeared at sample {} \
                 (pulses {pulses:.3}, gaps {gaps:.3})",
                m.sample_start
            );
        }
        eprintln!(
            "[{SIGNAL_001}] 20 Msps -> {:.1} Msps (bw {:.1} MHz, stage 1 /{}, stage 2 {:?}): \
             {} messages, worst peak offset {worst_offset:.2} samples, worst pulse/gap \
             {worst_contrast:.1}",
            rate / 1e6,
            bw / 1e6,
            ddc.plan().xlate_decimation,
            ddc.plan().resample.as_ref().map(|r| r.kind),
            msgs.len()
        );
    }
}
