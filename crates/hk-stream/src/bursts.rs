//! Burst bits and symbols streams (T-060, docs/stream-contract.md §13): what external programs
//! read to decode bursts (FSK sensors and other unknown bursty emitters) without hard-coding a
//! decoder in hackriff.
//!
//! # Profile (contract 1.1, no new wire elements)
//! - **Header:** `kind` `bits` (`datatype` `ru8`, one byte per bit, value 0 or 1) or `symbols`
//!   (`datatype` `rf32_le`, one soft value per symbol: LLR-like, positive = 1). `framing` carries
//!   `payload` (`hard-bits` / `soft-symbols`) and `bits_per_symbol` 1. `emitter_id`, `center_hz`
//!   and `bandwidth_hz` describe the requested target when there is one.
//! - **Per burst, two records:** a status record (type 3) with [`BurstStatus`], then the data
//!   record (type 1) flagged `BURST_START | BURST_END`, timed at the first symbol, whose
//!   `sample_index` is the source sample of the first symbol centre.
//! - Bits and symbols are in the **framing model's polarity** (inverted bursts are complemented),
//!   so `sync_bit`, `payload_bit` and `payload_bits` index straight into the data record.
//! - The status record is metadata by construction (flat numbers, booleans and tokens,
//!   [`crate::policy::metadata_is_allowlist_shaped`]); payload bytes never ride on it.
//! - **Content withheld:** when the burst's own class forbids content on a stream whose header
//!   class permits it, only the status record goes out, with `content_withheld: true`. On a
//!   stream whose header class forbids content the data record is sent and the egress gate
//!   reduces it to a header-only `GATED` record (§6).

use hk_model::{DetectionId, EmitterId};
use serde_json::{Map, Value, json};

use crate::audio::ListenTarget;
use crate::ondemand::{OpenRefusal, OpenRequest};

/// `datatype` of a burst bits stream: one `u8` (0 or 1) per bit.
pub const BITS_DATATYPE: &str = "ru8";
/// `datatype` of a burst symbols stream: one `f32` LE soft value per symbol (positive = 1).
pub const SYMBOLS_DATATYPE: &str = "rf32_le";

/// Per-burst metadata sent as a status record before each data record.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BurstStatus {
    /// Bursts published on this stream so far (0-based).
    pub burst: u64,
    /// Elements (bits or symbols) in the data record.
    pub symbols: u64,
    /// Symbol rate, Bd.
    pub symbol_rate_bd: f64,
    /// RF centre of the burst, Hz.
    pub f_center_hz: f64,
    /// Burst bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// Per-symbol SNR, dB.
    pub snr_db: Option<f64>,
    /// A sync word was located.
    pub framed: bool,
    /// The burst was complemented to reach the framing model's polarity.
    pub inverted: bool,
    /// Index of the first sync bit.
    pub sync_bit: Option<u64>,
    /// Sync word length, bits.
    pub sync_bits: Option<u64>,
    /// Index of the first payload bit (after sync and any length field).
    pub payload_bit: Option<u64>,
    /// Payload length, bits (up to the CRC field for CRC models).
    pub payload_bits: Option<u64>,
    /// Payload bit order when packed into bytes: `msb-first` or `lsb-first`.
    pub bit_order: Option<&'static str>,
    /// CRC verdict: `valid`, `invalid`, `unknown` (a CRC model exists but this frame wasn't
    /// evaluated, e.g. truncated before the CRC field) or `absent` (no CRC model). `None` only
    /// when the burst has no located frame at all (`framed: false`); a **framed** burst (T-954)
    /// always carries one of the four strings, never an omitted field.
    pub crc: Option<&'static str>,
    /// Emitter the burst was stored under, once known.
    pub emitter_id: Option<EmitterId>,
    /// The burst's class forbids content: no data record follows.
    pub content_withheld: bool,
}

fn finite(v: f64) -> Option<Value> {
    v.is_finite().then(|| json!((v * 1000.0).round() / 1000.0))
}

impl BurstStatus {
    /// The status record payload: a flat object, absent and non-finite fields omitted.
    pub fn to_value(&self) -> Value {
        debug_assert!(
            !self.framed || self.crc.is_some(),
            "a framed burst's status record must carry a crc verdict (T-954)"
        );
        let mut m = Map::new();
        m.insert("burst".into(), json!(self.burst));
        m.insert("symbols".into(), json!(self.symbols));
        for (k, v) in [
            ("symbol_rate_bd", Some(self.symbol_rate_bd)),
            ("f_center_hz", Some(self.f_center_hz)),
            ("bandwidth_hz", Some(self.bandwidth_hz)),
            ("snr_db", self.snr_db),
        ] {
            if let Some(v) = v.and_then(finite) {
                m.insert(k.into(), v);
            }
        }
        m.insert("framed".into(), json!(self.framed));
        m.insert("inverted".into(), json!(self.inverted));
        for (k, v) in [
            ("sync_bit", self.sync_bit),
            ("sync_bits", self.sync_bits),
            ("payload_bit", self.payload_bit),
            ("payload_bits", self.payload_bits),
        ] {
            if let Some(v) = v {
                m.insert(k.into(), json!(v));
            }
        }
        if let Some(o) = self.bit_order {
            m.insert("bit_order".into(), json!(o));
        }
        if let Some(c) = self.crc {
            m.insert("crc".into(), json!(c));
        }
        if let Some(e) = self.emitter_id {
            m.insert("emitter_id".into(), json!(e.to_string()));
        }
        m.insert("content_withheld".into(), json!(self.content_withheld));
        Value::Object(m)
    }
}

/// What a bits or symbols request targets. No parameter selects every burst in the run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BurstTarget {
    /// Every burst the run demodulates.
    All,
    /// An inventory emitter (`emitter=<id>`).
    Emitter(EmitterId),
    /// A detection (`detection=<id>`).
    Detection(DetectionId),
    /// A selected extent (`f_lo=<Hz>&f_hi=<Hz>`).
    Range {
        /// Lower edge, Hz.
        f_lo_hz: f64,
        /// Upper edge, Hz.
        f_hi_hz: f64,
    },
}

impl BurstTarget {
    /// Parses a request: no parameters is [`BurstTarget::All`]; otherwise the Listen target
    /// grammar (`emitter`, `detection`, or `f_lo` and `f_hi`), unknown parameters refused.
    pub fn from_request(req: &OpenRequest) -> Result<Self, OpenRefusal> {
        if req.params.is_empty() {
            return Ok(Self::All);
        }
        Ok(match ListenTarget::from_request(req)? {
            ListenTarget::Emitter(id) => Self::Emitter(id),
            ListenTarget::Detection(id) => Self::Detection(id),
            ListenTarget::Range { f_lo_hz, f_hi_hz } => Self::Range { f_lo_hz, f_hi_hz },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::metadata_is_allowlist_shaped;

    #[test]
    fn status_is_metadata_shaped_and_targets_parse() {
        let s = BurstStatus {
            burst: 3,
            symbols: 120,
            symbol_rate_bd: 4800.0,
            f_center_hz: 433.973e6,
            bandwidth_hz: 24e3,
            snr_db: Some(f64::NAN),
            framed: true,
            sync_bit: Some(32),
            sync_bits: Some(16),
            payload_bit: Some(48),
            payload_bits: Some(48),
            bit_order: Some("msb-first"),
            crc: Some("valid"),
            emitter_id: Some(EmitterId::new()),
            ..BurstStatus::default()
        };
        let v = s.to_value();
        assert!(metadata_is_allowlist_shaped(&v), "{v}");
        assert!(v.get("snr_db").is_none(), "non-finite omitted");
        assert_eq!(v["payload_bit"], 48);

        // T-954: a framed burst's crc field is never omitted, whatever the verdict.
        for verdict in ["valid", "invalid", "unknown", "absent"] {
            let framed = BurstStatus {
                framed: true,
                crc: Some(verdict),
                ..BurstStatus::default()
            };
            let v = framed.to_value();
            assert_eq!(v["crc"], json!(verdict), "{v}");
        }

        let none = OpenRequest::default();
        assert_eq!(BurstTarget::from_request(&none).unwrap(), BurstTarget::All);
        let range = OpenRequest {
            params: vec![
                ("f_lo".into(), "433.9e6".into()),
                ("f_hi".into(), "434e6".into()),
            ],
            peer: String::new(),
        };
        assert!(matches!(
            BurstTarget::from_request(&range).unwrap(),
            BurstTarget::Range { .. }
        ));
        let bad = OpenRequest {
            params: vec![("mode".into(), "fsk".into())],
            peer: String::new(),
        };
        assert_eq!(BurstTarget::from_request(&bad).unwrap_err().status, 400);
    }
}
