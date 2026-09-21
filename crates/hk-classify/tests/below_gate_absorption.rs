//! **Below its own SNR gate, a family may be missed but must not be *replaced*** (T-435).
//!
//! ADR-0016 §2: a family whose SNR gate the measurement does not reach contributes no likelihood
//! mass at all — its share goes to `unknown` with reason `low_snr`, *never* to a neighbouring
//! family. "Not measured" is not "ruled out".
//!
//! # The failure this guards
//!
//! It is written out in full in `classifier.rs`, because it has happened: dropping a gated family
//! and renormalising over the survivors is what makes a low-SNR FSK burst come back as `analog` —
//! FSK is gated out below 20 dB while `analog`, gated at 10, is still standing and collects the
//! whole distribution. Measured at the time: a **0.65 wrong-label rate** in that bin, and on a real
//! 915 MHz FSK burst a `analog`/`nbfm` call at confidence 1.00 with open-set 0.00. A confidently
//! wrong family is the one outcome the open set exists to prevent (CLAUDE.md: ML must have an
//! explicit unknown/open-set output; a classifier that is confidently wrong is worse than one that
//! abstains).
//!
//! The fix was the **below-gate densities** — `DensityModel::builtin_below_gate()`, fitted where
//! the family is gated, whose plausibility becomes `unknown` mass and never a claim. This file
//! asserts that they keep working. It is a *capability* guard, not a probe: it fails if that
//! mechanism regresses, and nothing else in the suite does.
//!
//! # Why this population, and why it is not a duplicate of `accuracy_sweep.rs`
//!
//! Two floors of ADR-0016 §7 bear on wrong labels: **≤ 0.05 per bin** and **≤ 0.02 overall**, and
//! the row says *any* bin. But the figure anyone actually quotes — §7.1's "Wrong-label, worst bin"
//! — is measured over "`synthetic-acceptance`, bins **≥ gate**, n ≥ 10", and the overall figure
//! averages the below-gate bins into 630 snippets where 3 wrong calls read as 0.005. **So the
//! below-gate bins are inside the ADR's floor and outside every figure that gets read.** This file
//! applies §7's existing per-bin floor to exactly those bins. No threshold is invented here and
//! none is relaxed: `WRONG_LABEL_MAX_PER_BIN` is §7's own 0.05.
//!
//! The scope is the three families whose gate sits **above** the lowest gate in the taxonomy —
//! `fsk` and `ook-ask` at 20 dB and `psk-qam` at 15 dB, against `analog`/`ofdm`/`css`/`pulsed` at
//! 10 dB. They are the only families that can be gated out *while another family is still allowed
//! to answer*, which is the whole mechanism above. A `pulsed` emission at 5 dB has nothing left
//! standing to be absorbed by.
//!
//! # What this file deliberately does NOT assert (T-435, and T-429's reasons before it)
//!
//! The **class name** changing with the SNR. Measured blind on the same generator over
//! 21 classes × 7 SNR rungs × 24 seeds (504 ladders): the within-family name reverses on
//! **9 of 504** acceptance ladders and 13 of 504 dev ladders, and six of the nine acceptance flips
//! are the identical sequence `qam64 → qam16 → qam16`. They are reported at `p` ≈ 0.5 — a call the
//! classifier already declares to be a coin toss — under a `psk-qam` family call that does not
//! move. Two reasons not to assert it here: a per-ladder exception list is **seed-count fragile**
//! (T-429 measured 1 flip at 6–8 seeds and 3 at 12 on its own ladder), and the systematic
//! `qam16`→`qam64` direction at the bottom of the named range is **T-246**'s open defect (the
//! psk-qam ranking is monotone in constellation size on a smeared constellation), not a new one.
//! The numbers and the mechanism are recorded in ADR-0016 §7.3.

use hk_classify::synth::{ACCEPTANCE_SEED_BASE, Class, SynthConfig, generate};
use hk_classify::thresholds::thresholds_of;
use hk_classify::{Classifier, ClassifyRequest, SymbolEstimator};
use hk_model::Timestamp;
use std::collections::BTreeMap;

/// ADR-0016 §7's per-bin wrong-label floor, unchanged. Quoted, not chosen.
const WRONG_LABEL_MAX_PER_BIN: f64 = 0.05;

/// How far below the family's own gate each bin sits, dB.
///
/// `−2.5` is the interesting end (the family is one step from being allowed to answer and the
/// survivors are at their most plausible) and `−10` is the bottom of §7's own grid.
const BELOW_GATE_OFFSETS: [f64; 4] = [-10.0, -7.5, -5.0, -2.5];

/// Waveforms per (class, offset). The cells are per **family**, so a `fsk` cell holds
/// `SEEDS × 4` snippets and the smallest cell (`ook-ask`, two classes) holds `SEEDS × 2` — well
/// past the `n ≥ 10` §7.1 requires before a per-bin rate means anything.
///
/// 24 is T-429's `SNR_SEEDS` on this same generator, chosen there as the count at which the
/// finding set stops moving, and taken here **before** the first run rather than after — changing
/// it having seen a rate would be choosing a seed range for its answer. The measurement is a pure
/// function of these seeds (§7.1's determinism record), so this test does not flake: it passes on
/// every run or fails on every run.
const SEEDS: u64 = 24;

/// The families that can be gated out while another family is still entitled to answer, which is
/// the mechanism in the module header. Derived, not listed: any family whose gate is above the
/// lowest gate in the taxonomy.
fn absorbable() -> Vec<&'static str> {
    let lowest = hk_model::classify::HK_MOD_V1
        .families
        .iter()
        .filter_map(|f| thresholds_of(f.name).and_then(|t| t.snr_gate_db))
        .fold(f64::INFINITY, f64::min);
    hk_model::classify::HK_MOD_V1
        .families
        .iter()
        .filter(|f| {
            thresholds_of(f.name)
                .and_then(|t| t.snr_gate_db)
                .is_some_and(|g| g > lowest)
        })
        .map(|f| f.name)
        .collect()
}

fn gate_of(family: &str) -> f64 {
    thresholds_of(family)
        .and_then(|t| t.snr_gate_db)
        .expect("an absorbable family has a gate")
}

/// One blind classification, through the two-stage measurement the pipeline performs — the same
/// path `accuracy_sweep.rs` uses, so a difference here is never a difference of harness.
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
    req.symbol_samples = Some(&s.symbol_samples);
    req.symbol_sample_rate_hz = Some(s.symbol_sample_rate_hz);
    Classifier::new().classify(&req)
}

/// One cell: (truth family, offset from that family's gate).
#[derive(Default)]
struct Cell {
    n: usize,
    unknown: usize,
    wrong: usize,
    /// Wrong **and** claimed at the classifier's own operating point: confidence at or above the
    /// claimed family's `min_confidence` and open set at or below its `open_set_max`. Reported,
    /// not asserted — it is a strict subset of `wrong`, so a floor on it would add nothing the
    /// wrong-label floor does not already give. It is here because the *severity* of a wrong
    /// label is invisible in a rate, and the build log records this count going 0 → 5 without
    /// any floor noticing.
    confident: usize,
    worst: Vec<String>,
}

#[test]
fn a_family_below_its_gate_is_missed_and_not_replaced() {
    let mut c14 = SymbolEstimator::new();
    let mut cells: BTreeMap<(&'static str, i64), Cell> = BTreeMap::new();
    let mut seed = ACCEPTANCE_SEED_BASE + 800_001;

    for family in absorbable() {
        let gate = gate_of(family);
        for class in Class::TAXONOMY
            .iter()
            .filter(|c| c.family() == Some(family))
        {
            for offset in BELOW_GATE_OFFSETS {
                let snr = gate + offset;
                for _ in 0..SEEDS {
                    seed += 1;
                    let c = classify_one(&mut c14, *class, snr, seed);
                    c.validate().expect("contract");
                    let cell = cells.entry((family, offset as i64)).or_default();
                    cell.n += 1;
                    if c.family == "unknown" {
                        cell.unknown += 1;
                    } else if c.family != family {
                        cell.wrong += 1;
                        let t = thresholds_of(&c.family);
                        let claimed = c.confidence >= t.map_or(0.6, |t| t.min_confidence)
                            && c.open_set_score <= t.map_or(0.5, |t| t.open_set_max);
                        if claimed {
                            cell.confident += 1;
                        }
                        cell.worst.push(format!(
                            "{class:?} @ {snr} dB (gate{offset:+}) -> {}{} conf {:.3} open_set \
                             {:.3}{}",
                            c.family,
                            c.class
                                .as_ref()
                                .map_or(String::new(), |k| format!("/{}", k.label)),
                            c.confidence,
                            c.open_set_score,
                            if claimed { "  CLAIMED" } else { "" },
                        ));
                    }
                }
            }
        }
    }

    // The grid must still be a grid: an empty or collapsed population would pass everything.
    let total: usize = cells.values().map(|c| c.n).sum();
    assert!(
        cells.len() >= 12 && total >= 800,
        "only {} cells over {total} snippets: the below-gate grid collapsed, so a pass would mean \
         nothing",
        cells.len()
    );

    eprintln!("below-gate absorption, blind on acceptance seeds (family, offset from its gate):");
    for ((fam, off), c) in &cells {
        eprintln!(
            "  {fam:<9} {off:>+4} dB  n={:<4} unknown={:<4} wrong={:<3} ({:.4})  claimed-wrong={}",
            c.n,
            c.unknown,
            c.wrong,
            c.wrong as f64 / c.n as f64,
            c.confident,
        );
        for w in &c.worst {
            eprintln!("        {w}");
        }
    }

    let mut over_floor = Vec::new();
    let mut absorbed = Vec::new();
    for ((fam, off), c) in &cells {
        let rate = c.wrong as f64 / c.n as f64;
        if rate > WRONG_LABEL_MAX_PER_BIN {
            over_floor.push(format!(
                "{fam} at gate{off:+} dB: {}/{} = {rate:.4} over ADR-0016 §7's per-bin floor of \
                 {WRONG_LABEL_MAX_PER_BIN}\n        {}",
                c.wrong,
                c.n,
                c.worst.join("\n        ")
            ));
        }
        // ADR-0016 §2 as a property rather than a rate: where the measurement was not allowed to
        // be made, the designed answer is `unknown`, so abstention must outnumber replacement.
        // This is what separates "the gate held a family back" (honest) from "the gate handed the
        // family's snippets to its neighbour" (the defect).
        if c.unknown < c.wrong {
            absorbed.push(format!(
                "{fam} at gate{off:+} dB: {} wrong against only {} abstentions\n        {}",
                c.wrong,
                c.unknown,
                c.worst.join("\n        ")
            ));
        }
    }

    assert!(
        over_floor.is_empty(),
        "{} below-gate bin(s) exceed ADR-0016 §7's per-bin wrong-label floor. These bins are \
         inside the floor and outside §7.1's quoted worst-bin figure, which is measured over bins \
         >= gate only — so nothing else in the suite would have said so. A family held back by \
         its SNR gate has NOT been ruled out; its share belongs on `unknown` (§2), and the \
         below-gate densities exist to put it there. Do not relax this number: it is §7's, and \
         the floors are a-priori.\n\n{}",
        over_floor.len(),
        over_floor.join("\n\n"),
    );
    assert!(
        absorbed.is_empty(),
        "{} below-gate bin(s) MISNAME more often than they abstain. That is the absorption \
         `classifier.rs` documents and the below-gate densities were built to stop: the gated \
         family is dropped, the distribution renormalises over whichever family is still allowed \
         to answer, and the snippet comes back confidently wrong. Abstaining is allowed; \
         replacing is not.\n\n{}",
        absorbed.len(),
        absorbed.join("\n\n"),
    );
}
