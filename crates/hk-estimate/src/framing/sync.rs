//! Preamble detection and cross-burst sync-word discovery (docs/04 §7.3; C21 "blind
//! discovery: align bursts on the preamble end, take the longest common bit substring").
//!
//! # Preamble
//! The longest run of alternating bits (`0101…`), tolerating isolated bit errors: a single
//! flipped bit splits a run into `[run ≥ 8][1][run ≥ 8]`, which is merged back.
//!
//! # Sync word
//! Bursts are aligned on their preamble end `e` (the first bit that breaks alternation), with
//! polarity normalised against the burst with the longest preamble. Per offset `d` from `e` the
//! majority bit and its share give a consensus; the **common region** is `[0, D)` where `D` is
//! the first offset whose share falls below the agreement threshold.
//!
//! **The start is ambiguous by construction.** `…10|0010…` (a `1010` preamble, sync 2DD4) and
//! `…01|0000…` (a `0101` preamble, sync 0C5F) look the same locally: a run of zeros after a
//! `1`, with the true boundary one bit apart. No local rule separates them; a CRC span does
//! (the inference re-aligns the word by up to ±3 bits on the span, [`SyncAnchor::CrcAligned`]).
//! Without a CRC the **convention** is spike S5's for the 915 MHz truth: the word starts at the
//! first bit of the equal pair that breaks alternation (`e − 1`). Rule, with `R = D`:
//!
//! - `R ≥ 15`: 16-bit word at `[e − 1, e + 15)` ([`SyncAnchor::AlternationBreak`]); the
//!   remaining `R − 15` common bits are a **fixed header** (constant fields such as a device
//!   ID, indistinguishable from a longer sync in a single-emitter corpus);
//! - `13 ≤ R < 15`: 16-bit word **end-anchored** at `D` ([`SyncAnchor::CommonRegionEnd`]);
//! - `7 ≤ R < 13`: 8-bit word at `e − 1`; `5 ≤ R < 7`: 8-bit word end-anchored; else no sync.
//!
//! Each burst is then searched for the learned word in both polarities with a bit-error budget,
//! requiring an alternating preamble of at least `locate_min_preamble_bits` immediately before
//! it (a 16-bit word with one error matches random bits at ~2.6·10⁻⁴ per position otherwise).

use serde::{Deserialize, Serialize};

use super::bits::hamming;

/// An alternating preamble run `[start, end)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreambleRun {
    /// First bit.
    pub start: usize,
    /// One past the last alternating bit (the first bit that breaks alternation).
    pub end: usize,
}

impl PreambleRun {
    /// Length, bits.
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    /// Always false for a found run.
    pub fn is_empty(&self) -> bool {
        self.end == self.start
    }
}

/// The longest error-tolerant alternating run of at least `min_len` bits.
pub fn find_preamble(bits: &[u8], min_len: usize) -> Option<PreambleRun> {
    let n = bits.len();
    if n < 2 {
        return None;
    }
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut s = 0;
    for i in 1..=n {
        if i == n || bits[i] & 1 == bits[i - 1] & 1 {
            runs.push((s, i));
            s = i;
        }
    }
    // Merge [A ≥ 8][singleton][C ≥ 8]: one flipped bit inside a preamble.
    let mut merged: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < runs.len() {
        let (cs, mut ce) = runs[i];
        let mut j = i + 1;
        while j + 1 < runs.len() {
            let mid = runs[j];
            let next = runs[j + 1];
            if ce - cs >= 8 && mid.1 - mid.0 == 1 && next.1 - next.0 >= 8 {
                ce = next.1;
                j += 2;
            } else {
                break;
            }
        }
        merged.push((cs, ce));
        i = j;
    }
    merged
        .into_iter()
        .filter(|&(s, e)| e - s >= min_len)
        .max_by(|a, b| (a.1 - a.0).cmp(&(b.1 - b.0)).then(b.0.cmp(&a.0)))
        .map(|(start, end)| PreambleRun { start, end })
}

/// Alternating bits ending at `end` (exclusive), tolerating isolated errors (no two
/// consecutive, at most `1 + len/32`). Polarity-agnostic.
pub fn alternating_bits_before(bits: &[u8], end: usize) -> usize {
    let end = end.min(bits.len());
    if end == 0 {
        return 0;
    }
    let reference = bits[end - 1] & 1;
    let mut last_good = end - 1;
    let mut errors = 0usize;
    let mut prev_err = false;
    let mut k = end - 1;
    while k > 0 {
        k -= 1;
        let expected = if (end - 1 - k) % 2 == 0 {
            reference
        } else {
            1 - reference
        };
        if bits[k] & 1 == expected {
            last_good = k;
            prev_err = false;
        } else {
            errors += 1;
            if prev_err || errors > 1 + (end - k) / 32 {
                break;
            }
            prev_err = true;
        }
    }
    end - last_good
}

/// One sync-word occurrence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncHit {
    /// Bit index of the first sync bit.
    pub bit_index: usize,
    /// Bit errors against the word.
    pub errors: usize,
    /// The burst's bits are the complement of the word (FSK polarity flipped).
    pub inverted: bool,
    /// Alternating preamble bits immediately before the word.
    pub preamble_bits: usize,
}

/// Finds `sync` in `bits` (either polarity, ≤ `max_errors`) with ≥ `min_preamble_bits` of
/// alternation right before it. Preference: fewest errors, then closest to `expected`, then
/// earliest.
pub fn locate_sync(
    bits: &[u8],
    sync: &[u8],
    max_errors: usize,
    min_preamble_bits: usize,
    expected: Option<usize>,
) -> Option<SyncHit> {
    let l = sync.len();
    if l == 0 || bits.len() < l + min_preamble_bits {
        return None;
    }
    let mut best: Option<(SyncHit, usize)> = None;
    for p in min_preamble_bits..=bits.len() - l {
        let d = hamming(&bits[p..p + l], sync);
        let (errors, inverted) = if d <= l - d {
            (d, false)
        } else {
            (l - d, true)
        };
        if errors > max_errors {
            continue;
        }
        let pre = alternating_bits_before(bits, p);
        if pre < min_preamble_bits {
            continue;
        }
        let dist = expected.map_or(0, |e| p.abs_diff(e));
        let hit = SyncHit {
            bit_index: p,
            errors,
            inverted,
            preamble_bits: pre,
        };
        let better = match &best {
            None => true,
            Some((b, bd)) => (errors, dist) < (b.errors, *bd),
        };
        if better {
            best = Some((hit, dist));
        }
    }
    best.map(|(h, _)| h)
}

/// A streaming sync-word correlator (the `sync_search` block, T-087): bits in, the Hamming
/// distance between the last `bits` bits and the word out. The word's MSB is its first bit on
/// air.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncCorrelator {
    word: u64,
    mask: u64,
    bits: u32,
    reg: u64,
    fill: u32,
}

impl SyncCorrelator {
    /// A correlator for a `bits`-bit word (1..=64). `None` if the word is wider.
    pub fn new(word: u64, bits: u32) -> Option<Self> {
        if !(1..=64).contains(&bits) {
            return None;
        }
        let mask = if bits == 64 {
            u64::MAX
        } else {
            (1u64 << bits) - 1
        };
        (word & !mask == 0).then_some(Self {
            word,
            mask,
            bits,
            reg: 0,
            fill: 0,
        })
    }

    /// Word length, bits.
    pub fn bits(&self) -> u32 {
        self.bits
    }

    /// Forgets received bits: the next `bits` bits must all arrive before a match.
    pub fn reset(&mut self) {
        self.reg = 0;
        self.fill = 0;
    }

    /// The last `bits` bits received (first on air = MSB).
    pub fn register(&self) -> u64 {
        self.reg & self.mask
    }

    /// Pushes one bit; the distance to the word once `bits` bits arrived since the last reset.
    #[inline]
    pub fn push(&mut self, bit: u8) -> Option<u32> {
        self.reg = (self.reg << 1) | u64::from(bit & 1);
        if self.fill < self.bits {
            self.fill += 1;
        }
        (self.fill == self.bits).then(|| ((self.reg ^ self.word) & self.mask).count_ones())
    }
}

/// How the learned sync was placed in the common region.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SyncAnchor {
    /// Starts at the first bit of the equal pair that breaks the preamble alternation (spike S5
    /// convention); extra common bits are a fixed header.
    AlternationBreak,
    /// Ends at the common-region end; absorbed alternation-continuing bits.
    CommonRegionEnd,
    /// Supplied by the caller (a prior framing model).
    Prior,
    /// Shifted by 1–3 bits so that it ends where a validated CRC span starts (the anchor rule
    /// had absorbed alternation-continuing or fixed-header bits).
    CrcAligned,
}

/// The consensus result of [`learn_sync`].
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LearnedSync {
    /// Word bits, in the polarity of the reference burst.
    pub bits: Vec<u8>,
    /// Common bits after the preamble end.
    pub common_bits: usize,
    /// Lowest majority share over the word.
    pub agreement: f64,
    /// Bursts aligned.
    pub aligned: usize,
    /// Placement.
    pub anchor: SyncAnchor,
    /// Fixed-header bits after the word inside the common region.
    pub fixed_header_bits: usize,
}

/// Cross-burst consensus after the preamble end. `runs[i]` is burst `i`'s preamble.
pub(crate) fn learn_sync(
    bursts: &[&[u8]],
    runs: &[Option<PreambleRun>],
    agreement_threshold: f64,
    min_bursts: usize,
    max_look: usize,
) -> Option<LearnedSync> {
    let usable: Vec<usize> = (0..bursts.len()).filter(|&i| runs[i].is_some()).collect();
    if usable.len() < min_bursts {
        return None;
    }
    let reference = *usable
        .iter()
        .max_by_key(|&&i| (runs[i].unwrap().len(), std::cmp::Reverse(i)))?;
    let window = |i: usize| -> Vec<u8> {
        let e = runs[i].unwrap().end;
        let lo = e.saturating_sub(2);
        let hi = (e + 16).min(bursts[i].len());
        bursts[i][lo..hi].to_vec()
    };
    let ref_win = window(reference);
    let flips: Vec<bool> = (0..bursts.len())
        .map(|i| {
            runs[i].is_some() && {
                let w = window(i);
                let n = w.len().min(ref_win.len());
                n > 0 && 2 * hamming(&w[..n], &ref_win[..n]) > n
            }
        })
        .collect();
    let min_count = min_bursts.max(usable.len().div_ceil(2));
    let consensus = |d: isize| -> Option<(u8, f64)> {
        let mut ones = 0usize;
        let mut count = 0usize;
        for &i in &usable {
            let pos = runs[i].unwrap().end as isize + d;
            if pos < 0 || pos as usize >= bursts[i].len() {
                continue;
            }
            let b = bursts[i][pos as usize] & 1 ^ u8::from(flips[i]);
            ones += usize::from(b);
            count += 1;
        }
        if count < min_count {
            return None;
        }
        let share = ones.max(count - ones) as f64 / count as f64;
        Some((u8::from(2 * ones > count), share))
    };
    let mut common = 0usize;
    while common < max_look {
        match consensus(common as isize) {
            Some((_, share)) if share >= agreement_threshold => common += 1,
            _ => break,
        }
    }
    let len = if common >= 13 {
        16
    } else if common >= 5 {
        8
    } else {
        return None;
    };
    let (start, anchor, fixed) = if common + 1 >= len {
        (-1isize, SyncAnchor::AlternationBreak, common + 1 - len)
    } else {
        (
            common as isize - len as isize,
            SyncAnchor::CommonRegionEnd,
            0,
        )
    };
    let mut word = Vec::with_capacity(len);
    let mut agreement: f64 = 1.0;
    for d in start..start + len as isize {
        let (b, share) = consensus(d)?;
        word.push(b);
        agreement = agreement.min(share);
    }
    Some(LearnedSync {
        bits: word,
        common_bits: common,
        agreement,
        aligned: usable.len(),
        anchor,
        fixed_header_bits: fixed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::bits::parse_bit_string;

    #[test]
    fn sync_correlator_counts_errors_after_a_full_word() {
        let mut c = SyncCorrelator::new(0b1011, 4).unwrap();
        assert!(SyncCorrelator::new(0b10110, 4).is_none());
        let got: Vec<Option<u32>> = [1u8, 0, 1, 1, 1, 0, 1, 0]
            .iter()
            .map(|&x| c.push(x))
            .collect();
        assert_eq!(got[..3], [None, None, None]);
        assert_eq!(got[3], Some(0));
        assert_eq!(c.register(), 0b1010);
        assert_eq!(got[7], Some(1));
        c.reset();
        assert_eq!(c.push(1), None);
        let mut w = SyncCorrelator::new(u64::MAX, 64).unwrap();
        assert_eq!((0..64).filter_map(|_| w.push(1)).last(), Some(0));
    }

    fn b(s: &str) -> Vec<u8> {
        parse_bit_string(s).unwrap()
    }

    #[test]
    fn preamble_tolerates_one_flip() {
        let mut bits = b("1100");
        bits.extend(std::iter::repeat_n([1u8, 0], 16).flatten());
        bits.extend(b("0010110111010100"));
        bits[4 + 13] ^= 1;
        let run = find_preamble(&bits, 16).unwrap();
        assert_eq!(run.start, 3);
        assert_eq!(run.end, 4 + 32);
        assert_eq!(alternating_bits_before(&bits, 36), 33);
    }

    #[test]
    fn sync_end_anchored_when_word_continues_the_preamble() {
        // Sync 0000110001011111 after a 0101 preamble: its first 0 continues the alternation.
        let sync = b("0000110001011111");
        let mut bursts = Vec::new();
        for k in 0..6u32 {
            let mut v: Vec<u8> = std::iter::repeat_n([0u8, 1], 20).flatten().collect();
            v.extend(&sync);
            // Varying bits after the sync.
            v.extend((0..40).map(|i| (((k * 7 + i * 13) ^ (i >> 2)) % 2) as u8));
            bursts.push(v);
        }
        let refs: Vec<&[u8]> = bursts.iter().map(|v| v.as_slice()).collect();
        let runs: Vec<_> = refs.iter().map(|v| find_preamble(v, 16)).collect();
        let l = learn_sync(&refs, &runs, 0.85, 3, 64).unwrap();
        assert_eq!(l.bits, sync);
        assert_eq!(l.anchor, SyncAnchor::AlternationBreak);
        let hit = locate_sync(&bursts[0], &sync, 1, 16, None).unwrap();
        assert_eq!(hit.bit_index, 40);
        assert!(!hit.inverted);
        let inv: Vec<u8> = bursts[1].iter().map(|x| 1 - x).collect();
        let hit = locate_sync(&inv, &sync, 1, 16, None).unwrap();
        assert_eq!((hit.bit_index, hit.inverted), (40, true));
    }
}
