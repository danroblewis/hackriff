//! libhackrf bindings, **receive only** (feature `hackrf`).
//!
//! Hand-written for the functions the RX source needs, checked against the installed
//! `libhackrf/hackrf.h` (libhackrf 0.9.2, release 2026.01.3). No transmit, sweep-TX or firmware
//! write function is declared, so no TX path is reachable from Rust; a unit test
//! (`source::hackrf::tests::ffi_declares_no_transmit_function`) keeps it that way.
//!
//! libhackrf (`hackrf.c`, `hackrf.h`) is BSD-3-Clause and links libusb (LGPL-2.1) dynamically
//! (ADR-0010 ledger). The system library is linked by `build.rs` (pkg-config `libhackrf`, or
//! `HACKRF_LIB_DIR`); it is never vendored.

#![allow(non_camel_case_types)]

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::{Arc, Mutex, PoisonError};

use super::pool::TransferPool;
use super::{HackRfDeviceInfo, RxDevice};
use crate::source::SourceError;

/// Opaque device handle.
#[repr(C)]
pub struct hackrf_device {
    _private: [u8; 0],
}

/// `hackrf_transfer` (hackrf.h). The last field is the library's TX context, unused here.
#[repr(C)]
pub struct hackrf_transfer {
    device: *mut hackrf_device,
    buffer: *mut u8,
    buffer_length: c_int,
    valid_length: c_int,
    rx_ctx: *mut c_void,
    reserved_ctx: *mut c_void,
}

/// `read_partid_serialno_t` (hackrf.h).
#[repr(C)]
#[derive(Default)]
struct read_partid_serialno_t {
    part_id: [u32; 2],
    serial_no: [u32; 4],
}

/// `hackrf_device_list_t` (hackrf.h). Read-only; freed by `hackrf_device_list_free`.
#[repr(C)]
struct hackrf_device_list_t {
    serial_numbers: *mut *mut c_char,
    usb_board_ids: *mut c_int,
    usb_device_index: *mut c_int,
    devicecount: c_int,
    usb_devices: *mut *mut c_void,
    usb_devicecount: c_int,
}

type hackrf_sample_block_cb_fn = unsafe extern "C" fn(transfer: *mut hackrf_transfer) -> c_int;

const HACKRF_SUCCESS: c_int = 0;
const HACKRF_TRUE: c_int = 1;

unsafe extern "C" {
    fn hackrf_init() -> c_int;
    fn hackrf_exit() -> c_int;
    fn hackrf_library_version() -> *const c_char;
    fn hackrf_library_release() -> *const c_char;
    fn hackrf_error_name(errcode: c_int) -> *const c_char;
    fn hackrf_device_list() -> *mut hackrf_device_list_t;
    fn hackrf_device_list_free(list: *mut hackrf_device_list_t);
    fn hackrf_open_by_serial(serial: *const c_char, device: *mut *mut hackrf_device) -> c_int;
    fn hackrf_close(device: *mut hackrf_device) -> c_int;
    fn hackrf_start_rx(
        device: *mut hackrf_device,
        callback: hackrf_sample_block_cb_fn,
        rx_ctx: *mut c_void,
    ) -> c_int;
    fn hackrf_stop_rx(device: *mut hackrf_device) -> c_int;
    fn hackrf_is_streaming(device: *mut hackrf_device) -> c_int;
    fn hackrf_set_freq(device: *mut hackrf_device, freq_hz: u64) -> c_int;
    fn hackrf_set_sample_rate(device: *mut hackrf_device, freq_hz: f64) -> c_int;
    fn hackrf_set_baseband_filter_bandwidth(device: *mut hackrf_device, bandwidth_hz: u32)
    -> c_int;
    fn hackrf_compute_baseband_filter_bw(bandwidth_hz: u32) -> u32;
    fn hackrf_set_lna_gain(device: *mut hackrf_device, value: u32) -> c_int;
    fn hackrf_set_vga_gain(device: *mut hackrf_device, value: u32) -> c_int;
    fn hackrf_set_amp_enable(device: *mut hackrf_device, value: u8) -> c_int;
    fn hackrf_set_antenna_enable(device: *mut hackrf_device, value: u8) -> c_int;
    fn hackrf_board_id_read(device: *mut hackrf_device, value: *mut u8) -> c_int;
    fn hackrf_board_id_name(board_id: c_int) -> *const c_char;
    fn hackrf_board_rev_read(device: *mut hackrf_device, value: *mut u8) -> c_int;
    fn hackrf_version_string_read(
        device: *mut hackrf_device,
        version: *mut c_char,
        length: u8,
    ) -> c_int;
    fn hackrf_usb_api_version_read(device: *mut hackrf_device, version: *mut u16) -> c_int;
    fn hackrf_board_partid_serialno_read(
        device: *mut hackrf_device,
        out: *mut read_partid_serialno_t,
    ) -> c_int;
    fn hackrf_get_transfer_buffer_size(device: *mut hackrf_device) -> usize;
}

/// Library users: `hackrf_init` on the first open, `hackrf_exit` after the last close.
static LIBRARY_USERS: Mutex<usize> = Mutex::new(0);

fn c_string(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: libhackrf returns static NUL-terminated strings (or NULL, handled above).
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

fn check(operation: &'static str, rc: c_int) -> Result<(), SourceError> {
    if rc == HACKRF_SUCCESS {
        return Ok(());
    }
    // SAFETY: `hackrf_error_name` accepts any value and returns a static string.
    let name = c_string(unsafe { hackrf_error_name(rc) });
    Err(SourceError::Device {
        source_name: super::HackRfSource::NAME,
        operation,
        message: format!("{name} ({rc})"),
    })
}

/// The serial of every connected HackRF (`None` where libhackrf could not read one), for naming
/// the device in a refused open (T-892). Enumeration is what `hackrf_open_by_serial` already does
/// internally, so it disturbs a device held elsewhere no more than the failed open did.
/// Call with the library acquired.
fn listed_serials() -> Vec<Option<String>> {
    // SAFETY: the library is initialised (caller holds a `LIBRARY_USERS` reference).
    let list = unsafe { hackrf_device_list() };
    if list.is_null() {
        return Vec::new();
    }
    // SAFETY: non-NULL list from libhackrf; read only, freed below.
    let l = unsafe { &*list };
    let n = usize::try_from(l.devicecount).unwrap_or(0);
    let serials = (0..n)
        .map(|i| {
            if l.serial_numbers.is_null() {
                return None;
            }
            // SAFETY: `serial_numbers` has `devicecount` entries, each NULL or a C string.
            let p = unsafe { *l.serial_numbers.add(i) };
            (!p.is_null()).then(|| c_string(p))
        })
        .collect();
    // SAFETY: the list came from `hackrf_device_list` and is freed exactly once.
    unsafe { hackrf_device_list_free(list) };
    serials
}

fn acquire_library() -> Result<(), SourceError> {
    let mut users = LIBRARY_USERS.lock().unwrap_or_else(PoisonError::into_inner);
    if *users == 0 {
        // SAFETY: plain library initialisation, serialised by the lock.
        check("hackrf_init", unsafe { hackrf_init() })?;
    }
    *users += 1;
    Ok(())
}

fn release_library() {
    let mut users = LIBRARY_USERS.lock().unwrap_or_else(PoisonError::into_inner);
    *users = users.saturating_sub(1);
    if *users == 0 {
        // SAFETY: no device is open any more (every close ran before this release).
        unsafe { hackrf_exit() };
    }
}

/// Size of one libhackrf USB transfer, bytes.
pub(crate) fn transfer_buffer_size() -> usize {
    // SAFETY: the device argument is unused by the library (documented in hackrf.h).
    unsafe { hackrf_get_transfer_buffer_size(std::ptr::null_mut()) }
}

/// The RX callback: copies the transfer into the pool and returns immediately.
unsafe extern "C" fn rx_callback(transfer: *mut hackrf_transfer) -> c_int {
    if transfer.is_null() {
        return 0;
    }
    // SAFETY: libhackrf passes a transfer valid for the duration of the call.
    let t = unsafe { &*transfer };
    if t.rx_ctx.is_null() || t.buffer.is_null() || t.valid_length <= 0 {
        return 0;
    }
    // SAFETY: `rx_ctx` is the `TransferPool` given to `hackrf_start_rx`; `LibHackRf` keeps that
    // `Arc` alive until after `hackrf_stop_rx` and `hackrf_close` have returned.
    let pool = unsafe { &*(t.rx_ctx as *const TransferPool) };
    let len = (t.valid_length as usize).min(t.buffer_length.max(0) as usize);
    // SAFETY: `buffer` holds `buffer_length` bytes, of which `valid_length` are valid.
    let data = unsafe { std::slice::from_raw_parts(t.buffer, len) };
    pool.on_transfer(data, hk_model::Timestamp::now().as_unix_nanos());
    0
}

/// An open HackRF One, receive only.
pub(crate) struct LibHackRf {
    dev: *mut hackrf_device,
    streaming: bool,
    pool: Option<Arc<TransferPool>>,
}

// SAFETY: the handle is only used through `&mut self` (one thread at a time); libhackrf devices
// may be driven from any thread. The callback thread only touches the pool.
unsafe impl Send for LibHackRf {}

impl LibHackRf {
    /// Opens `serial` (or the first device) and reads its identity.
    pub fn open(serial: Option<&str>) -> Result<(Self, HackRfDeviceInfo), SourceError> {
        acquire_library()?;
        let wanted = match serial.map(CString::new).transpose() {
            Ok(s) => s,
            Err(_) => {
                release_library();
                return Err(SourceError::OutOfRange {
                    what: "serial number (contains NUL)",
                    value: 0.0,
                });
            }
        };
        let mut dev: *mut hackrf_device = std::ptr::null_mut();
        let ptr = wanted.as_ref().map_or(std::ptr::null(), |s| s.as_ptr());
        // SAFETY: `ptr` is NULL or a NUL-terminated string alive for the call; `dev` is written.
        let rc = unsafe { hackrf_open_by_serial(ptr, &mut dev) };
        if rc != HACKRF_SUCCESS {
            // SAFETY: `hackrf_error_name` accepts any value and returns a static string.
            let name = c_string(unsafe { hackrf_error_name(rc) });
            let device = super::describe_open_target(serial, &listed_serials());
            release_library();
            return Err(super::open_failure(rc, &name, device));
        }
        let device = Self {
            dev,
            streaming: false,
            pool: None,
        };
        let info = device.read_info()?;
        Ok((device, info))
    }

    fn read_info(&self) -> Result<HackRfDeviceInfo, SourceError> {
        let mut board_id = 0u8;
        // SAFETY: valid open device and out-pointer.
        check("hackrf_board_id_read", unsafe {
            hackrf_board_id_read(self.dev, &mut board_id)
        })?;
        let mut version = [0 as c_char; 256];
        // SAFETY: the buffer holds 256 bytes; the length excludes the NUL (hackrf.h).
        check("hackrf_version_string_read", unsafe {
            hackrf_version_string_read(self.dev, version.as_mut_ptr(), 255)
        })?;
        let mut usb_api = 0u16;
        // SAFETY: valid open device and out-pointer.
        check("hackrf_usb_api_version_read", unsafe {
            hackrf_usb_api_version_read(self.dev, &mut usb_api)
        })?;
        let mut ids = read_partid_serialno_t::default();
        // SAFETY: valid open device and out-pointer to a correctly laid out struct.
        check("hackrf_board_partid_serialno_read", unsafe {
            hackrf_board_partid_serialno_read(self.dev, &mut ids)
        })?;
        let board_rev = if usb_api >= 0x0106 {
            let mut rev = 0xFEu8;
            // SAFETY: valid open device and out-pointer; API >= 1.06 supports the request.
            (unsafe { hackrf_board_rev_read(self.dev, &mut rev) } == HACKRF_SUCCESS).then_some(rev)
        } else {
            None
        };
        let s = ids.serial_no;
        Ok(HackRfDeviceInfo {
            serial: format!("{:08x}{:08x}{:08x}{:08x}", s[0], s[1], s[2], s[3]),
            board_id,
            // SAFETY: accepts any id and returns a static string.
            board_name: c_string(unsafe { hackrf_board_id_name(c_int::from(board_id)) }),
            board_rev,
            // SAFETY: NUL-terminated within the zero-initialised 256-byte buffer.
            firmware: c_string(version.as_ptr()),
            usb_api_version: usb_api,
            // SAFETY: static strings.
            library_version: c_string(unsafe { hackrf_library_version() }),
            library_release: c_string(unsafe { hackrf_library_release() }),
        })
    }
}

impl RxDevice for LibHackRf {
    fn set_freq(&mut self, hz: u64) -> Result<(), SourceError> {
        // SAFETY: valid open device.
        check("hackrf_set_freq", unsafe { hackrf_set_freq(self.dev, hz) })
    }

    fn set_sample_rate(&mut self, hz: f64) -> Result<(), SourceError> {
        // SAFETY: valid open device.
        check("hackrf_set_sample_rate", unsafe {
            hackrf_set_sample_rate(self.dev, hz)
        })
    }

    fn set_baseband_filter(&mut self, hz: u32) -> Result<(), SourceError> {
        // SAFETY: valid open device.
        check("hackrf_set_baseband_filter_bandwidth", unsafe {
            hackrf_set_baseband_filter_bandwidth(self.dev, hz)
        })
    }

    fn compute_baseband_filter_bw(&self, hz: u32) -> u32 {
        // SAFETY: a pure table lookup.
        unsafe { hackrf_compute_baseband_filter_bw(hz) }
    }

    fn set_lna_gain(&mut self, db: u32) -> Result<(), SourceError> {
        // SAFETY: valid open device.
        check("hackrf_set_lna_gain", unsafe {
            hackrf_set_lna_gain(self.dev, db)
        })
    }

    fn set_vga_gain(&mut self, db: u32) -> Result<(), SourceError> {
        // SAFETY: valid open device.
        check("hackrf_set_vga_gain", unsafe {
            hackrf_set_vga_gain(self.dev, db)
        })
    }

    fn set_amp_enable(&mut self, on: bool) -> Result<(), SourceError> {
        // SAFETY: valid open device.
        check("hackrf_set_amp_enable", unsafe {
            hackrf_set_amp_enable(self.dev, u8::from(on))
        })
    }

    fn set_antenna_enable(&mut self, on: bool) -> Result<(), SourceError> {
        // SAFETY: valid open device.
        check("hackrf_set_antenna_enable", unsafe {
            hackrf_set_antenna_enable(self.dev, u8::from(on))
        })
    }

    fn start_rx(&mut self, pool: Arc<TransferPool>) -> Result<(), SourceError> {
        let ctx = Arc::as_ptr(&pool) as *mut c_void;
        self.pool = Some(pool);
        // SAFETY: valid open device; `ctx` stays alive in `self.pool` until after close (Drop).
        check("hackrf_start_rx", unsafe {
            hackrf_start_rx(self.dev, rx_callback, ctx)
        })?;
        self.streaming = true;
        Ok(())
    }

    fn stop_rx(&mut self) -> Result<(), SourceError> {
        if !self.streaming {
            return Ok(());
        }
        self.streaming = false;
        // SAFETY: valid open device; waits for in-flight transfers to finish.
        check("hackrf_stop_rx", unsafe { hackrf_stop_rx(self.dev) })
    }

    fn is_streaming(&self) -> bool {
        // SAFETY: valid open device.
        self.streaming && unsafe { hackrf_is_streaming(self.dev) } == HACKRF_TRUE
    }
}

impl Drop for LibHackRf {
    fn drop(&mut self) {
        let _ = self.stop_rx();
        // Keep the bias tee off for whoever opens the device next.
        // SAFETY: valid open device; closed exactly once, below.
        unsafe {
            hackrf_set_antenna_enable(self.dev, 0);
            hackrf_close(self.dev);
        }
        // The callback context may be released only once the device is closed.
        self.pool = None;
        release_library();
    }
}
