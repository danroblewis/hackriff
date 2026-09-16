//! T-248 diagnostic: **why** each held-out generator is claimed rather than abstained on.
//!
//! Reported, never asserted — the gate is `tests/e2e/.../m3_grid.rs`. This module reproduces that
//! gate's held-out loop **seed for seed** (same order over [`Class::HELD_OUT`], same
//! `ACCEPTANCE_SEED_BASE + 700_000` base, same 12 trials at each of 20/25/30 dB) so every number
//! printed here is the same population the gate rules on.
//!
//! What it decomposes, per generator: the abstention count, which family/class claimed the rest,
//! and the per-dimension z of the winning class — the question being whether any `features@2`
//! dimension separates an unlisted family member from its listed siblings at all.

use hk_classify::density::DensityModel;
use hk_classify::synth::{ACCEPTANCE_SEED_BASE, Class, SynthConfig, generate};
use hk_classify::{Classifier, ClassifyRequest, FeatureInput, Features, SymbolEstimator, features};
use hk_model::Timestamp;
use hk_model::classify::UNKNOWN;

const TRIALS: u32 = 12;
const OFFSETS: [f64; 3] = [0.0, 5.0, 10.0];

struct Trial {
    abstained: bool,
    family: String,
    class: Option<String>,
    confidence: f64,
    open_set: f64,
    f: Features,
}

/// One classification, exactly as the gate does it.
fn run(
    classifier: &Classifier,
    c14: &mut SymbolEstimator,
    class: Class,
    snr_db: f64,
    seed: u64,
) -> Trial {
    let s = generate(class, &SynthConfig::new(snr_db, seed));
    let symbols = c14.from_samples(
        &s.symbol_samples,
        s.symbol_sample_rate_hz,
        Some(s.obw_hz),
        Some(snr_db),
    );
    let mut req = ClassifyRequest::new(&s.samples, s.sample_rate_hz, Timestamp::UNIX_EPOCH);
    req.obw_hz = Some(s.obw_hz);
    req.snr_db = Some(snr_db);
    req.symbols = symbols.as_ref();
    req.symbol_samples = Some(&s.symbol_samples);
    req.symbol_sample_rate_hz = Some(s.symbol_sample_rate_hz);
    let c = classifier.classify(&req);
    let f = features(&FeatureInput {
        samples: &s.samples,
        sample_rate_hz: s.sample_rate_hz,
        obw_hz: Some(s.obw_hz),
        snr_db: Some(snr_db),
        symbols: symbols.as_ref(),
    });
    Trial {
        abstained: c.family == UNKNOWN || c.open_set_score >= 0.5,
        family: c.family.clone(),
        class: c.class.as_ref().map(|k| k.label.clone()),
        confidence: c.confidence,
        open_set: c.open_set_score,
        f,
    }
}

/// Mean signed z of every dimension of `class`'s density over `rows`, worst first.
fn dim_z(model: &DensityModel, class: &str, rows: &[&Features]) -> Vec<(String, f64, f64)> {
    let Some(c) = model.class(class) else {
        return Vec::new();
    };
    let mut out: Vec<(String, f64, f64)> = c
        .dims
        .iter()
        .filter_map(|d| {
            let zs: Vec<f64> = rows
                .iter()
                .filter_map(|r| r.get(&d.feature))
                .map(|x| (x - d.mean) / d.sigma)
                .collect();
            if zs.is_empty() {
                return None;
            }
            let mean = zs.iter().sum::<f64>() / zs.len() as f64;
            let sd = (zs.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / zs.len() as f64).sqrt();
            Some((d.feature.clone(), mean, sd))
        })
        .collect();
    out.sort_by(|a, b| b.1.abs().total_cmp(&a.1.abs()));
    out
}

#[test]
fn t248_why_each_held_out_generator_is_claimed() {
    let classifier = Classifier::new();
    let model = classifier.model().clone();
    let mut c14 = SymbolEstimator::new();
    let mut seed = ACCEPTANCE_SEED_BASE + 700_000;
    let mut all_unknown = 0u32;
    let mut all_total = 0u32;

    for class in Class::HELD_OUT {
        let mut trials: Vec<Trial> = Vec::new();
        for offset in OFFSETS {
            for _ in 0..TRIALS {
                seed += 1;
                trials.push(run(&classifier, &mut c14, *class, 20.0 + offset, seed));
            }
        }
        let hits = trials.iter().filter(|t| t.abstained).count() as u32;
        all_unknown += hits;
        all_total += trials.len() as u32;
        let claimed: Vec<&Trial> = trials.iter().filter(|t| !t.abstained).collect();
        eprintln!(
            "\n[T-248] {:<24} abstained {hits:>2}/{:<3} probes {:?} belongs {:?}",
            class.label(),
            trials.len(),
            class.probes_family(),
            class.nearest_family()
        );
        if claimed.is_empty() {
            continue;
        }
        // Which family/class absorbed it, and how confidently.
        let mut by: std::collections::BTreeMap<String, (u32, f64, f64)> =
            std::collections::BTreeMap::new();
        for t in &claimed {
            let key = format!("{}/{}", t.family, t.class.clone().unwrap_or_default());
            let e = by.entry(key).or_insert((0, 0.0, 0.0));
            e.0 += 1;
            e.1 += t.confidence;
            e.2 += t.open_set;
        }
        for (k, (n, conf, os)) in &by {
            eprintln!(
                "[T-248]   claimed as {k:<22} x{n:<3} mean confidence {:.3}, mean open_set {:.3}",
                conf / f64::from(*n),
                os / f64::from(*n)
            );
        }
        // The density's own best class — the one whose plausibility drives the open-set score.
        // (`Classification::class` is the tree's separate within-family call and is often a
        // different class, so decomposing against it explains nothing.)
        let mut best: std::collections::BTreeMap<String, (u32, f64, f64, f64)> =
            std::collections::BTreeMap::new();
        for t in &claimed {
            if let Some(s) = model.score(&t.family, &t.f) {
                let e = best.entry(s.class.clone()).or_insert((0, 0.0, 0.0, 0.0));
                e.0 += 1;
                e.1 += s.m;
                e.2 += s.plausibility;
                e.3 += s.worst.as_ref().map_or(0.0, |(_, z)| *z);
            }
        }
        for (k, (n, m, pl, w)) in &best {
            let nf = f64::from(*n);
            eprintln!(
                "[T-248]   density best class {k:<10} x{n:<3} m {:.3} (m_p95 {:.3}), plausibility {:.3}, max|z| {:.2}",
                m / nf,
                model.class(k).map_or(f64::NAN, |c| c.m_p95),
                pl / nf,
                w / nf
            );
        }
        if let Some(winner) = best
            .iter()
            .max_by_key(|(_, (n, _, _, _))| *n)
            .map(|(k, _)| k)
        {
            let rows: Vec<&Features> = claimed.iter().map(|t| &t.f).collect();
            eprintln!(
                "[T-248]   vs class `{winner}` worst dims, mean z +- sd over the {} claimed:",
                rows.len()
            );
            for (name, mean, sd) in dim_z(&model, winner, &rows).iter().take(8) {
                eprintln!("[T-248]     {name:<16} z {mean:+7.2} +- {sd:.2}");
            }
        }
    }
    eprintln!(
        "\n[T-248] FULL GRID: unknown recall {:.4}, false-known {:.4} over {all_total}",
        f64::from(all_unknown) / f64::from(all_total),
        f64::from(all_total - all_unknown) / f64::from(all_total)
    );
}
