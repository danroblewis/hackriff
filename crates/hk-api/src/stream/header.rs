//! The stream header (docs/stream-contract.md §4): the first frame of every stream, a JSON object
//! that makes the stream self-describing (SigMF-style `global`, ADR-0004).

use hk_model::{BitstreamId, ContentClass, EmitterId, Framing, ProvenanceId, Timestamp};
use serde::{Deserialize, Deserializer, Serialize};

use super::frame::{HEADER_MAX_LEN, MAX_FRAME_LEN};
use super::record::BINARY_RECORD_HEADER_LEN;

/// Schema id carried in every header.
pub const STREAM_SCHEMA: &str = "hackriff.stream";
/// Contract major version. A reader refuses a different major version.
pub const STREAM_VERSION_MAJOR: u32 = 1;
/// Contract minor version. Minor versions only add optional fields and record types.
pub const STREAM_VERSION_MINOR: u32 = 0;
/// Default `max_frame_len` for new streams (1 MiB).
pub const DEFAULT_MAX_FRAME_LEN: u32 = 1024 * 1024;

/// What a stream carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StreamKind {
    /// Decoded messages and annotations, NDJSON records with `metadata`/`content` split.
    Messages,
    /// Hard bits (packing in `framing`/`datatype`). Content-bearing.
    Bits,
    /// Soft symbols. Content-bearing.
    Symbols,
    /// IQ slices. Content-bearing.
    Iq,
    /// Demodulated audio PCM. Content-bearing.
    Audio,
    /// Power spectra (PSD/waterfall rows). Metadata: energy vs frequency, not message content.
    Spectrum,
}

impl StreamKind {
    /// Every kind, for exhaustive tests.
    pub const ALL: &'static [StreamKind] = &[
        StreamKind::Messages,
        StreamKind::Bits,
        StreamKind::Symbols,
        StreamKind::Iq,
        StreamKind::Audio,
        StreamKind::Spectrum,
    ];

    /// Binary records (everything but `messages`).
    pub const fn is_binary(self) -> bool {
        !matches!(self, StreamKind::Messages)
    }

    /// Whether a binary record's payload is signal content, gated by `content_class`.
    /// `spectrum` is not: a power spectrum says that energy exists at a frequency (metadata),
    /// and the survey waterfall must work for every class. `messages` are gated per field.
    pub const fn payload_is_content(self) -> bool {
        matches!(
            self,
            StreamKind::Bits | StreamKind::Symbols | StreamKind::Iq | StreamKind::Audio
        )
    }

    /// Wire name, e.g. `"iq"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            StreamKind::Messages => "messages",
            StreamKind::Bits => "bits",
            StreamKind::Symbols => "symbols",
            StreamKind::Iq => "iq",
            StreamKind::Audio => "audio",
            StreamKind::Spectrum => "spectrum",
        }
    }
}

/// Header errors.
#[derive(Debug, thiserror::Error)]
pub enum HeaderError {
    /// Not valid header JSON.
    #[error("header JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// Wrong schema id.
    #[error("not a hackriff stream (schema {0:?})")]
    Schema(String),
    /// A major version this build does not speak.
    #[error(
        "unsupported stream contract version {0:?} (this build speaks {STREAM_VERSION_MAJOR}.x)"
    )]
    Version(String),
    /// Inconsistent fields.
    #[error("invalid header: {0}")]
    Invalid(String),
}

/// The first frame of a stream.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamHeader {
    /// Always [`STREAM_SCHEMA`].
    pub schema: String,
    /// `"<major>.<minor>"`.
    pub version: String,
    /// Producer-chosen stream name, e.g. `decodes/adsb`.
    pub stream_id: String,
    /// What the records carry.
    pub kind: StreamKind,
    /// Ceiling class for every record in the stream. On read a missing or unknown class is
    /// [`ContentClass::FAIL_CLOSED`].
    #[serde(
        deserialize_with = "class_fail_closed",
        default = "fail_closed_default"
    )]
    pub content_class: ContentClass,
    /// Producer, e.g. `hk-plugins:readsb@3.16`.
    pub source: String,
    /// Emitter the whole stream belongs to, if one (records also carry their own).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emitter_id: Option<EmitterId>,
    /// Provenance of the samples the stream derives from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance_ref: Option<ProvenanceId>,
    /// Live Bitstream descriptor row (docs/07 §2.16), for bits/symbols/messages streams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitstream_id: Option<BitstreamId>,
    /// SigMF datatype of binary payload elements (`ci8`, `cf32_le`, `ri16_le`, `ru8`...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub datatype: Option<String>,
    /// Sample (or symbol, or spectrum-row) rate, Hz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate_hz: Option<f64>,
    /// RF centre frequency, Hz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub center_hz: Option<f64>,
    /// Bandwidth, Hz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_hz: Option<f64>,
    /// FFT size, for spectrum streams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fft_size: Option<u32>,
    /// Bit/symbol framing (docs/07 §2.16), for bits and symbols streams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framing: Option<Framing>,
    /// Schema id of message records' `metadata`/`content`, for messages streams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_schema: Option<String>,
    /// Largest record payload on this stream.
    pub max_frame_len: u32,
    /// Fixed binary record header length: 32 for binary kinds, 0 for messages.
    pub record_header_len: u32,
    /// When the stream opened.
    pub t_start: Timestamp,
    /// hackriff build that produced the stream.
    pub hackriff_version: String,
}

fn fail_closed_default() -> ContentClass {
    ContentClass::FAIL_CLOSED
}

fn class_fail_closed<'de, D: Deserializer<'de>>(d: D) -> Result<ContentClass, D::Error> {
    let raw = Option::<serde_json::Value>::deserialize(d)?;
    Ok(ContentClass::parse_fail_closed(
        raw.as_ref().and_then(|v| v.as_str()),
    ))
}

impl StreamHeader {
    /// A header with the current schema/version and defaults for the optional fields.
    pub fn new(
        stream_id: impl Into<String>,
        kind: StreamKind,
        content_class: ContentClass,
        source: impl Into<String>,
    ) -> Self {
        Self {
            schema: STREAM_SCHEMA.to_owned(),
            version: format!("{STREAM_VERSION_MAJOR}.{STREAM_VERSION_MINOR}"),
            stream_id: stream_id.into(),
            kind,
            content_class,
            source: source.into(),
            emitter_id: None,
            provenance_ref: None,
            bitstream_id: None,
            datatype: None,
            sample_rate_hz: None,
            center_hz: None,
            bandwidth_hz: None,
            fft_size: None,
            framing: None,
            message_schema: None,
            max_frame_len: DEFAULT_MAX_FRAME_LEN,
            record_header_len: if kind.is_binary() {
                BINARY_RECORD_HEADER_LEN as u32
            } else {
                0
            },
            t_start: Timestamp::now(),
            hackriff_version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }

    /// Checks schema, version and field consistency.
    pub fn validate(&self) -> Result<(), HeaderError> {
        if self.schema != STREAM_SCHEMA {
            return Err(HeaderError::Schema(self.schema.clone()));
        }
        let major = self
            .version
            .split('.')
            .next()
            .and_then(|m| m.parse::<u32>().ok());
        if major != Some(STREAM_VERSION_MAJOR) {
            return Err(HeaderError::Version(self.version.clone()));
        }
        if self.max_frame_len == 0 || self.max_frame_len > MAX_FRAME_LEN {
            return Err(HeaderError::Invalid(format!(
                "max_frame_len {} outside 1..={MAX_FRAME_LEN}",
                self.max_frame_len
            )));
        }
        let expected = if self.kind.is_binary() {
            BINARY_RECORD_HEADER_LEN as u32
        } else {
            0
        };
        if self.record_header_len != expected {
            return Err(HeaderError::Invalid(format!(
                "record_header_len {} for kind {}, expected {expected}",
                self.record_header_len,
                self.kind.as_str()
            )));
        }
        if self.kind.is_binary() && self.max_frame_len < BINARY_RECORD_HEADER_LEN as u32 + 8 {
            return Err(HeaderError::Invalid(
                "max_frame_len too small for a binary record".into(),
            ));
        }
        if matches!(self.kind, StreamKind::Iq | StreamKind::Audio)
            && (self.datatype.is_none() || self.sample_rate_hz.is_none())
        {
            return Err(HeaderError::Invalid(format!(
                "{} streams need datatype and sample_rate_hz",
                self.kind.as_str()
            )));
        }
        Ok(())
    }

    /// Serialises the header frame payload.
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, HeaderError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > HEADER_MAX_LEN as usize {
            return Err(HeaderError::Invalid(format!(
                "header is {} bytes, limit {HEADER_MAX_LEN}",
                bytes.len()
            )));
        }
        Ok(bytes)
    }

    /// Parses and validates a header frame payload (content class fails closed).
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, HeaderError> {
        let h: StreamHeader = serde_json::from_slice(bytes)?;
        h.validate()?;
        Ok(h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips_and_fails_closed() {
        let mut h = StreamHeader::new(
            "iq/test",
            StreamKind::Iq,
            ContentClass::Unrestricted,
            "test",
        );
        h.datatype = Some("ci8".into());
        h.sample_rate_hz = Some(2e6);
        let bytes = h.to_json_bytes().unwrap();
        assert_eq!(StreamHeader::from_json_bytes(&bytes).unwrap(), h);

        for replacement in [None, Some("\"unrestrictedd\""), Some("null"), Some("7")] {
            let mut v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            match replacement {
                None => {
                    v.as_object_mut().unwrap().remove("content_class");
                }
                Some(r) => v["content_class"] = serde_json::from_str(r).unwrap(),
            }
            let parsed = StreamHeader::from_json_bytes(&serde_json::to_vec(&v).unwrap()).unwrap();
            assert_eq!(
                parsed.content_class,
                ContentClass::MetadataOnly,
                "{replacement:?}"
            );
        }
    }

    #[test]
    fn header_validation() {
        let mut h = StreamHeader::new("x", StreamKind::Iq, ContentClass::Unrestricted, "t");
        assert!(h.validate().is_err(), "iq without datatype");
        h.datatype = Some("ci8".into());
        h.sample_rate_hz = Some(1.0);
        h.validate().unwrap();
        h.version = "2.0".into();
        assert!(matches!(h.validate(), Err(HeaderError::Version(_))));
        h.version = "1.7".into();
        h.validate().unwrap();
        h.record_header_len = 0;
        assert!(h.validate().is_err());
        let mut m = StreamHeader::new("m", StreamKind::Messages, ContentClass::Unrestricted, "t");
        m.max_frame_len = MAX_FRAME_LEN + 1;
        assert!(m.validate().is_err());
        m.max_frame_len = 1024;
        m.schema = "other".into();
        assert!(matches!(m.validate(), Err(HeaderError::Schema(_))));
    }
}
