//! T-964: **a completed survey pass must light the coverage map over the range it swept.**
//!
//! The measured failure, on the live HackRF through the shipped UI (`Scan everything (fast)`,
//! 1 MHz–6 GHz): after a pass that visited every step the banner read *"4.3 % of this surface was
//! ever sampled (176 of 4096 coverage cells)"* and the survey view showed a faint dotted row. Two
//! separate things were true at once, and only one of them was a defect:
//!
//!  1. **The records were there.** Every step's tune interval reached the observation log with its
//!     own centre and its own interval, and `/api/coverage` folded them — over a window that
//!     contains the pass. That is what this file pins, through the mock SDR device, so a regression
//!     in the recording path (the interactive observer, one dwell record per steady tune) is caught
//!     by a test about the user-visible feature rather than by a test about records.
//!  2. **The question being asked was the wrong one.** "Ever sampled" is a question about the
//!     *frequency* axis; the number quoted was the share of (time × frequency) **cells**. A front
//!     end sees one window at a time, so a *finished* full-range pass can only ever occupy a thin
//!     diagonal of that grid: 4.3 % was arithmetically true of a 128 × 32 grid and false of the
//!     survey. So `GET /api/coverage` now serves the collapse itself — `bands` — with its rule
//!     attached, and a client quotes it instead of folding a grid into a claim of its own.
//!
//! Through the mock SDR device, as the e2e rule requires: `hk serve --device mock:<fixture>` and the
//! sweep started the way the app starts it, `POST /api/control/scan`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use hk_cli::pipeline::{LiveArgs, TempDataDirGuard, temp_data_dir};
use hk_cli::serve::{ServeOptions, ServeSource, Serving, start};
use serde_json::Value;

const TOKEN: &str = "t964-scan-coverage-token-0123456789";
/// The fixture's own tuning (T-049), which the mock reports before the sweep moves it.
const FIXTURE_CENTER_HZ: f64 = 100.8e6;
/// The band the sweep walks: 20 MHz at the fixture's 2.4 Msps is 12 steps of 1.8 MHz.
const LO_HZ: f64 = 88e6;
const HI_HZ: f64 = 108e6;
/// A band the sweep never reaches, so grey can be checked to still mean grey.
const UNSWEPT_LO_HZ: f64 = 200e6;
const UNSWEPT_HI_HZ: f64 = 220e6;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/hackrf/2026-09-13/fm_100p8M_2p4M_l32g30a1_t1p5_5s.sigmf-meta")
}

/// `hk serve` over the mock SDR device, a fresh temp data dir, the guard first (T-236).
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
        // `handle` drops after the send: the observation log seals its open hour on this thread, so
        // the data directory is not free until it ends (T-236).
        let _ = tx.send(handle.wait());
    });
    let _ = rx.recv_timeout(Duration::from_secs(30));
    let _ = waiter.join();
    drop(serving.server);
}

fn call(addr: SocketAddr, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
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
        "{method} {path} HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {TOKEN}\r\n{ct}Connection: \
         close\r\n\r\n{body}"
    )
    .unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let status = raw
        .get(9..12)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("malformed response head: {raw:?}"));
    let b = raw.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, serde_json::from_str(b).unwrap_or(Value::Null))
}

fn get(addr: SocketAddr, path: &str) -> (u16, Value) {
    call(addr, "GET", path, None)
}

fn post(addr: SocketAddr, path: &str, body: &str) -> (u16, Value) {
    call(addr, "POST", path, Some(body))
}

fn wait_for(what: &str, limit: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The states of a plane, in order, as the route serves them.
fn states(plane: &Value) -> Vec<String> {
    plane["cells"]
        .as_array()
        .map(|cs| {
            cs.iter()
                .map(|c| c["state"].as_str().unwrap_or("?").to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// The `bands` census this file asserts against, folded **from the served cells** — so the served
/// census is checked against the answer beside it rather than taken on trust.
fn fold_bands(grid: &Value, plane: &Value) -> (usize, usize, usize, usize) {
    let nf = grid["cells"].as_u64().unwrap_or(0) as usize;
    let st = states(plane);
    assert!(nf > 0 && !st.is_empty() && st.len() % nf == 0, "{grid}");
    let mut rank = vec![0u8; nf];
    for (i, s) in st.iter().enumerate() {
        let r = match s.as_str() {
            "observed" => 3,
            "excluded" => 2,
            "unknown" => 1,
            _ => 0,
        };
        let f = i % nf;
        rank[f] = rank[f].max(r);
    }
    let n = |want: u8| rank.iter().filter(|r| **r == want).count();
    (n(3), n(2), n(0), n(1))
}

/// **The whole test: a pass over N steps lights every stepped cell, and says so on the axis the
/// question is about.**
#[test]
fn a_full_sweep_pass_lights_every_stepped_cell_of_the_coverage_map() {
    let (_dir_guard, serving, addr) = start_server();
    wait_for("a live front end", Duration::from_secs(30), || {
        get(addr, "/api/control/state").1["tuning"]["center_hz"].as_f64() == Some(FIXTURE_CENTER_HZ)
    });

    // ---- the sweep the app starts, at a dwell short enough to be a test ----
    let (st, v) = post(
        addr,
        "/api/control/scan",
        &format!(r#"{{"f_lo_hz": {LO_HZ}, "f_hi_hz": {HI_HZ}, "dwell_s": 0.5}}"#),
    );
    assert_eq!(st, 200, "{v}");
    let steps = v["scan"]["plan"]["steps"].as_u64().unwrap_or(0);
    assert_eq!(steps, 12, "20 MHz at 1.8 MHz of usable span per step: {v}");

    // A WHOLE pass: every step of the plan has been taken (a step's record is written when the step
    // ends, so the pass is over only once the next one has begun).
    wait_for(
        "a whole pass of the range",
        Duration::from_secs(240),
        || {
            get(addr, "/api/control/scan").1["scan"]["progress"]["steps_done"]
                .as_u64()
                .unwrap_or(0)
                > steps
        },
    );
    let (st, v) = post(addr, "/api/control/scan/stop", "{}");
    assert_eq!(st, 200, "{v}");

    // ---- the window: the server's own record horizon, which is what a client asks over ----
    //
    // Never a wall clock. The records carry capture time, and `horizon.oldest_record_s` /
    // `as_of_s` are the two ends of what this answer can speak about (`ui/src/surface/bootstrap.ts`
    // floors the surface at exactly this `oldest_record_s`). A test that invented a window from its
    // own clock would be asserting about the clock.
    let probe = format!("/api/coverage?f_lo={LO_HZ}&f_hi={HI_HZ}&cells=64");
    let (st, p) = get(addr, &probe);
    assert_eq!(st, 200, "{p}");
    let t0 = p["horizon"]["oldest_record_s"]
        .as_f64()
        .unwrap_or_else(|| panic!("the server holds no tune record at all: {p}"));
    let t1 = p["horizon"]["as_of_s"]
        .as_f64()
        .unwrap_or_else(|| panic!("no record reaches into the window: {p}"));

    // ---- 1. the swept range is lit, on the axis "ever sampled" is a question about ----
    let cells = 64;
    let rows = 32;
    let q = format!(
        "/api/coverage?f_lo={LO_HZ}&f_hi={HI_HZ}&cells={cells}&rows={rows}&t0={t0}&t1={t1}"
    );
    let (st, v) = get(addr, &q);
    assert_eq!(st, 200, "{v}");
    let bands = &v["any"]["bands"];
    assert_eq!(
        bands["cells"].as_u64(),
        Some(cells),
        "the collapse is over the frequency axis, so it has one cell per frequency cell: {bands}"
    );
    let sampled = bands["observed_cells"].as_u64().unwrap_or(0)
        + bands["excluded_cells"].as_u64().unwrap_or(0);
    assert_eq!(
        sampled,
        cells,
        "a pass that visited every step of {LO_HZ}-{HI_HZ} Hz left {} of {cells} frequency cells \
         reading as never sampled. This is T-964: the sweep IS the feature that lights the map, so \
         a completed pass that does not light the range it swept is the defect, and the share a \
         client quotes must be this one. bands = {bands}",
        cells - sampled
    );
    assert_eq!(bands["unobserved_cells"].as_u64(), Some(0), "{bands}");
    assert_eq!(
        bands["observed_fraction"].as_f64(),
        Some(1.0),
        "the whole swept range: {bands}"
    );
    assert!(
        bands["rule"]
            .as_str()
            .unwrap_or_default()
            .contains("ANY row"),
        "the collapse must state its own rule, so a client cannot read it as a claim about an \
         instant: {bands}"
    );

    // 2. The served census is the fold of the served cells — the two encodings cannot drift.
    let (obs, exc, unobs, unk) = fold_bands(&v["grid"], &v["any"]);
    assert_eq!(
        (
            bands["observed_cells"].as_u64().unwrap_or(0) as usize,
            bands["excluded_cells"].as_u64().unwrap_or(0) as usize,
            bands["unobserved_cells"].as_u64().unwrap_or(0) as usize,
            bands["unknown_cells"].as_u64().unwrap_or(0) as usize,
        ),
        (obs, exc, unobs, unk),
        "the `bands` census disagrees with the cells served beside it: {bands}"
    );

    // 3. And it is a DIFFERENT number from the grid census, which is the whole point: the grid is
    //    what the canvas draws, and a sweep occupies one window at a time. A "fix" that made the
    //    grid itself read observed everywhere would be painting spectrum as measured at instants
    //    the radio was demonstrably elsewhere.
    let grid_cells = (cells * rows) as usize;
    let grid_sampled = v["any"]["observed_cells"].as_u64().unwrap_or(0)
        + v["any"]["excluded_cells"].as_u64().unwrap_or(0);
    assert!(
        (grid_sampled as usize) < grid_cells,
        "the (time x frequency) grid reads sampled everywhere: a sweep visits one window at a time, \
         so this can only be the fold claiming coverage of instants nothing measured ({grid_sampled} \
         of {grid_cells})"
    );

    // 4. The step's own band and interval, not one coarse claim over the pass: the tune history has
    //    a record per step, each with its own centre (T-406's finding, still the provenance).
    let (st, obsv) = get(
        addr,
        &format!("/api/observations?f_lo={LO_HZ}&f_hi={HI_HZ}&t0={t0}&t1={t1}"),
    );
    assert_eq!(st, 200, "{obsv}");
    let mut centres: Vec<f64> = obsv["records"]
        .as_array()
        .map(|rs| {
            rs.iter()
                .filter_map(|r| r["window"]["center_hz"].as_f64())
                .filter(|c| (LO_HZ..=HI_HZ).contains(c))
                .collect()
        })
        .unwrap_or_default();
    centres.sort_by(f64::total_cmp);
    centres.dedup_by(|a, b| (*a - *b).abs() < 1.0);
    assert!(
        centres.len() >= steps as usize,
        "one record per step, each with its own centre: {} distinct centres for {steps} steps \
         ({centres:?})",
        centres.len()
    );

    // ---- 5. grey still means grey: a band the pass never reached is not lit by any of this ----
    let q = format!(
        "/api/coverage?f_lo={UNSWEPT_LO_HZ}&f_hi={UNSWEPT_HI_HZ}&cells={cells}&rows={rows}&t0={t0}&t1={t1}"
    );
    let (st, u) = get(addr, &q);
    assert_eq!(st, 200, "{u}");
    let ub = &u["any"]["bands"];
    assert_eq!(
        ub["observed_cells"].as_u64(),
        Some(0),
        "spectrum the pass never reached must stay unobserved on the collapsed axis too — the \
         collapse widens the *question*, never the claim: {ub}"
    );
    assert_eq!(ub["unobserved_cells"].as_u64(), Some(cells), "{ub}");
    assert_eq!(ub["observed_fraction"].as_f64(), Some(0.0), "{ub}");

    stop_server(serving);
}
