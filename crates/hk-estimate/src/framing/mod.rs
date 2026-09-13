//! C21 bit-framing inference across bursts (T-013, AWARE-036): preamble, sync word,
//! polarity, bit order, standard whitening, length field, CRC identification and a payload
//! assessment, from the hard bits of many bursts of one emitter cluster.
//!
//! Pipeline ([`infer_framing`]):
//!
//! 1. **Preamble** per burst ([`sync::find_preamble`]): the longest alternating run, isolated
//!    bit errors tolerated.
//! 2. **Sync word** ([`sync`]): bursts aligned on the preamble end (polarity normalised), the
//!    common zero-entropy region after it, a 16- (or 8-) bit word placed by the documented
//!    anchor rule, then located in every burst in both polarities with a bit-error budget and a
//!    preamble requirement. A caller can supply the word from an earlier model
//!    ([`FramingConfig::sync_prior`]), e.g. to find it in weak bursts.
//! 3. **CRC** ([`search`]): fixed-position differential search over the catalogue polynomials
//!    ([`crc::CATALOGUE`], with and without reflection, any init/xorout), bit order, byte order,
//!    span and position, with the constant explained by standard init/xorout, polarity and
//!    whitening (PN9, 7-bit LFSRs); otherwise a length-field-driven exact search. Claimed only
//!    above the thresholds in [`search`]'s docs: ≥ 3 validating bursts over ≥ 3 distinct
//!    messages, validate ratio ≥ 0.5, false-alarm bound ≤ 10⁻³. **Unknown if nothing
//!    validates.**
//! 4. Without a CRC: a **length field** that tracks the observed burst length (raw or after
//!    PN9), and **whitening** by a pooled byte-entropy drop.
//! 5. **Payload assessment** ([`PayloadAssessment`]): per-position entropy across bursts. A
//!    payload with no constant positions and ~1 bit of entropy everywhere is labelled
//!    `encrypted-or-scrambled` and analysis stops there: no decryption, and no de-scrambling
//!    beyond the standard whitening sequences above (CLAUDE.md legal guardrails).
//!
//! The [`FramingModel`] and [`BurstFrame`]s are **structure metadata** (positions, lengths,
//! algorithm ids, counts). Payload bits are only produced on request by
//! [`FramingResult::payload`]; the caller gates them with the emitter's content class.

pub mod bits;
pub mod crc;
pub mod model;
pub(crate) mod payload;
pub(crate) mod search;
pub mod sync;
pub mod whitening;

use serde::{Deserialize, Serialize};

pub use bits::BitOrder;
pub use crc::{CATALOGUE, CatalogueEntry, CrcParams, Endianness};
pub use model::{
    BurstFrame, CrcCandidate, CrcMethod, CrcModel, CrcSpan, FrameShape, FramingModel,
    FramingReason, FramingStatus, LengthFieldKind, LengthFieldModel, LengthFieldSource,
    PayloadAssessment, PayloadClass, Polarity, PreambleModel, SyncModel, WhiteningEvidence,
    WhiteningModel,
};
pub use sync::{PreambleRun, SyncAnchor, SyncHit, find_preamble, locate_sync};
pub use whitening::Whitening;

use search::{Corpus, corrected_after_sync, crc_layout, validate};

/// Inference id and version recorded in every [`FramingModel`].
pub const FRAMING_VERSION: &str = "hk-estimate/c21-framing@0.1.0";

/// Settings. Defaults are the documented thresholds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FramingConfig {
    /// Shortest alternating run that counts as a preamble, bits.
    pub min_preamble_bits: usize,
    /// Fewest bursts to learn a sync or claim a CRC.
    pub min_bursts: usize,
    /// Majority share for a common bit after the preamble.
    pub common_agreement: f64,
    /// Sync bit errors tolerated per 16 bits.
    pub sync_max_errors_per_16: usize,
    /// Alternating bits required right before a located sync.
    pub locate_min_preamble_bits: usize,
    /// Bits examined after the preamble end.
    pub max_look_bits: usize,
    /// A known sync word (transmission order, observed polarity): skip learning.
    pub sync_prior: Option<Vec<u8>>,
    /// CRC claim: minimum validating fraction of bursts with a sync.
    pub crc_min_validate_ratio: f64,
    /// CRC claim: maximum false-alarm bound.
    pub crc_max_false_alarm: f64,
    /// CRC claim: minimum validating bursts.
    pub crc_min_valid: usize,
    /// CRC claim: minimum distinct covered messages.
    pub crc_min_distinct: usize,
    /// Longest frame searched, bytes.
    pub max_frame_bytes: usize,
    /// Length field (burst-length evidence): minimum supporting fraction.
    pub length_field_min_support: f64,
    /// Payload assessment: minimum bursts.
    pub payload_min_bursts: usize,
    /// Payload assessment: positions examined at most.
    pub payload_max_bits: usize,
}

impl Default for FramingConfig {
    fn default() -> Self {
        Self {
            min_preamble_bits: 16,
            min_bursts: 3,
            common_agreement: 0.85,
            sync_max_errors_per_16: 1,
            locate_min_preamble_bits: 16,
            max_look_bits: 64,
            sync_prior: None,
            crc_min_validate_ratio: 0.5,
            crc_max_false_alarm: 1e-3,
            crc_min_valid: 3,
            crc_min_distinct: 3,
            max_frame_bytes: 256,
            length_field_min_support: 0.7,
            payload_min_bursts: 8,
            payload_max_bits: 256,
        }
    }
}

/// Payload bits of one frame. **Content**: store or stream only under a content class that
/// permits it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayloadBits {
    /// Bits, transmission order, polarity corrected and dewhitened.
    pub bits: Vec<u8>,
    /// Whole bytes packed with `bit_order`.
    pub bytes: Vec<u8>,
    /// Packing used.
    pub bit_order: BitOrder,
    /// CRC result for the frame.
    pub crc_valid: Option<bool>,
}

/// The model and per-burst frame records.
#[derive(Clone, Debug, PartialEq)]
pub struct FramingResult {
    /// Structure.
    pub model: FramingModel,
    /// One per input burst.
    pub frames: Vec<BurstFrame>,
}

impl FramingResult {
    /// Payload bits of burst `index` (`bits` must be the same burst that was inferred): from
    /// the first bit after the header to the CRC field (CRC models), else to the end of the
    /// burst. `None` without a located sync or a truncated CRC frame.
    pub fn payload(&self, index: usize, bits: &[u8]) -> Option<PayloadBits> {
        let frame = self.frames.get(index)?;
        let sync = self.model.sync.as_ref()?;
        let at = frame.sync_bit?;
        let l = sync.length_bits;
        let mut raw: Vec<u8> = bits.get(at..)?.to_vec();
        let order = self.model.bit_order.unwrap_or(BitOrder::MsbFirst);
        if let Some(crc) = &self.model.crc {
            // `crc_layout` expects the learned (observed) polarity and applies the CRC's.
            let learned_inverted = frame.inverted ^ (crc.polarity == Polarity::Inverted);
            if learned_inverted {
                raw.iter_mut().for_each(|b| *b ^= 1);
            }
            let lay = crc_layout(crc, self.model.length_field.as_ref(), &raw, l)?;
            let payload_start = lay.start.max(l);
            let end = lay.start + 8 * lay.covered_bytes;
            let bits = lay.bits[payload_start..end].to_vec();
            return Some(PayloadBits {
                bytes: bits::pack(&bits, order),
                bits,
                bit_order: order,
                crc_valid: frame.crc_valid,
            });
        }
        if frame.inverted {
            raw.iter_mut().for_each(|b| *b ^= 1);
        }
        let mut bits = raw.get(l..)?.to_vec();
        if let Some(w) = &self.model.whitening {
            w.whitening.apply(&mut bits);
        }
        Some(PayloadBits {
            bytes: bits::pack(&bits, order),
            bits,
            bit_order: order,
            crc_valid: None,
        })
    }
}

/// Infers the framing of `bursts` (hard bits, transmission order). See the
/// [module docs](self).
pub fn infer_framing<B: AsRef<[u8]>>(bursts: &[B], cfg: &FramingConfig) -> FramingResult {
    let refs: Vec<&[u8]> = bursts.iter().map(AsRef::as_ref).collect();
    let n = refs.len();
    let runs: Vec<Option<PreambleRun>> = refs
        .iter()
        .map(|b| find_preamble(b, cfg.min_preamble_bits))
        .collect();
    let mut reasons = Vec::new();
    let mut lens: Vec<usize> = runs.iter().flatten().map(PreambleRun::len).collect();
    lens.sort_unstable();
    let preamble = (!lens.is_empty()).then(|| PreambleModel {
        pattern: "alternating".into(),
        length_bits_min: lens[0],
        length_bits_median: lens[lens.len() / 2],
        length_bits_max: *lens.last().unwrap(),
        bursts: lens.len(),
    });
    let mut frames: Vec<BurstFrame> = (0..n)
        .map(|i| BurstFrame {
            index: i,
            bits: refs[i].len(),
            preamble: runs[i],
            sync_bit: None,
            sync_bit_errors: None,
            preamble_bits_before_sync: None,
            inverted: false,
            crc_valid: None,
            truncated: false,
        })
        .collect();
    let mut model = FramingModel {
        version: FRAMING_VERSION.into(),
        status: if preamble.is_some() {
            FramingStatus::PreambleOnly
        } else {
            FramingStatus::Unknown
        },
        bursts: n,
        preamble,
        sync: None,
        bit_order: None,
        polarity: Polarity::Unresolved,
        whitening: None,
        length_field: None,
        crc: None,
        crc_candidate: None,
        frame: FrameShape::default(),
        payload: None,
        confidence: if lens.is_empty() { 0.0 } else { 0.1 },
        reasons: Vec::new(),
    };
    if n < cfg.min_bursts {
        reasons.push(FramingReason::InsufficientCorpus);
    }

    // ---- sync word
    let (mut word, common, agreement, anchor, fixed_header) = match &cfg.sync_prior {
        Some(prior) if !prior.is_empty() => {
            reasons.push(FramingReason::SyncFromPrior);
            (prior.clone(), 0, 1.0, SyncAnchor::Prior, 0)
        }
        _ => match sync::learn_sync(
            &refs,
            &runs,
            cfg.common_agreement,
            cfg.min_bursts,
            cfg.max_look_bits,
        ) {
            Some(l) => (
                l.bits,
                l.common_bits,
                l.agreement,
                l.anchor,
                l.fixed_header_bits,
            ),
            None => {
                reasons.push(if lens.is_empty() {
                    FramingReason::NoPreamble
                } else {
                    FramingReason::NoCommonSync
                });
                model.reasons = reasons;
                return FramingResult { model, frames };
            }
        },
    };
    let l = word.len();
    let max_errors = cfg.sync_max_errors_per_16 * l / 16;
    let locate = |word: &[u8], i: usize| {
        locate_sync(
            refs[i],
            word,
            max_errors,
            cfg.locate_min_preamble_bits,
            runs[i].map(|r| r.end),
        )
    };
    let mut hits: Vec<Option<SyncHit>> = (0..n).map(|i| locate(&word, i)).collect();
    // Refine the learned word by majority over the located windows.
    if anchor != SyncAnchor::Prior && hits.iter().flatten().count() >= cfg.min_bursts {
        let refined: Vec<u8> = (0..l)
            .map(|k| {
                let (ones, count) = hits
                    .iter()
                    .enumerate()
                    .filter_map(|(i, h)| h.map(|h| (i, h)))
                    .fold((0usize, 0usize), |(o, c), (i, h)| {
                        (
                            o + usize::from(refs[i][h.bit_index + k] ^ u8::from(h.inverted)),
                            c + 1,
                        )
                    });
                u8::from(2 * ones > count)
            })
            .collect();
        if refined != word {
            word = refined;
            hits = (0..n).map(|i| locate(&word, i)).collect();
        }
    }
    let mut hit_idx: Vec<usize> = (0..n).filter(|&i| hits[i].is_some()).collect();
    let found = hit_idx.len();
    if found == 0 {
        reasons.push(FramingReason::NoCommonSync);
        model.sync = Some(SyncModel {
            bits: bits::bit_string(&word),
            length_bits: l,
            hex: bits::hex(&word, BitOrder::MsbFirst),
            hex_bit_order: BitOrder::MsbFirst,
            found_in: 0,
            found_ratio: 0.0,
            max_bit_errors: max_errors,
            consensus_agreement: agreement,
            common_bits_after_preamble: common,
            fixed_header_bits: fixed_header,
            anchor,
        });
        model.reasons = reasons;
        return FramingResult { model, frames };
    }
    model.status = FramingStatus::SyncOnly;
    let mut anchor = anchor;
    let mut fixed_header = fixed_header;
    let mut hex_order = BitOrder::MsbFirst;

    // Bursts from the (shifted) sync start in the learned polarity, and the word there.
    let shifted = |d: i32| -> (Vec<usize>, Vec<SyncHit>, Vec<Vec<u8>>, Vec<u8>) {
        let (mut idx, mut hs, mut fr) = (Vec::new(), Vec::new(), Vec::new());
        for &i in &hit_idx {
            let h = hits[i].unwrap();
            let p = h.bit_index as i64 + i64::from(d);
            if p < 0 || p as usize + l > refs[i].len() {
                continue;
            }
            let p = p as usize;
            let f: Vec<u8> = refs[i][p..]
                .iter()
                .map(|b| (b & 1) ^ u8::from(h.inverted))
                .collect();
            idx.push(i);
            hs.push(SyncHit {
                bit_index: p,
                errors: h.errors,
                inverted: h.inverted,
                preamble_bits: sync::alternating_bits_before(refs[i], p),
            });
            fr.push(f);
        }
        let w: Vec<u8> = (0..l)
            .map(|k| u8::from(2 * fr.iter().filter(|f: &&Vec<u8>| f[k] == 1).count() > fr.len()))
            .collect();
        (idx, hs, fr, w)
    };
    let (_, _, mut from_sync, _) = shifted(0);

    // ---- CRC, over sync alignments 0, ∓1, ∓2, ∓3 bits (the anchor rule is ambiguous by the
    // bits the sync shares with the preamble or a fixed header; a validated span resolves it).
    if found >= cfg.min_bursts {
        const DELTAS: [i32; 7] = [0, -1, 1, -2, 2, -3, 3];
        let mut hypotheses = 0u64;
        let mut chosen: Option<(i32, search::SearchOutcome)> = None;
        let mut candidate = None;
        for pass in 0..2 {
            for d in DELTAS {
                if anchor == SyncAnchor::Prior && d != 0 {
                    continue;
                }
                let (_, _, fr, _) = shifted(d);
                if fr.len() < cfg.min_bursts {
                    continue;
                }
                let corpus = Corpus {
                    frames: &fr,
                    sync_len: l,
                };
                let out = if pass == 0 {
                    search::search_fixed(&corpus, cfg)
                } else {
                    search::search_length_field(&corpus, cfg)
                };
                hypotheses += out.hypotheses;
                if d == 0 && candidate.is_none() {
                    candidate = out.candidate.clone();
                }
                let better = match (&out.crc, &chosen) {
                    (Some(_), None) => true,
                    (Some(c), Some((_, best))) => {
                        c.validated > best.crc.as_ref().unwrap().validated
                    }
                    _ => false,
                };
                if better {
                    chosen = Some((d, out));
                }
            }
            if chosen.is_some() {
                break;
            }
        }
        // The claim must hold against every hypothesis searched over all alignments.
        if let Some((_, out)) = &mut chosen {
            let crc = out.crc.as_mut().unwrap();
            crc.false_alarm_bound *= hypotheses as f64 / crc.hypotheses.max(1) as f64;
            crc.hypotheses = hypotheses;
            if crc.false_alarm_bound > cfg.crc_max_false_alarm {
                candidate = Some(CrcCandidate {
                    description: crc.algorithm.clone(),
                    validated: crc.validated,
                    tested: crc.tested,
                    distinct_messages: crc.distinct_messages,
                    false_alarm_bound: crc.false_alarm_bound,
                    reason: FramingReason::CrcBelowThreshold,
                });
                chosen = None;
            }
        }
        let outcome = match chosen {
            Some((d, out)) => {
                if d != 0 {
                    let (idx, hs, fr, w) = shifted(d);
                    for &i in &hit_idx {
                        hits[i] = None;
                    }
                    for (k, &i) in idx.iter().enumerate() {
                        let mut h = hs[k];
                        h.errors = bits::hamming(&fr[k][..l], &w);
                        hits[i] = Some(h);
                    }
                    hit_idx = idx;
                    from_sync = fr;
                    word = w;
                    anchor = SyncAnchor::CrcAligned;
                    fixed_header = (fixed_header as i64 - i64::from(d)).max(0) as usize;
                }
                out
            }
            None => search::SearchOutcome {
                candidate,
                hypotheses,
                ..Default::default()
            },
        };
        model.crc_candidate = outcome.candidate;
        for &i in &hit_idx {
            let h = hits[i].unwrap();
            frames[i].sync_bit = Some(h.bit_index);
            frames[i].sync_bit_errors = Some(h.errors);
            frames[i].preamble_bits_before_sync = Some(h.preamble_bits);
            frames[i].inverted = h.inverted;
        }
        if let Some(mut crc) = outcome.crc {
            let lf = outcome.length_field;
            let mut validated = 0;
            for (k, &i) in hit_idx.iter().enumerate() {
                let v = validate(&crc, lf.as_ref(), &from_sync[k], l);
                frames[i].crc_valid = v;
                frames[i].truncated = v.is_none();
                validated += usize::from(v == Some(true));
            }
            crc.validated = validated;
            crc.tested = hit_idx.len();
            crc.validate_ratio = validated as f64 / hit_idx.len().max(1) as f64;
            if let Some(w) = crc.whitening {
                model.whitening = Some(WhiteningModel {
                    whitening: w,
                    name: w.name(),
                    evidence: WhiteningEvidence::Crc,
                });
            }
            model.polarity = crc.polarity;
            model.bit_order = Some(crc.bit_order);
            if crc.polarity == Polarity::Inverted {
                word.iter_mut().for_each(|b| *b ^= 1);
                for &i in &hit_idx {
                    frames[i].inverted ^= true;
                }
            }
            hex_order = crc.bit_order;
            if let (Some(covered), Some(off)) = (crc.span.covered_bits, crc.span.crc_offset_bits) {
                let start = crc.span.start_bits.max(0) as usize;
                model.frame = FrameShape {
                    header_bits: start,
                    payload_bits: Some((off.max(0) as usize).saturating_sub(start)),
                    crc_bits: Some(usize::from(crc.params.width)),
                    frame_bits: Some(l + off.max(0) as usize + usize::from(crc.params.width)),
                };
                let _ = covered;
            } else {
                model.frame.crc_bits = Some(usize::from(crc.params.width));
            }
            model.length_field = lf;
            model.status = FramingStatus::Complete;
            model.crc = Some(crc);
        } else {
            reasons.push(FramingReason::NoCrcValidated);
            if let Some(c) = &model.crc_candidate {
                reasons.push(c.reason);
            }
        }
    } else {
        reasons.push(FramingReason::InsufficientCorpus);
        for &i in &hit_idx {
            let h = hits[i].unwrap();
            frames[i].sync_bit = Some(h.bit_index);
            frames[i].sync_bit_errors = Some(h.errors);
            frames[i].preamble_bits_before_sync = Some(h.preamble_bits);
            frames[i].inverted = h.inverted;
        }
    }

    // ---- no CRC: length field, whitening by entropy
    if model.crc.is_none() {
        model.frame.header_bits = fixed_header;
        let after: Vec<Vec<u8>> = from_sync
            .iter()
            .map(|f| corrected_after_sync(f, l, Polarity::Normal, None, 0))
            .collect();
        model.length_field = payload::length_field_from_burst_length(&after, cfg);
        if model.length_field.is_none() {
            for w in Whitening::PN9 {
                let dewhite: Vec<Vec<u8>> = from_sync
                    .iter()
                    .map(|f| corrected_after_sync(f, l, Polarity::Normal, Some(w), 0))
                    .collect();
                if let Some(lf) = payload::length_field_from_burst_length(&dewhite, cfg) {
                    model.length_field = Some(lf);
                    model.whitening = Some(WhiteningModel {
                        whitening: w,
                        name: w.name(),
                        evidence: WhiteningEvidence::LengthField,
                    });
                    break;
                }
            }
        }
        if model.whitening.is_none() {
            let rows: Vec<&[u8]> = after.iter().map(Vec::as_slice).collect();
            if let Some((w, raw, white)) =
                payload::whitening_by_entropy(&rows, BitOrder::MsbFirst, cfg.payload_max_bits)
            {
                model.whitening = Some(WhiteningModel {
                    whitening: w,
                    name: w.name(),
                    evidence: WhiteningEvidence::ByteEntropy {
                        raw_bits_per_byte: raw,
                        whitened_bits_per_byte: white,
                    },
                });
            }
        }
        reasons.push(FramingReason::PolarityUnresolved);
        reasons.push(FramingReason::BitOrderUnresolved);
    }
    model.sync = Some(SyncModel {
        bits: bits::bit_string(&word),
        length_bits: l,
        hex: bits::hex(&word, hex_order),
        hex_bit_order: hex_order,
        found_in: hit_idx.len(),
        found_ratio: hit_idx.len() as f64 / n.max(1) as f64,
        max_bit_errors: max_errors,
        consensus_agreement: agreement,
        common_bits_after_preamble: common,
        fixed_header_bits: fixed_header,
        anchor,
    });

    // ---- payload assessment (statistics only)
    let result_so_far = FramingResult {
        model: model.clone(),
        frames: frames.clone(),
    };
    let payloads: Vec<Vec<u8>> = hit_idx
        .iter()
        .filter_map(|&i| result_so_far.payload(i, refs[i]))
        .filter(|p| model.crc.is_none() || p.crc_valid != Some(false))
        .map(|p| p.bits)
        .collect();
    let rows: Vec<&[u8]> = payloads.iter().map(Vec::as_slice).collect();
    let assessment = payload::assess_payload(
        &rows,
        model.length_field.is_some(),
        model.bit_order.unwrap_or(BitOrder::MsbFirst),
        cfg,
    );
    if assessment.class == PayloadClass::EncryptedOrScrambled {
        reasons.push(FramingReason::EncryptedOrScrambled);
    }
    model.payload = Some(assessment);
    model.confidence = match (&model.crc, &model.sync) {
        (Some(c), _) => 0.5 + 0.5 * c.validate_ratio,
        (None, Some(s)) => 0.5 * s.found_ratio * s.consensus_agreement,
        _ => 0.1,
    };
    reasons.dedup();
    model.reasons = reasons;
    FramingResult { model, frames }
}
