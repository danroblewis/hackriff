//! CRC / cyclic-code / parity search over frames (suggestions, never truth).
//!
//! # Method
//! A bit-serial CRC with generator `G` (degree `w`), init `I` and xorout `X` over `L` message
//! bits `M` followed by the `w`-bit field `F` satisfies, reading the covered bits as a polynomial
//! `P = M·x^w + F`: `P mod G = (I·x^L mod G) ⊕ X`, a constant for every frame of the same length
//! (and block class). So the XOR of two such frames is divisible by `G`, and `G` divides the GCD
//! of all frame differences. The search:
//!
//! 1. Hypothesis cells: coverage start `s`, trailing uncovered bits `t`, bit order (air order or
//!    per-byte reflected), and classes `m` (frames grouped by index mod m, each class with its
//!    own constant: RDS offset words). Stages, cheapest first: (A) `s = 0`, air order, `m = 1`,
//!    every `t`; (B) `s = 0`, air order, `m = 2..`; (C) other starts and reflected order. A later
//!    stage runs only when the earlier ones found nothing.
//! 2. Per cell, a robust GCD of the differences within each (class, length) group (differences
//!    that would drop the GCD below the smallest width are outliers; a bad reference frame is
//!    retried). Frames of different lengths are differenced only when no group has two frames
//!    (that assumes init 0 and is flagged).
//! 3. Per width (32 down to 3): degree-`w` divisors of the GCD with a constant term, enumerated
//!    exhaustively when the cofactor degree is ≤ 12, else the catalogue generators of that width
//!    are tested (`method: catalogue`).
//! 4. Validation over every **distinct** frame; init/xorout solved over GF(2) from two lengths
//!    (a linear system in `I`), else the standard settings, else `init 0` with the constant as
//!    xorout. Divisors of a stronger generator in the same cell are dropped as sub-codes.
//! 5. Claim only when `validated − constants ≥ 2`, `validated / tested ≥ 0.5` and the evidence
//!    `w·(validated − constants) − log2(hypotheses)` is ≥ 16 bits.
//!
//! A generator whose order `n` (of `x`) equals the covered length is a cyclic `(n, n−w)` code
//! and is labelled `bch` (named when it is a textbook BCH generator); a shorter coverage is a
//! shortened code used as a CRC. Byte-aligned CRCs of width 8/16/24/32 are also mapped onto the
//! RevEng model and the `framing::crc` catalogue by trial (packing order × field byte order).
//!
//! Parity: a trailing bit equal to the XOR of a span (even/odd), and per-character parity over
//! 8-bit characters at any phase.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use super::gf2::{Poly, clmul, gcd, mod_small, order_of_x, reflect, solve, xpow_mod};
use super::{
    BchFragment, BlockFragment, Budget, CrcBlocks, CrcFragment, CrcSpan, FragmentParams, Meter,
    ParityFragment, WorkReport, hex_bits,
};
use crate::framing::bits::{BitOrder, pack};
use crate::framing::crc::{CATALOGUE, CrcParams, Endianness, read_field};

/// Textbook binary BCH generators (full form) by `(n, k)`.
const BCH_TABLE: &[(usize, usize, u64)] = &[
    (7, 4, 0xB),
    (15, 11, 0x13),
    (15, 7, 0x1D1),
    (15, 5, 0x537),
    (31, 26, 0x25),
    (31, 21, 0x769),
];

/// Generators named outside the byte-CRC catalogue (full form, width).
const EXTRA_GENERATORS: &[(u64, u8, &str)] = &[
    (0x5B9, 10, "RDS (26,16) shortened cyclic"),
    (0x769, 10, "BCH(31,21)"),
    (0x1FF_F409, 24, "CRC-24/Mode-S"),
];

/// Search settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CodeSearchConfig {
    /// Smallest width tried (≥ 3).
    pub min_width: u8,
    /// Largest width tried (≤ 32).
    pub max_width: u8,
    /// Most trailing uncovered bits after the check field.
    pub max_tail_bits: usize,
    /// Coverage starts tried in stage C (stages A/B use 0).
    pub starts: Vec<usize>,
    /// Most classes (block index mod m) tried.
    pub max_classes: usize,
    /// Most differences folded into one GCD.
    pub max_gcd_diffs: usize,
    /// Work cap.
    pub budget: Budget,
}

impl Default for CodeSearchConfig {
    fn default() -> Self {
        Self {
            min_width: 3,
            max_width: 32,
            max_tail_bits: 16,
            starts: vec![8, 16, 24, 32],
            max_classes: 8,
            max_gcd_diffs: 16,
            budget: Budget::default(),
        }
    }
}

/// What a code suggestion is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodeKind {
    /// A CRC (or a shortened cyclic code used as one).
    Crc,
    /// A full-length cyclic code of length 2^m − 1 (BCH family).
    Bch,
}

/// Cyclic-code facts about a generator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CyclicInfo {
    /// Natural length (order of `x` modulo the generator).
    pub n: u64,
    /// Natural dimension `n − w`.
    pub k: u64,
    /// Covered length in these frames (`n` unless shortened).
    pub covered_bits: usize,
    /// Textbook name, when the generator is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// A byte-oriented RevEng description that reproduces the frames.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RevEngMatch {
    /// Catalogue name, or a `variant` name.
    pub name: String,
    /// RevEng parameters.
    pub params: CrcParams,
    /// Byte packing order of the frames.
    pub byte_order: BitOrder,
    /// Byte order of the check field.
    pub field_endianness: Endianness,
}

/// A CRC / cyclic-code suggestion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodeSuggestion {
    /// CRC or BCH.
    pub kind: CodeKind,
    /// Check bits.
    pub width: u8,
    /// Generator, full form with the `x^w` term.
    pub generator: u64,
    /// Generator, normal form without the `x^w` term.
    pub poly: u32,
    /// First covered bit.
    pub start_bit: usize,
    /// Trailing bits after the check field (not covered).
    pub tail_bits: usize,
    /// `air` (bits as received) or `byte-reflected` (bits reversed within each byte).
    pub bit_order: String,
    /// Frame index classes (1 = one constant for every frame).
    pub classes: usize,
    /// Register init in bit-serial form (0 when not separable).
    pub init: u64,
    /// Final XOR (bit-serial form); with classes > 1, the class-0 constant.
    pub xorout: u64,
    /// Whether `init` was separated from `xorout` (needs two frame lengths).
    pub init_resolved: bool,
    /// Per-class constants (offset words) when `classes > 1`, in class order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub class_constants: Vec<u64>,
    /// Frames of different lengths were combined (assumes init 0).
    pub cross_length: bool,
    /// `exhaustive` (GCD divisor) or `catalogue` (a catalogue generator tested against a loose
    /// GCD).
    pub method: String,
    /// Cyclic-code facts (when the order of `x` is within reach).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cyclic: Option<CyclicInfo>,
    /// Name of a known generator with this polynomial (naming only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub known_as: Option<String>,
    /// Byte-oriented RevEng mapping, when one reproduces the frames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reveng: Option<RevEngMatch>,
    /// Distinct frames that satisfy the suggestion.
    pub validated: usize,
    /// Distinct frames tested.
    pub tested: usize,
    /// `w·(validated − constants) − log2(hypotheses)`.
    pub evidence_bits: f64,
    /// 0–1: validated share, discounted when evidence is thin.
    pub score: f64,
    /// Human-readable reasons.
    pub reasons: Vec<String>,
    /// Recipe fragment (`crc` or `bch`).
    pub fragment: BlockFragment,
}

/// Where a parity check sits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "scope")]
pub enum ParityScope {
    /// One parity bit (`len − 1 − tail_bits`) over `[start_bit, parity bit]`.
    Frame {
        /// First covered bit.
        start_bit: usize,
        /// Bits after the parity bit.
        tail_bits: usize,
    },
    /// Every `char_bits`-bit character from `phase` has parity.
    Character {
        /// Character width.
        char_bits: usize,
        /// First character bit.
        phase: usize,
    },
}

/// A parity suggestion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParitySuggestion {
    /// Where.
    #[serde(flatten)]
    pub scope: ParityScope,
    /// `even` or `odd`.
    pub parity: String,
    /// Checks (frames or characters) that hold.
    pub validated: usize,
    /// Checks tested.
    pub tested: usize,
    /// 0–1.
    pub score: f64,
    /// Recipe fragment.
    pub fragment: BlockFragment,
}

/// The result of [`search_codes`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CodeReport {
    /// Code suggestions, best first.
    pub codes: Vec<CodeSuggestion>,
    /// Parity suggestions, best first.
    pub parity: Vec<ParitySuggestion>,
    /// Frames given.
    pub frames: usize,
    /// Work done.
    pub work: WorkReport,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    Air,
    ByteReflected,
}

impl View {
    fn name(self) -> &'static str {
        match self {
            Self::Air => "air",
            Self::ByteReflected => "byte-reflected",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Cell {
    start: usize,
    tail: usize,
    view: View,
    classes: usize,
}

/// One covered frame of a cell.
struct Covered {
    class: usize,
    len: usize,
    poly: Poly,
}

/// Searches CRC / cyclic codes and parity over `frames` (bits, one `u8` per bit), in order (the
/// order defines block classes).
pub fn search_codes(frames: &[Vec<u8>], cfg: &CodeSearchConfig) -> CodeReport {
    let indexed: Vec<(usize, &[u8])> = frames
        .iter()
        .enumerate()
        .map(|(i, f)| (i, f.as_slice()))
        .collect();
    let mut meter = Meter::new(cfg.budget);
    let (codes, parity) = search_indexed(&indexed, cfg, &mut meter);
    CodeReport {
        codes,
        parity,
        frames: frames.len(),
        work: meter.report(),
    }
}

/// [`search_codes`] over frames carrying their own class index (stream blocks keep their block
/// index so gaps do not shift classes).
pub(crate) fn search_indexed(
    frames: &[(usize, &[u8])],
    cfg: &CodeSearchConfig,
    meter: &mut Meter,
) -> (Vec<CodeSuggestion>, Vec<ParitySuggestion>) {
    let parity = search_parity(frames, cfg, meter);
    let min_w = usize::from(cfg.min_width.clamp(3, 32));
    let max_w = usize::from(cfg.max_width.clamp(cfg.min_width.clamp(3, 32), 32));
    let min_len = frames.iter().map(|f| f.1.len()).min().unwrap_or(0);
    let fixed = frames.iter().all(|f| f.1.len() == min_len);
    let mut found: Vec<CodeSuggestion> = Vec::new();
    if frames.len() < 3 || min_len < min_w + 2 {
        meter.skip("code search: needs ≥ 3 frames longer than the smallest width");
        return (found, parity);
    }
    let tails = 0..=cfg.max_tail_bits.min(min_len.saturating_sub(min_w + 2));
    let mut stages: Vec<Vec<Cell>> = Vec::new();
    stages.push(
        tails
            .clone()
            .map(|tail| Cell {
                start: 0,
                tail,
                view: View::Air,
                classes: 1,
            })
            .collect(),
    );
    if fixed {
        let mut b = Vec::new();
        for m in 2..=cfg.max_classes.min(frames.len() / 3) {
            for tail in tails.clone() {
                b.push(Cell {
                    start: 0,
                    tail,
                    view: View::Air,
                    classes: m,
                });
            }
        }
        stages.push(b);
    }
    let mut c = Vec::new();
    for view in [View::Air, View::ByteReflected] {
        let starts = std::iter::once(0).chain(cfg.starts.iter().copied());
        for start in starts {
            if view == View::Air && start == 0 {
                continue;
            }
            for tail in tails.clone() {
                if start + tail + 2 * min_w <= min_len {
                    c.push(Cell {
                        start,
                        tail,
                        view,
                        classes: 1,
                    });
                }
            }
        }
    }
    stages.push(c);
    let total_cells: usize = stages.iter().map(Vec::len).sum();
    // Hypotheses for the false-alarm bound: every cell × every width's candidate divisors.
    let log2_h = ((total_cells * (max_w - min_w + 1) * 8).max(1) as f64).log2();
    'stages: for (si, stage) in stages.iter().enumerate() {
        for cell in stage {
            if !meter.ok() {
                meter.skip(format!("code search stopped at the work cap in stage {si}"));
                break 'stages;
            }
            meter.hypothesis();
            search_cell(frames, *cell, min_w, max_w, log2_h, cfg, meter, &mut found);
        }
        if !found.is_empty() {
            break;
        }
    }
    found.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(b.evidence_bits.total_cmp(&a.evidence_bits))
    });
    for code in &mut found {
        attach_parity(code, &parity);
    }
    (found, parity)
}

fn covered_bits(frame: &[u8], cell: Cell) -> Option<Vec<u8>> {
    let end = frame.len().checked_sub(cell.tail)?;
    if end <= cell.start {
        return None;
    }
    let bits = &frame[cell.start..end];
    match cell.view {
        View::Air => Some(bits.to_vec()),
        View::ByteReflected => {
            if bits.len() % 8 != 0 {
                return None;
            }
            Some(
                bits.chunks_exact(8)
                    .flat_map(|c| c.iter().rev().copied())
                    .collect(),
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn search_cell(
    frames: &[(usize, &[u8])],
    cell: Cell,
    min_w: usize,
    max_w: usize,
    log2_h: f64,
    cfg: &CodeSearchConfig,
    meter: &mut Meter,
    found: &mut Vec<CodeSuggestion>,
) {
    // Distinct covered frames per (class, length).
    let mut seen = HashSet::new();
    let mut covered: Vec<Covered> = Vec::new();
    for (idx, f) in frames {
        let Some(bits) = covered_bits(f, cell) else {
            continue;
        };
        let class = idx % cell.classes;
        if bits.len() < 2 * min_w || !seen.insert((class, bits.clone())) {
            continue;
        }
        meter.charge(bits.len() as u64 / 8 + 1);
        covered.push(Covered {
            class,
            len: bits.len(),
            poly: Poly::from_bits(&bits),
        });
    }
    if covered.len() < 3 {
        return;
    }
    let Some((g, cross_length)) = robust_gcd(&covered, min_w, cfg.max_gcd_diffs, meter) else {
        return;
    };
    let Some(deg) = g.degree() else {
        return;
    };
    if deg < min_w {
        return;
    }
    let before = found.len();
    for w in (min_w..=max_w.min(deg)).rev() {
        if !meter.ok() {
            return;
        }
        let (cands, method) = divisors_of_degree(&g, w, meter);
        for gen_full in cands {
            // A divisor of a generator already accepted in this cell is a sub-code.
            if found[before..].iter().any(|s| {
                let (_, r, _) = Poly::from_u64(s.generator).div_rem(&Poly::from_u64(gen_full));
                r.is_zero()
            }) {
                continue;
            }
            if let Some(s) = validate(
                frames,
                &covered,
                cell,
                gen_full,
                w,
                cross_length,
                method,
                log2_h,
                meter,
            ) {
                found.push(s);
            }
        }
    }
}

/// GCD of frame differences within (class, length) groups; outliers skipped. Falls back to
/// differences across lengths (init 0) when no group has two frames. `Some((gcd, cross_length))`.
fn robust_gcd(
    covered: &[Covered],
    min_w: usize,
    max_diffs: usize,
    meter: &mut Meter,
) -> Option<(Poly, bool)> {
    let mut groups: BTreeMap<(usize, usize), Vec<usize>> = BTreeMap::new();
    for (i, c) in covered.iter().enumerate() {
        groups.entry((c.class, c.len)).or_default().push(i);
    }
    let grouped: Vec<&Vec<usize>> = groups.values().filter(|g| g.len() >= 2).collect();
    let pairs_in_groups: usize = grouped.iter().map(|g| g.len() - 1).sum();
    let cross = pairs_in_groups < 2;
    let sets: Vec<Vec<usize>> = if cross {
        let mut by_class: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (i, c) in covered.iter().enumerate() {
            by_class.entry(c.class).or_default().push(i);
        }
        by_class.into_values().filter(|v| v.len() >= 2).collect()
    } else {
        grouped.into_iter().cloned().collect()
    };
    if sets.is_empty() {
        return None;
    }
    for attempt in 0..3 {
        let mut diffs: Vec<Poly> = Vec::new();
        for set in &sets {
            let r = set[attempt.min(set.len() - 1)];
            for &j in set {
                if j == r {
                    continue;
                }
                let mut d = covered[j].poly.clone();
                d.add(&covered[r].poly);
                meter.charge(d.words() + 1);
                if !d.is_zero() {
                    diffs.push(d);
                }
                if diffs.len() >= max_diffs * 2 {
                    break;
                }
            }
        }
        if diffs.len() < 2 {
            return None;
        }
        let mut g = diffs[0].clone();
        let mut used = 1;
        let mut outliers = 0;
        for d in &diffs[1..] {
            if !meter.ok() {
                return None;
            }
            let (h, ops) = gcd(&g, d);
            meter.charge(ops);
            if h.degree().is_none_or(|x| x < min_w) {
                outliers += 1;
                if outliers * 4 > diffs.len() {
                    break;
                }
                continue;
            }
            g = h;
            used += 1;
            if used >= max_diffs {
                break;
            }
        }
        if used >= 2 && outliers * 4 <= diffs.len() {
            return Some((g, cross));
        }
    }
    None
}

/// Degree-`w` divisors of `g` with a constant term: exhaustive over the cofactor when its degree
/// is ≤ 12, else the catalogue generators of width `w` that divide `g`.
fn divisors_of_degree(g: &Poly, w: usize, meter: &mut Meter) -> (Vec<u64>, &'static str) {
    let deg = g.degree().unwrap_or(0);
    let d = deg - w;
    let mut out = Vec::new();
    if d <= 12 {
        for q in (1u64 << d)..(1u64 << (d + 1)) {
            let (p, r, ops) = g.div_rem(&Poly::from_u64(q));
            meter.charge(ops);
            if r.is_zero()
                && let Some(p) = p.to_u64()
                && p & 1 == 1
                && p.leading_zeros() as usize == 63 - w
                && !out.contains(&p)
            {
                out.push(p);
            }
        }
        (out, "exhaustive")
    } else {
        for gen_full in known_generators(w) {
            let (_, r, ops) = g.div_rem(&Poly::from_u64(gen_full));
            meter.charge(ops);
            if r.is_zero() && !out.contains(&gen_full) {
                out.push(gen_full);
            }
        }
        (out, "catalogue")
    }
}

fn known_generators(w: usize) -> Vec<u64> {
    let mut v: Vec<u64> = CATALOGUE
        .iter()
        .filter(|e| usize::from(e.params.width) == w)
        .map(|e| u64::from(e.params.poly) | (1u64 << w))
        .collect();
    v.extend(
        EXTRA_GENERATORS
            .iter()
            .filter(|(_, ew, _)| usize::from(*ew) == w)
            .map(|(g, _, _)| *g),
    );
    v.extend(
        BCH_TABLE
            .iter()
            .filter(|(n, k, _)| n - k == w)
            .map(|(_, _, g)| *g),
    );
    v.sort_unstable();
    v.dedup();
    v
}

fn known_name(gen_full: u64, w: usize) -> Option<String> {
    if let Some((_, _, name)) = EXTRA_GENERATORS
        .iter()
        .find(|(g, ew, _)| *g == gen_full && usize::from(*ew) == w)
    {
        return Some((*name).to_owned());
    }
    let names: Vec<&str> = CATALOGUE
        .iter()
        .filter(|e| {
            usize::from(e.params.width) == w && u64::from(e.params.poly) | (1 << w) == gen_full
        })
        .map(|e| e.name)
        .collect();
    (!names.is_empty()).then(|| format!("poly of {}", names.join(", ")))
}

#[allow(clippy::too_many_arguments)]
fn validate(
    frames: &[(usize, &[u8])],
    covered: &[Covered],
    cell: Cell,
    gen_full: u64,
    w: usize,
    cross_length: bool,
    method: &str,
    log2_h: f64,
    meter: &mut Meter,
) -> Option<CodeSuggestion> {
    let g = Poly::from_u64(gen_full);
    let mask = (1u64 << w) - 1;
    // Constant per (class, length): modal remainder.
    let mut consts: BTreeMap<(usize, usize), BTreeMap<u64, usize>> = BTreeMap::new();
    for c in covered {
        let (r, ops) = c.poly.rem(&g);
        meter.charge(ops);
        let r = r.to_u64().unwrap_or(0);
        let key = if cross_length {
            (c.class, 0)
        } else {
            (c.class, c.len)
        };
        *consts.entry(key).or_default().entry(r).or_default() += 1;
    }
    let tested = covered.len();
    let mut validated = 0;
    let mut modal: BTreeMap<(usize, usize), u64> = BTreeMap::new();
    for (key, hist) in &consts {
        let (&r, &n) = hist.iter().max_by_key(|(_, n)| **n).expect("non-empty");
        if n >= 2 || hist.len() == 1 {
            modal.insert(*key, r);
        }
        if n >= 2 {
            validated += n;
        }
    }
    let n_consts = consts
        .values()
        .filter(|h| h.values().any(|&n| n >= 2))
        .count();
    if validated < n_consts + 2 || validated * 2 < tested {
        return None;
    }
    let evidence = (w * (validated - n_consts)) as f64 - log2_h;
    if evidence < 16.0 {
        return None;
    }
    let classes = cell.classes;
    // Init / xorout.
    let mut reasons = Vec::new();
    let (init, xorout, init_resolved, class_constants) = if cross_length {
        let k = modal.get(&(0, 0)).copied().unwrap_or(0);
        reasons.push("frames of different lengths combined: assumes init 0".to_owned());
        (0, k, false, Vec::new())
    } else if classes > 1 {
        let per_class: Vec<u64> = (0..classes)
            .map(|c| {
                modal
                    .iter()
                    .find(|((cc, _), _)| *cc == c)
                    .map_or(0, |(_, v)| *v)
            })
            .collect();
        reasons.push(format!(
            "{classes} block classes with distinct constants (offset words), init taken as 0"
        ));
        (0, per_class[0], false, per_class)
    } else {
        let lens: Vec<(usize, u64)> = modal.iter().map(|((_, l), k)| (*l, *k)).collect();
        solve_init(&lens, gen_full, w, mask)
    };
    let score = (validated as f64 / tested as f64) * (1.0 - (-evidence / 16.0).exp());
    let order = order_of_x(gen_full, 1 << 16);
    let min_cov = covered.iter().map(|c| c.len).min().unwrap_or(0);
    let max_cov = covered.iter().map(|c| c.len).max().unwrap_or(0);
    let cyclic = order.map(|n| CyclicInfo {
        n,
        k: n - w as u64,
        covered_bits: max_cov,
        name: BCH_TABLE
            .iter()
            .find(|(bn, bk, bg)| *bg == gen_full && *bn as u64 == n && bn - bk == w)
            .map(|(bn, bk, _)| format!("BCH({bn},{bk})")),
    });
    let is_bch = matches!(&cyclic, Some(c) if c.n as usize == min_cov && min_cov == max_cov)
        && (min_cov + 1).is_power_of_two()
        && classes == 1
        && init == 0
        && xorout == 0;
    let kind = if is_bch { CodeKind::Bch } else { CodeKind::Crc };
    if let Some(c) = &cyclic {
        if is_bch {
            reasons.push(format!(
                "generator divides x^{} + 1: cyclic ({}, {}) code{}",
                c.n,
                c.n,
                c.k,
                c.name
                    .as_ref()
                    .map(|n| format!(" = {n}"))
                    .unwrap_or_default()
            ));
        } else if (c.n as usize) > max_cov {
            reasons.push(format!(
                "shortened cyclic ({}, {}) code over {max_cov} covered bits",
                c.n, c.k
            ));
        }
    }
    reasons.push(format!(
        "{validated}/{tested} distinct frames satisfy it ({} constant{})",
        n_consts,
        if n_consts == 1 { "" } else { "s" }
    ));
    let reveng = if classes == 1 {
        reveng_match(frames, cell, gen_full, w, init, xorout, meter)
    } else {
        None
    };
    let poly = (gen_full & mask) as u32;
    let fragment = match kind {
        CodeKind::Bch => BlockFragment {
            block: "bch".into(),
            params: FragmentParams::Bch(BchFragment {
                word_bits: min_cov + cell.tail + cell.start,
                n: min_cov,
                k: min_cov - w,
                poly: hex_bits(gen_full, w + 1),
                parity: None,
            }),
        },
        CodeKind::Crc => {
            let (refin, init_s, xor_s, poly_s) = match &reveng {
                Some(r) => (
                    r.params.refin,
                    hex_bits(u64::from(r.params.init), w),
                    hex_bits(u64::from(r.params.xorout), w),
                    hex_bits(u64::from(r.params.poly), w),
                ),
                None => (
                    cell.view == View::ByteReflected,
                    hex_bits(init, w),
                    hex_bits(xorout, w),
                    hex_bits(gen_full & mask, w),
                ),
            };
            BlockFragment {
                block: "crc".into(),
                params: FragmentParams::Crc(CrcFragment {
                    width: w as u8,
                    poly: if classes > 1 {
                        hex_bits(gen_full, w + 1)
                    } else {
                        poly_s
                    },
                    init: init_s,
                    refin,
                    refout: refin,
                    xorout: xor_s,
                    span: (classes == 1).then_some(CrcSpan {
                        start_bit: cell.start,
                        end_trim_bits: cell.tail,
                    }),
                    blocks: (classes > 1).then(|| CrcBlocks {
                        data_bits: min_cov - w,
                        check_bits: w,
                        offsets: class_constants
                            .iter()
                            .map(|c| vec![hex_bits(*c, w)])
                            .collect(),
                    }),
                    strip: false,
                }),
            }
        }
    };
    Some(CodeSuggestion {
        kind,
        width: w as u8,
        generator: gen_full,
        poly,
        start_bit: cell.start,
        tail_bits: cell.tail,
        bit_order: cell.view.name().to_owned(),
        classes,
        init,
        xorout,
        init_resolved,
        class_constants,
        cross_length,
        method: method.to_owned(),
        cyclic,
        known_as: known_name(gen_full, w),
        reveng,
        validated,
        tested,
        evidence_bits: evidence,
        score,
        reasons,
        fragment,
    })
}

/// Init/xorout from per-length constants `K_L = (I·x^L mod G) ⊕ X`, where `L` is the message
/// length (covered − w).
fn solve_init(
    lens: &[(usize, u64)],
    gen_full: u64,
    w: usize,
    mask: u64,
) -> (u64, u64, bool, Vec<u64>) {
    let reg = |i: u64, l: usize| mod_small(clmul(i, xpow_mod(l as u64, gen_full)), gen_full);
    if lens.len() >= 2 {
        let (l1, k1) = (lens[0].0 - w, lens[0].1);
        let (l2, k2) = (lens[1].0 - w, lens[1].1);
        let cols: Vec<u64> = (0..w).map(|i| reg(1 << i, l1) ^ reg(1 << i, l2)).collect();
        if let Some((i0, free)) = solve(&cols, k1 ^ k2, w as u32) {
            // Prefer a standard init within the solution set.
            let consistent = |i: u64| {
                let x = k1 ^ reg(i, l1);
                lens.iter()
                    .all(|(l, k)| reg(i, l - w) ^ x == *k)
                    .then_some(x)
            };
            for i in [0, mask, i0] {
                if let Some(x) = consistent(i) {
                    return (i, x, free == 0, Vec::new());
                }
            }
        }
    }
    let (l, k) = lens.first().map_or((w, 0), |(l, k)| (l - w, *k));
    for (i, x) in [(0, 0), (mask, 0), (0, mask), (mask, mask)] {
        if reg(i, l) ^ x == k {
            return (i, x, false, Vec::new());
        }
    }
    (0, k, false, Vec::new())
}

/// Tries byte-oriented RevEng descriptions (catalogue entries with this generator first, then
/// the bit-serial init/xorout in both reflections) against up to 8 frames.
fn reveng_match(
    frames: &[(usize, &[u8])],
    cell: Cell,
    gen_full: u64,
    w: usize,
    init: u64,
    xorout: u64,
    meter: &mut Meter,
) -> Option<RevEngMatch> {
    if !(8..=32).contains(&w) || w % 8 != 0 {
        return None;
    }
    let mask = (1u64 << w) - 1;
    let poly = (gen_full & mask) as u32;
    let sample: Vec<&[u8]> = frames
        .iter()
        .filter_map(|(_, f)| {
            let end = f.len().checked_sub(cell.tail)?;
            ((end - cell.start.min(end)) % 8 == 0 && end > cell.start + w)
                .then(|| &f[cell.start..end])
        })
        .take(8)
        .collect();
    if sample.len() < 3 {
        return None;
    }
    let mut cands: Vec<(Option<&'static str>, CrcParams)> = CATALOGUE
        .iter()
        .filter(|e| usize::from(e.params.width) == w && e.params.poly == poly)
        .map(|e| (Some(e.name), e.params))
        .collect();
    let w32 = w as u32;
    for refl in [false, true] {
        for i in [init, reflect(init, w32), 0, mask] {
            for x in [xorout, reflect(xorout, w32), 0, mask] {
                cands.push((
                    None,
                    CrcParams {
                        width: w as u8,
                        poly,
                        init: i as u32,
                        refin: refl,
                        refout: refl,
                        xorout: x as u32,
                    },
                ));
            }
        }
    }
    for (name, params) in cands {
        for order in BitOrder::ALL {
            for e in [Endianness::Big, Endianness::Little] {
                meter.charge(sample.len() as u64 * 16);
                let ok = sample.iter().all(|bits| {
                    let bytes = pack(bits, order);
                    let nb = bytes.len() - w / 8;
                    params.compute(&bytes[..nb]) == read_field(&bytes[nb..], w as u8, e)
                });
                if ok {
                    return Some(RevEngMatch {
                        name: name.map_or_else(|| params.name(), str::to_owned),
                        params,
                        byte_order: order,
                        field_endianness: e,
                    });
                }
            }
        }
    }
    None
}

fn search_parity(
    frames: &[(usize, &[u8])],
    cfg: &CodeSearchConfig,
    meter: &mut Meter,
) -> Vec<ParitySuggestion> {
    let mut out = Vec::new();
    let mut distinct: Vec<&[u8]> = Vec::new();
    let mut seen = HashSet::new();
    for (_, f) in frames {
        if seen.insert(*f) {
            distinct.push(f);
        }
    }
    let min_len = distinct.iter().map(|f| f.len()).min().unwrap_or(0);
    if distinct.len() < 8 || min_len < 4 {
        return out;
    }
    // Frame parity: bit (len − 1 − t) over [s, len − t).
    for tail in 0..=cfg.max_tail_bits.min(min_len - 2) {
        for start in std::iter::once(0).chain(cfg.starts.iter().copied()) {
            if start + 2 > min_len - tail {
                continue;
            }
            meter.charge(distinct.len() as u64 * (min_len as u64 / 64 + 1));
            let ones = distinct
                .iter()
                .filter(|f| f[start..f.len() - tail].iter().fold(0u8, |a, b| a ^ b) == 1)
                .count();
            let n = distinct.len();
            for (parity, hold) in [("even", n - ones), ("odd", ones)] {
                if hold as f64 >= 0.97 * n as f64 {
                    let p = parity.to_owned();
                    out.push(ParitySuggestion {
                        scope: ParityScope::Frame {
                            start_bit: start,
                            tail_bits: tail,
                        },
                        parity: p.clone(),
                        validated: hold,
                        tested: n,
                        score: (hold as f64 / n as f64) * (1.0 - 0.5f64.powi(n as i32 - 4)),
                        fragment: BlockFragment {
                            block: "parity".into(),
                            params: FragmentParams::Parity(ParityFragment {
                                parity: p,
                                char_bits: None,
                                span: CrcSpan {
                                    start_bit: start,
                                    end_trim_bits: tail,
                                },
                            }),
                        },
                    });
                }
            }
        }
    }
    // Character parity (8-bit characters at any phase).
    for phase in 0..8 {
        let mut hold = [0usize; 2];
        let mut n = 0;
        for f in &distinct {
            for c in f.get(phase..).unwrap_or(&[]).chunks_exact(8) {
                n += 1;
                hold[usize::from(c.iter().fold(0u8, |a, b| a ^ b))] += 1;
            }
        }
        meter.charge(n as u64);
        if n < 32 {
            continue;
        }
        for (parity, h) in [("even", hold[0]), ("odd", hold[1])] {
            if h as f64 >= 0.85 * n as f64 {
                let p = parity.to_owned();
                out.push(ParitySuggestion {
                    scope: ParityScope::Character {
                        char_bits: 8,
                        phase,
                    },
                    parity: p.clone(),
                    validated: h,
                    tested: n,
                    score: h as f64 / n as f64,
                    fragment: BlockFragment {
                        block: "parity".into(),
                        params: FragmentParams::Parity(ParityFragment {
                            parity: p,
                            char_bits: Some(8),
                            span: CrcSpan {
                                start_bit: phase,
                                end_trim_bits: 0,
                            },
                        }),
                    },
                });
            }
        }
    }
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    out
}

/// A BCH suggestion leaving one trailing bit gets the frame-parity result over the whole word.
fn attach_parity(code: &mut CodeSuggestion, parity: &[ParitySuggestion]) {
    if code.kind != CodeKind::Bch || code.tail_bits != 1 {
        return;
    }
    let found = parity.iter().find(|p| {
        p.scope
            == ParityScope::Frame {
                start_bit: code.start_bit,
                tail_bits: 0,
            }
    });
    if let (Some(p), FragmentParams::Bch(b)) = (found, &mut code.fragment.params) {
        b.parity = Some(p.parity.clone());
        code.reasons
            .push(format!("trailing bit is {} parity over the word", p.parity));
    }
}
