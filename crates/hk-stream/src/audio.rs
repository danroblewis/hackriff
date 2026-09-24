//! The audio stream profile (contract 1.1, T-043; docs/stream-contract.md §12). Legal
//! guardrail: audio is content, so it only exists on a stream whose class permits content; the
//! producer gates before attaching (see [`crate::ondemand`]) and the egress gate withholds any
//! audio payload offered under another class.
//!
//! # Wire shape
//! - **Header:** kind `audio`, `datatype` [`AUDIO_DATATYPE`] (`ri16_le`, mono), `sample_rate_hz`
//!   [`AUDIO_SAMPLE_RATE_HZ`], `center_hz`/`bandwidth_hz` the demodulated RF channel, and the
//!   [`AudioInfo`] profile: mode chosen automatically (no manual mode), its confidence and
//!   evidence rules, the estimated parameters, squelch and AGC settings, frame length.
//! - **Data records** (type 1): `payload` whole `i16` samples, normally
//!   [`AudioInfo::frame_samples`] per record; `sample_index` counts audio samples from 0 at the
//!   stream start and `t` is the time of the first sample. A jump in `sample_index` is a gap
//!   (squelch closed, samples skipped to stay live) and is flagged `DISCONTINUITY`. Sequence
//!   numbers and "dropped N" markers are the contract's: a gap in `seq` is loss.
//! - **Status records** (type 3, [`AudioStatus`]): level, noise, squelch, AGC gain, latency and
//!   loss counters, a few times a second. Metadata only.

use std::str::FromStr;

use hk_model::{DetectionId, EmitterId, EstimatedParams};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ondemand::{OpenRefusal, OpenRequest};

/// Audio payload datatype: signed 16-bit little-endian PCM, mono.
pub const AUDIO_DATATYPE: &str = "ri16_le";
/// Audio sample rate, Hz.
pub const AUDIO_SAMPLE_RATE_HZ: f64 = 48_000.0;
/// Samples per data record (20 ms).
pub const AUDIO_FRAME_SAMPLES: usize = 960;
/// Largest frame on an audio stream: a record header plus two frames of samples (status records
/// are far smaller).
pub const AUDIO_MAX_FRAME_LEN: u32 =
    (crate::record::BINARY_RECORD_HEADER_LEN + 4 * AUDIO_FRAME_SAMPLES) as u32;

/// Squelch settings (header).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SquelchInfo {
    /// Channel SNR at which the squelch opens, dB (closes `hysteresis_db` below it).
    pub open_snr_db: f64,
    /// Hysteresis, dB.
    pub hysteresis_db: f64,
    /// Channel noise power the SNR is measured against, dBFS (estimated from the signal);
    /// `None`: no estimate, the squelch stays open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noise_dbfs: Option<f64>,
}

/// AGC settings (header).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AgcInfo {
    /// Whether AGC runs (FM modes are scaled by deviation instead).
    pub enabled: bool,
    /// Target output level, dBFS.
    pub target_dbfs: f64,
    /// Largest gain, dB.
    pub max_gain_db: f64,
}

/// How the demodulated channel was refined from the demodulator's own output (T-070, header).
/// Metadata only: the tuning, the objective's figures and the search statistics.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AudioRefinement {
    /// `refined by output analysis`.
    pub provenance: String,
    /// Objective name and version, e.g. `hk-demod/wfm-output@1`.
    pub objective: String,
    /// Refined channel centre, RF Hz (also the header's `center_hz`).
    pub center_hz: f64,
    /// Refined channel bandwidth, Hz (also the header's `bandwidth_hz`).
    pub bandwidth_hz: f64,
    /// Where the search started (the selection or detection), Hz.
    pub start_center_hz: f64,
    /// Start width, Hz.
    pub start_bandwidth_hz: f64,
    /// Objective value at the result.
    pub quality: f64,
    /// The centre converged.
    pub converged: bool,
    /// Search iterations.
    pub iterations: u32,
    /// Measurements made.
    pub evaluations: u32,
    /// Search time, s.
    pub elapsed_s: f64,
    /// Mode parameters measured at the result (e.g. `pilot_hz`, `occupied_bandwidth_hz`).
    #[serde(default)]
    pub mode_params: std::collections::BTreeMap<String, f64>,
    /// Short labels measured at the result (e.g. `rds_pi`).
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
}

/// The `audio` header profile.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AudioInfo {
    /// Channels (1).
    pub channels: u32,
    /// Samples per data record.
    pub frame_samples: u32,
    /// Mode chosen by auto-mode selection: `wfm`, `nbfm`, `am`, `usb`, `lsb`, `cw`.
    pub mode: String,
    /// Selector confidence, 0–1.
    pub mode_confidence: f64,
    /// Selector rules version.
    pub mode_rules: String,
    /// Parameters estimated from the signal (bandwidth, CFO, deviation, pilot).
    pub params: EstimatedParams,
    /// Channel SNR over the probe, dB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snr_db: Option<f64>,
    /// Squelch.
    pub squelch: SquelchInfo,
    /// AGC.
    pub agc: AgcInfo,
    /// De-emphasis time constant, s (WFM).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deemphasis_s: Option<f64>,
    /// Demodulator id and version.
    pub demod: String,
    /// Output-driven refinement of the channel (T-070), when it ran and locked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refinement: Option<AudioRefinement>,
    /// Recipe pipeline serving this audio (ADR-0011 §8.2, T-866; absent on Listen's own chain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline_id: Option<String>,
    /// That pipeline's recipe, `<id>@<version>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<String>,
    /// The recipe's `audio` output id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_id: Option<String>,
    /// The pipeline's edit revision when the stream was offered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_rev: Option<u32>,
}

/// A status record's fields.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AudioStatus {
    /// Channel power, dBFS.
    pub level_dbfs: f64,
    /// Channel SNR, dB (`None` without a noise estimate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snr_db: Option<f64>,
    /// Squelch open.
    pub squelch_open: bool,
    /// AGC gain, dB.
    pub agc_gain_db: f64,
    /// Audio data records published.
    pub frames: u64,
    /// Frames withheld while the squelch was closed.
    pub squelched_frames: u64,
    /// Source samples lost to ring overruns or skipped to stay live.
    pub lost_samples: u64,
    /// Newest processing latency (source chunk read to record published), ms.
    pub latency_ms: f64,
    /// Stream samples waiting in the ring behind the reader, as seconds.
    pub backlog_s: f64,
    /// Refined channel centre in force, Hz (T-070; `None` when not refined).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refined_center_hz: Option<f64>,
    /// Refined channel bandwidth in force, Hz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refined_bandwidth_hz: Option<f64>,
    /// Live re-refinements that retuned the channel.
    #[serde(default)]
    pub refine_updates: u64,
}

impl AudioStatus {
    /// The status as a flat JSON object (what `Publisher::publish_status` accepts).
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// Encodes `samples` (±1 full scale) as `ri16_le`, clamped, appending to `out`.
pub fn encode_pcm(samples: &[f32], out: &mut Vec<u8>) {
    out.reserve(samples.len() * 2);
    for &s in samples {
        let v = if s.is_finite() {
            (s.clamp(-1.0, 1.0) * 32767.0).round() as i16
        } else {
            0
        };
        out.extend_from_slice(&v.to_le_bytes());
    }
}

/// Decodes `ri16_le` bytes to ±1 samples (a trailing odd byte is ignored).
pub fn decode_pcm(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(2)
        .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32767.0)
        .collect()
}

/// What to listen to. There is no mode or parameter field: those are estimated.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ListenTarget {
    /// An inventory emitter (`emitter=<id>`).
    Emitter(EmitterId),
    /// A detection (`detection=<id>`).
    Detection(DetectionId),
    /// A selected frequency extent (`f_lo=<Hz>&f_hi=<Hz>`).
    Range {
        /// Lower edge, Hz.
        f_lo_hz: f64,
        /// Upper edge, Hz.
        f_hi_hz: f64,
    },
}

/// Widest selection accepted, Hz.
pub const MAX_LISTEN_SPAN_HZ: f64 = 1.0e6;

impl ListenTarget {
    /// Parses a listen request. Parameters such as `mode` are refused: the mode is estimated.
    pub fn from_request(req: &OpenRequest) -> Result<Self, OpenRefusal> {
        let bad = |why: &str| OpenRefusal::new(400, "bad-request", why);
        for (k, _) in &req.params {
            if !matches!(k.as_str(), "emitter" | "detection" | "f_lo" | "f_hi") {
                return Err(bad(&format!(
                    "unknown parameter {k:?}: listening takes emitter, detection, or f_lo and \
                     f_hi (mode and parameters are estimated from the signal)"
                )));
            }
        }
        if let Some(id) = req.param("emitter") {
            return EmitterId::from_str(id)
                .map(Self::Emitter)
                .map_err(|_| bad("emitter is not an id"));
        }
        if let Some(id) = req.param("detection") {
            return DetectionId::from_str(id)
                .map(Self::Detection)
                .map_err(|_| bad("detection is not an id"));
        }
        let hz = |name: &str| {
            req.param(name)
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v >= 0.0)
        };
        match (hz("f_lo"), hz("f_hi")) {
            (Some(f_lo_hz), Some(f_hi_hz)) if f_hi_hz > f_lo_hz => {
                if f_hi_hz - f_lo_hz > MAX_LISTEN_SPAN_HZ {
                    return Err(bad("selection wider than 1 MHz"));
                }
                Ok(Self::Range { f_lo_hz, f_hi_hz })
            }
            (Some(_), Some(_)) => Err(bad("need f_lo < f_hi")),
            _ => Err(bad("need emitter, detection, or f_lo and f_hi (Hz)")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::metadata_is_allowlist_shaped;

    fn req(q: &[(&str, &str)]) -> OpenRequest {
        OpenRequest {
            params: q
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            peer: String::new(),
        }
    }

    #[test]
    fn listen_targets_parse_and_manual_modes_are_refused() {
        let id = EmitterId::new();
        assert_eq!(
            ListenTarget::from_request(&req(&[("emitter", &id.to_string())])).unwrap(),
            ListenTarget::Emitter(id)
        );
        assert_eq!(
            ListenTarget::from_request(&req(&[("f_lo", "101.2e6"), ("f_hi", "101.4e6")])).unwrap(),
            ListenTarget::Range {
                f_lo_hz: 101.2e6,
                f_hi_hz: 101.4e6
            }
        );
        for bad in [
            req(&[("f_lo", "2"), ("f_hi", "1")]),
            req(&[("f_lo", "1"), ("f_hi", "3e6")]),
            req(&[("emitter", "nope")]),
            req(&[("f_lo", "1"), ("f_hi", "2"), ("mode", "am")]),
            req(&[]),
        ] {
            let e = ListenTarget::from_request(&bad).unwrap_err();
            assert_eq!(e.status, 400, "{bad:?}");
        }
    }

    #[test]
    fn status_is_metadata_shaped_and_pcm_round_trips() {
        let s = AudioStatus {
            level_dbfs: -31.5,
            snr_db: Some(22.0),
            squelch_open: true,
            frames: 7,
            ..AudioStatus::default()
        };
        assert!(metadata_is_allowlist_shaped(&s.to_value()));
        let mut b = Vec::new();
        encode_pcm(&[0.0, 0.5, -1.0, 2.0, f32::NAN], &mut b);
        assert_eq!(b.len(), 10);
        let d = decode_pcm(&b);
        assert!((d[1] - 0.5).abs() < 1e-4 && d[2] == -1.0 && d[3] == 1.0 && d[4] == 0.0);
    }
}
