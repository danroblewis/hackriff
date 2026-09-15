//! `hk-fake-readsb`: a stand-in for readsb, used only by `hk-plugins/tests/readsb.rs` (T-015
//! review) via `HK_READSB` to exercise `hk-plugin-readsb`'s crash/idle handling without a 9s
//! real-readsb wait or shell-timing tricks. Never shipped or referenced by a manifest.
//!
//! Mode selected by `FAKE_READSB_MODE` (default `idle_wedge`):
//! - `wedge_immediately`: drain whatever is already buffered on stdin (best effort, non-blocking
//!   in spirit — a single read call), print the real readsb "SDR wedged" line to stderr, then
//!   sleep far longer than any test should wait, before eventually exiting. A test that sees the
//!   wrapper end quickly anyway is exercising the stderr-detection path, not the waiter thread.
//! - `idle_wedge`: reads stdin continuously, tracking the time since the last byte arrived; once
//!   that exceeds `FAKE_READSB_IDLE_MS` (default 5000) it prints the wedge line and exits, the
//!   same as real readsb's own ~9s watchdog but on a test-friendly timescale. A test that keeps
//!   this from ever firing is exercising the wrapper's keepalive.
//! - `late_beast` (T-103): a slow-starting readsb. It opens the `--net-connector` Beast
//!   connection only `FAKE_READSB_CONNECT_DELAY_MS` (default 3000) after start, as real readsb
//!   did under heavy load, and "decodes" one CRC-valid DF17 squitter ([`LATE_BEAST_FRAME`]) at the
//!   first input sample that is not silence (`uc8` 0x80). Like readsb, the frame is forwarded over
//!   Beast (stamped from the sample count since the first byte read) only if the connection
//!   exists by then; the `--raw` line is always printed. Exits 0 at stdin EOF.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn now_ms(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

fn main() {
    let mode = std::env::var("FAKE_READSB_MODE").unwrap_or_else(|_| "idle_wedge".into());
    if mode == "late_beast" {
        late_beast();
    }
    if mode == "wedge_immediately" {
        let mut buf = [0u8; 4096];
        let _ = std::io::stdin().read(&mut buf);
        eprintln!("<3>SDR wedged, exiting!");
        std::thread::sleep(Duration::from_secs(60));
        std::process::exit(2);
    }

    let idle_ms: u64 = std::env::var("FAKE_READSB_IDLE_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5000);
    let start = Instant::now();
    let last_byte_ms = Arc::new(AtomicU64::new(now_ms(start)));

    let watchdog_last = Arc::clone(&last_byte_ms);
    thread_spawn_watchdog(start, watchdog_last, idle_ms);

    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 65_536];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => {
                // Clean EOF: the wrapper closed our stdin on purpose.
                eprintln!("fake readsb: stdin closed, exiting cleanly");
                std::process::exit(0);
            }
            Ok(_) => last_byte_ms.store(now_ms(start), Ordering::Relaxed),
            Err(_) => std::process::exit(0),
        }
    }
}

fn thread_spawn_watchdog(start: Instant, last_byte_ms: Arc<AtomicU64>, idle_ms: u64) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(100));
            let elapsed = now_ms(start).saturating_sub(last_byte_ms.load(Ordering::Relaxed));
            if elapsed > idle_ms {
                eprintln!("<3>SDR wedged, exiting!");
                std::process::exit(2);
            }
        }
    });
}

/// The DF17 identification squitter `late_beast` "decodes" (ICAO 4840d6, CRC-24 valid).
const LATE_BEAST_FRAME: [u8; 14] = [
    0x8d, 0x48, 0x40, 0xd6, 0x20, 0x2c, 0xc3, 0x71, 0xc3, 0x2c, 0xe0, 0x57, 0x60, 0x98,
];
/// The sample rate the wrapper's readsb input runs at (the readsb manifest's only rate).
const LATE_BEAST_RATE_HZ: f64 = 2_400_000.0;

/// `late_beast` mode (module doc).
fn late_beast() -> ! {
    let port = std::env::args()
        .find_map(|a| {
            let spec = a.strip_prefix("--net-connector=")?;
            spec.split(',').nth(1)?.parse::<u16>().ok()
        })
        .unwrap_or_else(|| {
            eprintln!("fake readsb: late_beast needs --net-connector");
            std::process::exit(9);
        });
    let delay_ms: u64 = std::env::var("FAKE_READSB_CONNECT_DELAY_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3000);
    let beast: Arc<Mutex<Option<TcpStream>>> = Arc::new(Mutex::new(None));
    let connector = Arc::clone(&beast);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(delay_ms));
        if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) {
            *connector.lock().unwrap() = Some(stream);
        }
    });

    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 65_536];
    let mut bytes_read = 0u64;
    let mut decoded = false;
    loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) | Err(_) => std::process::exit(0),
            Ok(n) => n,
        };
        if !decoded && let Some(pos) = buf[..n].iter().position(|&b| b != 0x80) {
            decoded = true;
            let sample = (bytes_read + pos as u64) / 2;
            // readsb stamps 200 µs after the preamble start, on a 12 MHz clock from its first
            // sample (hk-plugin-readsb "Timestamps").
            let ticks = ((sample as f64 / LATE_BEAST_RATE_HZ + 200e-6) * 12e6).round() as u64;
            if let Some(stream) = beast.lock().unwrap().as_mut() {
                let mut frame = vec![0x1a, 0x33];
                let mut body = ticks.to_be_bytes()[2..].to_vec();
                body.push(0xff);
                body.extend_from_slice(&LATE_BEAST_FRAME);
                for b in body {
                    frame.push(b);
                    if b == 0x1a {
                        frame.push(0x1a);
                    }
                }
                let _ = stream.write_all(&frame).and_then(|()| stream.flush());
            }
            let hex: String = LATE_BEAST_FRAME
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect();
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "*{hex};").and_then(|()| out.flush());
        }
        bytes_read += n as u64;
    }
}
