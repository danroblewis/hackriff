//! AWARE-042 (duty-cycle and occupancy statistics): an on/off burst schedule shows as per-bin
//! SK > 1 and in the max-hold, and the SK recovers the duty cycle.

mod common;

use common::*;
use hk_core::Discontinuity;
use hk_dsp::synth::{self, Rng};
use hk_dsp::{
    InputInfo, PersistenceConfig, PowerUnit, SpectrumFrame, StftConfig, StftProcessor, WelchConfig,
    WindowKind, sk,
};

#[test]
fn aware_042_burst_schedule_visible_in_sk_and_max_hold() {
    let fs = 1e6;
    let n = 1024;
    let m = 50;
    let bw = fs / n as f64;
    let burst_bins = 205.0; // ≈ +200 kHz
    let duty = 0.2; // 2 segments on, 8 off
    let frames_wanted = 4;
    let len = frames_wanted * m * n;

    let mut rng = Rng::new(0xa3a3_0042);
    let mut x = synth::complex_noise(&mut rng, len, 1e-4); // −40 dBFS
    let burst = synth::tone(0, len, burst_bins * bw, fs, 1e-2, 0.0); // −20 dBFS when on
    for seg in 0..len / n {
        if seg % 10 < 2 {
            synth::add_into(
                &mut x[seg * n..(seg + 1) * n],
                &burst[seg * n..(seg + 1) * n],
            );
        }
    }

    let mut config = StftConfig::new(
        WelchConfig {
            fft_len: n,
            overlap: 0,
            window: WindowKind::Hann,
            holds: true,
            spectral_kurtosis: true,
        },
        m,
    );
    config.persistence = Some(PersistenceConfig {
        levels: 80,
        min_db: -80.0,
        max_db: 0.0,
        tau_s: 1.0,
    });
    let mut stft = StftProcessor::new(config).unwrap();
    let prov = provenance(433.92e6, fs);
    let mut frames: Vec<SpectrumFrame> = Vec::new();
    for (i, chunk) in x.chunks(4096).enumerate() {
        let flags = if i == 0 {
            Discontinuity::STREAM_START
        } else {
            Discontinuity::NONE
        };
        let h = header((i * 4096) as u64, &prov, flags);
        stft.push(InputInfo::from(&h), chunk, |f| frames.push(f.clone()));
    }
    assert_eq!(frames.len(), frames_wanted);

    let sigma = sk::std_dev(m as u32);
    for f in &frames {
        let s = &f.spectrum;
        let b = s.bin_for_offset_hz(burst_bins * bw).unwrap();
        let skb = f64::from(s.sk[b]);
        assert!(skb > 1.0 + 10.0 * sigma, "burst bin SK {skb}");

        // Duty cycle from SK (constant-envelope burst, far above noise).
        let duty_est = 1.0 / (1.0 + skb * (m as f64 - 1.0) / (m as f64 + 1.0));
        assert!((duty_est - duty).abs() < 0.02, "duty {duty_est}");

        // Bins away from the burst look like noise: mean SK ≈ 1 and few cross 1 + 5σ. (At
        // M = 50 SK is right-skewed, so a Gaussian 5σ cut still passes ~0.5% of noise bins;
        // T-006 should use Pearson thresholds.)
        let quiet: Vec<usize> = (0..n).filter(|&i| i.abs_diff(b) > 4).collect();
        let quiet_sk: Vec<f32> = quiet.iter().map(|&i| s.sk[i]).collect();
        assert!((mean(&quiet_sk) - 1.0).abs() < 0.05);
        let false_hits = quiet_sk
            .iter()
            .filter(|&&v| f64::from(v) > 1.0 + 5.0 * sigma)
            .count();
        assert!(
            false_hits * 100 <= quiet.len(),
            "{false_hits} noise bins flagged"
        );

        // Max-hold shows the burst at its on-power; the average dilutes it by the duty cycle.
        let max_db = s.to_db(&s.max_hold, PowerUnit::DbfsPerBin);
        let avg_db = s.to_db(&s.psd, PowerUnit::DbfsPerBin);
        assert!(
            (f64::from(max_db[b]) - (-20.0)).abs() < 0.1,
            "max-hold {}",
            max_db[b]
        );
        assert!((f64::from(max_db[b] - avg_db[b]) - db(1.0 / duty)).abs() < 0.2);
        let noise_avg = avg_db[quiet[0]];
        assert!(max_db[b] > noise_avg + 40.0);
    }

    // Persistence: the burst's level at the burst bin has hits; a quiet bin never reaches it.
    let p = stft.persistence().unwrap();
    let b = frames[0]
        .spectrum
        .bin_for_offset_hz(burst_bins * bw)
        .unwrap();
    let level = 60; // −20 dBFS on an −80..0 dB, 80-level grid
    assert!((p.level_db(level) + 20.0).abs() < 1e-3);
    assert!(p.value(b, level) + p.value(b, level - 1) > 1.0);
    assert_eq!(p.value(b + 100, level), 0.0);
}
