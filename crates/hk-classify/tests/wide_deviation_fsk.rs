//! T-852: **wide-deviation 2-FSK is FSK.** The `2fsk` dev grid used to stop at modulation index
//! h = 1.6, and deployed FSK does not — POCSAG keys ±4.5 kHz at 2400/1200 Bd (h = 3.75, 7.5), and
//! the ISM sensor scene the pipeline replays through the mock SDR (`fsk_burst_train`, ±9.6 kHz at
//! 4800 Bd) is h = 4. A snippet like that sat outside the fitted `2fsk` envelope on every dimension
//! that scales with the deviation-to-rate ratio (`obw_over_rs` z = 7.7 on the device path), so it
//! came back `unknown` at open-set 1.000 however clean it was.
//!
//! **Blind.** Acceptance seeds only (the densities are fitted on the dev split), the hidden truth is
//! the generator's class, and which snippets count as "wide" is decided by **C14's own measured
//! index**, never by the generator's draw — the classifier sees exactly what it would in
//! production.
//!
//! Red if the generator's range is narrowed back, or if the shipped densities are refitted without
//! it: measured on this population with the pre-T-852 densities, **16 of 46** wide snippets were
//! claimed `fsk` (0.35; the misses `unknown`, 20 of them at open-set 1.000), against **43 of 46**
//! (0.93) with the refitted ones.

use hk_classify::synth::{ACCEPTANCE_SEED_BASE, Class, SynthConfig, generate};
use hk_classify::thresholds::thresholds_of;
use hk_classify::{Classifier, ClassifyRequest, SymbolEstimator};
use hk_model::Timestamp;

/// Acceptance seeds per SNR. 160 draws give ~46 that C14 measures at h ≥ 2; at 30 per SNR a draw
/// of ~29 was too small a population to hold to a floor (it read 26 of 29 where 80 read 70 of 73).
const TRIALS: u64 = 80;

/// C14-measured modulation index from which a snippet counts as wide-deviation: above the old
/// grid's top (1.6) with margin, so nothing here was ever inside the old envelope.
const WIDE_H: f64 = 2.0;

#[test]
fn wide_deviation_2fsk_is_claimed_fsk_not_unknown() {
    let open_set_max = thresholds_of("fsk").unwrap().open_set_max;
    let classifier = Classifier::new();
    let mut c14 = SymbolEstimator::new();
    let (mut wide, mut claimed, mut calls) = (0u32, 0u32, Vec::new());
    // Above the FSK gate (20 dB), where a family may be claimed at all.
    for snr_db in [25.0, 30.0] {
        for seed in ACCEPTANCE_SEED_BASE + 8_520..ACCEPTANCE_SEED_BASE + 8_520 + TRIALS {
            let s = generate(Class::Fsk2, &SynthConfig::new(snr_db, seed));
            let symbols = c14.from_samples(
                &s.symbol_samples,
                s.symbol_sample_rate_hz,
                Some(s.obw_hz),
                Some(snr_db),
            );
            let Some(h) = symbols.as_ref().and_then(|p| p.mod_index_h.value()) else {
                continue;
            };
            if h < WIDE_H {
                continue;
            }
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
            let c = classifier.classify(&req);
            wide += 1;
            if c.family == "fsk" && c.open_set_score < open_set_max {
                claimed += 1;
            }
            calls.push(format!(
                "{snr_db} dB h={h:.2}: {} open-set {:.3}",
                c.family, c.open_set_score
            ));
        }
    }
    eprintln!("[T-852] wide-deviation 2-FSK claimed fsk: {claimed} of {wide}");
    // The population is decided by C14, so its size is measured rather than assumed; the generator
    // draws h log-uniformly over [0.4, 5], and C14 has to
    // measure the index as well, so ~46 of the 160 draws land above 2.
    assert!(
        wide >= 40,
        "too few wide-deviation snippets to measure ({wide}): {calls:#?}"
    );
    // ADR-0016 §7's known top-1 floor is 0.90 over the whole taxonomy; a sub-population the grid now
    // covers is held to the same floor.
    let rate = f64::from(claimed) / f64::from(wide);
    assert!(
        rate >= 0.90,
        "wide-deviation 2-FSK claimed fsk on {claimed} of {wide} ({rate:.2}): {calls:#?}"
    );
}
