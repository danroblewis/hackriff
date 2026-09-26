//! What the control-channel hunt decided about each channel it looked at (T-977).
//!
//! Before this, `/api/status`'s `chains.cc_*` counters were the *only* trace a hunt left when it
//! rejected something: `cc_demods 12, cc_confirmed 0` says twelve channels were demodulated and
//! none was a control channel, and says nothing at all about **which** twelve or **why** each
//! lost. A P25 C4FM emission the run had already detected blind could sit in the inventory reading
//! `family: unknown`, `resolution: not-searched` while the chain demodulated its channel and threw
//! the answer away in a `debug_enabled()` `eprintln!`.
//!
//! Two things fix that and they are different objects, deliberately:
//!
//! 1. **The durable half** is an `emitter_synthesis` row, written by [`crate::synth::attach`] for a
//!    rejected candidate exactly as it already was for a confirmed one. That is what moves the
//!    emitter off `resolution: not-searched` — a stored row *is* a finished search (ADR-0021
//!    §7A.4) — and it is what carries the blind level measurement (four-level FM at 4800 Bd) onto
//!    the row.
//! 2. **The ephemeral half** is this module: the last pass's channel list, in memory, with the
//!    verdict on each. It is the answer to "which channels did the occupancy sweep choose, and why
//!    was each rejected", including the channels that never reached a demodulation because the
//!    admission cap refused them — those have no emitter and no synthesis row, so nothing durable
//!    could hold them. `GET /api/trunking/cc-candidates` serves it.
//!
//! **One pass, not an accumulator.** The log holds the most recent completed pass and nothing
//! else: a hunt runs every `period_s` forever, so anything that grew per pass would grow without
//! bound on the capture-adjacent chain thread. A caller that wants history reads the synthesis
//! rows, which are durable by construction.

use std::sync::{Mutex, PoisonError};

use hk_model::Timestamp;

/// Why a channel the hunt looked at is or is not a control channel.
///
/// Closed, and each variant is a *measurement* rather than a shrug: the difference between "the
/// cap refused it" and "it demodulated and nothing framed" is the difference between a budget and
/// a finding, and a reader must not have to infer which happened.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CcOutcome {
    /// Frame sync **and** CRC-valid blocks: a control channel ([`hk_detect::trunk::CcConfirmer`]).
    Confirmed,
    /// A framing's frame sync was seen at the expected spacing, but too few CRC-valid blocks to
    /// confirm. The air interface is recognised and the channel is not a control channel — on P25
    /// that is exactly what a voice or data channel looks like, since LDU/HDU frames carry the
    /// same 48-bit sync as a TSBK frame.
    SyncWithoutCheck,
    /// Demodulated, and no framing in the catalogue found sync.
    NoSync,
    /// The down-conversion or the symbol recovery produced nothing to scan.
    NotDemodulated,
    /// Candidacy held, but the per-pass demodulation cap was already spent on higher-occupancy
    /// channels. **Nothing was measured about this channel beyond its occupancy**, which is why it
    /// is its own outcome rather than folded into `NoSync`.
    AdmissionRefused,
}

impl CcOutcome {
    /// The wire word. Stable: a client filters on it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::SyncWithoutCheck => "sync-without-check",
            Self::NoSync => "no-sync",
            Self::NotDemodulated => "not-demodulated",
            Self::AdmissionRefused => "admission-refused",
        }
    }
}

/// How a channel came to be looked at.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CcCandidacy {
    /// Frequency-channel occupancy over the pass's window reached [`MIN_CC_FCO`] — the C23 test,
    /// and the only one before T-977.
    ///
    /// [`MIN_CC_FCO`]: hk_detect::trunk::MIN_CC_FCO
    Occupancy,
    /// Blind detection already has an emitter on this raster channel. Its occupancy may be far
    /// below the candidacy floor — an intermittent burst train is not a control channel and never
    /// will be — but the run has already decided there is an emission here, so the demodulation is
    /// spent on answering *what it is* rather than on hunting a control channel.
    ///
    /// Not a band-plan lookup: the emitter came from blind detection, and the channel it maps to
    /// is the receiver's own fitted raster.
    DetectedEmitter,
}

impl CcCandidacy {
    /// The wire word.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Occupancy => "occupancy",
            Self::DetectedEmitter => "detected-emitter",
        }
    }
}

/// What one framing in the catalogue scored on one channel.
#[derive(Clone, Debug, PartialEq)]
pub struct CcFramingVerdict {
    /// `p25-phase1` · `dmr-bs-data` · `nxdn-cac`.
    pub framing: String,
    pub sync_hits: u32,
    pub crc_valid: u32,
    pub crc_checked: u32,
}

/// One channel of one pass, with what the chain decided about it.
#[derive(Clone, Debug, PartialEq)]
pub struct CcChannelVerdict {
    /// Raster index relative to the fitted grid origin; negative below the tuned centre.
    pub k: i64,
    /// Channel centre, RF Hz, on the **fitted** grid — where the hunt actually looked.
    pub center_hz: f64,
    /// The raster width the occupancy was integrated over, Hz.
    pub bandwidth_hz: f64,
    /// Frequency-channel occupancy over the pass's window, 0–1.
    pub fco: f64,
    pub candidacy: CcCandidacy,
    pub outcome: CcOutcome,
    /// One sentence, rendered here so no client has to (ADR-0015 §3.4).
    pub reason: String,
    /// Every framing tried, with what it scored. Empty when nothing was demodulated.
    pub framings: Vec<CcFramingVerdict>,
    /// Discrete FM levels the blind structure measurement found, `None` when it abstained.
    pub levels: Option<u32>,
    /// Measured symbol rate, Bd; `None` when the measurement abstained.
    pub symbol_rate_bd: Option<f64>,
    /// Symbols recovered from the window, 0 when nothing demodulated.
    pub symbols: u64,
    /// The inventory emitter the verdict was filed against, when blind detection had one.
    pub emitter_id: Option<String>,
}

/// One completed pass of the hunt.
#[derive(Clone, Debug, PartialEq)]
pub struct CcPass {
    /// 1-based pass number within this chain's life.
    pub pass: u64,
    /// Start of the window analysed, absolute capture time.
    pub t_start: Timestamp,
    /// End of that window.
    pub t_end: Timestamp,
    /// The device whose window this was — provenance, as every time-varying record carries.
    pub device_id: String,
    /// Where the device said it was tuned, Hz. The only frequency the hunt is given.
    pub tune_center_hz: f64,
    /// The a-priori channel raster, Hz.
    pub raster_hz: f64,
    /// The receiver's own fitted grid offset, Hz: the raster origin is `tune_center_hz + this`.
    pub grid_offset_hz: f64,
    /// Raster channels whose occupancy was measured.
    pub channels_swept: usize,
    /// The channels the pass chose to look at, occupancy-first then detected-emitter, each with
    /// its verdict. Bounded by the node's `max_demods` plus the candidates admission refused.
    pub channels: Vec<CcChannelVerdict>,
}

impl CcPass {
    /// The wire shape `GET /api/trunking/cc-candidates` serves (`docs/api.md`).
    ///
    /// Rendered here rather than in `hk-api` for the same reason `hk_pipeline::vlf`'s reports are:
    /// every number in it is a measurement this crate made, and the API crate must not have to
    /// know what a raster index or a grid offset is to serve one.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "pass": self.pass,
            "t_start": self.t_start.as_unix_nanos() as f64 / 1e9,
            "t_end": self.t_end.as_unix_nanos() as f64 / 1e9,
            "device_id": self.device_id,
            "tune_center_hz": self.tune_center_hz,
            "raster_hz": self.raster_hz,
            "grid_offset_hz": self.grid_offset_hz,
            "channels_swept": self.channels_swept,
            "channels": self.channels.iter().map(CcChannelVerdict::to_json).collect::<Vec<_>>(),
        })
    }
}

impl CcChannelVerdict {
    /// One channel's row of the pass report.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "k": self.k,
            "center_hz": self.center_hz,
            "bandwidth_hz": self.bandwidth_hz,
            "fco": self.fco,
            "candidacy": self.candidacy.as_str(),
            "outcome": self.outcome.as_str(),
            "reason": self.reason,
            "framings": self.framings.iter().map(|f| serde_json::json!({
                "framing": f.framing,
                "sync_hits": f.sync_hits,
                "crc_valid": f.crc_valid,
                "crc_checked": f.crc_checked,
            })).collect::<Vec<_>>(),
            "levels": self.levels,
            "symbol_rate_bd": self.symbol_rate_bd,
            "symbols": self.symbols,
            "emitter_id": self.emitter_id,
        })
    }
}

/// The last completed pass of one control-channel hunt, readable by the API.
///
/// A plain mutex over one `Option`: the write is once per `period_s` (0.5 s of stream time in the
/// built-in hunt) and the read is per HTTP request, so there is no contention to design around,
/// and **no growth** — the previous pass is dropped, not appended to.
#[derive(Debug, Default)]
pub struct CcVerdictLog(Mutex<Option<CcPass>>);

impl CcVerdictLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the recorded pass.
    pub fn record(&self, pass: CcPass) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(pass);
    }

    /// The last completed pass, or `None` when no hunt has finished one.
    ///
    /// `None` is a real state and is served as one: a run whose band never triggered a hunt has
    /// **not** looked at any channel, which is not the same fact as a pass that found nothing.
    pub fn last_pass(&self) -> Option<CcPass> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(sec: i64) -> Timestamp {
        Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + sec * 1_000_000_000)
    }

    fn pass(n: u64) -> CcPass {
        CcPass {
            pass: n,
            t_start: t(n as i64),
            t_end: t(n as i64 + 1),
            device_id: "dev".into(),
            tune_center_hz: 852e6,
            raster_hz: 12_500.0,
            grid_offset_hz: -8_200.0,
            channels_swept: 33,
            channels: Vec::new(),
        }
    }

    /// **No pass is not an empty pass.** A hunt that never ran must not read as one that ran and
    /// chose nothing — the decode-side form of the canvas's grey rule.
    #[test]
    fn an_unrun_hunt_reads_as_no_pass_rather_than_an_empty_one() {
        let log = CcVerdictLog::new();
        assert_eq!(log.last_pass(), None);
        log.record(pass(1));
        assert_eq!(log.last_pass().unwrap().channels, Vec::new());
    }

    /// The log holds ONE pass. A hunt runs every `period_s` for the life of the run, so an
    /// accumulator here would grow without bound on a chain thread that gates the ring.
    #[test]
    fn only_the_last_pass_is_kept_however_many_run() {
        let log = CcVerdictLog::new();
        for n in 1..=1000 {
            log.record(pass(n));
        }
        assert_eq!(log.last_pass().unwrap().pass, 1000);
    }
}
