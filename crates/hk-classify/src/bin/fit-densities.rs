//! Fits `data/densities-1.json` from the synthetic **dev** grid (ADR-0016 §4.3, §7).
//!
//! ```text
//! cargo run -p hk-classify --bin fit-densities
//! ```
//!
//! Dev seeds only ([`hk_classify::synth::DEV_SEEDS`]), and only at or above each family's SNR gate
//! — the density describes what a family looks like *when the classifier is allowed to claim it*.
//! The acceptance seeds are never touched here, so the accuracy the tests report is blind
//! (docs/10 §3.2, the ADR's evaluation protocol).

use std::path::PathBuf;

use hk_classify::density::DensityModel;
use hk_classify::features::{FeatureInput, features};
use hk_classify::harness::{SeedGuard, Split};
use hk_classify::synth::{Class, DEV_SEEDS, SynthConfig, generate};
use hk_classify::thresholds::thresholds_of;

/// Seeds per class (a slice of the dev range: enough for a stable mean and sigma per feature).
const SEEDS_PER_CLASS: u64 = 60;

/// SNR offsets above the family gate, dB.
const SNR_STEPS: [f64; 4] = [0.0, 5.0, 10.0, 15.0];

fn main() {
    // T-213: every seed fitting touches must be a dev seed. This is the runtime half of the
    // dev/acceptance split — the ranges being disjoint (checked at compile time in
    // `hk_classify::synth`) only means the split is *possible*; this fails loudly, with the
    // offending seed named, if an acceptance seed ever ends up here by mistake.
    let mut guard = SeedGuard::new(Split::Dev);
    let mut labelled = Vec::new();
    let mut cells = 0usize;
    for class in Class::TAXONOMY {
        let family = class.family().expect("a taxonomy class has a family");
        let gate = thresholds_of(family)
            .and_then(|t| t.snr_gate_db)
            .unwrap_or(10.0);
        for step in SNR_STEPS {
            let snr = gate + step;
            for seed in DEV_SEEDS.start..(DEV_SEEDS.start + SEEDS_PER_CLASS) {
                guard.require(seed);
                let s = generate(*class, &SynthConfig::new(snr, seed));
                let f = features(&FeatureInput {
                    samples: &s.samples,
                    sample_rate_hz: s.sample_rate_hz,
                    obw_hz: Some(s.obw_hz),
                    snr_db: Some(snr),
                    symbols: None,
                });
                labelled.push((class.label().to_owned(), family.to_owned(), f));
                cells += 1;
            }
        }
    }
    let model = DensityModel::fit(
        &labelled,
        &format!(
            "hk-classify synth dev grid: seeds {}..{}, gate+{{0,5,10,15}} dB, {cells} snippets",
            DEV_SEEDS.start,
            DEV_SEEDS.start + SEEDS_PER_CLASS
        ),
    );
    model.validate().expect("fitted model is valid");
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data/densities-1.json");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&model).expect("serialise"),
    )
    .expect("write densities");
    for c in &model.classes {
        println!(
            "{:<11} {:<11} {:>2} dims, n = {}",
            c.family,
            c.class,
            c.dims.len(),
            c.n
        );
    }
    println!("wrote {}", path.display());
}
