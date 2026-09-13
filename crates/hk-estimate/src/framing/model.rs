//! The framing model and per-burst frame records. **Structure only**: nothing here carries
//! payload bits, so a model can always be stored and streamed as metadata.

use serde::{Deserialize, Serialize};

use super::bits::BitOrder;
use super::crc::{CrcParams, Endianness};
use super::sync::SyncAnchor;
use super::whitening::Whitening;

/// FSK polarity against the demodulator convention (bit 1 = +deviation).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Polarity {
    /// Confirmed by a CRC: bit 1 = +deviation.
    Normal,
    /// Confirmed by a CRC: the observed bits are complemented.
    Inverted,
    /// No CRC: the sync is reported as observed (bit 1 = +deviation).
    Unresolved,
}

/// How far inference got.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FramingStatus {
    /// Nothing found.
    Unknown,
    /// Preambles but no common sync.
    PreambleOnly,
    /// Preamble and sync; no CRC validated.
    SyncOnly,
    /// Preamble, sync and a validated CRC.
    Complete,
}

/// Why something is unknown or qualified.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FramingReason {
    /// Fewer bursts than the minimum.
    InsufficientCorpus,
    /// No alternating preamble in enough bursts.
    NoPreamble,
    /// No common bits after the preamble.
    NoCommonSync,
    /// No CRC hypothesis validated.
    NoCrcValidated,
    /// A CRC hypothesis validated on some bursts but below the claim thresholds.
    CrcBelowThreshold,
    /// Too few distinct messages to tell a CRC from repetition.
    InsufficientMessageVariety,
    /// A linear CRC relation holds but no standard init/xorout/polarity/whitening explains it.
    CrcInitUnresolved,
    /// The payload looks encrypted or scrambled: labelled, not analysed further.
    EncryptedOrScrambled,
    /// Polarity unresolved (no CRC).
    PolarityUnresolved,
    /// Bit order unresolved (no CRC): hex rendered MSB first.
    BitOrderUnresolved,
    /// The sync was supplied by a prior model.
    SyncFromPrior,
}

/// Preamble statistics over the bursts where one was found.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreambleModel {
    /// `"alternating"` (`0101…`).
    pub pattern: String,
    /// Shortest, bits.
    pub length_bits_min: usize,
    /// Median, bits.
    pub length_bits_median: usize,
    /// Longest, bits.
    pub length_bits_max: usize,
    /// Bursts with a preamble.
    pub bursts: usize,
}

/// The sync word.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SyncModel {
    /// Bits in transmission order, corrected polarity (`"0010110111010100"`).
    pub bits: String,
    /// Length, bits.
    pub length_bits: usize,
    /// Hex in `hex_bit_order` (whole bytes only).
    pub hex: Option<String>,
    /// Bit order used for `hex` (the CRC's when resolved, else MSB first).
    pub hex_bit_order: BitOrder,
    /// Bursts where the word was located.
    pub found_in: usize,
    /// `found_in` / bursts.
    pub found_ratio: f64,
    /// Bit-error budget used to locate it.
    pub max_bit_errors: usize,
    /// Lowest cross-burst majority share over the word when learned.
    pub consensus_agreement: f64,
    /// Common (zero-entropy) bits after the preamble end.
    pub common_bits_after_preamble: usize,
    /// Constant bits after the word (a fixed header: an ID, a type).
    pub fixed_header_bits: usize,
    /// Placement rule.
    pub anchor: SyncAnchor,
}

/// Evidence that a whitening sequence is in use.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum WhiteningEvidence {
    /// The CRC validates only after dewhitening.
    Crc,
    /// A length field appears only after dewhitening.
    LengthField,
    /// Pooled payload byte entropy drops after dewhitening.
    ByteEntropy {
        /// Raw, bits per byte.
        raw_bits_per_byte: f64,
        /// Dewhitened, bits per byte.
        whitened_bits_per_byte: f64,
    },
}

/// Detected whitening.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WhiteningModel {
    /// Sequence (starts at the first bit after the sync).
    pub whitening: Whitening,
    /// Name.
    pub name: String,
    /// Evidence.
    pub evidence: WhiteningEvidence,
}

/// Length-field encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LengthFieldKind {
    /// One byte.
    U8,
    /// Two bytes, big endian.
    U16Be,
    /// Two bytes, little endian.
    U16Le,
    /// The low 11 bits of a 16-bit big-endian header (IEEE 802.15.4g PHR style).
    Phr11,
}

impl LengthFieldKind {
    /// All kinds.
    pub const ALL: [LengthFieldKind; 4] = [
        LengthFieldKind::U8,
        LengthFieldKind::U16Be,
        LengthFieldKind::U16Le,
        LengthFieldKind::Phr11,
    ];

    /// Field width on air, bits.
    pub fn width_bits(self) -> usize {
        match self {
            LengthFieldKind::U8 => 8,
            _ => 16,
        }
    }

    /// Value from whole bytes.
    pub fn read(self, bytes: &[u8]) -> Option<usize> {
        match self {
            LengthFieldKind::U8 => bytes.first().map(|&b| usize::from(b)),
            LengthFieldKind::U16Be if bytes.len() >= 2 => {
                Some(usize::from(bytes[0]) << 8 | usize::from(bytes[1]))
            }
            LengthFieldKind::U16Le if bytes.len() >= 2 => {
                Some(usize::from(bytes[1]) << 8 | usize::from(bytes[0]))
            }
            LengthFieldKind::Phr11 if bytes.len() >= 2 => {
                Some((usize::from(bytes[0]) << 8 | usize::from(bytes[1])) & 0x7FF)
            }
            _ => None,
        }
    }
}

/// Where a length field came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LengthFieldSource {
    /// It places a validating CRC in every supporting burst.
    CrcPositions,
    /// It correlates with the observed burst length.
    BurstLength,
}

/// A length-field hypothesis. The value counts bytes after the field; the frame after the
/// field is `value + adjust_bytes` bytes (CRC included for `CrcPositions` models).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LengthFieldModel {
    /// Field start, bits after the sync word.
    pub offset_bits: usize,
    /// Encoding.
    pub kind: LengthFieldKind,
    /// Width, bits.
    pub width_bits: usize,
    /// Byte packing.
    pub bit_order: BitOrder,
    /// Constant added to the value, bytes.
    pub adjust_bytes: i32,
    /// Supporting bursts.
    pub support: usize,
    /// Bursts tested.
    pub tested: usize,
    /// `support / tested`.
    pub support_ratio: f64,
    /// Evidence.
    pub source: LengthFieldSource,
}

/// How a CRC was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CrcMethod {
    /// Fixed position; `crc(data) ⊕ field` constant across bursts (init, xorout, polarity and
    /// whitening cancel), then the constant explained by a standard setting.
    FixedPositionDifferential,
    /// Position given by a length field; exact validation.
    LengthField,
}

/// The CRC coverage, in bits relative to the end of the sync word.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CrcSpan {
    /// First covered bit (negative: the sync word is covered).
    pub start_bits: i32,
    /// Covered bits, fixed-length frames.
    pub covered_bits: Option<usize>,
    /// CRC field position, fixed-length frames.
    pub crc_offset_bits: Option<i32>,
    /// Coverage set per burst by the length field.
    pub from_length_field: bool,
}

/// An identified CRC.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CrcModel {
    /// Catalogue name or variant description (the CRC id).
    pub algorithm: String,
    /// `algorithm` is a catalogue entry.
    pub catalogue: bool,
    /// Parameters.
    pub params: CrcParams,
    /// Coverage.
    pub span: CrcSpan,
    /// CRC field byte order.
    pub endianness: Endianness,
    /// Byte packing of the covered bits.
    pub bit_order: BitOrder,
    /// Polarity it validated under.
    pub polarity: Polarity,
    /// Whitening it validated under.
    pub whitening: Option<Whitening>,
    /// First whitened bit, bits after the sync word (0, or after an unwhitened length field).
    pub whitening_offset_bits: usize,
    /// Bursts validating.
    pub validated: usize,
    /// Bursts tested (every burst with a located sync).
    pub tested: usize,
    /// `validated / tested`.
    pub validate_ratio: f64,
    /// Distinct covered messages among the validating bursts.
    pub distinct_messages: usize,
    /// Upper bound on the expected number of hypotheses validating this well by chance.
    pub false_alarm_bound: f64,
    /// Hypotheses searched.
    pub hypotheses: u64,
    /// Search method.
    pub method: CrcMethod,
}

/// The best CRC hypothesis that was **not** claimed, for reporting.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CrcCandidate {
    /// Description.
    pub description: String,
    /// Bursts validating.
    pub validated: usize,
    /// Bursts tested.
    pub tested: usize,
    /// Distinct covered messages.
    pub distinct_messages: usize,
    /// False-alarm bound.
    pub false_alarm_bound: f64,
    /// Why it was not claimed.
    pub reason: FramingReason,
}

/// Payload character, from per-position entropy across bursts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PayloadClass {
    /// Constant fields or a length field: structure is visible.
    Structured,
    /// Every position near 1 bit of entropy, no constant fields after standard dewhitening:
    /// **labelled and left alone** (metadata only; no decryption or de-scrambling is attempted
    /// beyond the standard whitening sequences).
    EncryptedOrScrambled,
    /// Too few bursts or bits to tell.
    InsufficientCorpus,
    /// Neither.
    Unknown,
}

impl PayloadClass {
    /// Wire name, e.g. `encrypted-or-scrambled`.
    pub fn as_str(self) -> &'static str {
        match self {
            PayloadClass::Structured => "structured",
            PayloadClass::EncryptedOrScrambled => "encrypted-or-scrambled",
            PayloadClass::InsufficientCorpus => "insufficient-corpus",
            PayloadClass::Unknown => "unknown",
        }
    }
}

/// Payload assessment (statistics only, no payload values).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PayloadAssessment {
    /// Class.
    pub class: PayloadClass,
    /// Bursts used.
    pub bursts: usize,
    /// Bit positions assessed.
    pub positions: usize,
    /// Mean binary entropy per position, bits.
    pub mean_entropy_bits: f64,
    /// Fraction of positions with a ≥ 90 % majority.
    pub constant_fraction: f64,
    /// Pooled byte entropy, bits per byte.
    pub byte_entropy_bits: f64,
    /// Analysis stopped at the label (encrypted or scrambled).
    pub stopped: bool,
}

/// Frame layout in bits.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FrameShape {
    /// Bits between the sync end and the first payload bit.
    pub header_bits: usize,
    /// Payload bits (fixed-length frames with a CRC).
    pub payload_bits: Option<usize>,
    /// CRC bits.
    pub crc_bits: Option<usize>,
    /// Sync start to CRC end (fixed-length frames).
    pub frame_bits: Option<usize>,
}

/// The inferred framing (C21 `FrameModel` + `CrcModel`). Structure metadata only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FramingModel {
    /// Inference id and version.
    pub version: String,
    /// Status.
    pub status: FramingStatus,
    /// Bursts offered.
    pub bursts: usize,
    /// Preamble.
    pub preamble: Option<PreambleModel>,
    /// Sync word.
    pub sync: Option<SyncModel>,
    /// Bit order (resolved by the CRC).
    pub bit_order: Option<BitOrder>,
    /// Polarity.
    pub polarity: Polarity,
    /// Whitening.
    pub whitening: Option<WhiteningModel>,
    /// Length field.
    pub length_field: Option<LengthFieldModel>,
    /// CRC (only above the claim thresholds; see [`super::FramingConfig`]).
    pub crc: Option<CrcModel>,
    /// Best unclaimed CRC hypothesis.
    pub crc_candidate: Option<CrcCandidate>,
    /// Layout.
    pub frame: FrameShape,
    /// Payload assessment.
    pub payload: Option<PayloadAssessment>,
    /// Confidence, 0–1.
    pub confidence: f64,
    /// Reasons and qualifications.
    pub reasons: Vec<FramingReason>,
}

impl FramingModel {
    /// A short structural id, e.g. `sync=2DD4;crc=CRC-16/CCITT-FALSE;payload=48`. Metadata.
    pub fn signature(&self) -> String {
        let mut parts = Vec::new();
        if let Some(s) = &self.sync {
            parts.push(format!(
                "sync={}",
                s.hex.clone().unwrap_or_else(|| s.bits.clone())
            ));
        }
        match &self.crc {
            Some(c) => parts.push(format!("crc={}", c.algorithm)),
            None => parts.push("crc=none".into()),
        }
        if let Some(p) = self.frame.payload_bits {
            parts.push(format!("payload={p}"));
        }
        if let Some(w) = &self.whitening {
            parts.push(format!("whitening={}", w.name));
        }
        parts.join(";")
    }
}

/// One burst's frame record. Positions and flags only; payload bits are recovered on demand by
/// [`super::FramingResult::payload`] and gated by the caller's content class.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BurstFrame {
    /// Index into the input.
    pub index: usize,
    /// Bits in the burst.
    pub bits: usize,
    /// Preamble run.
    pub preamble: Option<super::sync::PreambleRun>,
    /// First sync bit.
    pub sync_bit: Option<usize>,
    /// Sync bit errors.
    pub sync_bit_errors: Option<usize>,
    /// Alternating bits right before the sync.
    pub preamble_bits_before_sync: Option<usize>,
    /// The burst must be complemented to reach the model polarity.
    pub inverted: bool,
    /// CRC result (`None` without a CRC model or when the frame is truncated).
    pub crc_valid: Option<bool>,
    /// Frame truncated before the CRC.
    pub truncated: bool,
}
