//! Exports the **dev** half of the synthetic AMC grid as training vectors for the per-family DL
//! stage (T-204). Training itself is Python (`py/hkpy/ml/train_amc`, orchestration only); this
//! binary is what guarantees the trainer sees exactly the vector the Rust stage will compute at
//! inference time — [`hk_classify::dl::dl_input`], one implementation, no mirrored featuriser to
//! drift.
//!
//! ```text
//! cargo run -p hk-classify --release --bin dl-train-export -- <out-dir> [train_seeds] [holdout_seeds]
//! ```
//!
//! **Dev seeds only**, enforced by [`SeedGuard`] at every use: fitting must never touch an
//! acceptance seed (ADR-0016 §7). The split inside dev is further divided so the model is trained
//! and calibrated on disjoint waveforms:
//!
//! | split | seeds | used for |
//! |---|---|---|
//! | `train` | `DEV_SEEDS.start …` | fitting the weights |
//! | `holdout` | 300 onwards in the dev range | temperature, the energy threshold, and the accuracy comparison |
//! | `ood` | the held-out generators, dev seeds | the open-set (AUROC, false-known) measurement |
//!
//! The output is one JSON object per line (never committed: it is a dataset).

use std::io::{BufWriter, Write};
use std::path::PathBuf;

use hk_classify::dl::{DL_INPUT_DIM, DL_INPUT_VERSION, dl_input};
use hk_classify::harness::{SeedGuard, Split};
use hk_classify::synth::{Class, DEV_SEEDS, SynthConfig, generate};
use hk_classify::thresholds::thresholds_of;

/// SNR offsets above each family's gate: the only region where a family may be claimed at all, so
/// the only region a within-family model is ever asked about.
const OFFSETS: [f64; 4] = [0.0, 5.0, 10.0, 15.0];

/// First holdout seed inside the dev range.
const HOLDOUT_BASE: u64 = 300;

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args.next().map(PathBuf::from).unwrap_or_else(|| {
        eprintln!("usage: dl-train-export <out-dir> [train_seeds] [holdout_seeds]");
        std::process::exit(2);
    });
    let n_train: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(200);
    let n_holdout: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(100);
    assert!(
        DEV_SEEDS.start + n_train <= HOLDOUT_BASE && HOLDOUT_BASE + n_holdout <= DEV_SEEDS.end,
        "train and holdout seed ranges must be disjoint and inside the dev range"
    );

    std::fs::create_dir_all(&out).expect("create output dir");
    let path = out.join("amc-dev.jsonl");
    let mut w = BufWriter::new(std::fs::File::create(&path).expect("create dataset"));

    // The runtime half of the dev/acceptance split: every seed below is checked, so a mistake
    // fails the export instead of quietly training on the test set.
    let mut guard = SeedGuard::new(Split::Dev);
    let mut rows = 0usize;
    let mut skipped = 0usize;

    let mut emit = |guard: &mut SeedGuard,
                    class: Class,
                    family: &str,
                    split: &str,
                    seeds: std::ops::Range<u64>,
                    offsets: &[f64],
                    gate: f64| {
        for &offset in offsets {
            let snr = gate + offset;
            for seed in seeds.clone() {
                guard.require(seed);
                let s = generate(class, &SynthConfig::new(snr, seed));
                let Some(x) = dl_input(&s.samples, s.sample_rate_hz, Some(s.obw_hz)) else {
                    skipped += 1;
                    continue;
                };
                writeln!(
                    w,
                    "{}",
                    serde_json::json!({
                        "input_version": DL_INPUT_VERSION,
                        "class": class.label(),
                        "family": family,
                        "split": split,
                        "snr_db": snr,
                        "offset_db": offset,
                        "seed": seed,
                        "x": x,
                    })
                )
                .expect("write row");
                rows += 1;
            }
        }
    };

    for class in Class::TAXONOMY {
        let family = class.family().expect("a taxonomy class has a family");
        let gate = thresholds_of(family)
            .and_then(|t| t.snr_gate_db)
            .unwrap_or(10.0);
        let train = DEV_SEEDS.start..(DEV_SEEDS.start + n_train);
        let holdout = HOLDOUT_BASE..(HOLDOUT_BASE + n_holdout);
        emit(&mut guard, *class, family, "train", train, &OFFSETS, gate);
        emit(
            &mut guard, *class, family, "holdout", holdout, &OFFSETS, gate,
        );
    }

    // Out-of-taxonomy generators, labelled by the family whose boundary they test
    // (`probes_family`, T-244): the open set is measured against what would actually be routed
    // into each model, not against noise. Routing by `nearest_family` instead — which is `None`
    // for a generator that belongs to no family — exported no `ood` rows at all for `analog`,
    // `psk-qam` and `pulsed`, and dropped three of the six generators entirely.
    for class in Class::HELD_OUT {
        let Some(family) = class.probes_family() else {
            continue;
        };
        let gate = thresholds_of(family)
            .and_then(|t| t.snr_gate_db)
            .unwrap_or(10.0);
        let seeds = HOLDOUT_BASE..(HOLDOUT_BASE + n_holdout);
        emit(&mut guard, *class, family, "ood", seeds, &OFFSETS, gate);
    }

    w.flush().expect("flush");
    println!(
        "wrote {rows} rows ({DL_INPUT_DIM} dims, input@{DL_INPUT_VERSION}) to {} \
         [{skipped} unmeasurable, {} dev seeds used]",
        path.display(),
        guard.seeds_used().len()
    );
}
