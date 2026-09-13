//! Length field from burst length, whitening by byte entropy, and the payload assessment
//! (encrypted-or-scrambled labelling). Statistics only; no payload values leave this module.

use super::FramingConfig;
use super::bits::{BitOrder, binary_entropy, byte_entropy, pack};
use super::model::{
    LengthFieldKind, LengthFieldModel, LengthFieldSource, PayloadAssessment, PayloadClass,
};
use super::whitening::Whitening;

/// A length field whose value tracks the observed bytes after it (±1 byte) in at least
/// `length_field_min_support` of ≥ 5 bursts, taking ≥ 3 distinct values spanning ≥ 3 bytes
/// among the supporters. `after_sync[i]` are burst `i`'s corrected bits after the sync.
pub(crate) fn length_field_from_burst_length(
    after_sync: &[Vec<u8>],
    cfg: &FramingConfig,
) -> Option<LengthFieldModel> {
    let mut best: Option<LengthFieldModel> = None;
    for offset in [0usize, 8, 16] {
        for kind in LengthFieldKind::ALL {
            let width = kind.width_bits();
            for order in BitOrder::ALL {
                let rows: Vec<(usize, i64)> = after_sync
                    .iter()
                    .filter(|b| b.len() >= offset + width + 8)
                    .filter_map(|b| {
                        let l = kind.read(&pack(&b[offset..offset + width], order))?;
                        let avail = ((b.len() - offset - width) / 8) as i64;
                        (l <= cfg.max_frame_bytes).then_some((l, avail - l as i64))
                    })
                    .collect();
                if rows.len() < 5 {
                    continue;
                }
                let mut cs: Vec<i64> = rows.iter().map(|r| r.1).collect();
                cs.sort_unstable();
                let med = cs[cs.len() / 2];
                let supporters: Vec<usize> = rows
                    .iter()
                    .filter(|r| (r.1 - med).abs() <= 1)
                    .map(|r| r.0)
                    .collect();
                let ratio = supporters.len() as f64 / rows.len() as f64;
                let mut distinct = supporters.clone();
                distinct.sort_unstable();
                distinct.dedup();
                let span = distinct
                    .last()
                    .zip(distinct.first())
                    .map_or(0, |(a, b)| a - b);
                if ratio < cfg.length_field_min_support || distinct.len() < 3 || span < 3 {
                    continue;
                }
                if best.as_ref().is_none_or(|b| ratio > b.support_ratio) {
                    best = Some(LengthFieldModel {
                        offset_bits: offset,
                        kind,
                        width_bits: width,
                        bit_order: order,
                        adjust_bytes: med as i32,
                        support: supporters.len(),
                        tested: rows.len(),
                        support_ratio: ratio,
                        source: LengthFieldSource::BurstLength,
                    });
                }
            }
        }
    }
    best
}

/// The first `n` bits reached by at least 75 % of `rows`, and those rows.
fn common_prefix<'a>(rows: &'a [&'a [u8]], max_bits: usize) -> (usize, Vec<&'a [u8]>) {
    if rows.is_empty() {
        return (0, Vec::new());
    }
    let mut lens: Vec<usize> = rows.iter().map(|r| r.len()).collect();
    lens.sort_unstable();
    let n = lens[lens.len() / 4].min(max_bits);
    (n, rows.iter().copied().filter(|r| r.len() >= n).collect())
}

/// A standard whitening under which pooled byte entropy of the payload drops by ≥ 1.5 bits
/// per byte (≥ 128 bytes pooled). Returns (whitening, raw, whitened).
pub(crate) fn whitening_by_entropy(
    after_sync: &[&[u8]],
    order: BitOrder,
    max_bits: usize,
) -> Option<(Whitening, f64, f64)> {
    let (n, rows) = common_prefix(after_sync, max_bits);
    let n = n / 8 * 8;
    if n < 16 || rows.len() * n / 8 < 128 {
        return None;
    }
    let pooled = |w: Option<Whitening>| -> f64 {
        let seq = w.map(|w| w.sequence(n));
        let mut bytes = Vec::with_capacity(rows.len() * n / 8);
        for r in &rows {
            let mut bits = r[..n].to_vec();
            if let Some(s) = &seq {
                bits.iter_mut().zip(s).for_each(|(b, m)| *b ^= m);
            }
            bytes.extend(pack(&bits, order));
        }
        byte_entropy(&bytes)
    };
    let raw = pooled(None);
    Whitening::all()
        .into_iter()
        .map(|w| (w, pooled(Some(w))))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .filter(|(_, e)| raw - e >= 1.5)
        .map(|(w, e)| (w, raw, e))
}

/// Per-position entropy across bursts over the payload region. `Structured` when ≥ 10 % of
/// positions carry a ≥ 90 % majority (or `structured_hint`, e.g. a length field);
/// `EncryptedOrScrambled` when fewer than 10 % do and the mean entropy is ≥ 0.85 bits;
/// `InsufficientCorpus` below `payload_min_bursts` bursts or 24 positions.
pub(crate) fn assess_payload(
    payloads: &[&[u8]],
    structured_hint: bool,
    order: BitOrder,
    cfg: &FramingConfig,
) -> PayloadAssessment {
    let (n, rows) = common_prefix(payloads, cfg.payload_max_bits);
    let insufficient = |bursts, positions| PayloadAssessment {
        class: PayloadClass::InsufficientCorpus,
        bursts,
        positions,
        mean_entropy_bits: f64::NAN,
        constant_fraction: f64::NAN,
        byte_entropy_bits: f64::NAN,
        stopped: false,
    };
    if rows.len() < cfg.payload_min_bursts || n < 24 {
        return insufficient(rows.len(), n);
    }
    let mut entropy = 0.0;
    let mut constant = 0usize;
    for pos in 0..n {
        let ones = rows.iter().filter(|r| r[pos] & 1 == 1).count();
        let share = ones as f64 / rows.len() as f64;
        entropy += binary_entropy(share);
        if share.max(1.0 - share) >= 0.9 {
            constant += 1;
        }
    }
    let mean_entropy = entropy / n as f64;
    let constant_fraction = constant as f64 / n as f64;
    let bytes: Vec<u8> = rows.iter().flat_map(|r| pack(&r[..n], order)).collect();
    let class = if structured_hint || constant_fraction >= 0.1 {
        PayloadClass::Structured
    } else if mean_entropy >= 0.85 {
        PayloadClass::EncryptedOrScrambled
    } else {
        PayloadClass::Unknown
    };
    PayloadAssessment {
        class,
        bursts: rows.len(),
        positions: n,
        mean_entropy_bits: mean_entropy,
        constant_fraction,
        byte_entropy_bits: byte_entropy(&bytes),
        stopped: class == PayloadClass::EncryptedOrScrambled,
    }
}
