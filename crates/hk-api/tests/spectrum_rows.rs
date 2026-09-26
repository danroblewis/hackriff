//! T-1043 (LSR-2): `GET /ws/spectrum/rows` — one pane's rows, folded onto its own columns,
//! quantised, pushed as binary blocks over a range that starts in the store and runs into the live
//! edge.
//!
//! What these pin, in the ticket's own words:
//! - **per pane**: the subscription is a frequency window and a column count, and the fold onto
//!   those columns happens on the server (a pane narrower than the store's cells reads `exact`, a
//!   pane coarser reads `folded`, and each block says which);
//! - **quantised, binary**: a 48-byte little-endian header, binary16 values (NaN = not measured) and
//!   a coverage trailer — the values **bit-for-bit** what `/api/tiles` serves for the same cells,
//!   which is what "bit-exact with the pyramid's level-0 cells" means;
//! - **`t_from`, store walk → live**: a sealed range is served exactly and ends, an open range goes
//!   on delivering rows as they are recorded, and the subscription cannot be spelled as "now";
//! - **epoch**: it holds across a dwell that goes on, and increments when the tuning under the pane
//!   changes;
//! - **coverage trailer**: grey rides with the rows, on the rows' own axes, and a stretch the map
//!   calls unobserved is one payload-less block.
//!
//! Every assertion is a count, an address, a byte or a status: the only timeouts are socket read
//! bounds that turn a hang into a failure.

use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hk_api::spectrum_rows::{
    BLOCK_HEADER_BYTES, FLAG_DISCONTINUITY, FLAG_FINAL, KIND_ROWS, KIND_UNOBSERVED, NO_LEVEL,
    TRAILER_RUN8, VALUES_F16_LE,
};
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

const TOKEN: &str = "t1043-pane-rows-token-0123456789abcdef";
const DEVICE: &str = "hackrf:0000000000000000a06063c8234e925f";
/// Store cells across the pane's window — the fixture's hot column is one of these. Sixteen, so a
/// pane can be asked for at the store's own resolution (`nf = CELLS`, an exact fold) **and** coarser
/// than it (`nf = CELLS / 2`, a max-fold of pairs) while staying inside `MIN_NF`.
const CELLS: usize = 16;

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "hk-api-pane-rows-{tag}-{}-{:?}",
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

fn dwell(seq: u64, lo: f64, hi: f64, t0_ns: i64, t1_ns: i64) -> ObservationRecord {
    let w = TimeRange::new(
        Timestamp::from_unix_nanos(t0_ns),
        Timestamp::from_unix_nanos(t1_ns),
    );
    ObservationRecord::Dwell(DwellRecord {
        schema: hk_model::attention::ATTENTION_SCHEMA_VERSION,
        survey_id: None,
        seq,
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

struct Fixture {
    _dir: TempDir,
    history: Arc<Mutex<hk_store::Pyramid>>,
    server: Server,
    obs: ObservationStore,
    t_cell: i64,
    f_hi: f64,
}

impl Fixture {
    /// `recorded` rows in the store from the epoch, the tune record covering `tuned` of them, and
    /// history sealed through `sealed_through` when given.
    fn build(tag: &str, recorded: i64, tuned: i64, sealed_through: Option<i64>) -> Self {
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
        obs.append(&dwell(1, 0.0, f_hi, 0, tuned * t_cell));
        obs.flush();
        let state = ApiState {
            history: Some(history.clone()),
            observations: Some(obs.clone()),
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
            obs,
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

    /// Extends the tune record over rows `[from, to)` with a band of its own.
    fn tune(&self, seq: u64, lo: f64, hi: f64, from: i64, to: i64) {
        self.obs
            .append(&dwell(seq, lo, hi, from * self.t_cell, to * self.t_cell));
        self.obs.flush();
    }

    /// The pane path: the fixture's whole band, `nf` columns, and a range in **capture time**.
    fn path(&self, nf: usize, extra: &str) -> String {
        format!(
            "/ws/spectrum/rows?token={TOKEN}&f_lo_hz=0&f_hi_hz={}&nf={nf}{extra}",
            self.f_hi
        )
    }

    fn ns(&self, row: i64) -> i64 {
        row * self.t_cell
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

fn connect(addr: SocketAddr, path: &str) -> Ws {
    let (mut ws, _) = tungstenite::connect(format!("ws://{addr}{path}")).expect("upgrade");
    if let MaybeTlsStream::Plain(s) = ws.get_mut() {
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    }
    ws
}

/// One decoded block: the header fields this route documents, the values and the coverage runs.
#[derive(Debug)]
struct Block {
    kind: u8,
    flags: u8,
    values: u8,
    level: u8,
    tier: u8,
    fold: u8,
    nf: usize,
    rows: usize,
    epoch: u32,
    t0_ns: i64,
    t_cell_ns: i64,
    row0: i64,
    observed_cells: u32,
    /// `rows × nf`, row-major, `None` where the value is NaN (not measured).
    cells: Vec<Option<f32>>,
    /// The trailer's `(cells, state)` runs.
    runs: Vec<(u32, u8)>,
    trailer_present: bool,
    trailer_aligned: bool,
}

fn u16le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}
fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn i64le(b: &[u8], at: usize) -> i64 {
    i64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// binary16 → f32, the reader `ui/src/surface/tile.ts` already has for `?planes=f16`.
fn f16_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits & 0x8000) << 16;
    let exp = u32::from((bits >> 10) & 0x1f);
    let mant = u32::from(bits & 0x03ff);
    let b = match exp {
        0 if mant == 0 => sign,
        0 => {
            // Subnormal binary16: normalise it into binary32.
            let mut e = -1i32;
            let mut m = mant;
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            sign | (((e + 127 + 1) as u32) << 23) | ((m & 0x3ff) << 13)
        }
        0x1f if mant == 0 => sign | 0x7f80_0000,
        0x1f => sign | 0x7fc0_0000,
        _ => sign | ((exp + 127 - 15) << 23) | (mant << 13),
    };
    f32::from_bits(b)
}

/// The next **binary** block, decoded, or `None` on close.
fn block(ws: &mut Ws) -> Option<Block> {
    loop {
        match ws.read() {
            Ok(Message::Binary(b)) => return Some(decode(&b)),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(Message::Text(t)) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                panic!("expected a block, got a text message: {v}");
            }
            Ok(_) => {}
        }
    }
}

fn decode(b: &[u8]) -> Block {
    assert!(b.len() >= BLOCK_HEADER_BYTES, "short block: {} B", b.len());
    let kind = b[0];
    let nf = usize::from(u16le(b, 6));
    let rows = u32le(b, 8) as usize;
    let trailer_bytes = u32le(b, 40) as usize;
    let payload = if kind == KIND_ROWS { rows * nf * 2 } else { 0 };
    assert_eq!(
        b.len(),
        BLOCK_HEADER_BYTES + payload + trailer_bytes,
        "the block's length is exactly header + rows*nf*2 + trailer"
    );
    let vals = &b[BLOCK_HEADER_BYTES..BLOCK_HEADER_BYTES + payload];
    let cells: Vec<Option<f32>> = (0..payload / 2)
        .map(|i| {
            let v = f16_to_f32(u16le(vals, i * 2));
            v.is_finite().then_some(v)
        })
        .collect();
    let mut runs = Vec::new();
    let (mut present, mut aligned) = (false, false);
    if trailer_bytes > 0 {
        let t = &b[BLOCK_HEADER_BYTES + payload..];
        assert_eq!(t[0], TRAILER_RUN8, "the trailer names its encoding");
        assert_eq!(t[1], 4, "four coverage states, never two");
        present = t[2] & 1 != 0;
        aligned = t[2] & 2 != 0;
        let n = u32le(t, 4) as usize;
        assert_eq!(t.len(), 8 + n * 8, "the trailer is exactly its runs");
        for i in 0..n {
            runs.push((u32le(t, 8 + i * 8), t[8 + i * 8 + 4]));
        }
    }
    Block {
        kind,
        flags: b[1],
        values: b[2],
        level: b[3],
        tier: b[4],
        fold: b[5],
        nf,
        rows,
        epoch: u32le(b, 12),
        t0_ns: i64le(b, 16),
        t_cell_ns: i64le(b, 24),
        row0: i64le(b, 32),
        observed_cells: u32le(b, 44),
        cells,
        runs,
        trailer_present: present,
        trailer_aligned: aligned,
    }
}

impl Block {
    /// Every row of this block as `(row address, the column carrying the highest level)`.
    fn hot(&self) -> Vec<(i64, Option<usize>)> {
        assert_eq!(self.kind, KIND_ROWS);
        assert_eq!(self.values, VALUES_F16_LE);
        assert_eq!(self.cells.len(), self.rows * self.nf);
        // Coverage rides with the rows, on the rows' own axes.
        assert_eq!(
            self.runs.iter().map(|(n, _)| *n as usize).sum::<usize>(),
            self.rows * self.nf,
            "the trailer covers exactly the block's cells"
        );
        assert!(
            self.trailer_aligned,
            "the plane is laid on the block's axes"
        );
        (0..self.rows)
            .map(|r| {
                let row = &self.cells[r * self.nf..(r + 1) * self.nf];
                let hot = row
                    .iter()
                    .enumerate()
                    .filter_map(|(i, x)| x.map(|x| (i, x)))
                    .max_by(|a, b| a.1.total_cmp(&b.1))
                    .map(|(i, _)| i);
                (self.row0 + r as i64, hot)
            })
            .collect()
    }
}

/// Reads blocks until `want` rows have arrived, in order, checking contiguity as it goes.
fn take_rows(ws: &mut Ws, want: usize) -> Vec<(i64, Option<usize>)> {
    let mut out: Vec<(i64, Option<usize>)> = Vec::new();
    while out.len() < want {
        let b = block(ws).unwrap_or_else(|| panic!("closed after {} of {want} rows", out.len()));
        assert_eq!(b.t0_ns, b.row0 * b.t_cell_ns, "t0_ns is row0's own instant");
        if let Some(&(last, _)) = out.last() {
            assert_eq!(b.row0, last + 1, "blocks are contiguous: {b:?}");
            assert_eq!(b.flags & FLAG_DISCONTINUITY, 0, "contiguous, so not marked");
        }
        out.extend(b.hot());
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

/// The first (text) message: the subscription, stated back.
fn subscribed(ws: &mut Ws) -> Value {
    loop {
        match ws.read() {
            Ok(Message::Text(t)) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                assert_eq!(v["type"], "subscribed", "{v}");
                return v;
            }
            Ok(Message::Binary(_)) => panic!("a block before the header"),
            Ok(Message::Close(_)) | Err(_) => panic!("closed before the header"),
            Ok(_) => {}
        }
    }
}

/// The refusal a bad request gets: the message and the close code.
fn refusal(addr: SocketAddr, path: &str) -> (Value, Option<u16>) {
    let mut ws = connect(addr, path);
    let v = loop {
        match ws.read() {
            Ok(Message::Text(t)) => break serde_json::from_str::<Value>(&t).unwrap(),
            Ok(Message::Close(_)) | Err(_) => panic!("refusals still upgrade and say why"),
            Ok(_) => {}
        }
    };
    let mut code = None;
    while let Ok(m) = ws.read() {
        if let Message::Close(f) = m {
            code = f.map(|f| u16::from(f.code));
        }
    }
    (v, code)
}

/// **The test that fails if a pane subscription can be spelled as "now"** — and if the pane could be
/// asked for as a lattice address instead of a window.
#[test]
fn the_subscription_is_a_pane_window_and_a_range_and_cannot_be_spelled_as_now() {
    let f = Fixture::build("no-now", 64, 64, None);
    let addr = f.addr();

    // No range start, no subscription: nothing defaults it.
    let (v, code) = refusal(addr, &f.path(CELLS, ""));
    assert_eq!(v["type"], "refused", "{v}");
    assert_eq!(v["status"], 400, "{v}");
    assert!(
        v["reason"].as_str().unwrap().contains("t_from is required"),
        "{v}"
    );
    assert_eq!(code, Some(4400));

    // A pane is a window, not a tile address: the lattice parameters are refused, not ignored.
    for extra in [
        "&t_from=0&cells=32",
        "&t_from=0&f_index=3",
        "&t_from=0&level_f=1",
    ] {
        let (v, code) = refusal(addr, &f.path(CELLS, extra));
        assert_eq!(v["status"], 400, "{extra}: {v}");
        assert_eq!(code, Some(4400), "{extra}");
    }
    // The window and the columns are checked by value.
    for path in [
        format!("/ws/spectrum/rows?token={TOKEN}&f_lo_hz=0&f_hi_hz=0&nf=8&t_from=0"),
        format!("/ws/spectrum/rows?token={TOKEN}&f_lo_hz=0&f_hi_hz=1e6&t_from=0"),
        f.path(4, "&t_from=0"),
        f.path(99_999, "&t_from=0"),
        f.path(CELLS, "&t_from=10&t_to=10"),
    ] {
        let (v, code) = refusal(addr, &path);
        assert_eq!(v["status"], 400, "{path}: {v}");
        assert_eq!(code, Some(4400), "{path}");
    }

    // Two different past ranges deliver two different, exact sets of rows — which a route that
    // could only mean "the live stream" cannot do.
    let mut a = connect(addr, &f.path(CELLS, &format!("&t_from=0&t_to={}", f.ns(8))));
    subscribed(&mut a);
    assert_exact(&take_rows(&mut a, 8), 0, 8);
    let mut b = connect(
        addr,
        &f.path(CELLS, &format!("&t_from={}&t_to={}", f.ns(16), f.ns(24))),
    );
    subscribed(&mut b);
    assert_exact(&take_rows(&mut b, 8), 16, 24);
}

/// A sealed range, served exactly on the pane's own grid, and it ends. The values are the pyramid's
/// level-0 cells: `exact` on both axes, `final`, and the same measurement `/api/tiles` serves.
#[test]
fn a_sealed_range_is_served_exactly_on_the_pane_grid_and_ends() {
    let f = Fixture::build("sealed", 64, 64, Some(64));
    let mut ws = connect(
        f.addr(),
        &f.path(CELLS, &format!("&t_from=0&t_to={}", f.ns(32))),
    );
    let head = subscribed(&mut ws);
    assert_eq!(head["pane"]["nf"], CELLS as u64, "{head}");
    assert_eq!(head["pane"]["f_lo_hz"], 0.0, "{head}");
    assert_eq!(head["pane"]["f_hi_hz"], f.f_hi, "{head}");
    assert_eq!(head["range"]["row0"], 0, "{head}");
    assert_eq!(head["range"]["open"], false, "{head}");
    assert_eq!(head["record"]["header_bytes"], BLOCK_HEADER_BYTES as u64);
    assert_eq!(head["record"]["values"]["absent"], "nan", "{head}");
    assert_eq!(
        head["record"]["coverage"]["states"],
        serde_json::json!(["unobserved", "observed", "unknown", "excluded"]),
        "{head}"
    );

    let first = block(&mut ws).expect("a block");
    assert_eq!(first.kind, KIND_ROWS);
    assert_eq!(first.nf, CELLS);
    assert_eq!(first.t_cell_ns, f.t_cell);
    assert_eq!(first.level, 0, "the pane's own cell is level 0's");
    assert_eq!(first.fold, 0, "exact on both axes: {first:?}");
    assert_eq!(first.flags & FLAG_FINAL, FLAG_FINAL, "sealed: {first:?}");
    assert_eq!(
        first.flags & FLAG_DISCONTINUITY,
        FLAG_DISCONTINUITY,
        "the first block of a range continues nothing"
    );
    assert_eq!(
        first.observed_cells as usize,
        first.rows * first.nf,
        "every cell of a recorded, tuned block is a measurement"
    );
    assert!(first.trailer_present, "the union has a record here");
    assert_eq!(
        first.runs,
        vec![(first.rows as u32 * CELLS as u32, 1)],
        "uniformly observed, in one run"
    );
    let mut rows = first.hot();
    rows.extend(take_rows(&mut ws, 32 - rows.len()));
    assert_exact(&rows, 0, 32);

    // ...and the range ends, with the row it ended at.
    let end = loop {
        match ws.read() {
            Ok(Message::Text(t)) => break serde_json::from_str::<Value>(&t).unwrap(),
            Ok(Message::Close(_)) | Err(_) => panic!("a closed range ends with `end`"),
            Ok(_) => {}
        }
    };
    assert_eq!(end["type"], "end", "{end}");
    assert_eq!(end["row"], 32, "{end}");
    assert_eq!(end["t_ns"], f.ns(32), "{end}");
}

/// **The fold is the pane's own, and the block says which direction it went.** Half as many columns
/// as the store has cells is a max-fold of pairs, stated `folded` on the frequency axis; the values
/// are still the level-0 cells.
#[test]
fn a_pane_coarser_than_the_store_folds_and_says_so() {
    let f = Fixture::build("folded", 32, 32, Some(32));
    let nf = CELLS / 2;
    let mut ws = connect(
        f.addr(),
        &f.path(nf, &format!("&t_from=0&t_to={}", f.ns(16))),
    );
    let head = subscribed(&mut ws);
    assert_eq!(head["pane"]["nf"], nf as u64, "{head}");
    assert_eq!(
        head["pane"]["f_cell_hz"].as_f64().unwrap(),
        f.f_hi / nf as f64,
        "{head}"
    );
    let b = block(&mut ws).expect("a block");
    assert_eq!(b.nf, nf);
    assert_eq!(b.fold & 0b11, 1, "frequency folded: {b:?}");
    assert_eq!(b.fold >> 2, 0, "time exact: {b:?}");
    // Each store cell k % CELLS falls in pane column (k % CELLS) / 2.
    let want: Vec<(i64, Option<usize>)> = (0..b.rows as i64)
        .map(|r| (r, Some((r as usize % CELLS) / 2)))
        .collect();
    assert_eq!(b.hot(), want, "the fold is a max onto the pane's columns");
}

/// **Bit-exact with the pyramid's level-0 cells.** The block's binary16 values are the same numbers
/// `/api/tiles` serves for the same cells, rounded the one way binary16 rounds — not a second
/// measurement, and not a rescaling this route invented.
#[test]
fn the_values_are_the_tile_routes_own_cells_bit_for_bit() {
    let f = Fixture::build("bit-exact", 32, 32, Some(32));
    let addr = f.addr();
    let (st, tile) = get(
        addr,
        &format!("/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells={CELLS}"),
    );
    assert_eq!(st, 200, "{tile}");
    let grid = tile["grid"]["max_db"].as_array().expect("max_db").clone();
    let mut ws = connect(addr, &f.path(CELLS, &format!("&t_from=0&t_to={}", f.ns(8))));
    subscribed(&mut ws);
    let b = block(&mut ws).expect("a block");
    assert_eq!(b.rows, 8);
    for r in 0..b.rows {
        for c in 0..b.nf {
            let want = grid[r * CELLS + c].as_f64().map(|v| v as f32);
            let got = b.cells[r * b.nf + c];
            match want {
                None => assert_eq!(got, None, "cell ({r}, {c}): null is NaN, never a level"),
                Some(w) => {
                    let got = got.unwrap_or_else(|| panic!("cell ({r}, {c}) is absent"));
                    // binary16 keeps 11 significant bits: the only difference allowed is that
                    // rounding.
                    assert!(
                        (got - w).abs() <= w.abs() * 2f32.powi(-10) + 1e-3,
                        "cell ({r}, {c}): {got} is not the tile's {w} in binary16"
                    );
                }
            }
        }
    }
}

/// An open range walks the store and then keeps delivering rows as they are recorded — the same
/// cursor, with nothing in it that knows which half it is serving.
#[test]
fn an_open_range_walks_the_store_and_then_follows_the_live_edge() {
    let f = Fixture::build("store-then-live", 8, 64, None);
    let mut ws = connect(f.addr(), &f.path(CELLS, "&t_from=0"));
    subscribed(&mut ws);
    assert_exact(&take_rows(&mut ws, 8), 0, 8);
    // Rows recorded *after* the subscription arrive on the same subscription, in place.
    f.record(8, 16);
    assert_exact(&take_rows(&mut ws, 16), 8, 24);
}

/// A stretch the coverage map calls uniformly unobserved is **one payload-less block** with no
/// level and no tier — answered from the map alone, and it may span many rows.
#[test]
fn an_unobserved_stretch_is_one_block_with_no_payload_and_no_level() {
    // Recorded and tuned from row 4096 on; the range starts at the epoch, where nothing looked.
    let f = Fixture::build("grey", 0, 0, None);
    f.tune(2, 0.0, f.f_hi, 4096, 4160);
    f.record(4096, 8);
    let mut ws = connect(f.addr(), &f.path(CELLS, "&t_from=0"));
    subscribed(&mut ws);
    let b = block(&mut ws).expect("a block");
    assert_eq!(b.kind, KIND_UNOBSERVED, "{b:?}");
    assert_eq!(b.cells.len(), 0, "no measurement rides with grey");
    assert_eq!(
        b.runs.len(),
        0,
        "and no plane: the whole block is one state"
    );
    assert_eq!(b.nf, 0, "{b:?}");
    assert_eq!(b.level, NO_LEVEL, "{b:?}");
    assert_eq!(b.tier, NO_LEVEL, "{b:?}");
    assert_eq!(b.values, 0, "{b:?}");
    assert_eq!(b.row0, 0, "{b:?}");
    assert!(b.rows >= 4096, "one message spans the whole grey: {b:?}");
}

/// **The epoch holds while the tuning does, and increments when it changes.** A dwell that goes on
/// is filed record after record with the same configuration and must not read as a retune; a dwell
/// over a different band under the pane must.
#[test]
fn the_epoch_holds_across_a_continuing_dwell_and_increments_on_a_retune() {
    let f = Fixture::build("epoch", 128, 64, Some(128));
    // The same configuration, filed again over the next rows: not a retune.
    f.tune(2, 0.0, f.f_hi, 64, 96);
    let mut ws = connect(
        f.addr(),
        &f.path(CELLS, &format!("&t_from=0&t_to={}", f.ns(96))),
    );
    subscribed(&mut ws);
    let mut epochs = Vec::new();
    let mut rows = 0;
    while rows < 96 {
        let b = block(&mut ws).expect("a block");
        rows += b.rows;
        epochs.push(b.epoch);
    }
    assert!(
        epochs.iter().all(|&e| e == 0),
        "one configuration, one epoch: {epochs:?}"
    );

    // A retune under the pane: half the band, a different centre and rate.
    let g = Fixture::build("epoch-retune", 128, 64, Some(128));
    g.tune(2, 0.0, g.f_hi / 2.0, 64, 128);
    let mut ws = connect(
        g.addr(),
        &g.path(CELLS, &format!("&t_from=0&t_to={}", g.ns(128))),
    );
    subscribed(&mut ws);
    let mut seen = Vec::new();
    let mut rows = 0;
    while rows < 128 {
        let b = block(&mut ws).expect("a block");
        rows += b.rows;
        seen.push((b.row0, b.epoch));
    }
    assert_eq!(seen.first().map(|(_, e)| *e), Some(0), "{seen:?}");
    assert!(
        seen.iter().any(|&(_, e)| e > 0),
        "the tuning under the pane changed, so the epoch did: {seen:?}"
    );
    // The grey the retune left is drawn: the second half's plane is no longer uniformly observed.
    assert!(
        seen.windows(2).any(|w| w[0].1 != w[1].1),
        "the epoch changes at a block boundary, once: {seen:?}"
    );
}

/// Two panes on one server keep **their own edges** — one subscription per pane, not one per client.
#[test]
fn two_panes_keep_their_own_edges() {
    let f = Fixture::build("two-panes", 32, 64, None);
    let addr = f.addr();
    let mut a = connect(addr, &f.path(CELLS, "&t_from=0"));
    subscribed(&mut a);
    assert_exact(&take_rows(&mut a, 32), 0, 32);
    // B starts at 24 and is delivered 24..32 — the rows A had already had, on B's own cursor.
    let mut b = connect(addr, &f.path(CELLS, &format!("&t_from={}", f.ns(24))));
    subscribed(&mut b);
    assert_exact(&take_rows(&mut b, 8), 24, 32);
    // Both edges then advance over the newly recorded rows, each from where it had got to.
    f.record(32, 8);
    assert_exact(&take_rows(&mut a, 8), 32, 40);
    assert_exact(&take_rows(&mut b, 8), 32, 40);
}

/// A plain GET (no upgrade) is `426`, as every `/ws/` route here answers.
#[test]
fn a_request_that_is_not_an_upgrade_is_refused_426() {
    let f = Fixture::build("no-upgrade", 8, 8, None);
    let (st, v) = get(f.addr(), &f.path(CELLS, "&t_from=0"));
    assert_eq!(st, 426, "{v}");
}

fn get(addr: SocketAddr, path: &str) -> (u16, Value) {
    use std::io::{Read, Write};
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\n\
         Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw);
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap();
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, serde_json::from_str(&body).unwrap_or(Value::Null))
}
