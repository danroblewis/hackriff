//! T-248 diagnostic, part 2: reported, never asserted.
//!
//! This module is what chose the two corrections T-248 shipped, and it keeps measuring them so a
//! later change that quietly undoes either shows up here.
//!
//! 1. **Which statistic of the z-vector is not diluted?** `m = d²/k` averages over ~27 dimensions,
//!    so the one to three dimensions that actually discriminate an unlisted family member vanish
//!    into it. The candidates below are measured against the same dev population the densities are
//!    fitted on. The **largest** |z| fails: genuine members routinely have one wild dimension,
//!    because several `features@2` dimensions are heavy-tailed, and the dev p95 of max |z| sits
//!    *above* what VSB-AM (3.47 vs `am` 3.80) and 16-APSK (2.76 vs `qam16` 3.56) reach — it would
//!    reject real signals first. The **second**-largest is the order statistic that survives that,
//!    and is what [`hk_classify::density::ClassDensity::z2_p99`] now calibrates.
//!
//! 2. **Is `symmetry` measuring what its name says?** It was computed about `mid = (lo+hi)/2`, the
//!    midpoint of the **occupied band itself** — and asymmetry about a band's own centre is ~0 by
//!    construction, so the feature was blind to the one property that distinguishes VSB-AM from AM
//!    and SSB from DSB. Measured that way `ssb` read +0.046 ± 0.486 and VSB-AM −0.009 ± 0.987:
//!    zero, with noise-level scatter. It is now measured about the strongest line in the band, and
//!    the raw per-class values below are what that is worth.

use hk_classify::density::{DensityModel, Z_CLAMP};
use hk_classify::features::{FeatureInput, features};
use hk_classify::symbols::SymbolEstimator;
use hk_classify::synth::{ACCEPTANCE_SEED_BASE, Class, DEV_SEEDS, SynthConfig, generate};
use hk_classify::thresholds::thresholds_of;
use hk_classify::{Classifier, ClassifyRequest, Features};
use hk_model::Timestamp;
use hk_model::classify::UNKNOWN;

/// Every candidate open-set statistic of one feature vector against one class.
struct Stats {
    m: f64,
    zmax: f64,
    z2nd: f64,
    n25: f64,
    n30: f64,
    top3: f64,
}

fn stats(model: &DensityModel, class: &str, f: &Features) -> Option<Stats> {
    let c = model.class(class)?;
    let mut zs: Vec<f64> = Vec::new();
    for d in &c.dims {
        let Some(x) = f.get(&d.feature) else { continue };
        zs.push(((x - d.mean) / d.sigma).clamp(-Z_CLAMP, Z_CLAMP).abs());
    }
    if zs.is_empty() {
        return None;
    }
    let m = zs.iter().map(|z| z * z).sum::<f64>() / zs.len() as f64;
    let mut sorted = zs.clone();
    sorted.sort_by(|a, b| b.total_cmp(a));
    Some(Stats {
        m,
        zmax: sorted[0],
        z2nd: *sorted.get(1).unwrap_or(&0.0),
        n25: zs.iter().filter(|z| **z > 2.5).count() as f64,
        n30: zs.iter().filter(|z| **z > 3.0).count() as f64,
        top3: sorted.iter().take(3).map(|z| z * z).sum::<f64>() / 3.0,
    })
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    if v.is_empty() {
        return f64::NAN;
    }
    v[(((v.len() as f64) * p) as usize).min(v.len() - 1)]
}

fn row(tag: &str, s: &[Stats]) {
    let mut m: Vec<f64> = s.iter().map(|x| x.m).collect();
    let mut zmax: Vec<f64> = s.iter().map(|x| x.zmax).collect();
    let mut z2: Vec<f64> = s.iter().map(|x| x.z2nd).collect();
    let mut n25: Vec<f64> = s.iter().map(|x| x.n25).collect();
    let mut n30: Vec<f64> = s.iter().map(|x| x.n30).collect();
    let mut t3: Vec<f64> = s.iter().map(|x| x.top3).collect();
    eprintln!(
        "[T-248] {tag:<34} n {:>4} | m {:>6.3} | zmax {:>5.2} | z2nd {:>5.2} | n>2.5 {:>4.1} | n>3 {:>4.1} | top3 {:>6.2}",
        s.len(),
        pct(&mut m, 0.95),
        pct(&mut zmax, 0.95),
        pct(&mut z2, 0.95),
        pct(&mut n25, 0.95),
        pct(&mut n30, 0.95),
        pct(&mut t3, 0.95),
    );
}

#[test]
fn t248_alternative_open_set_statistics_and_the_symmetry_feature() {
    let model = DensityModel::builtin();
    let mut c14 = SymbolEstimator::new();
    let mut feat = |class: Class, snr: f64, seed: u64| {
        let s = generate(class, &SynthConfig::new(snr, seed));
        let symbols = c14.from_samples(
            &s.symbol_samples,
            s.symbol_sample_rate_hz,
            Some(s.obw_hz),
            Some(snr),
        );
        features(&FeatureInput {
            samples: &s.samples,
            sample_rate_hz: s.sample_rate_hz,
            obw_hz: Some(s.obw_hz),
            snr_db: Some(snr),
            symbols: symbols.as_ref(),
        })
    };

    // ---- 1. Candidate statistics: dev p95 (genuine members) vs the held-out claimants. ----
    eprintln!("\n[T-248] p95 of each candidate statistic. DEV = genuine members of the class.");
    for (dev_class, held, claimed_as) in [
        (Class::Am, Class::VsbAm, "am"),
        (Class::Qpsk, Class::Pi4Dqpsk, "qpsk"),
        (Class::Qam16, Class::Apsk16, "qam16"),
    ] {
        let family = dev_class.family().unwrap();
        let gate = thresholds_of(family)
            .and_then(|t| t.snr_gate_db)
            .unwrap_or(10.0);
        let mut dev = Vec::new();
        for step in [0.0, 5.0, 10.0, 15.0] {
            for seed in DEV_SEEDS.start..(DEV_SEEDS.start + 30) {
                let f = feat(dev_class, gate + step, seed);
                if let Some(s) = stats(model, claimed_as, &f) {
                    dev.push(s);
                }
            }
        }
        let mut ood = Vec::new();
        for offset in [0.0, 5.0, 10.0] {
            for seed in 0..30u64 {
                let f = feat(held, 20.0 + offset, ACCEPTANCE_SEED_BASE + 900_000 + seed);
                if let Some(s) = stats(model, claimed_as, &f) {
                    ood.push(s);
                }
            }
        }
        eprintln!("[T-248] --- against class `{claimed_as}` ---");
        row(&format!("DEV {}", dev_class.label()), &dev);
        row(&format!("OOD {}", held.label()), &ood);
    }

    // ---- 2. Is `symmetry` measuring sideband balance at all? ----
    eprintln!(
        "\n[T-248] mean raw `symmetry` per class (+1 = lower sideband only, -1 = upper only)"
    );
    let mut show = |c: Class| {
        let vals: Vec<f64> = (0..8u64)
            .filter_map(|s| feat(c, 25.0, ACCEPTANCE_SEED_BASE + 950_000 + s).get("symmetry"))
            .collect();
        if vals.is_empty() {
            return;
        }
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let sd = (vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64).sqrt();
        eprintln!("[T-248]   {:<24} symmetry {mean:+.4} +- {sd:.4}", c.label());
    };
    for c in [
        Class::Am,
        Class::Ssb,
        Class::Wfm,
        Class::Nbfm,
        Class::Cw,
        Class::VsbAm,
        Class::DsbSc,
    ] {
        show(c);
    }

    // ---- 3. Where does the claimed family's own plausibility sit? (fix 1's lever) ----
    eprintln!(
        "\n[T-248] claimed-family plausibility on the gate's own held-out seeds (fix 1's lever)"
    );
    let classifier = Classifier::new();
    let mut seed = ACCEPTANCE_SEED_BASE + 700_000;
    for class in Class::HELD_OUT {
        let mut pls: Vec<f64> = Vec::new();
        let mut reported: Vec<f64> = Vec::new();
        for offset in [0.0_f64, 5.0, 10.0] {
            for _ in 0..12 {
                seed += 1;
                let snr = 20.0 + offset;
                let s = generate(*class, &SynthConfig::new(snr, seed));
                let symbols = c14.from_samples(
                    &s.symbol_samples,
                    s.symbol_sample_rate_hz,
                    Some(s.obw_hz),
                    Some(snr),
                );
                let mut req =
                    ClassifyRequest::new(&s.samples, s.sample_rate_hz, Timestamp::UNIX_EPOCH);
                req.obw_hz = Some(s.obw_hz);
                req.snr_db = Some(snr);
                req.symbols = symbols.as_ref();
                req.symbol_samples = Some(&s.symbol_samples);
                req.symbol_sample_rate_hz = Some(s.symbol_sample_rate_hz);
                let c = classifier.classify(&req);
                if c.family == UNKNOWN || c.open_set_score >= 0.5 {
                    continue;
                }
                let f = features(&FeatureInput {
                    samples: &s.samples,
                    sample_rate_hz: s.sample_rate_hz,
                    obw_hz: Some(s.obw_hz),
                    snr_db: Some(snr),
                    symbols: symbols.as_ref(),
                });
                if let Some(sc) = model.score(&c.family, &f) {
                    pls.push(sc.plausibility);
                    reported.push(1.0 - c.open_set_score);
                }
            }
        }
        if pls.is_empty() {
            continue;
        }
        let mean = pls.iter().sum::<f64>() / pls.len() as f64;
        let rep = reported.iter().sum::<f64>() / reported.len() as f64;
        let would_abstain = pls.iter().filter(|p| **p < 0.5).count();
        eprintln!(
            "[T-248]   {:<24} claimed {:>2}: claimed-family plausibility {mean:.3} (the score \
             actually used: {rep:.3}); {would_abstain} of them would abstain under fix 1",
            class.label(),
            pls.len()
        );
    }
}
