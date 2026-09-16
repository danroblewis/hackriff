//! The on-demand channelised IQ profile (T-165, ADR-0013 §4.9 gap 8; docs/stream-contract.md
//! §12.3). `open/iq?emitter=<id>` or `open/iq?f_lo=<Hz>&f_hi=<Hz>` streams the requested band's
//! raw down-converted samples: no mode, no demodulation, no parameter — the channel itself,
//! `cf32_le`.
//!
//! **Legal guardrail.** Raw channelised IQ is unambiguously content (more directly than
//! demodulated audio: it carries everything audio does and the RF envelope besides), so it is
//! never served except on a stream whose class permits content — [`crate::header::StreamKind::Iq`]
//! is already one of [`crate::header::StreamKind::payload_is_content`]'s kinds, so the egress
//! gate ([`crate::gate::binary_payload_permitted`]) withholds the payload exactly as it does for
//! `bits`/`symbols`/`audio` on a stream whose class forbids it; the opener additionally gates
//! *before* any ring read (the same [`crate::audio::ListenTarget`]/`listen_class` rule Listen and
//! burst content already use — see `hk_pipeline::chains::iq`).
//!
//! # Wire shape
//! - **Header:** kind `iq`, `datatype` [`IQ_DATATYPE`] (`cf32_le`), `sample_rate_hz` the DDC's
//!   output rate, `center_hz`/`bandwidth_hz` the requested channel, `emitter_id` when the target
//!   was an emitter.
//! - **Data records** (type 1): `payload` `re, im` `f32` pairs LE, one baseband sample pair each;
//!   `sample_index` counts channel samples from 0 at the stream start; `t` is the time of the
//!   first sample. A gap (retune off-window recovery, lost ring samples) is flagged
//!   `DISCONTINUITY`.

use std::str::FromStr;

use hk_model::EmitterId;

use crate::ondemand::{OpenRefusal, OpenRequest};

/// IQ payload datatype: complex `f32` little-endian (re, im), stream-contract §12.1.
pub const IQ_DATATYPE: &str = "cf32_le";

/// Widest requested band, Hz: bounds the DDC's filter cost and a consumer's network throughput
/// (`cf32_le` at the default ≥ 2× decimation is up to ~32 MB/s at this ceiling). Generous enough
/// for a wideband digital channel (DAB ~1.5 MHz, ADS-B 2 MHz) while staying well inside a single
/// on-demand chain's admitted share of the run's CPU budget (T-071).
pub const MAX_IQ_SPAN_HZ: f64 = 2_000_000.0;

/// What to stream. There is no mode or parameter field: raw IQ is never demodulated.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum IqTarget {
    /// An inventory emitter (`emitter=<id>`): its measured centre and bandwidth.
    Emitter(EmitterId),
    /// A selected frequency extent (`f_lo=<Hz>&f_hi=<Hz>`).
    Range {
        /// Lower edge, Hz.
        f_lo_hz: f64,
        /// Upper edge, Hz.
        f_hi_hz: f64,
    },
}

impl IqTarget {
    /// Parses an `open/iq` request. Any other parameter (`mode`, `detection`, …) is refused:
    /// there is nothing to estimate or demodulate.
    pub fn from_request(req: &OpenRequest) -> Result<Self, OpenRefusal> {
        let bad = |why: &str| OpenRefusal::new(400, "bad-request", why);
        for (k, _) in &req.params {
            if !matches!(k.as_str(), "emitter" | "f_lo" | "f_hi") {
                return Err(bad(&format!(
                    "unknown parameter {k:?}: open/iq takes emitter, or f_lo and f_hi (raw \
                     channelised IQ is never demodulated: no mode or parameter)"
                )));
            }
        }
        if let Some(id) = req.param("emitter") {
            return EmitterId::from_str(id)
                .map(Self::Emitter)
                .map_err(|_| bad("emitter is not an id"));
        }
        let hz = |name: &str| {
            req.param(name)
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v >= 0.0)
        };
        match (hz("f_lo"), hz("f_hi")) {
            (Some(f_lo_hz), Some(f_hi_hz)) if f_hi_hz > f_lo_hz => {
                if f_hi_hz - f_lo_hz > MAX_IQ_SPAN_HZ {
                    return Err(bad(&format!(
                        "selection wider than {} MHz",
                        MAX_IQ_SPAN_HZ / 1e6
                    )));
                }
                Ok(Self::Range { f_lo_hz, f_hi_hz })
            }
            (Some(_), Some(_)) => Err(bad("need f_lo < f_hi")),
            _ => Err(bad("need emitter, or f_lo and f_hi (Hz)")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn iq_targets_parse_and_reject_mode_and_detection() {
        let id = EmitterId::new();
        assert_eq!(
            IqTarget::from_request(&req(&[("emitter", &id.to_string())])).unwrap(),
            IqTarget::Emitter(id)
        );
        assert_eq!(
            IqTarget::from_request(&req(&[("f_lo", "101.2e6"), ("f_hi", "101.4e6")])).unwrap(),
            IqTarget::Range {
                f_lo_hz: 101.2e6,
                f_hi_hz: 101.4e6
            }
        );
        for bad in [
            req(&[("f_lo", "2"), ("f_hi", "1")]),
            req(&[("emitter", "nope")]),
            req(&[("f_lo", "0"), ("f_hi", "3e6")]), // wider than MAX_IQ_SPAN_HZ
            req(&[("f_lo", "1"), ("f_hi", "2"), ("mode", "am")]),
            req(&[("detection", "d1")]),
            req(&[]),
        ] {
            let e = IqTarget::from_request(&bad).unwrap_err();
            assert_eq!(e.status, 400, "{bad:?}");
        }
    }
}
