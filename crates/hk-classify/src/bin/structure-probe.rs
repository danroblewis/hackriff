//! T-233: the two error rates the fingerprint's modulation-structure discriminator has to state,
//! measured through the real [`hk_model::Fingerprint::compare`] on **dev** seeds.
//!
//! ```text
//! cargo run --release -p hk-classify --bin structure-probe -- [emitters] [obw_tolerance]
//! ```
//!
//! Every pair below is built at an **identical centre, bandwidth, symbol rate and family label**,
//! so the only thing that can decide the comparison is `hk_classify::structure`. That is the
//! question T-233 was set: what is left when every cheap feature has already agreed.
//!
//! * **false split** — two observations of *one emitter* (same waveform, different record length,
//!   residual CFO, IQ imbalance or quantisation) compare as two.
//! * **false merge** — two *distinct emitters* of different classes compare as one. Reported both
//!   unconditioned and conditioned on their occupied bandwidths already matching, since at a
//!   matched symbol rate that means a matched modulation index — which is the only way two such
//!   emissions reach this comparison in the first place.
//!
//! This is the tool the numbers in [`hk_model::ModulationStructure`] and on
//! [`Tolerances::STRUCTURE_SIGMAS_DEFAULT`] come from. It reads **dev** seeds;
//! `crates/hk-classify/tests/structure_rates.rs` re-measures the headline figures on the
//! acceptance seeds, where they can be claimed.

use hk_classify::structure::modulation_structure;
use hk_classify::synth::{Class, SynthConfig, generate};
use hk_model::{Fingerprint, ModulationStructure, Tolerances};

/// Observation variants of one emission: every field here is a property of how it was watched,
/// never of what was transmitted.
fn observations(snr: f64, seed: u64) -> Vec<SynthConfig> {
    let base = SynthConfig::new(snr, seed);
    vec![
        base,
        SynthConfig {
            samples: 8_192,
            ..base
        },
        SynthConfig {
            samples: 12_288,
            ..base
        },
        SynthConfig {
            samples: 6_144,
            ..base
        },
        SynthConfig {
            lo_offset_hz: 40.0,
            ..base
        },
        SynthConfig {
            iq_imbalance: 0.03,
            ..base
        },
        SynthConfig {
            quantise_8bit: false,
            ..base
        },
    ]
}

/// A fingerprint that agrees with every other one this tool builds on centre, bandwidth, symbol
/// rate and family, so only the structure can decide.
fn fingerprint(s: ModulationStructure) -> Fingerprint {
    Fingerprint {
        family: Some("same".into()),
        symbol_rate_hz: Some(9600.0),
        structure: Some(s),
        ..Fingerprint::new(446.1e6, 16e3)
    }
}

struct Emitter {
    obw_hz: f64,
    views: Vec<ModulationStructure>,
}

fn emitters(class: Class, snr: f64, n: usize) -> Vec<Emitter> {
    (0..n as u64)
        .filter_map(|seed| {
            let mut obw = 0.0;
            let views: Vec<ModulationStructure> = observations(snr, seed)
                .iter()
                .filter_map(|cfg| {
                    let s = generate(class, cfg);
                    obw = s.obw_hz;
                    modulation_structure(&s.samples, cfg.snr_db)
                })
                .collect();
            (views.len() > 1).then_some(Emitter { obw_hz: obw, views })
        })
        .collect()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let n: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(150);
    let obw_tol: f64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(0.01);
    let tol = Tolerances::default();
    let classes = [
        ("am", Class::Am, 10.0),
        ("wfm", Class::Wfm, 10.0),
        ("nbfm", Class::Nbfm, 10.0),
        ("2fsk", Class::Fsk2, 20.0),
        ("gfsk", Class::Gfsk, 20.0),
        ("msk", Class::Msk, 20.0),
        ("4fsk", Class::Fsk4, 20.0),
        ("bpsk", Class::Bpsk, 15.0),
        ("qpsk", Class::Qpsk, 15.0),
        ("8psk", Class::Psk8, 15.0),
    ];
    // The three pairs T-233 names, plus the nearest neighbours nobody asked about — a
    // discriminator that only works on the pairs it was aimed at is a fit, not a measurement.
    let pairs = [
        ("bpsk", "qpsk"),
        ("bpsk", "8psk"),
        ("qpsk", "8psk"),
        ("2fsk", "gfsk"),
        ("2fsk", "msk"),
        ("gfsk", "msk"),
        ("2fsk", "4fsk"),
        ("am", "wfm"),
        ("am", "nbfm"),
        ("nbfm", "wfm"),
    ];

    for offset in [-5.0_f64, 0.0, 5.0, 10.0] {
        println!("\n================ family gate {offset:+} dB ================");
        let data: Vec<(&str, Vec<Emitter>)> = classes
            .iter()
            .map(|(name, class, gate)| (*name, emitters(*class, gate + offset, n)))
            .collect();

        println!("  {:<6} {:>9} {:>14}", "class", "emitters", "false split");
        for (name, es) in &data {
            let mut n_cmp = 0;
            let mut split = 0;
            for e in es {
                for i in 1..e.views.len() {
                    n_cmp += 1;
                    if !fingerprint(e.views[0])
                        .compare(&fingerprint(e.views[i]), &tol)
                        .within
                    {
                        split += 1;
                    }
                }
            }
            println!(
                "  {name:<6} {:>9} {:>13.4}",
                es.len(),
                split as f64 / n_cmp.max(1) as f64
            );
        }
        println!(
            "  {:<13} {:>9} {:>14}   {:>9} {:>14}",
            "pair", "all pairs", "false merge", "obw-matched", "false merge"
        );
        for (a, b) in pairs {
            let ea = &data.iter().find(|(n, _)| *n == a).unwrap().1;
            let eb = &data.iter().find(|(n, _)| *n == b).unwrap().1;
            let (mut all, mut all_merge, mut cond, mut cond_merge) = (0u64, 0u64, 0u64, 0u64);
            for x in ea {
                for y in eb {
                    let merged = fingerprint(x.views[0])
                        .compare(&fingerprint(y.views[0]), &tol)
                        .within;
                    all += 1;
                    all_merge += u64::from(merged);
                    if (x.obw_hz / y.obw_hz - 1.0).abs() <= obw_tol {
                        cond += 1;
                        cond_merge += u64::from(merged);
                    }
                }
            }
            println!(
                "  {:<13} {all:>9} {:>13.4}   {cond:>9} {:>13.4}",
                format!("{a}/{b}"),
                all_merge as f64 / all.max(1) as f64,
                cond_merge as f64 / cond.max(1) as f64,
            );
        }
    }
}
