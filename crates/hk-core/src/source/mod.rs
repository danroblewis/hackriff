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
//! Implementations: [`SigmfReplaySource`] (deterministic file replay, the basis of offline tests),
//! [`HackRfSource`] (libhackrf receive, cargo feature `hackrf`; T-037a), [`RtlSdrSource`]
//! (librtlsdr receive, cargo feature `rtlsdr`; T-514) and the [`mock`] SDR device (a SigMF
//! recording behind the device contract, retuned realistically; T-049). Every tunable device
//! passes the [`conformance`] suite. [`accessory`] (T-891) is the accessory-fed source — a VLF/LF
//! receiver into a soundcard, below the HackRF's 1 MHz floor — and its SigMF mock; it is fixed at
//! baseband, so `tune` is `Unsupported` and the tuning checks do not apply.
//!
//! TX is not part of these traits. It stays gated (C37).
//!
//! # Device-local vs shared-air reasoning (T-259, T-302)
//!
//! Multiple SDRs seeing the same airwaves raises a question: which computations are
//! device-specific, and which are about shared air?
//!
//! **Device-local physics** (must read the device) — images, harmonics, intermodulation
//! distortion, noise floor, gain state (LNA/VGA/amp), and bias-tee state (T-325) — are tied to one
//! receive chain. These belong in the provenance object [`BlockHeader::provenance`], which carries
//! `device_id`, antenna port and `bias_tee` (T-302: an artifact is a property of ONE receive
//! chain, and is gated on these fields). When a detection uses device-local physics (e.g. to
//! measure an emission's noise floor rise), it must read the provenance of the samples it
//! examined.
//!
//! Bias tee sits squarely on this side: it is DC the *device* puts on its *own* antenna port, and
//! an active antenna's LNA moves that chain's noise floor and gain structure. A source reports it
//! as three states (`unknown`/`off`/`on`) and never infers it — **nothing here ever enables a bias
//! tee on its own**; only an explicit [`SourceControl::set_bias_tee`] does, and the stream then
//! reports what it was told.
//!
//! **Shared-air reasoning** (must NOT read the device) — deduplication, signal clustering,
//! and identity across time — operates on the measurements and conclusions, not on which device
//! made them. At a seam where two non-coherently stitched devices sample overlapping bands, one
//! real emitter may appear as two detections from the two devices. The dedup/clustering layer
//! must not assume one-to-one correspondence to devices; two detections from different devices
//! with matching time and frequency should be merged into one emitter, not kept apart.
//!
//! # Device contract (T-037a; for drivers and the T-049 mock SDR)
//!
//! Every receiver (HackRF One, later SoapySDR devices, a mock replaying SigMF) implements the same
//! generic contract; device specifics stay in its own module.
//!
//! - **Open:** a [`SourceDriver`] (`name`, `available`, `capabilities`) opens a [`Source`] from an
//!   [`OpenRequest`]: device selector, centre, rate, named gains, baseband filter, bias tee.
//!   Values are validated against the driver's [`SourceCapabilities`]; the source starts
//!   streaming on the first read (or [`SourceControl::start`]).
//! - **Capabilities:** frequency ranges, sample rates, ADC bits, native format, duplex, named
//!   [`GainStage`]s (an on/off amplifier is a stage whose one step spans its range, see
//!   [`GainStage::quantise`]), optional baseband filters, optional bias tee, external clock,
//!   `hardware_timestamps`, RF-path boundaries. Optional sweep mode:
//!   [`SourceControl::sweep_capability`].
//! - **Control** ([`SourceControl`], `Send + Sync`): `tune`, `set_sample_rate`,
//!   [`SourceControl::set_gain`] (one named stage, quantised by its [`GainStage`]),
//!   `set_baseband_filter`, `set_bias_tee` (optional: [`SourceError::Unsupported`] without the
//!   capability), `start_sweep`/`stop_sweep` (optional), `start`, `stop`. Out-of-range values
//!   return [`SourceError::OutOfRange`] at the call. Accepted changes apply at the next block
//!   boundary: that block carries the new provenance and [`Discontinuity`] flags; samples taken
//!   before the change settles are dropped and reported as `GAP` + `dropped_before`, never
//!   delivered under the new provenance. [`SourceControl::set_gains`] is the legacy
//!   LNA/VGA/amp convenience of the v1 scheduler.
//! - **Stream** ([`Source`]): blocks of contiguous samples ([`Source::read_block_ci8`] native,
//!   [`Source::read_block`] normalised). The first block carries `STREAM_START`; the sample counter
//!   is monotonic and counts lost samples, so every overrun or discard is a `GAP` with an exact
//!   `dropped_before`. `Ok(None)` after `stop` or at the end of a finite source.
//! - **Timestamps:** each block's `host_time` and its provenance `timestamp_method`
//!   (`host-arrival` for USB radios, `synthetic` for generated data, `external-reference` with a
//!   disciplined clock); `SourceCapabilities::hardware_timestamps` says whether the device stamps.
//!   **Times never go backwards** — the conformance suite's `timestamps` check — and that promise
//!   is kept for a *composition* of sources too (T-474): a `--loop` replay re-opens this interface
//!   on the same recording, so each pass starts at the recording's own datetime again, and
//!   `hk_pipeline::capture`'s `Axis` splices the pass onto one monotone capture-time axis before
//!   anything downstream sees it. So a source replays its own timestamps; the *stream* a consumer
//!   reads is monotone. No reader has to defend against a clock that rewinds.
//! - **Overruns and drops:** [`SourceControl::stats`] ([`SourceStats`]: blocks, samples, overrun
//!   events, dropped and discarded samples), consistent with the stream's gaps.
//! - **Identity:** [`SourceControl::device_info`] ([`DeviceInfo`]: provenance `device_id`, SigMF
//!   `core:hw` description).
//! - **Backpressure:** [`Source::pausable`] is `false` for anything that streams in real time
//!   (a radio, or a mock emulating one); lossless pipelines refuse such sources.

pub mod accessory;
pub mod conformance;
pub mod format;
pub mod hackrf;
pub mod mock;
pub mod rtlsdr;
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

pub use accessory::{
    ACCESSORY_MOCK_DRIVER, AccessoryControl, AccessoryDescriptor, AccessoryKind,
    AccessoryMockDriver, AccessoryMockOptions, AccessorySource, AudioInput, AudioRead,
    accessory_capabilities, write_real_sigmf,
};
pub use hackrf::{
    HackRfConfig, HackRfControl, HackRfDeviceInfo, HackRfDriver, HackRfSource, HackRfStats,
};
pub use mock::{
    Coverage, MockClock, MockEnd, MockFault, MockOptions, MockSdrControl, MockSdrDriver,
    MockSdrSource, MockStats, Recording,
};
pub use rtlsdr::{
    R820T_GAINS_DB, R820T_MAX_HZ, R820T_MIN_HZ, R820T_RATES_HZ, R820T_TUNING_STEP_HZ, RtlSdrConfig,
    RtlSdrControl, RtlSdrDeviceInfo, RtlSdrDriver, RtlSdrSource, RtlSdrStats,
};
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
    /// The device driver reported an error.
    #[error("{source_name}: {operation} failed: {message}")]
    Device {
        /// Which source.
        source_name: &'static str,
        /// The driver call that failed.
        operation: &'static str,
        /// The driver's error.
        message: String,
    },
    /// The device could not be opened because **another process holds it** — or, where the
    /// driver cannot tell the two apart, because this user is not permitted to open it (T-892).
    ///
    /// Generic across drivers: each driver decides from its own error codes which
    /// [`InUseCertainty`] it can honestly claim, and the message names the device and never
    /// sends the user to fix permissions when the likelier cause is a second program. Distinct
    /// from [`SourceError::Device`], which is every other driver failure.
    #[error("{}", in_use_message(source_name, device, *certainty, driver_message))]
    DeviceInUse {
        /// Which source.
        source_name: &'static str,
        /// The device, as specifically as the driver can name it (a serial where known).
        device: String,
        /// Whether the driver could rule out a permissions failure.
        certainty: InUseCertainty,
        /// The driver's own error, verbatim, for the record.
        driver_message: String,
    },
}

/// How sure a driver is that a refused open means "another process holds the device" (T-892).
///
/// Two states, and the weaker one is not rounded up: some drivers (libhackrf through libusb on
/// macOS) report a device held elsewhere and a user without USB permissions with the **same**
/// code, and saying "in use" there would be a guess — `Unknown` is not `Off`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InUseCertainty {
    /// The driver reported the device busy: another process (or another handle) holds it.
    InUse,
    /// The driver's code covers both "held by another process" and "not permitted".
    InUseOrNotPermitted,
}

fn in_use_message(
    source_name: &str,
    device: &str,
    certainty: InUseCertainty,
    driver_message: &str,
) -> String {
    match certainty {
        InUseCertainty::InUse => format!(
            "{source_name} {device} is in use by another process; close the program holding it \
             (another `hk serve`, hackrf_transfer, an SDR app) and try again \
             [driver: {driver_message}]"
        ),
        InUseCertainty::InUseOrNotPermitted => format!(
            "{source_name} {device} is in use by another process, or not permitted: the driver \
             cannot tell which. Most often another program holds it (another `hk serve`, \
             hackrf_transfer, an SDR app) — close it and try again; if nothing else is using \
             the device, check this user's USB permissions (udev rules on Linux) \
             [driver: {driver_message}]"
        ),
    }
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

/// The granularity of the device's centre frequency: the smallest change in a commanded centre
/// that puts the front end somewhere else (T-341).
///
/// **Three states, and `Unknown` is never "any frequency".** This is the third axis of the
/// achievable `(centre, span)` grid — [`SourceCapabilities::frequency_ranges`] bound the centre,
/// [`SourceCapabilities::sample_rates`] bound the span, and this says which centres *between* the
/// bounds exist. Navigation snaps to that grid (the user's invariant, CLAUDE.md "Navigation is
/// discretized to achievable capture states"), so a source that cannot state a step must say so
/// rather than have one invented for it: reading "nothing said" as "1 Hz" would offer the user
/// centres the radio cannot reach, which is the lie this whole rule exists to prevent. The
/// three-state shape is [`hk_model::BiasTee`]'s, for the same reason.
///
/// There is deliberately no `f64` conversion. [`TuningStep::step_hz`] returns `Option<f64>`, so
/// every site that wants a number handles the unknown case.
///
/// # Which way to err
///
/// A step **coarser** than the truth offers fewer centres, all of them reachable — the view loses
/// choice, not honesty. A step **finer** than the truth offers centres that do not exist, and the
/// radio silently lands somewhere else while the axis claims otherwise. So a driver that is unsure
/// declares the coarser figure, or `Unknown`; it never rounds down to 1 Hz because the API happens
/// to take integer hertz. (The same direction as T-334's resolution ladder: err coarser, because
/// coarse repeats a measured value while fine invents one.)
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum TuningStep {
    /// The source cannot say: a replayed recording (the file records the centre it was made at,
    /// never the synthesiser grid of the device that made it), or a driver that does not know its
    /// front end's granularity. **Never read this as 1 Hz, and never as "continuous"** — the grid
    /// is unknown, so nothing may claim a centre is achievable. This is the default, so a source
    /// added without thinking about it reports "nothing said" rather than a fiction.
    #[default]
    Unknown,
    /// Achievable centres are `k · step_hz` for integer `k`, within the frequency ranges.
    ///
    /// The device lands within `step_hz / 2` of the requested centre — that residual *is* what a
    /// step means, and it is bounded by the step by construction, so nothing downstream may read
    /// a grid point as exact beyond it.
    Uniform {
        /// Grid spacing, Hz, anchored at 0 Hz. Positive and finite.
        step_hz: f64,
    },
}

impl TuningStep {
    /// The grid spacing in Hz, or `None` when the source cannot say.
    ///
    /// `None` means **unknown, not 1 Hz and not continuous**: do not `unwrap_or(1.0)` it into a
    /// claim that any centre is reachable.
    pub const fn step_hz(self) -> Option<f64> {
        match self {
            Self::Unknown => None,
            Self::Uniform { step_hz } => Some(step_hz),
        }
    }

    /// The state as stored/wire text (`unknown`, `uniform`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Uniform { .. } => "uniform",
        }
    }

    /// The nearest achievable centre to `hz`, or `None` when the step is unknown (in which case
    /// **nothing may be snapped**: an unknown grid cannot name a nearest point).
    ///
    /// Ties round away from zero, as [`f64::round`] does; the choice is immaterial because both
    /// neighbours are within `step_hz / 2`.
    pub fn snap_hz(self, hz: f64) -> Option<f64> {
        let step = self.step_hz()?;
        if !step.is_finite() || step <= 0.0 || !hz.is_finite() {
            return None;
        }
        Some((hz / step).round() * step)
    }
}

/// HackRF One centre-frequency granularity, Hz: `30 MHz / 2^20` = 28.6102294921875 Hz.
///
/// Derived (T-341) from the upstream firmware's `max2837_set_frequency`, which is where the fine
/// half of a HackRF tune happens — the RFFC5072 mixer LO moves on a coarse grid and the MAX2837 IF
/// synthesiser covers the remainder, so the RF granularity is the MAX2837's:
/// <https://github.com/greatscottgadgets/hackrf/blob/7a6b09962402836745d74e133d46a5d95102a232/firmware/common/max2837.c>.
/// That code targets a VCO at `4/3 · f` against `PFD_FREQ_HZ = 40 MHz` with a **20-bit fractional
/// divider**, so the VCO grid is `40 MHz / 2^20` and the RF grid is `3/4` of it: `30 MHz / 2^20`.
///
/// **Why not 1 Hz.** `hackrf_set_freq` takes an integer number of hertz and this driver already
/// truncates to it ([`HackRfDevice::set_freq`]), so a 1 Hz *command* is accepted — but 28 of every
/// 29 such commands land the LO on the same synthesiser point as their neighbour. Declaring 1 Hz
/// would put 28 centres on the navigation grid that do not exist, which is exactly the front-end
/// detail the UI may not imply. Declaring the synthesiser grid errs on the safe side: every
/// declared centre is one the hardware distinguishes, and the tune lands within half a step
/// (≤ 14.31 Hz) of it.
///
/// **Unverified by measurement.** The figure is read from the firmware source cited above, not
/// from a counter on the bench; the error it bounds (≤ 14.31 Hz) is far below the finest FFT bin
/// the UI draws, so a bench check is worth doing but gates nothing.
pub const HACKRF_ONE_TUNING_STEP_HZ: f64 = 30.0e6 / (1u32 << 20) as f64;

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

impl GainStage {
    /// `db` snapped to this stage, or `None` outside `min_db..=max_db` (or not finite). A stepped
    /// stage rounds down to a step; an on/off stage (one step spanning the range, e.g. an RF
    /// amplifier) takes `max_db` from the midpoint up, else `min_db`; `step_db` 0 is continuous.
    pub fn quantise(&self, db: f64) -> Option<f64> {
        if !db.is_finite() || db < self.min_db - 1e-9 || db > self.max_db + 1e-9 {
            return None;
        }
        let span = self.max_db - self.min_db;
        let q = if self.step_db <= 0.0 {
            db
        } else if self.step_db >= span {
            if db >= self.min_db + span / 2.0 {
                self.max_db
            } else {
                self.min_db
            }
        } else {
            self.min_db + ((db - self.min_db) / self.step_db + 1e-9).floor() * self.step_db
        };
        Some(q.clamp(self.min_db, self.max_db))
    }

    /// The stage's usable range, dB.
    pub fn span_db(&self) -> f64 {
        (self.max_db - self.min_db).max(0.0)
    }

    /// The stage's one step spans its whole range: an on/off control (a HackRF RF amplifier), not
    /// something a search can walk through. [`crate::gain`] engages such a stage last.
    pub fn is_binary(&self) -> bool {
        self.span_db() > 0.0 && self.step_db >= self.span_db() - 1e-9
    }

    /// The smallest gain change this stage can make, dB: its own step, or — for a stage declared
    /// continuous because [`GainStage`] cannot express its real step table (the RTL's 29 uneven
    /// steps, see [`rtlsdr`]) — a 32nd of its range, which the driver then snaps to whatever the
    /// device actually has. Never 0, so a search over it terminates.
    pub fn fine_step_db(&self) -> f64 {
        if self.step_db > 0.0 {
            self.step_db.min(self.span_db())
        } else {
            (self.span_db() / 32.0).max(f64::MIN_POSITIVE)
        }
    }
}

/// One named gain value (a [`GainStage`] name and dB).
#[derive(Clone, Debug, PartialEq)]
pub struct NamedGain {
    /// Stage name, from [`SourceCapabilities::gain_stages`].
    pub stage: String,
    /// Gain, dB.
    pub db: f64,
}

impl NamedGain {
    /// `stage` at `db`.
    pub fn new(stage: impl Into<String>, db: f64) -> Self {
        Self {
            stage: stage.into(),
            db,
        }
    }
}

/// Stream counters for overrun and drop reporting (see the module's device contract).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SourceStats {
    /// Blocks delivered.
    pub blocks: u64,
    /// Samples delivered.
    pub samples: u64,
    /// Overrun events: device or driver buffers lost before the stream read them.
    pub overruns: u64,
    /// Samples lost to overruns (each run is a `GAP` in the stream).
    pub dropped_samples: u64,
    /// Samples discarded on purpose while a control change settled (also `GAP`s).
    pub discarded_samples: u64,
}

/// A source's identity for provenance and SigMF.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Driver name, e.g. `hackrf-one`.
    pub driver: String,
    /// Provenance `device_id`, e.g. `hackrf:<serial>`.
    pub device_id: String,
    /// SigMF `core:hw`: model, serial, firmware, antenna.
    pub hw: String,
}

/// What an optional hardware sweep mode can do.
#[derive(Clone, Debug, PartialEq)]
pub struct SweepCapability {
    /// Sweepable range.
    pub frequency_range: FrequencyRange,
    /// Sample rates a sweep can use.
    pub sample_rates: SampleRates,
    /// Fastest retune rate, hops per second, if known.
    pub max_hops_per_s: Option<f64>,
}

/// A sweep request: hop from `lo_hz` to `hi_hz` in `step_hz` steps, `samples_per_hop` each.
/// Each hop's first block carries `RETUNE`; settle samples are discarded as for `tune`.
#[derive(Clone, Debug, PartialEq)]
pub struct SweepPlan {
    /// Lowest hop centre, Hz.
    pub lo_hz: f64,
    /// Highest hop centre, Hz.
    pub hi_hz: f64,
    /// Hop step, Hz.
    pub step_hz: f64,
    /// Sample rate during the sweep, Hz.
    pub sample_rate_hz: f64,
    /// Samples delivered per hop.
    pub samples_per_hop: usize,
}

/// A generic open request (see [`SourceDriver`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OpenRequest {
    /// Device selector (serial number, index or URI); `None` opens the first device.
    pub device: Option<String>,
    /// Centre frequency, Hz.
    pub center_hz: f64,
    /// Sample rate, Hz.
    pub sample_rate_hz: f64,
    /// Named gains; stages not listed keep the driver's defaults.
    pub gains: Vec<NamedGain>,
    /// Baseband filter bandwidth, Hz; `None` lets the driver choose.
    pub baseband_filter_hz: Option<f64>,
    /// Antenna-port bias tee (only when the capability exists).
    pub bias_tee: bool,
}

/// Opens sources of one kind of device (HackRF One, the T-049 mock, SoapySDR later).
///
/// # N-shaped trait: no singleton
///
/// `open(&self, request: &OpenRequest) -> Result<Box<dyn Source>, SourceError>` is **deliberately
/// N-shaped**: `&self` (not `&mut self`), a fresh `Box<dyn Source>` per call, with
/// `OpenRequest.device` selecting by serial, index, or URI. There is no singleton driver
/// registry and no lock; the trait is ready for multiple concurrent SDRs. Each call opens one
/// independent stream, so a caller opening two sources from the same driver or concurrently
/// gets two separate stream handles.
///
/// The single-device assumption lived not in this trait, but in its **consumers**. Since T-510
/// the pipeline composes N of them: `hk_pipeline::Pipeline::start_multi` spawns one
/// `{source, ring, capture thread, history reader, detector, IQ ring, coverage observer}` set per
/// front end into the run's shared, already-per-device stores, and each one's measurements carry
/// its own provenance `device_id`. What is still one-per-run is the **primary's** control plane —
/// `hk_pipeline::SourceSlot`, `hk_pipeline::SwitchableControl`,
/// `hk_pipeline::config::PipelineConfig::device_id`, re-plumbing, chains and the scheduler — and
/// `hk_cli`'s `driver_for` (one driver + one device per invocation, T-512). A further front end
/// collects passively and widens the coverage available to display; it never adds a view window.
/// This trait's contract is unchanged, as ADR-0005 Consequences anticipated ("Multiple HackRFs
/// (a survey radio + a dwell radio) are a later option the policy can grow into").
///
/// One caveat that is not software: 20 Msps of ci8 is ~40 MB/s and saturates a USB 2.0
/// controller, so concurrent front ends at full rate want **separate USB controllers**.
pub trait SourceDriver: Send + Sync {
    /// Driver name, e.g. `hackrf`.
    fn name(&self) -> &'static str;
    /// The driver is usable in this build (e.g. its library is linked).
    fn available(&self) -> bool;
    /// What sources of this driver can do.
    fn capabilities(&self) -> SourceCapabilities;
    /// Opens a source configured by `request`.
    fn open(&self, request: &OpenRequest) -> Result<Box<dyn Source>, SourceError>;
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
    /// Centre-frequency granularity (T-341): the third axis of the achievable `(centre, span)`
    /// grid the navigation surface snaps to. [`TuningStep::Unknown`] when the source cannot say —
    /// never read as 1 Hz.
    pub tuning_step: TuningStep,
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
    /// HackRF One: 1 MHz–6 GHz, 2–20 Msps, centres on a
    /// [`HACKRF_ONE_TUNING_STEP_HZ`] grid, 8-bit, half duplex, TX-capable hardware, LNA 0–40 dB
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
            tuning_step: TuningStep::Uniform {
                step_hz: HACKRF_ONE_TUNING_STEP_HZ,
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
                // The RF amplifier as an on/off stage (nominal ~11 dB; S4 measured ~15 dB at
                // 98 MHz, so calibration never assumes the nominal value).
                GainStage {
                    name: "amp".into(),
                    min_db: 0.0,
                    max_db: 11.0,
                    step_db: 11.0,
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

    /// RTL-SDR with a Rafael Micro R820T tuner (NooElec NESDR Nano 3 and friends; T-514):
    /// 25 MHz–1.75 GHz, the [`R820T_RATES_HZ`] rates topping out at 2.4 Msps, centres on a
    /// [`R820T_TUNING_STEP_HZ`] grid, 8-bit **unsigned** (`cu8`) samples, receive only, one
    /// combined gain stage `lna` over 0–49.6 dB in 29 non-uniform steps, **no** RF amplifier,
    /// **no** selectable baseband filter, **no** bias tee, **no** external clock, no hardware
    /// timestamps.
    ///
    /// Every "no" above is a real limit of this front end, declared so nothing upstream offers
    /// the user a control or a span the hardware cannot deliver. The gain stage is declared
    /// continuous because [`GainStage`] cannot express 29 uneven steps; the driver snaps each
    /// request to the device's own table and records what the device reports back, so
    /// provenance never claims a gain the tuner did not take (see [`rtlsdr`] for the detail, and
    /// [`R820T_GAINS_DB`] for the table).
    pub fn rtl_sdr_r820t() -> Self {
        Self {
            driver: "rtl-sdr".into(),
            kind: SourceKind::Hardware,
            frequency_ranges: vec![FrequencyRange {
                min_hz: R820T_MIN_HZ,
                max_hz: R820T_MAX_HZ,
            }],
            sample_rates: SampleRates::Discrete(R820T_RATES_HZ.to_vec()),
            tuning_step: TuningStep::Uniform {
                step_hz: R820T_TUNING_STEP_HZ,
            },
            adc_bits: 8,
            native_format: Datatype::Cu8,
            duplex: Duplex::ReceiveOnly,
            tx_capable: false,
            controllable: true,
            gain_stages: vec![GainStage {
                name: "lna".into(),
                min_db: 0.0,
                max_db: 49.6,
                step_db: 0.0,
            }],
            rf_amp: false,
            baseband_filter: None,
            bias_tee: false,
            external_clock: false,
            hardware_timestamps: false,
            rf_path_boundaries_hz: Vec::new(),
        }
    }

    /// `hz` lies in one of the frequency ranges.
    pub fn supports_frequency(&self, hz: f64) -> bool {
        self.frequency_ranges.iter().any(|r| r.contains(hz))
    }

    /// The frequency range containing `hz`, else the nearest one; `None` when there are none.
    pub fn nearest_range(&self, hz: f64) -> Option<&FrequencyRange> {
        let distance = |r: &FrequencyRange| {
            if r.contains(hz) {
                0.0
            } else if hz < r.min_hz {
                r.min_hz - hz
            } else {
                hz - r.max_hz
            }
        };
        self.frequency_ranges
            .iter()
            .min_by(|a, b| distance(a).total_cmp(&distance(b)))
    }

    /// The nearest **achievable** centre to `hz` (T-341): the closest point of the
    /// [`SourceCapabilities::tuning_step`] grid that lies inside a frequency range.
    ///
    /// `None` when the source cannot state a step ([`TuningStep::Unknown`]) or has no frequency
    /// ranges — an unknown grid has no nearest point, and answering `hz` unchanged would claim
    /// the device can sit exactly there. This is the backend's authority on "which states are
    /// realizable"; the view snaps against it rather than deciding for itself.
    ///
    /// At a band edge the nearest grid point can fall just outside the range (`6 GHz` is not
    /// generally a multiple of the step), so the result is walked one step **inward** — inside the
    /// range and reachable, never outside it and merely close.
    pub fn snap_center_hz(&self, hz: f64) -> Option<f64> {
        let step = self.tuning_step.step_hz()?;
        if !step.is_finite() || step <= 0.0 || !hz.is_finite() {
            return None;
        }
        let range = self.nearest_range(hz)?;
        let mut v = (hz / step).round() * step;
        if v < range.min_hz {
            v = (range.min_hz / step).ceil() * step;
        }
        if v > range.max_hz {
            v = (range.max_hz / step).floor() * step;
        }
        range.contains(v).then_some(v)
    }

    /// The widest span (instantaneous bandwidth) this source can deliver as **live IQ**, Hz:
    /// its highest sample rate. A view wider than this was never inside one capture window, so it
    /// is survey/spectrum-history overview rather than live detail (T-341). `None` when no rate is
    /// reported.
    pub fn max_live_span_hz(&self) -> Option<f64> {
        match &self.sample_rates {
            SampleRates::Continuous { max_hz, .. } => Some(*max_hz),
            SampleRates::Discrete(v) => v
                .iter()
                .copied()
                .fold(None, |m: Option<f64>, r| Some(m.map_or(r, |m| m.max(r)))),
        }
    }

    /// The nearest achievable span to `hz`: the sample rate closest to it, since the span of a
    /// live window **is** the sample rate. `None` when no rate is reported.
    ///
    /// A continuous range answers `hz` clamped into it; a discrete list answers its nearest entry.
    pub fn snap_span_hz(&self, hz: f64) -> Option<f64> {
        if !hz.is_finite() {
            return None;
        }
        match &self.sample_rates {
            SampleRates::Continuous { min_hz, max_hz } => {
                (max_hz >= min_hz).then(|| hz.clamp(*min_hz, *max_hz))
            }
            SampleRates::Discrete(v) => v
                .iter()
                .copied()
                .filter(|r| r.is_finite())
                .min_by(|a, b| (a - hz).abs().total_cmp(&(b - hz).abs())),
        }
    }

    /// The gain stage named `name`.
    pub fn gain_stage(&self, name: &str) -> Option<&GainStage> {
        self.gain_stages.iter().find(|s| s.name == name)
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

    /// Sets one named gain stage ([`SourceCapabilities::gain_stages`]; the value is quantised by
    /// [`GainStage::quantise`]). The next block carries [`Discontinuity::GAIN_CHANGE`].
    fn set_gain(&self, stage: &str, db: f64) -> Result<(), SourceError> {
        let _ = (stage, db);
        Err(SourceError::Unsupported {
            source_name: "source",
            operation: "set_gain",
        })
    }

    /// Stream counters (overruns, drops, discards), when the source keeps them.
    fn stats(&self) -> Option<SourceStats> {
        None
    }

    /// The device identity, when known.
    fn device_info(&self) -> Option<DeviceInfo> {
        None
    }

    /// The optional hardware sweep mode; `None` without it.
    fn sweep_capability(&self) -> Option<SweepCapability> {
        None
    }

    /// Starts a hardware sweep (optional capability).
    fn start_sweep(&self, plan: &SweepPlan) -> Result<(), SourceError> {
        let _ = plan;
        Err(SourceError::Unsupported {
            source_name: "source",
            operation: "start_sweep",
        })
    }

    /// Stops a hardware sweep (optional capability).
    fn stop_sweep(&self) -> Result<(), SourceError> {
        Err(SourceError::Unsupported {
            source_name: "source",
            operation: "stop_sweep",
        })
    }
}

/// A sample stream, owned by the capture thread. Its [`SourceControl`] comes from
/// [`Source::control`].
pub trait Source: Send {
    /// What the source can do.
    fn capabilities(&self) -> &SourceCapabilities;

    /// The control handle, shareable across threads.
    fn control(&self) -> Arc<dyn SourceControl>;

    /// The stream can be paused: the next read may be delayed indefinitely without losing or
    /// shifting samples (a recording read on demand). Consumers may then apply backpressure (the
    /// pipeline's lossless replay mode). Live radios keep streaming whether or not they are read,
    /// so the default is `false` (fail safe).
    fn pausable(&self) -> bool {
        false
    }

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

    /// Applies the tuning, rate, gain and filter changes to `tune`. The bias tee is not part of
    /// [`Tune`]: it rides on `Provenance::bias_tee` (T-325), so each stream applies `bias_tee`
    /// to its own state and mints new provenance when it changes.
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
    fn named_gain_stages_quantise_generically() {
        let caps = SourceCapabilities::hackrf_one();
        let lna = caps.gain_stage("lna").unwrap();
        assert_eq!(lna.quantise(30.0), Some(24.0));
        assert_eq!(lna.quantise(40.0), Some(40.0));
        assert_eq!(lna.quantise(48.0), None);
        assert_eq!(lna.quantise(f64::NAN), None);
        let amp = caps.gain_stage("amp").unwrap();
        assert_eq!(
            (amp.quantise(0.0), amp.quantise(11.0)),
            (Some(0.0), Some(11.0))
        );
        assert_eq!(
            (amp.quantise(4.0), amp.quantise(6.0)),
            (Some(0.0), Some(11.0))
        );
        assert!(caps.gain_stage("mixer").is_none());
        let continuous = GainStage {
            name: "if".into(),
            min_db: -10.0,
            max_db: 20.0,
            step_db: 0.0,
        };
        assert_eq!(continuous.quantise(3.3), Some(3.3));
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
            noise_sigma_lsb: None,
            temperature_c: None,
            antenna_port: None,
            bias_tee: hk_model::BiasTee::Unknown,
            clock_source: ClockSource::Internal,
            clock_locked: true,
            calibration_state_ref: None,
            spur_mask_ref: None,
            timestamp_method: TimestampMethod::Synthetic,
            timestamp_error_budget_ns: None,
            capture_artefacts: Vec::new(),
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

    // ---- T-341: the tuning step, and the achievable (centre, span) grid ----

    #[test]
    fn unknown_tuning_step_is_never_read_as_a_number() {
        // "Cannot report" and "reports 1 Hz" are different facts (the T-325 BiasTee lesson).
        assert_eq!(TuningStep::default(), TuningStep::Unknown);
        assert_eq!(TuningStep::Unknown.step_hz(), None);
        assert_eq!(TuningStep::Unknown.snap_hz(100e6), None);
        assert_eq!(TuningStep::Unknown.as_str(), "unknown");

        // A replay says so, and nothing invents a grid for it.
        let mut caps = SourceCapabilities::hackrf_one();
        caps.tuning_step = TuningStep::Unknown;
        assert_eq!(caps.snap_center_hz(100e6), None);
    }

    #[test]
    fn hackrf_tuning_step_is_the_max2837_synthesiser_grid() {
        // 30 MHz / 2^20: the RF granularity of the 20-bit fractional-N at a 40 MHz PFD with the
        // VCO at 4/3 the RF frequency (firmware max2837.c, cited on the constant).
        assert_eq!(HACKRF_ONE_TUNING_STEP_HZ, 28.6102294921875);
        let caps = SourceCapabilities::hackrf_one();
        assert_eq!(
            caps.tuning_step,
            TuningStep::Uniform {
                step_hz: HACKRF_ONE_TUNING_STEP_HZ
            }
        );

        // Err coarser, never finer: 1 Hz apart is the *same* achievable centre, so a 1 Hz grid
        // would put 28 centres on the navigation axis that do not exist.
        let a = caps.snap_center_hz(100_000_000.0).unwrap();
        let b = caps.snap_center_hz(100_000_001.0).unwrap();
        assert_eq!(a, b, "1 Hz apart snaps to one grid point");
        // And the snap lands within half a step of what was asked for, by construction.
        assert!((a - 100e6).abs() <= HACKRF_ONE_TUNING_STEP_HZ / 2.0 + 1e-9);
        assert_eq!(
            (a / HACKRF_ONE_TUNING_STEP_HZ).round() * HACKRF_ONE_TUNING_STEP_HZ,
            a
        );
    }

    #[test]
    fn a_snapped_centre_is_inside_the_band_at_both_edges() {
        let caps = SourceCapabilities::hackrf_one();
        for want in [1e6, 6e9, 0.0, 9e9] {
            let got = caps.snap_center_hz(want).expect("hackrf states a step");
            assert!(
                caps.supports_frequency(got),
                "snapped {want} to {got}, outside 1 MHz - 6 GHz"
            );
        }
        // Walked inward, not merely near: the nearest grid point to 6 GHz is above it.
        let top = caps.snap_center_hz(6e9).unwrap();
        assert!(top <= 6e9 && 6e9 - top < HACKRF_ONE_TUNING_STEP_HZ);
    }

    #[test]
    fn the_span_axis_is_the_sample_rate() {
        let caps = SourceCapabilities::hackrf_one();
        assert_eq!(caps.max_live_span_hz(), Some(20e6));
        assert_eq!(caps.snap_span_hz(2.4e6), Some(2.4e6));
        assert_eq!(caps.snap_span_hz(50e6), Some(20e6), "clamped to the widest");
        assert_eq!(
            caps.snap_span_hz(1e3),
            Some(2e6),
            "clamped to the narrowest"
        );

        let mut discrete = SourceCapabilities::hackrf_one();
        discrete.sample_rates = SampleRates::Discrete(vec![2.048e6, 8e6, 20e6]);
        assert_eq!(discrete.max_live_span_hz(), Some(20e6));
        assert_eq!(
            discrete.snap_span_hz(7e6),
            Some(8e6),
            "nearest, not the floor"
        );
        assert_eq!(discrete.snap_span_hz(1.0), Some(2.048e6));
    }
}
