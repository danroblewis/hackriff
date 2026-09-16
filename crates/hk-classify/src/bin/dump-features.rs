//! Prints the `features@1` vector and the classifier's call for each synthetic class.
//!
//! ```text
//! cargo run -p hk-classify --bin dump-features -- [snr_db] [seed]
//! ```
//!
//! This is the tool behind every threshold in [`hk_classify::tree`]: it shows what a feature
//! actually measures on a known waveform, so a rule can be set from the physics instead of from a
//! failing test. It reads only the **dev** seeds by default.

use hk_classify::classifier::{Classifier, ClassifyRequest};
use hk_classify::features::{FeatureInput, features};
use hk_classify::symbols::SymbolEstimator;
use hk_classify::synth::{Class, SynthConfig, generate};
use hk_model::Timestamp;

fn main() {
    let mut args = std::env::args().skip(1);
    let snr_db: f64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(25.0);
    let seed: u64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(7);
    let show: Vec<&str> = vec![
        "env_cv",
        "low_fraction",
        "duty",
        "c20_norm",
        "mu42_a",
        "c40_norm",
        "c42_norm",
        "if_bimodality",
        "if_modality",
        "if_slope_r2",
        "flatness",
        "symmetry",
        "carrier_line_db",
        "cp_corr",
        "gamma_max",
        // The C14 dimensions (T-238): absent here means C14 abstained on that snippet.
        "cyclic_db",
        "obw_over_rs",
        "blind_fsk",
    ];
    print!("{:<12} {:>9} {:>7}", "class", "obw kHz", "call");
    for name in &show {
        print!(" {:>9}", &name[..name.len().min(9)]);
    }
    println!();
    let classifier = Classifier::new();
    let mut c14 = SymbolEstimator::new();
    for class in Class::TAXONOMY.iter().chain(Class::HELD_OUT) {
        let s = generate(*class, &SynthConfig::new(snr_db, seed));
        let symbols = c14.from_samples(
            &s.symbol_samples,
            s.symbol_sample_rate_hz,
            Some(s.obw_hz),
            Some(snr_db),
        );
        let f = features(&FeatureInput {
            samples: &s.samples,
            sample_rate_hz: s.sample_rate_hz,
            obw_hz: Some(s.obw_hz),
            snr_db: Some(snr_db),
            symbols: symbols.as_ref(),
        });
        let mut req = ClassifyRequest::new(&s.samples, s.sample_rate_hz, Timestamp::UNIX_EPOCH);
        req.obw_hz = Some(s.obw_hz);
        req.snr_db = Some(snr_db);
        req.symbols = symbols.as_ref();
        let c = classifier.classify_features(&f, &req);
        print!(
            "{:<12} {:>9.1} {:>7}",
            class.label(),
            s.obw_hz / 1e3,
            &c.family[..c.family.len().min(7)]
        );
        for name in &show {
            match f.get(name) {
                Some(v) => print!(" {v:>9.3}"),
                None => print!(" {:>9}", "-"),
            }
        }
        println!();
    }
}
