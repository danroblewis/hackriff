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
//! 2. **C14's rate search started at `f_min ∝ fs/n`**, so the band searched — and `rate_range_hz`,
//!    a reported field — was itself a function of the record. **T-327 pinned it**, and the third
//!    test below now guards the pin rather than the defect. Pinning it did *not* remove the window
//!    dependence (10.16 → 9.40 dB mean movement), which is the part of T-310's account T-327
//!    overturned: the geometry was a real defect but not the mechanism. Finding 1 is.
//! 3. **A short burst puts the four line significances off *together*, not one at a time.** That is
//!    why T-328 re-measured T-310's four-dimension expansion with the geometry pinned and still
//!    left it unshipped: `density`'s second-largest-|z| term (T-248) withholds a claim on exactly
//!    that pattern, so four dimensions reject genuine short bursts from their own class. The
//!    fourth test below pins the pattern; the end-to-end figures are on `symbol_features`.
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

/// Dev seeds for the T-328 test below, which needs a per-class **sd** and not only a mean.
const SPREAD_SEEDS: u64 = 8;

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

/// **C14's rate search band does not move with the record** — T-310 found that it did, T-327
/// pinned it, and this now guards the pin in the other direction.
///
/// T-310 measured `f_min = max(rate_min_cells·fs/n, obw·rate_min_obw)`, so the band searched — and
/// `rate_range_hz`, a *reported* field — was a function of the record length: over the taxonomy the
/// lower edge moved 212 → 27 Hz (`am`), 377 → 129 (`nbfm`) and 1965 → 246 (most keyed classes)
/// between an eighth of the window and all of it. Two readings of one emitter then did not search
/// the same band.
///
/// T-327 removed the `fs/n` term. The resolution limit behind it is real but belongs to the
/// transform, so it now lives in `lines::spectral_line` as a DC guard that limits which bins may be
/// *reported* without moving the band, the harmonic candidate set `{f/4, f/3, f/2, f, 2f}`, or the
/// reported range.
///
/// **This is not the same claim as "`cyclic_db` is now length-free".** T-327 measured that it is
/// not: with the band and the whitening width both pinned, the mean `|N − N/8|` movement of
/// `cyclic_db` falls only from 10.16 dB to 9.40 dB, and `2fsk`, `gfsk`, `msk` and `4fsk` still read
/// lower the longer they are watched. The geometry was one defect; it was not the mechanism. See
/// `hk_classify::features::symbol_features`.
#[test]
fn the_rate_search_band_does_not_move_with_the_window() {
    let mut c14 = SymbolEstimator::new();
    let mut checked = 0;
    // Every class, not just one: the old defect's size depended on OBW99, so a single narrowband
    // or wideband class could pass while the band still moved everywhere else.
    for class in Class::TAXONOMY {
        for seed in DEV_SEEDS.start..DEV_SEEDS.start + SEEDS {
            let (Some(short), Some(long)) = (
                estimate(&mut c14, *class, seed, SNR_DB, 8),
                estimate(&mut c14, *class, seed, SNR_DB, 1),
            ) else {
                continue;
            };
            checked += 1;
            assert_eq!(
                short.rate_range_hz.0.to_bits(),
                long.rate_range_hz.0.to_bits(),
                "{} seed {seed}: the rate search's lower edge is {} Hz over N/8 and {} Hz over N — \
                 it must be a function of OBW99 alone, never of the record",
                class.label(),
                short.rate_range_hz.0,
                long.rate_range_hz.0
            );
            assert_eq!(
                short.rate_range_hz.1.to_bits(),
                long.rate_range_hz.1.to_bits(),
                "{} seed {seed}: the upper edge moved too ({} vs {})",
                class.label(),
                short.rate_range_hz.1,
                long.rate_range_hz.1
            );
        }
    }
    eprintln!("  rate_range_hz identical at N/8 and N on {checked} class × seed pairs");
    assert!(
        checked >= 4 * Class::TAXONOMY.len(),
        "only {checked} pairs ran"
    );
}

/// The four line significances of one class, at one window fraction, over the spread seeds.
fn per_method(c14: &mut SymbolEstimator, class: Class, den: usize) -> Vec<[f64; 4]> {
    (DEV_SEEDS.start..DEV_SEEDS.start + SPREAD_SEEDS)
        .filter_map(|seed| estimate(c14, class, seed, SNR_DB, den))
        .map(|p| {
            let mut v = [f64::NEG_INFINITY; 4];
            for l in &p.lines {
                v[l.method as usize] = l.significance_db;
            }
            v
        })
        .filter(|v| v.iter().all(|x| x.is_finite()))
        .collect()
}

/// **T-328: carrying the four line significances as four dimensions would put all four of them off
/// at once on a short burst, which is exactly the signature that withholds a claim.**
///
/// T-310 measured a large discrimination win for the expansion and refused to take it, on the
/// grounds that it made the burst case worse; T-327 pinned C14's geometry so the measurement is no
/// longer confounded; T-328 re-measured it and **still** refused. The full end-to-end figures are
/// on [`hk_classify::features`]'s `symbol_features`. This pins the mechanism, which is the part a
/// later reader would otherwise re-derive wrongly — "four dimensions each better behaved than one"
/// is true per dimension and beside the point.
///
/// [`hk_classify::density`] scores membership as a **conjunction**: the mean squared z over ~27
/// dimensions *and* the second-largest |z| against the class's dev `z2_p99` (T-248, which exists
/// because a genuine member routinely has **one** wild dimension and essentially never two). So a
/// statistic that contributes one off dimension is survivable and one that contributes several at
/// once is not — and the four line significances move **together** when the window shortens,
/// because they are four periodograms of four feature series of the *same* truncated record.
///
/// Counted here as: fit each of the four on full windows over the dev seeds, then count how many of
/// them sit more than 2 sd from that mean on the same class's N/8 burst. The median over the
/// taxonomy is at least two, so the second-largest |z| is off too.
///
/// Asserted on the median over 21 classes, never on one class: the claim is that this is the normal
/// case for the taxonomy, not that some class does it.
#[test]
fn a_short_burst_puts_all_four_cyclic_line_dimensions_off_at_once() {
    /// How far from its full-window mean a significance must sit to count as an off dimension.
    const OFF_SD: f64 = 2.0;
    /// Floor on the fitted sd, dB. `density::fit` shrinks every sigma towards the feature's own
    /// scale for the same reason: one over-clean cell must not manufacture a large z. No dB
    /// measurement on this path is repeatable to better than about half a decibel.
    const SD_FLOOR_DB: f64 = 0.5;

    let mut c14 = SymbolEstimator::new();
    eprintln!(
        "\n=========== T-328: how many of the four line significances are off on an N/8 burst ==========="
    );
    eprintln!("  {:<11} {:>10} {:>10}", "class", "off of 4", "|z| of max");
    let mut counts: Vec<f64> = Vec::new();
    let mut max_z: Vec<f64> = Vec::new();
    for class in Class::TAXONOMY {
        let full = per_method(&mut c14, *class, 1);
        let burst = per_method(&mut c14, *class, 8);
        if full.len() < 4 || burst.is_empty() {
            continue;
        }
        let n = full.len() as f64;
        let mean: Vec<f64> = (0..4)
            .map(|i| full.iter().map(|v| v[i]).sum::<f64>() / n)
            .collect();
        let sd: Vec<f64> = (0..4)
            .map(|i| {
                let var =
                    full.iter().map(|v| (v[i] - mean[i]).powi(2)).sum::<f64>() / (n - 1.0).max(1.0);
                var.sqrt().max(SD_FLOOR_DB)
            })
            .collect();
        // The shipped statistic, for the same class, measured the same way: the max of the four
        // against the full-window distribution of that max.
        let fmax: Vec<f64> = full
            .iter()
            .map(|v| v.iter().copied().fold(f64::NEG_INFINITY, f64::max))
            .collect();
        let mmean = fmax.iter().sum::<f64>() / n;
        let msd = (fmax.iter().map(|v| (v - mmean).powi(2)).sum::<f64>() / (n - 1.0).max(1.0))
            .sqrt()
            .max(SD_FLOOR_DB);

        let off = burst
            .iter()
            .map(|b| {
                (0..4)
                    .filter(|&i| (b[i] - mean[i]).abs() > OFF_SD * sd[i])
                    .count() as f64
            })
            .sum::<f64>()
            / burst.len() as f64;
        let z = burst
            .iter()
            .map(|b| {
                let m = b.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                ((m - mmean) / msd).abs()
            })
            .sum::<f64>()
            / burst.len() as f64;
        eprintln!("  {:<11} {off:>10.2} {z:>10.2}", class.label());
        counts.push(off);
        max_z.push(z);
    }
    assert!(
        counts.len() >= Class::TAXONOMY.len() - 1,
        "only {} classes produced an estimate at both lengths",
        counts.len()
    );
    counts.sort_by(f64::total_cmp);
    let median = counts[counts.len() / 2];
    max_z.sort_by(f64::total_cmp);
    eprintln!(
        "  median over the taxonomy: {median:.2} of 4 dimensions off; the shipped max sits at \
         |z| = {:.2}",
        max_z[max_z.len() / 2]
    );
    assert!(
        median >= 2.0,
        "an N/8 burst puts a median of only {median:.2} of the four line significances more than \
         {OFF_SD} sd from their full-window mean. The T-328 refusal rests on two or more being off \
         at once — density's second-largest-|z| tail term. If this stops holding, re-run the \
         end-to-end measurement on symbol_features before concluding the expansion is safe."
    );
}
