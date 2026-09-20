//! Field-boundary suggestions over aligned frames, emitted as a draft field map.
//!
//! Frames are aligned at their first bit (usually the bit after the sync word). Per bit
//! position: coverage, share of ones, binary entropy, constancy `max(p, 1−p)` and the
//! transition rate between consecutive frames. Detectors claim bits first, then the rest is
//! classified per bit and merged into runs:
//!
//! 1. **Check field**: the best single-class code from [`super::codes`] (frame end minus its
//!    tail).
//! 2. **Length field** (variable-length frames): an 8/16-bit field at bit offsets ≤ 64 whose
//!    value × {8, 1} + a constant equals the frame length in ≥ 90% of frames.
//! 3. **Counter**: an LSB that toggles in ≥ 90% of consecutive frames and a run of bits before
//!    it whose value steps by 1 in ≥ `counter_hold` of pairs; the width is the varying span,
//!    rounded up to whole bytes in byte-structured frames.
//! 4. Unclaimed bits: `constant` (constancy ≥ threshold), `high-entropy` (entropy ≥ threshold)
//!    or `mixed`; equal runs merge. In byte-structured frames boundaries between unclaimed
//!    runs snap to byte boundaries.
//!
//! Positions every frame has (before the check field) are classified; a variable-length tail is
//! one payload region whose length comes from the length field when found, else `remainder`
//! (the check field then stays out of the map and is listed as an end-anchored suggestion).

use std::collections::BTreeMap;

use hk_recipe::{Display, Field, FieldMap, FieldType, Length, LengthFrom, LengthKeyword, Unit};
use serde::{Deserialize, Serialize};

use super::codes::{CodeSearchConfig, CodeSuggestion, search_indexed};
use super::sync::align_frames;
use super::{Budget, Meter, WorkReport, bits_value, hex_bits};
use crate::framing::bits::binary_entropy;

/// Settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FieldsConfig {
    /// Constancy at or above which a bit is constant.
    pub constant_threshold: f64,
    /// Entropy (bits) at or above which a bit is high-entropy.
    pub high_entropy: f64,
    /// Share of consecutive frame pairs a counter must step by 1.
    pub counter_hold: f64,
    /// Fewest frames.
    pub min_frames: usize,
    /// Run the code search to locate a check field.
    pub find_codes: bool,
    /// Code search settings.
    pub codes: CodeSearchConfig,
    /// Align frames on a sync word first (inside the work cap); frames without it are dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align: Option<SyncAlign>,
    /// Work cap.
    pub budget: Budget,
}

/// Alignment of [`FieldsConfig::align`]: the bits after the first occurrence of `sync` (either
/// polarity, ≤ `max_errors`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SyncAlign {
    /// Sync word, one `u8` per bit.
    pub sync: Vec<u8>,
    /// Tolerated bit errors.
    pub max_errors: usize,
}

/// Longest frame classified (bits).
pub const MAX_FIELD_BITS: usize = 4096;

/// `1 − e^(−bits/8)`: a 0–1 confidence from significance bits.
fn confidence(bits: f64) -> f64 {
    1.0 - (-bits.max(0.0) / 8.0).exp()
}

impl Default for FieldsConfig {
    fn default() -> Self {
        Self {
            constant_threshold: 0.98,
            high_entropy: 0.9,
            counter_hold: 0.8,
            min_frames: 4,
            find_codes: true,
            codes: CodeSearchConfig {
                max_classes: 1,
                ..CodeSearchConfig::default()
            },
            align: None,
            budget: Budget::default(),
        }
    }
}

/// What a region looks like.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FieldKind {
    /// The same in (nearly) every frame.
    Constant,
    /// Steps by one between consecutive frames.
    Counter,
    /// Tracks the frame length.
    Length,
    /// Close to random.
    HighEntropy,
    /// Neither constant nor random.
    Mixed,
    /// A CRC / code check field.
    Check,
}

/// Per-bit statistics.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BitStat {
    /// Frames long enough to have this bit.
    pub coverage: usize,
    /// Share of ones.
    pub ones: f64,
    /// Binary entropy, bits.
    pub entropy: f64,
    /// Share of consecutive frame pairs where the bit changes.
    pub transition: f64,
}

/// A field-boundary suggestion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FieldSuggestion {
    /// Field name in the draft map.
    pub name: String,
    /// Kind.
    pub kind: FieldKind,
    /// First bit (from the frame start, or from the frame end when `from_end`).
    pub bit_offset: usize,
    /// Width; `None` when it varies per frame.
    pub bit_len: Option<usize>,
    /// Anchored at the frame end (`bit_offset` counts back from the end to the field start).
    pub from_end: bool,
    /// Majority value of a constant field (≤ 64 bits).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_hex: Option<String>,
    /// Length field: frame bits = value × scale + add.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length_scale: Option<u32>,
    /// Length field addend (bits).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length_add: Option<i64>,
    /// Mean per-bit entropy.
    pub mean_entropy: f64,
    /// Mean per-bit constancy.
    pub mean_constancy: f64,
    /// 0–1.
    pub score: f64,
    /// Human-readable reasons.
    pub reasons: Vec<String>,
}

/// [`suggest_fields`] result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FieldsReport {
    /// Frames given.
    pub frames: usize,
    /// Every frame is a whole number of bytes.
    pub byte_structured: bool,
    /// Every frame has the same length.
    pub fixed_length: bool,
    /// Per-bit statistics from the frame start (≤ 4096 positions).
    pub per_bit: Vec<BitStat>,
    /// Regions in frame order.
    pub suggestions: Vec<FieldSuggestion>,
    /// Draft field map (bits unit) the user edits.
    pub field_map: FieldMap,
    /// `FieldMap::validate` errors of the draft (empty when valid).
    pub field_map_errors: Vec<String>,
    /// Code suggestions used to place the check field.
    pub codes: Vec<CodeSuggestion>,
    /// Work done.
    pub work: WorkReport,
}

#[derive(Clone, Debug)]
struct Region {
    kind: FieldKind,
    start: usize,
    len: usize,
    score: f64,
    scale: Option<u32>,
    add: Option<i64>,
    reasons: Vec<String>,
}

/// Suggests field boundaries over aligned `frames` (bits, one `u8` per bit), in capture order.
pub fn suggest_fields(frames: &[Vec<u8>], cfg: &FieldsConfig) -> FieldsReport {
    let mut meter = Meter::new(cfg.budget);
    let aligned;
    let frames = match &cfg.align {
        Some(a) => {
            aligned = align_frames(frames, &a.sync, a.max_errors, &mut meter);
            aligned.as_slice()
        }
        None => frames,
    };
    let n = frames.len();
    let min_len = frames.iter().map(Vec::len).min().unwrap_or(0);
    let max_len = frames.iter().map(Vec::len).max().unwrap_or(0);
    let fixed = min_len == max_len;
    let byte_structured = n > 0 && frames.iter().all(|f| f.len() % 8 == 0);
    let mut report = FieldsReport {
        frames: n,
        byte_structured,
        fixed_length: fixed,
        per_bit: Vec::new(),
        suggestions: Vec::new(),
        field_map: FieldMap {
            description: "draft from the authoring assist (suggestions, edit before use)".into(),
            unit: Unit::Bits,
            endianness: Default::default(),
            bit_order: Default::default(),
            fields: Vec::new(),
        },
        field_map_errors: Vec::new(),
        codes: Vec::new(),
        work: WorkReport::default(),
    };
    if n < cfg.min_frames.max(2) || min_len < 8 {
        meter.skip("fields: needs ≥ min_frames frames of ≥ 8 bits");
        report.work = meter.report();
        return report;
    }
    if max_len > MAX_FIELD_BITS {
        meter.skip(format!(
            "fields: frames longer than {MAX_FIELD_BITS} bits are not classified (post a shorter span)"
        ));
        report.work = meter.report();
        return report;
    }
    // 1. Check field.
    if cfg.find_codes {
        let idx: Vec<(usize, &[u8])> = frames
            .iter()
            .enumerate()
            .map(|(i, f)| (i, f.as_slice()))
            .collect();
        report.codes = search_indexed(&idx, &cfg.codes, &mut meter).0;
    }
    let check = report
        .codes
        .iter()
        .find(|c| c.classes == 1 && c.score >= 0.5)
        .cloned();
    let (check_w, tail) = check
        .as_ref()
        .map_or((0, 0), |c| (usize::from(c.width), c.tail_bits));
    let head_len = min_len.saturating_sub(check_w + tail);
    let log2_head = (head_len.max(2) as f64).log2();
    // Per-bit statistics.
    let span = max_len;
    // A lookup, a branch and two counters per frame bit.
    meter.charge((n * span) as u64 * 2 + 1);
    let mut stats = Vec::with_capacity(span);
    for i in 0..span {
        let mut cov = 0;
        let mut ones = 0;
        let (mut pairs, mut changes) = (0, 0);
        let mut prev: Option<u8> = None;
        for f in frames {
            match f.get(i) {
                Some(&b) => {
                    cov += 1;
                    ones += usize::from(b & 1);
                    if let Some(p) = prev {
                        pairs += 1;
                        changes += usize::from(p != b & 1);
                    }
                    prev = Some(b & 1);
                }
                None => prev = None,
            }
        }
        let p = if cov > 0 {
            ones as f64 / cov as f64
        } else {
            0.0
        };
        stats.push(BitStat {
            coverage: cov,
            ones: p,
            entropy: binary_entropy(p),
            transition: if pairs > 0 {
                changes as f64 / pairs as f64
            } else {
                0.0
            },
        });
    }
    let constancy = |i: usize| stats[i].ones.max(1.0 - stats[i].ones);
    let mut claimed = vec![false; head_len];
    let mut regions: Vec<Region> = Vec::new();
    // 2. Length field.
    let lengths: Vec<usize> = frames.iter().map(Vec::len).collect();
    if !fixed {
        let step = if byte_structured { 8 } else { 1 };
        'len: for off in (0..=64usize).step_by(step) {
            for width in [8usize, 16] {
                if off + width > head_len {
                    continue;
                }
                meter.charge((n * (width + 4)) as u64);
                let vals: Vec<u64> = frames
                    .iter()
                    .map(|f| bits_value(&f[off..off + width]))
                    .collect();
                for scale in [8u32, 1] {
                    let add = lengths[0] as i64 - vals[0] as i64 * i64::from(scale);
                    let hold = vals
                        .iter()
                        .zip(&lengths)
                        .filter(|(v, l)| **v as i64 * i64::from(scale) + add == **l as i64)
                        .count();
                    if hold as f64 >= 0.9 * n as f64 {
                        claimed[off..off + width].fill(true);
                        // A random value matches a length with chance 2^−width; ~130
                        // (offset, width, scale) hypotheses.
                        let share = hold as f64 / n as f64;
                        let sig = ((hold - 1) * width) as f64
                            - n as f64 * binary_entropy(share)
                            - 130f64.log2();
                        regions.push(Region {
                            kind: FieldKind::Length,
                            start: off,
                            len: width,
                            score: share * confidence(sig),
                            scale: Some(scale),
                            add: Some(add),
                            reasons: vec![format!(
                                "frame bits = value × {scale} + {add} in {hold}/{n} frames"
                            )],
                        });
                        break 'len;
                    }
                }
            }
        }
    }
    // 3. Counters.
    let used = &frames[..n.min(512)];
    let mut j = 0;
    while j < head_len {
        if claimed[j] || stats[j].transition < 0.9 {
            j += 1;
            continue;
        }
        let mut kmax = 0;
        let (mut hold_at, mut steps_at) = (0.0, 0);
        let pairs = used.len().saturating_sub(1).max(1);
        for k in 1..=(j + 1).min(32) {
            let a = j + 1 - k;
            if claimed[a] {
                break;
            }
            let modulus = if k == 64 { u64::MAX } else { (1u64 << k) - 1 };
            meter.charge((used.len() * (k + 4)) as u64);
            let vals: Vec<u64> = used.iter().map(|f| bits_value(&f[a..=j])).collect();
            let steps = vals
                .windows(2)
                .filter(|w| w[1].wrapping_sub(w[0]) & modulus == 1)
                .count();
            let hold = steps as f64 / pairs as f64;
            if hold >= cfg.counter_hold {
                kmax = k;
                hold_at = hold;
                steps_at = steps;
            } else {
                break;
            }
        }
        let first_varying = (j + 1 - kmax..=j).find(|&i| constancy(i) < cfg.constant_threshold);
        let Some(fv) = first_varying.filter(|_| kmax > 0) else {
            j += 1;
            continue;
        };
        let varying = j + 1 - fv;
        if varying < 3 {
            j += 1;
            continue;
        }
        let width = if byte_structured {
            (varying.div_ceil(8) * 8).min(kmax)
        } else {
            varying
        };
        let start = j + 1 - width;
        claimed[start..=j].fill(true);
        // Random `varying`-bit values step by one with chance 2^−varying per pair; every end
        // bit and width was a hypothesis.
        let sig = (varying * steps_at) as f64
            - pairs as f64 * binary_entropy(hold_at.min(1.0))
            - log2_head
            - 5.0;
        regions.push(Region {
            kind: FieldKind::Counter,
            start,
            len: width,
            score: hold_at * confidence(sig),
            scale: None,
            add: None,
            reasons: vec![format!(
                "steps by one between consecutive frames in {:.0}% of pairs",
                hold_at * 100.0
            )],
        });
        j += 1;
    }
    // 4. Unclaimed runs.
    let class = |i: usize| {
        if constancy(i) >= cfg.constant_threshold {
            FieldKind::Constant
        } else if stats[i].entropy >= cfg.high_entropy {
            FieldKind::HighEntropy
        } else {
            FieldKind::Mixed
        }
    };
    let mut runs: Vec<(usize, usize)> = Vec::new(); // [start, end)
    let mut i = 0;
    while i < head_len {
        if claimed[i] {
            i += 1;
            continue;
        }
        let k = class(i);
        let mut e = i + 1;
        while e < head_len && !claimed[e] && class(e) == k {
            e += 1;
        }
        runs.push((i, e));
        i = e;
    }
    if byte_structured {
        // Snap boundaries between adjacent unclaimed runs to bytes.
        for r in 1..runs.len() {
            if runs[r - 1].1 == runs[r].0 && runs[r].0 % 8 != 0 {
                let b = runs[r].0;
                let down = b - b % 8;
                let up = down + 8;
                let snapped = if b - down <= up - b { down } else { up };
                let snapped = snapped.clamp(runs[r - 1].0, runs[r].1);
                runs[r - 1].1 = snapped;
                runs[r].0 = snapped;
            }
        }
        runs.retain(|r| r.1 > r.0);
        // Merge neighbours that now share a class.
    }
    for (s, e) in runs {
        let kinds: Vec<FieldKind> = (s..e).map(class).collect();
        let all_const = kinds.iter().all(|k| *k == FieldKind::Constant);
        let ent = (s..e).map(|i| stats[i].entropy).sum::<f64>() / (e - s) as f64;
        let kind = if all_const {
            FieldKind::Constant
        } else if ent >= cfg.high_entropy {
            FieldKind::HighEntropy
        } else {
            FieldKind::Mixed
        };
        let score = match kind {
            // A random bit is this constant over `coverage` frames with chance
            // ≈ 2^−(coverage·(1 − H(constancy)) − 1); any run start was a hypothesis.
            FieldKind::Constant => {
                let sig = (s..e)
                    .map(|i| {
                        (stats[i].coverage as f64 * (1.0 - binary_entropy(constancy(i))) - 1.0)
                            .max(0.0)
                    })
                    .sum::<f64>()
                    - log2_head;
                (s..e).map(constancy).sum::<f64>() / (e - s) as f64 * confidence(sig)
            }
            FieldKind::HighEntropy => ent,
            _ => 0.5,
        };
        if let Some(last) = regions.iter_mut().find(|r| {
            r.start + r.len == s
                && r.kind == kind
                && matches!(
                    kind,
                    FieldKind::Constant | FieldKind::HighEntropy | FieldKind::Mixed
                )
        }) {
            last.len += e - s;
            continue;
        }
        regions.push(Region {
            kind,
            start: s,
            len: e - s,
            score,
            scale: None,
            add: None,
            reasons: Vec::new(),
        });
    }
    regions.sort_by_key(|r| r.start);
    // Variable-length payload.
    let length_region = regions
        .iter()
        .find(|r| r.kind == FieldKind::Length)
        .cloned();
    let mut payload: Option<Region> = None;
    if !fixed {
        let mut start = head_len;
        if let Some(last) = regions.last()
            && last.start + last.len == head_len
            && matches!(last.kind, FieldKind::HighEntropy | FieldKind::Mixed)
        {
            start = last.start;
            regions.pop();
        }
        payload = Some(Region {
            kind: FieldKind::HighEntropy,
            start,
            len: 0,
            score: 0.5,
            scale: None,
            add: None,
            reasons: vec!["length varies per frame".into()],
        });
    }
    // Suggestions and the draft map.
    let mean = |s: usize, e: usize| {
        let e = e.min(span).max(s + 1).min(span);
        let s = s.min(e.saturating_sub(1));
        let k = (e - s).max(1) as f64;
        (
            (s..e).map(|i| stats[i].entropy).sum::<f64>() / k,
            (s..e).map(constancy).sum::<f64>() / k,
        )
    };
    let mut fields: Vec<Field> = Vec::new();
    let mut suggestions: Vec<FieldSuggestion> = Vec::new();
    let mut names: BTreeMap<FieldKind, usize> = BTreeMap::new();
    let mut name_for = |kind: FieldKind, start: usize| -> String {
        let base = match kind {
            FieldKind::Constant => "const",
            FieldKind::Counter => "counter",
            FieldKind::Length => "length",
            FieldKind::HighEntropy => "field",
            FieldKind::Mixed => "mixed",
            FieldKind::Check => "check",
        };
        let c = names.entry(kind).or_default();
        *c += 1;
        if *c == 1 && matches!(kind, FieldKind::Length | FieldKind::Check) {
            base.to_owned()
        } else {
            format!("{base}_{start}")
        }
    };
    let length_name = length_region.as_ref().map(|_| "length".to_owned());
    for r in &regions {
        let name = name_for(r.kind, r.start);
        let (ent, con) = mean(r.start, r.start + r.len);
        let value_hex = (r.kind == FieldKind::Constant && r.len <= 64).then(|| {
            let bits: Vec<u8> = (r.start..r.start + r.len)
                .map(|i| u8::from(stats[i].ones >= 0.5))
                .collect();
            hex_bits(bits_value(&bits), r.len)
        });
        let mut reasons = r.reasons.clone();
        if reasons.is_empty() {
            reasons.push(format!(
                "mean entropy {ent:.2} bits, constancy {:.0}%",
                con * 100.0
            ));
        }
        let ty = if r.len <= 64 {
            FieldType::Uint
        } else {
            FieldType::Bytes
        };
        let display = match (ty, r.kind) {
            (FieldType::Bytes, _) => None,
            (_, FieldKind::Counter | FieldKind::Length) => Some(Display::Dec),
            _ => Some(Display::Hex),
        };
        fields.push(field(
            &name,
            ty,
            Some(r.start),
            Length::Fixed(r.len as u32),
            display,
        ));
        suggestions.push(FieldSuggestion {
            name,
            kind: r.kind,
            bit_offset: r.start,
            bit_len: Some(r.len),
            from_end: false,
            value_hex,
            length_scale: r.scale,
            length_add: r.add,
            mean_entropy: ent,
            mean_constancy: con,
            score: r.score,
            reasons,
        });
    }
    let check_name = "check".to_owned();
    match (&payload, &check) {
        (None, Some(c)) => {
            // Fixed length: the check field and the tail follow the head.
            let (ent, con) = mean(head_len, head_len + check_w);
            fields.push(field(
                &check_name,
                FieldType::Uint,
                Some(head_len),
                Length::Fixed(check_w as u32),
                Some(Display::Hex),
            ));
            suggestions.push(check_suggestion(&check_name, c, head_len, false, ent, con));
            if tail > 0 {
                fields.push(field(
                    "tail",
                    FieldType::Uint,
                    Some(head_len + check_w),
                    Length::Fixed(tail as u32),
                    Some(Display::Hex),
                ));
            }
        }
        (Some(p), c) => {
            let name = name_for(FieldKind::HighEntropy, p.start);
            let (ent, con) = mean(p.start, head_len.max(p.start + 1));
            let (length, reason) = match (&length_region, &length_name) {
                (Some(lr), Some(ln)) => {
                    let scale = lr.scale.unwrap_or(1);
                    let add = lr.add.unwrap_or(0) - p.start as i64 - (check_w + tail) as i64;
                    (
                        Length::FromField(LengthFrom {
                            field: ln.clone(),
                            scale,
                            add,
                        }),
                        format!("length = {ln} × {scale} + {add} bits"),
                    )
                }
                _ => (
                    Length::Keyword(LengthKeyword::Remainder),
                    "no length field found: remainder".to_owned(),
                ),
            };
            let offset = (p.start <= head_len).then_some(p.start);
            fields.push(field(&name, FieldType::Bytes, offset, length.clone(), None));
            suggestions.push(FieldSuggestion {
                name,
                kind: FieldKind::HighEntropy,
                bit_offset: p.start,
                bit_len: None,
                from_end: false,
                value_hex: None,
                length_scale: None,
                length_add: None,
                mean_entropy: ent,
                mean_constancy: con,
                score: p.score,
                reasons: vec!["length varies per frame".into(), reason],
            });
            if let Some(c) = c {
                let (ent, con) = (1.0, 0.5);
                if matches!(length, Length::FromField(_)) {
                    fields.push(field(
                        &check_name,
                        FieldType::Uint,
                        None,
                        Length::Fixed(check_w as u32),
                        Some(Display::Hex),
                    ));
                    if tail > 0 {
                        fields.push(field(
                            "tail",
                            FieldType::Uint,
                            None,
                            Length::Fixed(tail as u32),
                            Some(Display::Hex),
                        ));
                    }
                }
                suggestions.push(check_suggestion(
                    &check_name,
                    c,
                    check_w + tail,
                    true,
                    ent,
                    con,
                ));
            }
        }
        (None, None) => {}
    }
    report.field_map.fields = fields;
    report.field_map_errors = match report.field_map.validate() {
        Ok(()) => Vec::new(),
        Err(errs) => errs.iter().map(|e| format!("{e:?}")).collect(),
    };
    report.per_bit = stats;
    report.suggestions = suggestions;
    report.work = meter.report();
    report
}

fn check_suggestion(
    name: &str,
    c: &CodeSuggestion,
    offset: usize,
    from_end: bool,
    ent: f64,
    con: f64,
) -> FieldSuggestion {
    FieldSuggestion {
        name: name.to_owned(),
        kind: FieldKind::Check,
        bit_offset: offset,
        bit_len: Some(usize::from(c.width)),
        from_end,
        value_hex: None,
        length_scale: None,
        length_add: None,
        mean_entropy: ent,
        mean_constancy: con,
        score: c.score,
        reasons: vec![format!(
            "{}-bit check, generator {} ({}/{} frames)",
            c.width,
            hex_bits(c.generator, usize::from(c.width) + 1),
            c.validated,
            c.tested
        )],
    }
}

fn field(
    name: &str,
    ty: FieldType,
    offset: Option<usize>,
    length: Length,
    display: Option<Display>,
) -> Field {
    Field {
        name: name.to_owned(),
        ty,
        label: None,
        offset: offset.map(|o| o as u32),
        length: Some(length),
        unit: None,
        endianness: None,
        bit_order: None,
        condition: None,
        repeat: None,
        values: BTreeMap::new(),
        flags: Vec::new(),
        charset: None,
        char_bits: None,
        parity: None,
        skip_bits: Vec::new(),
        scale: None,
        add: None,
        value_unit: None,
        display,
        fields: Vec::new(),
        terms: Vec::new(),
    }
}
