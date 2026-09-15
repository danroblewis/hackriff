//! GF(2) polynomial arithmetic for the code search ([`super::codes`]).
//!
//! [`Poly`] holds arbitrary-degree polynomials as little-endian `u64` words (bit `i` of the
//! vector = coefficient of `xⁱ`). Bits in transmission order map to a polynomial with the first
//! bit as the highest-degree coefficient, the convention of a bit-serial CRC register. The small
//! helpers (`*_small`) work on generators of degree ≤ 32 held in a `u64` with the `x^w` term.

/// A polynomial over GF(2).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct Poly {
    w: Vec<u64>,
}

impl Poly {
    /// From a `u64` (bit `i` = coefficient of `xⁱ`).
    #[cfg(test)]
    pub fn from_u64(v: u64) -> Self {
        let mut p = Self { w: vec![v] };
        p.trim();
        p
    }

    /// From bits in transmission order: the first bit is the coefficient of `x^(n−1)`.
    pub fn from_bits(bits: &[u8]) -> Self {
        let n = bits.len();
        let mut w = vec![0u64; n.div_ceil(64)];
        for (i, &b) in bits.iter().enumerate() {
            if b & 1 == 1 {
                let d = n - 1 - i;
                w[d / 64] |= 1 << (d % 64);
            }
        }
        let mut p = Self { w };
        p.trim();
        p
    }

    fn trim(&mut self) {
        while self.w.last() == Some(&0) {
            self.w.pop();
        }
    }

    pub fn is_zero(&self) -> bool {
        self.w.is_empty()
    }

    /// Degree; `None` for the zero polynomial.
    pub fn degree(&self) -> Option<usize> {
        let last = *self.w.last()?;
        Some((self.w.len() - 1) * 64 + 63 - last.leading_zeros() as usize)
    }

    /// The value as a `u64` when the degree is below 64.
    pub fn to_u64(&self) -> Option<u64> {
        match self.w.len() {
            0 => Some(0),
            1 => Some(self.w[0]),
            _ => None,
        }
    }

    /// Number of words (the cost unit of one shifted XOR).
    pub fn words(&self) -> u64 {
        self.w.len() as u64
    }

    /// `self ^= other · x^shift`.
    pub fn xor_shifted(&mut self, other: &Poly, shift: usize) {
        let ws = shift / 64;
        let bs = shift % 64;
        let need = other.w.len() + ws + 1;
        if self.w.len() < need {
            self.w.resize(need, 0);
        }
        for (i, &o) in other.w.iter().enumerate() {
            self.w[i + ws] ^= o << bs;
            if bs != 0 {
                self.w[i + ws + 1] ^= o >> (64 - bs);
            }
        }
        self.trim();
    }

    /// `self ^= other`.
    pub fn add(&mut self, other: &Poly) {
        self.xor_shifted(other, 0);
    }

    /// Remainder modulo a non-zero `m`, and the word operations spent.
    pub fn rem(&self, m: &Poly) -> (Poly, u64) {
        let dm = m.degree().expect("non-zero modulus");
        let mut r = self.clone();
        let mut ops = 1;
        while let Some(dr) = r.degree() {
            if dr < dm {
                break;
            }
            r.xor_shifted(m, dr - dm);
            ops += m.words() + 1;
        }
        (r, ops)
    }

    /// Remainder modulo a generator of degree 1–62 held in a `u64` (the `x^w` bit set), bit by
    /// bit without allocating; costs one step per coefficient word bit (`64 · words`).
    pub fn rem_small(&self, g: u64) -> u64 {
        let top = 1u64 << (63 - g.leading_zeros());
        let mut reg = 0u64;
        for &word in self.w.iter().rev() {
            for b in (0..64).rev() {
                reg = (reg << 1) | ((word >> b) & 1);
                if reg & top != 0 {
                    reg ^= g;
                }
            }
        }
        reg
    }

    /// Bit `i` (coefficient of `xⁱ`).
    fn bit(&self, i: usize) -> bool {
        self.w.get(i / 64).is_some_and(|w| (w >> (i % 64)) & 1 == 1)
    }

    /// Whether the low `len` coefficients repeat with period `k` (`bᵢ = bᵢ₊ₖ`), and the bit
    /// comparisons spent.
    pub fn is_periodic(&self, len: usize, k: usize) -> (bool, u64) {
        let mut ops = 0;
        for i in 0..len.saturating_sub(k) {
            ops += 1;
            if self.bit(i) != self.bit(i + k) {
                return (false, ops);
            }
        }
        (true, ops)
    }

    /// Quotient and remainder modulo a non-zero `m`, and the word operations spent.
    #[cfg(test)]
    pub fn div_rem(&self, m: &Poly) -> (Poly, Poly, u64) {
        let dm = m.degree().expect("non-zero modulus");
        let mut r = self.clone();
        let mut q = Poly::default();
        let mut ops = 1;
        while let Some(dr) = r.degree() {
            if dr < dm {
                break;
            }
            let s = dr - dm;
            r.xor_shifted(m, s);
            q.xor_shifted(&Poly::from_u64(1), s);
            ops += m.words() + 2;
        }
        (q, r, ops)
    }
}

/// Greatest common divisor, and the word operations spent.
pub(crate) fn gcd(a: &Poly, b: &Poly) -> (Poly, u64) {
    let (mut a, mut b) = (a.clone(), b.clone());
    let mut ops = 0;
    while !b.is_zero() {
        let (r, o) = a.rem(&b);
        ops += o;
        a = b;
        b = r;
    }
    (a, ops)
}

/// Quotient and remainder of `a / b` for polynomials held in `u64`s (`b` non-zero), and the
/// steps spent.
pub(crate) fn divrem_small(a: u64, b: u64) -> (u64, u64, u64) {
    let db = 63 - b.leading_zeros();
    let (mut q, mut r, mut ops) = (0u64, a, 1u64);
    while r != 0 {
        let dr = 63 - r.leading_zeros();
        if dr < db {
            break;
        }
        q |= 1 << (dr - db);
        r ^= b << (dr - db);
        ops += 1;
    }
    (q, r, ops)
}

/// Greatest common divisor of two polynomials held in `u64`s, and the steps spent.
pub(crate) fn gcd_small(a: u64, b: u64) -> (u64, u64) {
    let (mut a, mut b, mut ops) = (a, b, 1u64);
    while b != 0 {
        let (_, r, o) = divrem_small(a, b);
        ops += o;
        a = b;
        b = r;
    }
    (a, ops)
}

/// The repeated part of a non-zero `g`: `gcd(g, g′)`, which is 1 exactly when `g` has no
/// repeated irreducible factor (and `g` itself when `g` is a square), and the steps spent.
pub(crate) fn repeated_part(g: u64) -> (u64, u64) {
    // Over GF(2) the derivative keeps the odd-degree terms, each lowered by one.
    let d = (g & 0xAAAA_AAAA_AAAA_AAAA) >> 1;
    if d == 0 {
        return (g, 1);
    }
    gcd_small(g, d)
}

/// Carry-less product of two polynomials of degree < 64.
pub(crate) fn clmul(a: u64, b: u64) -> u128 {
    let mut r = 0u128;
    let mut b = b;
    let mut i = 0;
    while b != 0 {
        if b & 1 == 1 {
            r ^= u128::from(a) << i;
        }
        b >>= 1;
        i += 1;
    }
    r
}

/// `v mod g` for a generator `g` of degree `w` (1 ≤ w ≤ 63, the `x^w` bit set).
pub(crate) fn mod_small(v: u128, g: u64) -> u64 {
    let w = 63 - g.leading_zeros();
    let mut v = v;
    while v != 0 {
        let d = 127 - v.leading_zeros();
        if d < w {
            break;
        }
        v ^= u128::from(g) << (d - w);
    }
    v as u64
}

/// `x^e mod g`.
pub(crate) fn xpow_mod(e: u64, g: u64) -> u64 {
    let mut result = mod_small(1, g);
    let mut base = mod_small(2, g);
    let mut e = e;
    while e != 0 {
        if e & 1 == 1 {
            result = mod_small(clmul(result, base), g);
        }
        base = mod_small(clmul(base, base), g);
        e >>= 1;
    }
    result
}

/// The multiplicative order of `x` modulo `g` (the natural length of the cyclic code `g`
/// generates), searched up to `limit`; `None` when `g(0) = 0` or the order exceeds `limit`.
pub(crate) fn order_of_x(g: u64, limit: u64) -> Option<u64> {
    if g & 1 == 0 || g < 2 {
        return None;
    }
    let one = mod_small(1, g);
    let mut r = one;
    for n in 1..=limit {
        r = mod_small(u128::from(r) << 1, g);
        if r == one {
            return Some(n);
        }
    }
    None
}

/// Bit-reverses the low `w` bits.
pub(crate) fn reflect(v: u64, w: u32) -> u64 {
    if w == 0 {
        return 0;
    }
    v.reverse_bits() >> (64 - w)
}

/// Solves `A·x = b` over GF(2) for a `w × w` system given by columns (`cols[i]` = `A·eᵢ`),
/// returning one solution (free variables zero) and the null-space dimension; `None` when
/// inconsistent.
pub(crate) fn solve(cols: &[u64], b: u64, w: u32) -> Option<(u64, u32)> {
    // Rows: for each equation bit r, the coefficients over the unknowns plus the right side.
    let n = cols.len();
    let mut rows: Vec<(u64, bool)> = (0..w)
        .map(|r| {
            let mut coef = 0u64;
            for (i, c) in cols.iter().enumerate() {
                if (c >> r) & 1 == 1 {
                    coef |= 1 << i;
                }
            }
            (coef, (b >> r) & 1 == 1)
        })
        .collect();
    let mut pivots = Vec::new();
    let mut row = 0;
    for col in 0..n {
        let Some(p) = (row..rows.len()).find(|&i| (rows[i].0 >> col) & 1 == 1) else {
            continue;
        };
        rows.swap(row, p);
        let pivot = rows[row];
        for (i, r) in rows.iter_mut().enumerate() {
            if i != row && (r.0 >> col) & 1 == 1 {
                r.0 ^= pivot.0;
                r.1 ^= pivot.1;
            }
        }
        pivots.push(col);
        row += 1;
    }
    if rows[row..].iter().any(|r| r.1) {
        return None;
    }
    let mut x = 0u64;
    for (i, &col) in pivots.iter().enumerate() {
        if rows[i].1 {
            x |= 1 << col;
        }
    }
    Some((x, (n - pivots.len()) as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assist_gf2_gcd_recovers_a_common_factor() {
        // g = BCH(31,21) generator; a = g·(x^3+x+1), b = g·(x^4+x+1).
        let g = 0x769u64;
        let a = Poly::from_u64(clmul(g, 0b1011) as u64);
        let b = Poly::from_u64(clmul(g, 0b10011) as u64);
        let (d, _) = gcd(&a, &b);
        assert_eq!(d.to_u64(), Some(g));
        let (q, r, _) = a.div_rem(&Poly::from_u64(g));
        assert!(r.is_zero());
        assert_eq!(q.to_u64(), Some(0b1011));
        assert_eq!(Poly::from_bits(&[1, 0, 1, 1]).to_u64(), Some(0b1011));
        // The allocation-free paths agree with the general ones.
        let long = Poly::from_bits(
            &(0..300)
                .map(|i| ((i * 7 + i / 3) % 2) as u8)
                .collect::<Vec<_>>(),
        );
        let (r, _) = long.rem(&Poly::from_u64(g));
        assert_eq!(Some(long.rem_small(g)), r.to_u64());
        let (q2, r2, _) = divrem_small(clmul(g, 0b1011) as u64 ^ 0b101, g);
        assert_eq!((q2, r2), (0b1011, 0b101));
        let alt = Poly::from_bits(&[1, 0, 1, 0, 1, 0, 1, 0, 1, 0]);
        assert!(alt.is_periodic(10, 2).0 && !alt.is_periodic(10, 1).0);
    }

    #[test]
    fn assist_gf2_small_helpers() {
        assert_eq!(order_of_x(0x769, 1 << 12), Some(31));
        assert_eq!(order_of_x(0x5B9, 1 << 12), Some(341));
        assert_eq!(xpow_mod(31, 0x769), 1);
        assert_eq!(reflect(0b0011, 4), 0b1100);
        // Designed generators have no repeated factor; CRC-24/Mode-S × (x+1) squares (x+1).
        for g in [0x1FF_F409u64, 0x769, 0x5B9, 0x1_1021, 0xB] {
            assert_eq!(repeated_part(g).0, 1, "{g:#x}");
        }
        assert_eq!(
            gcd_small(clmul(0x769, 0b1011) as u64, clmul(0x769, 0b111) as u64).0,
            0x769
        );
        let squared = clmul(0x1FF_F409, 0b11) as u64;
        assert_eq!(divrem_small(repeated_part(squared).0, 0b101).1, 0);
        assert_eq!(repeated_part(0b101).0, 0b101);
        // Equation bit 0: x0 + x1 = 1; equation bit 1: x1 = 0 → x = 0b01.
        let (x, free) = solve(&[0b01, 0b11], 0b01, 2).unwrap();
        assert_eq!((x & 0b11, free), (0b01, 0));
    }
}
