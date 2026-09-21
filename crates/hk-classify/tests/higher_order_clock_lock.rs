//! **C14 must report a symbol rate on genuine 8-PSK, and a verifier that cannot run must say so**
//! (T-589).
//!
//! # What was measured, before anything was changed
//!
//! T-246 found, while fixing the psk-qam ALRT, that C14 reported a trusted symbol rate on genuine
//! 8-PSK at 20 and 30 dB in **zero of twelve** cases. The consequence is not a degraded verifier
//! but an **absent** one: `verify` skips with [`SkipReason::NoClockLock`] before it reaches the
//! likelihood, so T-246's correction was exercised only by handing the stage the generator's own
//! geometry and never by the shipped path. A stage that silently does not run reads exactly like a
//! stage that ran and agreed.
//!
//! Measuring the stage input first — as T-246 did, and as it had to, having refuted two earlier
//! attributions that way — the two obvious suspects are both **wrong**:
//!
//! - *"The cyclic line is weaker at 8-PSK's tighter symbol spacing."* It is not. On all twelve
//!   snippets the rate consensus **passed** (`RateTrust::consensus` true, 12 of 12), with four
//!   direct lines at the same rate from **both** independent method groups at 19.8–26.5 dB — a
//!   stronger line than BPSK's on the same grid. The rate was found, correctly, every time.
//! - *"The 8th-power line is too weak to see."* Also not the failure. The 8th-power line is
//!   never computed at all.
//!
//! The gate that actually fails is `digital_structure`. C14 labels four families —
//! `{ook, fsk, bpsk, qpsk}` — and the structure gate was "one of those four scored ≥ 0.5, or a
//! transition fit converged". BPSK is recognised by its **second**-power carrier line (`c2` 0.89 to
//! 1.00 on this grid) and QPSK by its **fourth** (`c4` 0.61 to 0.73). Genuine 8-PSK collapses at
//! the **eighth** power and at no lower one, so it reads `c2` 0.06–0.12 and `c4` 0.05–0.10 — the
//! bias-corrected null — and every family score is ≈ 0. The correctly-measured rate was then
//! discarded with `NoDigitalStructure` on 12 of 12.
//!
//! So the defect is that a *family-label* test was doing duty as a *digital-structure* test, and
//! the family taxonomy stops at order four.
//!
//! # The fix, and why it admits 8-PSK and nothing else
//!
//! `RateTrust::higher_order_linear`: the same ratio-veto shape the `bpsk` and `qpsk` scores already
//! use, one order up — `c8` above the null, **and** `c4` not dominating `c8`. The veto is what does
//! the work. A high `c8` on its own is unremarkable (`ook` 0.90, `am` 0.96, `ppm` 0.94, `vsb-am`
//! 0.73 all have one), but in every such case a *lower* order line is at least as strong, because
//! the structure is an envelope or a carrier rather than an order-8 alphabet. Only a suppressed
//! carrier whose alphabet first collapses at eight passes both halves.
//!
//! This is structure evidence and **nothing else**: it is not a family score, it does not join
//! `FamilyScores` or `features@1`, and it adds no classification hypothesis (restoring `qam16` /
//! `qam64` to the psk-qam hypothesis set is T-590, deliberately separate).

use std::collections::BTreeMap;

use hk_classify::classifier::{Classifier, ClassifyRequest};
use hk_classify::symbols::SymbolEstimator;
use hk_classify::synth::{ACCEPTANCE_SEED_BASE, Class, SynthConfig, generate};
use hk_classify::verify::{SkipReason, VerifyInput, VerifyOutcome, verify};
use hk_model::Timestamp;
use hk_model::classify::LabelP;

const SEEDS: u64 = 6;
const SNRS: [f64; 2] = [20.0, 30.0];

fn seed(k: u64) -> u64 {
    ACCEPTANCE_SEED_BASE + 900 + k
}

/// The generator's own symbol rate for `(class, seed)` — [`generate`]'s first draw, reproduced
/// exactly. Used **only** to score the receiver's estimate, never fed to it.
fn true_rate_bd(class: Class, seed: u64) -> f64 {
    let mut rng = hk_dsp::synth::Rng::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ class as u64);
    25e3 + 75e3 * rng.unit()
}

/// **The defect itself.** C14 must report a trusted symbol rate on genuine 8-PSK, and it must be
/// the right rate.
///
/// Before the fix this read `8psk 20 dB: 0/6` and `8psk 30 dB: 0/6`.
#[test]
fn c14_reports_a_trusted_symbol_rate_on_genuine_8psk() {
    let mut locks: BTreeMap<(&str, u64), usize> = BTreeMap::new();
    let mut ran = 0;
    let mut worst_err_pct: f64 = 0.0;
    for (class, name) in [
        (Class::Bpsk, "bpsk"),
        (Class::Qpsk, "qpsk"),
        (Class::Psk8, "8psk"),
    ] {
        for snr in SNRS {
            for k in 0..SEEDS {
                let s = generate(class, &SynthConfig::new(snr, seed(k)));
                let mut c14 = SymbolEstimator::new();
                let Some(p) = c14.from_samples(
                    &s.symbol_samples,
                    s.symbol_sample_rate_hz,
                    Some(s.obw_hz),
                    Some(snr),
                ) else {
                    continue;
                };
                ran += 1;
                if !p.rate_trusted() {
                    continue;
                }
                let rate = p
                    .symbol_rate_bd
                    .value()
                    .expect("a trusted rate has a value");
                *locks.entry((name, snr as u64)).or_default() += 1;
                let err = (rate / true_rate_bd(class, seed(k)) - 1.0).abs() * 100.0;
                worst_err_pct = worst_err_pct.max(err);
            }
        }
    }
    println!("T-589 C14 lock rate per class and SNR (of {SEEDS} each): {locks:?}");
    println!("T-589 C14 ran on {ran} of 36 snippets; worst rate error {worst_err_pct:.3} %");
    assert_eq!(ran, 36, "C14 must have been offered every snippet");
    // The finding, as a count: zero before, six of six after, at BOTH SNRs.
    assert_eq!(
        locks.get(&("8psk", 20)).copied().unwrap_or(0),
        6,
        "genuine 8-PSK at 20 dB: {locks:?}"
    );
    assert_eq!(
        locks.get(&("8psk", 30)).copied().unwrap_or(0),
        6,
        "genuine 8-PSK at 30 dB: {locks:?}"
    );
    // A lock that reported the wrong rate would be worse than no lock: C14's own fingerprinting
    // tolerance is ±1 % (docs/04 §7.6).
    assert!(
        worst_err_pct < 1.0,
        "worst symbol-rate error {worst_err_pct:.3} % over every lock"
    );
    // The classes that already worked must be untouched: 11 of 12 bpsk, 12 of 12 qpsk, exactly as
    // measured before the change.
    assert_eq!(
        locks.get(&("bpsk", 20)).copied().unwrap_or(0)
            + locks.get(&("bpsk", 30)).copied().unwrap_or(0),
        11,
        "bpsk lock rate moved: {locks:?}"
    );
    assert_eq!(
        locks.get(&("qpsk", 20)).copied().unwrap_or(0)
            + locks.get(&("qpsk", 30)).copied().unwrap_or(0),
        12,
        "qpsk lock rate moved: {locks:?}"
    );
}

/// Lock counts over the **whole** 31-class grid, per class and SNR, against the table measured
/// before the change.
///
/// Loosening a trust gate is exactly the change that can buy one class a lock by handing a false
/// one to five others, so the guard is the entire table rather than the one row that was meant to
/// move. Every entry below is a measurement: `Psk8` is the only one that changed, from `0 0` to
/// `6 6`.
#[test]
fn the_order_8_gate_admits_8psk_and_moves_no_other_class() {
    // (class, locks at 20 dB, locks at 30 dB, order-8 structure asserted anywhere in the 12).
    const EXPECT: &[(Class, usize, usize, bool)] = &[
        (Class::Am, 0, 0, false),
        (Class::Nbfm, 0, 0, false),
        (Class::Wfm, 0, 1, false),
        (Class::Ssb, 0, 1, false),
        (Class::Cw, 6, 6, false),
        (Class::Ook, 6, 6, false),
        (Class::Ask4, 0, 0, false),
        (Class::Fsk2, 6, 6, false),
        (Class::Gfsk, 5, 5, false),
        (Class::Msk, 6, 6, false),
        (Class::Fsk4, 5, 6, false),
        (Class::Bpsk, 6, 5, false),
        (Class::Qpsk, 6, 6, false),
        // The one row this ticket moves: 0, 0 before.
        (Class::Psk8, 6, 6, true),
        (Class::Qam16, 5, 6, false),
        (Class::Qam64, 4, 3, false),
        (Class::Ofdm, 0, 0, false),
        (Class::Chirp, 1, 0, false),
        (Class::Ppm, 6, 6, false),
        (Class::Pulse, 0, 0, false),
        (Class::NoiseLike, 0, 0, false),
        (Class::Ask3, 0, 0, false),
        (Class::Fsk8, 0, 0, false),
        (Class::ChirpedFsk, 0, 0, false),
        (Class::CostasHop, 0, 0, false),
        (Class::OfdmOddCp, 0, 0, false),
        (Class::NoiseBurst, 0, 0, false),
        (Class::DsbSc, 3, 4, false),
        (Class::VsbAm, 0, 0, false),
        (Class::Pi4Dqpsk, 5, 5, false),
        (Class::Apsk16, 0, 0, false),
        (Class::CodedPulse, 6, 6, false),
    ];
    let mut wrong: Vec<String> = Vec::new();
    let mut ran = 0;
    for &(class, at20, at30, order8) in EXPECT {
        let mut got = [0usize; 2];
        let mut saw_order8 = false;
        for (i, snr) in SNRS.into_iter().enumerate() {
            for k in 0..SEEDS {
                let s = generate(class, &SynthConfig::new(snr, seed(k)));
                let mut c14 = SymbolEstimator::new();
                let Some(p) = c14.from_samples(
                    &s.symbol_samples,
                    s.symbol_sample_rate_hz,
                    Some(s.obw_hz),
                    Some(snr),
                ) else {
                    continue;
                };
                ran += 1;
                got[i] += usize::from(p.rate_trusted());
                saw_order8 |= p.rate_trust.higher_order_linear;
            }
        }
        if got != [at20, at30] || saw_order8 != order8 {
            wrong.push(format!(
                "{class:?}: locks {got:?} (want [{at20}, {at30}]), order-8 structure {saw_order8} \
                 (want {order8})"
            ));
        }
    }
    println!(
        "T-589 grid: C14 ran on {ran} of {} snippets",
        EXPECT.len() * 12
    );
    assert_eq!(
        ran,
        EXPECT.len() * 12,
        "every snippet must have reached C14, or the table below is comparing nothing"
    );
    assert!(
        wrong.is_empty(),
        "the order-8 structure gate moved a class it must not: {wrong:#?}"
    );
}

/// **The absence must be visible.** Every classification states what the post-sync verifier did —
/// it ran, or the reason it did not — and on genuine 8-PSK that reason is no longer "no clock
/// lock".
///
/// Before the fix the outcome of [`verify`] was dropped on the floor at the call site, so a
/// classification whose verifier confirmed the call and one whose verifier never ran carried
/// identical reasons; and all twelve 8-PSK classifications were the second kind.
#[test]
fn every_classification_states_what_the_verifier_did_and_8psk_no_longer_lacks_a_clock() {
    const VERIFIER_REASONS: &[&str] = &[
        "verifier_confirmed",
        "verifier_reranked",
        SkipReason::Abstained.as_str(),
        SkipReason::NoClassCall.as_str(),
        SkipReason::SingleCandidate.as_str(),
        SkipReason::NoClockLock.as_str(),
        SkipReason::NoModel.as_str(),
        SkipReason::Geometry.as_str(),
        SkipReason::NoSymbolView.as_str(),
    ];
    let mut stated = 0;
    let mut no_clock = 0;
    let mut tally: BTreeMap<String, usize> = BTreeMap::new();
    let mut n = 0;
    for snr in SNRS {
        for k in 0..SEEDS {
            let s = generate(Class::Psk8, &SynthConfig::new(snr, seed(k)));
            let mut c14 = SymbolEstimator::new();
            let symbols = c14.from_samples(
                &s.symbol_samples,
                s.symbol_sample_rate_hz,
                Some(s.obw_hz),
                Some(snr),
            );
            let mut req = ClassifyRequest::new(&s.samples, s.sample_rate_hz, Timestamp::UNIX_EPOCH);
            req.obw_hz = Some(s.obw_hz);
            req.snr_db = Some(snr);
            req.symbols = symbols.as_ref();
            req.symbol_samples = Some(&s.symbol_samples);
            req.symbol_sample_rate_hz = Some(s.symbol_sample_rate_hz);
            let c = Classifier::new().classify(&req);
            n += 1;
            for r in &c.reasons {
                if VERIFIER_REASONS.contains(&r.as_str()) {
                    stated += 1;
                    *tally.entry(r.clone()).or_default() += 1;
                }
            }
            no_clock += usize::from(
                c.reasons
                    .iter()
                    .any(|r| r == SkipReason::NoClockLock.as_str()),
            );
        }
    }
    println!("T-589 shipped path on genuine 8-PSK, {n} classifications: {tally:?}");
    assert_eq!(n, 12);
    // No classification may be silent about the stage: one verifier outcome each, never zero and
    // never two.
    assert_eq!(
        stated, 12,
        "every classification must state exactly one verifier outcome: {tally:?}"
    );
    // The finding, as a count: twelve of twelve before, none now. What remains is
    // `verifier_single_candidate` — the tree's own call being decisive, which is the same gate
    // bpsk and qpsk stop at and is not a missing stage.
    assert_eq!(
        no_clock, 0,
        "genuine 8-PSK still reaches the verifier without a clock lock on {no_clock} of 12: \
         {tally:?}"
    );
}

/// **And the count that proves the stage is reachable at all**: with the tree left undecided
/// between two orders — the condition the verifier exists for — it now RUNS on genuine 8-PSK,
/// twelve times out of twelve, and confirms the truth.
///
/// This is T-246's own construction (a forced two-candidate prior, since a real tree call is
/// decisive enough to skip with [`SkipReason::SingleCandidate`]) extended to the class that could
/// not reach the stage. Unlike T-246's version it tolerates **no** [`SkipReason::NoClockLock`]:
/// that skip is the defect, and before the fix this ran 0 times.
#[test]
fn the_verifier_runs_on_genuine_8psk_when_the_tree_is_undecided() {
    let mut ran = 0;
    let mut offered = 0;
    let mut confirmed = 0;
    let mut skipped: Vec<String> = Vec::new();
    for snr in SNRS {
        for k in 0..SEEDS {
            let s = generate(Class::Psk8, &SynthConfig::new(snr, seed(k)));
            let mut c14 = SymbolEstimator::new();
            let symbols = c14.from_samples(
                &s.symbol_samples,
                s.symbol_sample_rate_hz,
                Some(s.obw_hz),
                Some(snr),
            );
            let mut req = ClassifyRequest::new(&s.samples, s.sample_rate_hz, Timestamp::UNIX_EPOCH);
            req.obw_hz = Some(s.obw_hz);
            req.snr_db = Some(snr);
            req.symbols = symbols.as_ref();
            let mut c = Classifier::new().classify(&req);
            if c.family != "psk-qam" || c.class.is_none() {
                // One snippet of the twelve (20 dB, seed 1000903) abstains at the family gate with
                // `low_confidence`. That is an honest upstream abstention and never becomes a
                // claim, so it is not offered to this stage at all — and it is counted out of the
                // denominator rather than quietly out of the numerator.
                continue;
            }
            offered += 1;
            // Level pegging between the truth and the order below it: nothing else changes, and
            // both are labels the tree's own family offers.
            if let Some(call) = c.class.as_mut() {
                call.dist = vec![
                    LabelP {
                        label: "qpsk".to_owned(),
                        p: 0.5,
                    },
                    LabelP {
                        label: "8psk".to_owned(),
                        p: 0.5,
                    },
                ];
                call.label = "qpsk".to_owned();
                call.p = 0.5;
            }
            let outcome = verify(
                &mut c,
                &VerifyInput {
                    samples: &s.symbol_samples,
                    sample_rate_hz: s.symbol_sample_rate_hz,
                    symbols: symbols.as_ref(),
                    snr_db: Some(snr),
                },
            );
            match &outcome {
                VerifyOutcome::Ran { to, .. } => {
                    ran += 1;
                    confirmed += usize::from(to == "8psk");
                }
                VerifyOutcome::Skipped(why) => skipped.push(format!("{snr} {k}: {why:?}")),
            }
        }
    }
    println!(
        "T-589: of 12 genuine 8-PSK snippets, {offered} reached the stage and the verifier RAN on          {ran} of them, choosing 8psk {confirmed} times"
    );
    assert!(
        skipped.is_empty(),
        "the stage must not skip on a class the system claims to handle: {skipped:#?}"
    );
    // Before the fix this ran 0 times: every one of the eleven skipped with `NoClockLock`.
    assert_eq!(
        offered, 11,
        "eleven of the twelve reach the psk-qam family; the twelfth abstains upstream"
    );
    assert_eq!(
        ran, offered,
        "a green run that never executed the stage is the failure this exists to prevent"
    );
    assert_eq!(
        confirmed, offered,
        "the ALRT, now actually reachable on 8-PSK, must pick the truth over the smaller alphabet"
    );
}
