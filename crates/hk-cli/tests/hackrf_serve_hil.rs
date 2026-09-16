//! HIL (T5, T-042): `hk serve --hackrf` runs the whole pipeline over the live HackRF One and the
//! API inventory shows real detections. **Receive only.** Ignored; run manually after checking the
//! device is free (`hackrf_info`):
//!
//! ```text
//! cargo test -p hk-cli --features hackrf --test hackrf_serve_hil -- --ignored --nocapture
//! ```
//!
//! 100.8 MHz / 2.4 Msps, LNA 32 / VGA 30 / amp on, ~20 s. The server is started in-process and
//! stopped at the end (nothing is left running).
#![cfg(feature = "hackrf")]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use hk_cli::pipeline::{LiveArgs, TempDataDirGuard, temp_data_dir};
use hk_cli::serve::{ServeOptions, ServeSource, Serving, start};
use hk_core::NamedGain;

const TOKEN: &str = "t042-hil-serve-token-0123456789abcdef";

fn get(addr: SocketAddr, path: &str) -> (u16, serde_json::Value) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let status = raw[9..12].parse().unwrap();
    let body = raw.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (
        status,
        serde_json::from_str(body).unwrap_or(serde_json::Value::Null),
    )
}

#[test]
#[ignore = "needs a HackRF One and FM broadcast reception (run with --ignored)"]
fn live_serve_inventory_shows_real_fm_detections() {
    let dir = temp_data_dir();
    let _guard = TempDataDirGuard::new(dir.clone());
    let Serving {
        server,
        handle,
        live_control,
        source_control,
        ..
    } = start(&ServeOptions {
        source: ServeSource::HackRf {
            spec: "hackrf".into(),
            live: LiveArgs {
                center_hz: 100.8e6,
                sample_rate_hz: 2.4e6,
                lna_db: 32.0,
                vga_db: 30.0,
                amp: true,
                baseband_filter_hz: None,
                gains: Vec::new(),
            },
        },
        data_dir: Some(dir.clone()),
        bind: "127.0.0.1:0".parse().unwrap(),
        ui_dist: None,
        fft_len: 1024,
        rows_per_s: 25.0,
        calibration: None,
        token: Some(TOKEN.into()),
        listen: Default::default(),
        compute: Default::default(),
        // T-178: the ring is allocated up front; keep the test's small.
        iq_buffer: hk_cli::pipeline::IqBufferArgs {
            retention_s: None,
            max_bytes: Some(256 << 20),
        },
    })
    .expect("start hk serve over the HackRF (is it free?)");
    let addr = server.local_addr();
    let (_, first) = get(addr, "/api/inventory");
    eprintln!(
        "inventory at start: {} entries",
        first["entries"].as_array().map_or(0, Vec::len)
    );

    // Live control is offered and validated; window changes re-plumb the run (T-050): into the
    // paging band (restricted class) and back, and a rate change and back.
    let lc = live_control.expect("live control for the live radio");
    assert_eq!(lc.set_center(500e3).unwrap_err().http_status(), 400);
    assert_eq!(
        lc.set_center(930.5e6)
            .expect("retune into paging")
            .center_hz,
        930.5e6
    );
    let (_, state) = get(addr, "/api/status");
    assert_eq!(
        state["control"]["content_class"], "restricted-paging",
        "the class follows the window"
    );
    assert_eq!(
        lc.set_center(100.8e6).expect("back to FM").center_hz,
        100.8e6
    );
    assert_eq!(lc.set_rate(10e6).expect("rate change").sample_rate_hz, 10e6);
    assert_eq!(lc.set_rate(2.4e6).expect("rate back").sample_rate_hz, 2.4e6);
    let t = lc
        .set_gains(&[NamedGain::new("lna", 32.0), NamedGain::new("vga", 30.0)])
        .expect("named gains");
    assert_eq!(t.bias_tee, Some(false), "the HackRF has a bias tee, off");

    let start_t = Instant::now();
    while start_t.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(500));
    }
    let (status, inv) = get(addr, "/api/inventory");
    assert_eq!(status, 200);
    let entries = inv["entries"].as_array().cloned().unwrap_or_default();
    let fm: Vec<f64> = entries
        .iter()
        .filter_map(|e| e["f_center_hz"].as_f64())
        .filter(|f| (88e6..108e6).contains(f))
        .collect();
    let (_, st) = get(addr, "/api/status");
    eprintln!(
        "inventory after 20 s: {} entries, FM-band centres (MHz): {:?}\nstatus source: {}",
        entries.len(),
        fm.iter()
            .map(|f| (f / 1e4).round() / 100.0)
            .collect::<Vec<_>>(),
        st["source"]
    );
    handle.stop();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(handle.wait().map_err(|e| format!("{e:#}")));
    });
    let summary = rx
        .recv_timeout(Duration::from_secs(60))
        .expect("the live run stops")
        .unwrap();
    eprintln!("{}", summary.to_text());
    let stats = source_control
        .and_then(|c| c.stats())
        .expect("source stats");
    eprintln!("source: {stats:?}");
    drop(server);
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    assert!(
        summary.counter("/source/samples") > 20_000_000,
        "samples flowed"
    );
    assert!(!fm.is_empty(), "real FM detections in the inventory");
}
