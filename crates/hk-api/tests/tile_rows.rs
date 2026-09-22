//! T-468: `GET /ws/tiles/rows` — rows pushed to a subscription over an **address range**.
//!
//! What these pin, in the ticket's own words:
//! - rows are pushed **by address range**, and every row arrives at the address it was recorded at
//!   (each fixture row carries its own index in which column is hot, so a row delivered at the wrong
//!   address, twice, or not at all fails a count, never a clock);
//! - the route serves a **sealed-history range as readily as the growing one** — through the same
//!   cursor, with nothing that knows which it is;
//! - two subscriptions on one server keep **their own edges** (one per pane, not one per client);
//! - and the subscription **cannot be expressed as "now"**: without `t_from` it is refused, and two
//!   different past ranges deliver two different, exact sets of rows — which a route that could only
//!   mean "the live stream" cannot do.
//!
//! Every assertion is a count, an address or a status (the user's standing rule, 2026-09-21): the
//! only timeouts are socket read bounds that turn a hang into a failure.

use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hk_api::{ApiState, Server, ServerConfig, Token};
use hk_model::attention::baseline::SiteKey;
use hk_model::attention::observation::{
    DwellRecord, ObservationRecord, ObservedWindow, Reason, Tier,
};
use hk_model::{FreqRange, PowerUnit, TimeRange, Timestamp};
use hk_store::history::{FrameInput, PyramidConfig};
use hk_store::observation::{ObservationLogConfig, ObservationStore};
use serde_json::Value;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

const TOKEN: &str = "t468-row-push-token-0123456789abcdef0";
const DEVICE: &str = "hackrf:0000000000000000a06063c8234e925f";
const CELLS: usize = 8;
/// Tiles of recorded rows the observation log says were tuned (the dwell's extent).
const DWELL_ROWS: i64 = 64;

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "hk-api-tile-rows-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dwell(lo: f64, hi: f64, t0_ns: i64, t1_ns: i64) -> ObservationRecord {
    let w = TimeRange::new(
        Timestamp::from_unix_nanos(t0_ns),
        Timestamp::from_unix_nanos(t1_ns),
    );
    ObservationRecord::Dwell(DwellRecord {
        schema: hk_model::attention::ATTENTION_SCHEMA_VERSION,
        survey_id: None,
        seq: 1,
        plan_version: 1,
        site: SiteKey::Unassigned,
        device_id: Some(DEVICE.to_string()),
        reason: Reason::RegionDwell { hop: 0 },
        tier: Tier::ScheduledPlan,
        window: ObservedWindow {
            center_hz: (lo + hi) / 2.0,
            sample_rate_hz: hi - lo,
            usable: FreqRange::new(lo, hi),
            dc_excluded: None,
            rbw_hz: 1e3,
        },
        rf_path: 0,
        planned: w,
        observed: w,
        preempted: false,
        dropped_samples: 0,
        overload: false,
        provenance_ref: None,
    })
}

/// A pyramid whose row `k` has exactly one hot column, `k mod CELLS`, over tile column
/// `f_index = 0` at level 0 — so every delivered row names the address it was recorded at.
struct Fixture {
    _dir: TempDir,
    history: Arc<Mutex<hk_store::Pyramid>>,
    server: Server,
    t_cell: i64,
    f_hi: f64,
}

impl Fixture {
    fn build(tag: &str, recorded: i64, sealed_through: Option<i64>) -> Self {
        let dir = TempDir::new(tag);
        let p = hk_store::Pyramid::open(dir.0.join("history"), PyramidConfig::default()).unwrap();
        let g = p.geometry().clone();
        let t_cell = g.levels[0].t_cell_ns;
        let f_cell = g.levels[0].f_cell_hz;
        let f_hi = f_cell * CELLS as f64;
        let history = Arc::new(Mutex::new(p));
        record(&history, t_cell, f_hi, 0, recorded);
        if let Some(r) = sealed_through {
            history
                .lock()
                .unwrap()
                .seal_through(Timestamp::from_unix_nanos(r * t_cell))
                .unwrap();
        }
        let obs =
            ObservationStore::open(ObservationLogConfig::new(dir.0.join("observations"))).unwrap();
        obs.append(&dwell(0.0, f_hi, 0, DWELL_ROWS * t_cell));
        obs.flush();
        let state = ApiState {
            history: Some(history.clone()),
            observations: Some(obs),
            ..ApiState::default()
        };
        let config = ServerConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            Token::from_config(TOKEN).unwrap(),
        );
        Fixture {
            _dir: dir,
            history,
            server: Server::start(config, state).unwrap(),
            t_cell,
            f_hi,
        }
    }

    fn addr(&self) -> SocketAddr {
        self.server.local_addr()
    }

    fn record(&self, from: i64, n: i64) {
        record(&self.history, self.t_cell, self.f_hi, from, n);
    }
}

fn record(history: &Arc<Mutex<hk_store::Pyramid>>, t_cell: i64, f_hi: f64, from: i64, n: i64) {
    let bin_hz = f_hi / CELLS as f64;
    let mut p = history.lock().unwrap();
    for k in from..from + n {
        let mut psd = [1e-9f32; CELLS];
        psd[(k as usize) % CELLS] = 1e-5;
        p.ingest(&FrameInput::new(
            Timestamp::from_unix_nanos(k * t_cell),
            t_cell,
            0.0,
            bin_hz,
            PowerUnit::Dbfs,
            &psd,
        ))
        .unwrap();
    }
}

fn path(extra: &str) -> String {
    format!("/ws/tiles/rows?token={TOKEN}&level_f=0&level_t=0&f_index=0&cells={CELLS}{extra}")
}

fn connect(addr: SocketAddr, path: &str) -> Ws {
    let (mut ws, _) = tungstenite::connect(format!("ws://{addr}{path}")).expect("upgrade");
    if let MaybeTlsStream::Plain(s) = ws.get_mut() {
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    }
    ws
}

/// The next text message as JSON, or `None` on close / error.
fn next(ws: &mut Ws) -> Option<Value> {
    loop {
        match ws.read() {
            Ok(Message::Text(t)) => return Some(serde_json::from_str(&t).unwrap()),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

/// Every row a `rows` message carries, as `(row address, hot column)`.
fn rows_of(v: &Value) -> Vec<(i64, Option<usize>)> {
    assert_eq!(v["type"], "rows", "{v}");
    let row0 = v["row0"].as_i64().unwrap();
    let n = v["rows"].as_i64().unwrap();
    let nf = v["nf"].as_u64().unwrap() as usize;
    let db = v["max_db"].as_array().unwrap();
    assert_eq!(db.len(), n as usize * nf, "max_db is rows x nf");
    // A block never crosses a tile, and says which tile and which row of it.
    assert_eq!(
        v["tile"]["t_index"].as_i64().unwrap(),
        row0.div_euclid(nf as i64)
    );
    assert_eq!(
        v["tile"]["row"].as_i64().unwrap(),
        row0.rem_euclid(nf as i64)
    );
    assert!(
        row0.rem_euclid(nf as i64) + n <= nf as i64,
        "block crosses a tile: {v}"
    );
    // Coverage rides with the rows, on the rows' own axes.
    assert_eq!(v["coverage"]["nt"].as_i64().unwrap(), n);
    assert_eq!(v["coverage"]["aligned"], true);
    (0..n)
        .map(|r| {
            let row = &db[r as usize * nf..(r as usize + 1) * nf];
            let hot = row
                .iter()
                .enumerate()
                .filter_map(|(i, x)| x.as_f64().map(|x| (i, x)))
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(i, _)| i);
            (row0 + r, hot)
        })
        .collect()
}

/// Reads `rows` messages until `want` rows have arrived; returns them in order.
fn take_rows(ws: &mut Ws, want: usize) -> Vec<(i64, Option<usize>)> {
    let mut out = Vec::new();
    while out.len() < want {
        let v = next(ws).unwrap_or_else(|| panic!("closed after {} of {want} rows", out.len()));
        out.extend(rows_of(&v));
    }
    out
}

fn assert_exact(rows: &[(i64, Option<usize>)], from: i64, to: i64) {
    let want: Vec<(i64, Option<usize>)> = (from..to)
        .map(|r| (r, Some((r as usize) % CELLS)))
        .collect();
    assert_eq!(
        rows,
        want.as_slice(),
        "rows [{from}, {to}) exactly, each at its own address"
    );
}

fn subscribed(ws: &mut Ws) -> Value {
    let v = next(ws).expect("subscribed");
    assert_eq!(v["type"], "subscribed", "{v}");
    v
}

/// **The test that fails if the subscription is expressible only as "now".** No `t_from`, no
/// subscription — there is no default anchor for a range to fall back on — and two different past
/// ranges on the same server deliver two different, exact sets of rows.
#[test]
fn the_subscription_is_an_address_range_and_cannot_be_spelled_as_now() {
    let fx = Fixture::build("now", 24, Some(24));

    for bad in [
        path(""),
        path("&t_to=8"),
        path("&t_index=1"),
        path("&t_from=9&t_to=9"),
    ] {
        let mut ws = connect(fx.addr(), &bad);
        let v = next(&mut ws).expect("a refusal message");
        assert_eq!(v["type"], "refused", "{bad}: {v}");
        assert_eq!(v["status"], 400, "{bad}: {v}");
    }

    for (from, to) in [(2, 7), (13, 22)] {
        let mut ws = connect(fx.addr(), &path(&format!("&t_from={from}&t_to={to}")));
        let s = subscribed(&mut ws);
        assert_eq!(s["range"]["t_from"], from);
        assert_eq!(s["range"]["t_to"], to);
        assert_eq!(s["range"]["open"], false);
        let rows = take_rows(&mut ws, (to - from) as usize);
        assert_exact(&rows, from, to);
        let end = next(&mut ws).expect("end");
        assert_eq!(end["type"], "end", "{end}");
        assert_eq!(end["row"], to);
    }
}

/// **A sealed range is served as readily as the growing one.** Everything here is behind the
/// watermark, so every block is `final`; the range spans three tiles, and each block patches one.
#[test]
fn a_sealed_history_range_is_served_exactly_and_ends() {
    let fx = Fixture::build("sealed", 32, Some(32));
    let mut ws = connect(fx.addr(), &path("&t_from=3&t_to=21"));
    subscribed(&mut ws);
    let mut rows = Vec::new();
    let mut blocks = 0;
    while rows.len() < 18 {
        let v = next(&mut ws).expect("rows");
        assert_eq!(v["final"], true, "a row behind the watermark is final: {v}");
        rows.extend(rows_of(&v));
        blocks += 1;
    }
    assert_exact(&rows, 3, 21);
    assert_eq!(
        blocks, 3,
        "[3,8) [8,16) [16,21): one block per tile it touches"
    );
    assert_eq!(next(&mut ws).expect("end")["type"], "end");
}

/// **A range past the data edge delivers what exists, then pushes the rest as it is recorded** —
/// the same cursor, no mode switch. Rows are pushed once each, never repeated, never skipped.
#[test]
fn a_range_past_the_edge_pushes_rows_as_they_are_recorded() {
    let fx = Fixture::build("growing", 10, None);
    let mut ws = connect(fx.addr(), &path("&t_from=6&t_to=20"));
    let s = subscribed(&mut ws);
    assert!(s["data_edge_s"].as_f64().is_some());
    // Rows 6..10 exist and arrive without anything else happening.
    let first = take_rows(&mut ws, 4);
    assert_exact(&first, 6, 10);
    // Now record, a few rows at a time, and each arrives.
    let mut rows = first;
    for (from, n) in [(10, 3), (13, 1), (14, 6)] {
        fx.record(from, n);
        rows.extend(take_rows(&mut ws, n as usize));
    }
    assert_exact(&rows, 6, 20);
    let end = next(&mut ws).expect("end");
    assert_eq!(end["type"], "end", "{end}");
}

/// **One edge per subscription, not one per client.** Two subscriptions on one server — a reader
/// walking a sealed stretch and one following the growing edge with an open range — progress
/// independently: the historical one finishes while the live one waits, and the live one then
/// receives exactly the rows recorded after it, nothing of the other's range.
#[test]
fn two_subscriptions_keep_their_own_edges() {
    let fx = Fixture::build("two", 16, Some(8));
    let mut live = connect(fx.addr(), &path("&t_from=12"));
    let s = subscribed(&mut live);
    assert_eq!(s["range"]["open"], true);
    assert!(s["range"]["t_to"].is_null());
    assert_exact(&take_rows(&mut live, 4), 12, 16);

    let mut past = connect(fx.addr(), &path("&t_from=1&t_to=5"));
    subscribed(&mut past);
    assert_exact(&take_rows(&mut past, 4), 1, 5);
    assert_eq!(next(&mut past).expect("end")["type"], "end");

    fx.record(16, 5);
    assert_exact(&take_rows(&mut live, 5), 16, 21);
}

/// **A stretch the coverage map calls unobserved is one message, and carries no level.** The band
/// at `f_index = 5` was never tuned; a range over it is answered from the map alone.
#[test]
fn an_unobserved_stretch_is_one_message_with_no_measurement() {
    let fx = Fixture::build("grey", 32, Some(32));
    let mut ws = connect(
        fx.addr(),
        &format!(
            "/ws/tiles/rows?token={TOKEN}&level_f=0&level_t=0&f_index=5&cells={CELLS}\
             &t_from=0&t_to=30"
        ),
    );
    subscribed(&mut ws);
    let v = next(&mut ws).expect("unobserved");
    assert_eq!(v["type"], "unobserved", "{v}");
    assert_eq!(v["row0"], 0);
    assert_eq!(v["rows"], 30);
    assert!(v.get("max_db").is_none(), "grey carries no level: {v}");
    assert_eq!(next(&mut ws).expect("end")["type"], "end");
}
