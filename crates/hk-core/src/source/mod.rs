//! The source abstraction (C01): every sample source, split into two handles.
//!
//! - **Stream** ([`Source`]), owned by the capture thread: [`Source::read_block`] fills a reused
//!   sample buffer and returns the [`BlockHeader`] (time, monotonic sample counter, provenance,
//!   discontinuity flags).
//! - **Control** ([`SourceControl`], from [`Source::control`]): `Send + Sync` and shared as an
//!   `Arc`, so a scheduler can tune, change rate, gains, baseband filter or bias tee, and start
//!   or stop, from another thread while capture runs, with no lock around the capture loop. A
//!   live source applies commands at the next block boundary: that block carries the matching
//!   [`Discontinuity`] flags (e.g. [`Discontinuity::RETUNE`]) and the new provenance.
//!   [`ControlMailbox`] is the non-blocking hand-over between the two. Sources that cannot honour
//!   a control (a file replay) return [`SourceError::Unsupported`].
//! - **Capabilities:** [`SourceCapabilities`] says what the device can do (frequency ranges,
//!   rates, bits, duplex, TX, gain stages, baseband filters, bias tee), so the planner can mark
//!   what the base device cannot do, and HackRF Pro or other SDRs fit later without changing the
//!   traits.
//!
//! Implementations: [`SigmfReplaySource`] (deterministic file replay, the basis of offline tests)
//! and [`HackRfSource`] (a stub until the driver lands).
//!
//! TX is not part of these traits. It stays gated (C37).

pub mod format;
pub mod hackrf;
pub mod sigmf_replay;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, TryLockError};

use hk_model::Tune;
use hk_model::sigmf::{Datatype, SigmfError};
use num_complex::{Complex, Complex32};

use crate::block::{BlockHeader, SampleBlock};

#[cfg(doc)]
use crate::block::Discontinuity;

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

/// Selectable baseband (anti-alias) filter bandwidths.
#[derive(Clone, Debug, PartialEq)]
pub enum BasebandFilters {
    /// Any bandwidth in an inclusive range, Hz.
    Continuous {
        /// Narrowest, Hz.
        min_hz: f64,
        /// Widest, Hz.
        max_hz: f64,
    },
    /// Only these bandwidths, Hz.
    Discrete(Vec<f64>),
}

impl BasebandFilters {
    /// `hz` is a selectable bandwidth.
    pub fn supports(&self, hz: f64) -> bool {
        match self {
            Self::Continuous { min_hz, max_hz } => (*min_hz..=*max_hz).contains(&hz),
            Self::Discrete(widths) => widths.contains(&hz),
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
    /// The source accepts tune/rate/gain/filter/bias-tee controls.
    pub controllable: bool,
    /// Adjustable gain stages.
    pub gain_stages: Vec<GainStage>,
    /// Has a switchable RF amplifier.
    pub rf_amp: bool,
    /// Selectable baseband filter bandwidths; `None` if not adjustable.
    pub baseband_filter: Option<BasebandFilters>,
    /// Has a switchable antenna-port bias tee.
    pub bias_tee: bool,
    /// Accepts an external clock reference (e.g. 10 MHz CLKIN).
    pub external_clock: bool,
    /// Provides hardware sample timestamps (HackRF One: no).
    pub hardware_timestamps: bool,
    /// RF-path switch frequencies, ascending, Hz: the front end changes filter/mixer path at each,
    /// so the noise floor steps there. Empty: a single path (or unknown, e.g. replay).
    pub rf_path_boundaries_hz: Vec<f64>,
}

/// HackRF One RF-path switch frequencies, Hz: low-pass mixer path below 2170 MHz, mixer bypass
/// from 2170 to 2740 MHz inclusive, high-pass mixer path above 2740 MHz.
///
/// Verified (T-032) against the upstream firmware: `min_bypass_freq = FP_MHZ(2170)` and
/// `max_bypass_freq = FP_MHZ(2740)` with `select_img_reject` choosing high-pass for
/// `f > max_bypass_freq` and bypass for `f >= min_bypass_freq`,
/// <https://github.com/greatscottgadgets/hackrf/blob/7a6b09962402836745d74e133d46a5d95102a232/firmware/common/tuning.c>
/// (lines 36–37, 86–95). The same file sets 2320 / 2580 MHz for Praline (HackRF Pro), which needs
/// its own capabilities. `rf_path` counts a boundary as reached at `f >= b`, so exactly 2740 MHz
/// is labelled high-pass while the firmware still bypasses there (one frequency point). S4 §3.7
/// measured the floor step at ~2.74 GHz.
pub const HACKRF_ONE_RF_PATH_BOUNDARIES_HZ: [f64; 2] = [2170e6, 2740e6];

impl SourceCapabilities {
    /// HackRF One: 1 MHz–6 GHz, 2–20 Msps, 8-bit, half duplex, TX-capable hardware, LNA 0–40 dB
    /// in 8 dB steps, VGA 0–62 dB in 2 dB steps, RF amp, MAX2837 baseband filters (1.75–28 MHz),
    /// bias tee, CLKIN, no hardware timestamps (docs/capabilities/C01).
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
            baseband_filter: Some(BasebandFilters::Discrete(
                [
                    1.75, 2.5, 3.5, 5.0, 5.5, 6.0, 7.0, 8.0, 9.0, 10.0, 12.0, 14.0, 15.0, 20.0,
                    24.0, 28.0,
                ]
                .iter()
                .map(|mhz| mhz * 1e6)
                .collect(),
            )),
            bias_tee: true,
            external_clock: true,
            hardware_timestamps: false,
            rf_path_boundaries_hz: HACKRF_ONE_RF_PATH_BOUNDARIES_HZ.to_vec(),
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

/// Front-end control: `Send + Sync`, held by a scheduler while the capture thread owns the
/// [`Source`] stream. Commands take effect at the next block boundary; that block carries the
/// matching [`Discontinuity`] flags and the new provenance.
pub trait SourceControl: Send + Sync {
    /// What the source can do.
    fn capabilities(&self) -> &SourceCapabilities;

    /// Retunes the centre frequency, Hz. The next block carries [`Discontinuity::RETUNE`].
    fn tune(&self, center_hz: f64) -> Result<(), SourceError>;

    /// Changes the sample rate, Hz. The next block carries [`Discontinuity::RATE_CHANGE`].
    fn set_sample_rate(&self, sample_rate_hz: f64) -> Result<(), SourceError>;

    /// Changes the gains. The next block carries [`Discontinuity::GAIN_CHANGE`].
    fn set_gains(&self, gains: &Gains) -> Result<(), SourceError>;

    /// Selects the baseband filter bandwidth, Hz (see [`SourceCapabilities::baseband_filter`]).
    /// The next block carries [`Discontinuity::PROVENANCE_CHANGE`].
    fn set_baseband_filter(&self, bandwidth_hz: f64) -> Result<(), SourceError>;

    /// Switches the antenna-port bias tee.
    fn set_bias_tee(&self, enabled: bool) -> Result<(), SourceError>;

    /// Starts streaming. Replay sources are ready without it.
    fn start(&self) -> Result<(), SourceError>;

    /// Stops streaming; the stream's later reads return `Ok(None)`.
    fn stop(&self) -> Result<(), SourceError>;
}

/// A sample stream, owned by the capture thread. Its [`SourceControl`] comes from
/// [`Source::control`].
pub trait Source: Send {
    /// What the source can do.
    fn capabilities(&self) -> &SourceCapabilities;

    /// The control handle, shareable across threads.
    fn control(&self) -> Arc<dyn SourceControl>;

    /// Reads the next block into `samples` (cleared first; no allocation once its capacity covers
    /// the block length) and returns its header, or `Ok(None)` at end of stream.
    ///
    /// Consecutive headers satisfy `next.first_sample() >= prev.first_sample() + prev_len`; a
    /// larger jump is a gap, flagged with [`Discontinuity::GAP`].
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

/// Front-end changes waiting for the next block boundary. A later post to the same setting
/// replaces an earlier one.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PendingControl {
    /// New centre frequency, Hz.
    pub center_hz: Option<f64>,
    /// New sample rate, Hz.
    pub sample_rate_hz: Option<f64>,
    /// New gains.
    pub gains: Option<Gains>,
    /// New baseband filter bandwidth, Hz.
    pub baseband_filter_hz: Option<f64>,
    /// New bias-tee state.
    pub bias_tee: Option<bool>,
}

impl PendingControl {
    /// Nothing is pending.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Applies the tuning, rate, gain and filter changes to `tune`. The bias tee has no
    /// Provenance field; a stream tracks it itself.
    pub fn apply_to(&self, tune: &mut Tune) {
        if let Some(hz) = self.center_hz {
            tune.center_hz = hz;
        }
        if let Some(hz) = self.sample_rate_hz {
            tune.sample_rate_hz = hz;
        }
        if let Some(g) = self.gains {
            tune.lna_db = g.lna_db;
            tune.vga_db = g.vga_db;
            tune.amp_on = g.amp_on;
        }
        if let Some(hz) = self.baseband_filter_hz {
            tune.bandwidth_hz = hz;
        }
    }
}

/// Hands control changes from a [`SourceControl`] to the capture thread without ever blocking
/// the capture thread.
#[derive(Debug)]
pub struct ControlMailbox {
    generation: AtomicU64,
    pending: Mutex<PendingControl>,
}

impl Default for ControlMailbox {
    fn default() -> Self {
        Self::new()
    }
}

impl ControlMailbox {
    /// An empty mailbox.
    pub fn new() -> Self {
        let mailbox = Self {
            generation: AtomicU64::new(0),
            pending: Mutex::new(PendingControl::default()),
        };
        // Initialise a lazily allocated OS mutex here, not on the capture thread's first take.
        drop(mailbox.pending.lock());
        mailbox
    }

    /// Control side: records a change (may briefly wait for another control thread).
    pub fn post(&self, change: impl FnOnce(&mut PendingControl)) {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        change(&mut pending);
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Capture side, at a block boundary: takes every change posted since the last take.
    /// `seen` is the caller's generation counter (start at 0). Never blocks: if a control thread
    /// holds the mailbox right now, returns `None` and the change applies at the next boundary.
    pub fn take(&self, seen: &mut u64) -> Option<PendingControl> {
        if self.generation.load(Ordering::SeqCst) == *seen {
            return None;
        }
        let mut pending = match self.pending.try_lock() {
            Ok(pending) => pending,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        // Under the lock, every completed post is included.
        *seen = self.generation.load(Ordering::SeqCst);
        Some(std::mem::take(&mut *pending))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{Discontinuity, ProvenanceHandle};
    use hk_model::{ClockSource, Provenance, TimestampMethod};

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
        assert!(caps.bias_tee);
        let filters = caps.baseband_filter.as_ref().unwrap();
        assert!(filters.supports(1.75e6) && filters.supports(28e6));
        assert!(!filters.supports(4e6));
    }

    #[test]
    fn control_handles_are_shareable() {
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<dyn SourceControl>();
        assert_send_sync::<ControlMailbox>();
    }

    #[test]
    fn mailbox_changes_apply_at_a_block_boundary_as_a_retune() {
        let mailbox = Arc::new(ControlMailbox::new());
        let mut tune = Tune {
            center_hz: 100e6,
            sample_rate_hz: 2e6,
            lna_db: 16.0,
            vga_db: 20.0,
            amp_on: false,
            bandwidth_hz: 1.75e6,
        };
        let record = |tune: &Tune| Provenance {
            device_id: "synthetic:mailbox".into(),
            tune: tune.clone(),
            overload: false,
            quantisation_limited: false,
            temperature_c: None,
            antenna_port: None,
            clock_source: ClockSource::Internal,
            clock_locked: true,
            calibration_state_ref: None,
            spur_mask_ref: None,
            timestamp_method: TimestampMethod::Synthetic,
            timestamp_error_budget_ns: None,
        };
        let mut provenance = ProvenanceHandle::new(record(&tune));
        let mut seen = 0;
        assert_eq!(mailbox.take(&mut seen), None);

        // A control thread posts two changes; the stream applies both at one boundary.
        let control = Arc::clone(&mailbox);
        std::thread::spawn(move || {
            control.post(|p| p.center_hz = Some(433.92e6));
            control.post(|p| {
                p.gains = Some(Gains {
                    lna_db: 24.0,
                    vga_db: 20.0,
                    amp_on: false,
                })
            });
        })
        .join()
        .unwrap();

        let pending = mailbox.take(&mut seen).expect("changes pending");
        pending.apply_to(&mut tune);
        let next = ProvenanceHandle::new(record(&tune));
        let flags = Discontinuity::between(provenance.get(), next.get());
        assert!(flags.contains(Discontinuity::RETUNE | Discontinuity::GAIN_CHANGE));
        assert!(!flags.contains(Discontinuity::RATE_CHANGE));
        provenance = next;
        assert_eq!(provenance.tune.center_hz, 433.92e6);
        assert_eq!(mailbox.take(&mut seen), None, "taken exactly once");
    }
}
