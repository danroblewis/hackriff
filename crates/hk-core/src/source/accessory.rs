//! Accessory-fed sources (T-891): a receiver the base front end cannot reach, fed to the host
//! through something other than the SDR — a VLF/LF E-field or loop receiver into a soundcard —
//! behind the same [`Source`]/[`SourceControl`] contract as every radio.
//!
//! The HackRF One tunes no lower than 1 MHz, so SPACE-001 (SID flare monitor), SPACE-041
//! (ELF/VLF broadband receiver) and PROP-019 (VLF/LF phase and reflection height) are
//! `needs-accessory`. This module is the accessory's device interface; it is generic over where
//! the audio comes from ([`AudioInput`]) and names no HackRF specifics.
//!
//! # What an accessory source is, on the wire
//!
//! - **Samples** are **real** audio: each [`Complex32`] carries the sample in `re` with `im = 0`.
//!   The spectrum is therefore Hermitian about 0 Hz; the receiver band is `0 ..= fs/2`.
//! - **Tuning** is fixed at baseband: provenance `tune.center_hz = 0`, `bandwidth_hz = fs/2`, no
//!   gains (a soundcard's input gain is not modelled). `tune` answers
//!   [`SourceError::Unsupported`], the documented answer of a source that cannot honour a
//!   control. `set_sample_rate` works among the input's rates and reaches the stream as
//!   `RATE_CHANGE` at the next block boundary.
//! - **Provenance** says it came through the accessory, twice: `device_id` is
//!   `<accessory kind>:<device>` (e.g. `vlf-receiver:mock:sid_20k`) and `antenna_port` is
//!   `accessory:<kind>`. [`AccessoryKind::from_provenance`] reads it back, so any consumer — and
//!   the VLF analysis service, which refuses anything else — can tell an accessory stream from a
//!   radio's.
//! - **Timestamps** follow the sample counter from the input's anchor; the method is the input's
//!   own (`host-arrival` for a live soundcard, `synthetic` for generated data, or what a recording
//!   says it was captured with — a GPS-disciplined soundcard is `external-reference`, which is
//!   what makes PROP-019's phase meaningful).
//! - **Overruns** (a soundcard xrun, or one injected into the mock) are a `GAP` with the exact
//!   `dropped_before`, counted in [`SourceStats`].
//!
//! # Implementations
//!
//! - [`AccessorySource`] over any [`AudioInput`]. A platform audio backend (CoreAudio/ALSA) is an
//!   `AudioInput`; none is linked in this build, so the live accessory needs one plus the
//!   hardware.
//! - [`AccessoryMockDriver`]: the mock accessory, replaying a real-valued SigMF recording
//!   (`rf32_le`, `ri16_le`) — recorded or synthetic VLF — behind the same interface, for e2e.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hk_model::sigmf::{Datatype, SigmfMeta, data_path_for};
use hk_model::{BiasTee, ClockSource, Provenance, SampleTime, Timestamp, TimestampMethod, Tune};
use num_complex::Complex32;

use super::sigmf_replay::parse_sigmf_datetime;
use super::{
    ControlMailbox, DeviceInfo, Duplex, FrequencyRange, Gains, OpenRequest, SampleRates, Source,
    SourceCapabilities, SourceControl, SourceDriver, SourceError, SourceKind, SourceStats,
    TuningStep,
};
use crate::block::{BlockHeader, Discontinuity, ProvenanceHandle};

/// What kind of accessory feeds a source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AccessoryKind {
    /// A VLF/LF E-field or loop receiver into a soundcard (SPACE-001, SPACE-041, PROP-019).
    VlfReceiver,
}

impl AccessoryKind {
    /// Every kind.
    pub const ALL: &'static [AccessoryKind] = &[AccessoryKind::VlfReceiver];

    /// Stable name, e.g. `vlf-receiver`.
    pub const fn as_str(self) -> &'static str {
        match self {
            AccessoryKind::VlfReceiver => "vlf-receiver",
        }
    }

    /// The kind named `s`.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == s)
    }

    /// Provenance `antenna_port` of a stream through this accessory: `accessory:<kind>`.
    pub fn antenna_port(self) -> String {
        format!("accessory:{}", self.as_str())
    }

    /// Provenance `device_id` of `device` through this accessory: `<kind>:<device>`.
    pub fn device_id(self, device: &str) -> String {
        format!("{}:{device}", self.as_str())
    }

    /// The accessory a stream came through, read from its provenance; `None` for a radio's own
    /// front end. Both marks must agree (the port and the device id), so a stream cannot claim
    /// an accessory by one field alone.
    pub fn from_provenance(p: &Provenance) -> Option<Self> {
        let kind = Self::parse(p.antenna_port.as_deref()?.strip_prefix("accessory:")?)?;
        p.device_id
            .starts_with(&format!("{}:", kind.as_str()))
            .then_some(kind)
    }
}

/// What one [`AudioInput::read`] produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioRead {
    /// Frames were appended; `lost_before` frames were lost immediately before them (an xrun).
    Frames {
        /// Frames lost before this read's first frame.
        lost_before: u64,
    },
    /// The input has ended (a recording's end); later reads also answer `End`.
    End,
}

/// Where an accessory source's real samples come from: a soundcard backend, or the mock's
/// recording. Owned by the capture thread.
pub trait AudioInput: Send {
    /// Rates the input can deliver, Hz.
    fn sample_rates(&self) -> Vec<f64>;
    /// The current rate, Hz.
    fn sample_rate_hz(&self) -> f64;
    /// Switches the rate (one of [`AudioInput::sample_rates`]).
    fn set_sample_rate(&mut self, hz: f64) -> Result<(), SourceError>;
    /// The instant of the first frame the input delivers.
    fn anchor(&self) -> Timestamp;
    /// How the input's times are obtained.
    fn timestamp_method(&self) -> TimestampMethod;
    /// Sample-clock reference and lock state.
    fn clock(&self) -> (ClockSource, bool) {
        (ClockSource::Internal, true)
    }
    /// Appends up to `max` frames (full scale ±1) to `out`, which the caller has cleared.
    fn read(&mut self, out: &mut Vec<f32>, max: usize) -> Result<AudioRead, SourceError>;
}

/// Capabilities of an accessory source delivering `rates`: receive-only, real samples on
/// `0 ..= max(rate)/2`, no tuning grid, no gains, filters, bias tee or clock input.
pub fn accessory_capabilities(
    driver: &str,
    kind: SourceKind,
    rates: &[f64],
    native_format: Datatype,
) -> SourceCapabilities {
    let max = rates.iter().copied().fold(0.0f64, f64::max);
    SourceCapabilities {
        driver: driver.into(),
        kind,
        frequency_ranges: vec![FrequencyRange {
            min_hz: 0.0,
            max_hz: max / 2.0,
        }],
        sample_rates: SampleRates::Discrete(rates.to_vec()),
        tuning_step: TuningStep::Unknown,
        adc_bits: match native_format {
            Datatype::Ri16Le => 16,
            _ => 24,
        },
        native_format,
        duplex: Duplex::ReceiveOnly,
        tx_capable: false,
        controllable: true,
        gain_stages: Vec::new(),
        rf_amp: false,
        baseband_filter: None,
        bias_tee: false,
        external_clock: false,
        hardware_timestamps: false,
        rf_path_boundaries_hz: Vec::new(),
    }
}

struct Shared {
    mailbox: ControlMailbox,
    stopped: AtomicBool,
    started: AtomicBool,
    blocks: AtomicU64,
    samples: AtomicU64,
    overruns: AtomicU64,
    dropped: AtomicU64,
    info: DeviceInfo,
}

/// The control half of an [`AccessorySource`].
pub struct AccessoryControl {
    caps: SourceCapabilities,
    shared: Arc<Shared>,
    name: &'static str,
}

impl AccessoryControl {
    fn unsupported(&self, operation: &'static str) -> SourceError {
        SourceError::Unsupported {
            source_name: self.name,
            operation,
        }
    }
}

impl SourceControl for AccessoryControl {
    fn capabilities(&self) -> &SourceCapabilities {
        &self.caps
    }

    /// Fixed at baseband: an audio-rate receiver has no local oscillator to move.
    fn tune(&self, _center_hz: f64) -> Result<(), SourceError> {
        Err(self.unsupported("tune (an accessory receiver is fixed at baseband)"))
    }

    fn set_sample_rate(&self, sample_rate_hz: f64) -> Result<(), SourceError> {
        if !sample_rate_hz.is_finite() || !self.caps.sample_rates.supports(sample_rate_hz) {
            return Err(SourceError::OutOfRange {
                what: "sample_rate_hz",
                value: sample_rate_hz,
            });
        }
        self.shared
            .mailbox
            .post(|p| p.sample_rate_hz = Some(sample_rate_hz));
        Ok(())
    }

    fn set_gains(&self, _gains: &Gains) -> Result<(), SourceError> {
        Err(self.unsupported("set_gains"))
    }

    fn set_baseband_filter(&self, _bandwidth_hz: f64) -> Result<(), SourceError> {
        Err(self.unsupported("set_baseband_filter"))
    }

    fn set_bias_tee(&self, _enabled: bool) -> Result<(), SourceError> {
        Err(self.unsupported("set_bias_tee"))
    }

    fn start(&self) -> Result<(), SourceError> {
        self.shared.started.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn stop(&self) -> Result<(), SourceError> {
        self.shared.stopped.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn stats(&self) -> Option<SourceStats> {
        let s = &self.shared;
        Some(SourceStats {
            blocks: s.blocks.load(Ordering::SeqCst),
            samples: s.samples.load(Ordering::SeqCst),
            overruns: s.overruns.load(Ordering::SeqCst),
            dropped_samples: s.dropped.load(Ordering::SeqCst),
            discarded_samples: 0,
        })
    }

    fn device_info(&self) -> Option<DeviceInfo> {
        Some(self.shared.info.clone())
    }
}

/// Who an [`AccessorySource`] is: the accessory, the driver and the device behind it.
#[derive(Clone, Debug)]
pub struct AccessoryDescriptor {
    /// The accessory.
    pub kind: AccessoryKind,
    /// Driver name (capabilities and `DeviceInfo`).
    pub driver: &'static str,
    /// Device selector; the provenance `device_id` is `<kind>:<device>`.
    pub device: String,
    /// SigMF `core:hw` description.
    pub hw: String,
    /// Live hardware, or a replay.
    pub source_kind: SourceKind,
    /// The input's native sample format.
    pub native_format: Datatype,
}

/// A source fed by an accessory through an [`AudioInput`] (see the module docs for the wire
/// contract).
pub struct AccessorySource {
    kind: AccessoryKind,
    caps: SourceCapabilities,
    input: Box<dyn AudioInput>,
    control: Arc<AccessoryControl>,
    shared: Arc<Shared>,
    provenance: ProvenanceHandle,
    block_len: usize,
    next_index: u64,
    /// Time anchor of the current rate segment.
    anchor: SampleTime,
    rate_hz: f64,
    first: bool,
    seen: u64,
    scratch: Vec<f32>,
    realtime: Option<(Instant, u64)>,
}

impl AccessorySource {
    /// An accessory source described by `desc` over `input` (the provenance `device_id` becomes
    /// `<kind>:<device>`), delivering `block_len` frames per block.
    pub fn new(desc: AccessoryDescriptor, input: Box<dyn AudioInput>, block_len: usize) -> Self {
        let AccessoryDescriptor {
            kind,
            driver,
            device,
            hw,
            source_kind,
            native_format,
        } = desc;
        let caps =
            accessory_capabilities(driver, source_kind, &input.sample_rates(), native_format);
        let device_id = kind.device_id(&device);
        let shared = Arc::new(Shared {
            mailbox: ControlMailbox::new(),
            stopped: AtomicBool::new(false),
            started: AtomicBool::new(false),
            blocks: AtomicU64::new(0),
            samples: AtomicU64::new(0),
            overruns: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            info: DeviceInfo {
                driver: driver.into(),
                device_id: device_id.clone(),
                hw,
            },
        });
        let control = Arc::new(AccessoryControl {
            caps: caps.clone(),
            shared: Arc::clone(&shared),
            name: driver,
        });
        let rate_hz = input.sample_rate_hz();
        let anchor = SampleTime {
            sample_index: 0,
            host_time: input.anchor(),
        };
        let provenance = ProvenanceHandle::new(accessory_provenance(kind, &device_id, &*input));
        Self {
            kind,
            caps,
            input,
            control,
            shared,
            provenance,
            block_len: block_len.max(1),
            next_index: 0,
            anchor,
            rate_hz,
            first: true,
            seen: 0,
            scratch: Vec::new(),
            realtime: None,
        }
    }

    /// Paces reads to the wall clock (a mock standing in for a live soundcard).
    pub fn with_realtime(mut self) -> Self {
        self.realtime = Some((Instant::now(), 0));
        self
    }

    /// The accessory this source is fed by.
    pub fn accessory(&self) -> AccessoryKind {
        self.kind
    }
}

fn accessory_provenance(
    kind: AccessoryKind,
    device_id: &str,
    input: &dyn AudioInput,
) -> Provenance {
    let fs = input.sample_rate_hz();
    let (clock_source, clock_locked) = input.clock();
    Provenance {
        device_id: device_id.into(),
        tune: Tune {
            center_hz: 0.0,
            sample_rate_hz: fs,
            lna_db: 0.0,
            vga_db: 0.0,
            amp_on: false,
            bandwidth_hz: fs / 2.0,
        },
        overload: false,
        quantisation_limited: false,
        noise_sigma_lsb: None,
        temperature_c: None,
        antenna_port: Some(kind.antenna_port()),
        bias_tee: BiasTee::Unknown,
        clock_source,
        clock_locked,
        calibration_state_ref: None,
        spur_mask_ref: None,
        timestamp_method: input.timestamp_method(),
        timestamp_error_budget_ns: None,
        capture_artefacts: Vec::new(),
    }
}

impl Source for AccessorySource {
    fn capabilities(&self) -> &SourceCapabilities {
        &self.caps
    }

    fn control(&self) -> Arc<dyn SourceControl> {
        Arc::clone(&self.control) as Arc<dyn SourceControl>
    }

    fn read_block(
        &mut self,
        samples: &mut Vec<Complex32>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        samples.clear();
        if self.shared.stopped.load(Ordering::SeqCst) {
            return Ok(None);
        }
        let mut flags = Discontinuity::NONE;
        if self.first {
            flags |= Discontinuity::STREAM_START;
        }
        if let Some(change) = self.shared.mailbox.take(&mut self.seen)
            && let Some(hz) = change.sample_rate_hz
            && hz != self.rate_hz
        {
            self.input.set_sample_rate(hz)?;
            let t = self.anchor.time_of(self.next_index, self.rate_hz);
            self.anchor = SampleTime {
                sample_index: self.next_index,
                host_time: t,
            };
            self.rate_hz = hz;
            let mut p = self.provenance.get().clone();
            p.tune.sample_rate_hz = hz;
            p.tune.bandwidth_hz = hz / 2.0;
            flags |= Discontinuity::between(self.provenance.get(), &p);
            self.provenance = ProvenanceHandle::new(p);
        }
        self.scratch.clear();
        let lost = match self.input.read(&mut self.scratch, self.block_len)? {
            AudioRead::End => return Ok(None),
            AudioRead::Frames { lost_before } => lost_before,
        };
        if self.scratch.is_empty() {
            return Ok(None);
        }
        if lost > 0 {
            flags |= Discontinuity::GAP;
            self.shared.overruns.fetch_add(1, Ordering::SeqCst);
            self.shared.dropped.fetch_add(lost, Ordering::SeqCst);
        }
        let first_sample = self.next_index + lost;
        let header = BlockHeader {
            time: SampleTime {
                sample_index: first_sample,
                host_time: self.anchor.time_of(first_sample, self.rate_hz),
            },
            provenance: self.provenance.clone(),
            discontinuity: flags,
            dropped_before: lost,
        };
        samples.extend(self.scratch.iter().map(|&s| Complex32::new(s, 0.0)));
        let n = samples.len() as u64;
        self.next_index = first_sample + n;
        self.first = false;
        self.shared.blocks.fetch_add(1, Ordering::SeqCst);
        self.shared.samples.fetch_add(n, Ordering::SeqCst);
        if let Some((t0, ref mut delivered)) = self.realtime {
            *delivered += n + lost;
            let due = t0 + Duration::from_secs_f64(*delivered as f64 / self.rate_hz);
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
        }
        Ok(Some(header))
    }
}

/// The mock accessory's settings.
#[derive(Clone, Debug)]
pub struct AccessoryMockOptions {
    /// The accessory it stands in for.
    pub kind: AccessoryKind,
    /// Frames per block.
    pub block_len: usize,
    /// Pace reads to the wall clock, as a live soundcard would (tests usually read unpaced).
    pub realtime: bool,
}

impl Default for AccessoryMockOptions {
    fn default() -> Self {
        Self {
            kind: AccessoryKind::VlfReceiver,
            block_len: 4800,
            realtime: false,
        }
    }
}

/// The mock accessory driver (`vlf-mock:<file.sigmf-meta>`): a real-valued SigMF recording —
/// captured or synthetic VLF — behind the accessory contract. Every open replays it from the
/// start. [`AccessoryMockDriver::inject_overrun`] drops frames from the open stream as a
/// soundcard xrun would.
pub struct AccessoryMockDriver {
    meta_path: PathBuf,
    meta: SigmfMeta,
    rate_hz: f64,
    options: AccessoryMockOptions,
    overrun: Arc<AtomicU64>,
}

/// Driver name of the mock accessory.
pub const ACCESSORY_MOCK_DRIVER: &str = "accessory-mock";

impl AccessoryMockDriver {
    /// Reads the recording's metadata; refuses a complex or unsupported datatype, or a missing
    /// sample rate.
    pub fn new(
        meta_path: impl AsRef<Path>,
        options: AccessoryMockOptions,
    ) -> Result<Self, SourceError> {
        let meta_path = meta_path.as_ref().to_path_buf();
        let meta = SigmfMeta::read(&meta_path)?;
        let dt = meta.global.datatype;
        if !matches!(dt, Datatype::Rf32Le | Datatype::Ri16Le) {
            return Err(SourceError::UnsupportedDatatype(dt));
        }
        let rate_hz = meta
            .global
            .sample_rate
            .filter(|r| r.is_finite() && *r > 0.0)
            .ok_or_else(|| SourceError::InvalidRecording("no core:sample_rate".into()))?;
        Ok(Self {
            meta_path,
            meta,
            rate_hz,
            options,
            overrun: Arc::new(AtomicU64::new(0)),
        })
    }

    /// The recording's rate, Hz: the only rate the mock delivers.
    pub fn sample_rate_hz(&self) -> f64 {
        self.rate_hz
    }

    /// The open request that reproduces the recording.
    pub fn default_request(&self) -> OpenRequest {
        OpenRequest {
            center_hz: 0.0,
            sample_rate_hz: self.rate_hz,
            ..OpenRequest::default()
        }
    }

    /// Drops the next `frames` frames of the open stream, as an xrun.
    pub fn inject_overrun(&self, frames: u64) {
        self.overrun.fetch_add(frames, Ordering::SeqCst);
    }

    /// A shareable handle to [`Self::inject_overrun`].
    pub fn overrun_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.overrun)
    }

    fn device(&self) -> String {
        let stem = self
            .meta_path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.trim_end_matches(".sigmf-meta"))
            .unwrap_or("recording");
        format!("mock:{stem}")
    }
}

impl SourceDriver for AccessoryMockDriver {
    fn name(&self) -> &'static str {
        ACCESSORY_MOCK_DRIVER
    }

    fn available(&self) -> bool {
        true
    }

    fn capabilities(&self) -> SourceCapabilities {
        accessory_capabilities(
            ACCESSORY_MOCK_DRIVER,
            SourceKind::Hardware,
            &[self.rate_hz],
            self.meta.global.datatype,
        )
    }

    fn open(&self, request: &OpenRequest) -> Result<Box<dyn Source>, SourceError> {
        if request.sample_rate_hz != 0.0 && request.sample_rate_hz != self.rate_hz {
            return Err(SourceError::OutOfRange {
                what: "sample_rate_hz",
                value: request.sample_rate_hz,
            });
        }
        if request.center_hz != 0.0 {
            return Err(SourceError::OutOfRange {
                what: "center_hz",
                value: request.center_hz,
            });
        }
        let input = SigmfAudioInput::open(&self.meta_path, &self.meta, Arc::clone(&self.overrun))?;
        let hw = self
            .meta
            .global
            .hw
            .clone()
            .unwrap_or_else(|| format!("mock {} (accessory)", self.options.kind.as_str()));
        let src = AccessorySource::new(
            AccessoryDescriptor {
                kind: self.options.kind,
                driver: ACCESSORY_MOCK_DRIVER,
                device: self.device(),
                hw,
                source_kind: SourceKind::Hardware,
                native_format: self.meta.global.datatype,
            },
            Box::new(input),
            self.options.block_len,
        );
        Ok(Box::new(if self.options.realtime {
            src.with_realtime()
        } else {
            src
        }))
    }
}

/// A real-valued SigMF recording as an [`AudioInput`] (the mock accessory's samples).
struct SigmfAudioInput {
    reader: BufReader<File>,
    datatype: Datatype,
    rate_hz: f64,
    anchor: Timestamp,
    method: TimestampMethod,
    clock: (ClockSource, bool),
    overrun: Arc<AtomicU64>,
    bytes: Vec<u8>,
    ended: bool,
}

impl SigmfAudioInput {
    fn open(
        meta_path: &Path,
        meta: &SigmfMeta,
        overrun: Arc<AtomicU64>,
    ) -> Result<Self, SourceError> {
        let data = data_path_for(meta_path);
        let file = File::open(&data).map_err(|e| SourceError::Io {
            path: Some(data.clone()),
            source: e,
        })?;
        let capture = meta.captures.first();
        let anchor = match capture.and_then(|c| c.datetime.as_deref()) {
            Some(dt) => parse_sigmf_datetime(dt).ok_or_else(|| {
                SourceError::InvalidRecording(format!("unparseable core:datetime {dt:?}"))
            })?,
            None => Timestamp::from_unix_nanos(0),
        };
        let recorded = capture
            .and_then(|c| c.provenance.as_ref())
            .or(meta.global.provenance.as_ref());
        let (method, clock) = match recorded {
            Some(p) => (p.timestamp_method, (p.clock_source, p.clock_locked)),
            // A recording that says nothing about its capture: times are the mock's own
            // construction from the anchor and the counter.
            None => (TimestampMethod::Synthetic, (ClockSource::Internal, true)),
        };
        Ok(Self {
            reader: BufReader::new(file),
            datatype: meta.global.datatype,
            rate_hz: meta.global.sample_rate.unwrap_or(0.0),
            anchor,
            method,
            clock,
            overrun,
            bytes: Vec::new(),
            ended: false,
        })
    }

    /// Reads up to `n` frames into `out`; returns the count (0 at the end).
    fn read_frames(&mut self, out: &mut Vec<f32>, n: usize) -> Result<usize, SourceError> {
        let bps = self.datatype.bytes_per_sample();
        self.bytes.resize(n * bps, 0);
        let mut got = 0;
        while got < self.bytes.len() {
            match self.reader.read(&mut self.bytes[got..]) {
                Ok(0) => break,
                Ok(k) => got += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    return Err(SourceError::InvalidRecording(format!(
                        "reading samples: {e}"
                    )));
                }
            }
        }
        let whole = got / bps;
        let chunks = self.bytes[..whole * bps].chunks_exact(bps);
        match self.datatype {
            Datatype::Rf32Le => {
                out.extend(chunks.map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])));
            }
            _ => out.extend(chunks.map(|c| f32::from(i16::from_le_bytes([c[0], c[1]])) / 32768.0)),
        }
        Ok(whole)
    }
}

impl AudioInput for SigmfAudioInput {
    fn sample_rates(&self) -> Vec<f64> {
        vec![self.rate_hz]
    }

    fn sample_rate_hz(&self) -> f64 {
        self.rate_hz
    }

    fn set_sample_rate(&mut self, hz: f64) -> Result<(), SourceError> {
        if hz == self.rate_hz {
            Ok(())
        } else {
            Err(SourceError::OutOfRange {
                what: "sample_rate_hz",
                value: hz,
            })
        }
    }

    fn anchor(&self) -> Timestamp {
        self.anchor
    }

    fn timestamp_method(&self) -> TimestampMethod {
        self.method
    }

    fn clock(&self) -> (ClockSource, bool) {
        self.clock
    }

    fn read(&mut self, out: &mut Vec<f32>, max: usize) -> Result<AudioRead, SourceError> {
        if self.ended {
            return Ok(AudioRead::End);
        }
        let mut lost = 0u64;
        let drop = self.overrun.swap(0, Ordering::SeqCst);
        if drop > 0 {
            let mut sink = Vec::new();
            let mut left = drop;
            while left > 0 {
                sink.clear();
                let k = self.read_frames(&mut sink, left.min(65_536) as usize)?;
                if k == 0 {
                    break;
                }
                lost += k as u64;
                left -= k as u64;
            }
        }
        if self.read_frames(out, max)? == 0 {
            self.ended = true;
            return Ok(AudioRead::End);
        }
        Ok(AudioRead::Frames { lost_before: lost })
    }
}

/// Writes `x` (full scale ±1) as a real-valued `rf32_le` SigMF recording at `meta_path`, with
/// the given rate and first-sample time — the synthetic-VLF fixture writer tests use. `provenance`
/// (optional) is stored as the capture's `hackriff:provenance`, e.g. to say the capture was
/// GPS-disciplined.
pub fn write_real_sigmf(
    meta_path: impl AsRef<Path>,
    x: &[f32],
    sample_rate_hz: f64,
    datetime: &str,
    description: &str,
    provenance: Option<Provenance>,
) -> Result<(), SourceError> {
    let meta_path = meta_path.as_ref();
    let mut meta = SigmfMeta::new(Datatype::Rf32Le);
    meta.global.sample_rate = Some(sample_rate_hz);
    meta.global.description = Some(description.into());
    if provenance.is_some() {
        meta.declare_hackriff_extension();
    }
    meta.captures.push(hk_model::sigmf::Capture {
        sample_start: 0,
        frequency: Some(0.0),
        datetime: Some(datetime.into()),
        provenance,
        clip_count: None,
        extra: Default::default(),
    });
    meta.write(meta_path)?;
    let bytes: Vec<u8> = x.iter().flat_map(|s| s.to_le_bytes()).collect();
    let data = data_path_for(meta_path);
    std::fs::write(&data, bytes).map_err(|e| SourceError::Io {
        path: Some(data),
        source: e,
    })
}
