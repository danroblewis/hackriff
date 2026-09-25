//! FLEX codewords: BCH(31,21) with generator `0x769` plus an even-parity bit, in **FLEX bit order**.
//!
//! A FLEX codeword is 32 bits on air. This module holds a word as a `u32` whose **bit `k` is the
//! `k`-th bit transmitted** (bit 0 first). The 21 information bits are transmitted first and read
//! least-significant first, so the information value is simply `word & 0x1F_FFFF`; the 10 check
//! bits follow (bits 21–30) and bit 31 is even parity over all 32.
//!
//! As a polynomial the first-transmitted bit is the highest-degree coefficient — the same cyclic
//! code POCSAG uses, sent in the same order, but with the information field's *integer* read the
//! other way round. That orientation is not taken on trust: it was settled on the explorer's live
//! capture (`flex-pagers-930p8`), where all six frame information words have syndrome zero read
//! this way and none do read the other (T-950).
//!
//! Correction is bounded-distance: up to two bit errors among the 31 code bits, by a syndrome
//! table (the code's design distance is 5). The parity bit is then checked over the corrected
//! word and reported, never used to "correct" a third error.

use std::sync::OnceLock;

/// The generator polynomial, `x^10 + x^9 + x^8 + x^6 + x^5 + x^3 + 1`.
pub const GENERATOR: u32 = 0x769;
/// Information bits per codeword.
pub const DATA_BITS: u32 = 21;
/// Mask of the information field of a word.
pub const DATA_MASK: u32 = (1 << DATA_BITS) - 1;
/// Most code-bit errors corrected.
pub const MAX_CORRECTED: u8 = 2;

/// The codeword polynomial of `word` (bit 30 = first-transmitted bit), 31 bits.
fn poly_of(word: u32) -> u32 {
    (word & 0x7FFF_FFFF).reverse_bits() >> 1
}

/// The word (FLEX bit order) of a 31-bit codeword polynomial.
fn word_of(poly: u32) -> u32 {
    (poly << 1).reverse_bits() & 0x7FFF_FFFF
}

/// Remainder of a 31-bit polynomial modulo [`GENERATOR`].
fn remainder(mut p: u32) -> u32 {
    for i in (10..31).rev() {
        if p >> i & 1 == 1 {
            p ^= GENERATOR << (i - 10);
        }
    }
    p
}

/// Syndrome of `word` (FLEX bit order): zero for a codeword.
pub fn syndrome(word: u32) -> u32 {
    remainder(poly_of(word))
}

/// Syndrome → error pattern (FLEX bit order) for every one- and two-bit error.
fn table() -> &'static [u32; 1024] {
    static TABLE: OnceLock<Box<[u32; 1024]>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = Box::new([0u32; 1024]);
        for i in 0..31 {
            let e = 1u32 << i;
            t[syndrome(e) as usize] = e;
            for j in (i + 1)..31 {
                let e2 = e | 1 << j;
                t[syndrome(e2) as usize] = e2;
            }
        }
        t
    })
}

/// The check bits and parity for 21 information bits: a complete codeword in FLEX bit order.
pub fn encode(data: u32) -> u32 {
    let data = data & DATA_MASK;
    // Information bits 0..21 are the high-degree coefficients 30..10.
    let msg = poly_of(data);
    let cw = word_of(msg | remainder(msg));
    let parity = cw.count_ones() & 1;
    cw | parity << 31
}

/// One received codeword, checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checked {
    /// The corrected word (FLEX bit order, parity bit as received).
    pub word: u32,
    /// Code-bit errors corrected (0–2).
    pub corrected: u8,
    /// Even parity holds over the corrected 32 bits.
    pub parity_ok: bool,
}

impl Checked {
    /// The 21 information bits.
    pub fn data(&self) -> u32 {
        self.word & DATA_MASK
    }

    /// Passed with its parity bit holding: the acceptance rule for a word whose content is used.
    ///
    /// Two-error correction alone accepts **about half of all random words** (497 of the 1024
    /// syndromes are correctable), so a correctable syndrome by itself says little; the parity bit
    /// halves that.
    pub fn valid(&self) -> bool {
        self.parity_ok
    }

    /// At most one correction and the parity holding: the rule for words used as **evidence**
    /// (about 1.6 % of random words pass it). A frame's measurements count these.
    pub fn clean(&self) -> bool {
        self.parity_ok && self.corrected <= 1
    }
}

/// Checks and corrects `word`: `None` when it is more than [`MAX_CORRECTED`] errors from any
/// codeword (detected, not corrected).
pub fn check(word: u32) -> Option<Checked> {
    let s = syndrome(word);
    let fix = if s == 0 {
        0
    } else {
        match table()[s as usize] {
            0 => return None,
            e => e,
        }
    };
    let word = word ^ fix;
    Some(Checked {
        word,
        corrected: fix.count_ones() as u8,
        parity_ok: word.count_ones().is_multiple_of(2),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_words_have_zero_syndrome_and_even_parity() {
        for data in [0, 1, 0x1F_FFFF, 0x00_040B, 0x12_3456, 0x0A_AAAA] {
            let w = encode(data);
            assert_eq!(syndrome(w), 0, "{data:06X}");
            assert_eq!(w.count_ones() % 2, 0, "{data:06X}");
            assert_eq!(w & DATA_MASK, data);
        }
    }

    /// The orientation this module claims, pinned against words read off the air: frame
    /// information words of `flex-pagers-930p8` (T-950), bit 0 first on air, read in the sync
    /// marker's polarity. Their complements — the data polarity — are codewords too, since the
    /// all-ones word is one (the generator has odd weight) and complementing 32 bits keeps even
    /// parity.
    #[test]
    fn frame_information_words_off_the_air_are_codewords() {
        for w in [0xBF21_F87Eu32, 0x8181_F77F, 0xF121_F274, 0xAEDF_F374] {
            assert_eq!(syndrome(w), 0, "{w:08X}");
        }
        // The same words read in the other bit order are not.
        for w in [0xBF21_F87Eu32, 0x8181_F77F] {
            assert_ne!(syndrome(w.reverse_bits() >> 1), 0);
        }
    }

    #[test]
    fn corrects_every_one_and_two_bit_error_and_detects_three() {
        let w = encode(0x15_A5A5);
        for i in 0..31 {
            let c = check(w ^ 1 << i).unwrap();
            assert_eq!((c.word, c.corrected, c.parity_ok), (w, 1, true));
            for j in (i + 1)..31 {
                let c = check(w ^ 1 << i ^ 1 << j).unwrap();
                assert_eq!((c.word, c.corrected), (w, 2));
            }
        }
        // Three errors are beyond the design distance: detected (None) or miscorrected, and a
        // miscorrection lands on a *different* codeword. Count how many are caught.
        let mut detected = 0;
        let mut total = 0;
        for i in 0..31 {
            for j in (i + 1)..31 {
                for k in (j + 1)..31 {
                    total += 1;
                    match check(w ^ 1 << i ^ 1 << j ^ 1 << k) {
                        None => detected += 1,
                        Some(c) => assert_ne!(c.word & 0x7FFF_FFFF, w & 0x7FFF_FFFF),
                    }
                }
            }
        }
        assert!(detected > 0 && detected < total);
    }

    #[test]
    fn a_parity_error_alone_is_reported_not_corrected() {
        let w = encode(0x00_1234) ^ 1 << 31;
        let c = check(w).unwrap();
        assert_eq!(c.corrected, 0);
        assert!(!c.parity_ok);
    }
}
