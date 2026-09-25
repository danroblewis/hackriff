//! T-940: **an FM station that is on the air reads `live`**, through the mock SDR device and the
//! real HTTP inventory route, under a retune — the explorer's query, verbatim.
//!
//! The defect (2026-09-25, live HackRF): `/api/inventory?t0=now-60&t1=now` listed every FM station,
//! **all `liveness: ended`**, including the Confirmed 101.3 MHz station while it was on air and
//! decoding, and the Selected panel said "Ended … — provisional". That breaks the signal-model
//! invariant (CLAUDE.md, ADR-0017/0019): a signal is ongoing (`end = null`) **until an end is
//! affirmatively detected**, and time the receiver did not observe is never silence.
//!
//! Root cause, three defects compounding on the one path the poll reads:
//!
//! 1. The inventory derives `open` as `now − t_end ≤ idle_gap`, with `t_end` the observation
//!    ledger's row for the station's track, and under a live dwell the gap is the 1 s floor. But
//!    an open track's row was written only by the **5 s** live offer — and a track whose entry came
//!    from a chain (a WFM station, which the offer used to skip) had no track row at all, so its
//!    `t_end` stayed at the chain's first 500 ms window ("Extent 11:59:16 → 11:59:16 · 500 ms").
//! 2. The silence was measured to the **wall clock**, so time the detector had not yet analysed
//!    and time the receiver was tuned elsewhere both counted as observed absence.
//! 3. A continuous carrier is cut into `max_duration_s` (1 s) split records, so even the tracker's
//!    own silence read ~1 s between pieces — exactly the 1 s floor — while the burst was in flight.
//!
//! This asserts on the served row (docs/07 Emitter + presence), blind: the station is found by the
//! detector, the fixture's truth (101.3 MHz, `signal_062_pipeline.rs`) is only how the test picks
//! which row to read.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hk_cli::pipeline::{LiveArgs, TempDataDirGuard, temp_data_dir};
use hk_cli::serve::{ServeOptions, ServeSource, Serving, start};
use serde_json::Value;

const TOKEN: &str = "t940-liveness-token-0123456789abcdef";
/// The fixture's known station (the truth `signal_062_pipeline.rs` and `api_contract.rs` share).
const STATION_HZ: f64 = 101.3e6;
const FIXTURE_CENTER_HZ: f64 = 100.8e6;
const FIXTURE_RATE_HZ: f64 = 2.4e6;
/// How long each phase polls the row, s. Many times the 1 s idle floor and more than the old 5 s
/// offer cadence, so a row that reads `live` only between refreshes cannot pass by luck.
const WATCH_S: f64 = 12.0;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/hackrf/2026-09-13/fm_100p8M_2p4M_l32g30a1_t1p5_5s.sigmf-meta")
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

/// `hk serve` over the mock SDR on the fixture, with the IQ ring on — the ring's tune journal is
/// what makes the inventory measure a 1 s idle gap for a band under continuous dwell, which is the
/// staging condition. The guard comes first so it drops last (T-236).
fn start_server() -> (TempDataDirGuard, Serving, SocketAddr) {
    let dir = temp_data_dir();
    let guard = TempDataDirGuard::new(dir.clone());
    let serving = start(&ServeOptions {
        source: ServeSource::HackRf {
            spec: format!("mock:{}", fixture_path().display()),
            extra: Vec::new(),
            live: LiveArgs::default(),
        },
        data_dir: Some(dir),
        bind: "127.0.0.1:0".parse().unwrap(),
        ui_dist: None,
        fft_len: 1024,
        rows_per_s: 25.0,
        calibration: None,
        token: Some(TOKEN.into()),
        listen: Default::default(),
        compute: Default::default(),
        iq_buffer: hk_cli::pipeline::IqBufferArgs {
            retention_s: None,
            max_bytes: Some(64 << 20),
        },
        iq_buffer_hooks: None,
    })
    .unwrap();
    let addr = serving.server.local_addr();
    (guard, serving, addr)
}

fn stop_server(serving: Serving) {
    serving.handle.stop();
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = serving.handle;
    let waiter = std::thread::spawn(move || {
        let _ = tx.send(handle.wait());
    });
    let _ = rx.recv_timeout(Duration::from_secs(30));
    let _ = waiter.join();
    drop(serving.server);
}

fn call(addr: SocketAddr, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let body = body.unwrap_or("");
    let ct = if body.is_empty() {
        String::new()
    } else {
        format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        )
    };
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {TOKEN}\r\n{ct}\
         Connection: close\r\n\r\n{body}"
    )
    .unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let status = raw
        .get(9..12)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("malformed response head: {raw:?}"));
    let body = raw.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

/// The explorer's query: the last minute, ending at the wall clock's now.
fn station_rows(addr: SocketAddr) -> Vec<Value> {
    let now = unix_now();
    let (st, v) = call(
        addr,
        "GET",
        &format!(
            "/api/inventory?t0={:.3}&t1={:.3}&limit=500",
            now - 60.0,
            now
        ),
        None,
    );
    assert_eq!(st, 200, "{v}");
    v["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .filter(|e| {
            e["f_center_hz"]
                .as_f64()
                .is_some_and(|f| (f - STATION_HZ).abs() < 50e3)
        })
        .cloned()
        .collect()
}

/// One device action (T-529) to `center_hz` at the fixture's rate.
fn retune(addr: SocketAddr, center_hz: f64) {
    let step = hk_core::source::HACKRF_ONE_TUNING_STEP_HZ;
    let center = step * (center_hz / step).round();
    let (st, r) = call(
        addr,
        "POST",
        "/api/control/window",
        Some(&format!(
            "{{\"center_hz\":{center:?},\"sample_rate_hz\":{FIXTURE_RATE_HZ:?}}}"
        )),
    );
    assert_eq!(st, 200, "{r}");
}

/// What one phase of polling saw.
struct Watch {
    reads: usize,
    /// Every read in which **no** row for the station read `live`, with the rows it did read and
    /// the detector's counters — the evidence a failure prints.
    bad: Vec<String>,
    /// The longest run of consecutive not-live reads.
    longest: usize,
    last_live: bool,
}

impl Watch {
    /// **Ongoing**, as the product promises it: live on at least three reads in four, never
    /// not-live for longer than [`MAX_STREAK`] reads in a row, and live on the last read.
    ///
    /// Why not every read: an END is **provisional** (ADR-0019 §6.2). When the detector loses the
    /// carrier for a moment — a retune, or a dropped-sample STFT reset on a loaded box
    /// (`gap_samples` in the evidence) — the tracker may observe a silence past the 1 s floor and
    /// end the interval, and the carrier's next record revokes it. That blip is the documented,
    /// revocable end. The defect was a *persistent* one: on the old code the steady dwell read
    /// not-live on **19 of 23** reads, in streaks of ~10 reads (the row's end advanced in exact 5 s
    /// steps against a 1 s gap), which fails every clause here.
    fn ongoing(&self) -> bool {
        self.last_live && self.bad.len() * 4 <= self.reads && self.longest <= MAX_STREAK
    }
}

/// Longest tolerated run of not-live reads: ~2.5 s at the poll cadence, which a provisional END
/// revoked by the carrier's next record fits inside and the old 5 s refresh cadence does not.
const MAX_STREAK: usize = 5;

/// Polls for `secs`.
fn watch(addr: SocketAddr, secs: f64) -> Watch {
    let deadline = Instant::now() + Duration::from_secs_f64(secs);
    let mut w = Watch {
        reads: 0,
        bad: Vec::new(),
        longest: 0,
        last_live: false,
    };
    let mut streak = 0;
    while Instant::now() < deadline {
        let rows = station_rows(addr);
        w.reads += 1;
        w.last_live = rows
            .iter()
            .any(|r| r["presence"]["liveness"].as_str() == Some("live"));
        if w.last_live {
            streak = 0;
        } else {
            streak += 1;
            w.longest = w.longest.max(streak);
            let brief: Vec<String> = rows
                .iter()
                .map(|r| {
                    format!(
                        "{} {} liveness={} last_interval={} silence_s={}",
                        r["id"],
                        r["state"],
                        r["presence"]["liveness"],
                        r["presence"]["last_interval"],
                        r["presence"]["silence_s"]
                    )
                })
                .collect();
            let (_, status) = call(addr, "GET", "/api/status", None);
            w.bad.push(format!(
                "t={:.2}: {brief:?} detect={}",
                unix_now(),
                status["readers"]["detect"]
            ));
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    w
}

fn assert_ongoing(what: &str, w: &Watch) {
    assert!(
        w.ongoing(),
        "{what}: the station read not-live in {} of {} in-window reads, longest run {}, last read \
         live: {}: {:#?}",
        w.bad.len(),
        w.reads,
        w.longest,
        w.last_live,
        w.bad
    );
}

/// After a retune the detector re-acquires the carrier on the new segment; its first record comes
/// one split record (1 s) or more after the discontinuity. A phase after a retune is judged after
/// this settle.
const SETTLE_S: f64 = 5.0;

#[test]
fn an_on_air_station_reads_live_in_the_window_through_a_retune() {
    let (_dir_guard, serving, addr) = start_server();

    // The station is found blind and given a row.
    let deadline = Instant::now() + Duration::from_secs(90);
    while station_rows(addr).is_empty() {
        assert!(
            Instant::now() < deadline,
            "the 101.3 MHz station never reached the inventory"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    // Let the run settle past its first live offer, so a pass here is not "the first write was
    // still fresh".
    std::thread::sleep(Duration::from_secs(6));

    // ---- a continuous carrier under a steady dwell is ongoing ----
    assert_ongoing("steady dwell", &watch(addr, WATCH_S));

    // ---- a retune that keeps the station in the window closes nothing ----
    retune(addr, FIXTURE_CENTER_HZ + 200e3);
    std::thread::sleep(Duration::from_secs_f64(SETTLE_S));
    assert_ongoing(
        "after a retune that kept it in the window",
        &watch(addr, WATCH_S),
    );

    // ---- tuned AWAY, the station is unobserved, and unobserved is not quiet ----
    // 98.0 MHz ± 1.2 MHz does not reach 101.3 MHz: nothing can be measured there, so nothing can
    // have been measured to stop, and the row stays ongoing by assumption (ADR-0019 §1). Every
    // read: with no detection on its band there is nothing that could even provisionally end it.
    retune(addr, 98.0e6);
    std::thread::sleep(Duration::from_secs_f64(SETTLE_S));
    let away = watch(addr, WATCH_S);
    assert!(
        away.bad.is_empty(),
        "tuned away, the station read not-live in {} of {} reads — a silence nobody observed was \
         taken for an end: {:#?}",
        away.bad.len(),
        away.reads,
        away.bad
    );

    // ---- and back on it, still on the air ----
    retune(addr, FIXTURE_CENTER_HZ);
    std::thread::sleep(Duration::from_secs_f64(SETTLE_S));
    assert_ongoing("back on the band", &watch(addr, WATCH_S));

    stop_server(serving);
}
