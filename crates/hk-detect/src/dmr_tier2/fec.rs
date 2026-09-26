//! The forward error correction a **conventional DMR** burst carries (T-989).
//!
//! `trunk::dmr` (T-271) deliberately flattened DMR's channel coding: a Tier III control channel
//! there is a sync followed by a contiguous CSBK, with no BPTC, no interleaving and no slot type.
//! That was honest for what T-271 claimed — *that* a control channel exists — and it is not enough
//! for this module's claim, which is what a **real off-air burst** is: to read a colour code, a
//! data type or a header out of one, the burst's actual coding has to be undone.
//!
//! # What is verified here, and how
//!
//! No standards document and no reference decoder was available while this was written, so every
//! constant is a **recollection**, and each one is put through an arithmetic check that a
//! mis-recollection would fail. That is the strongest verification available offline, and it is
//! not nothing: the first Golay generator tried here (`x¹²+x¹⁰+x⁸+x⁵+x⁴+x³+1`) produced a
//! distance-**5** code and was rejected by exactly the test below before any of it was written
//! down.
//!
//! - **Golay(20,8)** — [`GOLAY_20_8_GEN`]. Exhaustively checked to generate a **(20,8,8)** code
//!   ([`tests::the_golay_20_8_generator_gives_a_distance_8_code`]), and only **four** of the 4096
//!   degree-12 generators do, so a wrong polynomial almost certainly fails the check. d = 8 also
//!   pins the correction radius at 3 by arithmetic ⌊(8−1)/2⌋, which is what the published name
//!   "Golay(20,8,7)" claims.
//! - **Hamming(15,11,3)**, **Hamming(13,9,3)** and **Hamming(7,4,3)** — the parity equations in
//!   [`H15_11_EQS`], [`H13_9_EQS`] and [`H7_4_EQS`]. Each is checked to have distinct, non-zero,
//!   non-unit syndrome columns and exhaustive minimum distance 3. They corroborate *each other*
//!   too: the nine syndrome columns of the (13,9) code are **exactly the last nine columns of the
//!   (15,11) code, in order** ([`tests::the_two_bptc_hamming_codes_share_one_column_table`]) —
//!   which is what two shortenings of one published table look like, and not what two independent
//!   mis-recollections look like.
//! - **BPTC(196,96)** — the 181-step interleave and the 13×15 matrix. 181 is invertible mod 196
//!   (its inverse is 13), so the interleave is a permutation; 13 × 15 + 1 = 196 and 9 × 11 − 3 =
//!   96, so the matrix and its R bits account for every bit. Both are tests.
//!
//! # UNVERIFIED, and what does not depend on it
//!
//! - **Reed–Solomon(12,9)** over GF(2⁸): the field polynomial [`RS_FIELD_POLY`] and the roots
//!   α⁰ α¹ α² could not be corroborated. The code below *is* an RS(12,9) — its parity is checked
//!   to detect every single- and double-symbol error ([`tests::rs_12_9_detects_one_and_two_symbol_errors`])
//!   — but whether it is **DMR's** RS(12,9) is not established here. Nothing this crate identifies
//!   depends on it: a full-LC header whose parity does not check is *counted and not published*,
//!   never guessed at, and the sync search and CACH that carry the identification never touch it.
//! - **Which of the four equivalent (20,8,8) cyclic codes** the air interface uses. Same shape of
//!   risk, same containment: a slot type that decodes is only believed when a second burst agrees
//!   with it (`super::Tier2Scan`), so a wrong generator reads as "colour code unknown" rather than
//!   as a confident wrong number.

/// Generator of the Golay(20,8) code the slot type is protected with: x¹²+x¹¹+x¹⁰+x⁹+x⁸+x⁵+x²+1.
///
/// Degree 12, held with the x¹² term, so the low 13 bits are the polynomial.
pub const GOLAY_20_8_GEN: u16 = 0x1F25;

/// Bits in a Golay(20,8) codeword.
pub const GOLAY_20_8_BITS: usize = 20;

/// Symbol errors the Golay(20,8) code corrects: ⌊(d−1)/2⌋ = 3 at the measured d = 8.
pub const GOLAY_20_8_CORRECT: u32 = 3;

/// The systematic Golay(20,8) codeword of `data`: the 8 data bits, then 12 parity bits.
pub fn golay_20_8_encode(data: u8) -> u32 {
    let msg = u32::from(data) << 12;
    let mut rem = msg;
    for i in (12..20).rev() {
        if rem >> i & 1 == 1 {
            rem ^= u32::from(GOLAY_20_8_GEN) << (i - 12);
        }
    }
    msg | (rem & 0xFFF)
}

/// The data of the codeword nearest `cw` (20 bits, data first), with the bits it had to correct.
///
/// Nearest-codeword over all 256 codewords — exact, not a syndrome table, and cheap at this size.
/// `None` when the nearest is further than [`GOLAY_20_8_CORRECT`] away, or when two codewords tie:
/// a refusal, never a guess. (At d = 8 a tie inside the radius is impossible; the check is there
/// so that a *wrong* generator, which is the residual risk, cannot silently answer either.)
pub fn golay_20_8_decode(cw: u32) -> Option<(u8, u32)> {
    let word = cw & 0xF_FFFF;
    let (mut best, mut best_data, mut tie) = (u32::MAX, 0u8, false);
    for d in 0..=255u8 {
        let e = (golay_20_8_encode(d) ^ word).count_ones();
        if e < best {
            best = e;
            best_data = d;
            tie = false;
        } else if e == best {
            tie = true;
        }
    }
    (best <= GOLAY_20_8_CORRECT && !tie).then_some((best_data, best))
}

/// Parity equations of the Hamming(15,11,3) code that protects a BPTC **row**: parity bit `j` is
/// the XOR of the data bits listed in `H15_11_EQS[j]`.
pub const H15_11_EQS: [&[usize]; 4] = [
    &[0, 1, 2, 3, 5, 7, 8],
    &[1, 2, 3, 4, 6, 8, 9],
    &[2, 3, 4, 5, 7, 9, 10],
    &[0, 1, 2, 4, 6, 7, 10],
];

/// Parity equations of the Hamming(13,9,3) code that protects a BPTC **column**.
pub const H13_9_EQS: [&[usize]; 4] = [
    &[0, 1, 3, 5, 6],
    &[0, 1, 2, 4, 6, 7],
    &[0, 1, 2, 3, 5, 7, 8],
    &[0, 2, 4, 5, 8],
];

/// Parity equations of the Hamming(7,4,3) code that protects the CACH's TACT bits.
pub const H7_4_EQS: [&[usize]; 3] = [&[0, 1, 2], &[0, 1, 3], &[0, 2, 3]];

/// What decoding one Hamming codeword did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HammingFix {
    /// Parity already agreed.
    Clean,
    /// One bit at this index was flipped back.
    Corrected(usize),
    /// A syndrome that names no position — possible only for a shortened code, and a refusal.
    Uncorrectable,
}

/// The parity bits of `data` (one bit per element, 0/1) under `eqs`, low bit = equation 0.
fn parity_of(data: &[u8], eqs: &[&[usize]]) -> u32 {
    let mut p = 0u32;
    for (j, eq) in eqs.iter().enumerate() {
        let mut b = 0u8;
        for &i in eq.iter() {
            b ^= data[i] & 1;
        }
        p |= u32::from(b) << j;
    }
    p
}

/// Appends the `eqs` parity bits to `data` in place: `out[k..]` are the parity bits.
pub fn hamming_encode(data: &[u8], eqs: &[&[usize]], out: &mut [u8]) {
    let k = data.len();
    out[..k].copy_from_slice(data);
    let p = parity_of(data, eqs);
    for j in 0..eqs.len() {
        out[k + j] = (p >> j & 1) as u8;
    }
}

/// Corrects one bit error in `bits` (`k` data bits then `eqs.len()` parity bits) in place.
pub fn hamming_correct(bits: &mut [u8], k: usize, eqs: &[&[usize]]) -> HammingFix {
    let want = parity_of(&bits[..k], eqs);
    let mut got = 0u32;
    for j in 0..eqs.len() {
        got |= u32::from(bits[k + j] & 1) << j;
    }
    let syndrome = want ^ got;
    if syndrome == 0 {
        return HammingFix::Clean;
    }
    // A parity bit alone: its syndrome is the unit vector of its own equation.
    for j in 0..eqs.len() {
        if syndrome == 1 << j {
            bits[k + j] ^= 1;
            return HammingFix::Corrected(k + j);
        }
    }
    // A data bit: its syndrome is the column of equations it appears in.
    for (i, bit) in bits.iter_mut().enumerate().take(k) {
        let mut col = 0u32;
        for (j, eq) in eqs.iter().enumerate() {
            if eq.contains(&i) {
                col |= 1 << j;
            }
        }
        if col == syndrome {
            *bit ^= 1;
            return HammingFix::Corrected(i);
        }
    }
    HammingFix::Uncorrectable
}

/// Bits in the BPTC's interleaved input, and in its matrix plus the one leading R bit.
pub const BPTC_BITS: usize = 196;
/// Payload bits a BPTC(196,96) carries.
pub const BPTC_PAYLOAD_BITS: usize = 96;
/// Payload bytes a BPTC(196,96) carries.
pub const BPTC_PAYLOAD_BYTES: usize = BPTC_PAYLOAD_BITS / 8;
/// The interleave step: bit `a` of the deinterleaved matrix is bit `a · 181 mod 196` on the air.
pub const BPTC_INTERLEAVE_STEP: usize = 181;
/// Rows in the BPTC matrix (9 information rows + 4 column-parity rows).
pub const BPTC_ROWS: usize = 13;
/// Columns in the BPTC matrix (11 information columns + 4 row-parity columns).
pub const BPTC_COLS: usize = 15;
/// Information rows.
pub const BPTC_INFO_ROWS: usize = 9;
/// Information columns.
pub const BPTC_INFO_COLS: usize = 11;
/// Reserved bits at the head of row 0, which carry no payload.
pub const BPTC_R_BITS: usize = 3;
/// Row/column correction passes before the decoder gives up.
pub const BPTC_MAX_PASSES: u32 = 5;

/// What decoding one BPTC block cost.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BptcStats {
    /// Bits the row and column codes flipped back.
    pub corrected: u32,
    /// Correction passes run.
    pub passes: u32,
}

/// Index of matrix cell `(row, col)` in the deinterleaved array (cell 0 is at index 1; index 0 is
/// the burst's leading R bit and carries nothing).
fn cell(row: usize, col: usize) -> usize {
    1 + row * BPTC_COLS + col
}

/// Deinterleaves and decodes one BPTC(196,96) block: 12 payload bytes, or `None`.
///
/// `None` is a **refusal**: the row and column codes are run to a fixed point and every row and
/// column must then be clean. A block that still disagrees with itself is not handed on with a
/// "probably", because every caller of this treats a returned payload as something to CRC-check
/// and then believe.
pub fn bptc_196_96_decode(raw: &[u8]) -> Option<([u8; BPTC_PAYLOAD_BYTES], BptcStats)> {
    if raw.len() != BPTC_BITS {
        return None;
    }
    let mut de = [0u8; BPTC_BITS];
    for (a, d) in de.iter_mut().enumerate() {
        *d = raw[(a * BPTC_INTERLEAVE_STEP) % BPTC_BITS] & 1;
    }
    let mut stats = BptcStats::default();
    for pass in 1..=BPTC_MAX_PASSES {
        stats.passes = pass;
        let mut fixed = false;
        for c in 0..BPTC_COLS {
            let mut col = [0u8; BPTC_ROWS];
            for (r, v) in col.iter_mut().enumerate() {
                *v = de[cell(r, c)];
            }
            if let HammingFix::Corrected(_) = hamming_correct(&mut col, BPTC_INFO_ROWS, &H13_9_EQS)
            {
                for (r, v) in col.iter().enumerate() {
                    de[cell(r, c)] = *v;
                }
                stats.corrected += 1;
                fixed = true;
            }
        }
        for r in 0..BPTC_INFO_ROWS {
            let mut row = [0u8; BPTC_COLS];
            row.copy_from_slice(&de[cell(r, 0)..cell(r, 0) + BPTC_COLS]);
            if let HammingFix::Corrected(_) = hamming_correct(&mut row, BPTC_INFO_COLS, &H15_11_EQS)
            {
                de[cell(r, 0)..cell(r, 0) + BPTC_COLS].copy_from_slice(&row);
                stats.corrected += 1;
                fixed = true;
            }
        }
        if !fixed {
            break;
        }
    }
    // Every row and column must now agree, or the block is refused.
    for c in 0..BPTC_COLS {
        let mut col = [0u8; BPTC_ROWS];
        for (r, v) in col.iter_mut().enumerate() {
            *v = de[cell(r, c)];
        }
        if hamming_correct(&mut col, BPTC_INFO_ROWS, &H13_9_EQS) != HammingFix::Clean {
            return None;
        }
    }
    for r in 0..BPTC_INFO_ROWS {
        let mut row = [0u8; BPTC_COLS];
        row.copy_from_slice(&de[cell(r, 0)..cell(r, 0) + BPTC_COLS]);
        if hamming_correct(&mut row, BPTC_INFO_COLS, &H15_11_EQS) != HammingFix::Clean {
            return None;
        }
    }
    let mut bits = [0u8; BPTC_PAYLOAD_BITS];
    let mut n = 0;
    for r in 0..BPTC_INFO_ROWS {
        let first = if r == 0 { BPTC_R_BITS } else { 0 };
        for c in first..BPTC_INFO_COLS {
            bits[n] = de[cell(r, c)];
            n += 1;
        }
    }
    debug_assert_eq!(n, BPTC_PAYLOAD_BITS);
    let mut out = [0u8; BPTC_PAYLOAD_BYTES];
    for (i, b) in bits.iter().enumerate() {
        out[i / 8] |= b << (7 - i % 8);
    }
    Some((out, stats))
}

/// The 196 on-air bits of a BPTC(196,96) carrying `payload` — the exact inverse of
/// [`bptc_196_96_decode`], for tests and for the synthetic bursts they run on.
pub fn bptc_196_96_encode(payload: &[u8; BPTC_PAYLOAD_BYTES]) -> [u8; BPTC_BITS] {
    let mut de = [0u8; BPTC_BITS];
    let mut n = 0;
    for r in 0..BPTC_INFO_ROWS {
        let first = if r == 0 { BPTC_R_BITS } else { 0 };
        for c in first..BPTC_INFO_COLS {
            de[cell(r, c)] = payload[n / 8] >> (7 - n % 8) & 1;
            n += 1;
        }
    }
    for r in 0..BPTC_INFO_ROWS {
        let mut row = [0u8; BPTC_COLS];
        let data: Vec<u8> = (0..BPTC_INFO_COLS).map(|c| de[cell(r, c)]).collect();
        hamming_encode(&data, &H15_11_EQS, &mut row);
        for (c, v) in row.iter().enumerate() {
            de[cell(r, c)] = *v;
        }
    }
    for c in 0..BPTC_COLS {
        let mut col = [0u8; BPTC_ROWS];
        let data: Vec<u8> = (0..BPTC_INFO_ROWS).map(|r| de[cell(r, c)]).collect();
        hamming_encode(&data, &H13_9_EQS, &mut col);
        for (r, v) in col.iter().enumerate() {
            de[cell(r, c)] = *v;
        }
    }
    let mut raw = [0u8; BPTC_BITS];
    for (a, d) in de.iter().enumerate() {
        raw[(a * BPTC_INTERLEAVE_STEP) % BPTC_BITS] = *d;
    }
    raw
}

/// **UNVERIFIED** field polynomial of the GF(2⁸) the full-LC Reed–Solomon(12,9) parity lives in:
/// x⁸+x⁷+x²+x+1. See the module docs for what does not depend on it.
pub const RS_FIELD_POLY: u16 = 0x187;
/// Symbols in the full-LC RS codeword.
pub const RS_12_9_N: usize = 12;
/// Information symbols in it (the 9-byte full LC).
pub const RS_12_9_K: usize = 9;

/// GF(2⁸) product under [`RS_FIELD_POLY`].
fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    for _ in 0..8 {
        if b & 1 == 1 {
            p ^= a;
        }
        let hi = a & 0x80;
        a <<= 1;
        if hi != 0 {
            a ^= (RS_FIELD_POLY & 0xFF) as u8;
        }
        b >>= 1;
    }
    p
}

/// α^`n`, α being the field's generator (2).
fn gf_pow_alpha(n: usize) -> u8 {
    let mut v = 1u8;
    for _ in 0..n {
        v = gf_mul(v, 2);
    }
    v
}

/// The generator polynomial (x+α⁰)(x+α¹)(x+α²), coefficient `i` of x^i.
fn rs_generator() -> [u8; 4] {
    let mut g = [0u8; 4];
    g[0] = 1;
    for (deg, r) in (0..(RS_12_9_N - RS_12_9_K)).enumerate() {
        let root = gf_pow_alpha(r);
        let mut next = [0u8; 4];
        for j in 0..=deg {
            next[j + 1] ^= g[j];
            next[j] ^= gf_mul(g[j], root);
        }
        g = next;
    }
    g
}

/// The 3 parity symbols of a 9-symbol full LC.
pub fn rs_12_9_parity(data: &[u8; RS_12_9_K]) -> [u8; RS_12_9_N - RS_12_9_K] {
    let p = RS_12_9_N - RS_12_9_K;
    let g = rs_generator();
    let mut work = [0u8; RS_12_9_N];
    work[..RS_12_9_K].copy_from_slice(data);
    for i in 0..RS_12_9_K {
        let coef = work[i];
        if coef == 0 {
            continue;
        }
        for j in 1..=p {
            work[i + j] ^= gf_mul(coef, g[p - j]);
        }
    }
    [work[RS_12_9_K], work[RS_12_9_K + 1], work[RS_12_9_K + 2]]
}

/// Whether a 12-symbol full-LC word's RS(12,9) parity checks (every syndrome zero).
///
/// Check only: with 3 parity symbols a single symbol error *could* be corrected, and this refuses
/// instead. The code's provenance is unverified (module docs), so the one thing it is used for is
/// a yes/no gate on publishing a header, where a false *no* costs a header and a false *yes*
/// would cost a wrong one.
pub fn rs_12_9_check(word: &[u8; RS_12_9_N]) -> bool {
    (0..(RS_12_9_N - RS_12_9_K)).all(|j| {
        let a = gf_pow_alpha(j);
        word.iter().fold(0u8, |acc, &s| gf_mul(acc, a) ^ s) == 0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_golay_20_8_generator_gives_a_distance_8_code() {
        // Linear, so minimum distance is the lightest non-zero codeword.
        let d = (1..=255u8)
            .map(|d| golay_20_8_encode(d).count_ones())
            .min()
            .unwrap();
        assert_eq!(d, 8, "Golay(20,8) distance");
        // …and only four degree-12 generators manage it, which is what makes the check a filter.
        let n = (1u32 << 12..1u32 << 13)
            .filter(|g| g & 1 == 1)
            .filter(|&g| {
                (1..=255u8)
                    .map(|d| {
                        let msg = u32::from(d) << 12;
                        let mut rem = msg;
                        for i in (12..20).rev() {
                            if rem >> i & 1 == 1 {
                                rem ^= g << (i - 12);
                            }
                        }
                        (msg | (rem & 0xFFF)).count_ones()
                    })
                    .min()
                    .unwrap()
                    >= 8
            })
            .count();
        assert_eq!(n, 4, "degree-12 generators reaching d = 8");
    }

    #[test]
    fn golay_20_8_corrects_three_errors_and_refuses_beyond() {
        for data in [0u8, 1, 0x5A, 0xFF] {
            let cw = golay_20_8_encode(data);
            assert_eq!(golay_20_8_decode(cw), Some((data, 0)));
            for bits in [[0usize, 5, 19], [1, 2, 3]] {
                let mut bad = cw;
                for b in bits {
                    bad ^= 1 << b;
                }
                assert_eq!(golay_20_8_decode(bad), Some((data, 3)));
            }
            // Four errors is past the radius: the answer is a refusal or a different word, never
            // this word claimed clean.
            let bad = cw ^ 0b1111;
            assert!(golay_20_8_decode(bad).is_none_or(|(_, e)| e > 0));
        }
    }

    /// Syndrome column of data bit `i` under `eqs`.
    fn column(i: usize, eqs: &[&[usize]]) -> u32 {
        let mut c = 0u32;
        for (j, eq) in eqs.iter().enumerate() {
            if eq.contains(&i) {
                c |= 1 << j;
            }
        }
        c
    }

    fn min_distance(k: usize, eqs: &[&[usize]]) -> u32 {
        let mut best = u32::MAX;
        for m in 1..(1u32 << k) {
            let data: Vec<u8> = (0..k).map(|i| (m >> i & 1) as u8).collect();
            let w = data.iter().map(|&b| u32::from(b)).sum::<u32>()
                + parity_of(&data, eqs).count_ones();
            best = best.min(w);
        }
        best
    }

    #[test]
    fn every_hamming_code_here_is_a_distance_3_code() {
        for (name, k, eqs) in [
            ("15,11", 11usize, &H15_11_EQS[..]),
            ("13,9", 9, &H13_9_EQS[..]),
            ("7,4", 4, &H7_4_EQS[..]),
        ] {
            let cols: Vec<u32> = (0..k).map(|i| column(i, eqs)).collect();
            assert!(cols.iter().all(|&c| c != 0), "{name}: a zero column");
            let mut all = cols.clone();
            all.extend((0..eqs.len()).map(|j| 1u32 << j));
            all.sort_unstable();
            let n = all.len();
            all.dedup();
            assert_eq!(all.len(), n, "{name}: repeated syndrome column");
            assert_eq!(min_distance(k, eqs), 3, "{name}: minimum distance");
        }
    }

    #[test]
    fn the_two_bptc_hamming_codes_share_one_column_table() {
        // The (13,9) columns are the last nine (15,11) columns, in order: two shortenings of one
        // published table, not two independent recollections.
        let long: Vec<u32> = (0..11).map(|i| column(i, &H15_11_EQS[..])).collect();
        let short: Vec<u32> = (0..9).map(|i| column(i, &H13_9_EQS[..])).collect();
        assert_eq!(short, long[2..], "column tables disagree");
    }

    #[test]
    fn hamming_correct_fixes_any_single_bit() {
        for (k, eqs) in [
            (11usize, &H15_11_EQS[..]),
            (9, &H13_9_EQS[..]),
            (4, &H7_4_EQS[..]),
        ] {
            let n = k + eqs.len();
            let data: Vec<u8> = (0..k).map(|i| (i % 3 == 0) as u8).collect();
            let mut cw = vec![0u8; n];
            hamming_encode(&data, eqs, &mut cw);
            assert_eq!(hamming_correct(&mut cw.clone(), k, eqs), HammingFix::Clean);
            for b in 0..n {
                let mut bad = cw.clone();
                bad[b] ^= 1;
                assert_eq!(hamming_correct(&mut bad, k, eqs), HammingFix::Corrected(b));
                assert_eq!(bad, cw, "not restored after flipping bit {b}");
            }
        }
    }

    #[test]
    fn the_bptc_interleave_is_a_permutation_and_the_matrix_accounts_for_every_bit() {
        let mut seen = [false; BPTC_BITS];
        for a in 0..BPTC_BITS {
            let p = (a * BPTC_INTERLEAVE_STEP) % BPTC_BITS;
            assert!(!seen[p], "181 is not invertible mod 196");
            seen[p] = true;
        }
        assert_eq!(BPTC_ROWS * BPTC_COLS + 1, BPTC_BITS);
        assert_eq!(
            BPTC_INFO_ROWS * BPTC_INFO_COLS - BPTC_R_BITS,
            BPTC_PAYLOAD_BITS
        );
    }

    #[test]
    fn bptc_round_trips_and_corrects_one_error_per_row_and_column() {
        let payload: [u8; 12] = [
            0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x11, 0x22, 0x33, 0x44,
        ];
        let raw = bptc_196_96_encode(&payload);
        let (got, stats) = bptc_196_96_decode(&raw).expect("clean block");
        assert_eq!(got, payload);
        assert_eq!(stats.corrected, 0);
        // One error in each of two different rows and columns is inside the product code's reach.
        for bits in [[3usize, 100], [0, 195], [50, 51]] {
            let mut bad = raw;
            for b in bits {
                bad[b] ^= 1;
            }
            let (got, stats) = bptc_196_96_decode(&bad).expect("correctable block");
            assert_eq!(got, payload, "payload after correcting {bits:?}");
            assert!(stats.corrected > 0);
        }
    }

    #[test]
    fn bptc_refuses_a_block_it_cannot_resolve() {
        let payload = [0xA5u8; 12];
        let mut raw = bptc_196_96_encode(&payload);
        // Twenty-odd errors: past any product-code reach, and the answer must be a refusal rather
        // than a payload nobody can trust.
        for b in (0..BPTC_BITS).step_by(7) {
            raw[b] ^= 1;
        }
        assert_eq!(bptc_196_96_decode(&raw), None);
    }

    #[test]
    fn rs_12_9_detects_one_and_two_symbol_errors() {
        let data = [0x01u8, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x10];
        let parity = rs_12_9_parity(&data);
        let mut word = [0u8; 12];
        word[..9].copy_from_slice(&data);
        word[9..].copy_from_slice(&parity);
        assert!(rs_12_9_check(&word));
        for i in 0..12 {
            let mut bad = word;
            bad[i] ^= 0x5A;
            assert!(!rs_12_9_check(&bad), "single error at {i} undetected");
            for j in (i + 1)..12 {
                let mut bad2 = bad;
                bad2[j] ^= 0x31;
                assert!(!rs_12_9_check(&bad2), "double error at {i},{j} undetected");
            }
        }
    }
}
