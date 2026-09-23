//! `hk-fake-satdump`: a stand-in for the real `satdump` CLI, used only by
//! `hk-plugins/tests/satdump.rs` via `HK_SATDUMP` to exercise `hk-plugin-satdump`'s FIFO
//! rendezvous, product reporting and crash handling without a real SatDump install or a real
//! polar-orbiter pass. Never shipped or referenced by a manifest.
//!
//! Reads `--file_path <fifo>` and `--out_dir <dir>` off its own argv (the two flags
//! `hk-plugin-satdump` always appends), opens the FIFO for reading (completing the wrapper's
//! writer-thread rendezvous) and drains it. Mode selected by `FAKE_SATDUMP_MODE`
//! (default `product_after_bytes`):
//! - `product_after_bytes`: once at least `FAKE_SATDUMP_PRODUCT_AFTER_BYTES` (default 4096) bytes
//!   have been read from the FIFO, writes a small file into `<out_dir>/nested/product.png` in a
//!   few chunks a beat apart, so the wrapper's watcher thread genuinely observes it settle rather
//!   than appearing already-stable on the first scan. Exits 0 on FIFO EOF (the wrapper closed its
//!   write end: a clean shutdown).
//! - `crash_immediately`: exits non-zero without opening the FIFO, so the wrapper's writer thread
//!   blocks on `open()` forever — this mode is for a test that kills the wrapper itself, not one
//!   that waits on it.
//! - `hang`: opens the FIFO, drains it, but never writes a product and never exits until killed
//!   (exercises the wrapper's shutdown-grace/kill path).

use std::fs;
use std::io::{Read, Write};
use std::time::Duration;

fn arg_value(flag: &str) -> Option<String> {
    let mut args = std::env::args();
    while let Some(a) = args.next() {
        if a == flag {
            return args.next();
        }
    }
    None
}

fn main() {
    let mode = std::env::var("FAKE_SATDUMP_MODE").unwrap_or_else(|_| "product_after_bytes".into());
    if mode == "crash_immediately" {
        eprintln!("fake satdump: crashing immediately");
        std::process::exit(3);
    }

    let fifo_path = arg_value("--file_path").expect("fake satdump: missing --file_path");
    let out_dir = arg_value("--out_dir").expect("fake satdump: missing --out_dir");
    let threshold: usize = std::env::var("FAKE_SATDUMP_PRODUCT_AFTER_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4096);

    let mut fifo = std::fs::File::open(&fifo_path).expect("fake satdump: opening fifo failed");
    let mut total = 0usize;
    let mut wrote_product = false;
    let mut buf = [0u8; 65_536];
    loop {
        let n = match fifo.read(&mut buf) {
            Ok(0) => break, // EOF: the wrapper closed its write end (clean shutdown)
            Ok(n) => n,
            Err(_) => break,
        };
        total += n;

        if mode == "product_after_bytes" && !wrote_product && total >= threshold {
            wrote_product = true;
            // Written on its own thread so this loop keeps draining the fifo continuously (a real
            // decoder's disk I/O never gets to stall its own input read), while the product still
            // grows in a few chunks a beat apart, so the wrapper's stability debounce is genuinely
            // exercised rather than seeing an already-finished file on its first scan.
            let out_dir = out_dir.clone();
            std::thread::spawn(move || {
                let dir = std::path::Path::new(&out_dir).join("nested");
                fs::create_dir_all(&dir).expect("fake satdump: creating out_dir failed");
                let product_path = dir.join("product.png");
                let mut f =
                    fs::File::create(&product_path).expect("fake satdump: creating product failed");
                for chunk in [b"PNGhead".as_slice(), b"...more bytes...", b"...end"] {
                    f.write_all(chunk).unwrap();
                    f.flush().unwrap();
                    std::thread::sleep(Duration::from_millis(300));
                }
            });
        }
    }

    if mode == "hang" {
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
    std::process::exit(0);
}
