//! librtlsdr bindings, **receive only** (feature `rtlsdr`).
//!
//! Hand-written for the functions the RX source needs, checked against the installed
//! `rtl-sdr.h` (librtlsdr 2.0.3, Homebrew). The RTL2832U cannot transmit, so there is no TX API
//! to bind; what this module *deliberately* leaves undeclared is `rtlsdr_write_eeprom`, which can
//! brick a dongle, and the test-mode/direct-sampling switches that would silently stop the stream
//! being what the provenance says it is. A unit test
//! (`source::rtlsdr::tests::ffi_declares_no_write_or_test_mode_function`) keeps it that way.
//!
//! **Licence (ADR-0010 ledger).** librtlsdr is GPL-2.0-or-later — a stricter licence than
//! BSD-3-Clause libhackrf — and links libusb (LGPL-2.1) dynamically. It is behind an
//! off-by-default cargo feature, so the default build links none of it, and the system library is
//! found by `build.rs` (pkg-config `librtlsdr`, or `RTLSDR_LIB_DIR`) and never vendored. Recorded
//! here as a fact for the ledger, not as a legal conclusion.
//!
//! # Threading
//!
//! `rtlsdr_read_async` **blocks** until `rtlsdr_cancel_async`, so it runs on a worker thread this
//! module owns, while the capture thread keeps calling `rtlsdr_set_center_freq` and friends on
//! the same handle. That is the documented librtlsdr usage (`rtl_fm` retunes from another thread
//! during an async read): the control calls are synchronous libusb control transfers on a
//! different endpoint from the bulk stream.

#![allow(non_camel_case_types)]

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::super::hackrf::pool::TransferPool;
use super::{RtlRxDevice, RtlSdrDeviceInfo, RtlSdrSource};
use crate::source::SourceError;

/// Opaque device handle.
#[repr(C)]
pub struct rtlsdr_dev_t {
    _private: [u8; 0],
}

/// `rtlsdr_read_async_cb_t` (rtl-sdr.h): `void (*)(unsigned char *buf, uint32_t len, void *ctx)`.
type ReadAsyncCb = unsafe extern "C" fn(buf: *mut u8, len: u32, ctx: *mut c_void);

const RTLSDR_SUCCESS: c_int = 0;
/// USB string buffers (`rtl-sdr.h` uses 256 bytes each).
const USB_STRING_LEN: usize = 256;

unsafe extern "C" {
    fn rtlsdr_get_device_count() -> u32;
    fn rtlsdr_get_device_name(index: u32) -> *const c_char;
    fn rtlsdr_get_index_by_serial(serial: *const c_char) -> c_int;
    fn rtlsdr_open(dev: *mut *mut rtlsdr_dev_t, index: u32) -> c_int;
    fn rtlsdr_close(dev: *mut rtlsdr_dev_t) -> c_int;
    fn rtlsdr_get_usb_strings(
        dev: *mut rtlsdr_dev_t,
        manufact: *mut c_char,
        product: *mut c_char,
        serial: *mut c_char,
    ) -> c_int;
    fn rtlsdr_set_center_freq(dev: *mut rtlsdr_dev_t, freq: u32) -> c_int;
    fn rtlsdr_set_sample_rate(dev: *mut rtlsdr_dev_t, rate: u32) -> c_int;
    fn rtlsdr_get_sample_rate(dev: *mut rtlsdr_dev_t) -> u32;
    fn rtlsdr_get_tuner_type(dev: *mut rtlsdr_dev_t) -> c_int;
    fn rtlsdr_get_tuner_gains(dev: *mut rtlsdr_dev_t, gains: *mut c_int) -> c_int;
    fn rtlsdr_set_tuner_gain(dev: *mut rtlsdr_dev_t, gain: c_int) -> c_int;
    fn rtlsdr_get_tuner_gain(dev: *mut rtlsdr_dev_t) -> c_int;
    fn rtlsdr_set_tuner_gain_mode(dev: *mut rtlsdr_dev_t, manual: c_int) -> c_int;
    fn rtlsdr_set_agc_mode(dev: *mut rtlsdr_dev_t, on: c_int) -> c_int;
    fn rtlsdr_reset_buffer(dev: *mut rtlsdr_dev_t) -> c_int;
    fn rtlsdr_read_async(
        dev: *mut rtlsdr_dev_t,
        cb: ReadAsyncCb,
        ctx: *mut c_void,
        buf_num: u32,
        buf_len: u32,
    ) -> c_int;
    fn rtlsdr_cancel_async(dev: *mut rtlsdr_dev_t) -> c_int;
}

fn c_string(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: librtlsdr returns static NUL-terminated strings (or NULL, handled above).
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

fn check(operation: &'static str, rc: c_int) -> Result<(), SourceError> {
    if rc == RTLSDR_SUCCESS {
        return Ok(());
    }
    Err(SourceError::Device {
        source_name: RtlSdrSource::NAME,
        operation,
        message: format!("librtlsdr returned {rc}"),
    })
}

/// The async callback: copies the transfer into the pool and returns immediately.
unsafe extern "C" fn read_callback(buf: *mut u8, len: u32, ctx: *mut c_void) {
    if buf.is_null() || ctx.is_null() || len == 0 {
        return;
    }
    // SAFETY: `ctx` is the `TransferPool` given to `rtlsdr_read_async`; `LibRtlSdr` keeps that
    // `Arc` alive until after `rtlsdr_cancel_async` returned and the worker thread joined.
    let pool = unsafe { &*(ctx as *const TransferPool) };
    // SAFETY: librtlsdr guarantees `len` readable bytes for the duration of the call.
    let data = unsafe { std::slice::from_raw_parts(buf, len as usize) };
    pool.on_transfer(data, hk_model::Timestamp::now().as_unix_nanos());
}

/// The device handle and the callback context, moved to the async worker thread.
///
/// SAFETY: librtlsdr drives one handle from several threads by design (`rtl_fm` retunes during an
/// async read). Only `rtlsdr_read_async` touches it from the worker; every other call stays on
/// the capture thread. The context is a `TransferPool` the caller keeps alive (in `self.pool`)
/// until after the worker has joined, and `TransferPool` is itself `Send + Sync`.
struct AsyncArgs {
    dev: *mut rtlsdr_dev_t,
    ctx: *mut c_void,
}
// SAFETY: see `AsyncArgs`.
unsafe impl Send for AsyncArgs {}

/// An open RTL-SDR dongle, receive only.
pub(crate) struct LibRtlSdr {
    dev: *mut rtlsdr_dev_t,
    worker: Option<JoinHandle<()>>,
    pool: Option<Arc<TransferPool>>,
    buffer_bytes: u32,
    buffers: u32,
    /// Gains the tuner actually offers, tenths of a dB (`rtlsdr_get_tuner_gains`).
    gains_tenth_db: Vec<i32>,
}

// SAFETY: the handle is only used through `&mut self` (one thread at a time) plus the worker
// thread, which only calls the two async entry points; librtlsdr supports that split.
unsafe impl Send for LibRtlSdr {}

/// librtlsdr has no global init/exit, but `rtlsdr_open` by index races with another process
/// enumerating: serialise our own opens.
static OPEN_LOCK: Mutex<()> = Mutex::new(());

impl LibRtlSdr {
    /// Opens `serial` (or the first device) and reads its identity.
    ///
    /// **By serial, never by index** where a serial is given: the index is a USB enumeration
    /// order that changes when anything else is plugged in, so an index would silently open the
    /// wrong radio and file its measurements under another device's provenance.
    pub fn open(
        serial: Option<&str>,
        buffers: u32,
        buffer_bytes: u32,
    ) -> Result<(Self, RtlSdrDeviceInfo), SourceError> {
        let _guard = OPEN_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        // SAFETY: no arguments.
        let count = unsafe { rtlsdr_get_device_count() };
        if count == 0 {
            return Err(SourceError::NotAvailable {
                source_name: RtlSdrSource::NAME,
                reason: "no RTL-SDR device is connected (check `rtl_test -t`)".into(),
            });
        }
        let index = match serial {
            None => 0,
            Some(s) => {
                let wanted = CString::new(s).map_err(|_| SourceError::OutOfRange {
                    what: "serial number (contains NUL)",
                    value: 0.0,
                })?;
                // SAFETY: a NUL-terminated string alive for the call.
                let idx = unsafe { rtlsdr_get_index_by_serial(wanted.as_ptr()) };
                if idx < 0 {
                    return Err(SourceError::NotAvailable {
                        source_name: RtlSdrSource::NAME,
                        reason: format!(
                            "no RTL-SDR with serial {s:?} among the {count} connected (librtlsdr \
                             returned {idx}); `rtl_test -t` lists the serials"
                        ),
                    });
                }
                idx as u32
            }
        };
        let mut dev: *mut rtlsdr_dev_t = std::ptr::null_mut();
        // SAFETY: `dev` is written by the call.
        check("rtlsdr_open", unsafe { rtlsdr_open(&mut dev, index) })?;
        let mut device = Self {
            dev,
            worker: None,
            pool: None,
            buffer_bytes,
            buffers,
            gains_tenth_db: Vec::new(),
        };
        device.gains_tenth_db = device.read_gains()?;
        // Manual gain, and the RTL2832U's own digital AGC off: both are gain state the provenance
        // has to be able to state, and neither may move on its own under a measurement.
        check("rtlsdr_set_tuner_gain_mode", unsafe {
            rtlsdr_set_tuner_gain_mode(device.dev, 1)
        })?;
        check("rtlsdr_set_agc_mode", unsafe {
            rtlsdr_set_agc_mode(device.dev, 0)
        })?;
        let info = device.read_info(index)?;
        Ok((device, info))
    }

    fn read_gains(&self) -> Result<Vec<i32>, SourceError> {
        // SAFETY: a NULL buffer asks for the count only (rtl-sdr.h).
        let n = unsafe { rtlsdr_get_tuner_gains(self.dev, std::ptr::null_mut()) };
        if n <= 0 {
            return Ok(Vec::new());
        }
        let mut gains = vec![0 as c_int; n as usize];
        // SAFETY: the buffer holds the `n` entries the call just reported.
        let got = unsafe { rtlsdr_get_tuner_gains(self.dev, gains.as_mut_ptr()) };
        gains.truncate(got.max(0) as usize);
        Ok(gains)
    }

    fn read_info(&self, index: u32) -> Result<RtlSdrDeviceInfo, SourceError> {
        let mut manufact = [0 as c_char; USB_STRING_LEN];
        let mut product = [0 as c_char; USB_STRING_LEN];
        let mut serial = [0 as c_char; USB_STRING_LEN];
        // SAFETY: three 256-byte buffers, the size rtl-sdr.h documents.
        check("rtlsdr_get_usb_strings", unsafe {
            rtlsdr_get_usb_strings(
                self.dev,
                manufact.as_mut_ptr(),
                product.as_mut_ptr(),
                serial.as_mut_ptr(),
            )
        })?;
        // SAFETY: accepts any index and returns a static string (or NULL).
        let device_name = c_string(unsafe { rtlsdr_get_device_name(index) });
        // SAFETY: valid open device.
        let tuner = unsafe { rtlsdr_get_tuner_type(self.dev) };
        Ok(RtlSdrDeviceInfo {
            // SAFETY: NUL-terminated within the zero-initialised buffers.
            serial: c_string(serial.as_ptr()),
            manufacturer: c_string(manufact.as_ptr()),
            product: c_string(product.as_ptr()),
            device_name,
            tuner: super::tuner_name(tuner).into(),
            gains_db: self
                .gains_tenth_db
                .iter()
                .map(|g| f64::from(*g) / 10.0)
                .collect(),
        })
    }
}

impl RtlRxDevice for LibRtlSdr {
    fn set_freq(&mut self, hz: u32) -> Result<(), SourceError> {
        // SAFETY: valid open device.
        check("rtlsdr_set_center_freq", unsafe {
            rtlsdr_set_center_freq(self.dev, hz)
        })
    }

    fn set_sample_rate(&mut self, hz: u32) -> Result<u32, SourceError> {
        // SAFETY: valid open device.
        check("rtlsdr_set_sample_rate", unsafe {
            rtlsdr_set_sample_rate(self.dev, hz)
        })?;
        // SAFETY: valid open device; returns the rate the divider actually realises.
        Ok(unsafe { rtlsdr_get_sample_rate(self.dev) })
    }

    fn gain_table_db(&self) -> Vec<f64> {
        self.gains_tenth_db
            .iter()
            .map(|g| f64::from(*g) / 10.0)
            .collect()
    }

    fn set_tuner_gain(&mut self, tenth_db: i32) -> Result<f64, SourceError> {
        // SAFETY: valid open device.
        check("rtlsdr_set_tuner_gain", unsafe {
            rtlsdr_set_tuner_gain(self.dev, tenth_db)
        })?;
        // SAFETY: valid open device; reports the table entry the tuner settled on.
        Ok(f64::from(unsafe { rtlsdr_get_tuner_gain(self.dev) }) / 10.0)
    }

    fn start_rx(&mut self, pool: Arc<TransferPool>) -> Result<(), SourceError> {
        if self.worker.is_some() {
            return Ok(());
        }
        // SAFETY: valid open device; drops whatever the USB FIFO accumulated while idle.
        check("rtlsdr_reset_buffer", unsafe {
            rtlsdr_reset_buffer(self.dev)
        })?;
        let ctx = Arc::as_ptr(&pool) as *mut c_void;
        self.pool = Some(pool);
        let args = AsyncArgs { dev: self.dev, ctx };
        let (buffers, buffer_bytes) = (self.buffers, self.buffer_bytes);
        self.worker = Some(std::thread::spawn(move || {
            let args = args;
            // SAFETY: the pool behind `ctx` outlives this thread (`stop_rx` joins before the
            // `Arc` is released, and `Drop` calls `stop_rx`). The call blocks until cancelled.
            unsafe {
                rtlsdr_read_async(args.dev, read_callback, args.ctx, buffers, buffer_bytes);
            }
        }));
        Ok(())
    }

    fn stop_rx(&mut self) -> Result<(), SourceError> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        // `rtlsdr_cancel_async` is a no-op if the async read has not reached its running state
        // yet, which is a real race when a stream is stopped straight after starting: retry until
        // the worker actually leaves `rtlsdr_read_async`.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !worker.is_finished() {
            // SAFETY: valid open device.
            unsafe { rtlsdr_cancel_async(self.dev) };
            if Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let joined = worker.join();
        self.pool = None;
        if joined.is_err() {
            return Err(SourceError::Device {
                source_name: RtlSdrSource::NAME,
                operation: "rtlsdr_cancel_async",
                message: "the async read thread panicked".into(),
            });
        }
        Ok(())
    }

    fn is_streaming(&self) -> bool {
        self.worker.as_ref().is_some_and(|w| !w.is_finished())
    }
}

impl Drop for LibRtlSdr {
    fn drop(&mut self) {
        let _ = self.stop_rx();
        // SAFETY: valid open device; closed exactly once, after the worker joined.
        unsafe { rtlsdr_close(self.dev) };
    }
}
