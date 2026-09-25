//! Analytic nulls (ADR-0015 §2.2's first list; T-853 = MAUTO M-2).
//!
//! The S4–S6 metrics have closed-form tails, so their bits need no calibration table and are
//! not bounded by any table's `log₂ N` (§13.2: "`cap_j = 32` for S4 … is reachable only
//! analytically"). They live here, beside the vocabulary, because the **blocks** that emit them
//! (`hk-blocks`) must compute them and cannot depend on `hk-synth`; `hk-synth` re-exports this
//! module as `hk_synth::nulls`.
//!
//! Every function returns a **lower bound** on the true significance — the Chernoff bound on a
//! tail is ≥ the tail, so `−log₂` of it is ≤ the tail's bits — and every degenerate input
//! (nothing tested, non-finite, fewer hits than chance) returns **0 bits**. A null here never
//! over-reports; that is the same fail-closed rule as §13.2's refusal.
//!
//! The Poisson/binomial bound is the one `hk_estimate::assist::sync` uses for its
//! `significance_bits` (the "assist formula" §2.2 names); it is restated rather than imported so
//! `hk-model` stays below `hk-estimate`.

/// Frames whose checked bits repeat with a period of at most this many bits (constant,
/// alternating, a short fill word) are not counted as independent differences (ADR-0022 §4.2,
/// after `assist::codes`): an all-zeros frame passes many CRCs with `init = 0` trivially, and a
/// beacon repeating one payload is one fact, not many.
pub const SHORT_PERIOD_MAX_BITS: usize = 16;

/// `−log₂` of the Chernoff bound on seeing **≥ `k`** events when `lambda` are expected
/// (Poisson). 0 when `k ≤ lambda`, and for non-finite or non-positive `lambda`.
pub fn chernoff_poisson_bits(k: f64, lambda: f64) -> f64 {
    if !(k.is_finite() && lambda.is_finite()) || lambda <= 0.0 || k <= lambda {
        return 0.0;
    }
    let nats = k * (k / lambda).ln() - (k - lambda);
    (nats / std::f64::consts::LN_2).max(0.0)
}

/// `−log₂` of the Chernoff bound on **≥ `k`** successes in `trials` Bernoulli(`p`) trials (the
/// binomial KL bound). 0 when `k ≤ trials·p`, when nothing was tried, or on non-finite input.
/// `k = trials` gives exactly `trials · log₂(1/p)` — every trial passing a `w`-bit check is
/// `w` bits per trial.
pub fn chernoff_binomial_bits(k: f64, trials: f64, p: f64) -> f64 {
    if !(k.is_finite() && trials.is_finite() && p.is_finite()) || trials <= 0.0 || k <= 0.0 {
        return 0.0;
    }
    let p = p.clamp(1e-300, 1.0);
    if p >= 1.0 || k <= trials * p {
        return 0.0;
    }
    let x = (k / trials).min(1.0);
    let rest = if x < 1.0 {
        (1.0 - x) * ((1.0 - x) / (1.0 - p)).ln()
    } else {
        0.0
    };
    let nats = trials * (x * (x / p).ln() + rest);
    (nats / std::f64::consts::LN_2).max(0.0)
}

/// Chance that a `width`-bit pattern matches with at most `max_errors` bit errors at one position
/// of random bits, counting the complemented pattern too when `either_polarity` (the
/// `polarity: either` sync search accepts both).
pub fn chance_per_position(width: u32, max_errors: u32, either_polarity: bool) -> f64 {
    let lp = width as usize;
    if lp == 0 {
        return 1.0;
    }
    let mut c = 1.0f64; // C(lp, k)
    let mut sum = 0.0;
    for k in 0..=(max_errors as usize).min(lp) {
        sum += c;
        c = c * (lp - k) as f64 / (k + 1) as f64;
    }
    let pol = if either_polarity { 2.0 } else { 1.0 };
    (pol * sum * 2f64.powi(-(lp as i32))).min(1.0)
}

/// `sync_excess` (ADR-0015 §2.2): the Poisson tail of `hits` sync matches over `positions`
/// searched positions, **minus the pattern width** — the assist `significance_bits` formula,
/// "any pattern of that width could have been found". Floored at 0.
///
/// The width is subtracted here as §2.2 states it, independently of the engine's `L_j`; for a
/// sync word the search proposed that is a charge the engine also makes, so this is
/// conservative (it can only under-report), never generous.
pub fn sync_excess_bits(
    hits: u64,
    positions: u64,
    width: u32,
    max_errors: u32,
    either_polarity: bool,
) -> f32 {
    if hits == 0 || positions == 0 {
        return 0.0;
    }
    let lambda = positions as f64 * chance_per_position(width, max_errors, either_polarity);
    let bits = chernoff_poisson_bits(hits as f64, lambda) - f64::from(width);
    bits.max(0.0) as f32
}

/// `check_distinct_valid` (ADR-0015 §2.2, ADR-0022 §4.2): `differences` independent frames that
/// passed a `width_bits`-bit check **without FEC correction** among `tested` frames checked, each
/// passing by chance with probability `2^−width_bits`. The binomial tail: equal to
/// `width × differences` when every tested frame passed, and `width − log₂(tested)`-like when one
/// frame in many passed — ADR-0022 §4.2's "with `tested` framings and one valid, the check
/// contributes `width − log₂(tested)` bits". Floored at 0.
pub fn check_bits(differences: u64, tested: u64, width_bits: f64) -> f32 {
    if differences == 0 || tested == 0 || !width_bits.is_finite() || width_bits <= 0.0 {
        return 0.0;
    }
    let tested = tested.max(differences);
    chernoff_binomial_bits(differences as f64, tested as f64, (-width_bits).exp2()) as f32
}

/// `field_fit` (ADR-0015 §2.2): `fits` of `trials` items fitting where a random item fits with
/// probability `p_chance` — the binomial against random frames. Floored at 0; `p_chance ≥ 1`
/// (anything fits) is 0 bits.
pub fn field_fit_bits(fits: u64, trials: u64, p_chance: f64) -> f32 {
    chernoff_binomial_bits(fits as f64, trials as f64, p_chance) as f32
}

/// Whether `bits` (one bit per byte, 0/1) repeats with some period `1..=max_period` over its
/// whole length — a constant, alternating or short-fill payload (see [`SHORT_PERIOD_MAX_BITS`]).
/// A frame no longer than `max_period` is not called periodic.
pub fn is_short_periodic(bits: &[u8], max_period: usize) -> bool {
    let n = bits.len();
    (1..=max_period.min(n.saturating_sub(1)))
        .filter(|&p| n > p && n > max_period)
        .any(|p| (p..n).all(|i| bits[i] & 1 == bits[i - p] & 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fixed_check_passing_every_frame_is_width_bits_per_frame() {
        // One CRC-24 frame tested, one passed: 24 bits (ADR-0022 §4.2's template-fixed row).
        assert!((check_bits(1, 1, 24.0) - 24.0).abs() < 1e-3);
        // Three CRC-16 frames, all passing: 48.
        assert!((check_bits(3, 3, 16.0) - 48.0).abs() < 1e-3);
    }

    #[test]
    fn one_valid_frame_among_many_tested_pays_for_the_multiplicity() {
        // One of 256 framings valid under a 24-bit check: about 24 − 8 bits, never 24.
        let b = check_bits(1, 256, 24.0);
        assert!(b < 24.0 - 7.0 && b > 24.0 - 10.0, "{b}");
        // Chance-level passing earns nothing: 1 in 2^8 tested at an 8-bit check.
        assert_eq!(check_bits(1, 256, 8.0), 0.0);
    }

    #[test]
    fn degenerate_inputs_are_zero_bits_never_negative() {
        assert_eq!(check_bits(0, 10, 16.0), 0.0);
        assert_eq!(check_bits(3, 0, 16.0), 0.0);
        assert_eq!(check_bits(3, 3, f64::NAN), 0.0);
        assert_eq!(sync_excess_bits(0, 1000, 16, 0, false), 0.0);
        assert_eq!(field_fit_bits(10, 10, 1.0), 0.0);
        assert_eq!(chernoff_poisson_bits(1.0, 2.0), 0.0);
    }

    #[test]
    fn sync_excess_subtracts_the_pattern_width() {
        // 3 hits of a 32-bit word in 10⁴ positions: λ ≈ 2.3e-6, the tail is ~56 bits; less 32.
        let b = sync_excess_bits(3, 10_000, 32, 0, false);
        assert!(b > 20.0 && b < 30.0, "{b}");
        // Chance-level hits of a 16-bit word (either polarity) over 10⁴ positions: 0.
        assert_eq!(sync_excess_bits(1, 10_000, 16, 0, true), 0.0);
        // Errors and polarity only ever widen the chance: fewer bits.
        assert!(sync_excess_bits(3, 10_000, 32, 2, true) < b);
    }

    #[test]
    fn chance_per_position_counts_error_balls_and_polarity() {
        assert!((chance_per_position(8, 0, false) - 1.0 / 256.0).abs() < 1e-12);
        assert!((chance_per_position(8, 0, true) - 2.0 / 256.0).abs() < 1e-12);
        assert!((chance_per_position(8, 1, false) - 9.0 / 256.0).abs() < 1e-12);
    }

    #[test]
    fn printable_text_beats_random_bytes() {
        // 40 of 40 characters printable where a random byte is printable 95/256 of the time.
        let b = field_fit_bits(40, 40, 95.0 / 256.0);
        assert!((f64::from(b) - 40.0 * (256.0f64 / 95.0).log2()).abs() < 1e-2);
        // At chance, nothing.
        assert_eq!(field_fit_bits(37, 100, 95.0 / 256.0), 0.0);
    }

    #[test]
    fn short_period_payloads_are_recognised() {
        assert!(is_short_periodic(&[0; 64], SHORT_PERIOD_MAX_BITS));
        let alt: Vec<u8> = (0..64).map(|i| (i % 2) as u8).collect();
        assert!(is_short_periodic(&alt, SHORT_PERIOD_MAX_BITS));
        let fill: Vec<u8> = (0..64).map(|i| u8::from(i % 12 < 5)).collect();
        assert!(is_short_periodic(&fill, SHORT_PERIOD_MAX_BITS));
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let random: Vec<u8> = (0..64)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x & 1) as u8
            })
            .collect();
        assert!(!is_short_periodic(&random, SHORT_PERIOD_MAX_BITS));
        // Too short to call.
        assert!(!is_short_periodic(&[0; 8], SHORT_PERIOD_MAX_BITS));
    }
}
