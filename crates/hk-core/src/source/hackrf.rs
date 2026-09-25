//! HackRF One source (C01, T-037a): libhackrf receive behind the [`Source`] / [`SourceControl`]
//! split. **Receive only**: no transmit function is bound ([`ffi`](self) declares RX, tuning and
//! identity calls only), so no TX path exists in this crate.
//!
//! # Build
//!
//! The driver needs cargo feature **`hackrf`** (off by default), which links the system libhackrf
//! (`build.rs`: pkg-config `libhackrf`, or `HACKRF_LIB_DIR`). Without it [`HackRfSource::open`]
//! returns [`SourceError::NotAvailable`], so CI builds and tests with no libhackrf installed. A
//! feature was chosen over `dlopen`: link errors surface at build time, no symbol table is
//! hand-maintained at runtime, and the default build carries no FFI at all. The stream logic below
//! is compiled either way and tested against a fake device.
//!
//! # Data path (no allocation per block)
//!
//! libusb callback → [`pool::TransferPool`] (fixed transfer buffers, lock-free SPSC index
//! queues; a transfer with no free buffer is dropped and counted) → the capture thread's
//! [`Source::read_block_ci8`], which copies one transfer into the caller's reused buffer and
//! recycles it. One block is one USB transfer (131 072 samples with libhackrf's default buffer).
//!
//! - **Sample counter:** the transfer's byte offset in the stream, counting dropped transfers, so
//!   a drop is a [`Discontinuity::GAP`] with an exact `dropped_before`.
//! - **Time:** [`TimestampMethod::HostArrival`]: the first block's arrival anchors its last
//!   sample, later samples are extrapolated by count at the tuned rate (re-anchored after a rate
//!   change). Times never go backwards.
//! - **Controls** ([`HackRfControl`]): validated against [`SourceCapabilities::hackrf_one`] when
//!   posted (gains are quantised down to the stage step), applied by the capture thread at the next
//!   block boundary through [`ControlMailbox`]. Transfers that arrived before the change finished,
//!   plus the one in flight, are discarded (settle), so **no block mixes two settings**; the next
//!   block carries the change's flags ([`Discontinuity::between`]) plus `GAP` for the discarded
//!   samples, counted separately from drops ([`HackRfStats::settle_discarded_samples`]).
//! - **Baseband filter:** `hackrf_compute_baseband_filter_bw(0.75 · fs)` unless set explicitly;
//!   the provenance records the bandwidth set.
//! - **Bias tee:** off at open and at close unless explicitly enabled.
//! - **Overload:** a block whose clipped I/Q components (codes −128 or 127) exceed
//!   [`HackRfConfig::overload_clip_fraction`] marks the tune state overloaded: a new provenance
//!   with `overload = true` (a `PROVENANCE_CHANGE`, which does not reset averaging). It is sticky
//!   until the next tune/gain change, as `Provenance::overload` specifies.
//! - **Provenance and SigMF:** device `hackrf:<serial>`, the tune (centre, rate, LNA/VGA/amp,
//!   filter), antenna port `unknown`, internal clock. Firmware, board and library versions are in
//!   [`HackRfDeviceInfo::hw_description`] (the pipeline writes it to SigMF `core:hw`).
//! - **Not pausable:** the radio keeps streaming, so lossless backpressure is refused.
//!
//! # Licensing (ADR-0010)
//!
//! `host/libhackrf/src/hackrf.c` and `hackrf.h` are **BSD-3-Clause**; libusb is LGPL-2.1, linked
//! dynamically; hackrf-tools and firmware (GPL) are not linked.
//!
//! # Hardware test
//!
//! `cargo test -p hk-core --features hackrf --test hackrf_hil -- --ignored --nocapture`
//! (receive only; check the device is free with `hackrf_info` first).

#[cfg(feature = "hackrf")]
mod ffi;
pub mod pool;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use hk_model::{BiasTee, ClockSource, Provenance, SampleTime, Timestamp, TimestampMethod, Tune};
use num_complex::{Complex, Complex32};
use serde_json::{Value, json};

use self::pool::{TransferCounters, TransferPool};
use super::{
    ControlMailbox, DeviceInfo, Gains, InUseCertainty, OpenRequest, Source, SourceCapabilities,
    SourceControl, SourceDriver, SourceError, SourceStats,
};
use crate::block::{BlockHeader, Discontinuity, ProvenanceHandle};

/// libhackrf's default USB transfer size, bytes (131 072 samples).
pub const DEFAULT_TRANSFER_BYTES: usize = 262_144;
/// Antenna port recorded in provenance: the antenna is not known to the software.
pub const ANTENNA_UNKNOWN: &str = "unknown";
/// No transfer for this long while streaming is an error (unplugged, USB stalled).
const STALL_TIMEOUT: Duration = Duration::from_secs(3);

/// Initial settings for [`HackRfSource::open_with`].
#[derive(Clone, Debug, PartialEq)]
pub struct HackRfConfig {
    /// Serial number to open; `None` opens the first device.
    pub serial: Option<String>,
    /// Centre frequency, Hz.
    pub center_hz: f64,
    /// Sample rate, Hz (2–20 Msps).
    pub sample_rate_hz: f64,
    /// Gains (quantised down to the stage steps).
    pub gains: Gains,
    /// Baseband filter bandwidth, Hz; `None` chooses `0.75 · fs`.
    pub baseband_filter_hz: Option<f64>,
    /// Antenna-port bias tee (default off).
    pub bias_tee: bool,
    /// Transfer buffers between the USB callback and the capture thread (64 ≈ 0.4 s at 20 Msps).
    pub queue_buffers: usize,
    /// Clipped fraction of a block's I/Q components that marks overload (1e-3).
    pub overload_clip_fraction: f64,
}

impl Default for HackRfConfig {
    fn default() -> Self {
        Self {
            serial: None,
            center_hz: 100e6,
            sample_rate_hz: 2e6,
            gains: Gains {
                lna_db: 16.0,
                vga_db: 20.0,
                amp_on: false,
            },
            baseband_filter_hz: None,
            bias_tee: false,
            queue_buffers: 64,
            overload_clip_fraction: 1e-3,
        }
    }
}

impl HackRfConfig {
    /// Maps a generic [`OpenRequest`] (named gains `lna`, `vga`, `amp`) onto HackRF settings;
    /// stages not named keep the defaults. Unknown stages and out-of-range values are refused.
    pub fn from_request(request: &OpenRequest) -> Result<Self, SourceError> {
        let caps = SourceCapabilities::hackrf_one();
        let mut config = Self {
            serial: request.device.clone(),
            center_hz: request.center_hz,
            sample_rate_hz: request.sample_rate_hz,
            baseband_filter_hz: request.baseband_filter_hz,
            bias_tee: request.bias_tee,
            ..Self::default()
        };
        for g in &request.gains {
            let db = caps
                .gain_stage(&g.stage)
                .and_then(|s| s.quantise(g.db))
                .ok_or(SourceError::OutOfRange {
                    what: "named gain (stage lna, vga or amp)",
                    value: g.db,
                })?;
            match g.stage.as_str() {
                "lna" => config.gains.lna_db = db,
                "vga" => config.gains.vga_db = db,
                _ => config.gains.amp_on = db > 0.0,
            }
        }
        Ok(config)
    }
}

/// The HackRF One [`SourceDriver`]: generic open requests to [`HackRfSource`].
#[derive(Clone, Copy, Debug, Default)]
pub struct HackRfDriver;

impl SourceDriver for HackRfDriver {
    fn name(&self) -> &'static str {
        "hackrf"
    }

    fn available(&self) -> bool {
        HackRfSource::available()
    }

    fn capabilities(&self) -> SourceCapabilities {
        SourceCapabilities::hackrf_one()
    }

    fn open(&self, request: &OpenRequest) -> Result<Box<dyn Source>, SourceError> {
        let config = HackRfConfig::from_request(request)?;
        Ok(Box::new(HackRfSource::open_with(&config)?))
    }
}

/// The opened device's identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HackRfDeviceInfo {
    /// Serial number, 32 hex digits (as `hackrf_info` prints it).
    pub serial: String,
    /// Board id.
    pub board_id: u8,
    /// Board name, e.g. `HackRF One`.
    pub board_name: String,
    /// Board revision code, when the firmware reports one.
    pub board_rev: Option<u8>,
    /// Firmware version string.
    pub firmware: String,
    /// USB API version (BCD, e.g. `0x0110`).
    pub usb_api_version: u16,
    /// libhackrf version.
    pub library_version: String,
    /// libhackrf release.
    pub library_release: String,
}

impl HackRfDeviceInfo {
    /// Provenance `device_id`: `hackrf:<serial>`.
    pub fn device_id(&self) -> String {
        format!("hackrf:{}", self.serial)
    }

    /// SigMF `core:hw`: board, serial, firmware, library and antenna.
    pub fn hw_description(&self) -> String {
        let rev = self
            .board_rev
            .map_or(String::new(), |r| format!(", board rev {r}"));
        format!(
            "{} serial {}, firmware {} (USB API {:x}.{:02x}){rev}, libhackrf {} ({}), antenna {}",
            self.board_name,
            self.serial,
            self.firmware,
            self.usb_api_version >> 8,
            self.usb_api_version & 0xff,
            self.library_version,
            self.library_release,
            ANTENNA_UNKNOWN
        )
    }
}

/// Stream counters (the transfer counters are updated by the USB callback).
#[derive(Debug, Default)]
pub struct HackRfStats {
    /// Callback-side counters: transfers, drops, truncations, queue depth.
    pub transfers: Arc<TransferCounters>,
    /// Samples delivered in blocks.
    pub delivered_samples: AtomicU64,
    /// Blocks delivered.
    pub delivered_blocks: AtomicU64,
    /// Samples discarded after a control change (settling; not drops).
    pub settle_discarded_samples: AtomicU64,
    /// Control changes applied.
    pub control_changes: AtomicU64,
    /// Blocks over the overload clip fraction.
    pub overload_blocks: AtomicU64,
    /// Clipped I/Q components seen.
    pub clipped_components: AtomicU64,
}

impl HackRfStats {
    /// A JSON snapshot.
    pub fn to_json(&self) -> Value {
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let t = &self.transfers;
        json!({
            "transfers": g(&t.transfers),
            "dropped_transfers": g(&t.dropped_transfers),
            "dropped_samples": g(&t.dropped_samples),
            "truncated_transfers": g(&t.truncated_transfers),
            "max_queue_depth": g(&t.max_queue_depth),
            "delivered_samples": g(&self.delivered_samples),
            "delivered_blocks": g(&self.delivered_blocks),
            "settle_discarded_samples": g(&self.settle_discarded_samples),
            "control_changes": g(&self.control_changes),
            "overload_blocks": g(&self.overload_blocks),
            "clipped_components": g(&self.clipped_components),
        })
    }
}

/// The device operations the stream needs: `ffi::LibHackRf` (feature `hackrf`), or a fake in
/// tests. Receive only by construction.
#[cfg_attr(not(any(test, feature = "hackrf")), allow(dead_code))]
pub(crate) trait RxDevice: Send {
    fn set_freq(&mut self, hz: u64) -> Result<(), SourceError>;
    fn set_sample_rate(&mut self, hz: f64) -> Result<(), SourceError>;
    fn set_baseband_filter(&mut self, hz: u32) -> Result<(), SourceError>;
    fn compute_baseband_filter_bw(&self, hz: u32) -> u32;
    fn set_lna_gain(&mut self, db: u32) -> Result<(), SourceError>;
    fn set_vga_gain(&mut self, db: u32) -> Result<(), SourceError>;
    fn set_amp_enable(&mut self, on: bool) -> Result<(), SourceError>;
    fn set_antenna_enable(&mut self, on: bool) -> Result<(), SourceError>;
    fn start_rx(&mut self, pool: Arc<TransferPool>) -> Result<(), SourceError>;
    fn stop_rx(&mut self) -> Result<(), SourceError>;
    fn is_streaming(&self) -> bool;
}

/// Validates a centre frequency; returns it rounded to 1 Hz.
fn checked_frequency(caps: &SourceCapabilities, hz: f64) -> Result<f64, SourceError> {
    if hz.is_finite() && caps.supports_frequency(hz) {
        Ok(hz.round())
    } else {
        Err(SourceError::OutOfRange {
            what: "centre frequency (Hz)",
            value: hz,
        })
    }
}

fn checked_rate(caps: &SourceCapabilities, hz: f64) -> Result<f64, SourceError> {
    if hz.is_finite() && caps.sample_rates.supports(hz) {
        Ok(hz)
    } else {
        Err(SourceError::OutOfRange {
            what: "sample rate (Hz)",
            value: hz,
        })
    }
}

fn checked_filter(caps: &SourceCapabilities, hz: f64) -> Result<f64, SourceError> {
    match &caps.baseband_filter {
        Some(f) if hz.is_finite() && f.supports(hz) => Ok(hz),
        _ => Err(SourceError::OutOfRange {
            what: "baseband filter bandwidth (Hz)",
            value: hz,
        }),
    }
}

/// Validates gains against the stages and quantises them down to each stage's step.
fn quantised_gains(caps: &SourceCapabilities, g: &Gains) -> Result<Gains, SourceError> {
    let stage = |name: &str, what: &'static str, v: f64| -> Result<f64, SourceError> {
        let err = SourceError::OutOfRange { what, value: v };
        let s = caps
            .gain_stages
            .iter()
            .find(|s| s.name == name)
            .ok_or(SourceError::OutOfRange { what, value: v })?;
        if !v.is_finite() || v < s.min_db - 1e-9 || v > s.max_db + 1e-9 {
            return Err(err);
        }
        let q = if s.step_db > 0.0 {
            s.min_db + ((v - s.min_db) / s.step_db + 1e-9).floor() * s.step_db
        } else {
            v
        };
        Ok(q.clamp(s.min_db, s.max_db))
    };
    if g.amp_on && !caps.rf_amp {
        return Err(SourceError::OutOfRange {
            what: "rf amplifier",
            value: 1.0,
        });
    }
    Ok(Gains {
        lna_db: stage("lna", "lna gain (dB)", g.lna_db)?,
        vga_db: stage("vga", "vga gain (dB)", g.vga_db)?,
        amp_on: g.amp_on,
    })
}

/// The HackRF One control handle: validates and posts; the stream applies at a block boundary.
pub struct HackRfControl {
    capabilities: SourceCapabilities,
    mailbox: ControlMailbox,
    stopped: AtomicBool,
    /// Last requested gains (named single-stage changes merge into them).
    requested: Mutex<Gains>,
    stats: Arc<HackRfStats>,
    device: DeviceInfo,
}

impl SourceControl for HackRfControl {
    fn capabilities(&self) -> &SourceCapabilities {
        &self.capabilities
    }

    fn tune(&self, center_hz: f64) -> Result<(), SourceError> {
        let hz = checked_frequency(&self.capabilities, center_hz)?;
        self.mailbox.post(|p| p.center_hz = Some(hz));
        Ok(())
    }

    fn set_sample_rate(&self, sample_rate_hz: f64) -> Result<(), SourceError> {
        let hz = checked_rate(&self.capabilities, sample_rate_hz)?;
        self.mailbox.post(|p| p.sample_rate_hz = Some(hz));
        Ok(())
    }

    fn set_gains(&self, gains: &Gains) -> Result<(), SourceError> {
        let g = quantised_gains(&self.capabilities, gains)?;
        let mut requested = self
            .requested
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *requested = g;
        self.mailbox.post(|p| p.gains = Some(g));
        Ok(())
    }

    fn set_gain(&self, stage: &str, db: f64) -> Result<(), SourceError> {
        let q = self
            .capabilities
            .gain_stage(stage)
            .and_then(|s| s.quantise(db))
            .ok_or(SourceError::OutOfRange {
                what: "named gain (stage lna, vga or amp)",
                value: db,
            })?;
        let mut requested = self
            .requested
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match stage {
            "lna" => requested.lna_db = q,
            "vga" => requested.vga_db = q,
            _ => requested.amp_on = q > 0.0,
        }
        let g = *requested;
        self.mailbox.post(|p| p.gains = Some(g));
        Ok(())
    }

    fn stats(&self) -> Option<SourceStats> {
        let s = &self.stats;
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        Some(SourceStats {
            blocks: g(&s.delivered_blocks),
            samples: g(&s.delivered_samples),
            overruns: g(&s.transfers.dropped_transfers),
            dropped_samples: g(&s.transfers.dropped_samples),
            discarded_samples: g(&s.settle_discarded_samples),
        })
    }

    fn device_info(&self) -> Option<DeviceInfo> {
        Some(self.device.clone())
    }

    fn set_baseband_filter(&self, bandwidth_hz: f64) -> Result<(), SourceError> {
        let hz = checked_filter(&self.capabilities, bandwidth_hz)?;
        self.mailbox.post(|p| p.baseband_filter_hz = Some(hz));
        Ok(())
    }

    fn set_bias_tee(&self, enabled: bool) -> Result<(), SourceError> {
        self.mailbox.post(|p| p.bias_tee = Some(enabled));
        Ok(())
    }

    /// Streaming starts with the first read; nothing to do.
    fn start(&self) -> Result<(), SourceError> {
        Ok(())
    }

    fn stop(&self) -> Result<(), SourceError> {
        self.stopped.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunState {
    Idle,
    Streaming,
    Finished,
}

/// The HackRF One stream. See the [module docs](self).
pub struct HackRfSource {
    control: Arc<HackRfControl>,
    stats: Arc<HackRfStats>,
    info: HackRfDeviceInfo,
    pool: Arc<TransferPool>,
    tune: Tune,
    bias_tee: BiasTee,
    filter_explicit: bool,
    overload_fraction: f64,
    overloaded: bool,
    provenance: ProvenanceHandle,
    state: RunState,
    mailbox_seen: u64,
    pending_flags: Discontinuity,
    discard_below_seq: u64,
    next_index: u64,
    anchor: Option<SampleTime>,
    progress: (u64, Instant),
    // Declared last: dropped (closing the device) after everything above.
    device: Box<dyn RxDevice>,
}

#[cfg_attr(feature = "hackrf", allow(dead_code))]
fn not_available() -> SourceError {
    SourceError::NotAvailable {
        source_name: HackRfSource::NAME,
        reason: "this build has no HackRF driver; rebuild with the `hackrf` cargo feature (e.g. \
                 `cargo run -p hk-cli --features hackrf`), which links the system libhackrf"
            .into(),
    }
}

/// `HACKRF_ERROR_BUSY` (hackrf.h): "Resource is busy, possibly the device is already opened".
pub(crate) const HACKRF_ERROR_BUSY: i32 = -6;
/// `HACKRF_ERROR_LIBUSB` (hackrf.h): a libusb error, named by `hackrf_error_name` through
/// `libusb_strerror` of the library's last libusb error.
pub(crate) const HACKRF_ERROR_LIBUSB: i32 = -1000;
/// `libusb_strerror(LIBUSB_ERROR_ACCESS)` (libusb's English strings, the library default).
pub(crate) const LIBUSB_ACCESS_TEXT: &str = "Access denied (insufficient permissions)";
/// `libusb_strerror(LIBUSB_ERROR_BUSY)`.
const LIBUSB_BUSY_TEXT: &str = "Resource busy";

/// How a refused `hackrf_open_by_serial` is reported (T-892).
///
/// libhackrf returns `HACKRF_ERROR_LIBUSB` (-1000) for every libusb failure, and names it by
/// libusb's last error. When another process holds the HackRF, macOS libusb reports
/// `LIBUSB_ERROR_ACCESS` — **the same code** as a user without USB permissions (observed by
/// T-356's HIL: `Access denied (insufficient permissions) (-1000)`). Passed through verbatim, that
/// sent the user to fix permissions on a device that was merely in use. So:
///
/// - `HACKRF_ERROR_BUSY`, or a libusb "Resource busy": [`InUseCertainty::InUse`];
/// - a libusb "Access denied": [`InUseCertainty::InUseOrNotPermitted`] — the driver cannot tell,
///   so neither can we;
/// - anything else (not found, no memory, …): the plain [`SourceError::Device`].
///
/// `name` is `hackrf_error_name(rc)`; `device` names what was being opened.
pub(crate) fn open_failure(rc: i32, name: &str, device: String) -> SourceError {
    let driver_message = format!("{name} ({rc})");
    let certainty = if rc == HACKRF_ERROR_BUSY
        || (rc == HACKRF_ERROR_LIBUSB && name.contains(LIBUSB_BUSY_TEXT))
    {
        Some(InUseCertainty::InUse)
    } else if rc == HACKRF_ERROR_LIBUSB && name.contains(LIBUSB_ACCESS_TEXT) {
        Some(InUseCertainty::InUseOrNotPermitted)
    } else {
        None
    };
    match certainty {
        Some(certainty) => SourceError::DeviceInUse {
            source_name: HackRfSource::NAME,
            device,
            certainty,
            driver_message,
        },
        None => SourceError::Device {
            source_name: HackRfSource::NAME,
            operation: "hackrf_open_by_serial",
            message: driver_message,
        },
    }
}

/// Names the device an open was aimed at: the requested serial, or — for "the first HackRF" —
/// the serials enumeration found (`listed`; entries libhackrf could not read are `None`).
#[cfg_attr(not(any(test, feature = "hackrf")), allow(dead_code))]
pub(crate) fn describe_open_target(requested: Option<&str>, listed: &[Option<String>]) -> String {
    if let Some(serial) = requested {
        return format!("serial {serial}");
    }
    let known: Vec<&str> = listed.iter().flatten().map(String::as_str).collect();
    match (listed.len(), known.as_slice()) {
        (1, [serial]) => format!("serial {serial}"),
        (0, _) => "(the first HackRF found)".into(),
        (1, []) => "(the only HackRF found; its serial unreadable)".into(),
        (n, []) => format!("(the first of {n} HackRFs found; serials unreadable)"),
        (n, serials) => format!(
            "(the first of {n} HackRFs found: serial {})",
            serials.join(", serial ")
        ),
    }
}

impl HackRfSource {
    /// Driver name used in errors and capabilities.
    pub const NAME: &'static str = "hackrf-one";

    /// This build includes the libhackrf driver (feature `hackrf`).
    pub const fn available() -> bool {
        cfg!(feature = "hackrf")
    }

    /// Opens a HackRF One by serial (or the first found) with default settings
    /// ([`HackRfConfig::default`]).
    pub fn open(serial: Option<&str>) -> Result<Self, SourceError> {
        Self::open_with(&HackRfConfig {
            serial: serial.map(str::to_owned),
            ..HackRfConfig::default()
        })
    }

    /// Opens a HackRF One and applies `config` (streaming starts with the first read).
    #[cfg(feature = "hackrf")]
    pub fn open_with(config: &HackRfConfig) -> Result<Self, SourceError> {
        let (device, info) = ffi::LibHackRf::open(config.serial.as_deref())?;
        let transfer_bytes = ffi::transfer_buffer_size();
        Self::from_device(Box::new(device), info, config, transfer_bytes)
    }

    /// Opens a HackRF One: not available without the `hackrf` feature.
    #[cfg(not(feature = "hackrf"))]
    pub fn open_with(config: &HackRfConfig) -> Result<Self, SourceError> {
        let _ = config;
        Err(not_available())
    }

    /// The capability descriptor this source reports.
    pub fn capabilities_descriptor() -> SourceCapabilities {
        SourceCapabilities::hackrf_one()
    }

    #[cfg_attr(not(any(test, feature = "hackrf")), allow(dead_code))]
    pub(crate) fn from_device(
        mut device: Box<dyn RxDevice>,
        info: HackRfDeviceInfo,
        config: &HackRfConfig,
        transfer_bytes: usize,
    ) -> Result<Self, SourceError> {
        let caps = SourceCapabilities::hackrf_one();
        let center = checked_frequency(&caps, config.center_hz)?;
        let rate = checked_rate(&caps, config.sample_rate_hz)?;
        let gains = quantised_gains(&caps, &config.gains)?;
        let filter = match config.baseband_filter_hz {
            Some(hz) => checked_filter(&caps, hz)?,
            None => f64::from(device.compute_baseband_filter_bw((0.75 * rate) as u32)),
        };
        device.set_sample_rate(rate)?;
        device.set_baseband_filter(filter as u32)?;
        device.set_freq(center as u64)?;
        device.set_lna_gain(gains.lna_db as u32)?;
        device.set_vga_gain(gains.vga_db as u32)?;
        device.set_amp_enable(gains.amp_on)?;
        device.set_antenna_enable(config.bias_tee)?;
        let tune = Tune {
            center_hz: center,
            sample_rate_hz: rate,
            lna_db: gains.lna_db,
            vga_db: gains.vga_db,
            amp_on: gains.amp_on,
            bandwidth_hz: filter,
        };
        let stats = Arc::new(HackRfStats::default());
        let pool = Arc::new(TransferPool::new(
            config.queue_buffers.max(2),
            transfer_bytes.max(2),
            Arc::clone(&stats.transfers),
        ));
        // T-325: the bias tee was just commanded above, so the driver reports Off/On, not Unknown.
        let bias_tee = if config.bias_tee {
            BiasTee::On
        } else {
            BiasTee::Off
        };
        let provenance = ProvenanceHandle::new(provenance_record(&info, &tune, false, bias_tee));
        Ok(Self {
            control: Arc::new(HackRfControl {
                capabilities: caps,
                mailbox: ControlMailbox::new(),
                stopped: AtomicBool::new(false),
                requested: Mutex::new(gains),
                stats: Arc::clone(&stats),
                device: DeviceInfo {
                    driver: Self::NAME.into(),
                    device_id: info.device_id(),
                    hw: info.hw_description(),
                },
            }),
            stats,
            info,
            pool,
            tune,
            bias_tee,
            filter_explicit: config.baseband_filter_hz.is_some(),
            overload_fraction: config.overload_clip_fraction,
            overloaded: false,
            provenance,
            state: RunState::Idle,
            mailbox_seen: 0,
            pending_flags: Discontinuity::STREAM_START,
            discard_below_seq: 0,
            next_index: 0,
            anchor: None,
            progress: (0, Instant::now()),
            device,
        })
    }

    /// The device identity.
    pub fn device_info(&self) -> &HackRfDeviceInfo {
        &self.info
    }

    /// Live counters (keep a clone before moving the source into a pipeline).
    pub fn stats(&self) -> Arc<HackRfStats> {
        Arc::clone(&self.stats)
    }

    /// The provenance in force for the next block (before any pending change).
    pub fn provenance(&self) -> ProvenanceHandle {
        self.provenance.clone()
    }

    /// The bias-tee state this driver has commanded (T-325).
    pub fn bias_tee(&self) -> BiasTee {
        self.bias_tee
    }

    fn mint(&mut self) -> Discontinuity {
        let prev = self.provenance.clone();
        self.provenance = ProvenanceHandle::new(provenance_record(
            &self.info,
            &self.tune,
            self.overloaded,
            self.bias_tee,
        ));
        Discontinuity::between(prev.get(), self.provenance.get())
    }

    fn begin(&mut self) -> Result<(), SourceError> {
        self.device.start_rx(Arc::clone(&self.pool))?;
        self.state = RunState::Streaming;
        self.progress = (self.pool.next_seq(), Instant::now());
        Ok(())
    }

    fn finish(&mut self) {
        if self.state == RunState::Streaming {
            let _ = self.device.stop_rx();
        }
        self.state = RunState::Finished;
    }

    /// Applies posted controls (capture thread, at a block boundary).
    fn apply_pending(&mut self) -> Result<(), SourceError> {
        let Some(change) = self.control.mailbox.take(&mut self.mailbox_seen) else {
            return Ok(());
        };
        let before = self.tune.clone();
        let bias_before = self.bias_tee;
        let result = self.apply_change(&change);
        // Whatever reached the device is in the provenance, even when a later call failed.
        self.discard_below_seq = self.pool.next_seq() + 1;
        self.stats.control_changes.fetch_add(1, Ordering::Relaxed);
        if self.tune != before {
            if self.tune.sample_rate_hz != before.sample_rate_hz {
                self.anchor = None;
            }
            self.overloaded = false;
            let flags = self.mint();
            self.pending_flags |= flags;
        } else if self.bias_tee != bias_before {
            // T-325: switching the bias tee changes the antenna port's DC state and, with an
            // active antenna, the gain structure. It is a provenance change in its own right,
            // even though `tune` is untouched — otherwise the blocks after it would keep
            // claiming the old bias-tee state.
            let flags = self.mint();
            self.pending_flags |= flags;
        }
        result
    }

    fn apply_change(&mut self, change: &super::PendingControl) -> Result<(), SourceError> {
        if let Some(hz) = change.sample_rate_hz {
            self.device.set_sample_rate(hz)?;
            self.tune.sample_rate_hz = hz;
            let bw = if self.filter_explicit {
                self.tune.bandwidth_hz
            } else {
                f64::from(self.device.compute_baseband_filter_bw((0.75 * hz) as u32))
            };
            self.device.set_baseband_filter(bw as u32)?;
            self.tune.bandwidth_hz = bw;
        }
        if let Some(bw) = change.baseband_filter_hz {
            self.device.set_baseband_filter(bw as u32)?;
            self.tune.bandwidth_hz = bw;
            self.filter_explicit = true;
        }
        if let Some(g) = change.gains {
            self.device.set_lna_gain(g.lna_db as u32)?;
            self.tune.lna_db = g.lna_db;
            self.device.set_vga_gain(g.vga_db as u32)?;
            self.tune.vga_db = g.vga_db;
            self.device.set_amp_enable(g.amp_on)?;
            self.tune.amp_on = g.amp_on;
        }
        if let Some(hz) = change.center_hz {
            self.device.set_freq(hz as u64)?;
            self.tune.center_hz = hz;
        }
        if let Some(on) = change.bias_tee {
            self.device.set_antenna_enable(on)?;
            self.bias_tee = if on { BiasTee::On } else { BiasTee::Off };
        }
        Ok(())
    }

    fn check_stall(&mut self) -> Result<(), SourceError> {
        let seq = self.pool.next_seq();
        if seq != self.progress.0 {
            self.progress = (seq, Instant::now());
            return Ok(());
        }
        let idle = self.progress.1.elapsed();
        if idle > STALL_TIMEOUT {
            return Err(SourceError::Device {
                source_name: Self::NAME,
                operation: "receive",
                message: format!(
                    "no samples for {:.1} s (streaming: {}); the device may be unplugged or USB \
                     stalled",
                    idle.as_secs_f64(),
                    self.device.is_streaming()
                ),
            });
        }
        Ok(())
    }

    /// The next block: `fill` receives its interleaved ci8 bytes.
    fn next_block(
        &mut self,
        fill: &mut dyn FnMut(&[u8]),
    ) -> Result<Option<BlockHeader>, SourceError> {
        loop {
            if self.control.stopped.load(Ordering::SeqCst) || self.state == RunState::Finished {
                self.finish();
                return Ok(None);
            }
            if self.state == RunState::Idle {
                self.begin()?;
            }
            self.apply_pending()?;
            let pool = Arc::clone(&self.pool);
            let Some(taken) = pool.take() else {
                self.check_stall()?;
                pool.wait(Duration::from_millis(50));
                continue;
            };
            let meta = taken.meta;
            let n = meta.len / 2;
            if meta.seq < self.discard_below_seq {
                self.stats
                    .settle_discarded_samples
                    .fetch_add(n as u64, Ordering::Relaxed);
                continue;
            }
            if n == 0 {
                continue;
            }
            let bytes = &taken.bytes()[..2 * n];
            let clipped = bytes.iter().filter(|&&b| b == 0x80 || b == 0x7f).count() as u64;
            self.stats
                .clipped_components
                .fetch_add(clipped, Ordering::Relaxed);
            let mut flags = std::mem::replace(&mut self.pending_flags, Discontinuity::NONE);
            if clipped as f64 > self.overload_fraction * bytes.len() as f64 {
                self.stats.overload_blocks.fetch_add(1, Ordering::Relaxed);
                if !self.overloaded {
                    self.overloaded = true;
                    flags |= self.mint();
                }
            }
            let index = meta.byte_offset / 2;
            let dropped = index.saturating_sub(self.next_index);
            if dropped > 0 {
                flags |= Discontinuity::GAP;
            }
            let fs = self.tune.sample_rate_hz;
            let anchor = *self.anchor.get_or_insert(SampleTime {
                sample_index: index + n as u64,
                host_time: Timestamp::from_unix_nanos(meta.arrival_ns),
            });
            let header = BlockHeader {
                time: SampleTime {
                    sample_index: index,
                    host_time: anchor.time_of(index, fs),
                },
                provenance: self.provenance.clone(),
                discontinuity: flags,
                dropped_before: dropped,
            };
            fill(bytes);
            self.next_index = index + n as u64;
            self.stats
                .delivered_samples
                .fetch_add(n as u64, Ordering::Relaxed);
            self.stats.delivered_blocks.fetch_add(1, Ordering::Relaxed);
            return Ok(Some(header));
        }
    }
}

fn provenance_record(
    info: &HackRfDeviceInfo,
    tune: &Tune,
    overload: bool,
    bias_tee: BiasTee,
) -> Provenance {
    Provenance {
        device_id: info.device_id(),
        tune: tune.clone(),
        overload,
        quantisation_limited: false,
        noise_sigma_lsb: None,
        temperature_c: None,
        antenna_port: Some(ANTENNA_UNKNOWN.into()),
        // T-325: the state this driver commanded with `set_antenna_enable`. libhackrf offers no
        // read-back, so this is what the bias tee was last set to, never a measured voltage; it
        // is `Off`/`On` and never `Unknown`, because the driver always knows what it commanded.
        bias_tee,
        clock_source: ClockSource::Internal,
        clock_locked: true,
        calibration_state_ref: None,
        spur_mask_ref: None,
        timestamp_method: TimestampMethod::HostArrival,
        timestamp_error_budget_ns: None,
        capture_artefacts: Vec::new(),
    }
}

impl Source for HackRfSource {
    fn capabilities(&self) -> &SourceCapabilities {
        &self.control.capabilities
    }

    fn control(&self) -> Arc<dyn SourceControl> {
        self.control.clone()
    }

    /// A live radio keeps streaming: never pausable (lossless mode is refused).
    fn pausable(&self) -> bool {
        false
    }

    fn read_block(
        &mut self,
        samples: &mut Vec<Complex32>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        samples.clear();
        self.next_block(&mut |bytes| {
            samples.extend(bytes.chunks_exact(2).map(|c| {
                Complex32::new(f32::from(c[0] as i8) / 128.0, f32::from(c[1] as i8) / 128.0)
            }))
        })
    }

    fn read_block_ci8(
        &mut self,
        samples: &mut Vec<Complex<i8>>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        samples.clear();
        self.next_block(&mut |bytes| {
            samples.extend(
                bytes
                    .chunks_exact(2)
                    .map(|c| Complex::new(c[0] as i8, c[1] as i8)),
            )
        })
    }
}

impl Drop for HackRfSource {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::thread::JoinHandle;

    use super::super::BasebandFilters;
    use super::*;

    /// The nearest-lower MAX2837 bandwidth (the fake device's filter table).
    fn table_filter_bw(hz: u32) -> u32 {
        match SourceCapabilities::hackrf_one().baseband_filter {
            Some(BasebandFilters::Discrete(v)) => v
                .iter()
                .map(|&w| w as u32)
                .filter(|&w| w <= hz)
                .max()
                .unwrap_or(v[0] as u32),
            _ => hz,
        }
    }

    /// Settings the fake radio currently has, plus a log of calls.
    #[derive(Debug, Default)]
    struct FakeState {
        freq: u64,
        lna: u32,
        vga: u32,
        amp: bool,
        antenna: bool,
        clip: bool,
        log: Vec<String>,
    }

    /// A fake radio: a thread fills 4096-byte transfers whose I code is `lna / 8` and Q code
    /// `freq / 1 MHz mod 100`, re-reading the settings halfway through each transfer (so a
    /// transfer in flight during a change mixes both, as on the real device).
    struct FakeDevice {
        state: Arc<Mutex<FakeState>>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
        period: Duration,
    }

    impl FakeDevice {
        fn new(period: Duration) -> (Self, Arc<Mutex<FakeState>>) {
            let state = Arc::new(Mutex::new(FakeState::default()));
            (
                Self {
                    state: Arc::clone(&state),
                    stop: Arc::new(AtomicBool::new(false)),
                    thread: None,
                    period,
                },
                state,
            )
        }

        fn log(&self, s: String) {
            self.state.lock().unwrap().log.push(s);
        }
    }

    fn codes(s: &FakeState) -> (u8, u8) {
        if s.clip {
            return (0x7f, 0x80);
        }
        ((s.lna / 8) as u8, (s.freq / 1_000_000 % 100) as u8)
    }

    impl RxDevice for FakeDevice {
        fn set_freq(&mut self, hz: u64) -> Result<(), SourceError> {
            self.state.lock().unwrap().freq = hz;
            self.log(format!("freq {hz}"));
            Ok(())
        }
        fn set_sample_rate(&mut self, hz: f64) -> Result<(), SourceError> {
            self.log(format!("rate {hz}"));
            Ok(())
        }
        fn set_baseband_filter(&mut self, hz: u32) -> Result<(), SourceError> {
            self.log(format!("filter {hz}"));
            Ok(())
        }
        fn compute_baseband_filter_bw(&self, hz: u32) -> u32 {
            table_filter_bw(hz)
        }
        fn set_lna_gain(&mut self, db: u32) -> Result<(), SourceError> {
            self.state.lock().unwrap().lna = db;
            Ok(())
        }
        fn set_vga_gain(&mut self, db: u32) -> Result<(), SourceError> {
            self.state.lock().unwrap().vga = db;
            Ok(())
        }
        fn set_amp_enable(&mut self, on: bool) -> Result<(), SourceError> {
            self.state.lock().unwrap().amp = on;
            Ok(())
        }
        fn set_antenna_enable(&mut self, on: bool) -> Result<(), SourceError> {
            self.state.lock().unwrap().antenna = on;
            Ok(())
        }
        fn start_rx(&mut self, pool: Arc<TransferPool>) -> Result<(), SourceError> {
            let (state, stop, period) =
                (Arc::clone(&self.state), Arc::clone(&self.stop), self.period);
            self.log("start".into());
            self.thread = Some(std::thread::spawn(move || {
                let mut buf = vec![0u8; 4096];
                while !stop.load(Ordering::SeqCst) {
                    let (i, q) = codes(&state.lock().unwrap());
                    for c in buf[..2048].chunks_exact_mut(2) {
                        c[0] = i;
                        c[1] = q;
                    }
                    std::thread::sleep(period / 2);
                    let (i, q) = codes(&state.lock().unwrap());
                    for c in buf[2048..].chunks_exact_mut(2) {
                        c[0] = i;
                        c[1] = q;
                    }
                    std::thread::sleep(period / 2);
                    pool.on_transfer(&buf, Timestamp::now().as_unix_nanos());
                }
            }));
            Ok(())
        }
        fn stop_rx(&mut self) -> Result<(), SourceError> {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(t) = self.thread.take() {
                t.join().unwrap();
            }
            self.log("stop".into());
            Ok(())
        }
        fn is_streaming(&self) -> bool {
            self.thread.is_some()
        }
    }

    impl Drop for FakeDevice {
        fn drop(&mut self) {
            let _ = self.stop_rx();
            self.log("close".into());
        }
    }

    fn fake_info() -> HackRfDeviceInfo {
        HackRfDeviceInfo {
            serial: "0000000000000000fake0000000000ab".into(),
            board_id: 2,
            board_name: "HackRF One".into(),
            board_rev: None,
            firmware: "fake".into(),
            usb_api_version: 0x0110,
            library_version: "0.9.2".into(),
            library_release: "fake".into(),
        }
    }

    fn open_fake(config: &HackRfConfig) -> (HackRfSource, Arc<Mutex<FakeState>>) {
        let (dev, state) = FakeDevice::new(Duration::from_millis(4));
        let src = HackRfSource::from_device(Box::new(dev), fake_info(), config, 4096).unwrap();
        (src, state)
    }

    fn fm_config() -> HackRfConfig {
        HackRfConfig {
            center_hz: 100.75e6,
            sample_rate_hz: 2e6,
            gains: Gains {
                lna_db: 32.0,
                vga_db: 30.0,
                amp_on: true,
            },
            ..HackRfConfig::default()
        }
    }

    const DEADLINE: Duration = Duration::from_secs(20);

    #[test]
    fn blocks_are_contiguous_with_hackrf_provenance() {
        let (mut src, state) = open_fake(&fm_config());
        assert!(!src.pausable(), "a live radio cannot pause");
        assert_eq!(src.capabilities().driver, "hackrf-one");
        let mut buf = Vec::new();
        let mut next = 0u64;
        let mut last_t = i64::MIN;
        for k in 0..20 {
            let h = src.read_block_ci8(&mut buf).unwrap().unwrap();
            assert_eq!(buf.len(), 2048);
            assert_eq!(h.first_sample(), next, "contiguous");
            assert_eq!(h.dropped_before, 0);
            if k == 0 {
                assert_eq!(h.discontinuity, Discontinuity::STREAM_START);
            } else {
                assert!(h.discontinuity.is_empty(), "{:?}", h.discontinuity);
            }
            let t = h.time.host_time.as_unix_nanos();
            assert!(t > last_t, "times increase");
            last_t = t;
            next = h.first_sample() + buf.len() as u64;
            let p = h.provenance.get();
            assert_eq!(p.device_id, "hackrf:0000000000000000fake0000000000ab");
            assert_eq!(p.antenna_port.as_deref(), Some("unknown"));
            assert_eq!(p.timestamp_method, TimestampMethod::HostArrival);
            assert_eq!(
                (
                    p.tune.center_hz,
                    p.tune.sample_rate_hz,
                    p.tune.lna_db,
                    p.tune.vga_db
                ),
                (100.75e6, 2e6, 32.0, 30.0)
            );
            assert!(p.tune.amp_on);
            assert_eq!(
                p.tune.bandwidth_hz, 1.75e6,
                "0.75 * 2 Msps -> 1.75 MHz filter"
            );
            assert!(!p.overload);
        }
        let s = state.lock().unwrap();
        assert!(!s.antenna, "bias tee off by default");
        assert!(s.log.contains(&"freq 100750000".to_string()));
    }

    #[test]
    fn controls_apply_at_a_block_boundary_and_no_block_mixes_settings() {
        let (mut src, state) = open_fake(&fm_config());
        let control = src.control();
        let mut buf = Vec::new();
        let start = Instant::now();
        let mut changed = None;
        let mut blocks = 0;
        while blocks < 60 {
            assert!(start.elapsed() < DEADLINE, "watchdog");
            let h = src.read_block_ci8(&mut buf).unwrap().unwrap();
            blocks += 1;
            if blocks == 10 {
                control
                    .set_gains(&Gains {
                        lna_db: 16.0,
                        vga_db: 30.0,
                        amp_on: true,
                    })
                    .unwrap();
                control.tune(433.92e6).unwrap();
            }
            let p = h.provenance.get();
            let want = (
                (p.tune.lna_db as u32 / 8) as i8,
                (433.92e6 as u64 / 1_000_000 % 100) as i8,
            );
            let old = ((32 / 8) as i8, 0i8);
            let expect = if p.tune.center_hz == 433.92e6 {
                want
            } else {
                old
            };
            for s in &buf {
                assert_eq!((s.re, s.im), expect, "block {blocks} mixes settings");
            }
            if h.discontinuity.contains(Discontinuity::RETUNE) {
                assert!(changed.is_none(), "one change block");
                assert!(h.discontinuity.contains(Discontinuity::GAIN_CHANGE));
                assert!(h.discontinuity.contains(Discontinuity::GAP));
                assert!(h.dropped_before >= 2048, "settle discard is a gap");
                changed = Some(h.dropped_before);
            }
        }
        let dropped = changed.expect("the change reached a block");
        let stats = src.stats();
        assert_eq!(
            stats.settle_discarded_samples.load(Ordering::Relaxed),
            dropped,
            "the gap is the settle discard, not a USB drop"
        );
        assert_eq!(stats.transfers.dropped_transfers.load(Ordering::Relaxed), 0);
        assert!(
            state
                .lock()
                .unwrap()
                .log
                .contains(&"freq 433920000".to_string())
        );
    }

    #[test]
    fn out_of_range_controls_are_refused_and_gains_quantised() {
        let (mut src, _state) = open_fake(&fm_config());
        let control = src.control();
        assert!(matches!(
            control.tune(500e3),
            Err(SourceError::OutOfRange { .. })
        ));
        assert!(control.tune(f64::NAN).is_err());
        assert!(control.set_sample_rate(40e6).is_err());
        assert!(control.set_baseband_filter(4e6).is_err());
        let bad = Gains {
            lna_db: 48.0,
            vga_db: 20.0,
            amp_on: false,
        };
        assert!(control.set_gains(&bad).is_err());
        control
            .set_gains(&Gains {
                lna_db: 30.0,
                vga_db: 31.0,
                amp_on: false,
            })
            .unwrap();
        let mut buf = Vec::new();
        let start = Instant::now();
        loop {
            assert!(start.elapsed() < DEADLINE, "watchdog");
            let h = src.read_block_ci8(&mut buf).unwrap().unwrap();
            if h.discontinuity.contains(Discontinuity::GAIN_CHANGE) {
                assert_eq!(
                    (h.provenance.tune.lna_db, h.provenance.tune.vga_db),
                    (24.0, 30.0)
                );
                break;
            }
        }
        assert!(
            HackRfSource::from_device(
                Box::new(FakeDevice::new(Duration::from_millis(4)).0),
                fake_info(),
                &HackRfConfig {
                    sample_rate_hz: 1e6,
                    ..HackRfConfig::default()
                },
                4096
            )
            .is_err()
        );
    }

    #[test]
    fn named_gains_stats_and_identity_through_the_generic_control() {
        let (mut src, _state) = open_fake(&fm_config());
        let control = src.control();
        assert!(control.set_gain("mixer", 10.0).is_err(), "unknown stage");
        assert!(control.set_gain("lna", 48.0).is_err(), "out of range");
        control.set_gain("vga", 41.0).unwrap();
        control.set_gain("amp", 0.0).unwrap();
        let mut buf = Vec::new();
        let start = Instant::now();
        let h = loop {
            assert!(start.elapsed() < DEADLINE, "watchdog");
            let h = src.read_block_ci8(&mut buf).unwrap().unwrap();
            if h.discontinuity.contains(Discontinuity::GAIN_CHANGE) {
                break h;
            }
        };
        let t = &h.provenance.tune;
        assert_eq!(
            (t.lna_db, t.vga_db, t.amp_on),
            (32.0, 40.0, false),
            "merged stages"
        );
        let stats = control.stats().expect("stats");
        assert!(stats.blocks > 0 && stats.samples >= 2048 * stats.blocks);
        assert_eq!(stats.overruns, 0);
        assert_eq!(stats.discarded_samples, h.dropped_before);
        let info = control.device_info().unwrap();
        assert_eq!(info.device_id, "hackrf:0000000000000000fake0000000000ab");
        assert!(info.hw.contains("antenna unknown"));
        assert!(
            control.sweep_capability().is_none(),
            "sweep mode not offered (yet)"
        );
    }

    #[test]
    fn generic_open_requests_map_named_gains() {
        let request = OpenRequest {
            device: Some("abc".into()),
            center_hz: 433.92e6,
            sample_rate_hz: 2e6,
            gains: vec![
                super::super::NamedGain::new("lna", 30.0),
                super::super::NamedGain::new("amp", 11.0),
            ],
            baseband_filter_hz: None,
            bias_tee: false,
        };
        let c = HackRfConfig::from_request(&request).unwrap();
        assert_eq!(c.serial.as_deref(), Some("abc"));
        assert_eq!(
            (c.gains.lna_db, c.gains.vga_db, c.gains.amp_on),
            (24.0, 20.0, true)
        );
        let mut bad = request.clone();
        bad.gains.push(super::super::NamedGain::new("mixer", 3.0));
        assert!(HackRfConfig::from_request(&bad).is_err());
        let driver = HackRfDriver;
        assert_eq!(driver.name(), "hackrf");
        assert_eq!(driver.capabilities().driver, "hackrf-one");
        if !driver.available() {
            assert!(matches!(
                driver.open(&request),
                Err(SourceError::NotAvailable { .. })
            ));
        }
    }

    #[test]
    fn clipping_marks_the_tune_state_overloaded_until_the_next_gain_change() {
        let (mut src, state) = open_fake(&fm_config());
        let control = src.control();
        let mut buf = Vec::new();
        let start = Instant::now();
        let read = |src: &mut HackRfSource, buf: &mut Vec<Complex<i8>>| {
            assert!(start.elapsed() < DEADLINE, "watchdog");
            src.read_block_ci8(buf).unwrap().unwrap()
        };
        let h = read(&mut src, &mut buf);
        assert!(!h.provenance.overload);
        state.lock().unwrap().clip = true;
        let over = loop {
            let h = read(&mut src, &mut buf);
            if h.provenance.overload {
                break h;
            }
        };
        assert!(
            over.discontinuity
                .contains(Discontinuity::PROVENANCE_CHANGE)
        );
        assert!(!over.discontinuity.contains(Discontinuity::GAIN_CHANGE));
        state.lock().unwrap().clip = false;
        for _ in 0..5 {
            assert!(read(&mut src, &mut buf).provenance.overload, "sticky");
        }
        control
            .set_gains(&Gains {
                lna_db: 8.0,
                vga_db: 30.0,
                amp_on: false,
            })
            .unwrap();
        loop {
            let h = read(&mut src, &mut buf);
            if h.discontinuity.contains(Discontinuity::GAIN_CHANGE) {
                assert!(!h.provenance.overload, "a new gain state starts clean");
                break;
            }
        }
        assert!(src.stats().overload_blocks.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn a_slow_reader_gets_counted_drops_and_an_exact_gap() {
        let (mut src, _state) = open_fake(&HackRfConfig {
            queue_buffers: 2,
            ..fm_config()
        });
        let mut buf = Vec::new();
        let first = src.read_block_ci8(&mut buf).unwrap().unwrap();
        let mut next = first.first_sample() + buf.len() as u64;
        std::thread::sleep(Duration::from_millis(200));
        let stats = src.stats();
        let dropped = stats.transfers.dropped_samples.load(Ordering::Relaxed);
        assert!(dropped > 0, "the pool overflowed");
        // The fake keeps producing while this loop catches up, so drops can still be counted
        // after the first snapshot: read until the gaps match the counter as it stands now.
        let mut gaps = 0;
        let start = Instant::now();
        loop {
            assert!(start.elapsed() < DEADLINE, "watchdog");
            let h = src.read_block_ci8(&mut buf).unwrap().unwrap();
            if h.dropped_before > 0 {
                assert!(h.discontinuity.contains(Discontinuity::GAP));
            }
            assert_eq!(h.first_sample(), next + h.dropped_before);
            gaps += h.dropped_before;
            next = h.first_sample() + buf.len() as u64;
            let counted = stats.transfers.dropped_samples.load(Ordering::Relaxed);
            assert!(gaps <= counted, "a gap never exceeds the counted drops");
            if gaps >= dropped && gaps == counted {
                break;
            }
        }
    }

    #[test]
    fn stop_ends_the_stream_and_drop_closes_the_device() {
        let (mut src, state) = open_fake(&fm_config());
        let control = src.control();
        let mut buf = Vec::new();
        assert!(src.read_block_ci8(&mut buf).unwrap().is_some());
        control.stop().unwrap();
        assert!(src.read_block_ci8(&mut buf).unwrap().is_none());
        assert!(src.read_block(&mut Vec::new()).unwrap().is_none());
        assert!(state.lock().unwrap().log.contains(&"stop".to_string()));
        drop(src);
        let log = &state.lock().unwrap().log;
        assert_eq!(log.last().map(String::as_str), Some("close"));
    }

    /// T-892: libhackrf's open codes are classified honestly — busy is in use, libusb's access
    /// error is "in use, or not permitted" (the two are one code), everything else stays a plain
    /// device error.
    #[test]
    fn open_failures_are_classified_by_what_the_driver_can_actually_tell() {
        let serial = "0000000000000000a06063c8234a8e5f";
        let target = describe_open_target(Some(serial), &[]);
        let access = open_failure(
            HACKRF_ERROR_LIBUSB,
            "Access denied (insufficient permissions)",
            target.clone(),
        );
        assert!(matches!(
            &access,
            SourceError::DeviceInUse { certainty: InUseCertainty::InUseOrNotPermitted, device, .. }
                if device == &format!("serial {serial}")
        ));
        let text = access.to_string();
        assert!(text.contains(serial), "{text}");
        assert!(
            text.contains("in use by another process, or not permitted"),
            "{text}"
        );
        assert!(
            text.contains("Access denied (insufficient permissions) (-1000)"),
            "{text}"
        );

        for (rc, name) in [
            (HACKRF_ERROR_BUSY, "HACKRF_ERROR_BUSY"),
            (HACKRF_ERROR_LIBUSB, "Resource busy"),
        ] {
            let e = open_failure(rc, name, target.clone());
            assert!(
                matches!(
                    e,
                    SourceError::DeviceInUse {
                        certainty: InUseCertainty::InUse,
                        ..
                    }
                ),
                "{rc} {name}: {e:?}"
            );
            assert!(!e.to_string().contains("not permitted"), "{e}");
        }
        for (rc, name) in [
            (-5, "HACKRF_ERROR_NOT_FOUND"),
            (
                HACKRF_ERROR_LIBUSB,
                "No such device (it may have been disconnected)",
            ),
        ] {
            assert!(matches!(
                open_failure(rc, name, target.clone()),
                SourceError::Device {
                    operation: "hackrf_open_by_serial",
                    ..
                }
            ));
        }
    }

    #[test]
    fn an_open_target_is_named_as_specifically_as_enumeration_allows() {
        let s = |v: &str| Some(v.to_owned());
        assert_eq!(describe_open_target(Some("abc"), &[s("xyz")]), "serial abc");
        assert_eq!(describe_open_target(None, &[s("abc")]), "serial abc");
        assert_eq!(describe_open_target(None, &[]), "(the first HackRF found)");
        assert_eq!(
            describe_open_target(None, &[None]),
            "(the only HackRF found; its serial unreadable)"
        );
        assert_eq!(
            describe_open_target(None, &[s("abc"), s("def")]),
            "(the first of 2 HackRFs found: serial abc, serial def)"
        );
        assert_eq!(
            describe_open_target(None, &[None, None]),
            "(the first of 2 HackRFs found; serials unreadable)"
        );
    }

    #[test]
    fn open_without_the_feature_reports_not_available() {
        if HackRfSource::available() {
            return;
        }
        let err = HackRfSource::open(None)
            .err()
            .expect("no driver in this build");
        assert!(matches!(
            err,
            SourceError::NotAvailable {
                source_name: "hackrf-one",
                ..
            }
        ));
        assert!(err.to_string().contains("hackrf"));
        assert_eq!(HackRfSource::capabilities_descriptor().adc_bits, 8);
    }

    /// Receive only: the FFI declares no transmit (or sweep/flash/CPLD) function.
    #[test]
    fn ffi_declares_no_transmit_function() {
        let text = include_str!("hackrf/ffi.rs");
        let declared: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("fn hackrf_"))
            .collect();
        assert!(declared.len() >= 20, "{declared:?}");
        for line in declared {
            let name = line.trim_start_matches("fn ").split('(').next().unwrap();
            for banned in [
                "tx", "sweep", "spiflash", "cpld", "si5351", "max2837", "rffc5071",
            ] {
                assert!(!name.contains(banned), "{name} must not be bound");
            }
        }
        assert!(!text.contains("hackrf_start_tx"));
        assert!(!text.contains("hackrf_set_txvga_gain"));
    }

    #[test]
    fn hw_description_names_serial_firmware_and_antenna() {
        let hw = fake_info().hw_description();
        assert!(
            hw.contains("serial 0000000000000000fake0000000000ab"),
            "{hw}"
        );
        assert!(hw.contains("firmware fake (USB API 1.10)"), "{hw}");
        assert!(hw.contains("antenna unknown"), "{hw}");
    }
}
