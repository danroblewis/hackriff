//! P25 Phase 1 voice frames: LDU1 link control and LDU2 encryption sync (C23, T-849).
//!
//! T-270 put the encryption check in front of any voice path and said plainly that nothing could
//! pass it: the one statement that may say *clear* is the ALGID, and the ALGID travels in the
//! voice frames on the **granted** channel, which nothing demodulated. This module reads those
//! frames — far enough, and no further, to expose the two fields the call-level questions need:
//!
//! - **LDU1 → link control (LC)**: who is talking to whom — the link-control format, the
//!   manufacturer id, and for a group-voice LC the service options, talkgroup and source unit.
//! - **LDU2 → encryption sync (ES)**: the message indicator, the **ALGID** and the key id.
//!
//! # Metadata only, and nothing decrypted
//!
//! The nine IMBE voice codewords that make up most of every LDU are **skipped**, not decoded:
//! there is no vocoder here, no voice payload is extracted, and no audio can come out of this
//! module. The message indicator is read as the 72 bits it is on the air; it is not used to derive
//! anything, because nothing in the workspace holds a key, a key schedule or a cipher, and nothing
//! may. Reading an algorithm *identifier* is metadata observation (docs/04 §1.3, §8.3).
//!
//! Nor does this module decide what the ALGID means for a call. [`EncryptionSync::encryption`]
//! hands the octet to [`super::tsbk::algid_encryption`] — still the single path in the workspace
//! that can decode a `Clear` — and whether a call's own ES overrides what its grant announced is
//! the caller's decision (T-330), made through [`super::voice::VoicePermit`] like everything else.
//!
//! # The frame, as this module reads it
//!
//! An LDU is 1728 bits on the air (180 ms at 9600 bit/s) = 864 dibits:
//!
//! ```text
//! sync 48 | NID 64 | IMBE1 144 | IMBE2 144 | LC/ES 40 | IMBE3 144 | LC/ES 40 | IMBE4 144 |
//! LC/ES 40 | IMBE5 144 | LC/ES 40 | IMBE6 144 | LC/ES 40 | IMBE7 144 | LC/ES 40 | IMBE8 144 |
//! LSD 32 | IMBE9 144                                          = 1680 bits, + 24 status dibits
//! ```
//!
//! - **Status symbols.** One status dibit follows every 35 information dibits, counted from the
//!   first dibit of the frame sync, so dibit 35, 71, 107 … (every 36th) of the frame is not data.
//!   The first one therefore falls **inside the NID** — 11 dibits after the sync. 864 − 24 = 840
//!   data dibits = 1680 bits, which is exactly the sum above; the arithmetic closing is the check.
//! - **NID**: NAC (12) and DUID (4) protected by a **BCH(63,16,23)** code, plus one trailing
//!   parity bit (64). DUID `0x5` is LDU1 and `0xA` is LDU2. The decoder here is maximum
//!   likelihood over all 2^16 codewords, accepted within the code's `t = 11`.
//! - **LC / ES**: 24 hexbits (6-bit symbols), each carried as a **Hamming(10,6,3)** codeword, in
//!   six 40-bit blocks of four between voice codewords. The 24 hexbits are a Reed–Solomon codeword
//!   over GF(64): **RS(24,12,13)** for the 72-bit LC (12 data hexbits, t = 6) and **RS(24,16,9)**
//!   for the 96-bit ES (16 data hexbits, t = 4), data first on the air.
//! - **LC** (9 octets): `P | SF | LCO(6)`, MFID, then for LCO 0 (group voice channel user) the
//!   service options, a reserved octet, the 16-bit talkgroup and the 24-bit source unit.
//! - **ES** (12 octets): MI (72 bits), ALGID (8), key id (16).
//!
//! # What was checked, and what was not — say it plainly
//!
//! **No independent oracle has confirmed this decoder**, for the same reason T-299 gives for the
//! TSBK decoder: it and the synthetic scene that exercises it (`hkpy.synth.trunking`) were written
//! from the same reading, by the same author, in the same task, so a shared misreading of the
//! standard would pass both. There is no real off-air P25 voice capture in the repository to run
//! it against (T-544 looked for one and found none), and none was consulted while writing this.
//! Everything below is **recalled, not verified against a source during this task**, and the
//! grades say how much internal evidence stands behind each recollection:
//!
//! - **Corroborated by two independent recollections — the BCH(63,16,23) generator.** Computed
//!   here from first principles (the least common multiple of the minimal polynomials of
//!   α¹…α²² over GF(64) with x⁶ + x + 1, [`bch_generator`]), it equals the octal
//!   `6331 1413 6723 5453` recalled from the published table of primitive BCH codes, bit for bit
//!   ([`BCH_63_16_GENERATOR`]; the test `the_nid_generator_is_the_published_bch_63_16_polynomial`
//!   pins the equality). Two routes to the same 48-bit number is not an accident.
//! - **Internally consistent — the Hamming(10,6,3) parity table.** [`HAMMING_10_6_PARITY`] is the
//!   encoder table recalled from the P25 decoders' `hamming` tables; its 64 entries are exactly the
//!   linear span of six generator columns, and those columns give minimum distance 3 (tested), so
//!   the recollection is at least a Hamming(10,6,3) code. Whether it is *P25's* column order is
//!   not something internal consistency can say.
//! - **Corroborated in-repo — the RS field and roots.** GF(64) on x⁶ + x + 1, first consecutive
//!   root α¹; the workspace's own FEC catalogue (`hk-blocks` `reed_solomon`, T-611) names the same
//!   field polynomial `0x43` and first root 1 for "every P25 code".
//! - **Recalled — the frame layout, the status-symbol rule, the DUID values, the LC and ES field
//!   positions.** The status and NID counts are recalled from how the dsd family reads a NID
//!   (NAC 6 dibits, DUID 2, BCH 3, *status*, BCH 20, trailing 1 — 24 + 11 = 35); the LDU body
//!   order from the same family's LDU readers; the field positions from docs/04 §8.3 and the
//!   SDRTrunk/op25 message classes as remembered. The arithmetic closes everywhere it can (1680 =
//!   48 + 64 + 9·144 + 6·40 + 32; 72 = 12 hexbits; 96 = 16 hexbits) but closing arithmetic is not
//!   verification.
//! - **UNVERIFIED and unused — the NID's trailing parity bit.** Written as even parity over the 63
//!   code bits; the decoder does not read it, so a different convention on the air costs nothing.
//! - **Not modelled — the IMBE codewords and the low-speed data.** Skipped by position. Their inner
//!   coding (Golay, Hamming, interleaving, PN scrambling) is irrelevant to reaching LC and ES.
//!
//! The day a real capture exists, the oracle comparison T-299 describes (op25/SDRTrunk behind the
//! C22 process boundary) is what turns "recalled" into "verified". Until then a decode here means
//! "this capture agrees with this module's reading of P25", which on synthetic data is by
//! construction and on real data is evidence.

use hk_model::Encryption;

use super::confirm::{P25_FRAME_SYNC_DIBITS, SYNC_TOLERANCE_DIBITS};
use super::tsbk::algid_encryption;

// ---------------------------------------------------------------------------------------------
// Frame geometry
// ---------------------------------------------------------------------------------------------

/// Dibits in one LDU on the air, status symbols included (1728 bits).
pub const LDU_DIBITS: usize = 864;
/// Every this-many dibits of a frame, counted from the first sync dibit, the last is a status
/// symbol rather than data (35 information dibits, then one status dibit).
pub const STATUS_PERIOD_DIBITS: usize = 36;
/// Information bits in one LDU once its 24 status dibits are removed.
pub const LDU_DATA_BITS: usize = 1680;
/// Bits in the network identifier (NAC 12 + DUID 4 + BCH parity 47 + trailing parity 1).
pub const NID_BITS: usize = 64;
/// Bits in one IMBE voice codeword. Skipped by position, never decoded.
pub const IMBE_BITS: usize = 144;
/// Hexbits in the LC or ES Reed–Solomon codeword.
pub const LDU_HEXBITS: usize = 24;
/// Bits in one Hamming(10,6,3)-coded hexbit.
pub const HEXBIT_CODE_BITS: usize = 10;

/// Where each 40-bit block of four coded hexbits starts, in bits from the first sync bit of the
/// de-statused frame: after IMBE 2, 3, 4, 5, 6 and 7.
///
/// Sync (48) and NID (64) put the body at bit 112; the first block follows two voice codewords
/// (112 + 288 = 400), and each later one follows the next voice codeword (+ 144 + 40 = +184).
pub const LC_BLOCK_STARTS: [usize; 6] = [400, 584, 768, 952, 1136, 1320];

/// DUID of a header data unit.
pub const DUID_HDU: u8 = 0x0;
/// DUID of a terminator without link control.
pub const DUID_TDU: u8 = 0x3;
/// DUID of logical link data unit 1 (carries link control).
pub const DUID_LDU1: u8 = 0x5;
/// DUID of a trunking signalling data unit.
pub const DUID_TSDU: u8 = 0x7;
/// DUID of logical link data unit 2 (carries encryption sync).
pub const DUID_LDU2: u8 = 0xA;
/// DUID of a packet data unit.
pub const DUID_PDU: u8 = 0xC;
/// DUID of a terminator with link control.
pub const DUID_TDULC: u8 = 0xF;

/// Most LDUs decoded from one window, so a pass's cost is bounded a priori.
///
/// An LDU is 180 ms, so a second of voice channel holds 5.6; 256 is ~45 s of continuous voice,
/// far beyond any window the follower buffers (0.5 s built in).
pub const MAX_LDU_PER_WINDOW: usize = 256;

/// Removes the status dibits from dibits that start at a frame's first sync dibit.
///
/// Dibit `i` is a status symbol when `i % 36 == 35`. The input must start on the sync; the rule is
/// positional, so a stream that starts mid-frame would lose data dibits instead.
pub fn strip_status(frame_dibits: &[u8]) -> Vec<u8> {
    frame_dibits
        .iter()
        .enumerate()
        .filter(|(i, _)| i % STATUS_PERIOD_DIBITS != STATUS_PERIOD_DIBITS - 1)
        .map(|(_, &d)| d & 3)
        .collect()
}

/// Inserts a status dibit after every 35 data dibits — the inverse of [`strip_status`], for tests
/// and synthesis.
pub fn insert_status(data_dibits: &[u8], status: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(data_dibits.len() * 36 / 35 + 1);
    for &d in data_dibits {
        out.push(d & 3);
        if out.len() % STATUS_PERIOD_DIBITS == STATUS_PERIOD_DIBITS - 1 {
            out.push(status & 3);
        }
    }
    out
}

/// Unpacks dibits into bits, MSB of each dibit first.
fn dibits_to_bits(dibits: &[u8]) -> Vec<u8> {
    dibits.iter().flat_map(|&d| [(d >> 1) & 1, d & 1]).collect()
}

/// Reads `n ≤ 64` bits MSB-first from a bit slice.
fn read_bits(bits: &[u8], at: usize, n: usize) -> u64 {
    bits[at..at + n]
        .iter()
        .fold(0u64, |acc, &b| (acc << 1) | u64::from(b & 1))
}

/// Writes the low `n` bits of `v` MSB-first into a bit slice.
fn write_bits(bits: &mut [u8], at: usize, n: usize, v: u64) {
    for i in 0..n {
        bits[at + i] = ((v >> (n - 1 - i)) & 1) as u8;
    }
}

// ---------------------------------------------------------------------------------------------
// GF(64) and the codes over it
// ---------------------------------------------------------------------------------------------

/// The field polynomial of GF(64) the P25 codes use: x⁶ + x + 1.
pub const GF64_POLY: u8 = 0x43;

/// GF(64) by log/antilog tables.
struct Gf64 {
    exp: [u8; 126],
    log: [u8; 64],
}

impl Gf64 {
    const fn new() -> Self {
        let mut exp = [0u8; 126];
        let mut log = [0u8; 64];
        let mut x: u8 = 1;
        let mut i = 0;
        while i < 63 {
            exp[i] = x;
            exp[i + 63] = x;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x40 != 0 {
                x ^= GF64_POLY;
            }
            i += 1;
        }
        Self { exp, log }
    }

    #[inline]
    fn mul(&self, a: u8, b: u8) -> u8 {
        if a == 0 || b == 0 {
            0
        } else {
            self.exp[self.log[a as usize] as usize + self.log[b as usize] as usize]
        }
    }

    #[inline]
    fn div(&self, a: u8, b: u8) -> u8 {
        debug_assert!(b != 0);
        if a == 0 {
            0
        } else {
            self.exp[self.log[a as usize] as usize + 63 - self.log[b as usize] as usize]
        }
    }

    /// α^e for any non-negative e.
    #[inline]
    fn alpha(&self, e: usize) -> u8 {
        self.exp[e % 63]
    }
}

static GF: Gf64 = Gf64::new();

/// The BCH(63,16,23) generator polynomial, bit `i` = coefficient of xⁱ (degree 47).
///
/// The published octal form `6331 1413 6723 5453`. [`bch_generator`] derives the same number from
/// the field, and a test pins the two equal — see the module docs for why that matters.
pub const BCH_63_16_GENERATOR: u64 = 0o6331_1413_6723_5453;

/// Derives the BCH(63,16,23) generator from GF(64): the product of `(x − αʲ)` over every exponent
/// in the cyclotomic cosets of 1…22, which is the LCM of their minimal polynomials.
pub fn bch_generator() -> u64 {
    let mut in_coset = [false; 63];
    for i in 1..=22usize {
        let mut j = i;
        while !in_coset[j] {
            in_coset[j] = true;
            j = (2 * j) % 63;
        }
    }
    // Polynomial over GF(64), coefficient i of x^i.
    let mut g: Vec<u8> = vec![1];
    for (j, _) in in_coset.iter().enumerate().filter(|(_, c)| **c) {
        let root = GF.alpha(j);
        let mut next = vec![0u8; g.len() + 1];
        for (i, &c) in g.iter().enumerate() {
            next[i + 1] ^= c;
            next[i] ^= GF.mul(c, root);
        }
        g = next;
    }
    g.iter()
        .enumerate()
        .map(|(i, &c)| {
            debug_assert!(c <= 1, "a BCH generator has binary coefficients");
            u64::from(c) << i
        })
        .fold(0, |a, b| a | b)
}

/// Systematic BCH(63,16) codeword of 16 data bits: data in bits 62…47, parity in 46…0 (bit 62 is
/// first on the air).
fn bch_codeword(data: u16) -> u64 {
    let mut rem = u64::from(data) << 47;
    for bit in (47..63).rev() {
        if rem >> bit & 1 == 1 {
            rem ^= BCH_63_16_GENERATOR << (bit - 47);
        }
    }
    (u64::from(data) << 47) | rem
}

/// The code's error-correcting capability, `⌊(23 − 1) / 2⌋`.
pub const NID_BCH_T: u32 = 11;

/// A decoded network identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nid {
    /// Network access code, 12 bits.
    pub nac: u16,
    /// Data unit id, 4 bits.
    pub duid: u8,
    /// Bit errors corrected in the 63-bit codeword.
    pub errors: u32,
}

/// The 64-bit NID for `nac` and `duid`: the BCH(63,16) codeword then an even-parity bit (the
/// latter UNVERIFIED and never read back — see the module docs).
pub fn nid_encode(nac: u16, duid: u8) -> u64 {
    let data = ((nac & 0x0FFF) << 4) | u16::from(duid & 0x0F);
    let cw = bch_codeword(data);
    (cw << 1) | u64::from(cw.count_ones() & 1)
}

/// Decodes a 64-bit NID by maximum likelihood over all 2¹⁶ codewords.
///
/// The nearest codeword wins if it is within [`NID_BCH_T`] bit errors **and** strictly nearer than
/// every other codeword; a tie is refused rather than broken. The trailing parity bit is ignored.
/// Cost: 65 536 XOR-and-popcounts, the codewords generated by a Gray-code walk so each is one XOR
/// from the last — tens of microseconds, paid once per frame sync.
pub fn nid_decode(word: u64) -> Option<Nid> {
    let r = word >> 1;
    // The codeword of each single data bit; the code is linear, so a Gray-code walk over the data
    // visits every codeword with one XOR per step.
    let rows: [u64; 16] = std::array::from_fn(|i| bch_codeword(1 << i));
    let (mut cw, mut data) = (0u64, 0u16);
    let (mut best, mut best_d, mut tie) = (0u16, u32::MAX, false);
    for step in 0u32..(1 << 16) {
        if step > 0 {
            let flip = step.trailing_zeros() as usize;
            cw ^= rows[flip];
            data ^= 1 << flip;
        }
        let d = (cw ^ r).count_ones();
        if d < best_d {
            (best, best_d, tie) = (data, d, false);
        } else if d == best_d {
            tie = true;
        }
    }
    (best_d <= NID_BCH_T && !tie).then_some(Nid {
        nac: best >> 4,
        duid: (best & 0x0F) as u8,
        errors: best_d,
    })
}

// ---------------------------------------------------------------------------------------------
// Hamming(10,6,3)
// ---------------------------------------------------------------------------------------------

/// The four parity bits of each 6-bit data value, as recalled from the P25 decoders' tables.
///
/// It is linear — every entry is the XOR of the entries for its set bits (32 → 14, 16 → 13,
/// 8 → 11, 4 → 7, 2 → 3, 1 → 12) — and those six columns are distinct, non-zero and of weight ≥ 2,
/// so the code has minimum distance 3. Both properties are tested.
pub const HAMMING_10_6_PARITY: [u8; 64] = [
    0, 12, 3, 15, 7, 11, 4, 8, 11, 7, 8, 4, 12, 0, 15, 3, 13, 1, 14, 2, 10, 6, 9, 5, 6, 10, 5, 9,
    1, 13, 2, 14, 14, 2, 13, 1, 9, 5, 10, 6, 5, 9, 6, 10, 2, 14, 1, 13, 3, 15, 0, 12, 4, 8, 7, 11,
    8, 4, 11, 7, 15, 3, 12, 0,
];

/// A 10-bit codeword: six data bits (first on the air) then four parity bits.
pub fn hamming_10_6_encode(data: u8) -> u16 {
    let d = data & 0x3F;
    (u16::from(d) << 4) | u16::from(HAMMING_10_6_PARITY[d as usize])
}

/// What Hamming decoding did to one hexbit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HammingOutcome {
    /// The codeword was valid.
    Clean,
    /// One bit was corrected.
    Corrected,
    /// The syndrome matches no single-bit error: the data bits are passed through **as received**
    /// for the Reed–Solomon layer to deal with, which is what it is there for.
    Uncorrectable,
}

/// Decodes a 10-bit Hamming codeword, correcting one error.
pub fn hamming_10_6_decode(cw: u16) -> (u8, HammingOutcome) {
    let data = ((cw >> 4) & 0x3F) as u8;
    let syndrome = HAMMING_10_6_PARITY[data as usize] ^ (cw & 0x0F) as u8;
    if syndrome == 0 {
        return (data, HammingOutcome::Clean);
    }
    // A parity-bit error shows its own single bit; a data-bit error shows that bit's column.
    if syndrome.count_ones() == 1 {
        return (data, HammingOutcome::Corrected);
    }
    for bit in 0..6 {
        if HAMMING_10_6_PARITY[1 << bit] == syndrome {
            return (data ^ (1 << bit), HammingOutcome::Corrected);
        }
    }
    (data, HammingOutcome::Uncorrectable)
}

// ---------------------------------------------------------------------------------------------
// Reed–Solomon over GF(64)
// ---------------------------------------------------------------------------------------------

/// A shortened RS(n, k) code over GF(64) with roots α¹ … α^(n−k), data first on the air.
///
/// Errors-only bounded-distance decoding (syndromes → Berlekamp–Massey → Chien → Forney), which
/// refuses rather than guesses: a locator of too high a degree, roots that are not exactly its
/// degree distinct positions inside the shortened word, or a result whose syndromes are not all
/// zero. `hk-blocks` has a general GF(2^m) kernel (T-611); it is crate-private there and
/// `hk-detect` sits below `hk-blocks` in the workspace, so this is the few dozen lines the two
/// P25 codes need rather than a new dependency edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rs64 {
    n: usize,
    k: usize,
}

/// The LC code: RS(24,12,13), t = 6.
pub const RS_24_12: Rs64 = Rs64 { n: 24, k: 12 };
/// The ES code: RS(24,16,9), t = 4.
pub const RS_24_16: Rs64 = Rs64 { n: 24, k: 16 };

impl Rs64 {
    /// Symbols per codeword.
    pub const fn n(&self) -> usize {
        self.n
    }
    /// Data symbols per codeword.
    pub const fn k(&self) -> usize {
        self.k
    }
    /// Correctable symbol errors.
    pub const fn t(&self) -> usize {
        (self.n - self.k) / 2
    }

    fn generator(&self) -> Vec<u8> {
        // Coefficient i of x^i.
        let mut g: Vec<u8> = vec![1];
        for i in 1..=(self.n - self.k) {
            let root = GF.alpha(i);
            let mut next = vec![0u8; g.len() + 1];
            for (j, &c) in g.iter().enumerate() {
                next[j + 1] ^= c;
                next[j] ^= GF.mul(c, root);
            }
            g = next;
        }
        g
    }

    /// The systematic codeword of `data` (`k` symbols, each < 64): data then `n − k` parity.
    pub fn encode(&self, data: &[u8]) -> Vec<u8> {
        assert_eq!(data.len(), self.k, "RS encode needs exactly k symbols");
        let p = self.n - self.k;
        let g = self.generator();
        // Long division of d(x)·x^p by g(x), highest power first.
        let mut work: Vec<u8> = data.iter().map(|&s| s & 0x3F).collect();
        work.extend(std::iter::repeat_n(0u8, p));
        for i in 0..self.k {
            let coef = work[i];
            if coef == 0 {
                continue;
            }
            // g is monic of degree p; its coefficient of x^(p - j) sits at g[p - j].
            for j in 1..=p {
                work[i + j] ^= GF.mul(coef, g[p - j]);
            }
        }
        let mut out: Vec<u8> = data.iter().map(|&s| s & 0x3F).collect();
        out.extend_from_slice(&work[self.k..]);
        out
    }

    /// Syndrome j = r(α^(1+j)), where `r[0]` is the coefficient of x^(n−1).
    fn syndromes(&self, r: &[u8]) -> Vec<u8> {
        (0..self.n - self.k)
            .map(|j| {
                let a = GF.alpha(1 + j);
                r.iter().fold(0u8, |acc, &s| GF.mul(acc, a) ^ s)
            })
            .collect()
    }

    /// Corrects `r` in place and returns the number of symbols corrected, or `None` if the word is
    /// not within `t` errors of a codeword (in which case `r` is left unchanged).
    pub fn decode(&self, r: &mut [u8]) -> Option<usize> {
        if r.len() != self.n {
            return None;
        }
        let s = self.syndromes(r);
        if s.iter().all(|&x| x == 0) {
            return Some(0);
        }
        let two_t = self.n - self.k;
        // Berlekamp–Massey: connection polynomial Λ, coefficient i of x^i.
        let mut lambda = vec![0u8; two_t + 1];
        let mut prev = vec![0u8; two_t + 1];
        lambda[0] = 1;
        prev[0] = 1;
        let (mut l, mut m, mut b) = (0usize, 1usize, 1u8);
        for n in 0..two_t {
            let mut d = s[n];
            for i in 1..=l {
                d ^= GF.mul(lambda[i], s[n - i]);
            }
            if d == 0 {
                m += 1;
                continue;
            }
            let coef = GF.div(d, b);
            let old = lambda.clone();
            for i in 0..=two_t - m {
                lambda[i + m] ^= GF.mul(coef, prev[i]);
            }
            if 2 * l <= n {
                l = n + 1 - l;
                prev = old;
                b = d;
                m = 1;
            } else {
                m += 1;
            }
        }
        if l > self.t() || lambda[l] == 0 {
            return None;
        }
        // Chien: position i (power e = n-1-i) is in error iff Λ(α^-e) = 0.
        let mut positions = Vec::with_capacity(l);
        for i in 0..self.n {
            let e = self.n - 1 - i;
            let x_inv = GF.alpha(63 - e % 63);
            let mut v = 0u8;
            for c in lambda[..=l].iter().rev() {
                v = GF.mul(v, x_inv) ^ c;
            }
            if v == 0 {
                positions.push(i);
            }
        }
        if positions.len() != l {
            // Roots outside the shortened word, or repeated: more errors than t.
            return None;
        }
        // Ω(x) = S(x)Λ(x) mod x^2t.
        let mut omega = vec![0u8; two_t];
        for (i, o) in omega.iter_mut().enumerate() {
            for j in 0..=i.min(l) {
                *o ^= GF.mul(lambda[j], s[i - j]);
            }
        }
        let mut fixed = r.to_vec();
        for &i in &positions {
            let e = self.n - 1 - i;
            let x_inv = GF.alpha(63 - e % 63);
            let mut num = 0u8;
            for c in omega.iter().rev() {
                num = GF.mul(num, x_inv) ^ c;
            }
            // Λ'(x): in characteristic 2 only the odd-power terms survive.
            let mut den = 0u8;
            let mut xp = 1u8; // x_inv^(j-1) for odd j, stepping by x_inv²
            let x_inv2 = GF.mul(x_inv, x_inv);
            for j in (1..=l).step_by(2) {
                den ^= GF.mul(lambda[j], xp);
                xp = GF.mul(xp, x_inv2);
            }
            if den == 0 {
                return None;
            }
            // First consecutive root α¹, so the magnitude is Ω(X⁻¹)/Λ'(X⁻¹) with no X factor.
            let mag = GF.div(num, den);
            if mag == 0 {
                return None;
            }
            fixed[i] ^= mag;
        }
        if fixed.iter().any(|&s| s > 0x3F) || self.syndromes(&fixed).iter().any(|&x| x != 0) {
            return None;
        }
        r.copy_from_slice(&fixed);
        Some(l)
    }
}

// ---------------------------------------------------------------------------------------------
// The two payloads
// ---------------------------------------------------------------------------------------------

/// Link-control octets in an LDU1.
pub const LC_BYTES: usize = 9;
/// Encryption-sync octets in an LDU2.
pub const ES_BYTES: usize = 12;
/// Message-indicator octets in an ES.
pub const MI_BYTES: usize = 9;

/// Link-control opcode of a **group voice channel user** LC.
pub const LCO_GROUP_VOICE: u8 = 0x00;
/// Link-control opcode of a **unit-to-unit voice channel user** LC.
pub const LCO_UNIT_TO_UNIT_VOICE: u8 = 0x03;

/// The link control an LDU1 carried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkControl {
    /// The nine octets as decoded.
    pub bytes: [u8; LC_BYTES],
}

/// The fields of a group-voice LC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupVoiceLc {
    /// The service-options octet, verbatim. As with a grant's, only its verified encryption bit
    /// ([`super::tsbk::SVC_ENCRYPTED`]) means anything here, and that bit may only ever *raise*.
    pub service_options: u8,
    /// Group (talkgroup) address.
    pub talkgroup: u16,
    /// Source unit address.
    pub source: u32,
}

impl LinkControl {
    /// The protected flag: the LC itself is encrypted, so none of its other fields may be read.
    pub const fn protected(&self) -> bool {
        self.bytes[0] & 0x80 != 0
    }
    /// The link-control opcode, 6 bits.
    pub const fn lco(&self) -> u8 {
        self.bytes[0] & 0x3F
    }
    /// The manufacturer id (0 = standard).
    pub const fn mfid(&self) -> u8 {
        self.bytes[1]
    }
    /// The group-voice fields, for an unprotected standard LCO 0 and nothing else.
    ///
    /// A protected LC is ciphertext and a manufacturer LC has its own layout, so both come back
    /// `None` rather than read through a layout that does not apply.
    pub fn group_voice(&self) -> Option<GroupVoiceLc> {
        (!self.protected() && self.lco() == LCO_GROUP_VOICE && self.mfid() == 0).then(|| {
            let b = &self.bytes;
            GroupVoiceLc {
                service_options: b[2],
                talkgroup: u16::from_be_bytes([b[4], b[5]]),
                source: (u32::from(b[6]) << 16) | (u32::from(b[7]) << 8) | u32::from(b[8]),
            }
        })
    }
}

/// The encryption sync an LDU2 carried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncryptionSync {
    /// Message indicator, 72 bits, verbatim. Recorded, never used for anything.
    pub mi: [u8; MI_BYTES],
    /// The algorithm id: 0x80 clear, anything else an algorithm.
    pub algid: u8,
    /// Key id.
    pub key_id: u16,
}

impl EncryptionSync {
    fn from_bytes(b: &[u8; ES_BYTES]) -> Self {
        let mut mi = [0u8; MI_BYTES];
        mi.copy_from_slice(&b[..MI_BYTES]);
        Self {
            mi,
            algid: b[9],
            key_id: u16::from_be_bytes([b[10], b[11]]),
        }
    }

    /// The twelve octets on the air.
    pub fn to_bytes(&self) -> [u8; ES_BYTES] {
        let mut b = [0u8; ES_BYTES];
        b[..MI_BYTES].copy_from_slice(&self.mi);
        b[9] = self.algid;
        b[10..].copy_from_slice(&self.key_id.to_be_bytes());
        b
    }

    /// What this ES's ALGID states, through the one path in the workspace that may decode `Clear`
    /// ([`algid_encryption`]). Deciding what that means for a call is not this module's business.
    pub fn encryption(&self) -> Encryption {
        algid_encryption(self.algid, Some(self.key_id))
    }
}

/// What one LDU carried beyond its NID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LduPayload {
    /// LDU1: link control.
    LinkControl(LinkControl),
    /// LDU2: encryption sync.
    EncryptionSync(EncryptionSync),
}

/// One decoded LDU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LduFrame {
    /// Index of the frame's first sync dibit in the scanned stream.
    pub start_dibit: usize,
    /// The network identifier.
    pub nid: Nid,
    /// Hexbits whose Hamming codeword had one bit corrected.
    pub hamming_corrected: u32,
    /// Hexbits whose Hamming syndrome was uncorrectable (passed through to RS).
    pub hamming_uncorrectable: u32,
    /// Hexbits the Reed–Solomon layer corrected.
    pub rs_corrected: u32,
    /// The payload.
    pub payload: LduPayload,
}

/// Packs 6-bit symbols MSB-first into bytes.
fn hexbits_to_bytes(hex: &[u8]) -> Vec<u8> {
    let bits: Vec<u8> = hex
        .iter()
        .flat_map(|&h| (0..6).rev().map(move |i| (h >> i) & 1))
        .collect();
    bits.chunks(8)
        .map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | b))
        .collect()
}

/// Splits bytes MSB-first into 6-bit symbols.
fn bytes_to_hexbits(bytes: &[u8]) -> Vec<u8> {
    let bits: Vec<u8> = bytes
        .iter()
        .flat_map(|&b| (0..8).rev().map(move |i| (b >> i) & 1))
        .collect();
    bits.chunks(6)
        .map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | b))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Scanning a demodulated stream
// ---------------------------------------------------------------------------------------------

/// What a scan of one channel's dibits found. Reported whether or not anything decoded, so a
/// channel with no voice frames is a legible zero rather than a silence.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LduScan {
    /// Positions whose frame sync was within tolerance with a whole LDU after it.
    pub sync_hits: u32,
    /// Of those, NIDs that decoded.
    pub nid_valid: u32,
    /// Valid NIDs naming a data unit other than LDU1/LDU2 (header, terminator, TSDU …).
    pub other_duid: u32,
    /// LDUs whose LC/ES Reed–Solomon word could not be decoded.
    pub rs_failed: u32,
    /// The decoded LDUs, in stream order.
    pub frames: Vec<LduFrame>,
}

impl LduScan {
    /// Decoded LDU1s.
    pub fn ldu1(&self) -> impl Iterator<Item = (&LduFrame, &LinkControl)> {
        self.frames.iter().filter_map(|f| match &f.payload {
            LduPayload::LinkControl(lc) => Some((f, lc)),
            LduPayload::EncryptionSync(_) => None,
        })
    }
    /// Decoded LDU2s.
    pub fn ldu2(&self) -> impl Iterator<Item = (&LduFrame, &EncryptionSync)> {
        self.frames.iter().filter_map(|f| match &f.payload {
            LduPayload::EncryptionSync(es) => Some((f, es)),
            LduPayload::LinkControl(_) => None,
        })
    }
}

fn sync_matches(dibits: &[u8], i: usize) -> bool {
    let mut miss = 0u32;
    for (k, &want) in P25_FRAME_SYNC_DIBITS.iter().enumerate() {
        if dibits[i + k] != want {
            miss += 1;
            if miss > SYNC_TOLERANCE_DIBITS {
                return false;
            }
        }
    }
    true
}

/// Decodes the LC/ES of one de-statused LDU's bits, given its DUID.
fn decode_payload(bits: &[u8], duid: u8) -> Option<(LduPayload, u32, u32, u32)> {
    let mut hex = [0u8; LDU_HEXBITS];
    let (mut corrected, mut uncorrectable) = (0u32, 0u32);
    for (b, &start) in LC_BLOCK_STARTS.iter().enumerate() {
        for w in 0..4 {
            let cw = read_bits(bits, start + w * HEXBIT_CODE_BITS, HEXBIT_CODE_BITS) as u16;
            let (d, outcome) = hamming_10_6_decode(cw);
            match outcome {
                HammingOutcome::Clean => {}
                HammingOutcome::Corrected => corrected += 1,
                HammingOutcome::Uncorrectable => uncorrectable += 1,
            }
            hex[b * 4 + w] = d;
        }
    }
    let code = match duid {
        DUID_LDU1 => RS_24_12,
        DUID_LDU2 => RS_24_16,
        _ => return None,
    };
    let rs = code.decode(&mut hex)? as u32;
    let bytes = hexbits_to_bytes(&hex[..code.k()]);
    let payload = if duid == DUID_LDU1 {
        let mut b = [0u8; LC_BYTES];
        b.copy_from_slice(&bytes[..LC_BYTES]);
        LduPayload::LinkControl(LinkControl { bytes: b })
    } else {
        let mut b = [0u8; ES_BYTES];
        b.copy_from_slice(&bytes[..ES_BYTES]);
        LduPayload::EncryptionSync(EncryptionSync::from_bytes(&b))
    };
    Some((payload, corrected, uncorrectable, rs))
}

/// Finds and decodes every LDU1/LDU2 in a demodulated C4FM dibit stream.
///
/// A frame is a P25 frame sync within [`SYNC_TOLERANCE_DIBITS`] (the control channel's a-priori
/// tolerance, 9.1e-12 per trial on noise), a NID within the BCH code's `t`, naming LDU1 or LDU2,
/// whose 24 hexbits then decode as a Reed–Solomon codeword. A frame that decodes is consumed whole,
/// so one LDU cannot yield two; anything short of that advances one dibit. At most
/// [`MAX_LDU_PER_WINDOW`] frames are returned.
pub fn scan_ldus(dibits: &[u8]) -> LduScan {
    let mut out = LduScan::default();
    if dibits.len() < LDU_DIBITS {
        return out;
    }
    let last = dibits.len() - LDU_DIBITS;
    let mut i = 0;
    while i <= last && out.frames.len() < MAX_LDU_PER_WINDOW {
        if !sync_matches(dibits, i) {
            i += 1;
            continue;
        }
        out.sync_hits += 1;
        let bits = dibits_to_bits(&strip_status(&dibits[i..i + LDU_DIBITS]));
        debug_assert_eq!(bits.len(), LDU_DATA_BITS);
        let Some(nid) = nid_decode(read_bits(&bits, 48, NID_BITS)) else {
            i += 1;
            continue;
        };
        out.nid_valid += 1;
        if nid.duid != DUID_LDU1 && nid.duid != DUID_LDU2 {
            out.other_duid += 1;
            i += 1;
            continue;
        }
        match decode_payload(&bits, nid.duid) {
            Some((payload, hamming_corrected, hamming_uncorrectable, rs_corrected)) => {
                out.frames.push(LduFrame {
                    start_dibit: i,
                    nid,
                    hamming_corrected,
                    hamming_uncorrectable,
                    rs_corrected,
                    payload,
                });
                i += LDU_DIBITS;
            }
            None => {
                out.rs_failed += 1;
                i += 1;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The inverse, for tests and synthesis
// ---------------------------------------------------------------------------------------------

/// The 864 on-air dibits of an LDU carrying `payload`, with `voice` supplying the IMBE and
/// low-speed-data bits (anything — they are skipped by the decoder) and `status` the status dibit.
///
/// This is the decoder's own reading run backwards, so a round trip proves consistency, not
/// conformance; the synthetic scene in `hkpy.synth.trunking` is a second, separately written
/// encoder of the same reading (see the module docs).
pub fn encode_ldu(
    nac: u16,
    payload: &LduPayload,
    voice: &mut dyn FnMut() -> u8,
    status: u8,
) -> Vec<u8> {
    let (duid, code, bytes): (u8, Rs64, Vec<u8>) = match payload {
        LduPayload::LinkControl(lc) => (DUID_LDU1, RS_24_12, lc.bytes.to_vec()),
        LduPayload::EncryptionSync(es) => (DUID_LDU2, RS_24_16, es.to_bytes().to_vec()),
    };
    let hex = code.encode(&bytes_to_hexbits(&bytes));
    let mut bits = vec![0u8; LDU_DATA_BITS];
    for (k, &d) in P25_FRAME_SYNC_DIBITS.iter().enumerate() {
        write_bits(&mut bits, 2 * k, 2, u64::from(d));
    }
    write_bits(&mut bits, 48, NID_BITS, nid_encode(nac, duid));
    for b in bits.iter_mut().skip(112) {
        *b = voice() & 1;
    }
    for (b, &start) in LC_BLOCK_STARTS.iter().enumerate() {
        for w in 0..4 {
            let cw = hamming_10_6_encode(hex[b * 4 + w]);
            write_bits(
                &mut bits,
                start + w * HEXBIT_CODE_BITS,
                HEXBIT_CODE_BITS,
                u64::from(cw),
            );
        }
    }
    let data: Vec<u8> = bits.chunks(2).map(|c| (c[0] << 1) | c[1]).collect();
    insert_status(&data, status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_model::{EncryptionEvidence, P25_ALGID_CLEAR};

    /// Deterministic xorshift source; `hk-detect` has no RNG dependency.
    struct Xs(u64);
    impl Xs {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[test]
    fn the_frame_arithmetic_closes() {
        assert_eq!(LDU_DIBITS * 2, 1728);
        assert_eq!(LDU_DIBITS / STATUS_PERIOD_DIBITS, 24);
        assert_eq!((LDU_DIBITS - 24) * 2, LDU_DATA_BITS);
        assert_eq!(48 + NID_BITS + 9 * IMBE_BITS + 6 * 40 + 32, LDU_DATA_BITS);
        // Each LC block follows the voice codeword before it.
        assert_eq!(LC_BLOCK_STARTS[0], 48 + NID_BITS + 2 * IMBE_BITS);
        for w in LC_BLOCK_STARTS.windows(2) {
            assert_eq!(w[1] - w[0], 40 + IMBE_BITS);
        }
        // After the last block: IMBE 8, the 32 LSD bits, IMBE 9 — and the frame ends exactly.
        assert_eq!(
            LC_BLOCK_STARTS[5] + 40 + IMBE_BITS + 32 + IMBE_BITS,
            LDU_DATA_BITS
        );
        assert_eq!(LC_BYTES * 8, 12 * 6);
        assert_eq!(ES_BYTES * 8, 16 * 6);
        // The first status symbol falls inside the NID: 24 sync dibits + 11 NID dibits.
        let marked = insert_status(&[0u8; 840], 3);
        assert_eq!(marked.len(), LDU_DIBITS);
        assert_eq!(marked[35], 3);
        assert_eq!(marked.iter().filter(|&&d| d == 3).count(), 24);
        assert_eq!(strip_status(&marked), vec![0u8; 840]);
    }

    /// Two independent routes to the same 48-bit number — the module docs' strongest evidence.
    #[test]
    fn the_nid_generator_is_the_published_bch_63_16_polynomial() {
        assert_eq!(bch_generator(), BCH_63_16_GENERATOR);
        assert_eq!(63 - BCH_63_16_GENERATOR.leading_zeros(), 47);
        // Every codeword is divisible by g, and the code's minimum distance is at least 23 over a
        // sample of codeword pairs (exhaustive d_min over 2^16 is the decoder's own loop).
        let mut rng = Xs(0x5EED_0849);
        for _ in 0..2000 {
            let (a, b) = (rng.next() as u16, rng.next() as u16);
            if a != b {
                assert!((bch_codeword(a) ^ bch_codeword(b)).count_ones() >= 23);
            }
        }
    }

    #[test]
    fn a_nid_decodes_through_eleven_bit_errors() {
        let mut rng = Xs(0x0849_0001);
        for trial in 0..200 {
            let nac = (rng.next() & 0xFFF) as u16;
            let duid = [DUID_LDU1, DUID_LDU2, DUID_HDU, DUID_TDU][trial % 4];
            let mut w = nid_encode(nac, duid);
            let errs = (trial % 12) as u32;
            let mut flipped = 0u64;
            while flipped.count_ones() < errs {
                flipped |= 1 << (1 + rng.below(63));
            }
            w ^= flipped;
            let nid = nid_decode(w).expect("within t");
            assert_eq!((nid.nac, nid.duid, nid.errors), (nac, duid, errs));
        }
    }

    #[test]
    fn the_hamming_table_is_a_linear_distance_3_code_and_corrects_one_error() {
        for d in 0u8..64 {
            let want = (0..6)
                .filter(|b| d >> b & 1 == 1)
                .fold(0u8, |acc, b| acc ^ HAMMING_10_6_PARITY[1 << b]);
            assert_eq!(HAMMING_10_6_PARITY[d as usize], want, "not linear at {d}");
        }
        for a in 0u8..64 {
            for b in (a + 1)..64 {
                let d = (hamming_10_6_encode(a) ^ hamming_10_6_encode(b)).count_ones();
                assert!(d >= 3, "{a} and {b} are only {d} apart");
            }
            let cw = hamming_10_6_encode(a);
            assert_eq!(hamming_10_6_decode(cw), (a, HammingOutcome::Clean));
            for bit in 0..10 {
                assert_eq!(
                    hamming_10_6_decode(cw ^ (1 << bit)),
                    (a, HammingOutcome::Corrected)
                );
            }
        }
    }

    #[test]
    fn reed_solomon_corrects_up_to_t_and_refuses_or_never_invents_beyond() {
        let mut rng = Xs(0x0849_0002);
        for code in [RS_24_12, RS_24_16] {
            for trial in 0..400 {
                let data: Vec<u8> = (0..code.k()).map(|_| rng.below(64) as u8).collect();
                let cw = code.encode(&data);
                assert_eq!(code.syndromes(&cw), vec![0; code.n() - code.k()]);
                let errs = trial % (code.t() + 1);
                let mut r = cw.clone();
                let mut hit = vec![false; code.n()];
                let mut placed = 0;
                while placed < errs {
                    let p = rng.below(code.n() as u64) as usize;
                    if !hit[p] {
                        hit[p] = true;
                        r[p] ^= 1 + rng.below(63) as u8;
                        placed += 1;
                    }
                }
                assert_eq!(
                    code.decode(&mut r),
                    Some(errs),
                    "{code:?} with {errs} errors"
                );
                assert_eq!(r, cw);
            }
            // Beyond t: a refusal, or a *valid* codeword — never a word that fails its own check.
            for _ in 0..200 {
                let data: Vec<u8> = (0..code.k()).map(|_| rng.below(64) as u8).collect();
                let mut r = code.encode(&data);
                for p in 0..=code.t() {
                    r[p * 2] ^= 1 + rng.below(63) as u8;
                }
                if code.decode(&mut r).is_some() {
                    assert!(code.syndromes(&r).iter().all(|&s| s == 0));
                }
            }
        }
    }

    fn lc(talkgroup: u16, source: u32, svc: u8) -> LinkControl {
        let [_, s0, s1, s2] = source.to_be_bytes();
        let [t0, t1] = talkgroup.to_be_bytes();
        LinkControl {
            bytes: [LCO_GROUP_VOICE, 0x00, svc, 0x00, t0, t1, s0, s1, s2],
        }
    }

    /// Frames of an LDU1/LDU2 superframe stream with the fields hidden in the stream alone, then
    /// read back blind — through random leading junk, bit errors, and a corrupted frame.
    #[test]
    fn a_synthetic_voice_stream_yields_its_link_control_and_encryption_sync() {
        let mut rng = Xs(0x0849_0003);
        let (nac, tg, src) = (0x293u16, 2468u16, 1_357_911u32);
        let es = EncryptionSync {
            mi: [0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x11],
            algid: 0x84,
            key_id: 0x1234,
        };
        let mut voice = rng_bit;
        let mut stream: Vec<u8> = (0..517).map(|i| ((i * 7 + 3) % 4) as u8).collect();
        let lead = stream.len();
        for k in 0..6 {
            let payload = if k % 2 == 0 {
                LduPayload::LinkControl(lc(tg, src, 0x00))
            } else {
                LduPayload::EncryptionSync(es)
            };
            stream.extend(encode_ldu(nac, &payload, &mut voice, 2));
        }
        // Symbol errors in every frame outside the sync, well within Hamming + RS; and frame 3
        // (an LDU2) wrecked in its ES blocks beyond anything RS(24,16) can correct.
        for f in 0..6 {
            let base = lead + f * LDU_DIBITS;
            for _ in 0..6 {
                let p = base + 60 + rng.below((LDU_DIBITS - 60) as u64) as usize;
                stream[p] ^= 1 + rng.below(3) as u8;
            }
        }
        let wreck = lead + 3 * LDU_DIBITS;
        for start in LC_BLOCK_STARTS {
            // Destatused bit → on-air dibit, roughly: wreck each block's first two hexbits.
            let at = start / 2;
            let on_air = at + at / 35;
            for d in 0..10 {
                stream[wreck + on_air + d] ^= 2;
            }
        }

        let scan = scan_ldus(&stream);
        let lcs: Vec<_> = scan.ldu1().collect();
        let ess: Vec<_> = scan.ldu2().collect();
        assert_eq!(lcs.len(), 3, "{scan:?}");
        assert_eq!(
            ess.len(),
            2,
            "the wrecked LDU2 must be refused, not guessed: {scan:?}"
        );
        assert!(scan.rs_failed >= 1);
        for (f, l) in &lcs {
            assert_eq!(f.nid.nac, nac);
            let gv = l.group_voice().expect("a group-voice LC");
            assert_eq!((gv.talkgroup, gv.source, gv.service_options), (tg, src, 0));
            assert_eq!((f.start_dibit - lead) % LDU_DIBITS, 0);
        }
        for (f, e) in &ess {
            assert_eq!(**e, es);
            assert_eq!(f.nid.duid, DUID_LDU2);
            let enc = e.encryption();
            assert!(enc.is_encrypted());
            assert_eq!(enc.algid(), Some(0x84));
            assert_eq!(enc.evidence(), Some(EncryptionEvidence::Algid));
        }
    }

    thread_local!(static BITS: std::cell::Cell<u64> = const { std::cell::Cell::new(0x9E37_79B9_7F4A_7C15) });
    fn rng_bit() -> u8 {
        BITS.with(|c| {
            let mut x = c.get();
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            c.set(x);
            (x & 1) as u8
        })
    }

    /// The clear ALGID reaches `Clear` through the ES, and only through `algid_encryption`.
    #[test]
    fn a_clear_algid_in_an_es_states_clear_by_algid_evidence() {
        let es = EncryptionSync {
            mi: [0; MI_BYTES],
            algid: P25_ALGID_CLEAR,
            key_id: 0,
        };
        let mut voice = rng_bit;
        let dibits = encode_ldu(0x293, &LduPayload::EncryptionSync(es), &mut voice, 1);
        let scan = scan_ldus(&dibits);
        let (_, got) = scan.ldu2().next().expect("one LDU2");
        assert!(got.encryption().is_clear());
        assert_eq!(got.encryption().evidence(), Some(EncryptionEvidence::Algid));
    }

    /// Random data — and a control channel's framing — carry no LDU, however long the stream.
    #[test]
    fn noise_and_other_data_units_yield_no_voice_frames() {
        let mut rng = Xs(0x0849_0004);
        let noise: Vec<u8> = (0..400_000).map(|_| rng.below(4) as u8).collect();
        let scan = scan_ldus(&noise);
        assert!(scan.frames.is_empty(), "{scan:?}");

        // A well-formed frame whose NID names a TSDU is counted as another data unit, not decoded.
        let mut voice = rng_bit;
        let mut frame = encode_ldu(0x293, &LduPayload::LinkControl(lc(1, 2, 0)), &mut voice, 1);
        let data = strip_status(&frame);
        let mut bits = dibits_to_bits(&data);
        write_bits(&mut bits, 48, NID_BITS, nid_encode(0x293, DUID_TSDU));
        let data: Vec<u8> = bits.chunks(2).map(|c| (c[0] << 1) | c[1]).collect();
        frame = insert_status(&data, 1);
        let scan = scan_ldus(&frame);
        assert!(scan.frames.is_empty());
        assert_eq!((scan.nid_valid, scan.other_duid), (1, 1));
    }

    /// A frame written by the **separately written** Python encoder (`hkpy.synth.trunking.
    /// p25_ldu_dibits`, rng seed 1: an LDU2 with MI 00..08, ALGID 0x84, key id 0x1234), and the RS
    /// parity that encoder computes for data 1..=12. The two implementations represent every code
    /// differently (see the module docs), so agreement here is agreement between two writings of
    /// the same reading — not conformance, but not one implementation agreeing with itself either.
    #[test]
    fn a_frame_from_the_python_encoder_decodes_here() {
        const PY_LDU2: [&str; 9] = [
            "111113113311333313133333022103222322221032332300212220222133022311203332201232113110323030021222",
            "222121303112032300023120012113202000023131331312202031233210232322212313320321323202300323221021",
            "011130300022000000000002010130020321232320002303111312020302222331003000000030232033220010231202",
            "223303311322000003000310031011233022311002230001000031001123121020101212231200212032111100012120",
            "302113213102013020032130010202312230032302231122100301023202011130000332112332010102211223021033",
            "133301133201020013002022331010321322010223220210313320010012012030310231321331133112233210302002",
            "020130331201233033303303033010102032212000231200032133111300310021222132031213202111021223331000",
            "010332012102211110323311112332320131223330320212322322012232122020312330332033033022020331233033",
            "211130110101310012031212213020312320213023000110310133223132011003332022320013030330133113322222",
        ];
        let dibits: Vec<u8> = PY_LDU2.concat().bytes().map(|c| c - b'0').collect();
        assert_eq!(dibits.len(), LDU_DIBITS);
        let scan = scan_ldus(&dibits);
        let (f, es) = scan.ldu2().next().expect("the Python LDU2 decodes");
        assert_eq!(es.mi, [0, 1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!((es.algid, es.key_id), (0x84, 0x1234));
        assert_eq!((f.nid.nac, f.nid.errors, f.rs_corrected), (0x293, 0, 0));

        let data: Vec<u8> = (1..=12).collect();
        assert_eq!(
            RS_24_12.encode(&data)[12..],
            [26, 33, 46, 33, 12, 58, 60, 23, 17, 40, 1, 58]
        );
    }

    /// A protected or manufacturer-specific LC is recorded, never read through the group layout.
    #[test]
    fn a_protected_or_manufacturer_lc_is_not_read_as_group_voice() {
        let mut p = lc(10, 20, 0);
        p.bytes[0] |= 0x80;
        assert!(p.protected() && p.group_voice().is_none());
        let mut m = lc(10, 20, 0);
        m.bytes[1] = 0x90;
        assert!(m.group_voice().is_none());
        assert_eq!(
            lc(10, 20, 0x40).group_voice().map(|g| g.service_options),
            Some(0x40)
        );
    }
}
