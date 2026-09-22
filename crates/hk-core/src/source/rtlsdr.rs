//! RTL-SDR source (C01, T-514): librtlsdr receive behind the [`Source`] / [`SourceControl`]
//! split, for the NooElec NESDR Nano 3 (RTL2832U + Rafael Micro R820T) and other R820T dongles.
//! **Receive only** — the RTL2832U has no transmitter, and [`ffi`](self) binds no EEPROM-write or
//! test-mode call either.
//!
//! # Build
//!
//! The driver needs cargo feature **`rtlsdr`** (off by default), which links the system librtlsdr
//! (`build.rs`: pkg-config `librtlsdr`, or `RTLSDR_LIB_DIR`). Without it [`RtlSdrSource::open`]
//! returns [`SourceError::NotAvailable`], so CI builds and tests with no librtlsdr installed. The
//! same shape as the `hackrf` feature, for the same reasons: link errors at build time, no
//! hand-maintained symbol table, and no FFI at all in the default build. Everything below except
//! [`ffi`](self) is compiled either way and tested against a fake device.
//!
//! **SoapySDR is not involved.** This is a native librtlsdr binding, one process per device.
//!
//! # What an RTL-SDR actually is, and is not (the point of this module)
//!
//! The capability descriptor [`SourceCapabilities::rtl_sdr_r820t`] states the real front end, so
//! nothing upstream offers the user something the hardware cannot do:
//!
//! - **25 MHz – 1.75 GHz**, not 1 MHz – 6 GHz. The R820T's own range; there is no HF below it
//!   without a converter, and nothing above it at all. Direct-sampling ("HF") mode is *not*
//!   offered: it changes what the samples mean, and this driver never silently switches the
//!   thing being measured under a measurement.
//! - **A short list of sample rates, topping out at 2.4 Msps.** See [`R820T_RATES_HZ`]: every
//!   rate offered is exactly realisable on the 28.8 MHz clock *and* known to stream without
//!   losses over USB. librtlsdr will accept 3.2 Msps and the dongle will drop samples; offering
//!   it would be offering detail the front end cannot deliver.
//! - **One gain knob, 29 discrete steps, 0 – 49.6 dB** ([`R820T_GAINS_DB`]) — the R820T's
//!   combined LNA + mixer + VGA gain, which librtlsdr sets as a single value. It is reported as
//!   one gain stage named `lna`, and the value that reaches
//!   [`hk_model::Tune::lna_db`] is the one the **device reports back** after snapping to its own
//!   table, never the value that was asked for. `vga_db` is 0 and `amp_on` false because those
//!   stages do not exist here.
//! - **No bias tee.** librtlsdr can toggle GPIO0 on dongles wired for it, with no read-back and
//!   no way to tell a board that has the circuit from one that does not. The Nano 3 has no bias
//!   tee, so [`SourceCapabilities::bias_tee`] is false, [`SourceControl::set_bias_tee`] is
//!   [`SourceError::Unsupported`], and provenance reports [`BiasTee::Unknown`] — **not `Off`**:
//!   this driver never drives that pin, so it cannot state what DC is on the port (T-325).
//! - **No selectable baseband filter, no RF amplifier, no external clock input, no hardware
//!   timestamps.** All reported as absent rather than faked. `Tune::bandwidth_hz` carries the
//!   sample rate, which is the width the stream actually delivers.
//! - **The tuner AGC and the RTL2832U's digital AGC are switched off at open** and never move on
//!   their own, so the gain state in provenance stays true for the whole block it describes.
//!
//! # Data path (no allocation per block)
//!
//! librtlsdr async callback → [`TransferPool`] (the same fixed-buffer, lock-free hand-over the
//! HackRF driver uses) → the capture thread's [`Source::read_block_ci8`]. `rtlsdr_read_async`
//! blocks its caller, so [`ffi`](self) runs it on a worker thread and cancels it on stop.
//!
//! - **Native format is `cu8`** (offset binary, 128 = zero), so [`Source::read_block_ci8`]
//!   re-centres each byte to signed — a constant offset, lossless — and
//!   [`Source::read_block`] divides the same signed code by 128. The two paths therefore agree
//!   exactly; the residual half-LSB DC that convention leaves is below the quantisation floor of
//!   an 8-bit ADC.
//! - **Sample counter:** the transfer's byte offset in the stream, dropped transfers included, so
//!   a drop is a [`Discontinuity::GAP`] with an exact `dropped_before`.
//! - **Time:** [`TimestampMethod::HostArrival`]; the first block's arrival anchors its last
//!   sample and later samples are extrapolated by count at the tuned rate (re-anchored after a
//!   rate change). Times never go backwards.
//! - **Controls** ([`RtlSdrControl`]) are validated when posted and applied by the capture thread
//!   at the next block boundary through [`ControlMailbox`]; transfers that arrived before the
//!   change finished, plus the one in flight, are discarded, so **no block mixes two settings**.
//! - **The R820T needs time to lock.** On a cold open the PLL can take ~10 s to produce samples
//!   (librtlsdr prints `[R82XX] PLL not locked!`), so the first block has its own, much longer
//!   timeout ([`RtlSdrConfig::first_block_timeout`], 20 s) before the stream calls the device
//!   stalled. After the first block the ordinary 3 s stall timeout applies. A slow first block is
//!   not a failure.
//! - **Overload:** a block whose clipped components (codes 0 or 255) exceed
//!   [`RtlSdrConfig::overload_clip_fraction`] mints provenance with `overload = true`, sticky
//!   until the next tune or gain change, as `Provenance::overload` specifies.
//! - **Not pausable:** the radio keeps streaming, so lossless backpressure is refused.
//!
//! # Hardware test
//!
//! `cargo test -p hk-core --features rtlsdr --test rtlsdr_hil -- --ignored --nocapture`
//! (receive only; check the device is free with `rtl_test -t` first, and never open the HackRF
//! from the same process).

#[cfg(feature = "rtlsdr")]
mod ffi;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use hk_model::{BiasTee, ClockSource, Provenance, SampleTime, Timestamp, TimestampMethod, Tune};
use num_complex::{Complex, Complex32};
use serde_json::{Value, json};

use super::hackrf::pool::{TransferCounters, TransferPool};
use super::{
    ControlMailbox, DeviceInfo, Gains, OpenRequest, Source, SourceCapabilities, SourceControl,
    SourceDriver, SourceError, SourceStats,
};
use crate::block::{BlockHeader, Discontinuity, ProvenanceHandle};

/// The R820T's 29 gain steps, dB, as `rtlsdr_get_tuner_gains` reports them (tenths of a dB there).
///
/// Verified against the connected NESDR Nano 3 with `rtl_test -d 0`, which printed exactly this
/// list. The steps are **not uniform** (0.5 dB in places, 4 dB in others), which
/// [`super::GainStage`] cannot express: the stage is therefore declared continuous over
/// 0–49.6 dB, and the driver snaps every requested gain to the **device's own** table (read at
/// open, so a dongle with a different tuner snaps to its table, not this one) and records what
/// the device reports back. A request for 30 dB is answered honestly with 29.7 dB in provenance.
pub const R820T_GAINS_DB: [f64; 29] = [
    0.0, 0.9, 1.4, 2.7, 3.7, 7.7, 8.7, 12.5, 14.4, 15.7, 16.6, 19.7, 20.7, 22.9, 25.4, 28.0, 29.7,
    32.8, 33.8, 36.4, 37.2, 38.6, 40.2, 42.1, 43.4, 43.9, 44.5, 48.0, 49.6,
];

/// Sample rates this driver offers, Hz.
///
/// Two constraints, both narrowing:
///
/// 1. **Exactly realisable.** librtlsdr programs the RTL2832U resampler with
///    `ratio = round_down_to_multiple_of_4(28.8 MHz · 2^22 / rate)` and the delivered rate is
///    `28.8 MHz · 2^22 / ratio`. Every rate here divides that product exactly, so the rate asked
///    for is the rate delivered (`rtlsdr_get_sample_rate` is read back and recorded regardless).
///    1.4 Msps, a rate other RTL tools offer, misses by 0.019 Hz — 13 parts per billion, which
///    `rtlsdr_get_sample_rate`'s integer return cannot even report, but which a timestamp
///    extrapolated by sample count accumulates into ~24 µs over half an hour. An error the driver
///    cannot see is exactly the kind not to take on, so that rate is not offered.
/// 2. **Streams without losses.** The RTL2832U accepts up to 3.2 Msps and drops samples above
///    ~2.4 Msps over USB. 2.4 Msps is the top entry.
///
/// The device's second rate window (225–300 ksps) is not offered: no round rate in it is exactly
/// realisable, and nothing in the system needs a rate that low from this front end.
pub const R820T_RATES_HZ: [f64; 8] = [
    1.024e6, 1.2e6, 1.536e6, 1.6e6, 1.8e6, 1.92e6, 2.048e6, 2.4e6,
];

/// Lowest centre the R820T tunes, Hz.
pub const R820T_MIN_HZ: f64 = 25e6;
/// Highest centre the R820T tunes, Hz.
pub const R820T_MAX_HZ: f64 = 1.75e9;

/// R820T centre-frequency granularity, Hz: `2 · 28.8 MHz / 2^16 / 2` = 439.453125 Hz.
///
/// Derived (as [`super::HACKRF_ONE_TUNING_STEP_HZ`] was) from the tuner driver rather than a
/// bench measurement. librtlsdr's `r82xx_set_pll` runs the R820T's fractional-N synthesiser
/// against `pll_ref = 28.8 MHz` with a **16-bit sigma-delta divider**, so the VCO grid is
/// `2 · 28.8 MHz / 2^16` = 878.906 Hz, and the RF grid is that divided by the mixer divider
/// `mix_div`, which is a power of two from 2 (high frequencies) to 64 (low ones).
///
/// The step therefore *varies with frequency*, which [`super::TuningStep::Uniform`] cannot say —
/// so this declares the **coarsest** of those grids, `mix_div = 2`. Because every finer grid is
/// this one refined by a power of two, every centre declared achievable here is achievable at
/// every frequency, and the tune lands within half a step (≤ 219.7 Hz). Erring coarse repeats a
/// reachable state; erring fine would invent 31 unreachable centres between each pair.
///
/// **Unverified by measurement.** Read from the librtlsdr source, not a counter on the bench.
pub const R820T_TUNING_STEP_HZ: f64 = 2.0 * 28.8e6 / 65536.0 / 2.0;

/// The tuner's reference clock, Hz, used by the rate derivation above.
const RTL_XTAL_HZ: f64 = 28.8e6;

/// Antenna port recorded in provenance: the antenna is not known to the software.
pub const ANTENNA_UNKNOWN: &str = "unknown";
/// No transfer for this long **after the stream has started** is an error.
const STALL_TIMEOUT: Duration = Duration::from_secs(3);

/// `rtlsdr_get_tuner_type` codes (rtl-sdr.h `enum rtlsdr_tuner`).
#[cfg_attr(not(any(test, feature = "rtlsdr")), allow(dead_code))]
pub(crate) fn tuner_name(code: i32) -> &'static str {
    match code {
        1 => "Elonics E4000",
        2 => "Fitipower FC0012",
        3 => "Fitipower FC0013",
        4 => "FCI FC2580",
        5 => "Rafael Micro R820T",
        6 => "Rafael Micro R828D",
        _ => "unknown tuner",
    }
}

/// The rate the RTL2832U resampler actually delivers for a requested rate, Hz, by librtlsdr's
/// own arithmetic (`rtlsdr_set_sample_rate`). Used to prove [`R820T_RATES_HZ`] are exact.
pub fn realised_sample_rate_hz(requested_hz: f64) -> f64 {
    if !(requested_hz.is_finite() && requested_hz > 0.0) {
        return f64::NAN;
    }
    let ratio = (RTL_XTAL_HZ * f64::from(1u32 << 22) / requested_hz) as u32 & !3u32;
    if ratio == 0 {
        return f64::NAN;
    }
    RTL_XTAL_HZ * f64::from(1u32 << 22) / f64::from(ratio)
}

/// The entry of `table` nearest `db` (ties to the lower gain); `None` for an empty table.
pub fn snap_gain_db(table: &[f64], db: f64) -> Option<f64> {
    if !db.is_finite() {
        return None;
    }
    table
        .iter()
        .copied()
        .filter(|g| g.is_finite())
        .min_by(|a, b| {
            (a - db)
                .abs()
                .total_cmp(&(b - db).abs())
                .then(a.total_cmp(b))
        })
}

/// Initial settings for [`RtlSdrSource::open_with`].
#[derive(Clone, Debug, PartialEq)]
pub struct RtlSdrConfig {
    /// Serial number to open, e.g. `"7673444264"`; `None` opens the first device.
    ///
    /// **Prefer a serial.** A USB index changes when anything else is plugged in, so an index
    /// can silently open a different radio and file its measurements under the wrong provenance.
    pub serial: Option<String>,
    /// Centre frequency, Hz.
    pub center_hz: f64,
    /// Sample rate, Hz (one of [`R820T_RATES_HZ`]).
    pub sample_rate_hz: f64,
    /// Tuner gain, dB; snapped to the device's own table.
    pub tuner_gain_db: f64,
    /// Transfer buffers between the USB callback and the capture thread.
    pub queue_buffers: usize,
    /// USB buffers librtlsdr keeps in flight (`rtlsdr_read_async`'s `buf_num`).
    pub usb_buffers: u32,
    /// Bytes per USB transfer (`buf_len`): a multiple of 16384 (65536 ≈ 13.6 ms at 2.4 Msps).
    pub buffer_bytes: u32,
    /// How long the **first** block may take before the stream reports a stall: the R820T PLL can
    /// need ~10 s to lock on a cold open, and that is not a failure.
    pub first_block_timeout: Duration,
    /// Clipped fraction of a block's components that marks overload (1e-3).
    pub overload_clip_fraction: f64,
}

impl Default for RtlSdrConfig {
    fn default() -> Self {
        Self {
            serial: None,
            center_hz: 100e6,
            sample_rate_hz: 2.048e6,
            tuner_gain_db: 28.0,
            queue_buffers: 64,
            usb_buffers: 15,
            buffer_bytes: 65_536,
            first_block_timeout: Duration::from_secs(20),
            overload_clip_fraction: 1e-3,
        }
    }
}

impl RtlSdrConfig {
    /// Maps a generic [`OpenRequest`] (named gain `lna`) onto RTL-SDR settings. An unknown stage,
    /// an out-of-range value, or a bias-tee request this hardware cannot honour is refused.
    pub fn from_request(request: &OpenRequest) -> Result<Self, SourceError> {
        let caps = SourceCapabilities::rtl_sdr_r820t();
        let mut config = Self {
            serial: request.device.clone(),
            center_hz: request.center_hz,
            sample_rate_hz: request.sample_rate_hz,
            ..Self::default()
        };
        if request.baseband_filter_hz.is_some() {
            return Err(SourceError::Unsupported {
                source_name: RtlSdrSource::NAME,
                operation: "set_baseband_filter",
            });
        }
        if request.bias_tee {
            return Err(SourceError::Unsupported {
                source_name: RtlSdrSource::NAME,
                operation: "set_bias_tee",
            });
        }
        for g in &request.gains {
            let db = caps
                .gain_stage(&g.stage)
                .and_then(|s| s.quantise(g.db))
                .ok_or(SourceError::OutOfRange {
                    what: "named gain (stage lna)",
                    value: g.db,
                })?;
            config.tuner_gain_db = db;
        }
        Ok(config)
    }
}

/// The RTL-SDR [`SourceDriver`]: generic open requests to [`RtlSdrSource`].
#[derive(Clone, Copy, Debug, Default)]
pub struct RtlSdrDriver;

impl SourceDriver for RtlSdrDriver {
    fn name(&self) -> &'static str {
        "rtlsdr"
    }

    fn available(&self) -> bool {
        RtlSdrSource::available()
    }

    fn capabilities(&self) -> SourceCapabilities {
        SourceCapabilities::rtl_sdr_r820t()
    }

    fn open(&self, request: &OpenRequest) -> Result<Box<dyn Source>, SourceError> {
        let config = RtlSdrConfig::from_request(request)?;
        Ok(Box::new(RtlSdrSource::open_with(&config)?))
    }
}

/// The opened dongle's identity.
#[derive(Clone, Debug, PartialEq)]
pub struct RtlSdrDeviceInfo {
    /// USB serial string, e.g. `7673444264`.
    pub serial: String,
    /// USB manufacturer string, e.g. `NooElec`.
    pub manufacturer: String,
    /// USB product string, e.g. `NESDR Nano 3`.
    pub product: String,
    /// librtlsdr's name for the USB id, e.g. `Generic RTL2832U OEM`.
    pub device_name: String,
    /// Tuner chip, e.g. `Rafael Micro R820T`.
    pub tuner: String,
    /// The gain steps this device reports, dB (`rtlsdr_get_tuner_gains`).
    pub gains_db: Vec<f64>,
}

impl RtlSdrDeviceInfo {
    /// Provenance `device_id`: `rtlsdr:<serial>`.
    pub fn device_id(&self) -> String {
        format!("rtlsdr:{}", self.serial)
    }

    /// SigMF `core:hw`: manufacturer, product, serial, tuner and antenna.
    pub fn hw_description(&self) -> String {
        let make = [self.manufacturer.as_str(), self.product.as_str()]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" ");
        let make = if make.is_empty() {
            self.device_name.clone()
        } else {
            make
        };
        format!(
            "{make} serial {}, {} tuner ({} gain steps), {}, antenna {ANTENNA_UNKNOWN}",
            self.serial,
            self.tuner,
            self.gains_db.len(),
            self.device_name,
        )
    }
}

/// Stream counters (the transfer counters are updated by the librtlsdr callback).
#[derive(Debug, Default)]
pub struct RtlSdrStats {
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
    /// Clipped components seen (codes 0 or 255).
    pub clipped_components: AtomicU64,
}

impl RtlSdrStats {
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

/// The device operations the stream needs: `ffi::LibRtlSdr` (feature `rtlsdr`), or a fake in
/// tests. Receive only by construction.
#[cfg_attr(not(any(test, feature = "rtlsdr")), allow(dead_code))]
pub(crate) trait RtlRxDevice: Send {
    fn set_freq(&mut self, hz: u32) -> Result<(), SourceError>;
    /// Sets the rate and returns the rate the resampler actually realises, Hz.
    fn set_sample_rate(&mut self, hz: u32) -> Result<u32, SourceError>;
    /// The gain steps this device offers, dB.
    fn gain_table_db(&self) -> Vec<f64>;
    /// Sets the tuner gain (tenths of a dB) and returns what the device reports back, dB.
    fn set_tuner_gain(&mut self, tenth_db: i32) -> Result<f64, SourceError>;
    fn start_rx(&mut self, pool: Arc<TransferPool>) -> Result<(), SourceError>;
    fn stop_rx(&mut self) -> Result<(), SourceError>;
    fn is_streaming(&self) -> bool;
}

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

/// Validates a gain against the one stage and snaps it to `table` (the device's own steps).
fn checked_gain(caps: &SourceCapabilities, table: &[f64], db: f64) -> Result<f64, SourceError> {
    let err = SourceError::OutOfRange {
        what: "named gain (stage lna)",
        value: db,
    };
    let Some(quantised) = caps.gain_stage("lna").and_then(|s| s.quantise(db)) else {
        return Err(err);
    };
    snap_gain_db(table, quantised).ok_or(err)
}

/// The RTL-SDR control handle: validates and posts; the stream applies at a block boundary.
pub struct RtlSdrControl {
    capabilities: SourceCapabilities,
    /// The gain steps the opened device reports (not necessarily [`R820T_GAINS_DB`]).
    gain_table_db: Vec<f64>,
    mailbox: ControlMailbox,
    stopped: AtomicBool,
    requested_gain_db: Mutex<f64>,
    stats: Arc<RtlSdrStats>,
    device: DeviceInfo,
}

impl RtlSdrControl {
    /// The gain steps the opened device reports, dB.
    pub fn gain_table_db(&self) -> &[f64] {
        &self.gain_table_db
    }
}

impl SourceControl for RtlSdrControl {
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

    /// The R820T has one gain knob. A non-zero `vga_db` or `amp_on` is **refused** rather than
    /// dropped: silently ignoring a requested gain would leave the caller believing in a gain
    /// structure the provenance does not record.
    fn set_gains(&self, gains: &Gains) -> Result<(), SourceError> {
        if gains.amp_on {
            return Err(SourceError::OutOfRange {
                what: "rf amplifier (this device has none)",
                value: 1.0,
            });
        }
        if gains.vga_db != 0.0 {
            return Err(SourceError::OutOfRange {
                what: "vga gain (this device has one combined gain stage, `lna`)",
                value: gains.vga_db,
            });
        }
        self.set_gain("lna", gains.lna_db)
    }

    fn set_gain(&self, stage: &str, db: f64) -> Result<(), SourceError> {
        if stage != "lna" {
            return Err(SourceError::OutOfRange {
                what: "named gain (stage lna)",
                value: db,
            });
        }
        let snapped = checked_gain(&self.capabilities, &self.gain_table_db, db)?;
        let mut requested = self
            .requested_gain_db
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *requested = snapped;
        self.mailbox.post(|p| {
            p.gains = Some(Gains {
                lna_db: snapped,
                vga_db: 0.0,
                amp_on: false,
            })
        });
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

    /// The R820T's baseband filter is not selectable through librtlsdr in a way this driver could
    /// report back, so it is not offered at all.
    fn set_baseband_filter(&self, _bandwidth_hz: f64) -> Result<(), SourceError> {
        Err(SourceError::Unsupported {
            source_name: RtlSdrSource::NAME,
            operation: "set_baseband_filter",
        })
    }

    /// No bias tee: see the [module docs](self). Refused rather than accepted-and-ignored.
    fn set_bias_tee(&self, _enabled: bool) -> Result<(), SourceError> {
        Err(SourceError::Unsupported {
            source_name: RtlSdrSource::NAME,
            operation: "set_bias_tee",
        })
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

/// The RTL-SDR stream. See the [module docs](self).
pub struct RtlSdrSource {
    control: Arc<RtlSdrControl>,
    stats: Arc<RtlSdrStats>,
    info: RtlSdrDeviceInfo,
    pool: Arc<TransferPool>,
    tune: Tune,
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
    first_block_timeout: Duration,
    delivered_any: bool,
    // Declared last: dropped (closing the device) after everything above.
    device: Box<dyn RtlRxDevice>,
}

#[cfg_attr(feature = "rtlsdr", allow(dead_code))]
fn not_available() -> SourceError {
    SourceError::NotAvailable {
        source_name: RtlSdrSource::NAME,
        reason: "this build has no RTL-SDR driver; rebuild with the `rtlsdr` cargo feature (e.g. \
                 `cargo run -p hk-cli --features rtlsdr`), which links the system librtlsdr"
            .into(),
    }
}

impl RtlSdrSource {
    /// Driver name used in errors and capabilities.
    pub const NAME: &'static str = "rtl-sdr";

    /// This build includes the librtlsdr driver (feature `rtlsdr`).
    pub const fn available() -> bool {
        cfg!(feature = "rtlsdr")
    }

    /// Opens an RTL-SDR by serial (or the first found) with default settings.
    pub fn open(serial: Option<&str>) -> Result<Self, SourceError> {
        Self::open_with(&RtlSdrConfig {
            serial: serial.map(str::to_owned),
            ..RtlSdrConfig::default()
        })
    }

    /// Opens an RTL-SDR and applies `config` (streaming starts with the first read).
    #[cfg(feature = "rtlsdr")]
    pub fn open_with(config: &RtlSdrConfig) -> Result<Self, SourceError> {
        let (device, info) = ffi::LibRtlSdr::open(
            config.serial.as_deref(),
            config.usb_buffers,
            config.buffer_bytes,
        )?;
        Self::from_device(Box::new(device), info, config)
    }

    /// Opens an RTL-SDR: not available without the `rtlsdr` feature.
    #[cfg(not(feature = "rtlsdr"))]
    pub fn open_with(config: &RtlSdrConfig) -> Result<Self, SourceError> {
        let _ = config;
        Err(not_available())
    }

    /// The capability descriptor this source reports.
    pub fn capabilities_descriptor() -> SourceCapabilities {
        SourceCapabilities::rtl_sdr_r820t()
    }

    #[cfg_attr(not(any(test, feature = "rtlsdr")), allow(dead_code))]
    pub(crate) fn from_device(
        mut device: Box<dyn RtlRxDevice>,
        info: RtlSdrDeviceInfo,
        config: &RtlSdrConfig,
    ) -> Result<Self, SourceError> {
        let caps = SourceCapabilities::rtl_sdr_r820t();
        let center = checked_frequency(&caps, config.center_hz)?;
        let rate = checked_rate(&caps, config.sample_rate_hz)?;
        let table = device.gain_table_db();
        let table = if table.is_empty() {
            R820T_GAINS_DB.to_vec()
        } else {
            table
        };
        let gain = checked_gain(&caps, &table, config.tuner_gain_db)?;
        // The rate the resampler actually realises, not the one that was asked for.
        let realised = f64::from(device.set_sample_rate(rate as u32)?);
        let applied_gain = device.set_tuner_gain((gain * 10.0).round() as i32)?;
        device.set_freq(center as u32)?;
        let tune = Tune {
            center_hz: center,
            sample_rate_hz: realised,
            lna_db: applied_gain,
            vga_db: 0.0,
            amp_on: false,
            // The delivered width. This front end has no separately selectable filter.
            bandwidth_hz: realised,
        };
        let stats = Arc::new(RtlSdrStats::default());
        let pool = Arc::new(TransferPool::new(
            config.queue_buffers.max(2),
            (config.buffer_bytes as usize).max(2),
            Arc::clone(&stats.transfers),
        ));
        let provenance = ProvenanceHandle::new(provenance_record(&info, &tune, false));
        Ok(Self {
            control: Arc::new(RtlSdrControl {
                capabilities: caps,
                gain_table_db: table,
                mailbox: ControlMailbox::new(),
                stopped: AtomicBool::new(false),
                requested_gain_db: Mutex::new(applied_gain),
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
            first_block_timeout: config.first_block_timeout,
            delivered_any: false,
            device,
        })
    }

    /// The device identity.
    pub fn device_info(&self) -> &RtlSdrDeviceInfo {
        &self.info
    }

    /// Live counters (keep a clone before moving the source into a pipeline).
    pub fn stats(&self) -> Arc<RtlSdrStats> {
        Arc::clone(&self.stats)
    }

    /// The provenance in force for the next block (before any pending change).
    pub fn provenance(&self) -> ProvenanceHandle {
        self.provenance.clone()
    }

    /// The bias-tee state this driver reports: always [`BiasTee::Unknown`], because it never
    /// drives the pin and cannot read it back (T-325).
    pub fn bias_tee(&self) -> BiasTee {
        BiasTee::Unknown
    }

    fn mint(&mut self) -> Discontinuity {
        let prev = self.provenance.clone();
        self.provenance =
            ProvenanceHandle::new(provenance_record(&self.info, &self.tune, self.overloaded));
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
        }
        result
    }

    fn apply_change(&mut self, change: &super::PendingControl) -> Result<(), SourceError> {
        if let Some(hz) = change.sample_rate_hz {
            let realised = f64::from(self.device.set_sample_rate(hz as u32)?);
            self.tune.sample_rate_hz = realised;
            self.tune.bandwidth_hz = realised;
        }
        if let Some(g) = change.gains {
            let applied = self
                .device
                .set_tuner_gain((g.lna_db * 10.0).round() as i32)?;
            self.tune.lna_db = applied;
        }
        if let Some(hz) = change.center_hz {
            self.device.set_freq(hz as u32)?;
            self.tune.center_hz = hz;
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
        // The R820T PLL can take ~10 s to lock on a cold open, so the first block gets a much
        // longer budget than the steady-state stall timeout.
        let budget = if self.delivered_any {
            STALL_TIMEOUT
        } else {
            self.first_block_timeout
        };
        if idle > budget {
            return Err(SourceError::Device {
                source_name: Self::NAME,
                operation: "receive",
                message: format!(
                    "no samples for {:.1} s (streaming: {}); the device may be unplugged, held by \
                     another process, or its tuner PLL never locked",
                    idle.as_secs_f64(),
                    self.device.is_streaming()
                ),
            });
        }
        Ok(())
    }

    /// The next block: `fill` receives its interleaved **cu8** bytes.
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
            // cu8 clips at the ends of the unsigned range.
            let clipped = bytes.iter().filter(|&&b| b == 0x00 || b == 0xff).count() as u64;
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
            self.delivered_any = true;
            self.stats
                .delivered_samples
                .fetch_add(n as u64, Ordering::Relaxed);
            self.stats.delivered_blocks.fetch_add(1, Ordering::Relaxed);
            return Ok(Some(header));
        }
    }
}

fn provenance_record(info: &RtlSdrDeviceInfo, tune: &Tune, overload: bool) -> Provenance {
    Provenance {
        device_id: info.device_id(),
        tune: tune.clone(),
        overload,
        quantisation_limited: false,
        noise_sigma_lsb: None,
        temperature_c: None,
        antenna_port: Some(ANTENNA_UNKNOWN.into()),
        // T-325: this driver never drives the bias-tee pin and librtlsdr offers no read-back, so
        // it cannot say what DC is on the port. `Unknown` is not `Off`.
        bias_tee: BiasTee::Unknown,
        clock_source: ClockSource::Internal,
        clock_locked: true,
        calibration_state_ref: None,
        spur_mask_ref: None,
        timestamp_method: TimestampMethod::HostArrival,
        timestamp_error_budget_ns: None,
        capture_artefacts: Vec::new(),
    }
}

impl Source for RtlSdrSource {
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
                Complex32::new(
                    f32::from(from_cu8(c[0])) / 128.0,
                    f32::from(from_cu8(c[1])) / 128.0,
                )
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
                    .map(|c| Complex::new(from_cu8(c[0]), from_cu8(c[1]))),
            )
        })
    }
}

/// One offset-binary `cu8` component re-centred to signed. Lossless: a constant −128 offset.
#[inline]
fn from_cu8(b: u8) -> i8 {
    (b ^ 0x80) as i8
}

impl Drop for RtlSdrSource {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::thread::JoinHandle;

    use hk_model::sigmf::Datatype;

    use super::super::conformance::{self, ConformanceSpec};
    use super::super::{Duplex, NamedGain, SampleRates, SourceKind, TuningStep};
    use super::*;

    const FAKE_TRANSFER_BYTES: u32 = 4096;

    /// Settings the fake dongle currently has, plus a log of calls.
    #[derive(Debug, Default)]
    struct FakeState {
        freq: u32,
        gain_db: f64,
        rate: u32,
        clip: bool,
        log: Vec<String>,
    }

    /// A fake RTL-SDR: a thread fills `cu8` transfers whose I code is `128 + gain index` and Q
    /// code `128 + (freq / 1 MHz mod 100)`, re-reading the settings halfway through each transfer
    /// (so a transfer in flight during a change mixes both, as on the real device). It paces
    /// itself at the configured sample rate, so block arrival times track the stream.
    struct FakeDevice {
        state: Arc<Mutex<FakeState>>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
        table: Vec<f64>,
    }

    impl FakeDevice {
        fn new() -> (Self, Arc<Mutex<FakeState>>) {
            let state = Arc::new(Mutex::new(FakeState::default()));
            (
                Self {
                    state: Arc::clone(&state),
                    stop: Arc::new(AtomicBool::new(false)),
                    thread: None,
                    table: R820T_GAINS_DB.to_vec(),
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
            return (0x00, 0xff);
        }
        let index = R820T_GAINS_DB
            .iter()
            .position(|g| (g - s.gain_db).abs() < 1e-9)
            .unwrap_or(0) as u8;
        (
            128u8.wrapping_add(index),
            128u8.wrapping_add((s.freq / 1_000_000 % 100) as u8),
        )
    }

    impl RtlRxDevice for FakeDevice {
        fn set_freq(&mut self, hz: u32) -> Result<(), SourceError> {
            self.state.lock().unwrap().freq = hz;
            self.log(format!("freq {hz}"));
            Ok(())
        }
        fn set_sample_rate(&mut self, hz: u32) -> Result<u32, SourceError> {
            self.state.lock().unwrap().rate = hz;
            self.log(format!("rate {hz}"));
            Ok(hz)
        }
        fn gain_table_db(&self) -> Vec<f64> {
            self.table.clone()
        }
        fn set_tuner_gain(&mut self, tenth_db: i32) -> Result<f64, SourceError> {
            // As the device does: settle on the nearest entry of its own table.
            let applied = snap_gain_db(&self.table, f64::from(tenth_db) / 10.0).ok_or(
                SourceError::OutOfRange {
                    what: "named gain (stage lna)",
                    value: f64::from(tenth_db) / 10.0,
                },
            )?;
            self.state.lock().unwrap().gain_db = applied;
            self.log(format!("gain {applied}"));
            Ok(applied)
        }
        fn start_rx(&mut self, pool: Arc<TransferPool>) -> Result<(), SourceError> {
            let (state, stop) = (Arc::clone(&self.state), Arc::clone(&self.stop));
            self.log("start".into());
            self.thread = Some(std::thread::spawn(move || {
                let len = FAKE_TRANSFER_BYTES as usize;
                let mut buf = vec![0u8; len];
                while !stop.load(Ordering::SeqCst) {
                    let period = {
                        let s = state.lock().unwrap();
                        Duration::from_secs_f64((len / 2) as f64 / f64::from(s.rate.max(1)))
                    };
                    let (i, q) = codes(&state.lock().unwrap());
                    for c in buf[..len / 2].chunks_exact_mut(2) {
                        c[0] = i;
                        c[1] = q;
                    }
                    std::thread::sleep(period / 2);
                    let (i, q) = codes(&state.lock().unwrap());
                    for c in buf[len / 2..].chunks_exact_mut(2) {
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
                let _ = t.join();
            }
            self.log("stop".into());
            Ok(())
        }
        fn is_streaming(&self) -> bool {
            self.thread.is_some()
        }
    }

    fn fake_info() -> RtlSdrDeviceInfo {
        RtlSdrDeviceInfo {
            serial: "7673444264".into(),
            manufacturer: "NooElec".into(),
            product: "NESDR Nano 3".into(),
            device_name: "Generic RTL2832U OEM".into(),
            tuner: tuner_name(5).into(),
            gains_db: R820T_GAINS_DB.to_vec(),
        }
    }

    fn fake_config() -> RtlSdrConfig {
        RtlSdrConfig {
            center_hz: 100e6,
            sample_rate_hz: 2.048e6,
            tuner_gain_db: 28.0,
            buffer_bytes: FAKE_TRANSFER_BYTES,
            queue_buffers: 32,
            ..RtlSdrConfig::default()
        }
    }

    fn open_fake(config: &RtlSdrConfig) -> (RtlSdrSource, Arc<Mutex<FakeState>>) {
        let (device, state) = FakeDevice::new();
        let src = RtlSdrSource::from_device(Box::new(device), fake_info(), config)
            .expect("open the fake device");
        (src, state)
    }

    /// A driver over the fake device, so the whole conformance suite runs with no hardware.
    struct FakeDriver;

    impl SourceDriver for FakeDriver {
        fn name(&self) -> &'static str {
            "rtlsdr-fake"
        }
        fn available(&self) -> bool {
            true
        }
        fn capabilities(&self) -> SourceCapabilities {
            SourceCapabilities::rtl_sdr_r820t()
        }
        fn open(&self, request: &OpenRequest) -> Result<Box<dyn Source>, SourceError> {
            let mut config = RtlSdrConfig::from_request(request)?;
            config.buffer_bytes = FAKE_TRANSFER_BYTES;
            config.queue_buffers = 32;
            let (device, _) = FakeDevice::new();
            Ok(Box::new(RtlSdrSource::from_device(
                Box::new(device),
                fake_info(),
                &config,
            )?))
        }
    }

    fn read(src: &mut RtlSdrSource, buf: &mut Vec<Complex<i8>>) -> BlockHeader {
        src.read_block_ci8(buf)
            .expect("read")
            .expect("the stream is open")
    }

    /// Reads until `pred` holds, within `max` blocks.
    fn wait_for(
        src: &mut RtlSdrSource,
        buf: &mut Vec<Complex<i8>>,
        max: usize,
        pred: impl Fn(&BlockHeader) -> bool,
    ) -> BlockHeader {
        for _ in 0..max {
            let h = read(src, buf);
            if pred(&h) {
                return h;
            }
        }
        panic!("not seen within {max} blocks");
    }

    #[test]
    fn capabilities_state_the_real_front_end() {
        let caps = SourceCapabilities::rtl_sdr_r820t();
        assert_eq!(caps.driver, RtlSdrSource::NAME);
        assert_eq!(caps.kind, SourceKind::Hardware);
        assert_eq!(caps.duplex, Duplex::ReceiveOnly);
        assert!(!caps.tx_capable, "the RTL2832U cannot transmit");
        assert_eq!(caps.native_format, Datatype::Cu8);
        assert_eq!(caps.adc_bits, 8);
        // 25 MHz - 1.75 GHz, and nothing outside it.
        assert!(caps.supports_frequency(R820T_MIN_HZ) && caps.supports_frequency(R820T_MAX_HZ));
        assert!(!caps.supports_frequency(1e6), "no HF without a converter");
        assert!(!caps.supports_frequency(2.4e9), "no 2.4 GHz on an R820T");
        // Rates: the declared list, and nothing above 2.4 Msps.
        assert_eq!(
            caps.sample_rates,
            SampleRates::Discrete(R820T_RATES_HZ.to_vec())
        );
        assert!(!caps.sample_rates.supports(3.2e6), "3.2 Msps drops samples");
        assert!(!caps.sample_rates.supports(20e6), "that is a HackRF rate");
        assert_eq!(caps.max_live_span_hz(), Some(2.4e6));
        // Everything this front end does not have is declared absent, not faked.
        assert!(!caps.rf_amp && !caps.bias_tee && !caps.external_clock);
        assert!(caps.baseband_filter.is_none());
        assert!(!caps.hardware_timestamps);
        assert!(caps.rf_path_boundaries_hz.is_empty());
        // One combined gain stage.
        assert_eq!(caps.gain_stages.len(), 1);
        let lna = caps.gain_stage("lna").expect("one stage named lna");
        assert_eq!((lna.min_db, lna.max_db), (0.0, 49.6));
        assert!(caps.gain_stage("vga").is_none() && caps.gain_stage("amp").is_none());
    }

    #[test]
    fn every_offered_sample_rate_is_exactly_realisable() {
        for rate in R820T_RATES_HZ {
            let realised = realised_sample_rate_hz(rate);
            assert!(
                (realised - rate).abs() < 1e-6,
                "{rate} Hz is realised as {realised} Hz"
            );
        }
        // 1.4 Msps is offered by other RTL tools and is NOT exact, which is why it is not here.
        // The miss is 0.019 Hz: too small for `rtlsdr_get_sample_rate` (an integer) to report,
        // and so an error the driver could never detect at runtime.
        let miss = (realised_sample_rate_hz(1.4e6) - 1.4e6).abs();
        assert!((1e-3..1.0).contains(&miss), "1.4 Msps misses by {miss} Hz");
        assert!(!R820T_RATES_HZ.contains(&1.4e6));
    }

    #[test]
    fn the_tuning_step_is_the_coarsest_r820t_grid() {
        assert!((R820T_TUNING_STEP_HZ - 439.453125).abs() < 1e-9);
        let caps = SourceCapabilities::rtl_sdr_r820t();
        assert_eq!(
            caps.tuning_step,
            TuningStep::Uniform {
                step_hz: R820T_TUNING_STEP_HZ
            }
        );
        // Every declared centre is on the grid and inside the range, and the residual is bounded
        // by half a step.
        let snapped = caps.snap_center_hz(100.000_1e6).expect("a known grid");
        assert!((snapped / R820T_TUNING_STEP_HZ).fract().abs() < 1e-6);
        assert!((snapped - 100.000_1e6).abs() <= R820T_TUNING_STEP_HZ / 2.0 + 1e-6);
        assert!(
            caps.snap_center_hz(R820T_MAX_HZ)
                .is_some_and(|f| f <= R820T_MAX_HZ)
        );
        assert!(
            caps.snap_center_hz(R820T_MIN_HZ)
                .is_some_and(|f| f >= R820T_MIN_HZ)
        );
    }

    #[test]
    fn gain_requests_snap_to_the_device_table() {
        let table = R820T_GAINS_DB.to_vec();
        assert_eq!(table.len(), 29, "the R820T has 29 gain steps");
        assert_eq!(snap_gain_db(&table, 30.0), Some(29.7));
        assert_eq!(snap_gain_db(&table, 0.0), Some(0.0));
        assert_eq!(snap_gain_db(&table, 49.6), Some(49.6));
        assert_eq!(snap_gain_db(&table, 100.0), Some(49.6));
        assert_eq!(snap_gain_db(&table, f64::NAN), None);
    }

    #[test]
    fn the_first_block_carries_the_requested_settings_and_the_device_reported_gain() {
        let mut config = fake_config();
        // Off-table: the device settles on 29.7 dB and provenance must say so, not 30.
        config.tuner_gain_db = 30.0;
        let (mut src, _state) = open_fake(&config);
        let mut buf = Vec::new();
        let h = read(&mut src, &mut buf);
        assert!(h.discontinuity.contains(Discontinuity::STREAM_START));
        let p = h.provenance.get();
        assert_eq!(p.device_id, "rtlsdr:7673444264");
        assert_eq!(p.tune.center_hz, 100e6);
        assert_eq!(p.tune.sample_rate_hz, 2.048e6);
        assert_eq!(p.tune.lna_db, 29.7, "the gain the device reported back");
        assert_eq!((p.tune.vga_db, p.tune.amp_on), (0.0, false));
        assert_eq!(p.tune.bandwidth_hz, 2.048e6);
        assert_eq!(p.bias_tee, BiasTee::Unknown, "never a fabricated `off`");
        assert_eq!(p.timestamp_method, TimestampMethod::HostArrival);
        assert_eq!(p.antenna_port.as_deref(), Some(ANTENNA_UNKNOWN));
        assert!(!src.pausable(), "a live radio is never pausable");
        assert_eq!(src.bias_tee(), BiasTee::Unknown);
    }

    #[test]
    fn cu8_samples_are_recentred_to_signed_on_both_read_paths() {
        let (mut src, state) = open_fake(&fake_config());
        let mut ci8 = Vec::new();
        read(&mut src, &mut ci8);
        // The fake codes I as 128 + gain index and Q as 128 + freq/1 MHz mod 100.
        let gain_index = R820T_GAINS_DB.iter().position(|g| *g == 28.0).unwrap() as i8;
        assert_eq!(ci8[0], Complex::new(gain_index, 0), "100 MHz mod 100 = 0");
        let mut f32s = Vec::new();
        src.read_block(&mut f32s).unwrap().unwrap();
        assert!((f32s[0].re - f32::from(gain_index) / 128.0).abs() < 1e-9);
        assert!(f32s[0].im.abs() < 1e-9);
        drop(state);
    }

    #[test]
    fn a_retune_reaches_a_block_flagged_retune_with_the_new_centre() {
        let (mut src, _state) = open_fake(&fake_config());
        let control = src.control();
        let mut buf = Vec::new();
        read(&mut src, &mut buf);
        control.tune(103e6).expect("an in-range retune");
        let h = wait_for(&mut src, &mut buf, 64, |h| {
            h.discontinuity.contains(Discontinuity::RETUNE)
        });
        assert_eq!(h.provenance.tune.center_hz, 103e6);
        assert!(
            h.discontinuity.contains(Discontinuity::GAP) && h.dropped_before > 0,
            "the settling transfers are discarded and reported as a gap"
        );
        assert_eq!(buf[0].im, 3, "the samples come from the new centre");
        let stats = control.stats().expect("counters");
        assert!(stats.discarded_samples > 0 && stats.discarded_samples <= h.dropped_before);
    }

    #[test]
    fn a_rate_change_reaches_a_block_flagged_rate_change() {
        let (mut src, _state) = open_fake(&fake_config());
        let control = src.control();
        let mut buf = Vec::new();
        read(&mut src, &mut buf);
        control.set_sample_rate(2.4e6).expect("an offered rate");
        let h = wait_for(&mut src, &mut buf, 64, |h| {
            h.discontinuity.contains(Discontinuity::RATE_CHANGE)
        });
        assert_eq!(h.provenance.tune.sample_rate_hz, 2.4e6);
        assert_eq!(
            h.provenance.tune.bandwidth_hz, 2.4e6,
            "the delivered width follows the rate"
        );
    }

    #[test]
    fn a_gain_change_reads_back_as_the_table_entry_the_device_took() {
        let (mut src, _state) = open_fake(&fake_config());
        let control = src.control();
        let mut buf = Vec::new();
        read(&mut src, &mut buf);
        control.set_gain("lna", 41.0).expect("an in-range gain");
        let h = wait_for(&mut src, &mut buf, 64, |h| {
            h.discontinuity.contains(Discontinuity::GAIN_CHANGE)
        });
        assert_eq!(
            h.provenance.tune.lna_db, 40.2,
            "41 dB is not a step; 40.2 is the nearest one"
        );
    }

    #[test]
    fn out_of_range_and_absent_controls_are_refused() {
        let (src, _state) = open_fake(&fake_config());
        let control = src.control();
        for hz in [1e6, 24e6, 1.8e9, 6e9, f64::NAN, f64::INFINITY] {
            assert!(
                matches!(control.tune(hz), Err(SourceError::OutOfRange { .. })),
                "tune({hz}) must be refused"
            );
        }
        for hz in [-1.0, 3.2e6, 20e6, f64::NAN] {
            assert!(
                matches!(
                    control.set_sample_rate(hz),
                    Err(SourceError::OutOfRange { .. })
                ),
                "set_sample_rate({hz}) must be refused"
            );
        }
        assert!(matches!(
            control.set_gain("vga", 20.0),
            Err(SourceError::OutOfRange { .. })
        ));
        assert!(matches!(
            control.set_gain("lna", 60.0),
            Err(SourceError::OutOfRange { .. })
        ));
        assert!(control.set_gain("lna", f64::NAN).is_err());
        // Absent hardware is Unsupported, never accepted-and-ignored.
        assert!(matches!(
            control.set_bias_tee(true),
            Err(SourceError::Unsupported { .. })
        ));
        assert!(matches!(
            control.set_baseband_filter(1e6),
            Err(SourceError::Unsupported { .. })
        ));
        assert!(matches!(
            control.start_sweep(&super::super::SweepPlan {
                lo_hz: 100e6,
                hi_hz: 101e6,
                step_hz: 1e6,
                sample_rate_hz: 2.048e6,
                samples_per_hop: 1024,
            }),
            Err(SourceError::Unsupported { .. })
        ));
        assert!(control.sweep_capability().is_none());
    }

    #[test]
    fn set_gains_refuses_stages_this_device_does_not_have() {
        let (src, _state) = open_fake(&fake_config());
        let control = src.control();
        assert!(
            control
                .set_gains(&Gains {
                    lna_db: 28.0,
                    vga_db: 0.0,
                    amp_on: false,
                })
                .is_ok()
        );
        for bad in [
            Gains {
                lna_db: 28.0,
                vga_db: 20.0,
                amp_on: false,
            },
            Gains {
                lna_db: 28.0,
                vga_db: 0.0,
                amp_on: true,
            },
        ] {
            assert!(
                matches!(control.set_gains(&bad), Err(SourceError::OutOfRange { .. })),
                "{bad:?} names a stage this device lacks"
            );
        }
    }

    #[test]
    fn an_open_request_for_absent_hardware_is_refused() {
        let base = OpenRequest {
            device: Some("7673444264".into()),
            center_hz: 100e6,
            sample_rate_hz: 2.048e6,
            gains: vec![NamedGain::new("lna", 28.0)],
            baseband_filter_hz: None,
            bias_tee: false,
        };
        assert!(RtlSdrConfig::from_request(&base).is_ok());
        assert!(matches!(
            RtlSdrConfig::from_request(&OpenRequest {
                bias_tee: true,
                ..base.clone()
            }),
            Err(SourceError::Unsupported { .. })
        ));
        assert!(matches!(
            RtlSdrConfig::from_request(&OpenRequest {
                baseband_filter_hz: Some(1e6),
                ..base.clone()
            }),
            Err(SourceError::Unsupported { .. })
        ));
        assert!(matches!(
            RtlSdrConfig::from_request(&OpenRequest {
                gains: vec![NamedGain::new("amp", 11.0)],
                ..base.clone()
            }),
            Err(SourceError::OutOfRange { .. })
        ));
        // Off-table gains are accepted at open and snapped by the device, not refused.
        let config = RtlSdrConfig::from_request(&OpenRequest {
            gains: vec![NamedGain::new("lna", 30.0)],
            ..base
        })
        .expect("30 dB is in range");
        assert_eq!(config.tuner_gain_db, 30.0);
    }

    #[test]
    fn stop_ends_the_stream_and_the_device_is_closed() {
        let (mut src, state) = open_fake(&fake_config());
        let control = src.control();
        let mut buf = Vec::new();
        read(&mut src, &mut buf);
        control.stop().unwrap();
        assert!(src.read_block_ci8(&mut buf).unwrap().is_none(), "stopped");
        drop(src);
        let log = state.lock().unwrap().log.clone();
        assert!(log.contains(&"stop".to_string()), "{log:?}");
        assert!(
            log.iter().any(|l| l.starts_with("rate "))
                && log.iter().any(|l| l.starts_with("gain ")),
            "the open applied rate and gain before streaming: {log:?}"
        );
    }

    #[test]
    fn overload_is_marked_sticky_in_provenance() {
        let (mut src, state) = open_fake(&fake_config());
        let mut buf = Vec::new();
        read(&mut src, &mut buf);
        assert!(!src.provenance().get().overload);
        state.lock().unwrap().clip = true;
        let h = wait_for(&mut src, &mut buf, 64, |h| h.provenance.overload);
        assert!(h.discontinuity.contains(Discontinuity::PROVENANCE_CHANGE));
        state.lock().unwrap().clip = false;
        // Sticky: it stays until the next tune or gain change clears it.
        for _ in 0..4 {
            assert!(read(&mut src, &mut buf).provenance.overload);
        }
        src.control().tune(101e6).unwrap();
        let cleared = wait_for(&mut src, &mut buf, 64, |h| {
            h.discontinuity.contains(Discontinuity::RETUNE)
        });
        assert!(!cleared.provenance.overload);
    }

    #[test]
    fn the_device_description_names_the_radio_and_its_tuner() {
        let info = fake_info();
        assert_eq!(info.device_id(), "rtlsdr:7673444264");
        let hw = info.hw_description();
        for part in [
            "NooElec",
            "NESDR Nano 3",
            "7673444264",
            "Rafael Micro R820T",
            "29 gain steps",
        ] {
            assert!(hw.contains(part), "{hw:?} is missing {part:?}");
        }
        assert_eq!(tuner_name(5), "Rafael Micro R820T");
        assert_eq!(tuner_name(99), "unknown tuner");
    }

    /// The whole device conformance suite, against the fake dongle, with no hardware.
    #[test]
    fn the_fake_dongle_passes_the_device_conformance_suite() {
        let request = OpenRequest {
            device: Some("7673444264".into()),
            center_hz: 100e6,
            sample_rate_hz: 2.048e6,
            // A table value, so the suite's `request-applied` check compares like with like.
            gains: vec![NamedGain::new("lna", 28.0)],
            baseband_filter_hz: None,
            bias_tee: false,
        };
        let mut spec = ConformanceSpec::new(request, 103e6);
        // Faster than 2.048 Msps, so the time anchor after the change lands later, never earlier.
        spec.alt_rate_hz = Some(2.4e6);
        spec.expect_pausable = Some(false);
        spec.warmup_blocks = 4;
        spec.deadline = Duration::from_secs(60);
        let report = conformance::run(&FakeDriver, &spec);
        report.assert_passed();
    }

    /// The bindings declare nothing that could transmit, write the dongle's EEPROM, or silently
    /// change what the samples mean (test mode, direct sampling).
    #[test]
    fn ffi_declares_no_write_or_test_mode_function() {
        let src = include_str!("rtlsdr/ffi.rs");
        for forbidden in [
            "rtlsdr_write_eeprom",
            "rtlsdr_set_testmode",
            "rtlsdr_set_direct_sampling",
            "rtlsdr_set_bias_tee",
        ] {
            assert!(
                !src.contains(&format!("fn {forbidden}")),
                "ffi.rs declares {forbidden}"
            );
        }
    }
}
