//! Per-block evidence accumulators (ADR-0015 §2.1; T-853 = MAUTO M-2). **Core interface.**
//!
//! A block's [`crate::Block::evidence`] summarises everything since its last `reset()` — the
//! search engine resets between windows (§2.1, "counters are windowed"). The accumulators here
//! are fixed-size (no allocation per item or per call), cleared by the owning block's `reset()`
//! and nothing else, so a mid-window `DISCONTINUITY` does not silently discard what the window
//! already showed.
//!
//! **What a block reports.** `raw` in the metric's natural unit and the support `n` always.
//! `bits`: the closed-form tail for an **analytic** metric (`hk_model::synth::null`), and
//! **0.0** for a **calibrated** one — a block cannot read its calibration table (`hk-synth`
//! owns them and scores `raw` against the `(block@version, metric, n, fill)` cell). 0.0 is the
//! `NoTable` answer (§13.2), so a caller that skips scoring under-reports, never over-reports.
//!
//! | Stage | Block | Metric (group) | `raw` | `n` |
//! |---|---|---|---|---|
//! | S0 | `lowpass` | `snr` | in-band excess over flat noise, dB | output samples |
//! | S1 | `fm_demod` | `snr` | phase-step coherence `1 − E|Δφ|/(π/2)` | samples |
//! | S1 | `am_demod` | `bimodality` (`demod_shape`) | envelope bimodality coefficient | samples |
//! | S1 | `fsk_demod`, `msk_demod` | `bimodality` (`demod_shape`) | discriminator bimodality coefficient | samples |
//! | S1 | `psk_demod` | `evm` | RMS EVM ÷ decision radius (small = evidence) | symbols |
//! | S1 | `subcarrier` | `pilot_lock` (`pilot`) | phase coherence `|Σz^m|/Σ|z|^m` | samples |
//! | S2 | `clock_recovery` | `eye_open` (`eye`), `timing_var` (`soft_quality`) | `(E|y|)²/E y²`; `E e²/E y²` (small = evidence) | symbols |
//! | S3 | `slicer`, `diff_decode`, `nrzi` | `bit_structure` (`bit_shape`) | dependence `1 − H₈/(8·H₁)`, 0 when degenerate | bits |
//! | S3 | `manchester` | `line_violations`, `bit_structure` (`bit_shape`) | violation rate (small = evidence); as above | pairs; bits |
//! | S4 | `sync_search` | `sync_excess` | hits | hits |
//! | S5 | `crc`, `bch`, `parity`, `checksum` | `check_distinct_valid` | independent clean-valid frames | frames tested |
//! | S6 | `text` | `field_fit` | printable characters | characters |
//! | S6 | `fields` | `field_fit` | frames fitting fully | frames |
//!
//! Groups follow ADR-0015 §13.1's measured table; a block publishing one metric at a stage
//! leaves it `undeclared`.

use hk_model::synth::null::{SHORT_PERIOD_MAX_BITS, check_bits, is_short_periodic};
use hk_model::synth::{Evidence, EvidenceSet, GroupId, MetricId, Stage};

/// Pushes one entry; a block never emits more than the set's capacity, so a refusal is a bug
/// caught by the tests, not a runtime path.
pub(crate) fn emit(out: &mut EvidenceSet, e: Evidence) {
    let pushed = out.push(e);
    debug_assert!(
        pushed.is_ok(),
        "a block emitted more than 4 evidence entries"
    );
}

/// A calibrated metric: `raw` and `n`, bits left for `hk-synth` to score (0.0 until it does).
pub(crate) fn calibrated(
    out: &mut EvidenceSet,
    stage: Stage,
    metric: MetricId,
    group: GroupId,
    raw: f64,
    n: u64,
) {
    if !raw.is_finite() || n == 0 {
        return;
    }
    emit(
        out,
        Evidence::new(stage, metric, group, raw as f32, saturate(n), 0.0),
    );
}

/// `n` as the record's `u32`.
pub(crate) fn saturate(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Running sums for the moments of a real series (bimodality).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Moments {
    n: u64,
    s1: f64,
    s2: f64,
    s3: f64,
    s4: f64,
}

impl Moments {
    #[inline]
    pub(crate) fn push(&mut self, x: f64) {
        if !x.is_finite() {
            return;
        }
        let x2 = x * x;
        self.n += 1;
        self.s1 += x;
        self.s2 += x2;
        self.s3 += x2 * x;
        self.s4 += x2 * x2;
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn count(&self) -> u64 {
        self.n
    }

    /// Sarle's bimodality coefficient `(γ² + 1) / κ` (population skewness γ, kurtosis κ):
    /// 1 for a symmetric two-level signal, 1/3 for a Gaussian, lower for heavy tails. `None`
    /// below 4 items or with no spread.
    pub(crate) fn bimodality(&self) -> Option<f64> {
        if self.n < 4 {
            return None;
        }
        let n = self.n as f64;
        let m = self.s1 / n;
        let e2 = self.s2 / n;
        let e3 = self.s3 / n;
        let e4 = self.s4 / n;
        let var = e2 - m * m;
        if var.is_nan() || var <= 1e-24 {
            return None;
        }
        let m3 = e3 - 3.0 * m * e2 + 2.0 * m * m * m;
        let m4 = e4 - 4.0 * m * e3 + 6.0 * m * m * e2 - 3.0 * m.powi(4);
        let skew = m3 / var.powf(1.5);
        let kurt = m4 / (var * var);
        (kurt > 0.0).then(|| ((skew * skew + 1.0) / kurt).clamp(0.0, 1.0))
    }
}

/// Bit-structure sanity (ADR-0015 §1.1 S3: "neither constant nor all-toggle"): how far the bits
/// depart from independent draws at their own ones-rate, `1 − H₈ / (8·H₁)` over overlapping
/// 8-bit windows — 0 for independent bits whatever their bias, larger for structured data
/// (preambles, sync words, framed fields). A degenerate stream (ones or transitions outside
/// 5–95 %: stuck, or a bare clock tone) scores 0.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BitStructure {
    counts: [u32; 256],
    reg: u8,
    filled: u8,
    n: u64,
    ones: u64,
    transitions: u64,
    prev: Option<u8>,
}

impl Default for BitStructure {
    fn default() -> Self {
        Self {
            counts: [0; 256],
            reg: 0,
            filled: 0,
            n: 0,
            ones: 0,
            transitions: 0,
            prev: None,
        }
    }
}

/// Fewest bits the structure metric reports on.
const BIT_STRUCTURE_MIN_BITS: u64 = 32;
/// The degenerate-stream gate, both on the ones-rate and on the transition rate.
const BIT_STRUCTURE_DEGENERATE: f64 = 0.05;

impl BitStructure {
    #[inline]
    pub(crate) fn push(&mut self, bit: u8) {
        let b = bit & 1;
        self.n += 1;
        self.ones += u64::from(b);
        if let Some(p) = self.prev {
            self.transitions += u64::from(p != b);
        }
        self.prev = Some(b);
        self.reg = (self.reg << 1) | b;
        if self.filled < 8 {
            self.filled += 1;
        }
        if self.filled == 8 {
            self.counts[self.reg as usize] += 1;
        }
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    /// The metric, or `None` below [`BIT_STRUCTURE_MIN_BITS`].
    pub(crate) fn raw(&self) -> Option<f64> {
        if self.n < BIT_STRUCTURE_MIN_BITS {
            return None;
        }
        let n = self.n as f64;
        let p1 = self.ones as f64 / n;
        let t = self.transitions as f64 / (n - 1.0);
        let lo = BIT_STRUCTURE_DEGENERATE;
        if !(lo..=1.0 - lo).contains(&p1) || !(lo..=1.0 - lo).contains(&t) {
            return Some(0.0);
        }
        let h1 = -(p1 * p1.log2() + (1.0 - p1) * (1.0 - p1).log2());
        let windows: u64 = self.counts.iter().map(|&c| u64::from(c)).sum();
        if windows == 0 || h1 <= 0.0 {
            return Some(0.0);
        }
        let w = windows as f64;
        let h8: f64 = self
            .counts
            .iter()
            .filter(|&&c| c > 0)
            .map(|&c| {
                let q = f64::from(c) / w;
                -q * q.log2()
            })
            .sum();
        Some((1.0 - h8 / (8.0 * h1)).clamp(0.0, 1.0))
    }

    /// Emits `bit_structure` at S3, group `bit_shape`.
    pub(crate) fn evidence(&self, out: &mut EvidenceSet) {
        if let Some(raw) = self.raw() {
            calibrated(
                out,
                Stage::S3,
                MetricId::BitStructure,
                GroupId::BitShape,
                raw,
                self.n,
            );
        }
    }
}

/// Most distinct frames one window tracks. Beyond it nothing new is counted (fewer bits, never
/// more) — a window with this many independent valid frames is solved many times over.
pub(crate) const DISTINCT_CAPACITY: usize = 256;

/// A bounded set of frame hashes (§2.1: "check blocks count distinct valid frames in a bounded
/// hash set"). Fixed-size: no allocation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DistinctSet {
    hashes: [u64; DISTINCT_CAPACITY],
    len: usize,
}

impl Default for DistinctSet {
    fn default() -> Self {
        Self {
            hashes: [0; DISTINCT_CAPACITY],
            len: 0,
        }
    }
}

impl DistinctSet {
    /// Inserts; whether it was new (and there was room).
    pub(crate) fn insert(&mut self, h: u64) -> bool {
        if self.hashes[..self.len].contains(&h) || self.len == DISTINCT_CAPACITY {
            return false;
        }
        self.hashes[self.len] = h;
        self.len += 1;
        true
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }
}

/// FNV-1a over one-bit-per-byte bits.
pub(crate) fn hash_bits(bits: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in bits {
        h ^= u64::from(b & 1);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^ (bits.len() as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

/// `check_distinct_valid` bookkeeping for a check block (ADR-0015 §2.2, ADR-0022 §4.2, T-210).
///
/// - `tested` counts every unit checked (a frame, or a codeword).
/// - A unit counts as a **difference** only when it passed **without FEC correction** and was
///   not already corrected upstream, is new to the window, and is not a short-period payload.
/// - The chance per unit is `2^−width`, `width` the **smallest** check width among the units
///   tested (a frame-length-dependent check is scored at its weakest).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CheckTally {
    tested: u64,
    distinct: DistinctSet,
    min_width: Option<f64>,
}

impl CheckTally {
    /// Records one checked unit.
    pub(crate) fn record(&mut self, bits: &[u8], clean_valid: bool, width_bits: f64) {
        self.tested += 1;
        if width_bits.is_finite() && width_bits > 0.0 {
            self.min_width = Some(self.min_width.map_or(width_bits, |w| w.min(width_bits)));
        }
        if clean_valid && !is_short_periodic(bits, SHORT_PERIOD_MAX_BITS) {
            self.distinct.insert(hash_bits(bits));
        }
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    /// Emits `check_distinct_valid` at S5 when anything was tested.
    pub(crate) fn evidence(&self, out: &mut EvidenceSet) {
        let Some(width) = self.min_width else {
            return;
        };
        if self.tested == 0 {
            return;
        }
        let d = self.distinct.len() as u64;
        emit(
            out,
            Evidence::new(
                Stage::S5,
                MetricId::CheckDistinctValid,
                GroupId::Undeclared,
                d as f32,
                saturate(self.tested),
                check_bits(d, self.tested, width),
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg_bits(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (x >> 63) as u8
            })
            .collect()
    }

    #[test]
    fn bimodality_separates_two_levels_from_gaussian_like_noise() {
        let mut two = Moments::default();
        for i in 0..1000 {
            two.push(if i % 3 == 0 { 1.0 } else { -1.0 });
        }
        assert!(two.bimodality().unwrap() > 0.9);
        let mut noise = Moments::default();
        let mut x = 1u64;
        for _ in 0..20_000 {
            // Sum of 12 uniforms − 6: near-Gaussian.
            let mut s = -6.0;
            for _ in 0..12 {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                s += (x >> 11) as f64 / (1u64 << 53) as f64;
            }
            noise.push(s);
        }
        let bc = noise.bimodality().unwrap();
        assert!((bc - 1.0 / 3.0).abs() < 0.03, "{bc}");
        assert_eq!(Moments::default().bimodality(), None);
    }

    #[test]
    fn bit_structure_is_near_zero_for_random_bits_and_high_for_structure() {
        let mut r = BitStructure::default();
        for b in lcg_bits(8192, 7) {
            r.push(b);
        }
        assert!(r.raw().unwrap() < 0.03, "{:?}", r.raw());
        let mut s = BitStructure::default();
        // A repeated 24-bit frame: at most 24 distinct 8-bit windows, so H₈ ≤ log₂ 24 and the
        // metric is at least 1 − 4.58/8 ≈ 0.43 — an order of magnitude above random bits.
        let frame = lcg_bits(24, 9);
        for _ in 0..300 {
            for &b in &frame {
                s.push(b);
            }
        }
        assert!(s.raw().unwrap() > 0.4, "{:?}", s.raw());
        // Constant and all-toggle streams are degenerate, not structured.
        let mut c = BitStructure::default();
        let mut t = BitStructure::default();
        for i in 0..512 {
            c.push(1);
            t.push((i % 2) as u8);
        }
        assert_eq!(c.raw(), Some(0.0));
        assert_eq!(t.raw(), Some(0.0));
        assert_eq!(BitStructure::default().raw(), None);
    }

    #[test]
    fn check_tally_counts_clean_new_non_periodic_units_only() {
        let mut t = CheckTally::default();
        let a = lcg_bits(40, 1);
        let b = lcg_bits(40, 2);
        t.record(&a, true, 16.0);
        t.record(&a, true, 16.0); // repeat: one fact
        t.record(&b, false, 16.0); // failed or corrected
        t.record(&[0; 40], true, 16.0); // constant payload
        let mut set = EvidenceSet::new();
        t.evidence(&mut set);
        let e = *set.iter().next().unwrap();
        assert_eq!(e.metric, MetricId::CheckDistinctValid);
        assert_eq!(e.stage, Stage::S5);
        assert_eq!(e.raw, 1.0);
        assert_eq!(e.n, 4);
        assert!(e.bits > 0.0 && e.bits < 16.0, "{}", e.bits);
        t.clear();
        let mut set = EvidenceSet::new();
        t.evidence(&mut set);
        assert!(set.is_empty());
    }

    #[test]
    fn the_distinct_set_is_bounded() {
        let mut d = DistinctSet::default();
        for h in 0..(DISTINCT_CAPACITY as u64 + 10) {
            d.insert(h);
        }
        assert_eq!(d.len(), DISTINCT_CAPACITY);
        assert!(!d.insert(1));
    }
}
