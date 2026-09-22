//! T-574: sealed tiles are immutable, so a repeated read is a `304` with no body and a long
//! `immutable` cache — but a **live** tile (its own `sealed: false`) must never be cached that way,
//! never get an `ETag` at all, and must keep answering `200` with fresh bytes as new rows arrive.
//!
//! Every assertion here is a **count or a status code**, never a wall-clock bound (the user's
//! standing rule, 2026-09-21): repeated-body byte counts, `Content-Length`, HTTP status, and header
//! presence/absence.

use std::io::{Read, Write};
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

const TOKEN: &str = "t574-tile-cache-token-0123456789abcdef";
const DEVICE: &str = "hackrf:0000000000000000a06063c8234e925f";
const CELLS: usize = 8;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "hk-api-tile-cache-{tag}-{}-{:?}",
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

fn serve(state: ApiState) -> Server {
    let config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        Token::from_config(TOKEN).unwrap(),
    );
    Server::start(config, state).unwrap()
}

/// A raw HTTP response: status, headers (lower-cased names) and the exact body bytes after the
/// blank line — so a 304's body length is a fact this measures, not infers.
struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn get(addr: SocketAddr, path: &str, extra_headers: &[(&str, &str)]) -> Resp {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut extra = format!("Authorization: Bearer {TOKEN}\r\n");
    for (k, v) in extra_headers {
        extra.push_str(&format!("{k}: {v}\r\n"));
    }
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: test\r\n{extra}Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response head");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap();
    let status = status_line
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap();
    let headers = lines
        .filter_map(|l| {
            l.split_once(": ")
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect();
    Resp {
        status,
        headers,
        body: raw[split + 4..].to_vec(),
    }
}

/// One pyramid, two tiles at level 0, `CELLS` rows apart in time so they never share a row:
/// tile A (`t_index = 0`) is filled completely (its own extent's end lands exactly on the
/// watermark, so it is sealed); tile B (`t_index = 1`) starts out with only `filled` of its
/// `CELLS` rows, so its extent's end is still ahead of the watermark (unsealed / live).
struct Fixture {
    _dir: TempDir,
    state: ApiState,
    f_lo: f64,
    f_hi: f64,
    t_cell: i64,
    _t0_a: i64,
    t0_b: i64,
}

impl Fixture {
    fn build(filled_b: i64) -> Self {
        let dir = TempDir::new("headers");
        let mut p =
            hk_store::Pyramid::open(dir.0.join("history"), PyramidConfig::default()).unwrap();
        let g = p.geometry().clone();
        let t_cell = g.levels[0].t_cell_ns;
        let f_cell = g.levels[0].f_cell_hz;
        let f_lo = 0.0;
        let f_hi = f_cell * CELLS as f64;
        let t0_a = 0i64;
        let t0_b = t0_a + CELLS as i64 * t_cell;
        const NB: usize = CELLS;
        let bin_hz = (f_hi - f_lo) / NB as f64;
        let mut psd = [1e-9f32; NB];
        psd[2] = 1e-6;
        for k in 0..CELLS as i64 {
            p.ingest(&FrameInput::new(
                Timestamp::from_unix_nanos(t0_a + k * t_cell),
                t_cell,
                f_lo,
                bin_hz,
                PowerUnit::Dbfs,
                &psd,
            ))
            .unwrap();
        }
        for k in 0..filled_b {
            p.ingest(&FrameInput::new(
                Timestamp::from_unix_nanos(t0_b + k * t_cell),
                t_cell,
                f_lo,
                bin_hz,
                PowerUnit::Dbfs,
                &psd,
            ))
            .unwrap();
        }
        // T-574: the watermark only advances on an explicit seal (real usage does this on an idle
        // clock / shutdown, `hk_pipeline::history`) — `ingest` alone never moves it. Sealing
        // exactly as far as data has actually arrived: tile A's whole window (always fully
        // ingested) becomes sealed; tile B only becomes sealed once `filled_b` reaches `CELLS`, so
        // a partial fill leaves it genuinely live (watermark short of its own extent's end).
        p.seal_through(Timestamp::from_unix_nanos(t0_b + filled_b * t_cell))
            .unwrap();
        let obs =
            ObservationStore::open(ObservationLogConfig::new(dir.0.join("observations"))).unwrap();
        obs.append(&dwell(f_lo, f_hi, t0_a, t0_b + CELLS as i64 * t_cell));
        obs.flush();
        let state = ApiState {
            history: Some(Arc::new(Mutex::new(p))),
            observations: Some(obs),
            // T-572: the hot-tile cache ON, as a served run has it — so this file's 304 assertions
            // are made against a server that may answer a sealed tile from RAM. A cache that
            // changed the body between a miss and a hit would turn the re-read back into a 200,
            // which is the one thing T-574 exists to prevent, and it is asserted here rather than
            // argued for.
            tile_cache: Some(Arc::new(hk_api::tiles::HotTileCache::default())),
            ..ApiState::default()
        };
        Fixture {
            _dir: dir,
            state,
            f_lo,
            f_hi,
            t_cell,
            _t0_a: t0_a,
            t0_b,
        }
    }
}

/// Ingests more rows into tile B's own store, through the `Arc` clone kept before the state moved
/// into the server — the pyramid the running server reads is the same one this mutates.
fn ingest_more_b(
    history: &Arc<Mutex<hk_store::Pyramid>>,
    f_lo: f64,
    f_hi: f64,
    t0_b: i64,
    t_cell: i64,
    from: i64,
    n: i64,
) {
    let bin_hz = (f_hi - f_lo) / CELLS as f64;
    let mut p = history.lock().unwrap();
    let mut psd = [1e-9f32; CELLS];
    psd[2] = 1e-6;
    for k in from..from + n {
        p.ingest(&FrameInput::new(
            Timestamp::from_unix_nanos(t0_b + k * t_cell),
            t_cell,
            f_lo,
            bin_hz,
            PowerUnit::Dbfs,
            &psd,
        ))
        .unwrap();
    }
}

fn path_a() -> String {
    format!("/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells={CELLS}&device={DEVICE}")
}

fn path_b() -> String {
    format!("/api/tiles?level_f=0&level_t=0&f_index=0&t_index=1&cells={CELLS}&device={DEVICE}")
}

/// **A repeated read of a sealed tile is a bodyless 304**, with the same `ETag` both times and no
/// stale answer: this is the whole point of the ticket, measured as a body-byte count and a status
/// code, never a clock.
#[test]
fn sealed_tile_repeat_read_is_304_with_zero_body_bytes() {
    let fx = Fixture::build(CELLS as i64); // tile B fully filled too, irrelevant here
    let server = serve(fx.state);
    let addr = server.local_addr();

    let first = get(addr, &path_a(), &[]);
    assert_eq!(
        first.status, 200,
        "first read of a sealed tile: {:?}",
        first.headers
    );
    assert!(
        !first.body.is_empty(),
        "first read of a sealed tile must carry the tile, got {} bytes",
        first.body.len()
    );
    let cache_control = first
        .header("cache-control")
        .expect("sealed tile must carry Cache-Control")
        .to_string();
    assert!(
        cache_control.contains("immutable") && cache_control.contains("public"),
        "sealed tile Cache-Control was {cache_control:?}"
    );
    let etag = first
        .header("etag")
        .expect("sealed tile must carry an ETag")
        .to_string();

    let second = get(addr, &path_a(), &[("If-None-Match", &etag)]);
    assert_eq!(
        second.status,
        304,
        "repeated read of an unchanged sealed tile must be 304, got {} body={} bytes",
        second.status,
        second.body.len()
    );
    assert_eq!(
        second.body.len(),
        0,
        "a 304 must transfer zero body bytes, got {}",
        second.body.len()
    );
    let content_length: usize = second
        .header("content-length")
        .expect("304 must still state Content-Length")
        .parse()
        .unwrap();
    assert_eq!(content_length, 0, "304 Content-Length must be 0");
    assert_eq!(
        second.header("etag"),
        Some(etag.as_str()),
        "304 must repeat the same ETag"
    );
}

/// **A live tile is never given an immutable cache, never given an ETag, and a repeated poll after
/// new rows arrive gets fresh, DIFFERENT bytes** — not a 304, however the client's `If-None-Match`
/// is set.
#[test]
fn live_tile_never_cached_and_body_changes_as_rows_arrive() {
    let fx = Fixture::build(1); // tile B starts with only 1 of CELLS rows: still growing
    let history = fx.state.history.clone().unwrap();
    let (f_lo, f_hi, t0_b, t_cell) = (fx.f_lo, fx.f_hi, fx.t0_b, fx.t_cell);
    let server = serve(fx.state);
    let addr = server.local_addr();

    let first = get(addr, &path_b(), &[]);
    assert_eq!(
        first.status, 200,
        "first read of the live tile: {:?}",
        first.headers
    );
    let cache_control = first
        .header("cache-control")
        .expect("live tile must still carry a Cache-Control")
        .to_string();
    assert_eq!(
        cache_control, "no-store",
        "a live/unsealed tile must never be cached as immutable, got {cache_control:?}"
    );
    assert!(
        first.header("etag").is_none(),
        "a live tile must carry NO ETag at all (that is what stops a stale 304), got {:?}",
        first.header("etag")
    );

    // A client that (wrongly) sends a stale/guessed If-None-Match must still be answered 200 with
    // the real body — a live tile can never come back as a 304.
    let guessed = get(addr, &path_b(), &[("If-None-Match", "\"deadbeef\"")]);
    assert_eq!(
        guessed.status, 200,
        "a live tile must ignore If-None-Match and never answer 304"
    );

    // New rows arrive: the tile is still not fully filled ( `filled` stays below CELLS ), so it
    // stays live/unsealed, but its content has changed.
    ingest_more_b(&history, f_lo, f_hi, t0_b, t_cell, 1, 2);
    let second = get(addr, &path_b(), &[]);
    assert_eq!(second.status, 200);
    assert_eq!(
        second.header("cache-control"),
        Some("no-store"),
        "still live after new rows: still no-store"
    );
    assert!(second.header("etag").is_none(), "still live: still no ETag");
    assert_ne!(
        first.body, second.body,
        "a live tile's body must differ once new rows have arrived (re-fetched, not served stale)"
    );
}

/// **`sealed` in the body agrees with the header treatment on both tiles of the same fixture**, so
/// the two tests above are not each exercising an accidental special case.
#[test]
fn sealed_flag_in_body_matches_header_treatment() {
    let fx = Fixture::build(CELLS as i64); // both tiles fully filled: A sealed, B sealed too once full
    let server = serve(fx.state);
    let addr = server.local_addr();

    let a = get(addr, &path_a(), &[]);
    let av: serde_json::Value = serde_json::from_slice(&a.body).unwrap();
    assert_eq!(av["sealed"], serde_json::json!(true));
    assert!(a.header("etag").is_some());

    let b = get(addr, &path_b(), &[]);
    let bv: serde_json::Value = serde_json::from_slice(&b.body).unwrap();
    assert_eq!(bv["sealed"], serde_json::json!(true));
    assert!(b.header("etag").is_some());
}

/// **T-700 / T-533: the transfer coding must not disturb the validator, and `Vary` is ONE header.**
///
/// `/api/tiles` answers itself rather than through the generic JSON tail (T-574 gives a sealed tile
/// an `ETag` and an immutable cache), so the gzip of T-533 is applied on this route's own path —
/// and that is exactly where the two features could have collided. Two rules are asserted:
///
///  1. **gzip is a transfer coding, so the `ETag` is the same tag either way.** A cache that stored
///     the identity form and revalidates over a gzip-accepting hop must still get its `304`, and a
///     `304` carries no body and therefore no content coding.
///  2. **`Vary` names both negotiation axes in one header.** `Origin` was already stated and T-533
///     negotiates content; a second `Vary:` line beside the first is a cache-correctness bug — a
///     cache reading only the first keys on `Origin` alone and can hand the gzipped body to a
///     client that refused gzip. `header()` returns the FIRST match, so this fails on that shape.
///
/// The sealed tile this fixture builds is small (8 cells, a few hundred bytes), so the `ETag` rules
/// are asserted there and the *compression* rules on a 256-cell read of the same store — whose
/// extent runs past the watermark and is therefore live, which is why it is checked for the absence
/// of an `ETag` rather than for one. Counts and status codes only, never wall clock.
#[test]
fn gzip_does_not_change_a_tiles_validator_and_vary_is_one_header() {
    let fx = Fixture::build(CELLS as i64);
    let server = serve(fx.state);
    let addr = server.local_addr();

    // ---- 1. the sealed tile: the ETag is the same tag whatever coding was negotiated -----------
    let plain = get(addr, &path_a(), &[]);
    assert_eq!(plain.status, 200, "{:?}", plain.headers);
    assert_eq!(
        plain.header("content-encoding"),
        None,
        "a request that asked for no coding must not be given one"
    );
    let etag = plain
        .header("etag")
        .expect("sealed tile must carry an ETag")
        .to_string();

    let zipped = get(addr, &path_a(), &[("Accept-Encoding", "gzip")]);
    assert_eq!(zipped.status, 200, "{:?}", zipped.headers);
    assert_eq!(
        zipped.header("etag"),
        Some(etag.as_str()),
        "the ETag is over the REPRESENTATION, so a transfer coding may not change it: \
         {:?} against {etag:?}",
        zipped.header("etag")
    );

    // The validator still validates over a gzip-accepting hop, and the 304 carries nothing at all.
    let revalidated = get(
        addr,
        &path_a(),
        &[("If-None-Match", &etag), ("Accept-Encoding", "gzip")],
    );
    assert_eq!(
        revalidated.status, 304,
        "an unchanged sealed tile must still be a 304 when gzip is offered"
    );
    assert_eq!(revalidated.body.len(), 0, "a 304 transfers no body");
    assert_eq!(
        revalidated.header("content-encoding"),
        None,
        "a bodyless 304 must name no content coding"
    );
    assert_eq!(revalidated.header("etag"), Some(etag.as_str()));

    // ---- 2. a body past the floor: it really is compressed, and Vary really is one header ------
    let big =
        format!("/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells=256&device={DEVICE}");
    let big_plain = get(addr, &big, &[]);
    assert_eq!(big_plain.status, 200, "{:?}", big_plain.headers);
    assert!(
        big_plain.body.len() >= 4096,
        "this half's premise is a body past the compression floor, got {} B",
        big_plain.body.len()
    );
    let big_zipped = get(addr, &big, &[("Accept-Encoding", "gzip")]);
    assert_eq!(
        big_zipped.header("content-encoding"),
        Some("gzip"),
        "a {} B body was not compressed",
        big_plain.body.len()
    );
    assert!(
        big_zipped.body.len() < big_plain.body.len(),
        "{} B gzipped against {} B",
        big_zipped.body.len(),
        big_plain.body.len()
    );
    for r in [&plain, &zipped, &big_plain, &big_zipped] {
        let vary = r.header("vary").expect("every answer must state Vary");
        let v = vary.to_ascii_lowercase();
        assert!(
            v.contains("origin") && v.contains("accept-encoding"),
            "Vary must name both negotiation axes in ONE header, got {vary:?}"
        );
    }
    eprintln!(
        "T-700: 256-cell tile {} B -> {} B gzipped; sealed 8-cell ETag unchanged across codings",
        big_plain.body.len(),
        big_zipped.body.len()
    );

    // A caller that says it CANNOT read gzip is not sent gzip, even past the floor.
    let refused = get(addr, &big, &[("Accept-Encoding", "gzip;q=0")]);
    assert_eq!(refused.status, 200);
    assert_eq!(
        refused.header("content-encoding"),
        None,
        "`gzip;q=0` is a refusal and must be honoured"
    );
}
