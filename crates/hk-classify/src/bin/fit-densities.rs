//! Fits the shipped density files from the synthetic **dev** grid (ADR-0016 §4.3, §7).
//!
//! ```text
//! cargo run -p hk-classify --bin fit-densities
//! ```
//!
//! Dev seeds only ([`hk_classify::synth::DEV_SEEDS`]). The acceptance seeds are never touched here,
//! so the accuracy the tests report is blind (docs/10 §3.2, the ADR's evaluation protocol).
//!
//! # Two models, because a density is asked two different questions
//!
//! 1. **`data/densities-1.json`** — fitted at and above each family's SNR gate. It ranks the
//!    families that may be claimed and calibrates the χ² open set. It has to stay **tight**: it is
//!    what tells an out-of-taxonomy generator that it belongs to no known class, and a class fitted
//!    across a wide SNR range grows wide enough to absorb one (measured: fitting a single model
//!    over gate−10…gate+15 dB raised known top-1 to 0.950 but dropped held-out unknown recall from
//!    0.829 to 0.519, and the false-known rate from 0.171 to 0.481).
//! 2. **`data/densities-below-gate-1.json`** — fitted *below* each family's gate. It answers only
//!    "could this snippet be the family whose gate held it back?", which `classifier` turns into
//!    `unknown` mass (ADR-0016 §2: a below-gate family is "not measured", not "ruled out"). That
//!    question is asked precisely where model 1 is an extrapolation, so it needs its own fit: with
//!    model 1 a genuine 2-FSK burst 10 dB under the FSK gate scored its own family at plausibility
//!    0.000, so no mass moved to `unknown` and the constant-envelope `analog` classes — which that
//!    waveform really does resemble at that SNR — claimed it (a 0.65 wrong-label rate in that bin).
//!
//! Only model 1 is ever used to *claim* a family, so widening model 2 cannot make the classifier
//! more confident; it can only move mass to `unknown`.

use std::path::PathBuf;

use hk_classify::density::DensityModel;
use hk_classify::features::{FeatureInput, features};
use hk_classify::harness::{SeedGuard, Split};
use hk_classify::symbols::SymbolEstimator;
use hk_classify::synth::{Class, DEV_SEEDS, SynthConfig, generate};
use hk_classify::thresholds::thresholds_of;

/// Seeds per class (a slice of the dev range: enough for a stable mean and sigma per feature).
const SEEDS_PER_CLASS: u64 = 60;

/// SNR offsets above the family gate, dB: where a family may be claimed.
const ABOVE_GATE: [f64; 4] = [0.0, 5.0, 10.0, 15.0];

/// SNR offsets below the family gate, dB: where a family is gated out and can only contribute
/// `unknown` mass.
const BELOW_GATE: [f64; 4] = [-10.0, -7.5, -5.0, -2.5];

fn main() {
    // T-213: every seed fitting touches must be a dev seed. This is the runtime half of the
    // dev/acceptance split — the ranges being disjoint (checked at compile time in
    // `hk_classify::synth`) only means the split is *possible*; this fails loudly, with the
    // offending seed named, if an acceptance seed ever ends up here by mistake.
    let mut guard = SeedGuard::new(Split::Dev);

    let above = fit(&mut guard, &ABOVE_GATE, "gate+{0,5,10,15} dB");
    let below = fit(&mut guard, &BELOW_GATE, "gate-{10,7.5,5,2.5} dB");

    write(&above, "densities-1.json");
    write(&below, "densities-below-gate-1.json");
    for c in &above.classes {
        println!(
            "{:<11} {:<11} {:>2} dims, n = {}",
            c.family,
            c.class,
            c.dims.len(),
            c.n
        );
    }
}

/// Fits one model over the given SNR offsets from each family's gate.
fn fit(guard: &mut SeedGuard, steps: &[f64], label: &str) -> DensityModel {
    let mut labelled = Vec::new();
    let mut cells = 0usize;
    // C14 runs here for the same reason it runs in the pipeline: without it the six symbol-derived
    // dimensions of `features@1` abstain, and a dimension that abstains during fitting is dropped
    // from every class's density (`MIN_PRESENCE`) — so it could never be scored on at classify
    // time either. Fitting and classifying must measure the same things (T-238).
    let mut c14 = SymbolEstimator::new();
    for class in Class::TAXONOMY {
        let family = class.family().expect("a taxonomy class has a family");
        let gate = thresholds_of(family)
            .and_then(|t| t.snr_gate_db)
            .unwrap_or(10.0);
        for step in steps {
            let snr = gate + step;
            for seed in DEV_SEEDS.start..(DEV_SEEDS.start + SEEDS_PER_CLASS) {
                guard.require(seed);
                let s = generate(*class, &SynthConfig::new(snr, seed));
                let symbols = c14.from_samples(
                    &s.symbol_samples,
                    s.symbol_sample_rate_hz,
                    Some(s.obw_hz),
                    Some(snr),
                );
                let f = features(&FeatureInput {
                    samples: &s.samples,
                    sample_rate_hz: s.sample_rate_hz,
                    obw_hz: Some(s.obw_hz),
                    snr_db: Some(snr),
                    symbols: symbols.as_ref(),
                });
                labelled.push((class.label().to_owned(), family.to_owned(), f));
                cells += 1;
            }
        }
    }
    let model = DensityModel::fit(
        &labelled,
        &format!(
            "hk-classify synth dev grid: seeds {}..{}, {label}, {cells} snippets",
            DEV_SEEDS.start,
            DEV_SEEDS.start + SEEDS_PER_CLASS
        ),
    );
    model.validate().expect("fitted model is valid");
    model
}

fn write(model: &DensityModel, name: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("data")
        .join(name);
    std::fs::write(
        &path,
        serde_json::to_string_pretty(model).expect("serialise"),
    )
    .expect("write densities");
    println!("wrote {}", path.display());
}
