//! Sync-word, frame-period and linear-block period hunting over bits (suggestions).
//!
//! # Sync words
//! 1. Seeds: every `seed_bits`-bit window (default 16) is counted with its complement folded in
//!    (so an inverted stream finds the same seed). Windows inside alternating or constant runs
//!    of ≥ 24 bits (preambles, idle carrier) and windows with a period ≤ 4 are skipped. In frames
//!    mode a window counts once per frame. Seeds need ≥ `min_occurrences` and ≥ 4× the chance
//!    expectation.
//! 2. Extension: each seed's occurrences are grown bit by bit to the right, then the left, while
//!    ≥ `consensus` of the occurrences agree (up to `max_sync_bits`). A leading alternating or
//!    constant run (the preamble) is stripped and reported; after a strip the word is snapped to
//!    a whole number of bytes when that restores ≤ 3 stripped bits or trims ≤ 4 trailing bits (a
//!    run of ones eats the leading ones of the word, a length byte's leading zeros extend it).
//!    Before that, the pattern is cut to the bits every tolerant occurrence agrees on and grown
//!    again over all occurrences; a sync grown into an adjacent fill word is trimmed when the
//!    trimmed word is found clearly more often. Patterns with < 16 evidence bits are dropped.
//! 3. Recount with `max_errors` (default `len / 16`) in both polarities; spacing statistics:
//!    modal interval, regularity (share of intervals equal to it), back-to-back repeats, and the
//!    share of occurrences directly after a preamble.
//! 4. Chance correction: `λ`, the occurrences expected in random bits of the same lengths (either
//!    polarity, ≤ `max_errors`; frames mode: frames holding one). Kind: `fill` (repeats back to
//!    back: idle codewords), `sync` (regular spacing, after a preamble, or in most frames beyond
//!    chance), else `repeat`. Ranked by kind, then by evidence
//!    `(occurrences − λ) × (len − log2 positions − errors·log2 len)` weighted by regularity and
//!    preamble share (`relative_score`). The absolute `score` is `1 − e^(−significance/32)`,
//!    significance = −log2 of the Chernoff bound on the count given `λ`, minus `len`.
//!
//! # Periods
//! - **Autocorrelation** (masked: degenerate runs excluded): agreement at each lag as a z-score;
//!   local maxima ≥ 8 σ, multiples of a stronger shorter lag marked as harmonics.
//! - **Linear block**: for block length `N` and alignment `o`, stack distinct `N`-bit blocks
//!   (≥ N + 8 rows) and take the GF(2) rank. Random blocks have full rank (a deficiency `d` has
//!   probability ≈ 2^(−d·(rows−N))); codewords of an (affine) linear code do not. Constant
//!   columns explain part of a deficiency and are reported. When a regular sync word exists the
//!   blocks are taken relative to each run of sync occurrences, so gaps between transmissions do
//!   not break the alignment.
//!
//! [`analyze_stream`] runs all three, then the code search ([`super::codes`]) over the blocks of
//! the best linear-block period, which recovers e.g. RDS's generator and offset words.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::codes::{CodeSearchConfig, CodeSuggestion, ParitySuggestion, search_indexed};
use super::{
    BlockFragment, Budget, FragmentParams, Meter, OffsetWord, OffsetWordsParams, SyncWordParams,
    WorkReport, bits_value, hex_bits,
};
use crate::framing::bits::{BitOrder, bit_string, hex, parse_bit_string};

/// Degenerate-run length (alternating or constant) excluded from seeds and periods.
const DEGENERATE_RUN: usize = 24;

/// Hunt settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    /// Seed window, bits (8–32).
    pub seed_bits: usize,
    /// Longest sync word, bits (≤ 64).
    pub max_sync_bits: usize,
    /// Fewest occurrences (or frames) for a pattern.
    pub min_occurrences: usize,
    /// Agreement needed to extend a pattern by one bit.
    pub consensus: f64,
    /// Tolerated bit errors when recounting (default `len / 16`).
    pub max_errors: Option<usize>,
    /// Most sync suggestions returned.
    pub max_candidates: usize,
    /// Smallest autocorrelation lag.
    pub min_lag: usize,
    /// Largest autocorrelation lag.
    pub max_lag: usize,
    /// Smallest linear-block length.
    pub min_block_bits: usize,
    /// Largest linear-block length (≤ 128).
    pub max_block_bits: usize,
    /// Code search over the best block period.
    pub codes: CodeSearchConfig,
    /// Work cap for the whole call.
    pub budget: Budget,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            seed_bits: 16,
            max_sync_bits: 64,
            min_occurrences: 3,
            consensus: 0.9,
            max_errors: None,
            max_candidates: 8,
            min_lag: 8,
            max_lag: 4096,
            min_block_bits: 8,
            max_block_bits: 64,
            codes: CodeSearchConfig::default(),
            budget: Budget::default(),
        }
    }
}

/// What a repeated pattern looks like.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PatternKind {
    /// Regularly spaced, after a preamble, or in most frames.
    Sync,
    /// Repeated without a clear frame role (repeated content).
    Repeat,
    /// Repeats back to back (idle/fill words).
    Fill,
}

/// A sync-word (repeated pattern) suggestion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SyncSuggestion {
    /// The pattern as observed most often, `"0101…"` in air order.
    pub bits: String,
    /// Width.
    pub bit_len: usize,
    /// MSB-first hex of the air-order bits (`0x…`).
    pub hex: String,
    /// Hex of the bytes when each byte is sent LSB first (whole bytes only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hex_lsb_first: Option<String>,
    /// Hex of the complement (the same word through an inverting demodulator).
    pub complement_hex: String,
    /// Kind.
    pub kind: PatternKind,
    /// Occurrences in the observed polarity.
    pub occurrences: usize,
    /// Occurrences of the complement.
    pub inverted_occurrences: usize,
    /// Errors tolerated when counting.
    pub max_errors: usize,
    /// Most common spacing between occurrences (stream mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modal_interval_bits: Option<usize>,
    /// Share of intervals equal to the modal one (frames mode: share of frames holding it).
    pub regularity: f64,
    /// Share of occurrences directly after ≥ 16 alternating bits.
    pub preamble_fraction: f64,
    /// Bits of preamble stripped from the front of the extended pattern.
    pub preamble_bits: usize,
    /// Frames holding the pattern (frames mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames_with: Option<usize>,
    /// Most common start offset within a frame (frames mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modal_offset: Option<usize>,
    /// Evidence, bits: occurrences beyond the chance expectation × information per occurrence
    /// (the ranking key).
    pub evidence_bits: f64,
    /// Significance, bits: −log2 of the Chernoff bound on this many occurrences in random bits
    /// of the same lengths, minus the pattern width (any pattern of that width could have been
    /// found).
    pub significance_bits: f64,
    /// 0–1, absolute: `1 − e^(−significance/32)`; noise scores ≈ 0.
    pub score: f64,
    /// 0–1, the ranking key relative to the best suggestion of this answer.
    pub relative_score: f64,
    /// Human-readable reasons.
    pub reasons: Vec<String>,
    /// `sync_search` fragment.
    pub fragment: BlockFragment,
    /// Occurrences (segment, bit position, inverted).
    #[serde(skip)]
    pub(crate) positions: Vec<(usize, usize, bool)>,
}

/// How a period was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PeriodMethod {
    /// Masked autocorrelation peak.
    Autocorrelation,
    /// GF(2) rank deficiency of stacked blocks.
    LinearBlock,
}

/// A period suggestion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PeriodSuggestion {
    /// Period (block length), bits.
    pub period_bits: usize,
    /// Alignment: bit position of a block start in the stream, modulo the period (linear
    /// block; relative to the first sync run when anchored).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset_bits: Option<usize>,
    /// Method.
    pub method: PeriodMethod,
    /// Autocorrelation agreement at the lag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agreement: Option<f64>,
    /// Autocorrelation z-score.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub z: Option<f64>,
    /// Rank of the stacked blocks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank: Option<usize>,
    /// `period − rank`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deficiency: Option<usize>,
    /// Columns constant over every block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constant_columns: Option<usize>,
    /// Distinct blocks stacked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<usize>,
    /// A shorter period this one is a multiple of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harmonic_of: Option<usize>,
    /// Evidence, bits (linear block) or σ (autocorrelation).
    pub evidence: f64,
    /// 0–1 within its method.
    pub score: f64,
    /// Human-readable reasons.
    pub reasons: Vec<String>,
}

/// [`analyze_stream`] result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StreamReport {
    /// Stream length.
    pub bit_len: usize,
    /// Period suggestions: linear-block first, then autocorrelation.
    pub periods: Vec<PeriodSuggestion>,
    /// Sync suggestions, best first.
    pub syncs: Vec<SyncSuggestion>,
    /// The linear-block period the code search ran on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_period: Option<PeriodSuggestion>,
    /// Code suggestions over those blocks.
    pub block_codes: Vec<CodeSuggestion>,
    /// Parity suggestions over those blocks.
    pub block_parity: Vec<ParitySuggestion>,
    /// A `sync_search` `offset-words` fragment when the best block code has per-class constants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset_words: Option<BlockFragment>,
    /// Work done.
    pub work: WorkReport,
}

/// [`hunt_sync_frames`] result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncFramesReport {
    /// Frames given.
    pub frames: usize,
    /// Sync suggestions, best first.
    pub syncs: Vec<SyncSuggestion>,
    /// Work done.
    pub work: WorkReport,
}

/// Sync, period and block-code hunt over one continuous bitstream (one `u8` per bit).
pub fn analyze_stream(bits: &[u8], cfg: &SyncConfig) -> StreamReport {
    let mut meter = Meter::new(cfg.budget);
    let n = bits.len();
    let degenerate = degenerate_mask(bits);
    let prefix = prefix_counts(&degenerate);
    let syncs = hunt(&[bits], false, cfg, &mut meter);
    let segments = syncs
        .iter()
        .find(|s| {
            s.kind == PatternKind::Sync
                && s.regularity >= 0.5
                && s.modal_interval_bits
                    .is_some_and(|d| d >= 2 * cfg.min_block_bits)
        })
        .map_or_else(|| vec![(0, n)], |s| anchor_segments(s, n));
    let mut periods = linear_blocks(bits, &prefix, &segments, cfg, &mut meter);
    let best_block = periods
        .iter()
        .find(|p| {
            p.harmonic_of.is_none() && p.deficiency.unwrap_or(0) > p.constant_columns.unwrap_or(0)
        })
        .cloned();
    periods.extend(autocorrelation(bits, &degenerate, cfg, &mut meter));
    let mut report = StreamReport {
        bit_len: n,
        periods,
        syncs,
        ..StreamReport::default()
    };
    if let Some(p) = best_block {
        let blocks = cut_blocks(bits, &prefix, &segments, &p, 4096);
        let frames: Vec<(usize, &[u8])> = blocks.iter().map(|(i, b)| (*i, *b)).collect();
        let mut codes_cfg = cfg.codes.clone();
        codes_cfg.max_tail_bits = codes_cfg.max_tail_bits.min(p.period_bits / 2);
        let (codes, parity) = search_indexed(&frames, &codes_cfg, &mut meter);
        report.offset_words = codes
            .first()
            .filter(|c| c.classes > 1)
            .map(|c| offset_words_fragment(c, p.period_bits));
        report.block_codes = codes;
        report.block_parity = parity;
        report.block_period = Some(p);
    }
    report.work = meter.report();
    report
}

/// Sync hunt over separate frames (bursts): a pattern counts once per frame.
pub fn hunt_sync_frames(frames: &[Vec<u8>], cfg: &SyncConfig) -> SyncFramesReport {
    let mut meter = Meter::new(cfg.budget);
    let segs: Vec<&[u8]> = frames.iter().map(Vec::as_slice).collect();
    let syncs = hunt(&segs, true, cfg, &mut meter);
    SyncFramesReport {
        frames: frames.len(),
        syncs,
        work: meter.report(),
    }
}

/// The bits after the first occurrence of `pattern` (either polarity, ≤ `max_errors`) in each
/// frame, complemented when the occurrence was inverted. Frames without one are dropped.
pub fn align_on_sync(frames: &[Vec<u8>], pattern: &[u8], max_errors: usize) -> Vec<Vec<u8>> {
    let mut meter = Meter::new(Budget { max_ops: u64::MAX });
    align_frames(frames, pattern, max_errors, &mut meter)
}

/// [`align_on_sync`] within a work meter (frames after the cap are dropped and reported).
pub(crate) fn align_frames(
    frames: &[Vec<u8>],
    pattern: &[u8],
    max_errors: usize,
    meter: &mut Meter,
) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for f in frames {
        // A sliding match per bit, then the copy.
        if !meter.charge(f.len() as u64 * 3 + 32) {
            meter.skip("sync alignment stopped at the work cap: later frames dropped");
            break;
        }
        let hits = recount(&[f.as_slice()], pattern, max_errors, true);
        if let Some(&(_, pos, inv)) = hits.first() {
            out.push(
                f[pos + pattern.len()..]
                    .iter()
                    .map(|b| (b & 1) ^ u8::from(inv))
                    .collect(),
            );
        }
    }
    out
}

fn degenerate_mask(bits: &[u8]) -> Vec<bool> {
    let n = bits.len();
    let mut m = vec![false; n];
    let mut i = 0;
    while i < n {
        let mut j = i + 1;
        while j < n && bits[j] == bits[i] {
            j += 1;
        }
        if j - i >= DEGENERATE_RUN {
            m[i..j].fill(true);
        }
        i = j;
    }
    let mut i = 0;
    while i < n {
        let mut j = i + 1;
        while j < n && bits[j] != bits[j - 1] {
            j += 1;
        }
        if j - i >= DEGENERATE_RUN {
            m[i..j].fill(true);
        }
        i = j;
    }
    m
}

fn prefix_counts(mask: &[bool]) -> Vec<u32> {
    let mut p = Vec::with_capacity(mask.len() + 1);
    p.push(0);
    let mut acc = 0;
    for &m in mask {
        acc += u32::from(m);
        p.push(acc);
    }
    p
}

fn low_complexity(w: u64, l: usize) -> bool {
    (1..=4).any(|p| {
        let m = (1u64 << (l - p)) - 1;
        ((w ^ (w >> p)) & m) == 0
    })
}

fn alternating_before(bits: &[u8], end: usize) -> usize {
    if end < 2 {
        return 0;
    }
    let mut k = 1;
    while k < end && bits[end - 1 - k] != bits[end - k] {
        k += 1;
    }
    if k >= 2 { k } else { 0 }
}

fn hunt(
    segs: &[&[u8]],
    frames_mode: bool,
    cfg: &SyncConfig,
    meter: &mut Meter,
) -> Vec<SyncSuggestion> {
    let l = cfg.seed_bits.clamp(8, 32);
    let lmask = (1u64 << l) - 1;
    let total_bits: usize = segs.iter().map(|s| s.len()).sum();
    if total_bits < 4 * l {
        return Vec::new();
    }
    let prefixes: Vec<Vec<u32>> = segs
        .iter()
        .map(|s| prefix_counts(&degenerate_mask(s)))
        .collect();
    let mut counts: HashMap<u64, u32> = HashMap::new();
    for (si, s) in segs.iter().enumerate() {
        // Degenerate mask, prefix counts and a hash-map update per window (plus a per-frame
        // hash-set insert in frames mode).
        meter.charge(s.len() as u64 * if frames_mode { 48 } else { 24 } + 64);
        let mut here = HashSet::new();
        let mut w = 0u64;
        for (i, &b) in s.iter().enumerate() {
            w = ((w << 1) | u64::from(b & 1)) & lmask;
            if i + 1 < l {
                continue;
            }
            let start = i + 1 - l;
            if prefixes[si][i + 1] != prefixes[si][start] || low_complexity(w, l) {
                continue;
            }
            let key = w.min(!w & lmask);
            if frames_mode && !here.insert(key) {
                continue;
            }
            *counts.entry(key).or_default() += 1;
        }
    }
    let chance = if frames_mode {
        let avg = total_bits as f64 / segs.len() as f64;
        segs.len() as f64 * (avg / 2f64.powi(l as i32 - 1)).min(1.0)
    } else {
        total_bits as f64 / 2f64.powi(l as i32 - 1)
    };
    let floor = (cfg.min_occurrences as f64).max(4.0 * chance + 2.0);
    let mut seeds: Vec<(u64, u32)> = counts
        .into_iter()
        .filter(|(_, c)| f64::from(*c) >= floor)
        .collect();
    seeds.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let positions_count = if frames_mode {
        (total_bits / segs.len()).max(1)
    } else {
        total_bits
    };
    let mut extended: Vec<Vec<u8>> = Vec::new();
    let mut out: Vec<SyncSuggestion> = Vec::new();
    for (key, _) in seeds.into_iter().take(512) {
        if extended.len() >= cfg.max_candidates * 3 || !meter.ok() {
            if !meter.ok() {
                meter.skip("sync hunt stopped at the work cap");
            }
            break;
        }
        let seed_bits: Vec<u8> = (0..l).rev().map(|i| ((key >> i) & 1) as u8).collect();
        let seed_inv: Vec<u8> = seed_bits.iter().map(|b| 1 - b).collect();
        if extended
            .iter()
            .any(|p| contains(p, &seed_bits) || contains(p, &seed_inv))
        {
            continue;
        }
        let occ = find_exact(segs, key, l, lmask, frames_mode, meter);
        if occ.len() < cfg.min_occurrences {
            continue;
        }
        let pattern = extend(segs, &occ, seed_bits.clone(), cfg);
        meter.charge(extend_ops(occ.len(), pattern.len(), l));
        extended.push(pattern.clone());
        if let Some(s) = finish(
            segs,
            pattern,
            frames_mode,
            positions_count,
            cfg,
            meter,
            true,
        ) {
            out.push(s);
        }
    }
    // A sync grown into the idle word that usually follows or precedes it: trim the part a
    // fill pattern explains and recount without growing again.
    let fills: Vec<Vec<u8>> = out
        .iter()
        .filter(|s| s.kind == PatternKind::Fill)
        .filter_map(|s| parse_bit_string(&s.bits))
        .collect();
    if !fills.is_empty() {
        for s in &mut out {
            if s.kind == PatternKind::Fill {
                continue;
            }
            let Some(bits) = parse_bit_string(&s.bits) else {
                continue;
            };
            // Codewords of one code share long runs (POCSAG's inverted sync ends in 20 bits of a
            // rotated idle word), so a trim is kept only when it is found clearly more often and
            // still carries evidence.
            let before = s.occurrences + s.inverted_occurrences;
            if let Some(t) = trim_fill(&bits, &fills)
                && t.len() >= 16
                && let Some(n) = finish(segs, t, frames_mode, positions_count, cfg, meter, false)
                && (n.occurrences + n.inverted_occurrences) * 4 >= before * 5
                && n.evidence_bits >= s.evidence_bits.min(64.0)
            {
                *s = n;
                s.reasons
                    .push("trimmed where a fill word explains the bits".into());
            }
        }
    }
    rank_syncs(out, cfg)
}

/// The pattern without a prefix or suffix (≥ 16 bits: a chance match is ~2⁻¹⁰) found inside a
/// fill pattern repeated.
fn trim_fill(p: &[u8], fills: &[Vec<u8>]) -> Option<Vec<u8>> {
    let n = p.len();
    for f in fills {
        let mut ff = f.clone();
        ff.extend_from_slice(f);
        let inv: Vec<u8> = ff.iter().map(|b| 1 - b).collect();
        let hit = |s: &[u8]| contains(&ff, s) || contains(&inv, s);
        for k in (16..=n.saturating_sub(8)).rev() {
            if hit(&p[n - k..]) {
                return Some(p[..n - k].to_vec());
            }
            if hit(&p[..k]) {
                return Some(p[k..].to_vec());
            }
        }
    }
    None
}

/// Work of one [`extend`]: a vote over every occurrence per grown bit (and the failing votes).
fn extend_ops(occurrences: usize, grown_len: usize, core_len: usize) -> u64 {
    (occurrences as u64 + 1) * (grown_len.saturating_sub(core_len) as u64 + 2) * 3
}

/// Chance that the pattern (`lp` bits, either polarity, ≤ `e` errors) matches at one position
/// of random bits.
fn chance_per_position(lp: usize, e: usize) -> f64 {
    let mut c = 1.0f64; // C(lp, k)
    let mut sum = 0.0;
    for k in 0..=e.min(lp) {
        sum += c;
        c = c * (lp - k) as f64 / (k + 1) as f64;
    }
    (2.0 * sum * 2f64.powi(-(lp as i32))).min(1.0)
}

/// −log2 of the Chernoff bound on seeing ≥ `k` occurrences by chance when `lambda` are expected:
/// Poisson in a stream (`frames = None`), binomial over `frames` otherwise.
fn chernoff_bits(k: f64, lambda: f64, frames: Option<f64>) -> f64 {
    if k <= lambda || lambda <= 0.0 {
        return 0.0;
    }
    let nats = match frames {
        None => k * (k / lambda).ln() - (k - lambda),
        Some(n) => {
            let p = (lambda / n).clamp(1e-300, 1.0);
            let x = (k / n).min(1.0);
            let rest = if x < 1.0 {
                (1.0 - x) * ((1.0 - x) / (1.0 - p).max(1e-300)).ln()
            } else {
                0.0
            };
            n * (x * (x / p).ln() + rest)
        }
    };
    (nats / std::f64::consts::LN_2).max(0.0)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    needle.len() <= hay.len() && hay.windows(needle.len()).any(|w| w == needle)
}

fn find_exact(
    segs: &[&[u8]],
    key: u64,
    l: usize,
    lmask: u64,
    frames_mode: bool,
    meter: &mut Meter,
) -> Vec<(usize, usize, bool)> {
    let inv_key = !key & lmask;
    let mut occ = Vec::new();
    for (si, s) in segs.iter().enumerate() {
        meter.charge(s.len() as u64);
        let mut w = 0u64;
        for (i, &b) in s.iter().enumerate() {
            w = ((w << 1) | u64::from(b & 1)) & lmask;
            if i + 1 < l {
                continue;
            }
            if w == key || w == inv_key {
                occ.push((si, i + 1 - l, w != key));
                if frames_mode {
                    break;
                }
            }
        }
        if occ.len() >= 20_000 {
            break;
        }
    }
    occ
}

/// Grows the seed right then left by consensus; returns the pattern (seed polarity) and how
/// far it grew to the left.
fn extend(
    segs: &[&[u8]],
    occ: &[(usize, usize, bool)],
    core: Vec<u8>,
    cfg: &SyncConfig,
) -> Vec<u8> {
    let l = core.len();
    let need = (cfg.consensus * occ.len() as f64).ceil() as usize;
    let max = cfg.max_sync_bits.clamp(l, 64);
    let vote = |off: &dyn Fn(usize) -> Option<usize>| -> Option<u8> {
        let (mut ones, mut avail) = (0, 0);
        for &(s, p, inv) in occ {
            if let Some(j) = off(p)
                && j < segs[s].len()
            {
                avail += 1;
                ones += usize::from((segs[s][j] & 1) ^ u8::from(inv));
            }
        }
        let agree = ones.max(avail - ones);
        (agree >= need).then_some(u8::from(ones * 2 >= avail))
    };
    let mut right = Vec::new();
    while l + right.len() < max {
        let k = right.len();
        match vote(&|p| Some(p + l + k)) {
            Some(b) => right.push(b),
            None => break,
        }
    }
    let mut left = Vec::new();
    while l + right.len() + left.len() < max {
        let k = left.len();
        match vote(&|p| p.checked_sub(k + 1)) {
            Some(b) => left.push(b),
            None => break,
        }
    }
    let mut pattern: Vec<u8> = left.into_iter().rev().collect();
    pattern.extend(core);
    pattern.extend(right);
    pattern
}

/// Recounts with tolerance, cuts to the bits the occurrences agree on, re-extends over every
/// occurrence (`extend_again`), strips a preamble, snaps to bytes and builds the suggestion.
fn finish(
    segs: &[&[u8]],
    pattern: Vec<u8>,
    frames_mode: bool,
    positions_count: usize,
    cfg: &SyncConfig,
    meter: &mut Meter,
    extend_again: bool,
) -> Option<SyncSuggestion> {
    let mut reasons = Vec::new();
    let errors_for = |len: usize| cfg.max_errors.unwrap_or(len / 16).min(len / 4);
    if !(8..=64).contains(&pattern.len()) {
        return None;
    }
    let seg_bits = segs.iter().map(|s| s.len() as u64).sum::<u64>();
    meter.charge(seg_bits * 2);
    let positions = recount(segs, &pattern, errors_for(pattern.len()), frames_mode);
    if positions.len() < cfg.min_occurrences {
        return None;
    }
    // A pattern grown from a seed that occurs in only some frames, matched everywhere through
    // the error tolerance, disagrees at the same bit positions in many occurrences (unlike
    // noise): keep the longest run of agreeing bits, then grow it again over all occurrences.
    meter.charge((pattern.len() * positions.len()) as u64 * 2 + 1);
    let bad: Vec<bool> = (0..pattern.len())
        .map(|k| {
            let dis = positions
                .iter()
                .filter(|(s, p, inv)| (segs[*s][p + k] & 1) ^ u8::from(*inv) != pattern[k])
                .count();
            dis as f64 > (1.0 - cfg.consensus) * positions.len() as f64
        })
        .collect();
    let (mut best, mut cur) = ((0usize, 0usize), 0usize);
    for (k, is_bad) in bad.iter().copied().chain(std::iter::once(true)).enumerate() {
        if is_bad {
            if cur > best.1 - best.0 {
                best = (k - cur, k);
            }
            cur = 0;
        } else {
            cur += 1;
        }
    }
    if best.1 - best.0 < 8 {
        return None;
    }
    if best.1 - best.0 < pattern.len() {
        reasons.push(format!(
            "cut to the {} bits every occurrence agrees on",
            best.1 - best.0
        ));
    }
    let core = pattern[best.0..best.1].to_vec();
    let mut pattern = if extend_again {
        let occ: Vec<(usize, usize, bool)> = positions
            .iter()
            .map(|&(s, p, inv)| (s, p + best.0, inv))
            .collect();
        let core_len = core.len();
        let grown = extend(segs, &occ, core, cfg);
        meter.charge(extend_ops(occ.len(), grown.len(), core_len));
        grown
    } else {
        core
    };
    // Leading alternating or constant run.
    let alt = 1 + pattern.windows(2).take_while(|w| w[0] != w[1]).count();
    let con = 1 + pattern.windows(2).take_while(|w| w[0] == w[1]).count();
    let run = if alt >= 8 {
        alt
    } else if con >= 8 {
        con
    } else {
        0
    };
    let mut stripped = 0;
    if run > 0 && pattern.len() >= run + 8 {
        stripped = run;
        let rest = pattern.len() - run;
        let rem = rest % 8;
        if rem != 0 && 8 - rem <= 3 {
            stripped -= 8 - rem;
        } else if rem != 0 && rem <= 4 {
            pattern.truncate(pattern.len() - rem);
            reasons.push(format!(
                "{rem} more bit(s) agree after the word; they may start the next field"
            ));
        }
        pattern.drain(..stripped);
        reasons.push(format!(
            "{stripped}-bit preamble run stripped from the front"
        ));
    }
    let lp = pattern.len();
    if !(8..=64).contains(&lp) {
        return None;
    }
    let e = errors_for(lp);
    meter.charge(seg_bits * 2);
    let mut positions = recount(segs, &pattern, e, frames_mode);
    if positions.len() < cfg.min_occurrences {
        return None;
    }
    let inverted = positions.iter().filter(|p| p.2).count();
    if inverted * 2 > positions.len() {
        for b in &mut pattern {
            *b = 1 - *b;
        }
        for p in &mut positions {
            p.2 = !p.2;
        }
    }
    let inverted = positions.iter().filter(|p| p.2).count();
    let occurrences = positions.len() - inverted;
    let total = positions.len();
    let pre = positions
        .iter()
        .filter(|(s, p, _)| alternating_before(segs[*s], *p) >= 16)
        .count() as f64
        / total as f64;
    let (modal, regularity, back_to_back, frames_with, modal_offset) = if frames_mode {
        let mut offs: HashMap<usize, usize> = HashMap::new();
        for (_, p, _) in &positions {
            *offs.entry(*p).or_default() += 1;
        }
        let mo = offs
            .into_iter()
            .max_by_key(|(o, c)| (*c, usize::MAX - o))
            .map(|x| x.0);
        (
            None,
            total as f64 / segs.len() as f64,
            false,
            Some(total),
            mo,
        )
    } else {
        let intervals: Vec<usize> = positions.windows(2).map(|w| w[1].1 - w[0].1).collect();
        if intervals.is_empty() {
            (None, 0.0, false, None, None)
        } else {
            let mut hist: HashMap<usize, usize> = HashMap::new();
            for &i in &intervals {
                *hist.entry(i).or_default() += 1;
            }
            let (d, c) = hist
                .iter()
                .max_by_key(|(d, c)| (**c, usize::MAX - **d))
                .map(|(d, c)| (*d, *c))
                .expect("non-empty");
            // Tiles the stream: spacing at (or just above) its own length.
            let b2b = intervals
                .iter()
                .filter(|&&i| i >= lp && i <= lp + lp / 4)
                .count()
                * 4
                >= intervals.len();
            (Some(d), c as f64 / intervals.len() as f64, b2b, None, None)
        }
    };
    // Chance model: occurrences expected in random bits of the same lengths (frames mode: frames
    // holding one). Evidence and significance count only what exceeds it.
    let q = chance_per_position(lp, e);
    let lambda: f64 = if frames_mode {
        segs.iter()
            .map(|s| 1.0 - (1.0 - q).powf((s.len() + 1).saturating_sub(lp) as f64))
            .sum()
    } else {
        segs.iter()
            .map(|s| (s.len() + 1).saturating_sub(lp) as f64)
            .sum::<f64>()
            * q
    };
    let excess = (total as f64 - lambda).max(0.0);
    let frames_n = frames_mode.then_some(segs.len() as f64);
    // Look-elsewhere: any of the 2^lp patterns of this width could have been the one.
    let significance = (chernoff_bits(total as f64, lambda, frames_n) - lp as f64).max(0.0);
    let in_most_frames = frames_mode && excess >= 0.5 * (segs.len() as f64 - lambda).max(1.0);
    let kind = if back_to_back {
        PatternKind::Fill
    } else if (!frames_mode && (regularity >= 0.5 || pre >= 0.2)) || in_most_frames {
        PatternKind::Sync
    } else {
        PatternKind::Repeat
    };
    let per = lp as f64 - (positions_count.max(2) as f64).log2() - e as f64 * (lp as f64).log2();
    let evidence = (excess * per).max(0.0);
    if lambda >= 0.1 * total as f64 {
        reasons.push(format!(
            "{lambda:.1} of the {total} occurrences are expected by chance"
        ));
    }
    match kind {
        PatternKind::Fill => reasons.push("repeats back to back: an idle/fill word".into()),
        PatternKind::Sync if frames_mode => reasons.push(format!(
            "in {total}/{} frames, most often at bit {}",
            segs.len(),
            modal_offset.unwrap_or(0)
        )),
        PatternKind::Sync => reasons.push(format!(
            "{total} occurrences, {:.0}% at {} bits spacing, {:.0}% after a preamble",
            regularity * 100.0,
            modal.unwrap_or(0),
            pre * 100.0
        )),
        PatternKind::Repeat => reasons.push(format!("{total} occurrences without a frame rhythm")),
    }
    if inverted > 0 {
        reasons.push(format!(
            "{inverted} occurrence(s) inverted (polarity flips)"
        ));
    }
    let value = bits_value(&pattern);
    let complement = !value & if lp == 64 { u64::MAX } else { (1 << lp) - 1 };
    let frame_bits = (kind == PatternKind::Sync && !frames_mode)
        .then_some(modal)
        .flatten()
        .filter(|d| *d > lp)
        .map(|d| d - lp);
    Some(SyncSuggestion {
        bits: bit_string(&pattern),
        bit_len: lp,
        hex: hex_bits(value, lp),
        hex_lsb_first: hex(&pattern, BitOrder::LsbFirst).map(|h| format!("0x{h}")),
        complement_hex: hex_bits(complement, lp),
        kind,
        occurrences,
        inverted_occurrences: inverted,
        max_errors: e,
        modal_interval_bits: modal,
        regularity,
        preamble_fraction: pre,
        preamble_bits: stripped,
        frames_with,
        modal_offset,
        evidence_bits: evidence,
        significance_bits: significance,
        score: 1.0 - (-significance / 32.0).exp(),
        relative_score: 0.0,
        reasons,
        fragment: BlockFragment {
            block: "sync_search".into(),
            params: FragmentParams::SyncWord(SyncWordParams {
                mode: "sync-word".into(),
                sync_word: hex_bits(value, lp),
                sync_bits: lp,
                max_errors: e,
                frame_bits,
                include_sync: false,
            }),
        },
        positions,
    })
}

/// Occurrences of `pattern` (≤ 64 bits) with ≤ `e` errors, either polarity, non-overlapping.
fn recount(
    segs: &[&[u8]],
    pattern: &[u8],
    e: usize,
    first_only: bool,
) -> Vec<(usize, usize, bool)> {
    let lp = pattern.len();
    if lp == 0 || lp > 64 {
        return Vec::new();
    }
    let pm = if lp == 64 { u64::MAX } else { (1u64 << lp) - 1 };
    let pv = bits_value(pattern);
    let mut out = Vec::new();
    for (si, s) in segs.iter().enumerate() {
        let mut w = 0u64;
        let mut next = 0;
        for (i, &b) in s.iter().enumerate() {
            w = ((w << 1) | u64::from(b & 1)) & pm;
            if i + 1 < lp || i + 1 - lp < next {
                continue;
            }
            let d = (w ^ pv).count_ones() as usize;
            let hit = if d <= e {
                Some(false)
            } else if lp - d <= e {
                Some(true)
            } else {
                None
            };
            if let Some(inv) = hit {
                out.push((si, i + 1 - lp, inv));
                next = i + 1;
                if first_only {
                    break;
                }
            }
        }
    }
    out
}

fn rank_syncs(mut out: Vec<SyncSuggestion>, cfg: &SyncConfig) -> Vec<SyncSuggestion> {
    let key = |s: &SyncSuggestion| {
        let kind = match s.kind {
            PatternKind::Sync => 1.0,
            PatternKind::Repeat => 0.5,
            PatternKind::Fill => 0.1,
        };
        s.evidence_bits * kind * (0.25 + 0.75 * s.regularity.min(1.0)) * (1.0 + s.preamble_fraction)
    };
    // Short patterns that recur by chance carry no evidence.
    out.retain(|s| s.evidence_bits >= 16.0);
    out.sort_by(|a, b| a.kind.cmp(&b.kind).then(key(b).total_cmp(&key(a))));
    let mut kept: Vec<SyncSuggestion> = Vec::new();
    for s in out {
        let bits = s.bits.as_bytes();
        let comp: Vec<u8> = bits
            .iter()
            .map(|b| if *b == b'1' { b'0' } else { b'1' })
            .collect();
        let total = s.occurrences + s.inverted_occurrences;
        let dup = kept.iter().any(|k| {
            let kb = k.bits.as_bytes();
            let kt = k.occurrences + k.inverted_occurrences;
            (contains(kb, bits) || contains(kb, &comp)) && total * 4 <= kt * 5
        });
        if !dup {
            kept.push(s);
        }
    }
    kept.truncate(cfg.max_candidates);
    let best = kept.iter().map(key).fold(0.0, f64::max);
    for s in &mut kept {
        s.relative_score = if best > 0.0 { key(s) / best } else { 0.0 };
    }
    kept
}

/// Runs of sync occurrences spaced by the modal interval → `[start, last + interval)`.
fn anchor_segments(s: &SyncSuggestion, n: usize) -> Vec<(usize, usize)> {
    let Some(d) = s.modal_interval_bits else {
        return vec![(0, n)];
    };
    let pos: Vec<usize> = s.positions.iter().map(|p| p.1).collect();
    let mut segs = Vec::new();
    let mut i = 0;
    while i < pos.len() {
        let mut j = i;
        while j + 1 < pos.len() && pos[j + 1] - pos[j] == d {
            j += 1;
        }
        segs.push((pos[i], (pos[j] + d).min(n)));
        i = j + 1;
    }
    segs
}

fn block_row(bits: &[u8]) -> u128 {
    bits.iter()
        .fold(0u128, |acc, &b| (acc << 1) | u128::from(b & 1))
}

fn linear_blocks(
    bits: &[u8],
    prefix: &[u32],
    segments: &[(usize, usize)],
    cfg: &SyncConfig,
    meter: &mut Meter,
) -> Vec<PeriodSuggestion> {
    let lo = cfg.min_block_bits.max(4);
    let hi = cfg.max_block_bits.min(128);
    if lo > hi {
        return Vec::new();
    }
    let log2_tests = ((lo..=hi).sum::<usize>().max(1) as f64).log2();
    let mut cands: Vec<(usize, usize, usize, usize, usize, f64)> = Vec::new(); // n, abs offset, rank, cc, rows, evidence
    'outer: for nb in lo..=hi {
        let mut best: Option<(usize, usize, usize, usize, usize, f64)> = None;
        for o in 0..nb {
            if !meter.ok() {
                meter.skip(format!(
                    "linear-block hunt stopped at the work cap (block {nb})"
                ));
                break 'outer;
            }
            meter.hypothesis();
            let cap = nb + 32;
            let mut rows: HashSet<u128> = HashSet::new();
            let mut ops = 0u64;
            'segs: for &(a, b) in segments {
                let mut p = a + o;
                while p + nb <= b {
                    if prefix[p + nb] == prefix[p] {
                        rows.insert(block_row(&bits[p..p + nb]));
                        // Pack bit by bit, hash-set insert.
                        ops += nb as u64 + 24;
                        if rows.len() >= cap {
                            break 'segs;
                        }
                    }
                    p += nb;
                }
            }
            meter.charge(ops);
            if rows.len() < nb + 8 {
                continue;
            }
            let mut basis = [0u128; 128];
            let mut rank = 0;
            let (mut all_and, mut all_or) = (u128::MAX, 0u128);
            for &r in &rows {
                all_and &= r;
                all_or |= r;
                let mut x = r;
                while x != 0 {
                    let lead = 127 - x.leading_zeros() as usize;
                    if basis[lead] == 0 {
                        basis[lead] = x;
                        rank += 1;
                        break;
                    }
                    x ^= basis[lead];
                }
            }
            meter.charge((rows.len() * rank) as u64 / 2 + 1);
            let d = nb - rank;
            if d == 0 {
                continue;
            }
            let colmask = if nb == 128 {
                u128::MAX
            } else {
                (1u128 << nb) - 1
            };
            let cc = (!(all_and ^ all_or) & colmask).count_ones() as usize;
            let evidence = (d * (rows.len() - nb)) as f64 - log2_tests;
            if evidence < 12.0 {
                continue;
            }
            let abs = (segments[0].0 + o) % nb;
            let better = best.is_none_or(|b| {
                let (bd, bcc, be) = (b.0 - b.2, b.3, b.5);
                (d as i64 - cc as i64, evidence) > (bd as i64 - bcc as i64, be)
            });
            if better {
                best = Some((nb, abs, rank, cc, rows.len(), evidence));
            }
        }
        if let Some(b) = best {
            cands.push(b);
        }
    }
    let mut out: Vec<PeriodSuggestion> = Vec::new();
    for &(nb, abs, rank, cc, rows, evidence) in &cands {
        let d = nb - rank;
        let harmonic_of = cands
            .iter()
            .filter(|f| {
                f.0 < nb && nb % f.0 == 0 && (f.0 - f.2) > f.3 && (abs + nb - f.1 % nb) % f.0 == 0
            })
            .map(|f| f.0)
            .min();
        let mut reasons = vec![format!(
            "{rows} distinct {nb}-bit blocks at offset {abs} have rank {rank} (deficiency {d}, {cc} constant column(s))"
        )];
        if d > cc {
            reasons
                .push("rank deficiency beyond constant columns: codewords of a linear code".into());
        }
        if let Some(f) = harmonic_of {
            reasons.push(format!("multiple of the {f}-bit block"));
        }
        out.push(PeriodSuggestion {
            period_bits: nb,
            offset_bits: Some(abs),
            method: PeriodMethod::LinearBlock,
            agreement: None,
            z: None,
            rank: Some(rank),
            deficiency: Some(d),
            constant_columns: Some(cc),
            rows: Some(rows),
            harmonic_of,
            evidence,
            score: 0.0,
            reasons,
        });
    }
    out.sort_by(|a, b| {
        let ka = (a.harmonic_of.is_some(), a.deficiency <= a.constant_columns);
        let kb = (b.harmonic_of.is_some(), b.deficiency <= b.constant_columns);
        ka.cmp(&kb).then(b.evidence.total_cmp(&a.evidence))
    });
    let best = out.iter().map(|p| p.evidence).fold(0.0, f64::max);
    for p in &mut out {
        p.score = if best > 0.0 { p.evidence / best } else { 0.0 };
    }
    out
}

fn pack_words(bits: impl Iterator<Item = bool>, n: usize) -> Vec<u64> {
    let mut v = vec![0u64; n.div_ceil(64) + 1];
    for (i, b) in bits.enumerate() {
        if b {
            v[i / 64] |= 1 << (i % 64);
        }
    }
    v
}

fn word_at(v: &[u64], i: usize) -> u64 {
    let (w, b) = (i / 64, i % 64);
    let lo = v[w] >> b;
    if b == 0 {
        lo
    } else {
        lo | (v.get(w + 1).copied().unwrap_or(0) << (64 - b))
    }
}

fn autocorrelation(
    bits: &[u8],
    degenerate: &[bool],
    cfg: &SyncConfig,
    meter: &mut Meter,
) -> Vec<PeriodSuggestion> {
    let n = bits.len();
    if n < 512 {
        return Vec::new();
    }
    let v = pack_words(bits.iter().map(|b| b & 1 == 1), n);
    let m = pack_words(degenerate.iter().map(|d| !d), n);
    let max_lag = cfg.max_lag.min(n / 2);
    let min_lag = cfg.min_lag.max(1);
    if min_lag > max_lag {
        return Vec::new();
    }
    let mut z = vec![0.0f64; max_lag + 2];
    let mut agree = vec![0.0f64; max_lag + 2];
    for lag in min_lag..=max_lag {
        if !meter.charge(((n - lag) / 64) as u64 * 3 + 1) {
            meter.skip(format!(
                "autocorrelation stopped at the work cap (lag {lag})"
            ));
            break;
        }
        let (mut cnt, mut dis) = (0u64, 0u64);
        let mut i = 0;
        while i + 64 <= n - lag {
            let ok = word_at(&m, i) & word_at(&m, i + lag);
            let x = word_at(&v, i) ^ word_at(&v, i + lag);
            cnt += u64::from(ok.count_ones());
            dis += u64::from((x & ok).count_ones());
            i += 64;
        }
        if cnt >= 256 {
            z[lag] = (cnt as f64 - 2.0 * dis as f64) / (cnt as f64).sqrt();
            agree[lag] = 1.0 - dis as f64 / cnt as f64;
        }
    }
    let mut peaks: Vec<usize> = (min_lag..=max_lag)
        .filter(|&l| z[l] >= 8.0 && z[l] >= z[l - 1] && z[l] >= z[l + 1])
        .collect();
    peaks.sort_by(|a, b| z[*b].total_cmp(&z[*a]));
    peaks.truncate(cfg.max_candidates * 2);
    let zmax = peaks.first().map_or(1.0, |&l| z[l]);
    let mut out: Vec<PeriodSuggestion> = peaks
        .iter()
        .map(|&lag| {
            let harmonic_of = peaks
                .iter()
                .filter(|&&l0| l0 < lag && lag % l0 == 0 && z[l0] >= 0.5 * z[lag])
                .min()
                .copied();
            let mut reasons = vec![format!(
                "bits agree {:.1}% with themselves {lag} bits later ({:.0} σ)",
                agree[lag] * 100.0,
                z[lag]
            )];
            if let Some(h) = harmonic_of {
                reasons.push(format!("multiple of the {h}-bit period"));
            }
            PeriodSuggestion {
                period_bits: lag,
                offset_bits: None,
                method: PeriodMethod::Autocorrelation,
                agreement: Some(agree[lag]),
                z: Some(z[lag]),
                rank: None,
                deficiency: None,
                constant_columns: None,
                rows: None,
                harmonic_of,
                evidence: z[lag],
                score: z[lag] / zmax,
                reasons,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        a.harmonic_of
            .is_some()
            .cmp(&b.harmonic_of.is_some())
            .then(b.evidence.total_cmp(&a.evidence))
    });
    out
}

fn cut_blocks<'a>(
    bits: &'a [u8],
    prefix: &[u32],
    segments: &[(usize, usize)],
    p: &PeriodSuggestion,
    max_blocks: usize,
) -> Vec<(usize, &'a [u8])> {
    let nb = p.period_bits;
    let abs = p.offset_bits.unwrap_or(0);
    let mut out = Vec::new();
    for &(a, b) in segments {
        // First block start ≥ a with the found phase (relative to the first segment).
        let o = (abs + nb - (a % nb)) % nb;
        let o = if segments.len() > 1 {
            (abs + nb - segments[0].0 % nb) % nb
        } else {
            o
        };
        let mut j = 0;
        let mut q = a + o;
        while q + nb <= b && out.len() < max_blocks {
            if prefix[q + nb] == prefix[q] {
                out.push((j, &bits[q..q + nb]));
            }
            j += 1;
            q += nb;
        }
    }
    out
}

fn offset_words_fragment(c: &CodeSuggestion, block_bits: usize) -> BlockFragment {
    let w = usize::from(c.width);
    let names: Vec<String> = (0..c.class_constants.len())
        .map(|i| format!("o{i}"))
        .collect();
    BlockFragment {
        block: "sync_search".into(),
        params: FragmentParams::OffsetWords(OffsetWordsParams {
            mode: "offset-words".into(),
            block_bits,
            check_bits: w,
            poly: hex_bits(c.generator, w + 1),
            offsets: names
                .iter()
                .zip(&c.class_constants)
                .map(|(n, k)| OffsetWord {
                    name: n.clone(),
                    word: hex_bits(*k, w),
                })
                .collect(),
            sequence: names.iter().map(|n| vec![n.clone()]).collect(),
        }),
    }
}
