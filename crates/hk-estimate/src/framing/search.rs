//! CRC search over a burst corpus aligned on the sync word.
//!
//! # Fixed-position frames: differential search
//! For a CRC with linear core `C` (width, poly, reflection) over `n` covered bytes,
//! `crc(d) = C(init, 0ⁿ) ⊕ xorout ⊕ C(0, d)`. Complementing the bits (FSK polarity) or XORing a
//! fixed whitening mask adds another data-independent term. So for the right core, span and
//! position, `K = C(0, d) ⊕ field` is **the same constant in every burst**, whatever init,
//! xorout, polarity and whitening are (the "XOR two messages" technique, docs/04 §7.4, C21).
//! The search tries every catalogue polynomial with and without reflection, both bit orders,
//! both field byte orders, span starts {sync start, sync end, +8, +16 bits} and every CRC
//! position, and counts the bursts sharing the modal `K`. The constant is then **explained** by
//! a standard setting: catalogue init/xorout pairs and {0, all ones}², normal or inverted
//! polarity, no whitening, PN9, or (16/32-bit only) the 7-bit LFSRs. A named catalogue entry is
//! preferred, then normal polarity, then no whitening.
//!
//! # Length-field frames
//! When no fixed position validates: a length field (offset 0/8/16 bits after the sync; u8,
//! u16 BE/LE, 11-bit PHR) sets the CRC position per burst; every catalogue entry is validated
//! exactly with polarity normal/inverted and whitening none / PN9 from the sync end or from
//! after the field, coverage from the field or after it, and adjustments −4..=4 bytes.
//!
//! # Claim thresholds (never claim below them)
//! - validating bursts `k ≥ 3`, covering `≥ 3` distinct messages;
//! - `k / tested ≥ 0.5` (`tested` = every burst with a located sync);
//! - false-alarm bound `H · C(N, k) · 2^(−w·k′) ≤ 10⁻³`, with `H` the hypotheses searched,
//!   `N` the bursts long enough for the hypothesis, and `k′ = k − 1` for the differential search
//!   (the first burst fixes `K`) or `k` for exact validation;
//! - the differential constant explained by a standard setting.

use std::collections::HashSet;

use super::FramingConfig;
use super::bits::{BitOrder, pack};
use super::crc::{CATALOGUE, CrcCore, CrcParams, Endianness, read_field};
use super::model::{
    CrcCandidate, CrcMethod, CrcModel, CrcSpan, FramingReason, LengthFieldKind, LengthFieldModel,
    LengthFieldSource, Polarity,
};
use super::whitening::Whitening;

/// Burst bits from the sync start, in the learned sync's polarity.
pub(crate) struct Corpus<'a> {
    pub frames: &'a [Vec<u8>],
    pub sync_len: usize,
}

/// Search result.
#[derive(Debug, Default)]
pub(crate) struct SearchOutcome {
    pub crc: Option<CrcModel>,
    pub length_field: Option<LengthFieldModel>,
    pub candidate: Option<CrcCandidate>,
    pub hypotheses: u64,
}

fn ln_choose(n: usize, k: usize) -> f64 {
    let k = k.min(n - k.min(n));
    (0..k).map(|i| ((n - i) as f64 / (i + 1) as f64).ln()).sum()
}

/// `H · C(N, k) · 2^(−w·k′)`.
pub(crate) fn false_alarm_bound(h: u64, n: usize, k: usize, width: u8, k_eff: usize) -> f64 {
    let ln = (h.max(1) as f64).ln() + ln_choose(n, k)
        - f64::from(width) * k_eff as f64 * std::f64::consts::LN_2;
    ln.exp()
}

/// Unique (width, poly) of the catalogue, each with and without reflection.
fn cores() -> Vec<CrcCore> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for e in CATALOGUE {
        for refl in [false, true] {
            if seen.insert((e.params.width, e.params.poly, refl)) {
                out.push(CrcCore::new(e.params.width, e.params.poly, refl));
            }
        }
    }
    out
}

/// Catalogue (init, xorout) pairs for the core's width and poly, then {0, ones}².
fn init_xorout_combos(core: &CrcCore) -> Vec<(u32, u32)> {
    let m = core.mask();
    let mut v: Vec<(u32, u32)> = CATALOGUE
        .iter()
        .filter(|e| e.params.width == core.width && e.params.poly == core.poly)
        .map(|e| (e.params.init, e.params.xorout))
        .collect();
    v.extend([(0, 0), (m, 0), (0, m), (m, m)]);
    let mut seen = HashSet::new();
    v.retain(|c| seen.insert(*c));
    v
}

fn rank_of(params: &CrcParams) -> usize {
    CATALOGUE
        .iter()
        .position(|e| e.params == *params)
        .unwrap_or(CATALOGUE.len() + 1)
}

fn natural_endianness(refl: bool) -> Endianness {
    if refl {
        Endianness::Little
    } else {
        Endianness::Big
    }
}

/// Whitening mask bytes for a span starting at `start` bits (relative to the sync end), `total`
/// bits long, whitening starting at `w_offset`.
fn mask_bytes(w: Whitening, start: i32, total: usize, w_offset: usize, order: BitOrder) -> Vec<u8> {
    let end = (start + total as i32).max(0) as usize;
    let seq = w.sequence(end.saturating_sub(w_offset));
    let bits: Vec<u8> = (0..total)
        .map(|i| {
            let pos = start + i as i32;
            if pos < w_offset as i32 {
                0
            } else {
                seq[pos as usize - w_offset]
            }
        })
        .collect();
    pack(&bits, order)
}

#[derive(Clone, Debug)]
struct Explanation {
    params: CrcParams,
    polarity: Polarity,
    whitening: Option<Whitening>,
}

impl Explanation {
    fn key(&self) -> (usize, bool, bool) {
        (
            rank_of(&self.params),
            self.polarity != Polarity::Normal,
            self.whitening.is_some(),
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn explain(
    core: &CrcCore,
    order: BitOrder,
    start: i32,
    n: usize,
    e: Endianness,
    key: u32,
    whitenings: &[Whitening],
) -> Option<Explanation> {
    let wb = usize::from(core.width / 8);
    let mask = core.mask();
    let combos = init_xorout_combos(core);
    let pol_contrib = core.run(0, &vec![0xFF; n]) ^ mask;
    let zeros: Vec<u32> = combos
        .iter()
        .map(|&(init, xorout)| core.run_zeros(core.internal_init(init), n) ^ xorout)
        .collect();
    let mut best: Option<Explanation> = None;
    let options = std::iter::once(None).chain(whitenings.iter().copied().map(Some));
    for w in options {
        let w_contrib = match w {
            None => 0,
            Some(w) => {
                let mb = mask_bytes(w, start, 8 * (n + wb), 0, order);
                core.run(0, &mb[..n]) ^ read_field(&mb[n..], core.width, e)
            }
        };
        for polarity in [Polarity::Normal, Polarity::Inverted] {
            let kt = key
                ^ w_contrib
                ^ if polarity == Polarity::Inverted {
                    pol_contrib
                } else {
                    0
                };
            for (ci, &(init, xorout)) in combos.iter().enumerate() {
                if zeros[ci] == kt {
                    let cand = Explanation {
                        params: core.params(init, xorout),
                        polarity,
                        whitening: w,
                    };
                    if best.as_ref().is_none_or(|b| cand.key() < b.key()) {
                        best = Some(cand);
                    }
                }
            }
        }
        if best.is_some() && w.is_none() {
            break;
        }
    }
    best
}

struct Hit {
    k: usize,
    n: usize,
    order: BitOrder,
    start: i32,
    core: usize,
    p: usize,
    e: Endianness,
    key: u32,
    distinct: usize,
}

/// Fixed-position differential search. See the [module docs](self).
pub(crate) fn search_fixed(c: &Corpus<'_>, cfg: &FramingConfig) -> SearchOutcome {
    let cores = cores();
    let tested = c.frames.len();
    let starts = [-(c.sync_len as i32), 0, 8, 16];
    let mut hits: Vec<Hit> = Vec::new();
    let mut h: u64 = 0;
    for order in BitOrder::ALL {
        for &s in &starts {
            let from = (c.sync_len as i32 + s) as usize;
            let bytes: Vec<Vec<u8>> = c
                .frames
                .iter()
                .map(|f| {
                    if from >= f.len() {
                        Vec::new()
                    } else {
                        let mut v = pack(&f[from..], order);
                        v.truncate(cfg.max_frame_bytes);
                        v
                    }
                })
                .collect();
            let max_len = bytes.iter().map(Vec::len).max().unwrap_or(0);
            for (ci, core) in cores.iter().enumerate() {
                let wb = usize::from(core.width / 8);
                let regs: Vec<Vec<u32>> = bytes
                    .iter()
                    .map(|b| {
                        let mut r = Vec::with_capacity(b.len() + 1);
                        let mut reg = 0;
                        r.push(0);
                        for &x in b {
                            reg = core.update(reg, x);
                            r.push(reg);
                        }
                        r
                    })
                    .collect();
                let ends: &[Endianness] = if wb == 1 {
                    &[Endianness::Big]
                } else {
                    &[Endianness::Big, Endianness::Little]
                };
                let mut keys: Vec<(u32, usize)> = Vec::with_capacity(bytes.len());
                for p in 1..=max_len.saturating_sub(wb) {
                    for &e in ends {
                        h += 1;
                        keys.clear();
                        for (b, by) in bytes.iter().enumerate() {
                            if by.len() >= p + wb {
                                keys.push((regs[b][p] ^ read_field(&by[p..], core.width, e), b));
                            }
                        }
                        if keys.len() < cfg.crc_min_valid {
                            continue;
                        }
                        keys.sort_unstable();
                        let (mut best_key, mut best_k, mut run_k) = (keys[0].0, 0, 0);
                        for i in 0..keys.len() {
                            run_k = if i > 0 && keys[i].0 == keys[i - 1].0 {
                                run_k + 1
                            } else {
                                1
                            };
                            if run_k > best_k {
                                best_k = run_k;
                                best_key = keys[i].0;
                            }
                        }
                        if best_k < cfg.crc_min_valid {
                            continue;
                        }
                        let distinct = keys
                            .iter()
                            .filter(|x| x.0 == best_key)
                            .map(|x| &bytes[x.1][..p])
                            .collect::<HashSet<_>>()
                            .len();
                        hits.push(Hit {
                            k: best_k,
                            n: keys.len(),
                            order,
                            start: s,
                            core: ci,
                            p,
                            e,
                            key: best_key,
                            distinct,
                        });
                    }
                }
            }
        }
    }
    let fa = |hit: &Hit| {
        false_alarm_bound(
            h,
            hit.n,
            hit.k,
            cores[hit.core].width,
            hit.k.saturating_sub(1),
        )
    };
    // Hits over constant regions (sync, fixed header) validate trivially with one message:
    // rank the varied ones first so they are not truncated away.
    hits.sort_by(|a, b| {
        (a.distinct < cfg.crc_min_distinct)
            .cmp(&(b.distinct < cfg.crc_min_distinct))
            .then(b.k.cmp(&a.k))
            .then(fa(a).total_cmp(&fa(b)))
    });
    hits.truncate(64);
    let pn9 = Whitening::PN9.to_vec();
    let all_w: Vec<Whitening> = Whitening::all();
    type Ranked = ((usize, usize, bool, bool, bool, bool, bool), CrcModel);
    let mut claims: Vec<Ranked> = Vec::new();
    let mut candidate: Option<CrcCandidate> = None;
    for (rank, hit) in hits.iter().enumerate() {
        let core = &cores[hit.core];
        let bound = fa(hit);
        let ratio = hit.k as f64 / tested.max(1) as f64;
        let stat_ok = bound <= cfg.crc_max_false_alarm
            && ratio >= cfg.crc_min_validate_ratio
            && hit.distinct >= cfg.crc_min_distinct;
        let explanation = if stat_ok {
            explain(core, hit.order, hit.start, hit.p, hit.e, hit.key, &pn9).or_else(|| {
                (core.width >= 16 && rank < 8)
                    .then(|| explain(core, hit.order, hit.start, hit.p, hit.e, hit.key, &all_w))
                    .flatten()
            })
        } else {
            None
        };
        match explanation {
            Some(x) if stat_ok => {
                let model = CrcModel {
                    algorithm: x.params.name(),
                    catalogue: x.params.catalogue_entry().is_some(),
                    params: x.params,
                    span: CrcSpan {
                        start_bits: hit.start,
                        covered_bits: Some(8 * hit.p),
                        crc_offset_bits: Some(hit.start + 8 * hit.p as i32),
                        from_length_field: false,
                    },
                    endianness: hit.e,
                    bit_order: hit.order,
                    polarity: x.polarity,
                    whitening: x.whitening,
                    whitening_offset_bits: 0,
                    validated: hit.k,
                    tested,
                    validate_ratio: ratio,
                    distinct_messages: hit.distinct,
                    false_alarm_bound: bound,
                    hypotheses: h,
                    method: CrcMethod::FixedPositionDifferential,
                };
                let key = (
                    usize::MAX - hit.k,
                    rank_of(&x.params),
                    x.polarity != Polarity::Normal,
                    x.whitening.is_some(),
                    hit.order != BitOrder::MsbFirst,
                    hit.e != natural_endianness(core.refl),
                    hit.start != 0,
                );
                claims.push((key, model));
            }
            _ => {
                if candidate.is_none() {
                    let reason = if hit.distinct < cfg.crc_min_distinct {
                        FramingReason::InsufficientMessageVariety
                    } else if bound <= cfg.crc_max_false_alarm
                        && ratio >= cfg.crc_min_validate_ratio
                    {
                        FramingReason::CrcInitUnresolved
                    } else {
                        FramingReason::CrcBelowThreshold
                    };
                    candidate = Some(CrcCandidate {
                        description: format!(
                            "CRC-{} core poly=0x{:X} refl={} {:?} start {:+} bits, crc at {:+} bits, {:?}",
                            core.width,
                            core.poly,
                            core.refl,
                            hit.order,
                            hit.start,
                            hit.start + 8 * hit.p as i32,
                            hit.e
                        ),
                        validated: hit.k,
                        tested,
                        distinct_messages: hit.distinct,
                        false_alarm_bound: bound,
                        reason,
                    });
                }
            }
        }
    }
    claims.sort_by(|a, b| a.0.cmp(&b.0));
    SearchOutcome {
        crc: claims.into_iter().next().map(|(_, m)| m),
        length_field: None,
        candidate,
        hypotheses: h,
    }
}

/// Burst bits after the sync end with polarity and whitening applied.
pub(crate) fn corrected_after_sync(
    frame: &[u8],
    sync_len: usize,
    polarity: Polarity,
    whitening: Option<Whitening>,
    whitening_offset: usize,
) -> Vec<u8> {
    let mut bits: Vec<u8> = frame.get(sync_len..).unwrap_or(&[]).to_vec();
    if polarity == Polarity::Inverted {
        bits.iter_mut().for_each(|b| *b ^= 1);
    }
    if let Some(w) = whitening {
        if whitening_offset < bits.len() {
            w.apply(&mut bits[whitening_offset..]);
        }
    }
    bits
}

/// One burst under a length-field hypothesis: (field value, bytes from the field, bytes after
/// the field); `None` when too short.
type LfBurst = Option<(usize, Vec<u8>, Vec<u8>)>;

/// Length-field-driven exact search. See the [module docs](self).
pub(crate) fn search_length_field(c: &Corpus<'_>, cfg: &FramingConfig) -> SearchOutcome {
    let tested = c.frames.len();
    struct LfHit {
        k: usize,
        n: usize,
        distinct: usize,
        entry: usize,
        adj: i32,
        e: Endianness,
        offset: usize,
        kind: LengthFieldKind,
        order: BitOrder,
        polarity: Polarity,
        whitening: Option<Whitening>,
        w_offset: usize,
        include_field: bool,
    }
    let entry_cores: Vec<CrcCore> = CATALOGUE
        .iter()
        .map(|e| CrcCore::new(e.params.width, e.params.poly, e.params.refin))
        .collect();
    let mut hits: Vec<LfHit> = Vec::new();
    let mut h: u64 = 0;
    for offset in [0usize, 8, 16] {
        for kind in LengthFieldKind::ALL {
            let width = kind.width_bits();
            let w_modes: [(Option<Whitening>, usize); 5] = [
                (None, 0),
                (Some(Whitening::Pn9Serial), 0),
                (Some(Whitening::Pn9Serial), offset + width),
                (Some(Whitening::Pn9Cc1101), 0),
                (Some(Whitening::Pn9Cc1101), offset + width),
            ];
            for order in BitOrder::ALL {
                for polarity in [Polarity::Normal, Polarity::Inverted] {
                    for &(whitening, w_offset) in &w_modes {
                        let per_burst: Vec<LfBurst> = c
                            .frames
                            .iter()
                            .map(|f| {
                                let bits = corrected_after_sync(
                                    f, c.sync_len, polarity, whitening, w_offset,
                                );
                                if bits.len() < offset + width + 8 {
                                    return None;
                                }
                                let fb = pack(&bits[offset..offset + width], order);
                                let l = kind.read(&fb)?;
                                let mut with_field = pack(&bits[offset..], order);
                                with_field.truncate(cfg.max_frame_bytes);
                                let mut after = pack(&bits[offset + width..], order);
                                after.truncate(cfg.max_frame_bytes);
                                Some((l, with_field, after))
                            })
                            .collect();
                        for include_field in [false, true] {
                            for (ei, entry) in CATALOGUE.iter().enumerate() {
                                let core = &entry_cores[ei];
                                let wb = usize::from(entry.params.width / 8);
                                let init = core.internal_init(entry.params.init);
                                let lead = if include_field { width / 8 } else { 0 };
                                // (adj index, endianness) -> members
                                let mut counts = vec![Vec::<usize>::new(); 18];
                                let mut available = [0usize; 18];
                                let n_end = if wb == 1 { 1 } else { 2 };
                                for (b, pb) in per_burst.iter().enumerate() {
                                    let Some((l, with_field, after)) = pb else {
                                        continue;
                                    };
                                    let data = if include_field { with_field } else { after };
                                    let base = *l as i64 + lead as i64;
                                    let lo = (base - 4).max(1);
                                    let hi = (base + 4).min(data.len() as i64 - wb as i64);
                                    if lo > hi {
                                        continue;
                                    }
                                    let mut reg = core.run(init, &data[..lo as usize]);
                                    for n in lo..=hi {
                                        let nu = n as usize;
                                        if n > lo {
                                            reg = core.update(reg, data[nu - 1]);
                                        }
                                        let crc = reg ^ entry.params.xorout;
                                        let ai = (n - base + 4) as usize;
                                        for ei2 in 0..n_end {
                                            let e = if ei2 == 0 {
                                                Endianness::Big
                                            } else {
                                                Endianness::Little
                                            };
                                            available[ai * 2 + ei2] += 1;
                                            if read_field(&data[nu..], entry.params.width, e) == crc
                                            {
                                                counts[ai * 2 + ei2].push(b);
                                            }
                                        }
                                    }
                                }
                                h += if wb == 1 { 9 } else { 18 };
                                for (slot, members) in counts.iter().enumerate() {
                                    if members.len() < cfg.crc_min_valid {
                                        continue;
                                    }
                                    let distinct = members
                                        .iter()
                                        .filter_map(|&b| per_burst[b].as_ref())
                                        .map(|(l, wf, af)| {
                                            let d = if include_field { wf } else { af };
                                            let n = (*l as i64 + (slot / 2) as i64 - 4
                                                + lead as i64)
                                                as usize;
                                            d[..n].to_vec()
                                        })
                                        .collect::<HashSet<_>>()
                                        .len();
                                    hits.push(LfHit {
                                        k: members.len(),
                                        n: available[slot],
                                        distinct,
                                        entry: ei,
                                        adj: (slot / 2) as i32 - 4,
                                        e: if slot % 2 == 0 {
                                            Endianness::Big
                                        } else {
                                            Endianness::Little
                                        },
                                        offset,
                                        kind,
                                        order,
                                        polarity,
                                        whitening,
                                        w_offset,
                                        include_field,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let fa = |x: &LfHit| false_alarm_bound(h, x.n, x.k, CATALOGUE[x.entry].params.width, x.k);
    let key = |x: &LfHit| {
        (
            x.distinct < cfg.crc_min_distinct,
            usize::MAX - x.k,
            x.entry,
            x.polarity != Polarity::Normal,
            x.whitening.is_some(),
            x.order != BitOrder::MsbFirst,
        )
    };
    hits.sort_by_key(key);
    let mut out = SearchOutcome {
        hypotheses: h,
        ..Default::default()
    };
    for x in &hits {
        let bound = fa(x);
        let ratio = x.k as f64 / tested.max(1) as f64;
        let ok = bound <= cfg.crc_max_false_alarm
            && ratio >= cfg.crc_min_validate_ratio
            && x.distinct >= cfg.crc_min_distinct;
        let params = CATALOGUE[x.entry].params;
        if ok {
            let width = x.kind.width_bits();
            let wb = i32::from(params.width / 8);
            out.crc = Some(CrcModel {
                algorithm: params.name(),
                catalogue: true,
                params,
                span: CrcSpan {
                    start_bits: (x.offset + if x.include_field { 0 } else { width }) as i32,
                    covered_bits: None,
                    crc_offset_bits: None,
                    from_length_field: true,
                },
                endianness: x.e,
                bit_order: x.order,
                polarity: x.polarity,
                whitening: x.whitening,
                whitening_offset_bits: x.w_offset,
                validated: x.k,
                tested,
                validate_ratio: ratio,
                distinct_messages: x.distinct,
                false_alarm_bound: bound,
                hypotheses: h,
                method: CrcMethod::LengthField,
            });
            out.length_field = Some(LengthFieldModel {
                offset_bits: x.offset,
                kind: x.kind,
                width_bits: width,
                bit_order: x.order,
                adjust_bytes: x.adj + wb,
                support: x.k,
                tested,
                support_ratio: ratio,
                source: LengthFieldSource::CrcPositions,
            });
            return out;
        }
        if out.candidate.is_none() {
            out.candidate = Some(CrcCandidate {
                description: format!(
                    "{} with {:?} length field at +{} bits ({:?}, adjust {:+}), {:?}",
                    params.name(),
                    x.kind,
                    x.offset,
                    x.order,
                    x.adj,
                    x.e
                ),
                validated: x.k,
                tested,
                distinct_messages: x.distinct,
                false_alarm_bound: bound,
                reason: if x.distinct < cfg.crc_min_distinct {
                    FramingReason::InsufficientMessageVariety
                } else {
                    FramingReason::CrcBelowThreshold
                },
            });
        }
    }
    out
}

/// A frame with polarity and whitening applied, and where its covered data and CRC field lie.
pub(crate) struct CrcLayout {
    /// Bits from the sync start (sync included), polarity and whitening applied.
    pub bits: Vec<u8>,
    /// First covered bit, index into `bits`.
    pub start: usize,
    /// Covered bytes.
    pub covered_bytes: usize,
}

/// Lays out one frame under a claimed CRC. `frame` holds bits from the sync start in the
/// learned polarity. `None` when the frame is too short. For a length-field model the field
/// value `L` gives `L + adjust_bytes − crc_bytes` covered bytes after the field, plus the field
/// itself when the span starts at the field.
pub(crate) fn crc_layout(
    crc: &CrcModel,
    length_field: Option<&LengthFieldModel>,
    frame: &[u8],
    sync_len: usize,
) -> Option<CrcLayout> {
    let wb = usize::from(crc.params.width / 8);
    let mut bits: Vec<u8> = frame.to_vec();
    if crc.polarity == Polarity::Inverted {
        bits.iter_mut().for_each(|b| *b ^= 1);
    }
    if let Some(w) = crc.whitening {
        let from = sync_len + crc.whitening_offset_bits;
        if from < bits.len() {
            w.apply(&mut bits[from..]);
        }
    }
    let start = sync_len as i64 + i64::from(crc.span.start_bits);
    if start < 0 || start as usize >= bits.len() {
        return None;
    }
    let start = start as usize;
    let covered_bytes = match (crc.span.covered_bits, length_field) {
        (Some(c), _) => c / 8,
        (None, Some(lf)) => {
            let fs = sync_len + lf.offset_bits;
            if fs + lf.width_bits > bits.len() {
                return None;
            }
            let l = lf
                .kind
                .read(&pack(&bits[fs..fs + lf.width_bits], lf.bit_order))?
                as i64;
            let include_field = start == fs;
            let n = l + i64::from(lf.adjust_bytes) - wb as i64
                + if include_field {
                    (lf.width_bits / 8) as i64
                } else {
                    0
                };
            if n < 1 {
                return None;
            }
            n as usize
        }
        (None, None) => return None,
    };
    if start + 8 * (covered_bytes + wb) > bits.len() {
        return None;
    }
    Some(CrcLayout {
        bits,
        start,
        covered_bytes,
    })
}

/// Exact per-burst validation of a claimed CRC (see [`crc_layout`]).
pub(crate) fn validate(
    crc: &CrcModel,
    length_field: Option<&LengthFieldModel>,
    frame: &[u8],
    sync_len: usize,
) -> Option<bool> {
    let lay = crc_layout(crc, length_field, frame, sync_len)?;
    let wb = usize::from(crc.params.width / 8);
    let end = lay.start + 8 * (lay.covered_bytes + wb);
    let data = pack(&lay.bits[lay.start..end], crc.bit_order);
    let n = lay.covered_bytes;
    Some(crc.params.compute(&data[..n]) == read_field(&data[n..], crc.params.width, crc.endianness))
}
