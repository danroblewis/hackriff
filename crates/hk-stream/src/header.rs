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
/// 1.1 (T-043): the optional header `audio` profile and the binary `status` record type (3).
/// 1.2 (T-089, ADR-0011): inspector streams: the optional header `inspector` profile and the
/// `frame`, `status` and `edit` message record types (§14).
/// 1.3 and 1.4 (T-410, T-413) changed only the presence stream's own `message_schema`; headers
/// kept saying 1.2.
/// 1.5 (T-874, ADR-0015 §12.13): audio `channels` may be 2 — interleaved `ri16_le` L/R frames,
/// only on a stream whose client asked for them (`channels=2`); a mono stream is unchanged. The
/// header now carries the document's version again.
pub const STREAM_VERSION_MINOR: u32 = 5;
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
    /// Sync-word match score per candidate bit position (`view=sync_search`, §14.4, T-162).
    /// **Content-bearing, unlike `spectrum`:** the caller supplies the word to correlate
    /// against, so a high score is an oracle over the withheld bits (choose a candidate word,
    /// read back whether — and how closely — it matches). Gated exactly like the `bits` it's
    /// computed from.
    SyncSearch,
    /// Clock-recovery eye/timing diagram: the waveform folded around each estimated symbol
    /// instant (`view=eye`, §14.4, T-161). **Content-bearing, like `symbols`:** the centre point
    /// of every trace *is* the pre-decision soft symbol, in symbol order, so slicing that one
    /// column recovers the demodulated bitstream directly. Gated exactly like the `iq`/`real`
    /// port it is folded from.
    Eye,
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
        StreamKind::SyncSearch,
        StreamKind::Eye,
    ];

    /// Binary records (everything but `messages`).
    pub const fn is_binary(self) -> bool {
        !matches!(self, StreamKind::Messages)
    }

    /// Whether a binary record's payload is signal content, gated by `content_class`.
    /// `spectrum` is not: a power spectrum says that energy exists at a frequency, independent
    /// of what a viewer asks for, so it must work for every class. `sync-search` **is** content,
    /// unlike `spectrum`: the caller picks the word it correlates against, so it can be used as
    /// an oracle over withheld bits (see [`StreamKind::SyncSearch`]) and must be gated the same
    /// as the `bits` it reads. `eye` is content for a blunter reason still: its trace centres are
    /// the soft symbols themselves (see [`StreamKind::Eye`]). `messages` are gated per field.
    pub const fn payload_is_content(self) -> bool {
        matches!(
            self,
            StreamKind::Bits
                | StreamKind::Symbols
                | StreamKind::Iq
                | StreamKind::Audio
                | StreamKind::SyncSearch
                | StreamKind::Eye
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
            StreamKind::SyncSearch => "sync-search",
            StreamKind::Eye => "eye",
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
    /// RF centre frequency, Hz. For spectrum streams, the frequency of row element `fft_size/2`
    /// (DC); see [`StreamHeader::spectrum_bin_hz`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub center_hz: Option<f64>,
    /// Bandwidth, Hz. For spectrum streams, the span of a row: `fft_size` bins of
    /// `bandwidth_hz / fft_size` each.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_hz: Option<f64>,
    /// FFT size (elements per row), for spectrum streams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fft_size: Option<u32>,
    /// Half-width of the DC/LO-leakage notch centred on `center_hz`, Hz, for spectrum streams
    /// (ADR-0013 §4.9 gap 10): the same tolerance the detector's DC rule excludes from detection
    /// (`hk_detect::DcRule::tolerance_hz`). `None` when the producer applies no DC mask for this
    /// stream (the field is additive; readers must treat a missing value as "no mask known", not
    /// zero).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dc_excluded_hz: Option<f64>,
    /// Bit/symbol framing (docs/07 §2.16), for bits and symbols streams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framing: Option<Framing>,
    /// Schema id of message records' `metadata`/`content`, for messages streams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_schema: Option<String>,
    /// Audio profile (contract 1.1, T-043): mode chosen, estimated parameters, squelch and AGC,
    /// for `audio` streams ([`crate::audio::AudioInfo`]). Metadata only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<crate::audio::AudioInfo>,
    /// Inspector profile (contract 1.2, §14.1): the pipeline output or capture whose frame
    /// records this stream carries. Metadata only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inspector: Option<crate::inspector::InspectorProfile>,
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
            dc_excluded_hz: None,
            framing: None,
            message_schema: None,
            audio: None,
            inspector: None,
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
        if !self.kind.is_binary() && (self.max_frame_len as usize) < super::record::MARKER_MAX_LEN {
            return Err(HeaderError::Invalid(format!(
                "max_frame_len {} too small for a messages drop marker ({} bytes)",
                self.max_frame_len,
                super::record::MARKER_MAX_LEN
            )));
        }
        if matches!(self.kind, StreamKind::Iq | StreamKind::Audio)
            && (self.datatype.is_none() || self.sample_rate_hz.is_none())
        {
            return Err(HeaderError::Invalid(format!(
                "{} streams need datatype and sample_rate_hz",
                self.kind.as_str()
            )));
        }
        if self.audio.is_some() && self.kind != StreamKind::Audio {
            return Err(HeaderError::Invalid(format!(
                "an audio profile on a {} stream",
                self.kind.as_str()
            )));
        }
        if let Some(a) = &self.audio
            && !(1..=crate::audio::AUDIO_MAX_CHANNELS).contains(&a.channels)
        {
            return Err(HeaderError::Invalid(format!(
                "audio channels {} (1 or 2)",
                a.channels
            )));
        }
        Ok(())
    }

    /// For `spectrum` streams: the centre frequency of row element `bin`, Hz.
    ///
    /// Spectrum rows are DC-centred and ascending (hk-dsp `Spectrum` bin order): element `i` of
    /// `N = fft_size` lies at `center_hz + (i − ⌊N/2⌋) · bandwidth_hz / N` and covers one bin width
    /// `bandwidth_hz / N` around that frequency, so element `⌊N/2⌋` is the tuned centre (DC) and a
    /// row spans `[f_0 − df/2, f_{N−1} + df/2]`. The web UI maps its frequency axis the same way
    /// (`ui/src/axis.ts`, T-045). `None` when the stream is not a spectrum, a geometry field is
    /// missing, or `bin >= fft_size`.
    pub fn spectrum_bin_hz(&self, bin: u32) -> Option<f64> {
        if self.kind != StreamKind::Spectrum {
            return None;
        }
        let (center, bw, n) = (self.center_hz?, self.bandwidth_hz?, self.fft_size?);
        if n == 0 || bin >= n {
            return None;
        }
        Some(center + (f64::from(bin) - f64::from(n / 2)) * bw / f64::from(n))
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

    /// T-162: unlike `spectrum` (a fixed measurement, safe as metadata for every class),
    /// `sync-search` lets the caller choose the word it correlates against — an oracle over
    /// withheld bits — so it must be gated exactly like `bits`.
    #[test]
    fn sync_search_payload_is_content_unlike_spectrum() {
        assert!(StreamKind::SyncSearch.payload_is_content());
        assert!(StreamKind::Bits.payload_is_content());
        assert!(!StreamKind::Spectrum.payload_is_content());
        assert_eq!(StreamKind::SyncSearch.as_str(), "sync-search");
        assert!(StreamKind::ALL.contains(&StreamKind::SyncSearch));
        // T-161: an eye row's trace centres are the soft symbols in order, so slicing one column
        // of it reproduces the demodulated bits. It is content for the same reason `symbols` is —
        // a plainer reason than `sync-search`'s oracle argument, and nothing like `spectrum`.
        assert!(StreamKind::Eye.payload_is_content());
        assert_eq!(StreamKind::Eye.as_str(), "eye");
        assert!(StreamKind::ALL.contains(&StreamKind::Eye));
    }

    /// T-165 (ADR-0013 gap 8): raw channelised IQ (`open/iq`) is content exactly like `bits`,
    /// never metadata — T-162's finding (a caller-shaped view over withheld content is an
    /// oracle) applies even more directly here, since IQ carries the RF envelope besides
    /// whatever a demodulator would extract from it. `payload_is_content` is the one property
    /// `gate::binary_payload_permitted` (§6, the egress enforcement point) checks before a
    /// class's `permits_content`, so this is what actually gates every `iq` binary record the
    /// same way it gates `bits`, regardless of how the header's class was decided.
    #[test]
    fn iq_payload_is_content_exactly_like_bits() {
        assert!(StreamKind::Iq.payload_is_content());
        assert!(StreamKind::Bits.payload_is_content());
        assert!(
            !StreamKind::Spectrum.payload_is_content(),
            "not metadata-shaped like spectrum"
        );
        assert_eq!(StreamKind::Iq.as_str(), "iq");
    }

    #[test]
    fn spectrum_bins_are_dc_centred_and_ascending() {
        let mut h = StreamHeader::new("s", StreamKind::Spectrum, ContentClass::Unrestricted, "t");
        assert_eq!(h.spectrum_bin_hz(0), None, "no geometry");
        h.center_hz = Some(100.8e6);
        h.bandwidth_hz = Some(2.4e6);
        h.fft_size = Some(4096);
        assert_eq!(h.spectrum_bin_hz(2048), Some(100.8e6), "DC is element N/2");
        assert_eq!(h.spectrum_bin_hz(0), Some(99.6e6), "element 0 is -fs/2");
        assert_eq!(h.spectrum_bin_hz(4095), Some(102.0e6 - 585.9375));
        // A 101.3 MHz station (+500 kHz) is element 2048 + 853.33 -> 2901, within one bin.
        assert!((h.spectrum_bin_hz(2901).unwrap() - 101.3e6).abs() < 585.9375);
        assert_eq!(h.spectrum_bin_hz(4096), None);
        h.fft_size = Some(5);
        h.center_hz = Some(0.0);
        h.bandwidth_hz = Some(5.0);
        let odd: Vec<f64> = (0..5).map(|i| h.spectrum_bin_hz(i).unwrap()).collect();
        assert_eq!(odd, [-2.0, -1.0, 0.0, 1.0, 2.0]);
        h.kind = StreamKind::Iq;
        assert_eq!(h.spectrum_bin_hz(2), None, "only spectrum rows have bins");
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
        // Audio channels (T-874): 1 or 2, nothing else.
        let mut a = StreamHeader::new("a", StreamKind::Audio, ContentClass::Unrestricted, "t");
        a.datatype = Some("ri16_le".into());
        a.sample_rate_hz = Some(48_000.0);
        for (channels, ok) in [(0, false), (1, true), (2, true), (3, false)] {
            a.audio = Some(crate::audio::AudioInfo {
                channels,
                ..Default::default()
            });
            assert_eq!(a.validate().is_ok(), ok, "channels {channels}");
        }
    }
}
