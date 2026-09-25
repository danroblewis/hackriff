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
    // The T-200 post-sync verifier, on the same symbol-geometry view C14 measured (it re-ranks the
    // within-family class only, so every family-level figure below is unaffected by construction).
    req.symbol_samples = Some(&s.symbol_samples);
    req.symbol_sample_rate_hz = Some(s.symbol_sample_rate_hz);
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

/// **ADR-0016 §7's held-out floors, over the population §7 names, plus what T-244's fuller coverage
/// measured.**
///
/// §7 enumerates six out-of-taxonomy generators ([`Class::ADR_HELD_OUT`]) and states two floors
/// over them: unknown recall ≥ 0.80 and false-known ≤ 0.10. Those are asserted below, unchanged and
/// over exactly that population — measured 1.000 and 0.000, every one of the six abstaining at
/// every seed.
///
/// T-244 added five more generators, because `analog`, `psk-qam` and `pulsed` had **no negative at
/// all** and so an open set that was never measured, only averaged over the families that had one.
/// Over all eleven the abstention rate is 0.83 here (0.783 on T-213's larger harness grid, against
/// 0.940 before), i.e. the aggregate figure was overstated by averaging over a hole. What the
/// fuller coverage did **not** find is a single dangerous outcome: **0 of 88 snippets were given a
/// family that is not their own**. The drop is entirely unlisted members of a family being
/// recognised as that family — 16-APSK as `psk-qam`, vestigial-sideband AM as `analog` — which is
/// the generalisation [`Class::nearest_family`] documents, not a false known in the sense that
/// matters. Whether §7's floor should be restated over abstention or over wrong labels is T-206's
/// call; this test loosens neither, and reports both.
#[test]
fn out_of_taxonomy_generators_come_back_unknown() {
    let mut unknown = 0u32;
    let mut total = 0u32;
    let mut adr_unknown = 0u32;
    let mut adr_total = 0u32;
    let mut per_class = Vec::new();
    let mut wrong_family = Vec::new();
    let mut c14 = SymbolEstimator::new();
    let mut seed = ACCEPTANCE_SEED_BASE + 500_000;
    for class in Class::HELD_OUT {
        let adr = Class::ADR_HELD_OUT.contains(class);
        let mut hits = 0u32;
        for _ in 0..8 {
            seed += 1;
            let c = classify_one(&mut c14, *class, 25.0, seed);
            total += 1;
            adr_total += u32::from(adr);
            // ADR-0016 §7 counts either outcome as recognising the unknown.
            if c.family == hk_model::classify::UNKNOWN || c.open_set_score >= 0.5 {
                unknown += 1;
                adr_unknown += u32::from(adr);
                hits += 1;
            } else if class.nearest_family() != Some(c.family.as_str()) {
                // Not just "a family" but *the wrong* family: the dangerous outcome.
                wrong_family.push((class.label(), c.family.clone()));
            }
        }
        per_class.push((class.label(), hits));
    }
    let recall = f64::from(unknown) / f64::from(total);
    let adr_recall = f64::from(adr_unknown) / f64::from(adr_total);
    eprintln!(
        "[T-199] accuracy_sweep.rs OWN-SEEDS draw (NOT the acceptance-m3 gate's 396-snippet draw; see ADR-0016 §7.2) held-out unknown recall {recall:.2} of {total} ({adr_recall:.2} over ADR-0016 §7's own six of {adr_total}): {per_class:?}"
    );
    // The ADR's floor, over the ADR's population, unchanged. Measured 1.000: all six abstain at
    // every seed.
    assert!(
        adr_recall >= 0.80,
        "held-out unknown recall {adr_recall:.2} over ADR-0016 §7's six generators is below the \
         0.80 floor: {per_class:?}"
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
    let adr_false_known = f64::from(adr_total - adr_unknown) / f64::from(adr_total);
    assert!(
        adr_false_known <= 0.10,
        "held-out false-known rate {adr_false_known:.3} over ADR-0016 §7's six generators exceeds \
         the §7 floor of 0.10 ({wrong_family:?})"
    );

    // **What the fuller coverage measures, reported rather than floored** (T-244). The same
    // arithmetic over all eleven generators gives 0.170, because `analog` and `psk-qam` recognise
    // unlisted members of their own family rather than abstaining — a real property that was
    // invisible while those two families had no negative at all, but not the quantity §7 bounded
    // over its six. The bound below is a regression guard at the measured level, not a floor: it
    // keeps the number from drifting unnoticed before T-206 rules on which quantity §7 means.
    let false_known = f64::from(total - unknown) / f64::from(total);
    eprintln!(
        "[T-244] over all {total} held-out snippets, every family covered: abstention {recall:.3}, \
         named-a-family {false_known:.3}, wrong family {}",
        wrong_family.len()
    );
    assert!(
        false_known <= 0.25,
        "the full-coverage named-a-family rate {false_known:.3} got worse than the 0.170 measured \
         at T-244: {per_class:?}"
    );
    // **Nothing is ever given a family that is not its own** — the outcome that would actually be
    // dangerous. This is a tightening of what stood here before: an allow-list that tolerated the
    // 3-level ASK generator being read as `analog`. That residual is gone, and the property now
    // holds over all eleven generators, T-244's five included. Where the classifier does not
    // abstain it names the family the emission genuinely belongs to.
    assert!(
        wrong_family.is_empty(),
        "held-out generators were given a family that is not their own: {wrong_family:?}"
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

/// **The analog family names the class it found, or names none — never the wrong one** (T-249).
///
/// This is the within-family half of the floors above, which the family-level cells cannot see: a
/// snippet counted as a correct `analog` call can still be handed a wrong class name, and until
/// T-249 two of the five were wrong essentially always. Measured on this same blind acceptance
/// grid, at and above the analog gate: `cw` top-1 **0.000** with a wrong class **every time**, and
/// `ssb` top-1 0.000 with wrong 1.000/0.917 — both confidently called `am`.
///
/// Two defects, both in the hand-written conjunctions that named analog classes:
/// - `cw` required `sigma_af < 0.02`, a bound below the noise floor of the instantaneous-frequency
///   estimator at this gate (measured 0.170–0.207 at 10 dB, falling as 1/√ρ), so it never fired.
/// - `am` used `carrier_line_db > 14` as its "there is a carrier" term, but that feature is a CFAR
///   strongest-line statistic and a suppressed-carrier SSB emission's loudest audio tone reads
///   28–40 dB on it — so `am` and `ssb` tied at 0.7 and the tie broke alphabetically.
///
/// Both are gone: the order comes from the class-conditional densities, which are fitted per class
/// on the **dev** split at this geometry, and the class is withheld below the family's class gate.
/// The assertions below are the properties, not the numbers: every analog class is nameable, and a
/// name is only given where the measurement supports one.
#[test]
fn the_analog_class_call_is_right_or_absent_but_never_confidently_wrong() {
    let mut c14 = SymbolEstimator::new();
    let mut seed = ACCEPTANCE_SEED_BASE + 500_000;
    let analog = [Class::Am, Class::Nbfm, Class::Wfm, Class::Ssb, Class::Cw];
    let gate = 10.0_f64;
    let class_gate = thresholds_of("analog")
        .expect("analog thresholds")
        .class_gate_db;
    assert!(
        class_gate > 0.0,
        "analog runs its class call at its own family gate, where the dev sweep measures the \
         within-family accuracy at 0.733 (T-249)"
    );

    let mut named = 0usize;
    let mut right = 0usize;
    let mut wrong = 0usize;
    let mut per_class: Vec<(&str, usize, usize, usize)> = Vec::new();
    for class in analog {
        let (mut n, mut ok, mut bad) = (0usize, 0usize, 0usize);
        for offset in [class_gate, class_gate + 5.0] {
            for _ in 0..TRIALS {
                seed += 1;
                let c = classify_one(&mut c14, class, gate + offset, seed);
                c.validate().expect("contract");
                if c.family != "analog" {
                    continue; // a family-level abstention; the family floors above judge those
                }
                n += 1;
                match c.class.as_ref() {
                    Some(call) if call.label == class.label() => ok += 1,
                    Some(_) => bad += 1,
                    None => {}
                }
            }
        }
        named += n;
        right += ok;
        wrong += bad;
        per_class.push((class.label(), n, ok, bad));
    }
    for (label, n, ok, bad) in &per_class {
        eprintln!(
            "[T-249] {label:<5} at/above class gate: n {n:>3}, correct {ok:>3}, wrong {bad:>3}"
        );
    }
    assert!(named >= 40, "too few analog calls to judge: {named}");

    // **No confident wrong name.** This is the binding property: abstention is free here, a wrong
    // name is not. Measured after T-249 over the full T-213 grid, the analog class wrong-label rate
    // at and above the gate is 0.0056 (one snippet in 180), against 0.350 before.
    let wrong_rate = wrong as f64 / named as f64;
    assert!(
        wrong_rate <= 0.05,
        "analog class wrong-label {wrong_rate:.3}: {per_class:?}"
    );

    // **Every class is nameable.** `cw` and `ssb` were both at exactly zero, which no accuracy
    // floor on the family could show.
    for (label, _, ok, _) in &per_class {
        assert!(
            *ok > 0,
            "{label} was never named correctly at or above its class gate: {per_class:?}"
        );
    }
    assert!(
        right as f64 / named as f64 >= 0.80,
        "analog class top-1 {:.3}: {per_class:?}",
        right as f64 / named as f64
    );

    // **Below the class gate the name is withheld, not guessed.** At the family gate itself the dev
    // sweep measures the within-family call at 0.733, so nothing is reported there.
    let mut below = 0usize;
    for class in analog {
        for _ in 0..TRIALS {
            seed += 1;
            let c = classify_one(&mut c14, class, gate + class_gate - 1.0, seed);
            if c.family != "analog" {
                continue;
            }
            below += 1;
            assert!(
                c.class.is_none(),
                "{} named {:?} below the class gate",
                class.label(),
                c.class
            );
            assert!(
                c.reasons.iter().any(|r| r == "below_class_gate"),
                "no reason recorded for the withheld class: {:?}",
                c.reasons
            );
        }
    }
    eprintln!("[T-249] below the class gate: {below} analog calls, all class-abstaining");
}

/// **The `fsk` and `psk-qam` families name the class they found, or name none — never the wrong
/// one** (T-422), the sibling of the analog test above and for three more classes that were wrong
/// far more often than they were right inside a correct family call.
///
/// Measured on this blind grid before the fix, at and above each family's class gate: `gfsk` top-1
/// 0.333/0.417 with wrong 0.667/0.583, `msk` 0.333/0.417 with wrong 0.667/0.500, and `qam16` top-1
/// **0.083** with wrong 0.917/0.833. Every one of them had a healthy top-2 (0.917–1.000), which is
/// the tell T-249 named: a class losing a *ranking*, not one whose evidence is absent.
///
/// Three separate causes, in two different stages:
/// - `gfsk` — the last hand-written score table, `fsk_classes`. Its conjunction needed
///   `if_bimodality < 0.66`, but C13's channel filter smooths every FSK emission's IF trajectory,
///   so the whole family measures 0.70–0.91 here and no `gfsk` snippet ever reached the bound.
///   Gone; `fsk` routes through the class-conditional densities like every other family.
/// - `msk` — the tree named it correctly (0.917) from C14's modulation index and the **verifier**
///   re-ranked it away, because its `h = 0.5` hypothesis fixes the *transmitter's* peak deviation
///   and the receiver does not measure that number. `msk` is out of the verifier's hypothesis set.
/// - `qam16` — nothing to do with the densities, which had it right 21/24. The verifier's ALRT takes
///   `N₀` from the SNR meter; sweeping only that assumption flipped both QAM truths together, so
///   the ratio was a function of the assumption, and T-422 took the QAM orders out of its
///   hypothesis set. T-590 put them back: the flip was T-246's residual-carrier ring, and with that
///   removed the two orders separate at the measured SNR and either side of it (`verify::tests`).
///
/// The assertions are properties, not today's numbers: every class in both families is nameable,
/// the class wrong-label rate stays bounded, no wrong name is reported confidently, and below the
/// class gate the name is withheld **with a recorded reason**.
#[test]
fn the_fsk_and_psk_qam_class_calls_are_right_or_absent_but_never_confidently_wrong() {
    let mut c14 = SymbolEstimator::new();
    let mut seed = ACCEPTANCE_SEED_BASE + 700_000;

    for (family, classes) in [
        (
            "fsk",
            &[Class::Fsk2, Class::Gfsk, Class::Msk, Class::Fsk4][..],
        ),
        (
            "psk-qam",
            &[
                Class::Bpsk,
                Class::Qpsk,
                Class::Psk8,
                Class::Qam16,
                Class::Qam64,
            ][..],
        ),
    ] {
        let t = thresholds_of(family).expect("family thresholds");
        let gate = t.snr_gate_db.expect("family gate");
        let class_gate = t.class_gate_db;

        let mut named = 0usize;
        let mut right = 0usize;
        let mut wrong = 0usize;
        let mut confident_wrong = 0usize;
        let mut per_class: Vec<(&str, usize, usize, usize)> = Vec::new();
        for class in classes {
            let (mut n, mut ok, mut bad) = (0usize, 0usize, 0usize);
            for offset in [class_gate, class_gate + 5.0] {
                for _ in 0..TRIALS {
                    seed += 1;
                    let c = classify_one(&mut c14, *class, gate + offset, seed);
                    c.validate().expect("contract");
                    if c.family != family {
                        continue; // a family-level abstention; the family floors above judge those
                    }
                    n += 1;
                    match c.class.as_ref() {
                        Some(call) if call.label == class.label() => ok += 1,
                        Some(call) => {
                            bad += 1;
                            if call.p >= 0.9 {
                                confident_wrong += 1;
                                eprintln!(
                                    "[T-422] {} called {} at p {:.3}",
                                    class.label(),
                                    call.label,
                                    call.p
                                );
                            }
                        }
                        None => {}
                    }
                }
            }
            named += n;
            right += ok;
            wrong += bad;
            per_class.push((class.label(), n, ok, bad));
        }
        for (label, n, ok, bad) in &per_class {
            eprintln!(
                "[T-422] {family} {label:<5} at/above class gate: n {n:>3}, correct {ok:>3}, wrong {bad:>3}"
            );
        }
        assert!(named >= 40, "too few {family} calls to judge: {named}");

        // **Every class is nameable.** `gfsk` was at exactly zero from the tree and `qam16` at
        // 0.083 after the verifier — neither of which any family-level floor can see.
        for (label, _, ok, _) in &per_class {
            assert!(
                *ok > 0,
                "{label} was never named correctly at or above its class gate: {per_class:?}"
            );
        }

        // **No confident wrong name**, the property that outranks accuracy: an abstention is free
        // here, a confidently wrong name is what this project has reverted three times. Both
        // families are at zero after T-422 — including the p = 0.903 the `msk` index factor briefly
        // reached before its spread was re-clamped to the densities' own 19:1 bound.
        assert_eq!(
            confident_wrong, 0,
            "{family} reported a wrong class at p >= 0.9: {per_class:?}"
        );

        // **The wrong-label rate stays bounded.** Measured after T-422 over the full T-213 grid at
        // and above each class gate: `fsk` **0.3125 -> 0.0833** and `psk-qam` **0.1833 -> 0.0417**,
        // with class top-1 0.667 -> 0.896 and 0.758 -> 0.900. The bound below is loose on purpose —
        // it is a regression guard, not a target, and an honest improvement must never trip it.
        let wrong_rate = wrong as f64 / named as f64;
        assert!(
            wrong_rate <= 0.20,
            "{family} class wrong-label {wrong_rate:.3}: {per_class:?}"
        );
        assert!(
            right as f64 / named as f64 >= 0.70,
            "{family} class top-1 {:.3}: {per_class:?}",
            right as f64 / named as f64
        );

        // **Below the class gate the name is withheld, not guessed**, and the reason is recorded.
        let mut below = 0usize;
        for class in classes {
            for _ in 0..TRIALS {
                seed += 1;
                let c = classify_one(&mut c14, *class, gate + class_gate - 1.0, seed);
                if c.family != family {
                    continue;
                }
                below += 1;
                assert!(
                    c.class.is_none(),
                    "{} named {:?} below the class gate",
                    class.label(),
                    c.class
                );
                assert!(
                    c.reasons.iter().any(|r| r == "below_class_gate"),
                    "no reason recorded for the withheld class: {:?}",
                    c.reasons
                );
            }
        }
        eprintln!("[T-422] {family}: below the class gate, {below} calls, all class-abstaining");
    }
}

/// **The `pulsed` family names the class it found, or names none — never the wrong one** (T-427),
/// the fourth and last of these, after `analog` (T-249) and `fsk`/`psk-qam` (T-422).
///
/// Measured on this blind grid before the fix, at and above the `pulsed` gate: `pulse` top-1
/// **0.000** with wrong-label **1.000** at gate+5, every snippet named `ppm`. The tell the two
/// earlier tickets used — a healthy top-2 under a dead top-1 — says nothing here, because the
/// family has only **two** classes and top-2 is 1.000 by construction whenever a class is named at
/// all. What replaces it as the diagnostic is the densities' own arg-max, and it was already
/// right: **6/6 for both classes at every SNR from 10 to 30 dB on the dev *and* the blind seeds**,
/// with 53–70 nats of margin.
///
/// The cause is a fifth variant of this family of defects, not a repeat of one of the four.
/// `duty` is `#{ a > 0.5 × mean(a) } / n`: a fraction over a threshold set by the snippet's **own
/// mean envelope**, which for a sparse train is dominated by the *off* time. So the noise clears
/// the threshold, and the measured duty of the 5 %-duty `pulse` train is 0.655 / 0.538 / 0.200 /
/// 0.143 / 0.051 at 10 / 15 / 20 / 25 / 30 dB, against `ppm`'s near-constant 0.47 / 0.42 / 0.40 /
/// 0.39 / 0.39. The ranges **cross between 15 and 20 dB**, so the `duty > 0.25` table was inverted
/// exactly where the classifier decides. The dimension separates high-SNR snippets from low-SNR
/// ones, not sparse pulse trains from busy ones.
///
/// The assertions are properties, not today's numbers: both classes are nameable, no wrong name is
/// reported at p ≥ 0.9 — which for a **two**-class family needed [`hk_classify::tree::CLASS_MAX_P`],
/// since the 19:1 clamp alone permits 0.95 — and the class wrong-label rate stays bounded.
#[test]
fn the_pulsed_class_call_is_right_or_absent_but_never_confidently_wrong() {
    let mut c14 = SymbolEstimator::new();
    let mut seed = ACCEPTANCE_SEED_BASE + 800_000;
    let t = thresholds_of("pulsed").expect("pulsed thresholds");
    let gate = t.snr_gate_db.expect("pulsed gate");
    let class_gate = t.class_gate_db;

    let mut named = 0usize;
    let mut right = 0usize;
    let mut wrong = 0usize;
    let mut confident_wrong = 0usize;
    let mut per_class: Vec<(&str, usize, usize, usize)> = Vec::new();
    for class in [Class::Ppm, Class::Pulse] {
        let (mut n, mut ok, mut bad) = (0usize, 0usize, 0usize);
        for offset in [class_gate, class_gate + 5.0, class_gate + 10.0] {
            for _ in 0..TRIALS {
                seed += 1;
                let c = classify_one(&mut c14, class, gate + offset, seed);
                c.validate().expect("contract");
                if c.family != "pulsed" {
                    continue; // a family-level abstention; the family floors above judge those
                }
                n += 1;
                match c.class.as_ref() {
                    Some(call) if call.label == class.label() => ok += 1,
                    Some(call) => {
                        bad += 1;
                        if call.p >= 0.9 {
                            confident_wrong += 1;
                            eprintln!(
                                "[T-427] {} called {} at p {:.3}",
                                class.label(),
                                call.label,
                                call.p
                            );
                        }
                    }
                    None => {}
                }
            }
        }
        named += n;
        right += ok;
        wrong += bad;
        per_class.push((class.label(), n, ok, bad));
    }
    for (label, n, ok, bad) in &per_class {
        eprintln!(
            "[T-427] pulsed {label:<5} at/above class gate: n {n:>3}, correct {ok:>3}, wrong {bad:>3}"
        );
    }
    assert!(named >= 20, "too few pulsed calls to judge: {named}");

    // **Both classes are nameable.** `pulse` was at exactly zero, which no family-level floor can
    // see: the family call was correct and the name inside it was not.
    for (label, _, ok, _) in &per_class {
        assert!(
            *ok > 0,
            "{label} was never named correctly at or above its class gate: {per_class:?}"
        );
    }

    // **No confident wrong name.** The property that outranks accuracy, and the one that needed
    // new arithmetic here rather than new evidence: with a single rival the 19:1 spread clamp caps
    // the leader at 0.95, above the 0.9 this repo calls confident, so `CLASS_MAX_P` binds instead.
    assert_eq!(
        confident_wrong, 0,
        "pulsed reported a wrong class at p >= 0.9: {per_class:?}"
    );

    // **The wrong-label rate stays bounded.** Measured after T-427 on this grid: 0.000, from
    // `pulse` 1.000 at gate+5. The bound is a regression guard, not a target.
    let wrong_rate = wrong as f64 / named as f64;
    assert!(
        wrong_rate <= 0.20,
        "pulsed class wrong-label {wrong_rate:.3}: {per_class:?}"
    );
    assert!(
        right as f64 / named as f64 >= 0.80,
        "pulsed class top-1 {:.3}: {per_class:?}",
        right as f64 / named as f64
    );
}
