//! Records (docs/stream-contract.md §5): NDJSON message records, fixed-header binary records, and
//! the "dropped N" marker.
//!
//! The *producer-side* types here ([`MessageRecord`], [`BinaryRecord`]) carry content; bytes for
//! them are only produced by the publisher, after the egress gate ([`super::gate`]). The
//! *reader-side* types ([`Record`] and friends) are what [`super::StreamReader`] returns.

use hk_model::{
    Annotation, AnnotationId, ContentClass, CrcStatus, Decode, DecodeId, DecodedIdentity,
    EmitterId, ProvenanceId, Timestamp,
};
use serde::Serialize;
use serde_json::Value;

/// Fixed length of a binary record header.
pub const BINARY_RECORD_HEADER_LEN: usize = 32;

/// Binary record type byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum BinaryRecordType {
    /// A data record: header then `payload_len` payload bytes (none if gated).
    Data = 1,
    /// A "dropped N" marker: `seq` is the first dropped seq, payload is the `u64` LE count.
    Dropped = 2,
}

/// Binary record flags (a bit set).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RecordFlags(pub u8);

impl RecordFlags {
    /// Payload withheld by the egress gate: `payload_len` is the withheld length, no bytes follow.
    pub const GATED: RecordFlags = RecordFlags(1 << 0);
    /// A discontinuity (gap, retune, ring overrun) precedes this record.
    pub const DISCONTINUITY: RecordFlags = RecordFlags(1 << 1);
    /// Samples were produced under an overloaded front end (suspect IMD).
    pub const OVERLOAD: RecordFlags = RecordFlags(1 << 2);
    /// First record of a burst.
    pub const BURST_START: RecordFlags = RecordFlags(1 << 3);
    /// Last record of a burst.
    pub const BURST_END: RecordFlags = RecordFlags(1 << 4);

    /// No flags.
    pub const fn empty() -> Self {
        RecordFlags(0)
    }

    /// Whether every bit of `other` is set.
    pub const fn contains(self, other: RecordFlags) -> bool {
        self.0 & other.0 == other.0
    }

    /// Union.
    #[must_use]
    pub const fn with(self, other: RecordFlags) -> Self {
        RecordFlags(self.0 | other.0)
    }
}

/// The 32-byte little-endian binary record header.
///
/// | Offset | Type | Field |
/// |---|---|---|
/// | 0 | u8 | record type (1 data, 2 dropped) |
/// | 1 | u8 | flags |
/// | 2 | u16 | reserved, 0 |
/// | 4 | u32 | payload length (withheld length when gated) |
/// | 8 | u64 | seq |
/// | 16 | i64 | timestamp, ns since Unix epoch (UTC) |
/// | 24 | u64 | sample index of the first element (stream sample-time) |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BinaryRecordHeader {
    /// Type byte (unknown types are passed through by readers).
    pub record_type: u8,
    /// Flags.
    pub flags: RecordFlags,
    /// Payload length.
    pub payload_len: u32,
    /// Sequence number.
    pub seq: u64,
    /// Timestamp.
    pub t: Timestamp,
    /// Sample index.
    pub sample_index: u64,
}

impl BinaryRecordHeader {
    /// Encodes the header.
    pub fn encode(&self) -> [u8; BINARY_RECORD_HEADER_LEN] {
        let mut b = [0u8; BINARY_RECORD_HEADER_LEN];
        b[0] = self.record_type;
        b[1] = self.flags.0;
        b[4..8].copy_from_slice(&self.payload_len.to_le_bytes());
        b[8..16].copy_from_slice(&self.seq.to_le_bytes());
        b[16..24].copy_from_slice(&self.t.as_unix_nanos().to_le_bytes());
        b[24..32].copy_from_slice(&self.sample_index.to_le_bytes());
        b
    }

    /// Decodes a header from the start of a record frame.
    pub fn decode(frame: &[u8]) -> Option<Self> {
        let b = frame.get(..BINARY_RECORD_HEADER_LEN)?;
        let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().expect("4"));
        let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().expect("8"));
        Some(Self {
            record_type: b[0],
            flags: RecordFlags(b[1]),
            payload_len: u32_at(4),
            seq: u64_at(8),
            t: Timestamp::from_unix_nanos(u64_at(16) as i64),
            sample_index: u64_at(24),
        })
    }
}

/// A binary record offered to a publisher. The publisher assigns `seq`.
#[derive(Clone, Copy, Debug)]
pub struct BinaryRecord<'a> {
    /// Time of the first element.
    pub t: Timestamp,
    /// Stream sample index of the first element.
    pub sample_index: u64,
    /// Flags (the publisher sets [`RecordFlags::GATED`] itself; producers may not).
    pub flags: RecordFlags,
    /// Payload bytes, whole elements of the header's datatype.
    pub payload: &'a [u8],
}

/// A message offered to a publisher: one decode or annotation, metadata and content kept apart.
#[derive(Clone, Debug, PartialEq)]
pub struct MessageRecord {
    /// Frame/label time.
    pub t: Timestamp,
    /// Emitter, if resolved.
    pub emitter_id: Option<EmitterId>,
    /// Provenance of the underlying samples.
    pub provenance_ref: Option<ProvenanceId>,
    /// The record's own class. The egress gate clamps it to the stream header's class.
    pub content_class: ContentClass,
    /// Decode row, if the message is a stored Decode.
    pub decode_id: Option<DecodeId>,
    /// Annotation row, if the message is a stored Annotation.
    pub annotation_id: Option<AnnotationId>,
    /// Producer, `decoder_id@version`.
    pub decoder: Option<String>,
    /// Frame model or label.
    pub frame_model: Option<String>,
    /// Frame check.
    pub crc_status: Option<CrcStatus>,
    /// Identity named by the frame (metadata).
    pub identity: Option<DecodedIdentity>,
    /// Metadata: always flows.
    pub metadata: Value,
    /// Content: only leaves when the effective class permits content.
    pub content: Option<Value>,
}

impl MessageRecord {
    /// A message for a Decode row.
    pub fn from_decode(
        d: &Decode,
        emitter_id: Option<EmitterId>,
        provenance_ref: Option<ProvenanceId>,
    ) -> Self {
        Self {
            t: d.t,
            emitter_id,
            provenance_ref,
            content_class: d.content_class,
            decode_id: Some(d.id),
            annotation_id: None,
            decoder: Some(format!("{}@{}", d.decoder_id, d.decoder_version)),
            frame_model: Some(d.frame_model.clone()),
            crc_status: Some(d.crc_status),
            identity: d.identity.clone(),
            metadata: d.metadata.clone(),
            content: d.content.clone(),
        }
    }

    /// A message for an Annotation row.
    pub fn from_annotation(
        a: &Annotation,
        emitter_id: Option<EmitterId>,
        provenance_ref: Option<ProvenanceId>,
    ) -> Self {
        Self {
            t: a.t,
            emitter_id,
            provenance_ref,
            content_class: a.content_class,
            decode_id: None,
            annotation_id: Some(a.id),
            decoder: Some(a.author_ref.clone()),
            frame_model: Some(a.value.clone()),
            crc_status: None,
            identity: None,
            metadata: a.metadata.clone(),
            content: a.content.clone(),
        }
    }
}

/// The on-wire message object. Built only by the gate, which decides whether `content` is set.
#[derive(Serialize)]
pub(crate) struct MessageWire<'a> {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub seq: u64,
    pub t: Timestamp,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emitter_id: Option<EmitterId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance_ref: Option<ProvenanceId>,
    pub content_class: ContentClass,
    pub gated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decode_id: Option<DecodeId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotation_id: Option<AnnotationId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decoder: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_model: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crc_status: Option<CrcStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<&'a DecodedIdentity>,
    pub metadata: &'a Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<&'a Value>,
}

/// A "dropped N" marker as a reader sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DropMarker {
    /// First dropped seq.
    pub first_seq: u64,
    /// Records dropped (consecutive seqs from `first_seq`).
    pub count: u64,
    /// Time of the first dropped record.
    pub t: Timestamp,
    /// Sample index of the first dropped record (0 for messages).
    pub sample_index: u64,
}

/// A message record as a reader sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct MessageEnvelope {
    /// Sequence number.
    pub seq: u64,
    /// Content class, failing closed when missing or unknown.
    pub content_class: ContentClass,
    /// The record was reduced to metadata by the gate.
    pub gated: bool,
    /// The whole JSON object.
    pub value: Value,
}

/// A binary data record as a reader sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct BinaryData {
    /// Header.
    pub header: BinaryRecordHeader,
    /// Payload (empty when gated).
    pub payload: Vec<u8>,
}

/// One record read from a stream.
#[derive(Clone, Debug, PartialEq)]
pub enum Record {
    /// A message (messages streams).
    Message(MessageEnvelope),
    /// A binary data record.
    Binary(BinaryData),
    /// Records were dropped for this consumer.
    Dropped(DropMarker),
    /// A record type this reader does not know (a newer minor version); skip it.
    Unknown(Vec<u8>),
}

/// Longest encoded marker, so markers can be built on the stack.
pub(crate) const MARKER_MAX_LEN: usize = 160;

/// Encodes a marker frame (prefix included) into `buf`; returns its length.
pub(crate) fn encode_marker(binary: bool, m: &DropMarker, buf: &mut [u8; MARKER_MAX_LEN]) -> usize {
    use std::io::Write as _;
    let payload_start = super::frame::LEN_PREFIX;
    let payload_len = if binary {
        let h = BinaryRecordHeader {
            record_type: BinaryRecordType::Dropped as u8,
            flags: RecordFlags::DISCONTINUITY,
            payload_len: 8,
            seq: m.first_seq,
            t: m.t,
            sample_index: m.sample_index,
        };
        buf[payload_start..payload_start + BINARY_RECORD_HEADER_LEN].copy_from_slice(&h.encode());
        buf[payload_start + BINARY_RECORD_HEADER_LEN..payload_start + BINARY_RECORD_HEADER_LEN + 8]
            .copy_from_slice(&m.count.to_le_bytes());
        BINARY_RECORD_HEADER_LEN + 8
    } else {
        let mut cursor = std::io::Cursor::new(&mut buf[payload_start..]);
        writeln!(
            cursor,
            "{{\"type\":\"dropped\",\"first_seq\":{},\"count\":{},\"t\":{}}}",
            m.first_seq,
            m.count,
            m.t.as_unix_nanos()
        )
        .expect("marker fits: three integers of at most 20 digits");
        cursor.position() as usize
    };
    buf[..payload_start].copy_from_slice(&(payload_len as u32).to_le_bytes());
    payload_start + payload_len
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_header_round_trip() {
        let h = BinaryRecordHeader {
            record_type: 1,
            flags: RecordFlags::GATED.with(RecordFlags::OVERLOAD),
            payload_len: 4096,
            seq: u64::MAX - 1,
            t: Timestamp::from_unix_nanos(-5),
            sample_index: 123_456_789,
        };
        assert_eq!(BinaryRecordHeader::decode(&h.encode()), Some(h));
        assert_eq!(BinaryRecordHeader::decode(&[0; 31]), None);
    }

    #[test]
    fn marker_encodings_fit_the_stack_buffer() {
        let m = DropMarker {
            first_seq: u64::MAX,
            count: u64::MAX,
            t: Timestamp::from_unix_nanos(i64::MIN),
            sample_index: u64::MAX,
        };
        let mut buf = [0u8; MARKER_MAX_LEN];
        for binary in [false, true] {
            let n = encode_marker(binary, &m, &mut buf);
            let len = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
            assert_eq!(len + 4, n);
            if !binary {
                let v: Value = serde_json::from_slice(&buf[4..n]).unwrap();
                assert_eq!(v["type"], "dropped");
                assert_eq!(v["count"], u64::MAX);
            }
        }
    }
}
