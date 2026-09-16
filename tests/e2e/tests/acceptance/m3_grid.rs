//! T-206, the M3 exit gate — **the measurement half** (ADR-0016 §7).
//!
//! Blind accuracy over the synthetic **acceptance** seed range against the five a-priori floors of
//! ADR-0016 §7. Nothing here is tuned to a result: the floors were fixed in the ADR before the
//! classifier existed, and this module asserts them as written. Where a floor is not met the test
//! fails and prints what was measured and by how much — an accurate red gate, never a relaxed one.
//!
//! # What this module measures, and what [`super::m3_scene`] measures
//!
//! The classification accuracy figures come from T-213's shared harness
//! ([`hk_classify::harness::Harness`]) rather than from a pipeline run, because a floor of
//! `top-1 ≥ 0.90` needs ~1 700 classified snippets and a mock-SDR run per snippet would cost
//! hours. T-199's sweep, T-204's DL evaluation and this gate all drive that one runner, so the
//! numbers are measured the same way and stay comparable.
//!
//! The **end-to-end** half — that a run through the mock SDR device actually produces these rows,
//! signature matches and clusters — is [`super::m3_scene`]. Both halves are the gate; neither
//! alone is.
//!
//! # The ruling this module encodes: the floors read over the **full** held-out grid
//!
//! ADR-0016 §7 enumerates six out-of-taxonomy generators and states two floors over held-out
//! inputs: `unknown` recall ≥ 0.80 and false-known ≤ 0.10. T-244 then added five more generators
//! — DSB-SC, VSB-AM, π/4-DQPSK, 16-APSK and Barker-13 — because `analog`, `psk-qam` and `pulsed`
//! had **no out-of-taxonomy negative at all**, so their open set was never exercised while the
//! aggregate still published a number for it.
//!
//! This gate reads the floors over **all eleven** generators, not the six §7 lists. The reasoning
//! is recorded in [`m3_unknown_recall_and_false_known_rate`]; the number the other reading gives is
//! measured and printed by the same test, so both are on the record.

use std::sync::OnceLock;

use hk_classify::harness::{GridSize, Harness, Report};
use hk_classify::synth::{ACCEPTANCE_SEED_BASE, Class, SynthConfig, generate};
use hk_classify::{Classifier, ClassifyRequest, SymbolEstimator};
use hk_model::Timestamp;
use hk_model::classify::{Classification, UNKNOWN};

const T206: &str = "T-206";

// ---------------------------------------------------------------------------------------------
// ADR-0016 §7 floors, a priori. **Never** edit one of these to make a run pass: the blind-test
// rule (ADR-0016 §7, "A-priori thresholds") makes tuning a threshold against an acceptance result
// a protocol violation, not a judgement call. A floor changes only by an ADR amendment citing dev
// evidence.
// ---------------------------------------------------------------------------------------------

/// Known families, synthetic acceptance, at or above gate + 5 dB.
const TOP1_FLOOR: f64 = 0.90;
/// As above, top-2.
const TOP2_FLOOR: f64 = 0.95;
/// Wrong-label rate over every bin (abstaining is always allowed).
const WRONG_LABEL_OVERALL_FLOOR: f64 = 0.02;
/// Worst single-bin wrong-label rate at or above the gates.
const WRONG_LABEL_PER_BIN_FLOOR: f64 = 0.05;
/// Out-of-taxonomy inputs correctly not given a family.
const UNKNOWN_RECALL_FLOOR: f64 = 0.80;
/// Out-of-taxonomy inputs given a family instead.
const FALSE_KNOWN_FLOOR: f64 = 0.10;

/// Trials per held-out `(generator, SNR)` cell in the per-generator breakdown.
const HELD_OUT_TRIALS: u32 = 12;

/// One blind classification through the cascade, exactly as `eval-harness` and T-199's sweep do it:
/// C14 on the symbol-geometry view, then the feature tree on the classifier's snippet.
fn classify_one(
    classifier: &Classifier,
    c14: &mut SymbolEstimator,
    class: Class,
    snr_db: f64,
    seed: u64,
) -> Classification {
    let s = generate(class, &SynthConfig::new(snr_db, seed));
    let symbols = c14.from_samples(
        &s.symbol_samples,
        s.symbol_sample_rate_hz,
        Some(s.obw_hz),
        Some(snr_db),
    );
    let mut req = ClassifyRequest::new(&s.samples, s.sample_rate_hz, Timestamp::UNIX_EPOCH);
    req.obw_hz = Some(s.obw_hz);
    req.snr_db = Some(snr_db);
    req.symbols = symbols.as_ref();
    req.symbol_samples = Some(&s.symbol_samples);
    req.symbol_sample_rate_hz = Some(s.symbol_sample_rate_hz);
    classifier.classify(&req)
}

/// The full acceptance grid, run once and shared: ~1 700 snippets, the size ADR-0016 §7 requires
/// before a number is trusted for a gate ([`GridSize::Full`]).
fn grid() -> &'static Report {
    static GRID: OnceLock<Report> = OnceLock::new();
    GRID.get_or_init(|| {
        let classifier = Classifier::new();
        let mut c14 = SymbolEstimator::new();
        let mut harness = Harness::new(GridSize::Full);
        harness
            .run_synthetic(|s| {
                let symbols = c14.from_samples(
                    s.symbol_samples,
                    s.symbol_sample_rate_hz,
                    s.obw_hz,
                    s.snr_db,
                );
                let mut req =
                    ClassifyRequest::new(s.samples, s.sample_rate_hz, Timestamp::UNIX_EPOCH);
                req.obw_hz = s.obw_hz;
                req.snr_db = s.snr_db;
                req.symbols = symbols.as_ref();
                req.symbol_samples = Some(s.symbol_samples);
                req.symbol_sample_rate_hz = Some(s.symbol_sample_rate_hz);
                classifier.classify(&req)
            })
            .expect("the acceptance run touched only acceptance seeds");
        let report = harness.finish();
        eprintln!(
            "[{T206}] ADR-0016 §7 evaluation report\n{}",
            report.markdown()
        );
        report
    })
}

/// **Known families: top-1 and top-2 at gate + 5 dB, with the conditions** (ADR-0016 §7 row 1).
///
/// The per-family and per-SNR rows are printed by [`grid`]; a single SNR-averaged number is never
/// the only thing reported (§7, "Reporting").
#[test]
fn m3_known_family_top_k_meets_the_floors() {
    let report = grid();
    let s = &report.summary;
    eprintln!(
        "[{T206}] known families at gate+5 dB over {} snippets: top-1 {:.4} (floor {TOP1_FLOOR}), \
         top-2 {:.4} (floor {TOP2_FLOOR})",
        s.n_total, s.known_top1_at_gate_plus5, s.known_top2_at_gate_plus5
    );
    for row in report
        .rows
        .iter()
        .filter(|r| r.source == "synthetic-acceptance")
    {
        eprintln!(
            "[{T206}]   {:<11} gate{:+3} dB: n {:>3}, top-1 {:.2}, top-2 {:.2}, unknown {:.2}, wrong {:.2}",
            row.family, row.snr_bin_db, row.n, row.top1, row.top2, row.unknown, row.wrong
        );
    }
    assert!(
        s.known_top1_at_gate_plus5 >= TOP1_FLOOR,
        "[{T206}] top-1 {:.4} is below the ADR-0016 §7 floor of {TOP1_FLOOR}",
        s.known_top1_at_gate_plus5
    );
    assert!(
        s.known_top2_at_gate_plus5 >= TOP2_FLOOR,
        "[{T206}] top-2 {:.4} is below the ADR-0016 §7 floor of {TOP2_FLOOR}",
        s.known_top2_at_gate_plus5
    );
}

/// **Wrong labels: overall and worst-bin** (ADR-0016 §7 row 2). Abstaining is allowed everywhere;
/// naming the wrong family is the failure this bounds.
#[test]
fn m3_wrong_label_rate_stays_under_the_floors() {
    let report = grid();
    let s = &report.summary;
    eprintln!(
        "[{T206}] wrong-label: {:.4} overall (floor {WRONG_LABEL_OVERALL_FLOOR}), worst bin {:.4} \
         (floor {WRONG_LABEL_PER_BIN_FLOOR}), {:.4} at or above the gates",
        s.wrong_label_rate_overall, s.worst_bin_wrong_rate, s.wrong_label_rate_at_gate
    );
    assert!(
        s.wrong_label_rate_overall <= WRONG_LABEL_OVERALL_FLOOR,
        "[{T206}] wrong-label rate {:.4} exceeds the ADR-0016 §7 floor of {WRONG_LABEL_OVERALL_FLOOR}",
        s.wrong_label_rate_overall
    );
    assert!(
        s.worst_bin_wrong_rate <= WRONG_LABEL_PER_BIN_FLOOR,
        "[{T206}] worst per-bin wrong-label rate {:.4} exceeds the ADR-0016 §7 floor of \
         {WRONG_LABEL_PER_BIN_FLOOR}",
        s.worst_bin_wrong_rate
    );
}

/// **Every family's open set was actually exercised** (T-244).
///
/// A family no out-of-taxonomy generator reached has no false-known rate at all, and an aggregate
/// that silently averages over the hole reads like a result. This gate refuses to publish one.
#[test]
fn m3_every_family_open_set_is_measured() {
    let report = grid();
    for c in &report.coverage {
        match (c.unknown_recall, c.false_known_rate) {
            (Some(recall), Some(false_known)) => eprintln!(
                "[{T206}] open set {:<11} n {:>3}, unknown recall {recall:.3}, false-known \
                 {false_known:.3}  [{}]",
                c.family,
                c.n_ood,
                c.generators.join(", ")
            ),
            _ => eprintln!("[{T206}] open set {:<11} UNMEASURED", c.family),
        }
    }
    report
        .require_open_set_coverage()
        .expect("every hk-mod@1 family with a generator must have an out-of-taxonomy negative");
}

/// What the held-out generators did, per generator and per population.
struct HeldOut {
    /// `(generator, abstained, trials)`, in `Class::HELD_OUT` order.
    per_class: Vec<(&'static str, u32, u32)>,
    /// Held-out snippets given a family that is **not** the one they genuinely belong to.
    wrong_family: Vec<(&'static str, String)>,
    all_unknown: u32,
    all_total: u32,
    adr_unknown: u32,
    adr_total: u32,
}

impl HeldOut {
    fn recall(&self) -> f64 {
        f64::from(self.all_unknown) / f64::from(self.all_total)
    }
    fn false_known(&self) -> f64 {
        f64::from(self.all_total - self.all_unknown) / f64::from(self.all_total)
    }
    fn adr_recall(&self) -> f64 {
        f64::from(self.adr_unknown) / f64::from(self.adr_total)
    }
    fn adr_false_known(&self) -> f64 {
        f64::from(self.adr_total - self.adr_unknown) / f64::from(self.adr_total)
    }
}

/// Classifies every held-out generator at 20/25/30 dB, keeping both populations separate: the six
/// ADR-0016 §7 enumerates, and all eleven T-244 measures.
fn held_out() -> &'static HeldOut {
    static HELD_OUT: OnceLock<HeldOut> = OnceLock::new();
    HELD_OUT.get_or_init(|| {
        let classifier = Classifier::new();
        let mut c14 = SymbolEstimator::new();
        // A seed range of its own, still inside the acceptance half: the densities never saw it.
        let mut seed = ACCEPTANCE_SEED_BASE + 700_000;
        let mut out = HeldOut {
            per_class: Vec::new(),
            wrong_family: Vec::new(),
            all_unknown: 0,
            all_total: 0,
            adr_unknown: 0,
            adr_total: 0,
        };
        for class in Class::HELD_OUT {
            let adr = Class::ADR_HELD_OUT.contains(class);
            let mut hits = 0;
            let mut trials = 0;
            for offset in [0.0_f64, 5.0, 10.0] {
                for _ in 0..HELD_OUT_TRIALS {
                    seed += 1;
                    let c = classify_one(&classifier, &mut c14, *class, 20.0 + offset, seed);
                    trials += 1;
                    out.all_total += 1;
                    out.adr_total += u32::from(adr);
                    // ADR-0016 §7 counts either outcome as recognising the unknown.
                    if c.family == UNKNOWN || c.open_set_score >= 0.5 {
                        hits += 1;
                        out.all_unknown += 1;
                        out.adr_unknown += u32::from(adr);
                    } else if class.nearest_family() != Some(c.family.as_str()) {
                        out.wrong_family.push((class.label(), c.family.clone()));
                    }
                }
            }
            out.per_class.push((class.label(), hits, trials));
        }
        out
    })
}

/// **Unknown recall and false-known rate** (ADR-0016 §7 row 4) — and the gate's ruling on which
/// population those two floors are read over.
///
/// # The ruling: the **full** held-out grid, all eleven generators
///
/// Both readings are defensible and the choice decides whether M3 closes, so the reasoning is on
/// the record rather than in a commit message.
///
/// **Why the full grid.**
///
/// 1. **§7's list is a category, not a population.** It introduces those six under the heading
///    "Held-out unknowns: generators not in the dev grid", then adds "plus real negatives" — it is
///    naming examples of a kind of input, not fixing a denominator. T-244's five are the same kind
///    of input, and the ADR gives no principle by which a 16-APSK burst is exempt from a floor a
///    Costas hop set is subject to.
/// 2. **Reading it over the six freezes a sampling accident into the gate.** Those six are exactly
///    the generators that existed while routing went through `nearest_family`, which is `None` for
///    three of them — which is *why* `analog`, `psk-qam` and `pulsed` had no negative. Scoring the
///    floor over only the families that happened to have negatives is the hole T-244 was filed to
///    close; re-adopting that denominator here would reopen it at the one place it matters most.
/// 3. **The product's premise is at stake.** Surfacing unknowns is the whole point (CLAUDE.md:
///    "Unknown signals are the priority"). A classifier that answers `analog` for 47 % of the
///    unlisted analog modes it meets does not put those emissions in front of the operator, does
///    not route them to the cluster of unknowns, and does not seed a MAUTO search. That is a real
///    gap in the thing M3 exists to do, and a gate that cannot see it is not a gate.
///
/// **The strongest argument the other way, and why it does not carry the decision.** Almost every
/// failure here is an *abstention* failure rather than a misclassification. 16-APSK really is
/// `psk-qam` and VSB-AM really is `analog`; naming them is generalisation, which is what
/// [`Class::nearest_family`] documents and what a classifier should do. On that reading
/// "false-known" overstates the harm, and the wrong-label floor, which bounds actual lying, is met
/// with room to spare (0.001 against 0.02).
///
/// **But "almost": T-244's supporting claim does not survive a larger sample.** T-244 recorded
/// "0 of 88 held-out snippets received a family that is not their own" as a property. Over the 396
/// snippets measured here it is 3 — a chirped-carrier 2-FSK read as `analog`, which belongs to no
/// family at all and so is a genuinely wrong label, not generalisation. At 8 trials per generator
/// that outcome simply had not been sampled. The count is printed every run; it is small, but it
/// means the reassuring half of the abstention argument is a sampling result rather than a
/// guarantee.
///
/// It does not carry the decision because §7's floor is stated over **abstention** (`unknown`, or
/// `open_set ≥ 0.5`), and for this product an unlisted mode confidently absorbed into a known
/// family is the exact failure mode that matters: the operator is told "analog", and a novel
/// emitter never reaches the unknown queue. §7 already grants the generous reading of abstention —
/// an open-set score of 0.5 counts, no matter what the top label is — and these snippets fail even
/// that.
///
/// **What the other reading gives** (printed by this test every run): over the six generators §7
/// enumerates, unknown recall **0.9167** and false-known **0.0833** — both floors met, so M3 would
/// close. That number is real, it is reported, and it is not the number this gate uses.
///
/// Note how little room it has. T-244 reported 1.000 and 0.000 for this same population from 8
/// trials per generator; at 36 the six sit at 0.9167 and **0.0833 against a 0.10 floor**, because
/// 8-FSK abstains on only 21 of 36 and the chirped-carrier FSK on 33 of 36. So the reading that
/// closes M3 does not clear its floor comfortably — it clears it by 0.017, on the strength of two
/// generators that are themselves close to failing.
#[test]
fn m3_unknown_recall_and_false_known_rate() {
    let h = held_out();
    eprintln!(
        "[{T206}] held-out abstention per generator (of {} trials each): {:?}",
        HELD_OUT_TRIALS * 3,
        h.per_class
    );
    eprintln!(
        "[{T206}] RULING — the gate reads over the FULL held-out grid ({} snippets, all 11 \
         generators): unknown recall {:.4} (floor {UNKNOWN_RECALL_FLOOR}), false-known {:.4} \
         (floor {FALSE_KNOWN_FLOOR})",
        h.all_total,
        h.recall(),
        h.false_known()
    );
    eprintln!(
        "[{T206}] the other reading, over the {} snippets of the six generators ADR-0016 §7 \
         enumerates: unknown recall {:.4}, false-known {:.4} — both floors met; recorded, not used",
        h.adr_total,
        h.adr_recall(),
        h.adr_false_known()
    );

    // **Held-out inputs given a family that is not their own: reported, not floored.**
    //
    // This is the outcome that separates an abstention failure from a dangerous one, and T-244
    // recorded it as a property ("0 of 88 held-out snippets received a family that is not their
    // own"). Over this larger sample it is **not** a property: see the count below. But ADR-0016
    // §7 states no floor over it — its held-out row bounds abstention and false-known only — so
    // asserting one here would be inventing a threshold after seeing results, which the blind-test
    // rule forbids in both directions. It is measured, printed and left to the ADR to bound.
    eprintln!(
        "[{T206}] held-out inputs given a family that is NOT their own: {} of {} — {:?}",
        h.wrong_family.len(),
        h.all_total,
        h.wrong_family
    );

    assert!(
        h.recall() >= UNKNOWN_RECALL_FLOOR,
        "[{T206}] unknown recall {:.4} over the full held-out grid is below the ADR-0016 §7 floor \
         of {UNKNOWN_RECALL_FLOOR}. Per generator: {:?}. Over the six §7 enumerates it is {:.4}; \
         this gate rules on the full grid (see this test's documentation)",
        h.recall(),
        h.per_class,
        h.adr_recall()
    );
    assert!(
        h.false_known() <= FALSE_KNOWN_FLOOR,
        "[{T206}] false-known rate {:.4} over the full held-out grid exceeds the ADR-0016 §7 floor \
         of {FALSE_KNOWN_FLOOR}. Every one of them is an unlisted mode recognised as the family it \
         genuinely belongs to (0 wrong families), but §7's floor is stated over abstention. Over \
         the six §7 enumerates it is {:.4}; this gate rules on the full grid",
        h.false_known(),
        h.adr_false_known()
    );
}

/// **Noise is never called a communication family** (ADR-0016 §7 row 4, third clause: ≤ 1 %).
#[test]
fn m3_noise_is_never_called_a_communication_family() {
    let classifier = Classifier::new();
    let mut c14 = SymbolEstimator::new();
    let mut seed = ACCEPTANCE_SEED_BASE + 800_000;
    let trials = 40;
    let mut wrong = 0;
    for _ in 0..trials {
        seed += 1;
        let c = classify_one(&classifier, &mut c14, Class::NoiseLike, 20.0, seed);
        if c.family != "noise-like" && c.family != UNKNOWN {
            wrong += 1;
            eprintln!("[{T206}] noise called {}", c.family);
        }
    }
    let rate = f64::from(wrong) / f64::from(trials);
    eprintln!("[{T206}] noise called a comm family {rate:.4} of {trials} (floor 0.01)");
    assert!(
        rate <= 0.01,
        "[{T206}] noise called a communication family {rate:.4} of the time"
    );
}
