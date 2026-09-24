//! Reed–Solomon kernel over GF(2^m), `2 ≤ m ≤ 16` (T-611): the field, an errors-only
//! bounded-distance decoder (syndromes → Berlekamp–Massey → Chien → Forney) and the CCSDS
//! Berlekamp dual-basis transform. Native: T-607 measured liquid-dsp's Reed–Solomon to be a
//! `libfec` wrapper that is absent without `libfec` (docs/18 §7.1.1), so there was no kernel
//! to adapt (ADR-0011 §9.4).
//!
//! ## Conventions (Karn's, which CCSDS 131.0-B, DVB and the P25 decoders share)
//!
//! - A codeword is `n` symbols, **first on air = the coefficient of the highest power**
//!   `x^(n−1)`; systematic, the `k` data symbols first and the `n − k` check symbols after.
//! - The generator is `g(x) = ∏_{i=0}^{n−k−1} (x − α^{prim·(fcr+i)})`, `α` the root of the
//!   field polynomial. CCSDS: field `0x187`, `fcr = 112`, `prim = 11`; DVB / ITU-T G.975:
//!   `0x11D`, `fcr = 0`, `prim = 1`.
//! - A **shortened** code (`n < 2^m − 1`: DVB RS(204,188), every P25 code) is the full code
//!   with its leading `2^m − 1 − n` data symbols fixed at zero and not sent; an error located
//!   there is refused.
//!
//! ## Decoding and what it refuses
//!
//! Up to `⌊(n − k)/2⌋` symbol errors are corrected. The decoder refuses (never "miscorrects
//! into a guess"): a locator of degree above that bound; a locator whose roots are not exactly
//! its degree distinct positions inside the (shortened) codeword; a Forney magnitude of zero or
//! a zero derivative; and, as a last check, a corrected word whose syndromes are not all zero.
//! A word with more than `t` errors can still land within `t` of a *different* codeword and be
//! "corrected" to it — the bounded-distance decoder's intrinsic miscorrection, ≈ 1/t! of such
//! words for long codes (RS(255,223): ~5·10⁻¹⁴), much more for the short P25 codes
//! (RS(24,16,9), t = 4: of order 10⁻²), which is why P25 checks a CRC or Golay layer after it.

/// A symbol value (the low `m` bits).
pub(crate) type Sym = u16;

/// GF(2^m) by log/antilog tables.
pub(crate) struct Field {
    m: u32,
    /// `2^m − 1`, the multiplicative order.
    nn: usize,
    /// The full field polynomial (with the `x^m` term).
    poly: u32,
    /// `α^i` for `i` in `0..2·nn`, doubled so a sum of two logs needs no reduction.
    exp: Vec<Sym>,
    /// `log α(x)` for `x` in `1..=nn` (`log[0]` unused).
    log: Vec<u32>,
}

impl Field {
    /// The field of the primitive polynomial `poly` of degree `m`, in full form (`0x187`) or
    /// without the `x^m` term (`0x87`).
    pub fn new(m: u32, poly: u64) -> Result<Self, String> {
        if !(2..=16).contains(&m) {
            return Err("symbol_bits must be 2–16".into());
        }
        let full = match poly >> m {
            0 => poly | 1 << m,
            1 => poly,
            _ => return Err(format!("poly degree above symbol_bits = {m}")),
        } as u32;
        let nn = (1usize << m) - 1;
        let mut exp = vec![0 as Sym; 2 * nn];
        let mut log = vec![0u32; nn + 1];
        let mut x = 1u32;
        for (i, e) in exp.iter_mut().take(nn).enumerate() {
            if i > 0 && x == 1 || x == 0 {
                return Err(format!("poly {full:#x} is not primitive"));
            }
            *e = x as Sym;
            log[x as usize] = i as u32;
            x <<= 1;
            if x >> m & 1 == 1 {
                x ^= full;
            }
        }
        if x != 1 {
            return Err(format!("poly {full:#x} is not primitive"));
        }
        exp.copy_within(0..nn, nn);
        Ok(Self {
            m,
            nn,
            poly: full,
            exp,
            log,
        })
    }

    /// `m`, bits per symbol.
    pub fn bits(&self) -> u32 {
        self.m
    }

    /// The full field polynomial.
    pub fn poly(&self) -> u32 {
        self.poly
    }

    /// `α^e`.
    #[inline]
    pub fn alpha(&self, e: usize) -> Sym {
        self.exp[e % self.nn]
    }

    #[inline]
    pub fn mul(&self, a: Sym, b: Sym) -> Sym {
        if a == 0 || b == 0 {
            0
        } else {
            self.exp[(self.log[a as usize] + self.log[b as usize]) as usize]
        }
    }

    /// `a · α^e` (`e < nn`).
    #[inline]
    fn mul_alpha(&self, a: Sym, e: usize) -> Sym {
        if a == 0 {
            0
        } else {
            self.exp[self.log[a as usize] as usize + e]
        }
    }

    /// `a / b`, `b ≠ 0`.
    #[inline]
    fn div(&self, a: Sym, b: Sym) -> Sym {
        if a == 0 {
            0
        } else {
            self.exp[(self.log[a as usize] + self.nn as u32 - self.log[b as usize]) as usize]
        }
    }

    /// The absolute trace `Tr(z) = z + z² + z⁴ + … + z^(2^(m−1))`, 0 or 1.
    fn trace(&self, z: Sym) -> Sym {
        let (mut t, mut acc) = (z, z);
        for _ in 1..self.m {
            t = self.mul(t, t);
            acc ^= t;
        }
        acc
    }
}

/// An `(n, k)` code with its decoder scratch (sized once, at build: `decode` allocates nothing).
pub(crate) struct Decoder {
    pub field: Field,
    pub n: usize,
    pub k: usize,
    nroots: usize,
    fcr: usize,
    prim: usize,
    /// `prim·(fcr+i) mod nn`: the log of syndrome `i`'s evaluation point.
    root_log: Vec<usize>,
    synd: Vec<Sym>,
    lambda: Vec<Sym>,
    prev: Vec<Sym>,
    tmp: Vec<Sym>,
    omega: Vec<Sym>,
    /// Located errors: (symbol index, magnitude).
    errs: Vec<(usize, Sym)>,
}

impl Decoder {
    /// The code; refuses a non-primitive field, `k ≥ n`, `n > 2^m − 1` and a `prim` sharing a
    /// factor with `2^m − 1` (it would map two positions onto one locator).
    pub fn new(field: Field, n: usize, k: usize, fcr: usize, prim: usize) -> Result<Self, String> {
        let nn = field.nn;
        if n > nn {
            return Err(format!("n above 2^symbol_bits − 1 = {nn}"));
        }
        if k == 0 || k >= n {
            return Err("need 0 < k < n".into());
        }
        if prim == 0 || gcd(prim % nn, nn) != 1 {
            return Err(format!("prim must be coprime with {nn}"));
        }
        let nroots = n - k;
        let t = nroots / 2;
        Ok(Self {
            n,
            k,
            nroots,
            fcr: fcr % nn,
            prim: prim % nn,
            root_log: (0..nroots).map(|i| prim * (fcr + i) % nn).collect(),
            synd: vec![0; nroots],
            lambda: vec![0; nroots + 1],
            prev: vec![0; nroots + 1],
            tmp: vec![0; nroots + 1],
            omega: vec![0; nroots],
            errs: Vec::with_capacity(t),
            field,
        })
    }

    /// Correctable symbol errors per codeword, `⌊(n − k)/2⌋`.
    pub fn t(&self) -> usize {
        self.nroots / 2
    }

    /// Syndromes of `r` into `self.synd`; whether any is non-zero.
    fn syndromes(&mut self, r: &[Sym]) -> bool {
        let f = &self.field;
        let mut any = false;
        for (s_out, &lr) in self.synd.iter_mut().zip(&self.root_log) {
            let mut s: Sym = 0;
            for &c in r {
                s = f.mul_alpha(s, lr) ^ c;
            }
            *s_out = s;
            any |= s != 0;
        }
        any
    }

    /// The log of `X⁻¹ = α^(−prim·p)` for the symbol at index `idx` (power `p = n − 1 − idx`).
    #[inline]
    fn xinv_log(&self, idx: usize) -> usize {
        let nn = self.field.nn;
        (nn - self.prim * (self.n - 1 - idx) % nn) % nn
    }

    /// Decodes the codeword `r` (`n` symbols) in place: `Some(symbols corrected)`, or `None`
    /// when it is uncorrectable — and then `r` is left exactly as received.
    pub fn decode(&mut self, r: &mut [Sym]) -> Option<usize> {
        debug_assert_eq!(r.len(), self.n);
        if !self.syndromes(r) {
            return Some(0);
        }
        let nr = self.nroots;
        // Berlekamp–Massey: the shortest LFSR Λ generating the syndromes.
        self.lambda.fill(0);
        self.lambda[0] = 1;
        self.prev.fill(0);
        self.prev[0] = 1;
        let (mut l, mut shift, mut last) = (0usize, 1usize, 1 as Sym);
        for step in 0..nr {
            let f = &self.field;
            let mut d = self.synd[step];
            for i in 1..=l.min(step) {
                d ^= f.mul(self.lambda[i], self.synd[step - i]);
            }
            if d == 0 {
                shift += 1;
                continue;
            }
            let coef = f.div(d, last);
            let grow = 2 * l <= step;
            if grow {
                self.tmp.copy_from_slice(&self.lambda);
            }
            for i in 0..=nr.saturating_sub(shift) {
                self.lambda[i + shift] ^= f.mul(coef, self.prev[i]);
            }
            if grow {
                l = step + 1 - l;
                self.prev.copy_from_slice(&self.tmp);
                last = d;
                shift = 1;
            } else {
                shift += 1;
            }
        }
        if l == 0 || l > self.t() || self.lambda[l] == 0 {
            return None;
        }
        // Chien search over the positions actually sent (a shortened code's padding is not).
        self.errs.clear();
        for idx in 0..self.n {
            let xl = self.xinv_log(idx);
            let f = &self.field;
            let mut v = self.lambda[0];
            for (i, &c) in self.lambda.iter().enumerate().take(l + 1).skip(1) {
                v ^= f.mul_alpha(c, i * xl % f.nn);
            }
            if v == 0 {
                if self.errs.len() == l {
                    return None;
                }
                self.errs.push((idx, 0));
            }
        }
        if self.errs.len() != l {
            return None;
        }
        // Ω(x) = S(x)·Λ(x) mod x^(n−k), then Forney:
        // e = X^(1−fcr) · Ω(X⁻¹) / Λ'(X⁻¹).
        let f = &self.field;
        for i in 0..nr {
            let mut o = 0;
            for j in 0..=l.min(i) {
                o ^= f.mul(self.lambda[j], self.synd[i - j]);
            }
            self.omega[i] = o;
        }
        let nn = f.nn;
        for e in 0..self.errs.len() {
            let idx = self.errs[e].0;
            let xl = self.xinv_log(idx);
            let mut num = 0;
            for (i, &o) in self.omega.iter().enumerate() {
                num ^= f.mul_alpha(o, i * xl % nn);
            }
            let mut den = 0;
            for i in (1..=l).step_by(2) {
                den ^= f.mul_alpha(self.lambda[i], (i - 1) * xl % nn);
            }
            if num == 0 || den == 0 {
                return None;
            }
            // X^(1−fcr) = (X⁻¹)^(fcr−1).
            let x_pow = (xl * ((self.fcr + nn - 1) % nn)) % nn;
            self.errs[e].1 = f.mul_alpha(f.div(num, den), x_pow);
        }
        for &(idx, e) in &self.errs {
            r[idx] ^= e;
        }
        if self.syndromes(r) {
            for &(idx, e) in &self.errs {
                r[idx] ^= e;
            }
            return None;
        }
        Some(l)
    }

    /// Systematic encoding: the `n − k` check symbols of `data` (`k` symbols) into `parity`.
    #[cfg(test)]
    pub fn encode(&self, data: &[Sym], parity: &mut [Sym]) {
        let f = &self.field;
        let nr = self.nroots;
        // g(x), g[j] = coefficient of x^j, monic.
        let mut g = vec![0 as Sym; nr + 1];
        g[0] = 1;
        for (deg, &lr) in self.root_log.iter().enumerate() {
            let root = f.alpha(lr);
            for j in (0..=deg + 1).rev() {
                let lower = if j > 0 { g[j - 1] } else { 0 };
                g[j] = lower ^ f.mul(g[j], root);
            }
        }
        parity.fill(0);
        for &d in data {
            let fb = d ^ parity[0];
            for j in 0..nr - 1 {
                parity[j] = parity[j + 1] ^ f.mul(fb, g[nr - 1 - j]);
            }
            parity[nr - 1] = f.mul(fb, g[0]);
        }
    }
}

fn gcd(a: usize, b: usize) -> usize {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// The CCSDS field, GF(2^8) over `x^8 + x^7 + x^2 + x + 1`.
pub(crate) const CCSDS_POLY: u32 = 0x187;

/// The CCSDS 131.0-B Berlekamp (dual-basis) representation of GF(2^8)/0x187: the dual basis
/// `{ℓ_0 … ℓ_7}` of `{1, β, …, β^7}`, `β = α^117`, so the on-air bit `i` (MSB first) of a
/// symbol `z` is `Tr(z · β^i)`. Derived from that definition, not tabulated; the test
/// `ccsds_dual_basis_is_the_trace_form_and_libfecs_matrix` holds it to libfec's `tal` matrix.
pub(crate) struct DualBasis {
    /// Conventional → on-air (dual).
    pub to_dual: [u8; 256],
    /// On-air (dual) → conventional.
    pub to_conv: [u8; 256],
}

impl DualBasis {
    /// For the CCSDS field only.
    pub fn ccsds(f: &Field) -> Option<Self> {
        if f.bits() != 8 || f.poly() != CCSDS_POLY {
            return None;
        }
        let beta = f.alpha(117);
        let mut powers = [0 as Sym; 8];
        let mut b: Sym = 1;
        for p in &mut powers {
            *p = b;
            b = f.mul(b, beta);
        }
        let mut to_dual = [0u8; 256];
        let mut to_conv = [0u8; 256];
        for z in 0..=255u8 {
            let mut d = 0u8;
            for (i, &p) in powers.iter().enumerate() {
                d |= (f.trace(f.mul(Sym::from(z), p)) as u8) << (7 - i);
            }
            to_dual[z as usize] = d;
            to_conv[d as usize] = z;
        }
        Some(Self { to_dual, to_conv })
    }
}
