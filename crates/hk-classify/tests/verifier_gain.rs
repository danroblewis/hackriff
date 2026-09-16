//! T-200: what the post-sync verifier is actually worth, measured on the confusable pairs
//! ADR-0016 §4.5 names — `2fsk` vs `gfsk`, and `bpsk` vs `qpsk`.
//!
//! **Blind.** Every waveform comes from the acceptance seed range (never fitted on), and the truth
//! label is used only to score the answer afterwards.
//!
//! The verifier re-ranks the **within-family class** and nothing else, so the family-level floors
//! (top-1, top-2, false-known, unknown recall, wrong-label) cannot move: that is asserted directly
//! in `family_level_answers_are_bit_identical_with_and_without_the_verifier`, which is a stronger
//! statement than re-measuring them. What can move is class accuracy, and this is where it is
//! measured, with the conditions stated rather than averaged away.

use hk_classify::synth::{ACCEPTANCE_SEED_BASE, Class, SynthConfig, generate};
use hk_classify::thresholds::thresholds_of;
use hk_classify::{Classifier, ClassifyRequest, SymbolEstimator};
use hk_model::Timestamp;
use hk_model::classify::Classification;

/// Trials per (class, SNR) cell.
const TRIALS: u64 = 12;

/// One blind classification, with the verifier either wired in or left out — the *only* difference
/// between the two arms, so any change measured here belongs to the verifier and to nothing else.
fn classify_one(
    c14: &mut SymbolEstimator,
    class: Class,
    snr_db: f64,
    seed: u64,
    verifier: bool,
) -> Classification {
    let s = generate(class, &SynthConfig::new(snr_db, seed));
    let symbols = c14.from_samples(
        &s.symbol_samples,
        s.symbol_sample_rate_hz,
        Some(s.obw_hz),
        Some(snr_db),
    );
    let mut req = ClassifyRequest::new(
        &s.samples,
        s.sample_rate_hz,
        Timestamp::from_unix_nanos(1_789_000_000_000_000_000),
    );
    req.obw_hz = Some(s.obw_hz);
    req.snr_db = Some(snr_db);
    req.symbols = symbols.as_ref();
    if verifier {
        req.symbol_samples = Some(&s.symbol_samples);
        req.symbol_sample_rate_hz = Some(s.symbol_sample_rate_hz);
    }
    Classifier::new().classify(&req)
}

fn gate_of(class: Class) -> f64 {
    class
        .family()
        .and_then(thresholds_of)
        .and_then(|t| t.snr_gate_db)
        .unwrap_or(10.0)
}

/// Class-level tally for one arm.
#[derive(Default, Clone, Copy)]
struct Tally {
    n: usize,
    right: usize,
    wrong: usize,
    abstained: usize,
    /// Wrong class calls reported at p >= 0.9: the confidently-wrong count, which may never grow.
    confident_wrong: usize,
}

impl Tally {
    fn rate(self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.right as f64 / self.n as f64
        }
    }
    fn fold(&mut self, other: &Tally) {
        self.n += other.n;
        self.right += other.right;
        self.wrong += other.wrong;
        self.abstained += other.abstained;
        self.confident_wrong += other.confident_wrong;
    }
}

/// Runs one pair of confusable classes at the stated SNR offsets, returning (off, on) tallies.
fn measure(pair: &[Class], offsets: &[f64], label: &str) -> (Tally, Tally) {
    let mut c14 = SymbolEstimator::new();
    let mut off = Tally::default();
    let mut on = Tally::default();
    for class in pair {
        let gate = gate_of(*class);
        for offset in offsets {
            let snr = gate + offset;
            let mut cell_off = Tally::default();
            let mut cell_on = Tally::default();
            // The same seeds in both arms: a paired comparison, not two independent samples.
            let base = ACCEPTANCE_SEED_BASE + 300_000 + 1_000 * (*class as u64);
            for trial in 0..TRIALS {
                let seed = base + trial;
                for (arm, tally) in [(false, &mut cell_off), (true, &mut cell_on)] {
                    let c = classify_one(&mut c14, *class, snr, seed, arm);
                    c.validate().expect("contract");
                    tally.n += 1;
                    match &c.class {
                        None => tally.abstained += 1,
                        Some(call) if call.label == class.label() => tally.right += 1,
                        Some(call) => {
                            tally.wrong += 1;
                            if call.p >= 0.9 {
                                tally.confident_wrong += 1;
                            }
                        }
                    }
                }
            }
            eprintln!(
                "[T-200] {label} {:<5} at {snr:>4.0} dB (gate{offset:+.0}): class top-1 {:.3} -> {:.3}  (n {}, abstained {} -> {}, confidently wrong {} -> {})",
                class.label(),
                cell_off.rate(),
                cell_on.rate(),
                cell_off.n,
                cell_off.abstained,
                cell_on.abstained,
                cell_off.confident_wrong,
                cell_on.confident_wrong,
            );
            off.fold(&cell_off);
            on.fold(&cell_on);
        }
    }
    eprintln!(
        "[T-200] {label} OVERALL: class top-1 {:.3} -> {:.3} over n {} (right {} -> {}, wrong {} -> {}, abstained {} -> {}, confidently wrong {} -> {})",
        off.rate(),
        on.rate(),
        off.n,
        off.right,
        on.right,
        off.wrong,
        on.wrong,
        off.abstained,
        on.abstained,
        off.confident_wrong,
        on.confident_wrong,
    );
    (off, on)
}

#[test]
fn the_verifier_earns_its_place_on_2fsk_versus_gfsk() {
    // FSK's S5 gate is 20 dB, so the stated conditions are 20, 25 and 30 dB in-band SNR.
    let (off, on) = measure(&[Class::Fsk2, Class::Gfsk], &[0.0, 5.0, 10.0], "2fsk/gfsk");
    assert!(off.n > 50, "too few samples to judge: {}", off.n);
    // A verifier that makes a confusable pair *worse* is not worth its complexity. This is a
    // regression guard at the measured level, never a floor that was loosened to let it pass.
    assert!(
        on.rate() >= off.rate() - 1e-9,
        "the verifier lowered 2fsk/gfsk class accuracy {:.3} -> {:.3}",
        off.rate(),
        on.rate()
    );
    // The property that outranks accuracy: abstention beats a confident misclassification.
    assert!(
        on.confident_wrong <= off.confident_wrong,
        "the verifier produced more confident wrong class calls ({} -> {})",
        off.confident_wrong,
        on.confident_wrong
    );
}

#[test]
fn the_verifier_earns_its_place_on_bpsk_versus_qpsk() {
    // PSK's S5 gate is 15 dB: the stated conditions are 15, 20 and 25 dB in-band SNR.
    let (off, on) = measure(&[Class::Bpsk, Class::Qpsk], &[0.0, 5.0, 10.0], "bpsk/qpsk");
    assert!(off.n > 50, "too few samples to judge: {}", off.n);
    assert!(
        on.rate() >= off.rate() - 1e-9,
        "the verifier lowered bpsk/qpsk class accuracy {:.3} -> {:.3}",
        off.rate(),
        on.rate()
    );
    assert!(
        on.confident_wrong <= off.confident_wrong,
        "the verifier produced more confident wrong class calls ({} -> {})",
        off.confident_wrong,
        on.confident_wrong
    );
}

/// Diagnostics, reported and never asserted: for each confusable class, what the tree offered, and
/// either why the verifier declined to run or what its likelihoods actually were. Without this a
/// "no gain" result is indistinguishable from "never ran".
#[test]
fn what_the_verifier_sees_on_each_confusable_class() {
    let mut c14 = SymbolEstimator::new();
    for class in [Class::Fsk2, Class::Gfsk, Class::Bpsk, Class::Qpsk] {
        let gate = gate_of(class);
        for offset in [0.0, 5.0, 10.0] {
            let snr = gate + offset;
            for trial in 0..3u64 {
                let seed = ACCEPTANCE_SEED_BASE + 500_000 + 100 * (class as u64) + trial;
                let s = generate(class, &SynthConfig::new(snr, seed));
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
                let mut c = Classifier::new().classify(&req);
                let tree = c
                    .class
                    .as_ref()
                    .map(|k| {
                        let mut d: Vec<String> = k
                            .dist
                            .iter()
                            .map(|lp| format!("{}={:.2}", lp.label, lp.p))
                            .collect();
                        d.sort();
                        format!("{} [{}]", k.label, d.join(" "))
                    })
                    .unwrap_or_else(|| "none".to_owned());
                let sps = symbols
                    .as_ref()
                    .and_then(|p| p.symbol_rate_bd.value())
                    .map(|r| s.symbol_sample_rate_hz / r);
                let outcome = hk_classify::verify(
                    &mut c,
                    &hk_classify::VerifyInput {
                        samples: &s.symbol_samples,
                        sample_rate_hz: s.symbol_sample_rate_hz,
                        symbols: symbols.as_ref(),
                        snr_db: Some(snr),
                    },
                );
                eprintln!(
                    "[T-200] {:<5} {snr:>4.0} dB family={:<8} trusted={} sps={:?} tree={tree} -> {outcome:?}",
                    class.label(),
                    c.family,
                    symbols.as_ref().is_some_and(|p| p.rate_trusted()),
                    sps.map(|v| (v * 10.0).round() / 10.0),
                );
            }
        }
    }
}

/// **Why every family-level floor is untouched, proved rather than re-measured.**
///
/// The verifier may only re-rank within-family classes. So across the whole taxonomy grid, the
/// family call and both distributions are bit-identical with and without it — which means top-1,
/// top-2, wrong-label, unknown recall and the false-known rate are all necessarily unchanged, since
/// every one of them is computed from exactly these fields.
#[test]
fn family_level_answers_are_bit_identical_with_and_without_the_verifier() {
    let mut c14 = SymbolEstimator::new();
    let mut seed = ACCEPTANCE_SEED_BASE + 400_000;
    let mut ran = 0usize;
    for class in Class::TAXONOMY.iter().chain(Class::HELD_OUT) {
        let gate = class
            .family()
            .and_then(thresholds_of)
            .and_then(|t| t.snr_gate_db)
            .unwrap_or(20.0);
        for offset in [-5.0, 0.0, 5.0, 10.0] {
            for _ in 0..2 {
                seed += 1;
                let snr = gate + offset;
                let off = classify_one(&mut c14, *class, snr, seed, false);
                let on = classify_one(&mut c14, *class, snr, seed, true);
                let what = class.label();
                assert_eq!(off.family, on.family, "{what} at {snr} dB");
                assert_eq!(off.confidence, on.confidence, "{what} at {snr} dB");
                assert_eq!(off.posterior, on.posterior, "{what} at {snr} dB");
                assert_eq!(off.likelihood, on.likelihood, "{what} at {snr} dB");
                assert_eq!(off.open_set_score, on.open_set_score, "{what} at {snr} dB");
                assert_eq!(off.entropy_norm, on.entropy_norm, "{what} at {snr} dB");
                assert_eq!(off.coarse, on.coarse, "{what} at {snr} dB");
                assert_eq!(off.flags, on.flags, "{what} at {snr} dB");
                // The row's arbitration stage is never promoted by a class-only re-rank.
                assert_eq!(off.stage, on.stage, "{what} at {snr} dB");
                if on
                    .class
                    .as_ref()
                    .is_some_and(|c| c.stage == hk_model::classify::Stage::Verifier)
                {
                    ran += 1;
                }
            }
        }
    }
    eprintln!("[T-200] the verifier ran on {ran} of the grid's classifications");
    assert!(
        ran > 0,
        "the verifier never ran: the measurement is vacuous"
    );
}
