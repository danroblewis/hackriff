//! Demodulation (docs/07 §2.14), Decode / Message (§2.15) and Bitstream (§2.16).
//!
//! All three are **append-only interpretations**: re-running a demodulator or decoder writes a
//! new row with its own version, and the measurement it ran on is unchanged. Outputs point back
//! at what produced them (`Decode.demodulation_ref`, `Bitstream.demodulation_ref`,
//! `Recording.trigger`), so producer rows never need updating.
//!
//! Content gating (ADR-0004): see [`crate::content`]. A Decode keeps metadata and content in
//! separate fields so that a gated class can still record the metadata.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cluster::InventoryIdentity;
use crate::content::ContentClass;
use crate::emitter::DecodedIdentity;
use crate::ids::{
    BitstreamId, DecodeId, DemodulationId, DetectionId, EmitterId, ProvenanceId, RecordingId,
};
use crate::region::TimeRange;
use crate::time::Timestamp;

/// Parameters estimated from the signal (C13/C14); never picked by hand.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EstimatedParams {
    /// Symbol rate, Bd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol_rate_hz: Option<f64>,
    /// Frequency deviation, Hz (FM/FSK).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deviation_hz: Option<f64>,
    /// Carrier frequency offset, Hz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cfo_hz: Option<f64>,
    /// Modulation order (2 for BPSK/2FSK, 4 for QPSK/4FSK...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mod_order: Option<u32>,
    /// Pulse-shaping roll-off, 0–1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roll_off: Option<f64>,
    /// Bandwidth used for the channel filter, Hz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_hz: Option<f64>,
    /// Measured stereo pilot frequency, Hz (WFM: nominally 19 kHz, on the receiver clock), when
    /// the pilot locked (T-037b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pilot_hz: Option<f64>,
}

/// A demodulation session on a channel (docs/07 §2.14).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Demodulation {
    /// Id.
    pub id: DemodulationId,
    /// Emitter demodulated, if resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emitter_ref: Option<EmitterId>,
    /// Detection the channel was derived from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detection_ref: Option<DetectionId>,
    /// Recording replayed (offline demodulation), if any. `None` for live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_ref: Option<RecordingId>,
    /// Mode or family, e.g. `wfm`, `2fsk`, `ook`.
    pub mode: String,
    /// Estimated parameters.
    pub params: EstimatedParams,
    /// Lock quality, 0–1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_quality: Option<f64>,
    /// Error vector magnitude, dB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evm_db: Option<f64>,
    /// Session time span.
    pub time: TimeRange,
    /// Demodulator id and version, e.g. `hk-demod/fsk@0.1.0`.
    pub demod_version: String,
}

/// Frame check result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CrcStatus {
    /// Check passed: the frame is ground truth.
    Valid,
    /// Check failed.
    Invalid,
    /// The frame format has no check.
    NoCrc,
    /// Check not yet known (inferred framing, C21).
    Unknown,
    /// The check passed only after FEC corrected bits that the frame's own check alone could not
    /// vouch for (T-210: RDS burst correction while block-synced). The frame is usable, but it is
    /// never CRC-valid evidence: confirm-by-decode, lifecycle and CRC-valid rates count `valid`
    /// only.
    Corrected,
}

/// A decode's evidence for the labelled-capture dataset export (T-205, ADR-0016 §7): the CRC
/// status and the demodulation it decoded, without identity or content. **Only [`CrcStatus::Valid`]
/// counts as decoder-validated ground truth** — a future bounded-correction status (T-210) is a
/// distinct variant and this contract never treats it as evidence, by construction: a reader
/// matches on `Valid` explicitly rather than `!= Invalid`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecodeEvidence {
    /// The decode.
    pub decode_id: DecodeId,
    /// The demodulation it decoded (names the emitter and the modulation `mode`).
    pub demodulation_id: DemodulationId,
    /// Frame check.
    pub crc_status: CrcStatus,
    /// Frame time.
    pub t: Timestamp,
}

/// Structured output from a decoder plugin or bit-framing inference (docs/07 §2.15).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Decode {
    /// Id.
    pub id: DecodeId,
    /// Demodulation decoded, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub demodulation_ref: Option<DemodulationId>,
    /// Recording decoded (replay), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_ref: Option<RecordingId>,
    /// Decoder id, e.g. `readsb`, `hk-rds`, `hk-infer`.
    pub decoder_id: String,
    /// Decoder version.
    pub decoder_version: String,
    /// Frame model name, e.g. `adsb-df17`, `rds-group-0a`, or an inferred-framing id.
    pub frame_model: String,
    /// Metadata fields: frame type, addresses and other non-content identifiers, lengths, timing,
    /// CRC detail. Always stored and streamed, whatever `content_class` says.
    pub metadata: Value,
    /// Content fields: payload, message text, voice/audio references. `None` when the frame has
    /// no content or it was withheld. The repository refuses `Some` unless `content_class`
    /// permits content (`RepoError::GatedContent`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    /// Frame check.
    pub crc_status: CrcStatus,
    /// Identity named by the frame, if any (metadata).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<DecodedIdentity>,
    /// Content gating class, chosen explicitly by the decoder (fail closed to
    /// [`ContentClass::FAIL_CLOSED`] when unknown).
    pub content_class: ContentClass,
    /// Frame time.
    pub t: Timestamp,
}

/// Label written in place of a decode label (`frame_model`, `decoder_id`, `decoder_version`) that
/// names a withheld identifier (T-036).
pub const WITHHELD_LABEL: &str = "withheld";

/// A decode as it leaves the repository, gated for an [`crate::IdentityAccess`] (T-036; rules in
/// [`crate::cluster`]).
///
/// When the gate withholds the row's detail, `decode.identity` is `None`, `decode.metadata` is
/// `{}`, `decode.content` is `None` (a content-permitting row whose identity is withheld) and any
/// label naming the identity, a metadata value or a content value reads [`WITHHELD_LABEL`]. The
/// class, CRC status, time and references are unchanged.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodeView {
    /// The decode, gated.
    pub decode: Decode,
    /// Its identity as shown: clear, withheld (scheme only) or none.
    pub identity: InventoryIdentity,
    /// Non-empty stored metadata was withheld.
    pub metadata_withheld: bool,
    /// Stored content was withheld.
    pub content_withheld: bool,
    /// At least one label was replaced by [`WITHHELD_LABEL`].
    pub labels_withheld: bool,
}

/// What a bitstream carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BitstreamPayload {
    /// Hard bits.
    HardBits,
    /// Soft symbols.
    SoftSymbols,
    /// Framed messages (newline-delimited JSON on stream-output, ADR-0004).
    Messages,
}

/// Framing metadata. The ADR-0004 stream header carries the same information.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Framing {
    /// Payload kind.
    pub payload: BitstreamPayload,
    /// Bits per symbol, for symbol payloads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bits_per_symbol: Option<u32>,
    /// Symbol rate, Bd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol_rate_hz: Option<f64>,
    /// Message schema id, for message payloads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_id: Option<String>,
    /// Sync word / preamble, hex, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_word_hex: Option<String>,
}

/// Where the bits are.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum BitstreamTransport {
    /// A stored file (Recording-like, under quota). Stored bits are content.
    Stored {
        /// Path relative to the device data directory.
        uri: String,
    },
    /// A live stream on stream-output (C24); the row is its descriptor (metadata).
    Live {
        /// Endpoint, e.g. `unix:///run/hackriff/bits-<id>.sock`.
        endpoint: String,
    },
}

/// A bit/soft-symbol artifact or live stream descriptor (docs/07 §2.16).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bitstream {
    /// Id.
    pub id: BitstreamId,
    /// Emitter, if resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emitter_ref: Option<EmitterId>,
    /// Producing demodulation, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub demodulation_ref: Option<DemodulationId>,
    /// Framing.
    pub framing: Framing,
    /// Stored or live.
    pub transport: BitstreamTransport,
    /// Time span; `end` equals `start` while a live stream is open.
    pub time: TimeRange,
    /// Source trust record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance_ref: Option<ProvenanceId>,
    /// Content gating class. The repository refuses a `Stored` bitstream whose class does not
    /// permit content; a `Live` descriptor is accepted and C24 gates the stream itself.
    pub content_class: ContentClass,
}
