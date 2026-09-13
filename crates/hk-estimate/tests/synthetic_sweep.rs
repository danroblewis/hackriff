//! C13 synthetic sweep (T1/T4) for AWARE-036 (unknown burst triage) and SIGNAL-062 (the
//! estimators the FM/RDS path uses): tone, 2-FSK, BPSK and noise across SNR, with stated
//! tolerances; the mean-vs-median N0 regression; noise-only snippets; normalisation.
//!
//! Truth: SNR = signal power / (N0 · OBW99 of the clean signal); OBW99 from an independent
//! 8192-bin periodogram of the clean signal; CFO from the construction (the detection box is
//! centred 1.234 kHz off the emission).
//!
//! Tolerances (in-band SNR over the true OBW99):
//! - ≥ 20 dB: OBW99 within 10 %, SNR within ±1 dB.
//! - 10–15 dB: OBW99 within 25 %, SNR within −5…+1.5 dB (the documented low-SNR bias).
//! - CFO at ≥ 10 dB: FSK mid-point within 1 % of the symbol rate; BPSK x² within 0.5 %; tone
//!   centroid within a quarter bin and carrier line within 2 Hz.

mod common;

use common::*;
use hk_dsp::synth::{Rng, complex_noise};
use hk_dsp::{WelchConfig, welch};
use hk_estimate::{
    Estimate, FamilyHint, Hints, Method, NoiseReference, NormaliseConfig, Reason, normalise,
};

const AWARE_036: &str = "AWARE-036";
const SIGNAL_062: &str = "SIGNAL-062";

const FS: f64 = 1e6;
const OFFSET: f64 = 123_400.0;
const BOX_ERROR: f64 = -1_234.0;

fn val(e: &Estimate, what: &str, ctx: &str) -> f64 {
    e.value()
        .unwrap_or_else(|| panic!("[{AWARE_036}] {ctx}: {what} abstained: {e:?}"))
}

#[test]
fn sweep_fsk_and_bpsk_across_snr() {
    let mut rng = Rng::new(10);
    let fsk_rate = 9_600.0;
    let fsk = cpfsk(&random_bits(&mut rng, 400), FS, fsk_rate, 4_800.0);
    let bpsk_rate = 20_000.0;
    let bpsk = bpsk_rrc(&mut rng, 400, FS, bpsk_rate, 0.35);
    let cases = [
        ("2fsk", &fsk, fsk_rate, FamilyHint::Fsk { levels: 2 }, 0.01),
        ("bpsk", &bpsk, bpsk_rate, FamilyHint::Dsb, 0.005),
    ];
    for (name, sig, rate, hint, cfo_tol) in cases {
        let obw_ref = obw99_reference(sig, FS);
        for (i, snr) in [6.0, 10.0, 15.0, 20.0, 30.0].into_iter().enumerate() {
            let scene = Scene::new(sig, FS, OFFSET, snr, obw_ref, 8_000, 8_000, 100 + i as u64);
            let req = scene.request(BOX_ERROR, obw_ref);
            let hints = Hints {
                family: hint,
                ..Default::default()
            };
            let (snip, ps) = run(&scene.iq, FS, &req, &hints);
            let ctx = format!("{name} at {snr} dB");
            let rf_true = CENTER_HZ + OFFSET;
            eprintln!(
                "[{AWARE_036}] {ctx}: obw {:?} (ref {obw_ref:.0}) snr box {:?} ext {:?} \
                 cfo {:?} centroid {:?} rf err {:?} noise {:?} rate {:.0} nfft {} cost {} us",
                ps.obw99_hz.value(),
                ps.snr_box_db.value(),
                ps.snr_extent_db.value(),
                ps.cfo_hz.value(),
                ps.cfo.centroid.value(),
                ps.rf_center_hz.value().map(|v| v - rf_true),
                ps.noise_density.value().map(|v| db(v / scene.n0())),
                snip.sample_rate_hz,
                ps.nfft,
                ps.cost_us
            );
            assert_eq!(
                ps.noise_density.method(),
                Method::NoisePad,
                "[{AWARE_036}] {ctx}"
            );
            if snr < 10.0 {
                // Below the asserted range: any value must carry a finite uncertainty.
                for e in [ps.obw99_hz, ps.snr_box_db, ps.cfo_hz] {
                    if let Estimate::Measured { sigma, .. } = e {
                        assert!(
                            sigma.is_finite() && sigma > 0.0,
                            "[{AWARE_036}] {ctx}: {e:?}"
                        );
                    }
                }
                continue;
            }
            let snr_est = val(&ps.snr_box_db, "snr box", &ctx);
            let snr_true = scene.snr_db_in(obw_ref);
            let (obw_tol, lo, hi) = if snr >= 20.0 {
                (0.10, -1.0, 1.0)
            } else {
                (0.25, -5.0, 1.5)
            };
            let err = snr_est - snr_true;
            assert!(
                (lo..=hi).contains(&err),
                "[{AWARE_036}] {ctx}: SNR {snr_est:.2} vs {snr_true:.2}"
            );
            // Below 15 dB a short burst may sit under the bandwidth floor (`min_band_z`).
            if snr < 15.0 && ps.obw99_hz.reason() == Some(Reason::LowSnr) {
                eprintln!(
                    "[{AWARE_036}] {ctx}: below the bandwidth floor: {:?}",
                    ps.obw99_hz
                );
            } else {
                let obw = val(&ps.obw99_hz, "obw99", &ctx);
                let rel = (obw - obw_ref).abs() / obw_ref;
                assert!(
                    rel <= obw_tol,
                    "[{AWARE_036}] {ctx}: OBW {obw:.0} vs {obw_ref:.0}"
                );
                let ext = val(&ps.snr_extent_db, "snr extent", &ctx) - snr_true;
                assert!(
                    (lo..=hi).contains(&ext),
                    "[{AWARE_036}] {ctx}: extent SNR err {ext:.2}"
                );
            }
            let cfo = val(&ps.cfo_hz, "cfo", &ctx);
            assert_ne!(
                ps.cfo_hz.method(),
                Method::CfoCentroid,
                "[{AWARE_036}] {ctx}: hint ignored"
            );
            let sigma = ps.cfo_hz.sigma().unwrap();
            // ≥ 15 dB: the stated tolerance. 10 dB: the reported uncertainty must cover the error.
            let tol = if snr >= 15.0 {
                cfo_tol * rate
            } else {
                3.0 * sigma + 0.5 * cfo_tol * rate
            };
            assert!(
                (cfo + BOX_ERROR).abs() <= tol,
                "[{AWARE_036}] {ctx}: CFO {cfo:.1} vs {:.1} (tol {tol:.1}, {:?})",
                -BOX_ERROR,
                ps.cfo
            );
            let rf = val(&ps.rf_center_hz, "rf centre", &ctx);
            assert!((rf - rf_true).abs() <= tol, "[{AWARE_036}] {ctx}: rf {rf}");
            assert!(
                sigma > 0.0 && sigma < 3.0 * cfo_tol * rate,
                "[{AWARE_036}] {ctx}: σ {sigma}"
            );
            let ext = ps.extent.expect("extent");
            let (s0, s1) = (ext.source_start, ext.source_end);
            let (t0, t1) = (scene.start as f64, (scene.start + scene.len) as f64);
            let slack = 3.0 * snip.time.source_per_output * (2.0 * snip.sample_rate_hz / obw_ref);
            assert!(
                (s0 - t0).abs() <= slack && (s1 - t1).abs() <= slack,
                "[{AWARE_036}] {ctx}: extent {s0:.0}..{s1:.0} vs {t0}..{t1} (slack {slack:.0})"
            );
        }
    }
}

#[test]
fn tone_centroid_line_and_power() {
    let len = 200_000;
    let sig = tone(len, 0.0, FS);
    let bw = 2_000.0;
    for (i, snr) in [10.0, 20.0, 30.0].into_iter().enumerate() {
        let scene = Scene::new(&sig, FS, OFFSET, snr, bw, 20_000, 20_000, 200 + i as u64);
        let (snip, ps) = run(&scene.iq, FS, &scene.request(-237.5, bw), &Hints::default());
        let ctx = format!("tone at {snr} dB in {bw} Hz");
        let bin = snip.sample_rate_hz / ps.nfft as f64;
        let line = val(&ps.shape.carrier_line, "carrier line", &ctx);
        assert!(
            (line - 237.5).abs() <= 2.0,
            "[{AWARE_036}] {ctx}: line {line}"
        );
        if snr < 15.0 && ps.obw99_hz.reason() == Some(Reason::LowSnr) {
            eprintln!(
                "[{AWARE_036}] {ctx}: line {line:.3}; below the bandwidth floor {:?}",
                ps.obw99_hz
            );
            continue;
        }
        let cfo = val(&ps.cfo.centroid, "centroid", &ctx);
        let obw = val(&ps.obw99_hz, "obw", &ctx);
        let snr_lin = 10f64.powf(val(&ps.snr_box_db, "snr", &ctx) / 10.0);
        let p_est = snr_lin * val(&ps.noise_density, "n0", &ctx) * obw;
        eprintln!(
            "[{AWARE_036}] {ctx}: centroid {cfo:.2} line {line:.3} obw {obw:.0} (bin {bin:.1}) \
             power err {:.2} dB",
            db(p_est / scene.signal_power)
        );
        assert!(
            (cfo - 237.5).abs() <= 0.25 * bin,
            "[{AWARE_036}] {ctx}: centroid {cfo}"
        );
        assert!(
            (line - 237.5).abs() <= 2.0,
            "[{AWARE_036}] {ctx}: line {line}"
        );
        // Resolution-limited: Hann leakage holds ~1 % of a tone's power beyond ±4 bins, and
        // the 5-bin smoothing adds 4 more.
        assert!(
            obw <= 16.0 * bin,
            "[{AWARE_036}] {ctx}: tone OBW {obw} ({bin} Hz bins)"
        );
        assert!(
            db(p_est / scene.signal_power).abs() <= 1.0,
            "[{AWARE_036}] {ctx}: power"
        );
    }
}

#[test]
fn noise_only_snippets_have_no_snr_and_no_cfo() {
    let mut false_cfo = 0;
    for seed in 0..40u64 {
        let mut rng = Rng::new(1000 + seed);
        let iq = complex_noise(&mut rng, 60_000, 1e-4);
        let req = hk_estimate::SnippetRequest {
            start_index: 10_000,
            end_index: 50_000,
            center_offset_hz: -200_000.0 + 10_000.0 * seed as f64,
            bandwidth_hz: 20_000.0,
        };
        for family in [
            FamilyHint::Unknown,
            FamilyHint::Dsb,
            FamilyHint::Fsk { levels: 2 },
        ] {
            let hints = Hints {
                family,
                ..Default::default()
            };
            let (_, ps) = run(&iq, FS, &req, &hints);
            if ps.cfo_hz.is_measured() || ps.snr_box_db.is_measured() {
                false_cfo += 1;
                eprintln!(
                    "[{AWARE_036}] noise seed {seed} {family:?}: {:?} {:?}",
                    ps.cfo_hz, ps.snr_box_db
                );
            }
            assert!(
                ps.noise_density.is_measured(),
                "[{AWARE_036}] noise density"
            );
            assert!(ps.rf_center_hz.value().is_none() || ps.cfo_hz.is_measured());
        }
    }
    assert_eq!(
        false_cfo, 0,
        "[{AWARE_036}] noise-only snippets produced CFO/SNR values"
    );
}

/// S5 pitfall 3: the median of χ²₂ PSD bins is ln 2 (−1.6 dB) of the mean; an N0 from it makes
/// pure noise look like signal. The estimator's pad N0 is the mean.
#[test]
fn noise_density_is_mean_not_median() {
    let mut rng = Rng::new(77);
    let iq = complex_noise(&mut rng, 200_000, 1e-4);
    let req = hk_estimate::SnippetRequest {
        start_index: 50_000,
        end_index: 150_000,
        center_offset_hz: 50_000.0,
        bandwidth_hz: 40_000.0,
    };
    let (snip, ps) = run(&iq, FS, &req, &Hints::default());
    let n0_true = 1e-4 / FS;
    let n0 = ps.noise_density.value().unwrap();
    assert_eq!(ps.noise_density.method(), Method::NoisePad);
    assert!(
        db(n0 / n0_true).abs() < 0.3,
        "[{AWARE_036}] pad N0 {:.2} dB off",
        db(n0 / n0_true)
    );
    assert!(
        ps.snr_box_db.reason() == Some(Reason::LowSnr),
        "{:?}",
        ps.snr_box_db
    );

    // Median-based density of the same snippet's pre-pad: one periodogram (χ²₂ bins, as in a
    // single spectrum frame), flat passband bins only.
    let pad = &snip.samples[..snip.box_range.start];
    let nfft = 1usize << (usize::BITS - 1 - pad.len().leading_zeros());
    let config = WelchConfig {
        overlap: 0,
        ..WelchConfig::new(nfft)
    };
    let s = welch(&pad[..nfft], snip.sample_rate_hz, 0.0, &config).unwrap();
    let fp = 0.98 * snip.passband_hz;
    let mut bins: Vec<f64> = (0..s.bins())
        .filter(|&k| s.bin_offset_hz(k).abs() <= fp)
        .map(|k| f64::from(s.psd[k]))
        .collect();
    bins.sort_by(f64::total_cmp);
    let median = bins[bins.len() / 2];
    let mean = bins.iter().sum::<f64>() / bins.len() as f64;
    eprintln!(
        "[{AWARE_036}] N0: estimator {:+.2} dB, pad mean {:+.2} dB, pad median {:+.2} dB (vs truth)",
        db(n0 / n0_true),
        db(mean / n0_true),
        db(median / n0_true)
    );
    assert!(
        db(mean / median) > 1.2,
        "the median of χ²₂ bins reads ~1.6 dB low"
    );
    let hints = Hints {
        noise: Some(NoiseReference {
            density: median,
            sigma_db: 0.1,
        }),
        ..Default::default()
    };
    let mut est = hk_estimate::ParamEstimator::new(Default::default());
    let with_median = est.estimate(&snip, &hints);
    eprintln!(
        "[{AWARE_036}] noise-only with median N0: snr {:?}",
        with_median.snr_box_db
    );
    // With the median the presence test is fooled (the whole band reads as signal); only the
    // fills-band guard stops a value. With the mean, presence already says low_snr.
    assert_ne!(
        with_median.snr_box_db.reason(),
        Some(Reason::LowSnr),
        "[{AWARE_036}] regression guard: a median N0 should look like signal on pure noise"
    );
}

#[test]
fn normalised_snippet_is_centred_resampled_and_unit_power() {
    let mut rng = Rng::new(5);
    let rate = 9_600.0;
    let sig = cpfsk(&random_bits(&mut rng, 400), FS, rate, 4_800.0);
    let obw_ref = obw99_reference(&sig, FS);
    let scene = Scene::new(&sig, FS, OFFSET, 25.0, obw_ref, 8_000, 8_000, 9);
    let hints = Hints {
        family: FamilyHint::Fsk { levels: 2 },
        ..Default::default()
    };
    let (snip, ps) = run(&scene.iq, FS, &scene.request(BOX_ERROR, obw_ref), &hints);
    let norm = normalise(&snip, &ps, &NormaliseConfig::default()).expect("normalise");
    let obw = ps.obw99_hz.value().unwrap();
    let p = power(&norm.samples);
    eprintln!(
        "[{SIGNAL_062}] normalised: {} samples at {:.0} Hz ({:.2} per OBW), power {p:.4}, \
         flags {:?}, first source index {:.1} (burst {}), rf {:.1}",
        norm.samples.len(),
        norm.sample_rate_hz,
        norm.samples_per_obw,
        norm.flags,
        norm.time.source_index,
        scene.start,
        norm.rf_center_hz - CENTER_HZ - OFFSET
    );
    assert!((p - 1.0).abs() < 1e-3, "unit power");
    assert!((norm.sample_rate_hz / obw - norm.samples_per_obw).abs() < 1e-9);
    assert!(norm.samples_per_obw >= 1.2 * 1.5 - 1e-9);
    assert!((norm.rf_center_hz - CENTER_HZ - OFFSET).abs() <= 0.01 * rate);
    // Centred: the IF clusters of the normalised samples are symmetric about 0 Hz.
    let fs = norm.sample_rate_hz;
    let fi: Vec<f64> = norm
        .samples
        .windows(2)
        .map(|w| f64::from((w[1] * w[0].conj()).arg()) * fs / std::f64::consts::TAU)
        .collect();
    let pos: Vec<f64> = fi.iter().copied().filter(|v| *v > 0.0).collect();
    let neg: Vec<f64> = fi.iter().copied().filter(|v| *v < 0.0).collect();
    let mid = 0.5
        * (pos.iter().sum::<f64>() / pos.len() as f64 + neg.iter().sum::<f64>() / neg.len() as f64);
    assert!(
        mid.abs() < 0.02 * rate,
        "[{SIGNAL_062}] normalised mid-frequency {mid:.1} Hz"
    );
    // Time map: output 0 sits at the extent start (± one output sample).
    let ext = ps.extent.unwrap();
    assert!(
        (norm.time.source_index - ext.source_start).abs() <= norm.time.source_per_output + 1e-6,
        "time map {} vs extent {}",
        norm.time.source_index,
        ext.source_start
    );
    let mp = ps.estimated_params();
    assert_eq!(mp.cfo_hz, ps.cfo_hz.value());
    assert_eq!(mp.bandwidth_hz, Some(obw));
}
