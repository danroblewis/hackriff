//! T-952 (SIGNAL-089 terrestrial APRS): the APRS recipe (`recipes/aprs.recipe.json`) decodes an
//! AX.25 UI frame **blind, through the mock SDR and the API**: NBFM -> Bell 202 AFSK 1200 tone
//! pair -> FSK discriminator -> 1200 Bd clock recovery -> NRZI decode -> flag .. flag HDLC
//! framing on the still-stuffed line (shared flags, T-1054) -> zero-bit destuff per frame ->
//! CRC-16/X-25 FCS -> address/control/PID/info fields. No APRS burst reached the explorer's antenna on 2026-09-25 (T-952's
//! notes), so there is no real recording; the fixture is `py/hkpy/synth/ax25.py`'s
//! `aprs_message` scenario.
//!
//! Shape, as a user driving `hk serve` (mirrors `tutorial_acars.rs`, T-096):
//! 1. The synthetic AX.25/AFSK recording replays in a loop through the mock SDR with its truth
//!    stripped ([`crate::blind::blind_live_streams`]).
//! 2. The emitter is found by matching `/api/inventory` against the private truth
//!    ([`found_blind`]); `POST /api/pipelines {recipe_id: "aprs", target: {emitter_id}}` attaches
//!    the built-in recipe. No frequency is looked up or configured.
//! 3. Frame records are read over TCP (`inspector/<pipeline>/frames`) and compared field for
//!    field with the fixture's hidden truth.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hk_api::{
    ApiState, AuditLog, Server, ServerConfig, StreamRegistry, StreamServer, StreamServerConfig,
    Token,
};
use hk_cli::pipeline::PipelineRecipes;
use hk_cli::record::http;
use hk_e2e::{SynthRequest, TruthItem};
use hk_stream::OpenerRegistry;
use serde_json::{Value, json};

use crate::blind::{BlindLive, BlindSource, blind_live_streams};
use crate::common::*;
use crate::listen::found_blind;

const TAG: &str = "SIGNAL-089/T-952 aprs recipe";
const LIMIT: Duration = Duration::from_secs(600);

// --- API wiring and helpers, as tutorial_acars.rs (T-096) ---------------------------------------

struct Served {
    live: BlindLive,
    server: Server,
    tcp: SocketAddr,
}

fn serve(meta: &Path, tag: &str) -> Served {
    let streams = StreamRegistry::new();
    let source = BlindSource {
        vouched_class: Some("unrestricted"),
        ..BlindSource::default()
    };
    let live = blind_live_streams(meta, tag, source, &streams);
    let rt = live.handle.recipe_runtime();
    let openers = OpenerRegistry::new()
        .with("stage", rt.stage_service())
        .with("inspector", rt.inspector_service());
    let token = Token::from_config(API_TOKEN).unwrap();
    let tcp = StreamServer::start(
        StreamServerConfig::new("127.0.0.1:0".parse().unwrap(), token.clone()),
        streams.clone(),
        openers.clone(),
    )
    .unwrap();
    let tcp_addr = tcp.local_addr();
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), token);
    config.stream_tcp = Some(tcp_addr);
    let state = ApiState {
        streams,
        inventory: Some(Arc::new(Mutex::new(repo(&live.dir.0)))),
        audit: Some(Arc::new(
            AuditLog::open(&live.dir.0.join("control-audit.jsonl")).unwrap(),
        )),
        on_demand: openers,
        recipes: Some(Arc::new(PipelineRecipes(rt))),
        ..ApiState::default()
    };
    let mut server = Server::start(config, state).unwrap();
    server.attach_stream_server(tcp);
    Served {
        live,
        server,
        tcp: tcp_addr,
    }
}

impl Served {
    fn addr(&self) -> SocketAddr {
        self.server.local_addr()
    }

    fn call(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let (code, bytes) = http(self.addr(), method, path, API_TOKEN, body.as_ref()).unwrap();
        (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    fn pipeline(&self, id: &str) -> Value {
        let (code, p) = self.call("GET", &format!("/api/pipelines/{id}"), None);
        assert_eq!(code, 200, "[{TAG}] {p}");
        p
    }

    fn wait_samples(&self, id: &str, samples: u64) {
        let deadline = Instant::now() + LIMIT;
        loop {
            let p = self.pipeline(id);
            if p["stats"]["samples"].as_u64().unwrap_or(0) >= samples {
                return;
            }
            assert_eq!(p["state"], json!("running"), "[{TAG}] {p}");
            assert!(Instant::now() < deadline, "[{TAG}] pipeline too slow: {p}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn finish(self) {
        let Served { live, server, .. } = self;
        drop(server);
        live.handle.stop();
        finish(live.handle);
    }
}

// --- Frame records, flattened: leaf values by path ---------------------------------------------

#[derive(Debug)]
struct Frame {
    values: BTreeMap<String, Value>,
}

fn frames(records: &[Value]) -> Vec<Frame> {
    records
        .iter()
        .filter(|v| v["type"] == "frame")
        .map(|v| {
            let mut values = BTreeMap::new();
            for n in v["content"]["layers"]["nodes"]
                .as_array()
                .into_iter()
                .flatten()
            {
                if !n["value"].is_null() {
                    let path = n["path"].as_str().unwrap_or("").to_owned();
                    values.insert(path, n["value"].clone());
                }
            }
            Frame { values }
        })
        .collect()
}

fn field<'a>(f: &'a Frame, name: &str) -> Option<&'a str> {
    f.values.get(name)?.as_str()
}

/// A `uint`/`display: hex` field's rendered value ("0x03"), read from the value directly (a
/// small integer) rather than `field`'s string accessor.
fn field_hex(f: &Frame, name: &str) -> Option<String> {
    f.values.get(name)?.as_u64().map(|v| format!("0x{v:02x}"))
}

fn majority<T: Ord + Clone>(xs: impl IntoIterator<Item = T>) -> Option<(T, f64)> {
    let mut counts = BTreeMap::new();
    let mut n = 0usize;
    for x in xs {
        *counts.entry(x).or_insert(0usize) += 1;
        n += 1;
    }
    let (v, c) = counts.into_iter().max_by_key(|(_, c)| *c)?;
    Some((v, c as f64 / n as f64))
}

fn message(fx: &hk_e2e::Fixture) -> TruthItem {
    fx.of_kind("aprs-message")
        .first()
        .map(|t| (*t).clone())
        .unwrap_or_else(|| panic!("[{TAG}] fixture truth has no aprs-message emission"))
}

fn start_aprs(s: &Served, emitter: &str) -> String {
    let (code, list) = s.call("GET", "/api/recipes", None);
    assert_eq!(code, 200, "[{TAG}] {list}");
    assert!(
        list["recipes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == "aprs" && r["builtin"] == json!(true)),
        "[{TAG}] the built-in APRS recipe is listed: {list}"
    );
    let (code, p) = s.call(
        "POST",
        "/api/pipelines",
        Some(json!({"recipe_id": "aprs", "target": {"emitter_id": emitter}})),
    );
    assert_eq!(code, 201, "[{TAG}] start: {p}");
    assert_eq!(p["state"], json!("running"), "[{TAG}] {p}");
    eprintln!(
        "[{TAG}] pipeline {} on emitter {emitter}: channel {}",
        p["id"], p["channel"]
    );
    p["id"].as_str().unwrap().to_owned()
}

fn crc_valid_rate(status: &Value) -> (f64, f64, f64) {
    let ok = status["crc.frames_ok"].as_f64().unwrap_or(0.0);
    let bad = status["crc.frames_bad"].as_f64().unwrap_or(0.0);
    (ok, bad, ok / (ok + bad).max(1.0))
}

// --- Synthetic: blind acceptance -----------------------------------------------------------------

#[test]
fn signal_089_aprs_recipe_decodes_blind_through_the_mock_sdr() {
    let fx = match SynthRequest::new("aprs_message")
        .seed(952)
        .param("prekey_s", 0.05)
        .param("info", "!4903.50N/07201.75W-HACKRIFF T952 TEST")
        .param("n_bursts", 3)
        .generate()
    {
        Ok(out) => out.fixture(0).unwrap(),
        Err(e) if e.is_unavailable() && !hk_e2e::synth::require_synth() => {
            eprintln!("SKIP {TAG}: {e}");
            return;
        }
        Err(e) => panic!("[{TAG}] synthetic scenario generation failed: {e}"),
    };
    let truth = message(&fx);
    let n = fx.n_samples().unwrap();
    let s = serve(&fx.meta_path, "t952-syn");
    let (emitter, f_center, bw) = found_blind(s.addr(), &truth, 0.0);
    let id = start_aprs(&s, &emitter);
    let frames_stream = tail(s.tcp, &format!("inspector/{id}/frames"), TAG);

    let start = s.pipeline(&id)["stats"]["samples"].as_u64().unwrap();
    s.wait_samples(&id, start + 8 * n);
    let status = s.pipeline(&id)["status"].clone();
    let all = frames(&frames_stream.finish());
    let (ok, bad, crc_rate) = crc_valid_rate(&status);
    eprintln!(
        "[{TAG}] CRC-valid (CRC-16/X-25 FCS) {ok}/{} = {crc_rate:.3}",
        ok + bad
    );

    assert!(!all.is_empty(), "[{TAG}] no AX.25 frames decoded");
    let control = majority(all.iter().filter_map(|f| field_hex(f, "control")));
    let pid = majority(all.iter().filter_map(|f| field_hex(f, "pid")));
    // The map keeps the closing HDLC flag (0x7E, '~') in `info` (as ACARS keeps its ETX/ETB).
    let info = majority(
        all.iter()
            .filter_map(|f| field(f, "info").map(|t| t.trim_end_matches('~'))),
    );

    let truth_dest_hex = truth.str("/frame/dest_hex").expect("truth dest_hex");
    let truth_src_hex = truth.str("/frame/src_hex").expect("truth src_hex");
    let truth_info = truth.str("/fields/info").expect("truth info");
    let (identity_type, identity_value) = truth.identity().expect("truth identity");
    assert_eq!(identity_type, "ax25_source");
    let truth_src_call = truth.str("/fields/src_call").expect("truth src_call");
    let truth_src_ssid = truth.f64("/fields/src_ssid").expect("truth src_ssid") as i64;
    assert_eq!(identity_value, format!("{truth_src_call}-{truth_src_ssid}"));

    eprintln!(
        "[{TAG}] RESULT emitter {:.4} MHz ({:.0} kHz); {} frames ({ok} CRC-valid); \
         control {control:?} pid {pid:?} info {info:?} vs truth dest {truth_dest_hex:?} \
         src {truth_src_hex:?} info {truth_info:?}",
        f_center / 1e6,
        bw / 1e3,
        all.len(),
    );

    let (control_v, control_share) = control.expect("[{TAG}] no control decoded");
    assert_eq!(control_v, "0x03", "[{TAG}] control (UI)");
    let (pid_v, _) = pid.expect("[{TAG}] no pid decoded");
    assert_eq!(pid_v, "0xf0", "[{TAG}] pid (no layer 3)");
    let (info_v, info_share) = info.expect("[{TAG}] no info decoded");
    assert_eq!(info_v, truth_info, "[{TAG}] info");
    // The recipe's crc.drop_invalid keeps only FCS-valid frames reaching `fields`, so every
    // decoded record here should already agree (unlike ACARS's 40-bit sync word, AX.25's 8-bit
    // flag chance-matches noise and idle flag runs often, which is why `crc_rate` over *all*
    // sync_search candidates — logged above — is low; that noise never reaches this stream).
    assert!(control_share > 0.9, "[{TAG}] control share {control_share}");
    assert!(info_share > 0.9, "[{TAG}] info share {info_share}");

    // `all` (from the `frames` inspector stream, post `crc.drop_invalid`) is the ground truth of
    // what decoded, already asserted non-empty above; `status["crc.*"]`, logged for context, is
    // cumulative-counter bookkeeping read at a slightly different instant and isn't a gate here.

    let (code, stopped) = s.call("DELETE", &format!("/api/pipelines/{id}"), None);
    assert_eq!(code, 200, "[{TAG}] {stopped}");
    s.finish();
}
