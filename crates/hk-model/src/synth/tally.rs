//! ADR-0022 §4.3.1's independent-trial count for a check stage (`check_distinct_valid`).
//!
//! **Core interface.** This is the count the confirm gate reads, and the one place it is
//! written. It lives beside the analytic nulls, below `hk-estimate`, because **both** callers
//! must run the same code and neither can depend on the other:
//!
//! - `hk-blocks`' `crc` / `bch` / `parity` / `checksum` blocks record every unit they check and
//!   emit the S5 `check_distinct_valid` evidence from it (ADR-0015 §2.1);
//! - `hk_estimate::assist::codes`' searched-generator path counts the frames behind a claim with
//!   it, so a search cannot credit trials the block path would refuse (T-577 §2.5, T-921);
//! - `hk-estimate`'s `degenerate_null` harness **measures** the false-confirm rate by driving
//!   this type, not a copy of it (T-921; T-577 measured a copy, and ADR-0022 §4.3.1 would not
//!   move the width floor on a copy's numbers).
//!
//! The count, per ADR-0022 §4.3.1: a unit adds a trial only when it passed **without FEC
//! correction**, is not [`degenerate_frame`] (hole A: the init-cancel frame), and its
//! zero-trimmed polynomial is neither a multiple nor a divisor of one already counted (hole B:
//! `g | P ⇒ g | m·P`, so shifts, doubled copies and repeats of one burst are one chance event).
//! The total is capped at the GF(2) **affine rank + 1** per frame length, where independence
//! actually saturates. Every rule here removes trials; none adds one.

use super::null::{SHORT_PERIOD_MAX_BITS, check_bits, is_short_periodic};
use super::{Evidence, EvidenceSet, GroupId, MetricId, Stage};

/// Longest frame (bits) whose independence [`CheckTally`] can analyse. A longer clean-valid frame
/// is **not counted** — fewer bits, never more (a frame this long is far past any check the
/// catalogue carries).
pub const MAX_TALLY_FRAME_BITS: usize = POLY_WORDS * 64;

/// Most independent frames one window counts (and most GF(2) basis vectors it keeps). The gate
/// needs at most `⌈(24 + L_check) / 8⌉` differences, so beyond this nothing new is counted —
/// fewer bits, never more.
pub const MAX_COUNTED_FRAMES: usize = 64;

const POLY_WORDS: usize = 64;

/// A frame as a GF(2) vector: bit `j` of the vector is the frame's bit `j`. Only the scratch
/// value `record` builds is ever this wide; what the tally **keeps** is `words_for(len)` words
/// in its [`Arena`].
type Vector = [u64; POLY_WORDS];

/// Frame length (bits) up to which the tally keeps its full [`MAX_COUNTED_FRAMES`] entries:
/// every framing the catalogue carries is shorter (ADS-B 112, RDS 104, AIS 168, an ACARS
/// block). Longer frames fill the arena sooner and are counted until it is full — fewer bits,
/// never more, the same direction as [`MAX_COUNTED_FRAMES`] itself.
const TALLY_FULL_FRAME_BITS: usize = 512;

/// Words of GF(2) storage one tally holds: two vectors per counted frame (its zero-trimmed
/// core, and its affine origin or basis vector) at [`TALLY_FULL_FRAME_BITS`]. **8 KiB**, fixed
/// at construction and never grown, against the 100 352 bytes of `3 × 64` inline [`Vector`]s
/// this replaced — paid by every crc/bch/checksum/parity instance, live recipes included
/// (T-928; T-453: measure, don't assume).
const TALLY_ARENA_WORDS: usize = 2 * MAX_COUNTED_FRAMES * (TALLY_FULL_FRAME_BITS / 64);

fn words_for(bits: usize) -> usize {
    bits.div_ceil(64).max(1)
}

fn get(v: &[u64], i: usize) -> bool {
    v[i / 64] >> (i % 64) & 1 == 1
}

/// Highest set bit within the first `words` words; a `v` shorter than `words` reads as zero
/// beyond its end (a stored vector carries only the words its length needs).
fn highest(v: &[u64], words: usize) -> Option<usize> {
    (0..words.min(v.len()))
        .rev()
        .find(|&w| v[w] != 0)
        .map(|w| w * 64 + 63 - v[w].leading_zeros() as usize)
}

/// `acc ^= p · x^shift` over the first `words` words; `p` reads as zero beyond its end.
fn xor_shifted(acc: &mut [u64], p: &[u64], shift: usize, words: usize) {
    let (ws, bs) = (shift / 64, shift % 64);
    let word = |i: usize| p.get(i).copied().unwrap_or(0);
    for i in (ws..words).rev() {
        let lo = word(i - ws) << bs;
        let hi = if bs != 0 && i > ws {
            word(i - ws - 1) >> (64 - bs)
        } else {
            0
        };
        acc[i] ^= lo | hi;
    }
}

/// The tally's GF(2) storage: one fixed [`TALLY_ARENA_WORDS`]-word allocation, handed out in
/// `words_for(len)`-word vectors and rewound whole by [`CheckTally::clear`]. A full arena stops
/// the tally counting, exactly as [`MAX_COUNTED_FRAMES`] does, so nothing here can allocate on
/// the record path or grow with the frames seen.
struct Arena {
    words: Box<[u64]>,
    used: usize,
}

impl Arena {
    fn new() -> Self {
        Self {
            words: vec![0; TALLY_ARENA_WORDS].into_boxed_slice(),
            used: 0,
        }
    }

    /// Stores the first `words` words of `v`, returning their place, or `None` when full.
    fn push(&mut self, v: &[u64], words: usize) -> Option<usize> {
        let at = self.used;
        let end = at + words;
        if end > self.words.len() {
            return None;
        }
        self.words[at..end].copy_from_slice(&v[..words]);
        self.used = end;
        Some(at)
    }

    fn get(&self, at: usize, words: usize) -> &[u64] {
        &self.words[at..at + words]
    }
}

/// One counted frame's **zero-trimmed polynomial** (its core, leading and trailing zeros
/// removed), coefficient `i` = the core's bit `i`: the reciprocal of the transmitted-order
/// polynomial, which preserves divisibility between polynomials with a nonzero constant term.
/// The coefficients live at `at` in the [`Arena`], over [`Core::words`] words.
#[derive(Clone, Copy)]
struct Core {
    at: usize,
    deg: usize,
}

impl Core {
    fn words(&self) -> usize {
        words_for(self.deg + 1)
    }
}

/// The zero-trimmed core of the `len`-bit frame `v`, written into `out`, and its degree; `None`
/// when `v` is zero.
fn core_of(v: &[u64], len: usize, out: &mut Vector) -> Option<usize> {
    let words = words_for(len);
    let top = highest(v, words)?;
    let low = (0..=top).find(|&i| get(v, i))?;
    out.fill(0);
    for i in low..=top {
        if get(v, i) {
            let j = i - low;
            out[j / 64] |= 1 << (j % 64);
        }
    }
    Some(top - low)
}

/// Whether the core `p` (degree `pdeg`) divides the core `q` (degree `qdeg`) over GF(2).
fn divides(p: &[u64], pdeg: usize, q: &[u64], qdeg: usize) -> bool {
    if qdeg < pdeg {
        return false;
    }
    let words = words_for(qdeg + 1);
    let mut r = [0u64; POLY_WORDS];
    r[..words].copy_from_slice(&q[..words]);
    while let Some(d) = highest(&r, words) {
        if d < pdeg {
            return false;
        }
        xor_shifted(&mut r, p, d - pdeg, words);
    }
    true
}

/// One basis vector of the affine span of the counted frames of one length (echelon form: its
/// highest set bit is its pivot, unique within the length), at `at` in the [`Arena`] over
/// `words_for(len)` words.
#[derive(Clone, Copy)]
struct Basis {
    len: usize,
    pivot: usize,
    at: usize,
}

/// The first frame of each length: the affine origin its span is measured from.
#[derive(Clone, Copy)]
struct Origin {
    len: usize,
    at: usize,
}

/// ADR-0022 §4.3.1's degenerate-frame guard: short-periodic as a whole, **or** with up to `w`
/// check-register bits trimmed from either end. An affine check with init = all ones is
/// satisfied by `1^w ‖ 0…0` (the first `w` bits cancel the register, the rest is idle fill), and
/// with xorout ≠ 0 by `init ‖ 0…0 ‖ xorout`: neither is periodic as a whole, so the whole-frame
/// guard let one idle frame confirm at width 32 (T-577's hole A).
///
/// `bits` is the span the check **covers**, not the frame it was cut from (T-928): with
/// `span.start_bit > 0` the cancelling `1^w` starts at `start_bit`, so trimming the ends of the
/// whole frame leaves the idle fill mixed with uncovered bits and misses the same frame.
///
/// `w` is the length of the shortest prefix that can **cancel** the check's state, not
/// necessarily the check field's width: a sum-type checksum consumes whole units, so its
/// cancelling prefix is `max(width, unit_bits)` (T-921 hole D — with `unit_bits > width` the
/// register trim left most of the cancelling unit inside the span and the idle frame counted).
pub fn degenerate_frame(bits: &[u8], w: usize) -> bool {
    let n = bits.len();
    let w = w.min(n);
    [(0, n), (w, n), (0, n - w), (w, n - w)]
        .iter()
        .any(|&(a, z)| {
            a <= z && {
                let span = &bits[a..z];
                is_short_periodic(span, SHORT_PERIOD_MAX_BITS)
                    || repeats_within(span, SHORT_PERIOD_MAX_BITS)
            }
        })
}

/// **Constant** (at any length ≥ 2), or periodic with at least two whole repeats at a period
/// `2 ≤ p ≤ max_period`.
///
/// [`is_short_periodic`] deliberately refuses to look at a span of `max_period` bits or fewer —
/// a span that short is trivially "periodic" at `p = n` — and that leaves the guard a hole
/// whenever the **idle tail** is short: `1^8 ‖ 0^16` is valid for an init = all-ones CRC-8, and
/// none of the four trims of it is longer than 16 bits, so nothing saw the idle fill and the
/// frame counted as an independent trial (T-921 hole C). "Constant", and "two whole repeats",
/// are structural claims at any length, so this closes the hole without calling a 16-bit span
/// periodic merely for being 16 bits long.
///
/// It only ever refuses a span — fewer bits, never more. The price is paid by a check whose
/// covered span is barely wider than its register, where a trim leaves few bits: a random
/// BCH(15, 7) word leaves 7, so ~2⁻⁶ of them read constant and are not counted. One-directional,
/// and the idle codeword it is there for (POCSAG's, say) is the commoner case by far.
fn repeats_within(bits: &[u8], max_period: usize) -> bool {
    let n = bits.len();
    if n >= 2 && bits.iter().all(|&b| b & 1 == bits[0] & 1) {
        return true;
    }
    (2..=max_period)
        .filter(|&p| n >= 2 * p)
        .any(|p| (p..n).all(|i| bits[i] & 1 == bits[i - p] & 1))
}

/// `check_distinct_valid` bookkeeping for a check block (ADR-0015 §2.2, ADR-0022 §4.2 and
/// **§4.3.1's count**, T-210, T-575).
///
/// - `tested` counts every unit checked (a frame, or a codeword).
/// - A unit is a **candidate** only when it passed **without FEC correction** and was not already
///   corrected upstream, and is not [`degenerate_frame`] (hole A).
/// - A candidate adds a trial only when its zero-trimmed polynomial is neither a multiple nor a
///   divisor of one already counted: for a linear check `g | P` implies `g | m·P`, so a
///   zero-padded shift of one burst (`m = x^k`), a frame holding two copies of one burst, or a
///   plain repeat is valid by construction once `P` is — one chance event, not several (hole B,
///   and the dedup the hash set used to do).
/// - The count is capped at the GF(2) **affine rank + 1** of the candidates, per frame length:
///   a frame in the span of the others' differences is valid automatically once they are.
/// - The chance per unit is `2^−width`, `width` the **smallest** check width among the units
///   tested (a frame-length-dependent check is scored at its weakest).
///
/// Storage is allocated once, at construction — one 8 KiB [`Arena`] plus the three entry
/// indices, **11 776 bytes measured** in all (T-928, from 100 352); `record` and `evidence`
/// never allocate.
pub struct CheckTally {
    tested: u64,
    min_width: Option<f64>,
    /// The counted cores, the per-length affine origins and the basis vectors.
    arena: Arena,
    counted: Vec<Core>,
    origins: Vec<Origin>,
    basis: Vec<Basis>,
    /// Candidates whose length already had an origin, or were the origin: `Σ (rank + 1)`.
    affine: u64,
}

impl Default for CheckTally {
    fn default() -> Self {
        Self {
            tested: 0,
            min_width: None,
            arena: Arena::new(),
            counted: Vec::with_capacity(MAX_COUNTED_FRAMES),
            origins: Vec::with_capacity(MAX_COUNTED_FRAMES),
            basis: Vec::with_capacity(MAX_COUNTED_FRAMES),
            affine: 0,
        }
    }
}

impl std::fmt::Debug for CheckTally {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CheckTally")
            .field("tested", &self.tested)
            .field("independent", &self.counted.len())
            .field("affine", &self.affine)
            .finish()
    }
}

impl CheckTally {
    /// Records one checked unit. `covered` is the unit's bits **as the check covers them** —
    /// `frame[span.start_bit .. frame_len − end_trim_bits]`, one BCH word, the parity units —
    /// never the whole frame a span was cut from: everything here (the degenerate guard, the
    /// polynomial, the affine span) is a statement about the bits the check actually read, and a
    /// caller passing the whole frame both misses hole A and counts frames that differ only
    /// outside the span as independent trials (T-928). `register_bits` is the check register's
    /// width `w` (the trim of the degenerate guard); `width_bits` is the chance width (it may be
    /// fractional, e.g. a block of offset words charges `log₂` of the alternatives).
    pub fn record(
        &mut self,
        covered: &[u8],
        clean_valid: bool,
        width_bits: f64,
        register_bits: usize,
    ) {
        self.tested += 1;
        if width_bits.is_finite() && width_bits > 0.0 {
            self.min_width = Some(self.min_width.map_or(width_bits, |w| w.min(width_bits)));
        }
        if !clean_valid
            || covered.len() > MAX_TALLY_FRAME_BITS
            || degenerate_frame(covered, register_bits)
        {
            return;
        }
        let len = covered.len();
        let mut v = [0u64; POLY_WORDS];
        for (j, &b) in covered.iter().enumerate() {
            if b & 1 == 1 {
                v[j / 64] |= 1 << (j % 64);
            }
        }
        let mut coef = [0u64; POLY_WORDS];
        let Some(deg) = core_of(&v, len, &mut coef) else {
            return;
        };
        let words = words_for(deg + 1);
        if self.counted.len() == MAX_COUNTED_FRAMES
            || self.counted.iter().any(|c| {
                let stored = self.arena.get(c.at, c.words());
                divides(stored, c.deg, &coef[..words], deg)
                    || divides(&coef[..words], deg, stored, c.deg)
            })
        {
            return;
        }
        let Some(at) = self.arena.push(&coef, words) else {
            return; // storage full: nothing more is counted — fewer bits, never more
        };
        self.counted.push(Core { at, deg });
        self.affine_insert(&v, len);
    }

    /// Adds `v` to its length's affine span; counts it when it widens the span.
    fn affine_insert(&mut self, v: &Vector, len: usize) {
        let words = words_for(len);
        let Some(origin) = self.origins.iter().find(|o| o.len == len).copied() else {
            if self.origins.len() < MAX_COUNTED_FRAMES
                && let Some(at) = self.arena.push(v, words)
            {
                self.origins.push(Origin { len, at });
                self.affine += 1;
            }
            return;
        };
        let mut d = *v;
        for (a, b) in d.iter_mut().zip(self.arena.get(origin.at, words)) {
            *a ^= b;
        }
        // Reduce by this length's basis, highest pivot first (the vector is kept sorted so).
        for b in self.basis.iter().filter(|b| b.len == len) {
            if get(&d, b.pivot) {
                for (a, x) in d.iter_mut().zip(self.arena.get(b.at, words)) {
                    *a ^= x;
                }
            }
        }
        let Some(pivot) = highest(&d, words) else {
            return; // in the span: valid automatically once the others are
        };
        if self.basis.len() == MAX_COUNTED_FRAMES {
            return;
        }
        let Some(stored) = self.arena.push(&d, words) else {
            return; // storage full, as above
        };
        let at = self
            .basis
            .iter()
            .position(|b| b.pivot < pivot)
            .unwrap_or(self.basis.len());
        self.basis.insert(
            at,
            Basis {
                len,
                pivot,
                at: stored,
            },
        );
        self.affine += 1;
    }

    /// `min(independent frames, Σ affine rank + 1)`: ADR-0022 §4.3.1's count, reported as the
    /// record's `raw` (the engine's `differences`).
    pub fn independent(&self) -> u64 {
        (self.counted.len() as u64).min(self.affine)
    }

    /// Resets for the next window, keeping the storage.
    pub fn clear(&mut self) {
        self.tested = 0;
        self.min_width = None;
        self.arena.used = 0;
        self.counted.clear();
        self.origins.clear();
        self.basis.clear();
        self.affine = 0;
    }

    /// Bytes of heap this tally holds: the fixed arena plus the three entry indices. Every
    /// crc/bch/checksum/parity instance pays it for as long as it lives (T-928).
    pub fn reserved_bytes(&self) -> usize {
        self.arena.words.len() * size_of::<u64>()
            + self.counted.capacity() * size_of::<Core>()
            + self.origins.capacity() * size_of::<Origin>()
            + self.basis.capacity() * size_of::<Basis>()
    }

    /// Emits `check_distinct_valid` at S5 when anything was tested.
    pub fn evidence(&self, out: &mut EvidenceSet) {
        let Some(width) = self.min_width else {
            return;
        };
        if self.tested == 0 {
            return;
        }
        let d = self.independent();
        let pushed = out.push(Evidence::new(
            Stage::S5,
            MetricId::CheckDistinctValid,
            GroupId::Undeclared,
            d as f32,
            u32::try_from(self.tested).unwrap_or(u32::MAX),
            check_bits(d, self.tested, width),
        ));
        debug_assert!(
            pushed.is_ok(),
            "a block emitted more than 4 evidence entries"
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

    /// T-577 hole A (ADR-0022 §4.3.1): an affine check with init = all ones is satisfied by
    /// `1^w ‖ 0…0`, and with xorout ≠ 0 by `init ‖ 0…0 ‖ xorout`. Neither is periodic as a
    /// whole, so the whole-frame guard counted one idle frame — enough to confirm at w = 32.
    /// The guard now also trims up to `w` bits from either end.
    #[test]
    fn an_init_cancel_idle_frame_is_degenerate_at_every_width() {
        for w in [8usize, 12, 16, 24, 32] {
            let mut f = vec![1u8; w];
            f.extend(std::iter::repeat_n(0u8, 64));
            assert!(!is_short_periodic(&f, SHORT_PERIOD_MAX_BITS), "w {w}");
            assert!(degenerate_frame(&f, w), "init ‖ 0…0, w {w}");
            let mut g = f.clone();
            g.extend(lcg_bits(w, w as u64));
            assert!(degenerate_frame(&g, w), "init ‖ 0…0 ‖ xorout, w {w}");
            let mut t = CheckTally::default();
            t.record(&f, true, w as f64, w);
            t.record(&g, true, w as f64, w);
            assert_eq!(t.independent(), 0, "w {w}");
        }
        // A real frame is not degenerate.
        assert!(!degenerate_frame(&lcg_bits(96, 5), 32));
    }

    /// T-921 hole C: an idle tail of 16 bits or fewer. [`is_short_periodic`] refuses to look at
    /// a span that short, so for `1^w ‖ 0^m` with `w + m ≤ 2w + 16` none of the four trims was
    /// ever tested and the init-cancel frame came back at w ≤ 16 — the widths the floor
    /// question is about. Two whole repeats at any length closes it.
    #[test]
    fn an_init_cancel_frame_with_a_short_idle_tail_is_degenerate() {
        for w in [4usize, 8, 12, 16] {
            for tail in 2..=SHORT_PERIOD_MAX_BITS {
                let mut f = vec![1u8; w];
                f.extend(std::iter::repeat_n(0u8, tail));
                assert!(
                    degenerate_frame(&f, w),
                    "1^{w} ‖ 0^{tail} must not be an independent trial"
                );
                let mut t = CheckTally::default();
                t.record(&f, true, w as f64, w);
                assert_eq!(t.independent(), 0, "w {w} tail {tail}");
            }
        }
        // T-921 hole D: a sum-type checksum consumes whole units, so the prefix that cancels its
        // state is `unit_bits` long even when the check field is narrower. Trimming only the
        // width leaves most of the cancelling unit in the span; the caller must pass
        // `max(width, unit_bits)`, which is what the `checksum` block now does.
        let unit = lcg_bits(32, 11);
        let mut f = unit.clone();
        f.extend(std::iter::repeat_n(0u8, 40));
        assert!(!degenerate_frame(&f, 8), "the width-8 trim cannot reach it");
        assert!(degenerate_frame(&f, 32), "the unit-wide trim does");
    }

    /// T-577 hole B (ADR-0022 §4.3.1): zero-padded shifts of one burst are all valid under a
    /// linear check once one is (g | P ⇒ g | x^k·P), and each counted as a distinct frame: one
    /// 2⁻ʷ event cleared the gate at every width ≥ 8. They are now one trial, as is a frame
    /// holding two copies of the burst.
    #[test]
    fn zero_padded_shifts_and_doubled_copies_of_one_burst_are_one_trial() {
        let burst = {
            let mut b = lcg_bits(40, 11);
            b[0] = 1;
            b[39] = 1;
            b
        };
        let frame = |at: usize| {
            let mut f = vec![0u8; 96];
            f[at..at + 40].copy_from_slice(&burst);
            f
        };
        let mut t = CheckTally::default();
        for at in [0, 3, 7, 20, 56] {
            t.record(&frame(at), true, 8.0, 8);
        }
        assert_eq!(t.independent(), 1, "five shifts of one burst");
        // Two copies of the burst in one frame: m = x^a + x^b, a multiple.
        let mut two = vec![0u8; 96];
        two[2..42].copy_from_slice(&burst);
        two[50..90].copy_from_slice(&burst);
        t.record(&two, true, 8.0, 8);
        assert_eq!(t.independent(), 1);
        // Different payloads still count.
        for seed in 20..25 {
            t.record(&lcg_bits(96, seed), true, 8.0, 8);
        }
        assert_eq!(t.independent(), 6);
        let mut set = EvidenceSet::new();
        t.evidence(&mut set);
        assert_eq!(set.iter().next().unwrap().raw, 6.0);
    }

    /// ADR-0022 §4.3.1: the count is capped at the affine rank + 1 per frame length — a frame in
    /// the span of the counted frames' differences (`a ⊕ b ⊕ c`) is valid automatically.
    #[test]
    fn a_frame_in_the_affine_span_of_the_counted_ones_adds_no_trial() {
        let a = lcg_bits(64, 31);
        let b = lcg_bits(64, 32);
        let c = lcg_bits(64, 33);
        let d: Vec<u8> = (0..64).map(|i| a[i] ^ b[i] ^ c[i]).collect();
        let mut t = CheckTally::default();
        for f in [&a, &b, &c, &d] {
            t.record(f, true, 16.0, 16);
        }
        assert_eq!(t.independent(), 3);
        t.clear();
        assert_eq!(t.independent(), 0);
        t.record(&a, true, 16.0, 16);
        assert_eq!(t.independent(), 1, "storage kept, counts reset");
    }

    #[test]
    fn check_tally_counts_clean_new_non_periodic_units_only() {
        let mut t = CheckTally::default();
        let a = lcg_bits(40, 1);
        let b = lcg_bits(40, 2);
        t.record(&a, true, 16.0, 16);
        t.record(&a, true, 16.0, 16); // repeat: one fact
        t.record(&b, false, 16.0, 16); // failed or corrected
        t.record(&[0; 40], true, 16.0, 16); // constant payload
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

    /// T-928 (T-575's review): every crc/bch/checksum/parity instance holds a `CheckTally`, and
    /// each used to carry `3 × 64` inline 4096-bit vectors — **100 352 bytes measured** (98 KiB:
    /// `Core` 520 + `Origin` 520 + `Basis` 528, times 64), whatever length the frames were, for
    /// every block of every live recipe and every searched candidate. The vectors now live in
    /// one fixed 8 KiB arena sized from `TALLY_FULL_FRAME_BITS`, still allocated once at
    /// construction so `record` stays allocation-free (ADR-0011 §1.4 rule 1).
    #[test]
    fn the_tally_holds_one_small_fixed_allocation() {
        let mut t = CheckTally::default();
        let bytes = t.reserved_bytes();
        eprintln!("[T-928] CheckTally residency {bytes} bytes (was 100352)");
        assert!(bytes <= 16 * 1024, "{bytes} bytes");
        // Filling it to the entry cap adds nothing: the storage is fixed at construction.
        for seed in 0..(MAX_COUNTED_FRAMES as u64 * 2) {
            t.record(&lcg_bits(128, seed), true, 16.0, 16);
        }
        assert_eq!(t.independent(), MAX_COUNTED_FRAMES as u64);
        assert_eq!(t.reserved_bytes(), bytes, "record never allocates");
        t.clear();
        assert_eq!(t.reserved_bytes(), bytes, "clear keeps the storage");
        t.record(&lcg_bits(128, 999), true, 16.0, 16);
        assert_eq!(t.independent(), 1, "and the arena is reusable");
    }

    /// The arena, not the entry cap, bounds the longest frames the tally analyses (4096 bits):
    /// it counts what fits and then stops — fewer bits, never more, and never a panic.
    #[test]
    fn the_longest_frames_are_counted_until_the_storage_is_full() {
        let mut t = CheckTally::default();
        for seed in 0..40u64 {
            t.record(&lcg_bits(MAX_TALLY_FRAME_BITS, 300 + seed), true, 32.0, 32);
        }
        let d = t.independent();
        eprintln!("[T-928] {d} of 40 frames of {MAX_TALLY_FRAME_BITS} bits counted");
        assert!((4..MAX_COUNTED_FRAMES as u64).contains(&d), "{d}");
        let mut set = EvidenceSet::new();
        t.evidence(&mut set);
        let e = *set.iter().next().unwrap();
        assert_eq!(e.raw, d as f32);
        assert_eq!(e.n, 40);
    }
}
