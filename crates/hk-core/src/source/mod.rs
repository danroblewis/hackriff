//! The source abstraction (C01): one trait for every sample source.
//!
//! A [`Source`] has three faces:
//!
//! - **Control:** [`Source::tune`], [`Source::set_sample_rate`], [`Source::set_gains`],
//!   [`Source::start`] and [`Source::stop`]. Sources that cannot honour a control (a file replay)
//!   return [`SourceError::Unsupported`].
//! - **Capabilities:** [`SourceCapabilities`] says what the device can do (frequency ranges,
//!   rates, bits, duplex, TX, gain stages), so the planner can mark what the base device cannot
//!   do, and HackRF Pro or other SDRs fit later without changing the trait.
//! - **Stream:** [`Source::read_block`] fills a reused sample buffer and returns the
//!   [`BlockHeader`] (time, monotonic sample counter, provenance, discontinuity flags).
//!
//! Implementations: [`SigmfReplaySource`] (deterministic file replay, the basis of offline tests)
//! and [`HackRfSource`] (a stub until the licence-isolated driver process lands).
//!
//! TX is not part of this trait. It stays gated (C37).

pub mod format;
pub mod hackrf;
pub mod sigmf_replay;

use std::path::PathBuf;

use hk_model::sigmf::{Datatype, SigmfError};
use num_complex::{Complex, Complex32};

use crate::block::{BlockHeader, SampleBlock};

pub use hackrf::HackRfSource;
pub use sigmf_replay::{Pacing, ReplayOptions, SigmfReplaySource};

/// Errors from a source.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// The source cannot be used in this build or on this host.
    #[error("{source_name} is not available: {reason}")]
    NotAvailable {
        /// Which source.
        source_name: &'static str,
        /// Why, and what to do instead.
        reason: String,
    },
    /// The source does not support this control.
    #[error("{source_name} does not support {operation}")]
    Unsupported {
        /// Which source.
        source_name: &'static str,
        /// The control that was attempted.
        operation: &'static str,
    },
    /// A control value is outside the device's capabilities.
    #[error("{what} {value} is out of range")]
    OutOfRange {
        /// Which quantity.
        what: &'static str,
        /// The rejected value.
        value: f64,
    },
    /// Reading sample data failed.
    #[error("{}: {source}", path.as_ref().map_or("<reader>".into(), |p| p.display().to_string()))]
    Io {
        /// File involved, if known.
        path: Option<PathBuf>,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The SigMF metadata could not be read.
    #[error(transparent)]
    Sigmf(#[from] SigmfError),
    /// The recording is malformed or uses a feature the replay source does not support.
    #[error("invalid recording: {0}")]
    InvalidRecording(String),
    /// The sample datatype cannot be normalised to complex float.
    #[error("unsupported sample datatype {0}")]
    UnsupportedDatatype(Datatype),
}

/// Where a source's samples come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SourceKind {
    /// A live radio.
    Hardware,
    /// A recording played back.
    Replay,
}

/// Transmit/receive arrangement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Duplex {
    /// Receive only.
    ReceiveOnly,
    /// Transmit or receive, not both at once (HackRF One).
    Half,
    /// Simultaneous transmit and receive.
    Full,
}

/// An inclusive frequency range, Hz.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrequencyRange {
    /// Lowest frequency, Hz.
    pub min_hz: f64,
    /// Highest frequency, Hz.
    pub max_hz: f64,
}

impl FrequencyRange {
    /// `hz` lies in the range.
    pub fn contains(&self, hz: f64) -> bool {
        (self.min_hz..=self.max_hz).contains(&hz)
    }
}

/// Supported sample rates.
#[derive(Clone, Debug, PartialEq)]
pub enum SampleRates {
    /// Any rate in an inclusive range, Hz.
    Continuous {
        /// Lowest rate, Hz.
        min_hz: f64,
        /// Highest rate, Hz.
        max_hz: f64,
    },
    /// Only these rates, Hz.
    Discrete(Vec<f64>),
}

impl SampleRates {
    /// `hz` is a supported rate.
    pub fn supports(&self, hz: f64) -> bool {
        match self {
            Self::Continuous { min_hz, max_hz } => (*min_hz..=*max_hz).contains(&hz),
            Self::Discrete(rates) => rates.contains(&hz),
        }
    }
}

/// One adjustable gain stage.
#[derive(Clone, Debug, PartialEq)]
pub struct GainStage {
    /// Stage name, e.g. `"lna"`, `"vga"`, `"amp"`.
    pub name: String,
    /// Minimum gain, dB.
    pub min_db: f64,
    /// Maximum gain, dB.
    pub max_db: f64,
    /// Step, dB (0 for continuous).
    pub step_db: f64,
}

/// What a source can do. The planner uses it to mark what the base device cannot do.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceCapabilities {
    /// Driver/model name, e.g. `"hackrf-one"` or `"sigmf-replay"`.
    pub driver: String,
    /// Live hardware or replay.
    pub kind: SourceKind,
    /// Tunable (or recorded) frequency ranges.
    pub frequency_ranges: Vec<FrequencyRange>,
    /// Supported sample rates.
    pub sample_rates: SampleRates,
    /// ADC resolution, bits.
    pub adc_bits: u8,
    /// The device's native sample format.
    pub native_format: Datatype,
    /// Transmit/receive arrangement.
    pub duplex: Duplex,
    /// The hardware can transmit. TX is still gated elsewhere (C37); this is a descriptor only.
    pub tx_capable: bool,
    /// The source accepts tune/rate/gain controls.
    pub controllable: bool,
    /// Adjustable gain stages.
    pub gain_stages: Vec<GainStage>,
    /// Has a switchable RF amplifier.
    pub rf_amp: bool,
    /// Has an antenna-port bias tee.
    pub bias_tee: bool,
    /// Accepts an external clock reference (e.g. 10 MHz CLKIN).
    pub external_clock: bool,
    /// Provides hardware sample timestamps (HackRF One: no).
    pub hardware_timestamps: bool,
}

impl SourceCapabilities {
    /// HackRF One: 1 MHz–6 GHz, 2–20 Msps, 8-bit, half duplex, TX-capable hardware, LNA 0–40 dB
    /// in 8 dB steps, VGA 0–62 dB in 2 dB steps, RF amp, bias tee, CLKIN, no hardware timestamps
    /// (docs/capabilities/C01).
    pub fn hackrf_one() -> Self {
        Self {
            driver: "hackrf-one".into(),
            kind: SourceKind::Hardware,
            frequency_ranges: vec![FrequencyRange {
                min_hz: 1e6,
                max_hz: 6e9,
            }],
            sample_rates: SampleRates::Continuous {
                min_hz: 2e6,
                max_hz: 20e6,
            },
            adc_bits: 8,
            native_format: Datatype::Ci8,
            duplex: Duplex::Half,
            tx_capable: true,
            controllable: true,
            gain_stages: vec![
                GainStage {
                    name: "lna".into(),
                    min_db: 0.0,
                    max_db: 40.0,
                    step_db: 8.0,
                },
                GainStage {
                    name: "vga".into(),
                    min_db: 0.0,
                    max_db: 62.0,
                    step_db: 2.0,
                },
            ],
            rf_amp: true,
            bias_tee: true,
            external_clock: true,
            hardware_timestamps: false,
        }
    }

    /// `hz` lies in one of the frequency ranges.
    pub fn supports_frequency(&self, hz: f64) -> bool {
        self.frequency_ranges.iter().any(|r| r.contains(hz))
    }
}

/// Front-end gain settings. Mirrors [`hk_model::Tune`]; stages a device lacks are ignored.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gains {
    /// LNA (IF) gain, dB.
    pub lna_db: f64,
    /// VGA (baseband) gain, dB.
    pub vga_db: f64,
    /// RF amplifier on.
    pub amp_on: bool,
}

/// A sample source: control, capabilities and a stream of blocks.
pub trait Source: Send {
    /// What the source can do.
    fn capabilities(&self) -> &SourceCapabilities;

    /// Retunes the centre frequency, Hz. The next block carries [`Discontinuity::RETUNE`].
    ///
    /// [`Discontinuity::RETUNE`]: crate::block::Discontinuity::RETUNE
    fn tune(&mut self, center_hz: f64) -> Result<(), SourceError>;

    /// Changes the sample rate, Hz.
    fn set_sample_rate(&mut self, sample_rate_hz: f64) -> Result<(), SourceError>;

    /// Changes the gains.
    fn set_gains(&mut self, gains: &Gains) -> Result<(), SourceError>;

    /// Starts streaming. Replay sources are ready without it.
    fn start(&mut self) -> Result<(), SourceError>;

    /// Stops streaming; later reads return `Ok(None)`.
    fn stop(&mut self) -> Result<(), SourceError>;

    /// Reads the next block into `samples` (cleared first; no allocation once its capacity covers
    /// the block length) and returns its header, or `Ok(None)` at end of stream.
    ///
    /// Consecutive headers satisfy `next.first_sample() >= prev.first_sample() + prev_len`; a
    /// larger jump is a gap, flagged with [`Discontinuity::GAP`].
    ///
    /// [`Discontinuity::GAP`]: crate::block::Discontinuity::GAP
    fn read_block(
        &mut self,
        samples: &mut Vec<Complex32>,
    ) -> Result<Option<BlockHeader>, SourceError>;

    /// Reads the next block as native signed 8-bit IQ (no normalisation), for sources whose
    /// native format is ci8 (HackRF) or cu8. This feeds a `Complex<i8>` ring at 2 bytes/sample
    /// instead of 8. The header semantics match [`Source::read_block`]. Other sources return
    /// [`SourceError::Unsupported`].
    fn read_block_ci8(
        &mut self,
        samples: &mut Vec<Complex<i8>>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        samples.clear();
        Err(SourceError::Unsupported {
            source_name: "source",
            operation: "read_block_ci8",
        })
    }

    /// Reads the next block into a freshly allocated [`SampleBlock`]. Convenient off the hot
    /// path.
    fn next_block(&mut self) -> Result<Option<SampleBlock>, SourceError> {
        let mut samples = Vec::new();
        Ok(self
            .read_block(&mut samples)?
            .map(|header| SampleBlock { header, samples }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hackrf_one_descriptor() {
        let caps = SourceCapabilities::hackrf_one();
        assert!(caps.supports_frequency(433.92e6));
        assert!(!caps.supports_frequency(500e3));
        assert!(!caps.supports_frequency(10e9));
        assert!(caps.sample_rates.supports(20e6));
        assert!(!caps.sample_rates.supports(40e6));
        assert_eq!(caps.adc_bits, 8);
        assert_eq!(caps.duplex, Duplex::Half);
        assert!(!caps.hardware_timestamps);
    }
}
