//! T-310: how `cyclic_db` moves with the length of the window C14 was given, and why.
//!
//! T-281 recorded that `cyclic_db` — a whitened peak-to-median ratio — grows about `10·log10(N)`,
//! from two classes on one seed at one SNR. Re-measured across seeds, SNRs and the whole taxonomy,
//! the dependence is real but that law is not: the mean slope is 3.3–6.6 dB/decade depending on
//! SNR, it spans −19.8 to +27.1 dB/decade across classes, and **6 of 21 taxonomy classes read a
//! lower `cyclic_db` the longer they are watched**. Coherent integration cannot produce a negative
//! slope, so the growth is not coherent integration.
//!
//! This module pins the two structural findings that explain it, because both are the kind of thing
//! a later reader re-derives wrongly from a single seed (which is how the `10·log10(N)` law got
//! into the code in the first place):
//!
//! 1. **`cyclic_db` is a max over four different feature series, and which one wins moves with the
//!    window.** Two readings of one emitter are then not the same measurement.
//! 2. **C14's rate search starts at `f_min ∝ fs/n`**, so a shorter window searches from a higher
//!    frequency and can only find a harmonic of a symbol-rate line it can no longer reach.
//!
//! Asserted on **means over dev seeds**, never on one seed: these are distributional statements
//! about a generator, and a single seed would pin noise. The full per-class table is printed rather
//! than asserted — the numbers are evidence, and only the structure is a contract.
//!
//! The reasoning from these measurements, and every length-free statistic that was tried and
//! refuted, is recorded on `hk_classify::features`'s `symbol_features`.

use hk_classify::symbols::SymbolEstimator;
use hk_classify::synth::{Class, DEV_SEEDS, SynthConfig, generate};
use hk_estimate::blind::SymbolParameters;

/// Window fractions every measurement below sweeps: the same emission, watched 8× to 1× as long.
const FRACTIONS: [usize; 4] = [8, 4, 2, 1];

/// Dev seeds per cell. Enough for a mean that is not one waveform; small enough to stay quick.
const SEEDS: u64 = 4;

/// SNR the structural claims are asserted at: comfortably above every family gate, so nothing here
/// is a statement about a marginal detection.
const SNR_DB: f64 = 25.0;

/// One C14 estimate over the first `1/den` of a class's symbol-geometry view.
fn estimate(
    c14: &mut SymbolEstimator,
    class: Class,
    seed: u64,
    snr_db: f64,
    den: usize,
) -> Option<SymbolParameters> {
    let s = generate(class, &SynthConfig::new(snr_db, seed));
    let n = s.symbol_samples.len() / den;
    c14.from_samples(
        &s.symbol_samples[..n],
        s.symbol_sample_rate_hz,
        Some(s.obw_hz),
        Some(snr_db),
    )
}

/// `cyclic_db` itself: the largest of the four whitened line significances, as
/// `hk_classify::features::symbol_features` takes it.
fn cyclic_db(p: &SymbolParameters) -> f64 {
    p.lines
        .iter()
        .map(|l| l.significance_db)
        .fold(f64::NEG_INFINITY, f64::max)
}

/// Mean `cyclic_db` over the dev seeds for one class at one window fraction.
fn mean_cyclic_db(c14: &mut SymbolEstimator, class: Class, den: usize) -> f64 {
    let v: Vec<f64> = (DEV_SEEDS.start..DEV_SEEDS.start + SEEDS)
        .filter_map(|seed| estimate(c14, class, seed, SNR_DB, den))
        .map(|p| cyclic_db(&p))
        .filter(|v| v.is_finite())
        .collect();
    assert!(!v.is_empty(), "{} produced no estimate", class.label());
    v.iter().sum::<f64>() / v.len() as f64
}

/// **The direction of the length dependence is class-dependent**, which is what refutes a single
/// `10·log10(N)` growth law for this statistic.
///
/// `ook` rises with the window, as coherent integration would predict. `2fsk` *falls* — and so do
/// `4fsk`, `gfsk`, `msk`, `wfm` and `ppm`. A statistic that moves in both directions cannot be
/// corrected by dividing out any single function of `N`, which is why nothing is rescaled.
#[test]
fn the_window_dependence_of_cyclic_db_runs_in_both_directions() {
    let mut c14 = SymbolEstimator::new();
    eprintln!("\n=========== cyclic_db vs window length, dev seeds at {SNR_DB} dB ===========");
    eprintln!(
        "  {:<8} {:>9} {:>9} {:>9} {:>9}",
        "class", "N/8", "N/4", "N/2", "N"
    );
    let row = |c14: &mut SymbolEstimator, class: Class| -> (f64, f64) {
        let v: Vec<f64> = FRACTIONS
            .iter()
            .map(|&den| mean_cyclic_db(c14, class, den))
            .collect();
        eprintln!(
            "  {:<8} {:>9.2} {:>9.2} {:>9.2} {:>9.2}",
            class.label(),
            v[0],
            v[1],
            v[2],
            v[3]
        );
        (v[0], v[3])
    };
    let (ook_short, ook_long) = row(&mut c14, Class::Ook);
    let (fsk_short, fsk_long) = row(&mut c14, Class::Fsk2);
    let (fsk4_short, fsk4_long) = row(&mut c14, Class::Fsk4);

    assert!(
        ook_long > ook_short + 3.0,
        "ook rises with the window as the coherent-growth argument predicts: \
         {ook_short:.2} dB at N/8 against {ook_long:.2} dB at N"
    );
    assert!(
        fsk_long < fsk_short,
        "2fsk FALLS as the window grows, which coherent integration cannot do: \
         {fsk_short:.2} dB at N/8 against {fsk_long:.2} dB at N"
    );
    assert!(
        fsk4_long < fsk4_short,
        "4fsk falls too: {fsk4_short:.2} dB at N/8 against {fsk4_long:.2} dB at N"
    );
}

/// **`cyclic_db` is a max over four feature series, and the winner changes with the window** — so
/// the same emitter read at two lengths is frequently not the same measurement.
///
/// Asserted per seed and counted, rather than on one waveform: the claim is that this is the normal
/// case, not that it happens somewhere.
#[test]
fn the_winning_cyclic_line_changes_when_the_window_changes() {
    let mut c14 = SymbolEstimator::new();
    let mut switched = 0;
    for seed in DEV_SEEDS.start..DEV_SEEDS.start + SEEDS {
        let mut methods = Vec::new();
        for &den in &FRACTIONS {
            if let Some(p) = estimate(&mut c14, Class::Fsk2, seed, SNR_DB, den) {
                if let Some(best) = p
                    .lines
                    .iter()
                    .max_by(|a, b| a.significance_db.total_cmp(&b.significance_db))
                {
                    methods.push(best.method);
                }
            }
        }
        eprintln!("  2fsk seed {seed}: winning line across N/8..N = {methods:?}");
        if methods.windows(2).any(|w| w[0] != w[1]) {
            switched += 1;
        }
    }
    assert!(
        switched * 2 >= SEEDS as usize,
        "the winning line method changed with the window on only {switched} of {SEEDS} seeds — \
         if this stops being the normal case, re-measure the whole finding rather than relaxing it"
    );
}

/// **C14's rate search starts at `f_min = max(rate_min_cells·fs/n, obw·rate_min_obw)`**, so the
/// band searched is itself a function of the record length.
///
/// This is the mechanism behind the negative slopes: at the short window the floor rises above the
/// true symbol-rate line, and only a harmonic of it remains findable. It is the same class of
/// defect as the analysis-resolution note on `features::spectral_features` — the measurement
/// geometry moving with the observation — and it lives in C14, which is why T-310 changed no
/// statistic here.
#[test]
fn the_rate_search_band_itself_moves_with_the_window() {
    let mut c14 = SymbolEstimator::new();
    let mut short_floor = 0.0;
    let mut long_floor = 0.0;
    let mut n = 0.0;
    for seed in DEV_SEEDS.start..DEV_SEEDS.start + SEEDS {
        let (Some(short), Some(long)) = (
            estimate(&mut c14, Class::Fsk2, seed, SNR_DB, 8),
            estimate(&mut c14, Class::Fsk2, seed, SNR_DB, 1),
        ) else {
            continue;
        };
        short_floor += short.rate_range_hz.0;
        long_floor += long.rate_range_hz.0;
        n += 1.0;
    }
    assert!(n > 0.0, "no 2fsk estimate ran");
    let (short_floor, long_floor) = (short_floor / n, long_floor / n);
    eprintln!(
        "  2fsk rate search starts at {short_floor:.1} Hz over N/8 and {long_floor:.1} Hz over N"
    );
    assert!(
        short_floor > long_floor,
        "the search band's lower edge must move with the record for the harmonic explanation to \
         hold: {short_floor:.1} Hz at N/8 against {long_floor:.1} Hz at N"
    );
}
