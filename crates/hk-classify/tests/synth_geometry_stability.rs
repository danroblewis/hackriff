//! **The dev grid's analysis geometry must be a property of the signal, not of the noise draw**
//! (T-564).
//!
//! T-435 measured, while characterising the confidently-wrong family call, that `synth::generate`
//! derived its channel-filter cutoff and both decimations from OBW99 **re-measured on the noisy
//! snippet**. So `decim = floor(fs / (2 · obw))` was set by the noise realisation: over 504 blind
//! ladders the OBW99 reported for one `(class, seed)` moved with the SNR by ≥ 2× on 115 (23 %) and
//! ≥ 3× on 90 (18 %). Two rungs of one ladder were then not the same experiment with more noise —
//! they were **different experiments**, so every accuracy figure computed up an SNR ladder silently
//! compared them, and a classifier that looked unstable with SNR might only have been seeing a
//! different filter each time.
//!
//! What this file asserts, in counts and never in wall-clock: for a fixed `(class, seed)` the
//! **delivered** geometry — channel-filter cutoff, analysis decimation, C14's decimation, and the
//! OBW99 all three are derived from — is *identical* across the whole SNR ladder, over a stated
//! number of ladders so it cannot pass on zero.
//!
//! What it deliberately does **not** assert: that `SynthSignal::obw_hz` is stable. That field is
//! the OBW99 C13 would *measure* on the delivered snippet, and measuring it on the noisy signal is
//! the product — the blind path must keep measuring. This is about the harness's analysis geometry
//! only. The two are now separate fields precisely so the line is visible
//! ([`hk_classify::synth::Geometry::geometry_obw_hz`] versus `SynthSignal::obw_hz`).

use hk_classify::synth::{Class, SynthConfig, generate};
use hk_classify::thresholds_of;

/// T-435's ladder: seven rungs at 2.5 dB spacing about each family's own SNR gate.
const OFFSETS: [f64; 7] = [-5.0, -2.5, 0.0, 2.5, 5.0, 7.5, 10.0];

/// Seeds per class. 21 taxonomy classes × 8 = **168 ladders**, 1 176 generated waveforms — the
/// count is stated in the assertion messages so a refactor that silently generated nothing would
/// fail rather than pass.
const SEEDS: u64 = 8;

fn gate_db(class: Class) -> f64 {
    let family = class.family().expect("a taxonomy class has a family");
    thresholds_of(family)
        .and_then(|t| t.snr_gate_db)
        .unwrap_or(10.0)
}

#[test]
fn delivered_geometry_is_identical_across_the_snr_ladder() {
    let mut ladders = 0usize;
    let mut moved: Vec<String> = Vec::new();
    for class in Class::TAXONOMY {
        let gate = gate_db(*class);
        for seed in 1..=SEEDS {
            let seed = seed * 1_000 + 7;
            let rungs: Vec<_> = OFFSETS
                .iter()
                .map(|off| {
                    let snr = gate + off;
                    (snr, generate(*class, &SynthConfig::new(snr, seed)).geometry)
                })
                .collect();
            ladders += 1;
            let (_, first) = rungs[0];
            for (snr, g) in &rungs[1..] {
                if *g != first {
                    moved.push(format!(
                        "{} seed {seed}: {:.1} dB {:?} vs {:.1} dB {:?}",
                        class.label(),
                        rungs[0].0,
                        first,
                        snr,
                        g
                    ));
                    break;
                }
            }
        }
    }
    assert_eq!(
        ladders,
        Class::TAXONOMY.len() * SEEDS as usize,
        "expected one ladder per (class, seed)"
    );
    assert_eq!(ladders, 168, "the stated ladder count must not drift");
    assert!(
        moved.is_empty(),
        "delivered analysis geometry moved with the SNR on {} of {ladders} ladders — it is a \
         statistic of the noise (T-564):\n{}",
        moved.len(),
        moved.join("\n")
    );
}

/// The spread T-435 reported, re-measurable on demand: 21 classes × 24 seeds = **504 ladders**,
/// 3 528 waveforms, binned by worst-rung / best-rung ratio for both the geometry OBW99 and the
/// reported (measured) one.
///
/// Ignored because it is a measurement, not a guard — run it with
/// `cargo nextest run -p hk-classify -E 'binary(synth_geometry_stability)' --run-ignored all
/// --no-capture`.
#[test]
#[ignore = "T-564 measurement, not a guard"]
fn t564_measure_obw_spread() {
    let mut rows: Vec<(f64, f64)> = Vec::new();
    let mut per_family: std::collections::BTreeMap<&str, (usize, usize)> = Default::default();
    for class in Class::TAXONOMY {
        let gate = gate_db(*class);
        let family = class.family().unwrap();
        for seed in 1..=24u64 {
            let seed = seed * 1_000 + 7;
            let mut geom: Vec<f64> = Vec::new();
            let mut reported: Vec<f64> = Vec::new();
            for off in OFFSETS {
                let s = generate(*class, &SynthConfig::new(gate + off, seed));
                geom.push(s.geometry.geometry_obw_hz);
                reported.push(s.obw_hz);
            }
            let ratio = |v: &[f64]| {
                let lo = v.iter().cloned().fold(f64::INFINITY, f64::min);
                let hi = v.iter().cloned().fold(0.0_f64, f64::max);
                hi / lo.max(1e-9)
            };
            let (rg, rr) = (ratio(&geom), ratio(&reported));
            rows.push((rg, rr));
            let e = per_family.entry(family).or_default();
            e.1 += 1;
            if rg >= 2.0 {
                e.0 += 1;
            }
        }
    }
    let n = rows.len();
    let count = |sel: fn(&(f64, f64)) -> f64, t: f64| rows.iter().filter(|r| sel(r) >= t).count();
    println!("T-564 / T-435 measure over {n} ladders (21 classes x 24 seeds x 7 rungs)");
    for (name, sel) in [
        (
            "geometry OBW99 (sets cutoff + decim)",
            (|r| r.0) as fn(&(f64, f64)) -> f64,
        ),
        (
            "reported obw_hz (what C13 measures)",
            (|r| r.1) as fn(&(f64, f64)) -> f64,
        ),
    ] {
        println!(
            "  {name}: >=2x on {} of {n} ({:.1} %), >=3x on {} ({:.1} %)",
            count(sel, 2.0),
            100.0 * count(sel, 2.0) as f64 / n as f64,
            count(sel, 3.0),
            100.0 * count(sel, 3.0) as f64 / n as f64,
        );
    }
    for (f, (bad, tot)) in per_family {
        println!("  geometry >=2x by family: {f} {bad}/{tot}");
    }
}
