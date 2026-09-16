//! T-233: the two error rates of the fingerprint's modulation-structure discriminator, measured
//! on the **acceptance** seeds — the only half where a rate may be claimed (ADR-0016 §7). The
//! statistic itself was chosen against the dev half, with `hk-classify`'s `structure-probe`.
//!
//! Every pair here is built at an **identical centre, bandwidth, symbol rate and family label**,
//! which is the condition T-233 was set: what is left to tell two emissions apart once every
//! cheap feature has already agreed. Truth is used only to choose what to generate and to score
//! afterwards; nothing looks a class up and tunes to it.
//!
//! The negative results are asserted as firmly as the positive ones. `2fsk` against `gfsk` is the
//! reason `Fingerprint::compare` keeps its exact family gate, so if a later change ever made that
//! pair separable, this test is where it would show — and the gate could then be revisited.

use hk_classify::structure::modulation_structure;
use hk_classify::synth::{ACCEPTANCE_SEED_BASE, Class, SynthConfig, generate};
use hk_classify::{SeedGuard, Split};
use hk_model::{Fingerprint, ModulationStructure, Tolerances};

/// Observation variants of one emission: every field is a property of how it was watched, never
/// of what was transmitted. Two views of one emitter differ by these and by nothing else.
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

/// A fingerprint identical to every other one here but for its structure, so only the structure
/// can decide the comparison.
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

/// `n` distinct emitters of `class` at `snr`. Seeds come from the acceptance range and are
/// checked through [`SeedGuard`], so a dev seed cannot leak in here.
///
/// `all_views` decides whether each emitter is also observed the other six ways
/// ([`observations`]). A false-*merge* measurement compares one view of one emitter against one
/// view of another and does not need the rest, so it does not pay for them.
fn emitters(
    class: Class,
    snr: f64,
    n: u64,
    all_views: bool,
    guard: &mut SeedGuard,
) -> Vec<Emitter> {
    (0..n)
        .filter_map(|i| {
            let seed = ACCEPTANCE_SEED_BASE + (class as u64) * 10_000 + i;
            guard.require(seed);
            let mut cfgs = observations(snr, seed);
            if !all_views {
                cfgs.truncate(1);
            }
            let mut obw = 0.0;
            let views: Vec<ModulationStructure> = cfgs
                .iter()
                .filter_map(|cfg| {
                    let s = generate(class, cfg);
                    obw = s.obw_hz;
                    modulation_structure(&s.samples, cfg.snr_db)
                })
                .collect();
            (!views.is_empty()).then_some(Emitter { obw_hz: obw, views })
        })
        .collect()
}

/// Two observations of one emitter comparing as two.
fn false_split_rate(es: &[Emitter]) -> f64 {
    let tol = Tolerances::default();
    let (mut n, mut split) = (0u32, 0u32);
    for e in es {
        for other in &e.views[1..] {
            n += 1;
            split += u32::from(
                !fingerprint(e.views[0])
                    .compare(&fingerprint(*other), &tol)
                    .within,
            );
        }
    }
    split as f64 / n.max(1) as f64
}

/// Two *distinct* emitters of different classes comparing as one, over the pairs whose occupied
/// bandwidths already match to ±1 %. At a matched symbol rate that means a matched modulation
/// index, and it is the only way two such emissions reach this comparison at all — the
/// fingerprint's bandwidth and symbol-rate features stop the rest long before.
fn false_merge_rate(a: &[Emitter], b: &[Emitter]) -> (f64, u32) {
    let tol = Tolerances::default();
    let (mut n, mut merged) = (0u32, 0u32);
    for x in a {
        for y in b {
            if (x.obw_hz / y.obw_hz - 1.0).abs() > 0.01 {
                continue;
            }
            n += 1;
            merged += u32::from(
                fingerprint(x.views[0])
                    .compare(&fingerprint(y.views[0]), &tol)
                    .within,
            );
        }
    }
    (merged as f64 / n.max(1) as f64, n)
}

/// **What the discriminator delivers**, on the acceptance seeds.
///
/// `bpsk` and `qpsk` at an identical centre, bandwidth, symbol rate **and family label** — so that
/// only the envelope's fourth moment can decide — and conditioned on their occupied bandwidths
/// already matching, which is the only way two such emissions reach this comparison at all:
///
/// | in-band SNR | false merge | false split |
/// |---|---|---|
/// | 15 dB (the PSK gate) | it abstains: the band is wider than the 0.18 the classes are apart | ~0 |
/// | 20 dB | 0.097 over 1613 pairs | ≤ 0.02 |
/// | 25 dB | 0.027 over 1572 pairs | ≤ 0.02 |
///
/// Against a baseline of **1.00** — with no structure there is nothing left to tell them apart —
/// and against the exact family gate in `Fingerprint::compare`, which splits this pair outright
/// and is what actually keeps the two emitters in two rows. What the structure adds is a second
/// line that still catches nine in ten of them when the labels agree, and the honest figure is
/// that it is a second line and not a guarantee.
///
/// The abstention at the gate is asserted too: it is the sigma doing its job rather than a
/// failure, and it has to stay a property of the derivation rather than of a tuned threshold.
#[test]
fn structure_separates_bpsk_from_qpsk_above_the_psk_gate() {
    let mut guard = SeedGuard::new(Split::Acceptance);
    const GATE: f64 = 15.0;
    for (offset, max_merge) in [(5.0, 0.18), (10.0, 0.07)] {
        let snr = GATE + offset;
        let bpsk = emitters(Class::Bpsk, snr, 250, false, &mut guard);
        let qpsk = emitters(Class::Qpsk, snr, 250, false, &mut guard);
        let (merge, n) = false_merge_rate(&bpsk, &qpsk);
        assert!(n >= 200, "too few bandwidth-matched pairs at {snr} dB: {n}");
        assert!(
            merge <= max_merge,
            "bpsk/qpsk false merge at {snr} dB: {merge:.4} over {n} pairs (limit {max_merge})"
        );
    }
    // The price, measured over every observation variant of each emitter.
    for (name, class) in [("bpsk", Class::Bpsk), ("qpsk", Class::Qpsk)] {
        for offset in [0.0_f64, 5.0, 10.0] {
            let es = emitters(class, GATE + offset, 60, true, &mut guard);
            let split = false_split_rate(&es);
            assert!(
                split <= 0.05,
                "{name} false split at {} dB: {split:.4} — a split mints a ghost inventory row",
                GATE + offset
            );
        }
    }
    // At the gate itself the band is wider than the separation and the dimension concludes
    // nothing, rather than concluding wrongly.
    let bpsk = emitters(Class::Bpsk, GATE, 120, false, &mut guard);
    let qpsk = emitters(Class::Qpsk, GATE, 120, false, &mut guard);
    let (merge, n) = false_merge_rate(&bpsk, &qpsk);
    assert!(
        merge > 0.5,
        "at the PSK gate the dimension should conclude nothing, not conclude: {merge:.4} of {n}"
    );
}

/// **What it does not deliver, and why the family gate stays exact.**
///
/// `2fsk`, `gfsk` and `msk` are constant-envelope *by construction*, so all three read `μ₄ = 1`
/// and the envelope's fourth moment cannot tell them apart at any SNR — they are not three
/// modulations but one modulation at three filter settings. They stay separate inventory rows
/// only because `Fingerprint::compare` still gates on the family label exactly, which is why
/// ADR-0016 §1's `family_of` relaxation is withdrawn rather than deferred: relaxing it would put
/// an `fsk`-labelled row and a `2fsk`-labelled one in reach of each other with nothing left to
/// separate them.
///
/// This asserts the negative so that it cannot quietly stop being true: if a later change made
/// the pair separable here, the gate would be worth revisiting.
#[test]
fn structure_cannot_separate_two_constant_envelope_classes() {
    let mut guard = SeedGuard::new(Split::Acceptance);
    const GATE: f64 = 20.0;
    for offset in [0.0_f64, 5.0, 10.0] {
        let snr = GATE + offset;
        let fsk2 = emitters(Class::Fsk2, snr, 120, false, &mut guard);
        for (name, other) in [
            ("gfsk", Class::Gfsk),
            ("msk", Class::Msk),
            ("4fsk", Class::Fsk4),
        ] {
            let es = emitters(other, snr, 120, false, &mut guard);
            let (merge, n) = false_merge_rate(&fsk2, &es);
            assert!(n >= 50, "too few bandwidth-matched 2fsk/{name} pairs: {n}");
            assert!(
                merge > 0.9,
                "2fsk/{name} at {snr} dB separated on structure ({merge:.4} of {n} merged) — if \
                 this is now real, revisit the exact family gate in hk_model::cluster"
            );
        }
        // And the same classes are never split from *themselves* either: whatever the dimension
        // cannot resolve, it also does not invent.
        let views = emitters(Class::Fsk2, snr, 60, true, &mut guard);
        assert!(false_split_rate(&views) <= 0.05);
    }
}

/// The dimension is allowed to split and never to merge, so adding it to a fingerprint can only
/// make entity resolution stricter. This is the property that made it safe to land at all, and it
/// holds for every class in the taxonomy rather than for the pairs T-233 was aimed at.
#[test]
fn carrying_structure_never_makes_a_pair_match_that_would_not_have() {
    let mut guard = SeedGuard::new(Split::Acceptance);
    let tol = Tolerances::default();
    for (class, gate) in [
        (Class::Am, 10.0),
        (Class::Wfm, 10.0),
        (Class::Fsk2, 20.0),
        (Class::Bpsk, 15.0),
        (Class::Qpsk, 15.0),
        (Class::Ook, 20.0),
        (Class::Ofdm, 15.0),
    ] {
        for e in emitters(class, gate + 5.0, 12, true, &mut guard) {
            for view in &e.views {
                let with = fingerprint(*view);
                let mut without = with.clone();
                without.structure = None;
                // Whatever the structure says, it cannot rescue a pair the rest already rejects…
                let mut far = without.clone();
                far.f_center_hz += 1e6;
                let mut far_with = with.clone();
                far_with.f_center_hz += 1e6;
                assert!(!with.compare(&far_with, &tol).within);
                // …and a pair that matched without it matches no *more* with it.
                assert!(without.compare(&without, &tol).within);
                assert!(without.compare(&with, &tol).within, "{view:?}");
            }
        }
    }
}
