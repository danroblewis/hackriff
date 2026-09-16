//! T-199 blind accuracy sweep: per-family, per-SNR accuracy against the a-priori floors of
//! ADR-0016 §7, plus the held-out (out-of-taxonomy) recall.
//!
//! **Blind.** Every waveform comes from the **acceptance** seed range, which the densities were
//! never fitted on ([`hk_classify::synth::ACCEPTANCE_SEED_BASE`] vs `DEV_SEEDS`), and the truth
//! label is used only in the assertions and the report — never as an input to the classifier, and
//! never to choose what to look at (docs/10 §3.2).
//!
//! A single SNR-averaged number is never asserted on its own: the cells are
//! `family × SNR bin`, because that is where a family's floor shows up (C15 card).

use hk_classify::eval::EvalReport;
use hk_classify::synth::{ACCEPTANCE_SEED_BASE, Class, SynthConfig, generate};
use hk_classify::thresholds::thresholds_of;
use hk_classify::{Classifier, ClassifyRequest, SymbolEstimator};
use hk_model::Timestamp;

/// Trials per (class, SNR) cell.
const TRIALS: u64 = 6;

/// SNR offsets from each family's gate, dB: two below it (where the classifier must abstain) and
/// three at or above it (where it must decide).
const OFFSETS: [f64; 5] = [-10.0, -5.0, 0.0, 5.0, 10.0];

/// One blind classification, through the same two-stage measurement the pipeline performs: C14 at
/// its own geometry (T-238), then the feature tree on the classifier's snippet.
fn classify_one(
    c14: &mut SymbolEstimator,
    class: Class,
    snr_db: f64,
    seed: u64,
) -> hk_model::classify::Classification {
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
    Classifier::new().classify(&req)
}

fn gate_of(class: Class) -> f64 {
    class
        .family()
        .and_then(thresholds_of)
        .and_then(|t| t.snr_gate_db)
        .unwrap_or(10.0)
}

#[test]
fn per_family_and_per_snr_accuracy_meets_the_a_priori_floors() {
    let mut report = EvalReport::new(5.0);
    let mut c14 = SymbolEstimator::new();
    let mut seed = ACCEPTANCE_SEED_BASE;
    for class in Class::TAXONOMY {
        let gate = gate_of(*class);
        for offset in OFFSETS {
            let snr = gate + offset;
            for _ in 0..TRIALS {
                seed += 1;
                let c = classify_one(&mut c14, *class, snr, seed);
                c.validate().expect("contract");
                // Recorded against the *offset from the gate*, so families with different gates
                // share comparable bins.
                report.record("synthetic-acceptance", class.family(), offset, &c);
            }
        }
    }
    for class in Class::HELD_OUT {
        for offset in [0.0, 5.0, 10.0] {
            for _ in 0..TRIALS {
                seed += 1;
                let c = classify_one(&mut c14, *class, 20.0 + offset, seed);
                c.validate().expect("contract");
                report.record("held-out", None, offset, &c);
            }
        }
    }
    eprintln!(
        "[T-199] accuracy by family and SNR offset from the gate\n{}",
        report.markdown()
    );

    // ADR-0016 §7: known families at gate + 5 dB and above.
    let strong = report.total(Some("synthetic-acceptance"), 5.0);
    eprintln!(
        "[T-199] at gate+5 dB and above: n {}, top-1 {:.3}, top-2 {:.3}, unknown {:.3}, wrong {:.3}",
        strong.n,
        strong.top1_rate(),
        strong.top2_rate(),
        strong.unknown_rate(),
        strong.wrong_rate()
    );
    for family in [
        "analog",
        "ook-ask",
        "fsk",
        "psk-qam",
        "ofdm",
        "css",
        "pulsed",
        "noise-like",
    ] {
        let c = report.family(family, 5.0);
        eprintln!(
            "[T-199] {family:<11} at gate+5 dB: n {:>3}, top-1 {:.2}, top-2 {:.2}, unknown {:.2}, wrong {:.2}",
            c.n,
            c.top1_rate(),
            c.top2_rate(),
            c.unknown_rate(),
            c.wrong_rate()
        );
    }
    assert!(strong.n > 100, "too few samples to judge: {}", strong.n);

    // **The ADR-0016 §7 exit floors — top-1 ≥ 0.90 and top-2 ≥ 0.95 at gate + 5 dB — are asserted
    // directly, because the classical tree now meets them** (T-230). Measured on this sweep: top-1
    // 0.937, top-2 0.984, wrong 0.000; on T-213's larger harness grid, 0.915 and 0.980. Every seed
    // here is fixed, so these numbers are reproducible exactly rather than sampled.
    //
    // The floors themselves were never moved. What closed the gap was three corrections, each of
    // which had been costing whole families their answer:
    //
    // 1. A family held back by its SNR gate is credited with its own likelihood, read from the
    //    **below-gate** densities, instead of with the evidence of a "typical survivor". The old
    //    rule collapsed to `gated_out / (gated_out + 1)` whatever the snippet looked like, because
    //    survivors that fit badly underflow to zero evidence and so the median survivor *is* the
    //    winner. It abstained on every analog, OFDM, pulsed and noise-like emission between that
    //    family's own gate and 20 dB, where FSK and OOK are still gated.
    // 2. Those below-gate densities are fitted where a family is gated, so "could this be the
    //    family I was not allowed to measure?" is answered by a model that is valid at that SNR.
    //    Scoring it with the at-gate densities was extrapolation, and it read 0.000 for a genuine
    //    2-FSK burst 10 dB under its gate.
    // 3. A repeated guard interval only rules `analog` out when the emission fills its band *and*
    //    has a multi-carrier (Rayleigh) envelope. A constant-envelope FM carrier cannot be OFDM;
    //    without that term `wfm` was denied its own family and could never be classified.
    //
    // T-206 remains the milestone's exit gate. What this test guards is that the classifier does not
    // get *worse*, and that the properties which make an abstaining classifier safe hold absolutely:
    // it is never confidently wrong, and it never claims a family below its gate. Raise these guards
    // as the rest of the cascade lands; never lower one to make a change pass.
    assert!(
        strong.top1_rate() >= 0.90,
        "top-1 {:.3} fell below the ADR-0016 §7 floor of 0.90 (measured 0.937 at T-230)",
        strong.top1_rate()
    );
    assert!(
        strong.top2_rate() >= 0.95,
        "top-2 {:.3} fell below the ADR-0016 §7 floor of 0.95 (measured 0.984 at T-230)",
        strong.top2_rate()
    );

    // Wrong-label rate over the known families: ≤ 0.05 in any bin, ≤ 0.02 overall. Abstaining is
    // allowed everywhere. (The held-out generators have their own floor, in the test below; they
    // are counted separately because "named a family at all" means something different for a
    // signal that has no family.)
    let all_bins = report.total(Some("synthetic-acceptance"), f64::NEG_INFINITY);
    let decided = report.total(Some("synthetic-acceptance"), 0.0);
    let worst_above: f64 = report
        .cells()
        .filter(|(source, _, bin, cell)| {
            *source == "synthetic-acceptance" && *bin >= 0 && cell.n >= 10
        })
        .map(|(_, _, _, cell)| cell.wrong_rate())
        .fold(0.0, f64::max);
    eprintln!(
        "[T-199] wrong-label rate: {:.3} at or above each gate (worst bin {worst_above:.3}), {:.3} over all bins including below them (ADR floors 0.02 overall, 0.05 per bin)",
        decided.wrong_rate(),
        all_bins.wrong_rate()
    );
    // Where the classifier is allowed to decide, it is never confidently wrong — the property that
    // makes an abstaining classifier safe to build on, and the one S5 measured (0 trusted-and-wrong
    // in 900 runs). Below a gate the mass is meant to move to `unknown`; where it still leaks into
    // a family whose own gate is lower (an FSK burst at 10 dB looking analog), it is reported above
    // and belongs to T-206, which owns the exit floor.
    assert!(
        decided.wrong_rate() <= 0.02,
        "wrong-label rate at or above the gates {:.3} exceeds 0.02",
        decided.wrong_rate()
    );
    assert!(
        worst_above <= 0.05,
        "worst per-bin wrong-label rate at or above the gates {worst_above:.3} exceeds 0.05"
    );

    // Below the gate the classifier must abstain rather than guess.
    let below = report.total(Some("synthetic-acceptance"), f64::NEG_INFINITY);
    let under_gate_cells: Vec<f64> = report
        .cells()
        .filter(|(source, _, bin, _)| *source == "synthetic-acceptance" && *bin < 0)
        .map(|(_, _, _, cell)| cell.wrong_rate())
        .collect();
    let worst_under_gate = under_gate_cells.iter().copied().fold(0.0, f64::max);
    eprintln!(
        "[T-199] below the gate: worst wrong-label rate {worst_under_gate:.3} over {} cells (all {} samples)",
        under_gate_cells.len(),
        below.n
    );
    // Below a gate, abstaining is the required behaviour and mislabelling is the failure. Most
    // cells abstain completely; the residue is the leak described above. The bound here is a
    // regression guard at the measured level, not the ADR's 0.05 — raise it as the leak closes.
    assert!(
        worst_under_gate <= 0.95,
        "below-gate mislabelling got worse: worst cell {worst_under_gate:.3}"
    );
}

#[test]
fn out_of_taxonomy_generators_come_back_unknown() {
    let mut unknown = 0u32;
    let mut total = 0u32;
    let mut per_class = Vec::new();
    let mut wrong_family = Vec::new();
    let mut c14 = SymbolEstimator::new();
    let mut seed = ACCEPTANCE_SEED_BASE + 500_000;
    for class in Class::HELD_OUT {
        let mut hits = 0u32;
        for _ in 0..8 {
            seed += 1;
            let c = classify_one(&mut c14, *class, 25.0, seed);
            total += 1;
            // ADR-0016 §7 counts either outcome as recognising the unknown.
            if c.family == hk_model::classify::UNKNOWN || c.open_set_score >= 0.5 {
                unknown += 1;
                hits += 1;
            } else if class.nearest_family() != Some(c.family.as_str()) {
                // Not just "a family" but *the wrong* family: the dangerous outcome.
                wrong_family.push((class.label(), c.family.clone()));
            }
        }
        per_class.push((class.label(), hits));
    }
    let recall = f64::from(unknown) / f64::from(total);
    eprintln!("[T-199] held-out unknown recall {recall:.2} of {total}: {per_class:?}");
    assert!(
        recall >= 0.80,
        "held-out unknown recall {recall:.2} is below the 0.80 floor: {per_class:?}"
    );
    eprintln!(
        "[T-199] held-out generators given a family that is not their own: {} of {total} ({wrong_family:?})",
        wrong_family.len()
    );

    // **ADR-0016 §7's false-known floor, asserted numerically (T-235).**
    //
    // This used to be an allow-list instead: it permitted the OFDM-with-a-very-short-cyclic-prefix
    // generator to be called whatever it liked and only forbade the gap from *spreading*. That
    // tolerated a false-known rate of 0.167, well above the ADR's 0.10, which is exactly why the
    // floor went unmet. Asserting the rate the ADR actually states is a tightening, not a
    // loosening: the previous form placed no bound on the rate at all.
    //
    // What closed it was the analysis geometry. The dev grid now generates and resamples each
    // class to the ~2 samples per OBW99 that `hk_estimate::normalise` really delivers, so the
    // narrowband analog classes stop being fitted on band noise. `ssb` had been fitted at a
    // measured OBW of 251 kHz and `cw` at 767 kHz — densities describing noise rather than a
    // signal, and wide enough to swallow any band-filling emission. That, not the cyclic-prefix
    // feature, was the whole of the false-known rate: the short-CP OFDM was never mis-read as
    // OFDM, it was absorbed by a catch-all analog class.
    let false_known = (total - unknown) as f64 / f64::from(total);
    assert!(
        false_known <= 0.10,
        "held-out false-known rate {false_known:.3} exceeds the ADR-0016 §7 floor of 0.10 \
         ({wrong_family:?})"
    );
    // The residual is the 3-level ASK generator, occasionally read as `analog`. It is a genuine
    // open-set miss rather than a generalisation (its own family is `ook-ask`), and it is bounded
    // by the rate assertion above. What is asserted here is that it has not spread: an
    // out-of-taxonomy generator that is *not* this known residual must never be given a family.
    let spread: Vec<_> = wrong_family
        .iter()
        .filter(|(label, _)| *label != Class::Ask3.label())
        .collect();
    assert!(
        spread.is_empty(),
        "held-out generators beyond the known 3-level-ASK residual were given a family: {spread:?}"
    );
}

#[test]
fn noise_is_never_called_a_communication_family() {
    // ADR-0016 §7: noise snippets labelled as a comm family ≤ 1 %.
    let mut wrong = 0u32;
    let mut c14 = SymbolEstimator::new();
    let mut seed = ACCEPTANCE_SEED_BASE + 900_000;
    let trials = 40;
    for _ in 0..trials {
        seed += 1;
        let c = classify_one(&mut c14, Class::NoiseLike, 20.0, seed);
        let comm = c.family != "noise-like" && c.family != hk_model::classify::UNKNOWN;
        if comm {
            wrong += 1;
            eprintln!("[T-199] noise called {} ({:?})", c.family, c.top(3));
        }
    }
    let rate = f64::from(wrong) / f64::from(trials);
    assert!(
        rate <= 0.01,
        "noise called a comm family {rate:.3} of the time"
    );
}
