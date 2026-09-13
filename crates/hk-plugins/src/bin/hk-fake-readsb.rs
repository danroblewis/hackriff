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

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

fn now_ms(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

fn main() {
    let mode = std::env::var("FAKE_READSB_MODE").unwrap_or_else(|_| "idle_wedge".into());
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
