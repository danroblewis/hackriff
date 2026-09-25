//! **T-901: `/ws/tiles/rows` pushes a live subscription's rows at the recording cadence.**
//!
//! THIS IS A `timing`-TIER TEST (docs/10 §3.6; `.config/nextest.toml`'s `default-filter` keeps it
//! out of every gate run and `just timing` runs it alone, on a quiet box or nightly). Its
//! assertion is a latency bound, which measures the machine's headroom as much as the code. The
//! deterministic halves of the same claim ARE in the gate: `hk-store`'s
//! `history::tests::deferred` pins that a deferred seal creates no file and folds no backlog of
//! rows inside `ingest`, i.e. that nothing a row push waits on can hold the view lock across a
//! seal.
//!
//! # What it measured before the fix
//!
//! The view writer held the view pyramid's mutex across every seal, 1.0-4.5 s once per 64-row
//! level-0 block on the dev box, and every step of a row subscription takes that mutex. Over the
//! same fixture and one subscription, the pushes then stalled for **1.2-4.3 s roughly every 64
//! rows** and delivered the rows in 45-64-row bursts afterwards (T-893's finding). After the fix,
//! on the same box at load ~44: 400 rows, max gap 0.80 s, p99 0.43 s, blocks of 1-7 rows.
//!
//! # Why the bound is not tighter
//!
//! A row is pushed only once the **tune record** reaches it (`rows.rs`, `record_reach`: its
//! coverage must be final when it goes), and with the ring's record the live edge trails the
//! spectrum by up to a control tick, measured at p50 0.16 s and max 0.50 s. That is a design
//! choice (T-468 review / T-596), not this defect, so the bound leaves room for it and for
//! scheduling noise, and still sits under the old stall.
//!
//! Through the mock SDR device, as the e2e rule requires: `hk serve` over `mock:<fixture>`.

use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use hk_cli::pipeline::{IqBufferArgs, LiveArgs, TempDataDirGuard, temp_data_dir};
use hk_cli::serve::{ServeOptions, ServeSource, Serving, start};
use serde_json::Value;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

const TOKEN: &str = "t901-row-push-cadence-0123456789abcdef";

/// Rows measured, after the warm-up. The ticket asks for >= 200.
const ROWS: i64 = 240;
/// Rows discarded first: the subscription's catch-up burst and the pipeline's start.
const WARMUP_ROWS: i64 = 25;
/// No gap between two pushes may exceed this. The defect's stall was 1.2-4.3 s.
const MAX_GAP: Duration = Duration::from_millis(1000);
/// ...and 99 % of gaps stay under this. The defect's p99 was the stall itself.
const P99_GAP: Duration = Duration::from_millis(600);

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/hackrf/2026-09-13/fm_100p8M_2p4M_l32g30a1_t1p5_5s.sigmf-meta")
}

fn connect(addr: SocketAddr, query: &str) -> Ws {
    let (ws, _) = tungstenite::connect(format!("ws://{addr}/ws/tiles/rows?{query}&token={TOKEN}"))
        .expect("row subscription handshake");
    if let MaybeTlsStream::Plain(s) = ws.get_ref() {
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    }
    ws
}

fn next(ws: &mut Ws) -> Value {
    loop {
        match ws
            .read()
            .expect("the row subscription went silent for 20 s")
        {
            Message::Text(t) => return serde_json::from_str(&t).unwrap(),
            Message::Close(c) => panic!("the server closed the subscription: {c:?}"),
            _ => {}
        }
    }
}

fn stop(serving: Serving) {
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

#[test]
fn live_row_pushes_arrive_at_the_recording_cadence_without_multi_second_stalls() {
    let dir = temp_data_dir();
    let _guard = TempDataDirGuard::new(dir.clone());
    let serving = start(&ServeOptions {
        source: ServeSource::HackRf {
            spec: format!("mock:{}", fixture_path().display()),
            live: LiveArgs::default(),
            extra: Vec::new(),
        },
        data_dir: Some(dir.clone()),
        bind: "127.0.0.1:0".parse().unwrap(),
        ui_dist: None,
        // `hk serve`'s own defaults: the display plan whose rows node (0, 0) of the view lattice
        // is sized to (T-484), 40 ms rows.
        fft_len: 4096,
        rows_per_s: 25.0,
        calibration: None,
        token: Some(TOKEN.into()),
        listen: Default::default(),
        compute: Default::default(),
        // A small ring: the tune record reads it, and nothing here needs a long one on disk.
        iq_buffer: IqBufferArgs {
            retention_s: Some(20.0),
            max_bytes: None,
        },
        iq_buffer_hooks: None,
    })
    .unwrap();
    let addr = serving.server.local_addr();

    // The column holding the fixture's centre, found from the lattice the route states back.
    let mut probe = connect(addr, "level_f=0&level_t=0&f_index=0&cells=256&t_from=0");
    let sub = next(&mut probe);
    assert_eq!(sub["type"], "subscribed", "{sub}");
    drop(probe);
    let f_cell = sub["extent"]["f_cell_hz"].as_f64().unwrap();
    let t_cell = sub["extent"]["t_cell_s"].as_f64().unwrap();
    let f_index = (100.8e6 / (f_cell * 256.0)).floor() as i64;

    // Wait for the data edge, then subscribe open-ended from it: the live case.
    let t0 = Instant::now();
    let edge = loop {
        let mut ws = connect(
            addr,
            &format!("level_f=0&level_t=0&f_index={f_index}&cells=256&t_from=0"),
        );
        let s = next(&mut ws);
        if let Some(e) = s["data_edge_s"].as_f64() {
            break e;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(30),
            "no data edge after 30 s: {s}"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    let from = (edge / t_cell).floor() as i64;
    let mut ws = connect(
        addr,
        &format!("level_f=0&level_t=0&f_index={f_index}&cells=256&t_from={from}"),
    );
    assert_eq!(next(&mut ws)["type"], "subscribed");

    let (mut rows, mut last): (i64, Option<Instant>) = (0, None);
    let mut gaps: Vec<(Duration, i64)> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    while rows < WARMUP_ROWS + ROWS {
        assert!(Instant::now() < deadline, "only {rows} rows in 120 s");
        let v = next(&mut ws);
        let n = match v["type"].as_str() {
            Some("rows") | Some("unobserved") => v["rows"].as_i64().unwrap(),
            _ => continue,
        };
        let now = Instant::now();
        if rows >= WARMUP_ROWS
            && let Some(l) = last
        {
            gaps.push((now - l, n));
        }
        last = Some(now);
        rows += n;
    }
    drop(ws);
    stop(serving);

    let mut sorted: Vec<Duration> = gaps.iter().map(|g| g.0).collect();
    sorted.sort();
    let max = *sorted.last().unwrap();
    let p99 = sorted[(sorted.len() * 99 / 100).min(sorted.len() - 1)];
    let p50 = sorted[sorted.len() / 2];
    let worst: Vec<String> = {
        let mut g = gaps.clone();
        g.sort_by(|a, b| b.0.cmp(&a.0));
        g.iter()
            .take(5)
            .map(|(d, n)| format!("{:.2} s then {n} rows", d.as_secs_f64()))
            .collect()
    };
    eprintln!(
        "T-901 row push cadence over {ROWS} rows / {} pushes: gap p50 {:.3} s, p99 {:.3} s, \
         max {:.3} s; worst: {worst:?}",
        gaps.len(),
        p50.as_secs_f64(),
        p99.as_secs_f64(),
        max.as_secs_f64()
    );
    assert!(
        max <= MAX_GAP && p99 <= P99_GAP,
        "row pushes stalled: max gap {:.2} s (bound {:.2}), p99 {:.2} s (bound {:.2}); worst \
         {worst:?}. A stall once per 64-row block is the view writer holding the view lock \
         across a seal (T-901).",
        max.as_secs_f64(),
        MAX_GAP.as_secs_f64(),
        p99.as_secs_f64(),
        P99_GAP.as_secs_f64()
    );
}
