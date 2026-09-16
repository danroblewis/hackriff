//! T-240 diagnosis: what the OTA fixtures measure versus the synthetic dev grid, per dimension.
//!
//! Reported, never asserted. T-238 named four dimensions (`sigma_aa`, `if_bimodality`,
//! `gamma_max`, `sigma_ap`) as "pinned at the z = 6 clamp". That reading came from the `worst`
//! column, which names the single largest |z| **per family** — including families that are
//! supposed to be rejected. This test decomposes the whole distance instead: for the class that
//! actually fits best, every dimension's z, z² and share of d², next to the dev grid's own mean
//! and spread for the same feature at the same SNR. That is the difference between "one wild
//! feature" and "the class genuinely sits somewhere else".

use std::path::{Path, PathBuf};

use hk_classify::density::{ClassDensity, DensityModel};
use hk_classify::features::{FeatureInput, Features, features};
use hk_classify::symbols::SymbolEstimator;
use hk_classify::synth::{Class, SynthConfig, generate};
use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::{InputInfo, WelchConfig, WindowKind, welch};
use hk_estimate::{Hints, ParamEstimator, SnippetExtractor, SnippetRequest};
use hk_model::{SampleTime, Timestamp};
use num_complex::{Complex, Complex32};

const FM: &str = "fm_100p8M_2p4M_l32g30a1_t1p5_5s";
const ISM: &str = "ism_915M_10M_l24g30a1_t42p3_1p2s";

fn fixture(name: &str) -> Option<(PathBuf, PathBuf)> {
    let rel = Path::new("fixtures/hackrf/2026-09-13").join(format!("{name}.sigmf-meta"));
    let mut dir = Some(hk_e2e::paths::repo_root());
    while let Some(d) = dir {
        let meta = d.join(&rel);
        let data = meta.with_extension("sigmf-data");
        if meta.is_file() && std::fs::metadata(&data).is_ok_and(|m| m.len() > 4096) {
            return Some((meta, data));
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    if std::env::var("HK_REQUIRE_FIXTURES").is_ok_and(|v| v == "1") {
        panic!("{name}: fixture data not fetched and HK_REQUIRE_FIXTURES=1");
    }
    eprintln!("SKIP {name}: fixture data is not fetched (git lfs pull)");
    None
}

fn provenance(meta: &Path) -> ProvenanceHandle {
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(meta).unwrap()).unwrap();
    ProvenanceHandle::new(
        serde_json::from_value(v["global"]["hackriff:provenance"].clone()).unwrap(),
    )
}

fn read_ci8(path: &Path, max: usize) -> Vec<Complex<i8>> {
    std::fs::read(path)
        .expect("read .sigmf-data")
        .chunks_exact(2)
        .take(max)
        .map(|c| Complex::new(c[0] as i8, c[1] as i8))
        .collect()
}

fn to_cf32(iq: &[Complex<i8>]) -> Vec<Complex32> {
    iq.iter()
        .map(|s| Complex32::new(f32::from(s.re) / 127.0, f32::from(s.im) / 127.0))
        .collect()
}

fn info(prov: &ProvenanceHandle) -> InputInfo<'_> {
    InputInfo {
        time: SampleTime {
            sample_index: 0,
            host_time: Timestamp::from_unix_nanos(1_789_000_000_000_000_000),
        },
        discontinuity: Discontinuity::STREAM_START,
        dropped_before: 0,
        provenance: prov,
    }
}

/// The measured feature vector for one detection box, through the same C13/C14 chain the
/// pipeline uses, plus the measured SNR.
fn measure(
    iq: &[Complex<i8>],
    prov: &ProvenanceHandle,
    request: &SnippetRequest,
) -> (Features, Option<f64>) {
    let mut extractor = SnippetExtractor::new(Default::default());
    let snip = extractor.extract(info(prov), iq, request).expect("snippet");
    let params = ParamEstimator::new(Default::default()).estimate(&snip, &Hints::default());
    let normalised =
        hk_estimate::normalise::normalise(&snip, &params, &Default::default()).expect("normalise");
    let window = SymbolEstimator::new().window_from_snippet(&snip, &params);
    let symbols = window.as_ref().map(|w| w.params.clone());
    let snr = params
        .snr_extent_db
        .value()
        .or_else(|| params.snr_box_db.value());
    let f = features(&FeatureInput {
        samples: &normalised.samples,
        sample_rate_hz: normalised.sample_rate_hz,
        obw_hz: params.obw99_hz.value(),
        snr_db: snr,
        symbols: symbols.as_ref(),
    });
    (f, snr)
}

/// Mean and standard deviation of each feature over `n` **dev** seeds of `class` at `snr_db`:
/// what the grid actually produces, in the feature's own units.
fn dev_stats(class: Class, snr_db: f64, n: u64) -> Vec<(String, f64, f64, usize)> {
    let mut c14 = SymbolEstimator::new();
    let mut rows: Vec<Features> = Vec::new();
    for seed in 0..n {
        let s = generate(class, &SynthConfig::new(snr_db, seed));
        let symbols = c14.from_samples(
            &s.symbol_samples,
            s.symbol_sample_rate_hz,
            Some(s.obw_hz),
            Some(snr_db),
        );
        rows.push(features(&FeatureInput {
            samples: &s.samples,
            sample_rate_hz: s.sample_rate_hz,
            obw_hz: Some(s.obw_hz),
            snr_db: Some(snr_db),
            symbols: symbols.as_ref(),
        }));
    }
    hk_classify::features::FEATURE_NAMES
        .iter()
        .map(|name| {
            let v: Vec<f64> = rows.iter().filter_map(|r| r.get(name)).collect();
            if v.len() < 2 {
                return ((*name).to_owned(), f64::NAN, f64::NAN, v.len());
            }
            let mean = v.iter().sum::<f64>() / v.len() as f64;
            let sd =
                (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (v.len() - 1) as f64).sqrt();
            ((*name).to_owned(), mean, sd, v.len())
        })
        .collect()
}

/// Decomposes the distance from `f` to `c`, sorted by contribution to d².
fn decompose(c: &ClassDensity, f: &Features, dev: &[(String, f64, f64, usize)]) {
    let mut rows: Vec<(f64, String)> = Vec::new();
    let mut d2 = 0.0;
    let mut k = 0usize;
    for dim in &c.dims {
        let Some(x) = f.get(&dim.feature) else {
            continue;
        };
        let raw = (x - dim.mean) / dim.sigma;
        let z = raw.clamp(-6.0, 6.0);
        d2 += z * z;
        k += 1;
        let (dev_mean, dev_sd) = dev
            .iter()
            .find(|(n, ..)| *n == dim.feature)
            .map(|(_, m, s, _)| (*m, *s))
            .unwrap_or((f64::NAN, f64::NAN));
        rows.push((
            z * z,
            format!(
                "{:<16} real {:>12.4}  fit mean {:>12.4} sigma {:>10.4}  z {:>+8.2}{}  \
                 dev@snr mean {:>12.4} sd {:>10.4}",
                dim.feature,
                x,
                dim.mean,
                dim.sigma,
                z,
                if raw.abs() > 6.0 {
                    " CLAMPED"
                } else {
                    "        "
                },
                dev_mean,
                dev_sd
            ),
        ));
    }
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    eprintln!(
        "  class {:<11} k {:>2}  d2 {:>8.1}  m {:>7.2}  m_p95 {:>6.2}  (m must approach m_p95 to be plausible)",
        c.class,
        k,
        d2,
        d2 / k as f64,
        c.m_p95
    );
    for (z2, line) in &rows {
        eprintln!("    {:>5.1}% z2 {:>7.1}  {line}", 100.0 * z2 / d2, z2);
    }
}

fn report(tag: &str, f: &Features, snr: Option<f64>, classes: &[(&str, Class)]) {
    let model = DensityModel::builtin();
    eprintln!("\n================ {tag}: measured SNR {snr:?} dB ================");
    eprintln!("  reasons: {:?}", f.reasons);
    for family in [
        "analog",
        "ook-ask",
        "fsk",
        "psk-qam",
        "ofdm",
        "css",
        "pulsed",
        "noise-like",
    ] {
        match model.score(family, f) {
            Some(s) => eprintln!(
                "  [family] {family:<11} best {:<11} m {:>8.2} plausibility {:.4}",
                s.class, s.m, s.plausibility
            ),
            None => eprintln!("  [family] {family:<11} not scored"),
        }
    }
    let snr = snr.unwrap_or(20.0);
    for (class, synth) in classes {
        let Some(c) = model.class(class) else {
            continue;
        };
        eprintln!("\n  --- {tag} vs {class} (dev grid at {snr:.1} dB, 40 seeds) ---");
        decompose(c, f, &dev_stats(*synth, snr, 40));
    }
}

/// One normalised snippet as the classifier receives it: samples, their sample rate, the measured
/// OBW99 and the measured in-band SNR.
type NormalisedSnippet = (Vec<Complex32>, f64, Option<f64>, Option<f64>);

/// The FM fixture's normalised snippet, through the same blind detector the OTA test uses.
fn fm_normalised() -> Option<NormalisedSnippet> {
    let (meta, data) = fixture(FM)?;
    let prov = provenance(&meta);
    let fs = prov.get().tune.sample_rate_hz;
    let iq = read_ci8(&data, (fs as usize).min(2_400_000));
    let cf = to_cf32(&iq);
    let cfg = WelchConfig {
        fft_len: 4096,
        overlap: 2048,
        window: WindowKind::Hann,
        holds: false,
        spectral_kurtosis: false,
    };
    let s = welch(&cf, fs, 0.0, &cfg).ok()?;
    let psd: Vec<f64> = s.psd.iter().map(|v| f64::from(*v)).collect();
    let peak = psd
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.total_cmp(b))
        .map_or(psd.len() / 2, |(i, _)| i);
    let floor = psd[peak] / 100.0;
    let (mut lo, mut hi) = (peak, peak);
    while lo > 0 && psd[lo - 1] >= floor {
        lo -= 1;
    }
    while hi + 1 < psd.len() && psd[hi + 1] >= floor {
        hi += 1;
    }
    let request = SnippetRequest {
        start_index: 0,
        end_index: iq.len() as u64,
        center_offset_hz: 0.5 * (s.bin_offset_hz(lo) + s.bin_offset_hz(hi)),
        bandwidth_hz: ((hi - lo + 1) as f64 * s.bin_width_hz()).max(2.0 * s.bin_width_hz()),
    };
    let mut extractor = SnippetExtractor::new(Default::default());
    let snip = extractor.extract(info(&prov), &iq, &request).ok()?;
    let params = ParamEstimator::new(Default::default()).estimate(&snip, &Hints::default());
    let n = hk_estimate::normalise::normalise(&snip, &params, &Default::default()).ok()?;
    let snr = params
        .snr_extent_db
        .value()
        .or_else(|| params.snr_box_db.value());
    Some((n.samples, n.sample_rate_hz, params.obw99_hz.value(), snr))
}

/// Are `sigma_ap` and `sigma_dp` a **length** mismatch between fitting and classifying?
///
/// Both measure the spread of the unwrapped phase residual, and an angle modulator *integrates*
/// its baseband — so the residual accumulates with observation length instead of being a
/// per-sample quantity (`synth.rs` calls `sigma_ap` "rad/sample", which is true of `sigma_af` and
/// not of this one). The dev grid generates 16 384 samples; the production snippet for a 5 s
/// broadcast capture is tens of times longer. If that is the cause, the real capture lands on the
/// synthetic curve once the two lengths match, and no impairment needs widening at all.
#[test]
fn phase_residual_features_scale_with_snippet_length() {
    eprintln!("\n================ sigma_ap / sigma_dp vs snippet length ================");
    eprintln!("  -- synthetic wfm at 15.4 dB (dev seeds, 8 per length) --");
    for n in [16_384usize, 32_768, 65_536, 131_072, 262_144] {
        let (mut ap, mut dp, mut len) = (0.0, 0.0, 0usize);
        let mut used = 0;
        for seed in 0..8u64 {
            let mut cfg = SynthConfig::new(15.4, seed);
            cfg.samples = n;
            let s = generate(Class::Wfm, &cfg);
            let f = features(&FeatureInput {
                samples: &s.samples,
                sample_rate_hz: s.sample_rate_hz,
                obw_hz: Some(s.obw_hz),
                snr_db: Some(15.4),
                symbols: None,
            });
            if let (Some(a), Some(d)) = (f.get("sigma_ap"), f.get("sigma_dp")) {
                ap += a;
                dp += d;
                len = s.samples.len();
                used += 1;
            }
        }
        if used > 0 {
            eprintln!(
                "    generated {n:>7} -> snippet {len:>7} samples  sigma_ap {:>9.1}  sigma_dp {:>9.1}",
                ap / f64::from(used),
                dp / f64::from(used)
            );
        }
    }
    let Some((samples, rate, obw, snr)) = fm_normalised() else {
        return;
    };
    eprintln!(
        "  -- real FM capture: normalised snippet {} samples at {:.1} kHz, OBW99 {:?} kHz, SNR {:?} dB --",
        samples.len(),
        rate / 1e3,
        obw.map(|o| (o / 1e2).round() / 10.0),
        snr.map(|s| (s * 10.0).round() / 10.0),
    );
    for take in [16_384usize, 32_768, 65_536, 131_072, 262_144, samples.len()] {
        if take > samples.len() {
            continue;
        }
        let f = features(&FeatureInput {
            samples: &samples[..take],
            sample_rate_hz: rate,
            obw_hz: obw,
            snr_db: snr,
            symbols: None,
        });
        eprintln!(
            "    prefix {take:>7} samples  sigma_ap {:>9.1}  sigma_dp {:>9.1}  if_std_norm {:>7.3}  sk_mean {:>6.2}",
            f.get("sigma_ap").unwrap_or(f64::NAN),
            f.get("sigma_dp").unwrap_or(f64::NAN),
            f.get("if_std_norm").unwrap_or(f64::NAN),
            f.get("sk_mean").unwrap_or(f64::NAN),
        );
    }
}

/// The 915 MHz capture's real FHSS hops sit megahertz away from the tuned centre; the fixture's
/// own truth list annotates the centre itself as a **DC-offset artefact**. The blind detector the
/// OTA test uses maximises spectral peak/median contrast, which that artefact wins outright. This
/// repeats the search with the DC region excluded, so what gets classified is an emission.
#[test]
fn ism_bursts_found_away_from_dc() {
    let Some((meta, data)) = fixture(ISM) else {
        return;
    };
    let prov = provenance(&meta);
    let fs = prov.get().tune.sample_rate_hz;
    let iq = read_ci8(&data, 12_000_000);
    let cf = to_cf32(&iq);
    let window = (0.002 * fs) as usize;
    let cfg = WelchConfig {
        fft_len: 1024,
        overlap: 512,
        window: WindowKind::Hann,
        holds: false,
        spectral_kurtosis: false,
    };
    const DC_GUARD_HZ: f64 = 60e3;
    let mut cands: Vec<(f64, usize, f64, f64)> = Vec::new();
    let mut start = 0;
    while start + window <= cf.len() {
        if let Ok(s) = welch(&cf[start..start + window], fs, 0.0, &cfg) {
            let psd: Vec<f64> = s.psd.iter().map(|v| f64::from(*v)).collect();
            let mut sorted = psd.clone();
            sorted.sort_by(f64::total_cmp);
            let median = sorted[sorted.len() / 2].max(1e-30);
            let mut peak = None;
            let mut best = 0.0;
            for (i, p) in psd.iter().enumerate() {
                if s.bin_offset_hz(i).abs() < DC_GUARD_HZ {
                    continue;
                }
                if *p > best {
                    best = *p;
                    peak = Some(i);
                }
            }
            if let Some(pk) = peak {
                let floor = psd[pk] / 100.0;
                let (mut lo, mut hi) = (pk, pk);
                while lo > 0 && psd[lo - 1] >= floor && s.bin_offset_hz(lo - 1).abs() >= DC_GUARD_HZ
                {
                    lo -= 1;
                }
                while hi + 1 < psd.len()
                    && psd[hi + 1] >= floor
                    && s.bin_offset_hz(hi + 1).abs() >= DC_GUARD_HZ
                {
                    hi += 1;
                }
                let bw = (hi - lo + 1) as f64 * s.bin_width_hz();
                if bw >= 50e3 {
                    cands.push((
                        10.0 * (psd[pk] / median).log10(),
                        start,
                        0.5 * (s.bin_offset_hz(lo) + s.bin_offset_hz(hi)),
                        bw,
                    ));
                }
            }
        }
        start += window;
    }
    cands.sort_by(|a, b| b.0.total_cmp(&a.0));
    eprintln!(
        "\n================ 915 MHz: strongest bursts with DC excluded (+-{:.0} kHz) ================",
        DC_GUARD_HZ / 1e3
    );
    for (contrast, at, offset, bw) in cands.iter().take(6) {
        eprintln!(
            "\n  burst at {:>7.1} ms, RF {:.4} MHz (offset {:+8.0} kHz), {:>6.1} kHz wide, peak/median {contrast:.1} dB",
            *at as f64 / fs * 1e3,
            (915e6 + offset) / 1e6,
            offset / 1e3,
            bw / 1e3
        );
        survey_box(
            &iq,
            &prov,
            &SnippetRequest {
                start_index: *at as u64,
                end_index: (*at + window) as u64,
                center_offset_hz: *offset,
                bandwidth_hz: *bw,
            },
            "dc-notched",
        );
    }
}

/// Measures one box and classifies it, reporting the discriminating features in physical units.
fn survey_box(iq: &[Complex<i8>], prov: &ProvenanceHandle, request: &SnippetRequest, tag: &str) {
    let mut extractor = SnippetExtractor::new(Default::default());
    let Ok(snip) = extractor.extract(info(prov), iq, request) else {
        eprintln!("    {tag}: snippet extraction failed");
        return;
    };
    let params = ParamEstimator::new(Default::default()).estimate(&snip, &Hints::default());
    let Ok(normalised) = hk_estimate::normalise::normalise(&snip, &params, &Default::default())
    else {
        eprintln!("    {tag}: normalise failed");
        return;
    };
    let w = SymbolEstimator::new().window_from_snippet(&snip, &params);
    let symbols = w.as_ref().map(|x| x.params.clone());
    let snr = params
        .snr_extent_db
        .value()
        .or_else(|| params.snr_box_db.value());
    let f = features(&FeatureInput {
        samples: &normalised.samples,
        sample_rate_hz: normalised.sample_rate_hz,
        obw_hz: params.obw99_hz.value(),
        snr_db: snr,
        symbols: symbols.as_ref(),
    });
    let mut req = hk_classify::ClassifyRequest::new(
        &normalised.samples,
        normalised.sample_rate_hz,
        Timestamp::from_unix_nanos(1_789_000_000_000_000_000),
    );
    req.obw_hz = params.obw99_hz.value();
    req.snr_db = snr;
    req.symbols = symbols.as_ref();
    req.symbol_samples = w.as_ref().map(|x| x.samples.as_slice());
    req.symbol_sample_rate_hz = w.as_ref().map(|x| x.sample_rate_hz);
    let c = hk_classify::Classifier::new().classify(&req);
    let model = DensityModel::builtin();
    let g = |n: &str| f.get(n).unwrap_or(f64::NAN);
    eprintln!(
        "    {tag}: snr {:>5.1} obw {:>7.1}k | flat {:>6.3} symm {:>+6.3} line {:>6.1}dB bimod {:>5.3} \
         modality {:>3.0} sigma_dp {:>8.3} sigma_ap {:>8.3} env_cv {:>6.3} if_std_norm {:>6.3} sigma_aa {:>6.3} gamma_max {:>7.1}",
        snr.unwrap_or(f64::NAN),
        params.obw99_hz.value().unwrap_or(f64::NAN) / 1e3,
        g("flatness"),
        g("symmetry"),
        g("carrier_line_db"),
        g("if_bimodality"),
        g("if_modality"),
        g("sigma_dp"),
        g("sigma_ap"),
        g("env_cv"),
        g("if_std_norm"),
        g("sigma_aa"),
        g("gamma_max"),
    );
    eprintln!(
        "        -> {} ({:.2}) open-set {:.2} | fsk m {:?} | C14 {:?} rate {:?} | reasons {:?}",
        c.family,
        c.confidence,
        c.open_set_score,
        model
            .score("fsk", &f)
            .map(|s| (s.class, (s.m * 10.0).round() / 10.0)),
        symbols.as_ref().map(|s| format!("{:?}", s.reasons)),
        symbols.as_ref().and_then(|s| s.symbol_rate_bd.value()),
        c.reasons,
    );
}

/// Is the blind-detected "strongest burst" actually one of the capture's 2-FSK sensor bursts?
/// Surveys the capture by **energy** (what a burst detector keys on) rather than by peak/median
/// spectral contrast (which a bare carrier wins), and classifies each candidate two ways: the
/// whole occupied span of the window, and a −20 dB contour grown around the strongest bin, which
/// is what isolates one emission rather than everything present in that 2 ms.
#[test]
fn ism_burst_survey() {
    let Some((meta, data)) = fixture(ISM) else {
        return;
    };
    let prov = provenance(&meta);
    let fs = prov.get().tune.sample_rate_hz;
    let iq = read_ci8(&data, 12_000_000);
    let cf = to_cf32(&iq);
    let window = (0.002 * fs) as usize;
    let cfg = WelchConfig {
        fft_len: 1024,
        overlap: 512,
        window: WindowKind::Hann,
        holds: false,
        spectral_kurtosis: false,
    };
    let mut wins: Vec<(f64, usize)> = Vec::new();
    let mut start = 0;
    while start + window <= cf.len() {
        let p = cf[start..start + window]
            .iter()
            .map(|s| f64::from(s.norm_sqr()))
            .sum::<f64>()
            / window as f64;
        wins.push((p, start));
        start += window;
    }
    let mut by_power = wins.clone();
    by_power.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut sorted: Vec<f64> = wins.iter().map(|(p, _)| *p).collect();
    sorted.sort_by(f64::total_cmp);
    let median = sorted[sorted.len() / 2];
    eprintln!(
        "\n================ 915 MHz burst survey: {} windows of 2 ms, median power {:.3e} ================",
        wins.len(),
        median
    );
    for (p, at) in by_power.iter().take(8) {
        let Ok(s) = welch(&cf[*at..*at + window], fs, 0.0, &cfg) else {
            continue;
        };
        let psd: Vec<f64> = s.psd.iter().map(|v| f64::from(*v)).collect();
        let (lo, hi) = hk_classify::features::occupied_band(&psd);
        let peak = psd
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.total_cmp(b))
            .map_or(psd.len() / 2, |(i, _)| i);
        let floor = psd[peak] / 100.0;
        let (mut clo, mut chi) = (peak, peak);
        while clo > 0 && psd[clo - 1] >= floor {
            clo -= 1;
        }
        while chi + 1 < psd.len() && psd[chi + 1] >= floor {
            chi += 1;
        }
        eprintln!(
            "\n  window at {:.1} ms, power {:.1} dB over median:",
            *at as f64 / fs * 1e3,
            10.0 * (p / median).log10()
        );
        survey_box(
            &iq,
            &prov,
            &SnippetRequest {
                start_index: *at as u64,
                end_index: (*at + window) as u64,
                center_offset_hz: 0.5 * (s.bin_offset_hz(lo) + s.bin_offset_hz(hi)),
                bandwidth_hz: (hi - lo + 1) as f64 * s.bin_width_hz(),
            },
            &format!(
                "occupied-span {:>6.1}k",
                (hi - lo + 1) as f64 * s.bin_width_hz() / 1e3
            ),
        );
        survey_box(
            &iq,
            &prov,
            &SnippetRequest {
                start_index: *at as u64,
                end_index: (*at + window) as u64,
                center_offset_hz: 0.5 * (s.bin_offset_hz(clo) + s.bin_offset_hz(chi)),
                bandwidth_hz: ((chi - clo + 1) as f64 * s.bin_width_hz())
                    .max(2.0 * s.bin_width_hz()),
            },
            &format!(
                "-20dB contour {:>6.1}k",
                (chi - clo + 1) as f64 * s.bin_width_hz() / 1e3
            ),
        );
    }
}

#[test]
fn fm_capture_versus_the_synthetic_grid() {
    let Some((meta, data)) = fixture(FM) else {
        return;
    };
    let prov = provenance(&meta);
    let fs = prov.get().tune.sample_rate_hz;
    let iq = read_ci8(&data, (fs as usize).min(2_400_000));
    let cf = to_cf32(&iq);
    let cfg = WelchConfig {
        fft_len: 4096,
        overlap: 2048,
        window: WindowKind::Hann,
        holds: false,
        spectral_kurtosis: false,
    };
    let s = welch(&cf, fs, 0.0, &cfg).expect("welch");
    let psd: Vec<f64> = s.psd.iter().map(|v| f64::from(*v)).collect();
    let peak = psd
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.total_cmp(b))
        .map_or(psd.len() / 2, |(i, _)| i);
    let floor = psd[peak] / 100.0;
    let (mut lo, mut hi) = (peak, peak);
    while lo > 0 && psd[lo - 1] >= floor {
        lo -= 1;
    }
    while hi + 1 < psd.len() && psd[hi + 1] >= floor {
        hi += 1;
    }
    let bw = ((hi - lo + 1) as f64 * s.bin_width_hz()).max(2.0 * s.bin_width_hz());
    let request = SnippetRequest {
        start_index: 0,
        end_index: iq.len() as u64,
        center_offset_hz: 0.5 * (s.bin_offset_hz(lo) + s.bin_offset_hz(hi)),
        bandwidth_hz: bw,
    };
    let (f, snr) = measure(&iq, &prov, &request);
    eprintln!("[T-240] FM detection box {:.1} kHz wide", bw / 1e3);
    report(
        "FM broadcast",
        &f,
        snr,
        &[
            ("wfm", Class::Wfm),
            ("nbfm", Class::Nbfm),
            ("am", Class::Am),
        ],
    );
}

#[test]
fn ism_burst_versus_the_synthetic_grid() {
    let Some((meta, data)) = fixture(ISM) else {
        return;
    };
    let prov = provenance(&meta);
    let fs = prov.get().tune.sample_rate_hz;
    let iq = read_ci8(&data, 12_000_000);
    let cf = to_cf32(&iq);
    let window = (0.002 * fs) as usize;
    let mut best = (f64::NEG_INFINITY, 0usize, 0.0, 0.0);
    let mut start = 0;
    while start + window <= cf.len() {
        let cfg = WelchConfig {
            fft_len: 1024,
            overlap: 512,
            window: WindowKind::Hann,
            holds: false,
            spectral_kurtosis: false,
        };
        if let Ok(s) = welch(&cf[start..start + window], fs, 0.0, &cfg) {
            let psd: Vec<f64> = s.psd.iter().map(|v| f64::from(*v)).collect();
            let mut sorted = psd.clone();
            sorted.sort_by(f64::total_cmp);
            let contrast = 10.0
                * (psd.iter().copied().fold(0.0_f64, f64::max)
                    / sorted[sorted.len() / 2].max(1e-30))
                .log10();
            if contrast > best.0 {
                let (lo, hi) = hk_classify::features::occupied_band(&psd);
                best = (
                    contrast,
                    start,
                    0.5 * (s.bin_offset_hz(lo) + s.bin_offset_hz(hi)),
                    (hi - lo + 1) as f64 * s.bin_width_hz(),
                );
            }
        }
        start += window;
    }
    let (contrast, at, offset, bandwidth) = best;
    eprintln!(
        "[T-240] 915 burst at {:.1} ms, {:+.1} kHz, {:.1} kHz wide, peak/median {contrast:.1} dB",
        at as f64 / fs * 1e3,
        offset / 1e3,
        bandwidth / 1e3
    );
    let request = SnippetRequest {
        start_index: at as u64,
        end_index: (at + window) as u64,
        center_offset_hz: offset,
        bandwidth_hz: bandwidth,
    };
    let (f, snr) = measure(&iq, &prov, &request);
    report(
        "915 MHz burst",
        &f,
        snr,
        &[
            ("2fsk", Class::Fsk2),
            ("gfsk", Class::Gfsk),
            ("msk", Class::Msk),
            ("am", Class::Am),
        ],
    );
}
