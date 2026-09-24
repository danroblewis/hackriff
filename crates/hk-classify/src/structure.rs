//! T-233: an emission's modulation-structure statistic, `envelope_shape` ([`ModulationStructure`]).
//!
//! # Status (T-594): a measurement with no consumer
//!
//! T-233 built this as a discriminator for entity resolution and put it on
//! `hk_model::Fingerprint` as split-only evidence. **T-594 removed it from the fingerprint**: no
//! pipeline stage ever called [`modulation_structure`], so over real replays the field was on 0
//! stored fingerprints and took part in 0 comparisons (`hk-pipeline`'s
//! `fingerprint_field_census`). It was removed rather than wired because every producer that
//! holds IQ also writes an exact `family`, and every family those producers write is
//! constant-envelope (analogue FM, the FSK chain, the trunking control channel) — for which this
//! statistic reads 1.00 by construction, so a wired comparison could only have split on what moves
//! a constant envelope (multipath fading, noise-correction error): the look, not the emitter.
//! `hk_model::cluster`'s module docs carry the full reasoning.
//!
//! The statistic itself is kept, with its derivation and its tests, because it is a correct
//! measurement of the emission (`am` against `wfm`, `bpsk` against `qpsk`) that C15 may use; it
//! is **not** an entity-resolution feature, and nothing in the inventory reads it. The
//! entity-resolution merge/split rates T-233 measured with it (`structure_rates`,
//! `structure-probe`) were removed with the comparison they measured.
//!
//! # The statistic, and why it separates what it separates
//!
//! [`ModulationStructure::envelope_shape`] is `μ₄ = E|s|⁴/(E|s|²)²` of the emission. By Jensen it
//! is ≥ 1, and it is **exactly 1 for any constant-envelope emission** — every FSK, MSK, FM and
//! unkeyed carrier — because then `|s|` is a constant and both moments are powers of it. It rises
//! with amplitude structure, which is what separates:
//!
//! - **an amplitude modulation from an angle one** — `am` ≈ 1.18 against `wfm` ≈ 1.00, which is
//!   the definition of the two rather than a property of any generator;
//! - **a binary phase alphabet from a quaternary one** — `bpsk` ≈ 1.38 against `qpsk` ≈ 1.20. A
//!   shaped BPSK's symbol transitions are all 180°, so the trajectory passes through the origin at
//!   every one of them and the envelope collapses there; a QPSK's are 90° three times in four and
//!   miss the origin. The fourth moment is where that shows up, because it weights the deep nulls.
//!
//! What it does **not** separate is `2fsk` from `gfsk` or `msk`: all three are constant-envelope
//! by construction and all three read 1.00. That is not a shortcoming of this statistic but a fact
//! about those three — see `hk_model::cluster::Fingerprint::compare` for what followed from it
//! (the exact family gate).
//!
//! # Why it measures the emission and not the observation (T-281's rule)
//!
//! It is a ratio of two expectations — never an extremum, never a maximum over method variants,
//! never a count that grows with how long anyone watched. Lengthening the record estimates it
//! **better** and moves it not at all. It is also invariant to amplitude (both moments scale
//! together), to a residual carrier offset and to any phase rotation, since it sees only `|s|`.
//! That is the property a cross-observation comparison needs: two observations were watched
//! differently by construction, so a statistic that moved with the watch would split or merge on
//! the watch.
//!
//! The one thing that does move it is noise, and that is why the value is reported **with the
//! sigma this module measured it to** rather than bare, so a consumer can compare in sigmas and
//! fit no width.
//!
//! # A second dimension, built and rejected
//!
//! T-233 also built `if_concentration` — `IQR/(P95−P5)` of the instantaneous frequency — to catch
//! what the envelope misses. On its face it works: `bpsk` reads 0.02 against `qpsk`'s 0.34,
//! because a shaped BPSK's instantaneous frequency is a train of π impulses between flat runs, and
//! it separates the pair at 10 dB where the envelope statistic cannot.
//!
//! It was dropped because it **measures the observation**. Those quantiles count samples, so the
//! ratio depends on how many samples a symbol transition occupies — and that is set by the
//! analysis geometry, which is pinned to the *measured* OBW99 and therefore moves with the
//! observation. Measured on one `bpsk` emitter at 25 dB, the same waveform read 0.014 over a
//! 16 384-sample record and 0.078 over a 6 144-sample one: a six-fold move with nothing changed
//! but the length of the look. Its within-record spread does not see this, because every block
//! shares the one geometry, so it reported a confident sigma alongside a wrong number — which is
//! the precise failure mode T-281 catalogued and T-310 found again in `cyclic_db`.
//!
//! One dimension that measures the emission beats two where the second is known to measure the
//! look. It is recorded here rather than deleted because the next person to reach for an
//! instantaneous-frequency shape statistic should know what it costs.

use num_complex::Complex32;

/// One emission's modulation structure: the dimensionless statistic and the uncertainty its
/// producer measured it to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModulationStructure {
    /// `E|s|⁴/(E|s|²)²` of the emission, noise-corrected. `1.0` = constant envelope.
    pub envelope_shape: f64,
    /// 1σ on [`Self::envelope_shape`]. Never zero in practice.
    pub envelope_shape_sigma: f64,
}

impl ModulationStructure {
    /// The value and its sigma are finite and non-negative (`μ₄ ≥ 0` for any distribution, and an
    /// uncertainty is not negative).
    pub fn is_valid(&self) -> bool {
        self.envelope_shape.is_finite()
            && self.envelope_shape >= 0.0
            && self.envelope_shape_sigma.is_finite()
            && self.envelope_shape_sigma >= 0.0
    }
}

/// Fewest samples the structure will be measured over. Below this the within-record spread has
/// too little to divide into [`SPREAD_BLOCKS`], so the module abstains rather than report a value
/// with an uncertainty it could not measure — the same rule [`crate::features`] applies to every
/// input it is not given.
pub const MIN_SAMPLES: usize = 512;

/// How well the in-band SNR handed to [`modulation_structure`] is assumed to be known, dB.
///
/// `envelope_shape` is the **noise-corrected** fourth moment, and the correction's only uncertain
/// input is `ρ` itself, so this is where that dimension's sigma comes from. ±3 dB is the S5
/// working figure for a blind in-band SNR estimate off an 8-bit front end with no preselector
/// (ADR-0016 "8-bit front end"; the family gates are stated in whole 5 dB steps for the same
/// reason). It is deliberately pessimistic: an over-tight sigma is what turns one emitter into
/// two inventory rows, and the error directions here are not symmetric.
pub const ENVELOPE_SNR_UNCERTAINTY_DB: f64 = 3.0;

/// One emission's modulation structure, or `None` when the snippet is too short
/// ([`MIN_SAMPLES`]), carries no power, or is handed an SNR that is not a number.
///
/// `snr_db` is the **in-band** SNR the emission was measured at, as C13 reports it.
///
/// # The noise correction, in closed form
///
/// For `x = s + n` with `n` circular complex Gaussian of variance `σ²` and `P = E|s|²`:
///
/// ```text
/// E|x|² = P + σ²
/// E|x|⁴ = E|s|⁴ + 4Pσ² + 2σ⁴
/// ```
///
/// so with `u = 1/ρ = σ²/P` the emission's own fourth moment comes out exactly:
///
/// ```text
/// μ₄ₛ = μ₄ₓ·(1 + u)² − 4u − 2u²
/// ```
///
/// That is not a fit: it is what additive circular Gaussian noise contributes, and it is why
/// `wfm`, `2fsk` and every other constant-envelope class read 1.00 after correction at any SNR
/// where the correction is applied at all, having read 1.09 at 15 dB before it.
///
/// # Where the sigma comes from
///
/// Two terms, in quadrature.
///
/// - **The correction's own error.** Differentiating the line above, `∂μ₄ₛ/∂u = 2μ₄ₓ(1+u) − 4 −
///   4u` (see [`slope`], which also says why that expression is not used bare), and an SNR known
///   to ±[`ENVELOPE_SNR_UNCERTAINTY_DB`] leaves `|Δu| = u·(10^(d/10) − 1)`. The product is what
///   the correction can be wrong by, and it is what makes the dimension quietly stop
///   discriminating as the SNR falls instead of confidently splitting emitters down there.
/// - **The record's own within-record spread**: the statistic recomputed over [`SPREAD_BLOCKS`]
///   contiguous blocks, and their sample standard deviation. That is how much the figure moves
///   depending on which part of the emission was looked at — a packet's alternating preamble
///   against its i.i.d. data, a burst that changed part-way — and it is taken whole rather than
///   divided down to a standard error of the whole-record figure, for the reason given on
///   [`block_spread`]. It is the direct form of the question "does this move with the
///   observation?", asked of each observation rather than assumed once for all of them.
///
pub fn modulation_structure(samples: &[Complex32], snr_db: f64) -> Option<ModulationStructure> {
    if samples.len() < MIN_SAMPLES || !snr_db.is_finite() {
        return None;
    }
    let u = 10f64.powf(-snr_db / 10.0);

    let env = envelope_shape(samples, u)?;
    let env_spread = block_spread(samples.len(), |r| {
        envelope_shape(&samples[r], u).map(|e| e.corrected)
    });
    // ∂μ₄ₛ/∂u, times the |Δu| an SNR known to ±d dB leaves.
    let du = u * (10f64.powf(ENVELOPE_SNR_UNCERTAINTY_DB / 10.0) - 1.0);
    let env_sigma = (slope(env.raw, u) * du).hypot(env_spread);

    let s = ModulationStructure {
        envelope_shape: env.corrected,
        envelope_shape_sigma: env_sigma,
    };
    s.is_valid().then_some(s)
}

/// `|∂μ₄ₛ/∂u|` — how far the corrected value moves per unit error in the noise-to-signal ratio —
/// evaluated **both** at the measured `μ₄ₓ` and at the dimension's calibration point, and taken as
/// the larger.
///
/// Differentiating `μ₄ₛ = μ₄ₓ(1+u)² − 4u − 2u²` with respect to `u` at fixed `μ₄ₓ` gives
/// `2μ₄ₓ(1+u) − 4 − 4u`, which **passes through zero at μ₄ₓ ≈ 2** — and an emission whose measured
/// fourth moment happens to sit near there is not thereby better corrected, only differently
/// wrong. Left alone, that zero reports a sigma of nearly nothing exactly where the correction is
/// least trustworthy: measured at 10 dB, `nbfm` — constant-envelope, true value 1.0 — read 1.61
/// over a full record and 0.95 over a two-thirds one while claiming σ ≈ 0.09.
///
/// So the sensitivity is floored at the value it takes where the dimension is **calibrated**: the
/// constant-envelope case `μ₄ₛ = 1`, i.e. `μ₄ₓ = (1 + 4u + 2u²)/(1+u)²`, where the derivative is
/// `−2` as `u → 0`. That calibration — a constant envelope reading exactly 1.0 — is the whole
/// claim this dimension makes, so it is the right place to ask how sensitive it is.
///
/// The effect is that the dimension **stops discriminating on its own** as the SNR falls, rather
/// than needing an SNR floor bolted on: at 25 dB the band is 0.03 against a `bpsk`/`qpsk`
/// separation of 0.18, at 20 dB 0.09, and by 15 dB it is 0.27 and the comparison concludes
/// nothing. Nothing is tuned to make that happen; it is what the correction's own error does.
fn slope(mu4x: f64, u: f64) -> f64 {
    let at = |m: f64| (2.0 * m * (1.0 + u) - 4.0 - 4.0 * u).abs();
    let reference = (1.0 + 4.0 * u + 2.0 * u * u) / (1.0 + u).powi(2);
    at(mu4x).max(at(reference))
}

/// Contiguous blocks a statistic's within-record spread is measured over ([`block_spread`]).
///
/// Four, because two is not enough to see a trend and eight leaves each block too short to
/// estimate a fourth moment on: with the analysis geometry at 2 samples per OBW99, a 16 k-sample
/// snippet is ~4 k samples and a few hundred symbols per block at eight, which is where the block
/// figures start reporting their own sampling error instead of the emission's variation.
const SPREAD_BLOCKS: usize = 4;

/// Sample standard deviation of `stat` over [`SPREAD_BLOCKS`] contiguous blocks of `len`.
///
/// This is how much the statistic moves depending on **which part of the emission** you looked
/// at — a packet's alternating preamble against its i.i.d. data, a burst that changed part-way —
/// and it is taken whole rather than divided down to a standard error of the whole-record figure,
/// because that is not what it is for. A second observation, elsewhere in time, sees a different
/// part of the emission; it is not entitled to agree more closely than the record already agreed
/// with itself.
fn block_spread(len: usize, stat: impl Fn(std::ops::Range<usize>) -> Option<f64>) -> f64 {
    let b = len / SPREAD_BLOCKS;
    if b == 0 {
        return 0.0;
    }
    let v: Vec<f64> = (0..SPREAD_BLOCKS)
        .filter_map(|i| stat(i * b..(i + 1) * b))
        .filter(|x| x.is_finite())
        .collect();
    if v.len() < 2 {
        return 0.0;
    }
    let n = v.len() as f64;
    let m = v.iter().sum::<f64>() / n;
    (v.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (n - 1.0)).sqrt()
}

struct Envelope {
    /// `μ₄ₓ` as measured, noise included.
    raw: f64,
    /// `μ₄ₛ`, the noise contribution removed.
    corrected: f64,
}

fn envelope_shape(x: &[Complex32], u: f64) -> Option<Envelope> {
    if x.is_empty() {
        return None;
    }
    let n = x.len() as f64;
    let mut m2 = 0.0;
    let mut m4 = 0.0;
    for s in x {
        let p = s.norm_sqr() as f64;
        m2 += p;
        m4 += p * p;
    }
    m2 /= n;
    m4 /= n;
    if !(m2.is_finite() && m2 > 0.0 && m4.is_finite()) {
        return None;
    }
    let raw = m4 / (m2 * m2);
    // μ₄ₛ = μ₄ₓ(1+u)² − 4u − 2u². Clamped at 0: the correction can overshoot below a constant
    // envelope's 1.0 when the SNR handed in is optimistic, and a negative fourth moment is not a
    // measurement of anything.
    let corrected = (raw * (1.0 + u).powi(2) - 4.0 * u - 2.0 * u * u).max(0.0);
    Some(Envelope { raw, corrected })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::{Class, SynthConfig, generate};

    fn of(class: Class, snr_db: f64, seed: u64) -> ModulationStructure {
        let s = generate(class, &SynthConfig::new(snr_db, seed));
        modulation_structure(&s.samples, snr_db).expect("a full snippet is measurable")
    }

    /// The correction is derived, not fitted, and this is the check that it is right: it is what
    /// puts every **constant-envelope** class — the ones whose `μ₄` is 1.0 *by construction*, not
    /// by measurement — back on 1.0, at an SNR where the uncorrected figure is visibly not.
    #[test]
    fn the_noise_correction_returns_a_constant_envelope_to_one() {
        for class in [
            Class::Wfm,
            Class::Nbfm,
            Class::Fsk2,
            Class::Gfsk,
            Class::Msk,
        ] {
            for snr in [15.0, 20.0, 25.0, 30.0] {
                let v: Vec<f64> = (0..8).map(|s| of(class, snr, s).envelope_shape).collect();
                let m = v.iter().sum::<f64>() / v.len() as f64;
                assert!(
                    (m - 1.0).abs() < 0.05,
                    "{class:?} at {snr} dB: {m} (constant envelope is 1.0 by construction)"
                );
            }
        }
        // Uncorrected, the same measurement is 1.09 at 15 dB: the correction is doing the work,
        // and the 15 dB row above is not passing because the noise happened to be small.
        let s = generate(Class::Fsk2, &SynthConfig::new(15.0, 3));
        let raw = envelope_shape(&s.samples, 0.0).unwrap().corrected;
        assert!(
            raw > 1.05,
            "uncorrected μ₄ₓ at 15 dB should be visibly > 1: {raw}"
        );
    }

    /// The statistic may not move with how long the emission was watched — the T-281 defect this
    /// discriminator exists to avoid, and the one that disqualified the `if_concentration`
    /// dimension recorded in the module docs. A quarter-length record must land inside the sigma
    /// the full-length one reported.
    #[test]
    fn the_value_does_not_move_with_the_length_of_the_record() {
        for class in [
            Class::Bpsk,
            Class::Qpsk,
            Class::Am,
            Class::Wfm,
            Class::Fsk2,
            Class::Gfsk,
        ] {
            for seed in 0..10 {
                for snr in [20.0, 25.0, 30.0] {
                    let full = generate(class, &SynthConfig::new(snr, seed));
                    let long = modulation_structure(&full.samples, snr).unwrap();
                    let short =
                        modulation_structure(&full.samples[..full.samples.len() / 4], snr).unwrap();
                    let band = 3.0
                        * (long.envelope_shape_sigma.powi(2) + short.envelope_shape_sigma.powi(2))
                            .sqrt();
                    assert!(
                        (long.envelope_shape - short.envelope_shape).abs() <= band,
                        "{class:?}/{seed} at {snr} dB: {long:?} vs {short:?} (band {band})"
                    );
                }
            }
        }
    }

    /// The sigma is not decoration: it widens where the correction is fragile, so the dimension
    /// stops discriminating as the SNR falls rather than splitting emitters down there. The check
    /// is that the band crosses the `bpsk`/`qpsk` separation of 0.18 between the PSK gate and
    /// 10 dB above it, with no constant tuned to put it there.
    #[test]
    fn the_band_widens_past_the_separation_it_has_to_resolve_as_the_snr_falls() {
        let band = |snr: f64| {
            let v: Vec<f64> = (0..12)
                .map(|s| of(Class::Bpsk, snr, s).envelope_shape_sigma)
                .collect();
            3.0 * 2f64.sqrt() * v.iter().sum::<f64>() / v.len() as f64
        };
        let (gate, sep) = (15.0_f64, 0.18_f64);
        assert!(
            band(gate) > sep,
            "at the PSK gate it should conclude nothing: {}",
            band(gate)
        );
        assert!(
            band(gate + 10.0) < sep / 2.0,
            "10 dB above it should resolve the pair with margin: {}",
            band(gate + 10.0)
        );
        // …and it falls monotonically in between: an SNR term plus a content term, nothing else.
        assert!(band(gate) > band(gate + 5.0) && band(gate + 5.0) > band(gate + 10.0));
    }

    #[test]
    fn it_abstains_rather_than_guess() {
        let s = generate(Class::Bpsk, &SynthConfig::new(25.0, 1));
        assert!(modulation_structure(&s.samples[..MIN_SAMPLES - 1], 25.0).is_none());
        assert!(modulation_structure(&s.samples, f64::NAN).is_none());
        // A dead channel has no envelope and no frequency spread to take a ratio of.
        assert!(modulation_structure(&vec![Complex32::new(0.0, 0.0); 4096], 25.0).is_none());
    }
}
