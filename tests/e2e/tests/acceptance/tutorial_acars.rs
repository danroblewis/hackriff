//! T-096/T-108, M1 Tutorial 3 (docs/tutorials/03-acars.md): the ACARS worked recipe
//! (`recipes/acars.recipe.json`) decodes an ACARS downlink block **blind, through the mock SDR
//! and the API**: AM → MSK 2400 Bd → chips → `+* SYN SYN SOH`..ETX framing → CRC-16/KERMIT →
//! fields (registration, label, block id, text).
//!
//! Shape, as a user driving `hk serve`:
//! 1. The synthetic ACARS recording (T-098, `py/hkpy/synth/acars.py`'s `acars_message` scenario;
//!    conventions from acarsdec's receiver source, T-108; no real recording or installed
//!    `acarsdec` yet — see docs/tutorials/03-acars.md) replays
//!    in a loop through the mock SDR with its truth stripped ([`crate::blind::blind_live_streams`]).
//! 2. The emitter is found by matching `/api/inventory` against the private truth
//!    ([`found_blind`]); `POST /api/pipelines {recipe_id: "acars", target: {emitter_id}}` attaches
//!    the built-in recipe. No frequency is looked up or configured.
//! 3. §14 frame records are read over TCP (`inspector/<pipeline>/blocks`) and compared field for
//!    field with the fixture's hidden truth.
//!
//! **Oracle.** `acarsdec` is the field's reference decoder; when `which acarsdec` finds it, this
//! test would cross-check it against the recording (T-098 found no cheap Homebrew install and
//! this environment has none either, so the branch below only reports the skip — no acarsdec
//! integration has been written, there being nothing to test it against).

use std::collections::BTreeMap;
use std::io::Write as _;
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hk_api::{
    ApiState, AuditLog, Server, ServerConfig, StreamRegistry, StreamServer, StreamServerConfig,
    Token,
};
use hk_cli::pipeline::PipelineRecipes;
use hk_cli::record::http;
use hk_e2e::{SynthRequest, TruthItem};
use hk_stream::{OpenerRegistry, Record, StreamReader};
use serde_json::{Value, json};

use crate::blind::{BlindLive, BlindSource, blind_live_streams};
use crate::common::*;
use crate::listen::found_blind;

const TAG: &str = "SIGNAL-062/T-096 acars recipe";
const LIMIT: Duration = Duration::from_secs(600);

// --- API wiring and helpers, as tutorial_rds.rs (T-094) ----------------------------------------

/// A blind looping run served like `hk serve`: HTTP API plus the TCP stream server.
struct Served {
    live: BlindLive,
    server: Server,
    tcp: SocketAddr,
}

fn serve(meta: &Path, tag: &str) -> Served {
    let streams = StreamRegistry::new();
    // The user vouches the recording's content class, as for any band without a content rule
    // (118–137 MHz has none, so frequency alone yields `metadata-only` and gates the fields):
    // user configuration, not truth.
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

/// A TCP consumer collecting a stream's JSON records until [`Tail::finish`].
struct Tail {
    sock: TcpStream,
    join: JoinHandle<Vec<Value>>,
}

/// Opens a stage/inspector tap and returns it reading, re-issuing a refusal.
///
/// **T-632.** The header used to be read on the spawned thread and any failure carried to
/// `finish()` as a panic, so a refusal frame failed the test at a point that said nothing about
/// what refused or when. A tap open is refused for reasons that are not this test's subject —
/// none of these files asserts a refusal at all: the TCP server's connection cap (`503 busy`), a
/// chain/tap budget, or a `404` in the moment before the recipe pipeline finishes registering.
/// So the header is read HERE, and a refusal is re-issued on a fresh connection a BOUNDED NUMBER
/// of times (attempts, never wall clock), printing each. Running out is the failure, and it names
/// the target.
fn tail(tcp: SocketAddr, target: &str) -> Tail {
    for attempt in 1..=OPEN_TRIES {
        let mut s = TcpStream::connect(tcp).unwrap();
        s.set_read_timeout(Some(LIMIT)).unwrap();
        let sep = if target.contains('?') { '&' } else { '?' };
        s.write_all(format!("{target}{sep}token={API_TOKEN}\n").as_bytes())
            .unwrap();
        let sock = s.try_clone().unwrap();
        let mut r = StreamReader::new(s);
        if let Err(e) = r.read_header() {
            eprintln!(
                "[{TAG}] tail {target} attempt {attempt}/{OPEN_TRIES}: no stream header ({e:?}), \
                 re-requesting"
            );
            let _ = sock.shutdown(Shutdown::Both);
            continue;
        }
        let join = std::thread::spawn(move || {
            let mut out = Vec::new();
            while let Ok(Some(rec)) = r.next_record() {
                match rec {
                    Record::Message(m) => out.push(m.value),
                    Record::Unknown(b) => {
                        if let Ok(v) = serde_json::from_slice(&b) {
                            out.push(v);
                        }
                    }
                    _ => {}
                }
            }
            out
        });
        return Tail { sock, join };
    }
    panic!("[{TAG}] tail {target}: {OPEN_TRIES} opens in a row produced no stream header");
}

impl Tail {
    fn finish(self) -> Vec<Value> {
        let _ = self.sock.shutdown(Shutdown::Both);
        self.join.join().expect("the tail reader thread")
    }
}

/// A §14.2 frame record, flattened: leaf values by path.
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

/// The most frequent value and its share.
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
    fx.of_kind("acars-message")
        .first()
        .map(|t| (*t).clone())
        .unwrap_or_else(|| panic!("[{TAG}] fixture truth has no acars-message emission"))
}

/// Starts the built-in ACARS recipe on `emitter` and checks the pipeline the API answers.
fn start_acars(s: &Served, emitter: &str) -> String {
    let (code, list) = s.call("GET", "/api/recipes", None);
    assert_eq!(code, 200, "[{TAG}] {list}");
    assert!(
        list["recipes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == "acars" && r["builtin"] == json!(true)),
        "[{TAG}] the built-in ACARS recipe is listed: {list}"
    );
    let (code, p) = s.call(
        "POST",
        "/api/pipelines",
        Some(json!({"recipe_id": "acars", "target": {"emitter_id": emitter}})),
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

// --- Oracle availability ------------------------------------------------------------------------

/// `acarsdec` is not expected to be installed (T-098: no cheap Homebrew install; not a system
/// package this task may install). When present, a real cross-check belongs here; for now the
/// test only reports the finding, honestly, rather than failing.
fn acarsdec_available() -> bool {
    std::process::Command::new("which")
        .arg("acarsdec")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// --- Synthetic: blind acceptance -----------------------------------------------------------------

#[test]
fn signal_062_acars_recipe_decodes_blind_through_the_mock_sdr() {
    if acarsdec_available() {
        eprintln!(
            "[{TAG}] acarsdec found on PATH but no oracle cross-check is implemented \
             (T-098 built no fixture against it); reporting recipe-only results"
        );
    } else {
        eprintln!(
            "[{TAG}] SKIP oracle: acarsdec not found on PATH (T-098: not cheaply installable); \
             asserting only against the synthetic fixture's own hidden truth"
        );
    }

    let fx = match SynthRequest::new("acars_message")
        .seed(96)
        .param("prekey_s", 0.02)
        .param("text", "HACKRIFF T096 TUTORIAL 3")
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
    let s = serve(&fx.meta_path, "t096-syn");
    let (emitter, f_center, bw) = found_blind(s.addr(), &truth, 0.0);
    let id = start_acars(&s, &emitter);
    let blocks = tail(s.tcp, &format!("inspector/{id}/blocks"));

    // Several loops of this recording (three bursts each): the block is decoded many times.
    let start = s.pipeline(&id)["stats"]["samples"].as_u64().unwrap();
    s.wait_samples(&id, start + 8 * n);
    let status = s.pipeline(&id)["status"].clone();
    let all = frames(&blocks.finish());
    let (ok, bad, crc_rate) = crc_valid_rate(&status);
    eprintln!(
        "[{TAG}] CRC-valid (CRC-16/KERMIT block check) {ok}/{} = {crc_rate:.3}",
        ok + bad
    );

    assert!(!all.is_empty(), "[{TAG}] no ACARS block frames decoded");
    let registration = majority(all.iter().filter_map(|f| field(f, "registration")));
    let label = majority(all.iter().filter_map(|f| field(f, "label")));
    let block_id = majority(all.iter().filter_map(|f| field(f, "block_id")));
    // The map keeps the closing ETX/ETB in `text` (rendered as an escape); the message is before it.
    let text = majority(all.iter().filter_map(|f| {
        field(f, "text").map(|t| t.trim_end_matches("\\x03").trim_end_matches("\\x17"))
    }));
    let mode = majority(all.iter().filter_map(|f| field(f, "mode")));

    let truth_reg = truth.str("/fields/reg").expect("truth registration");
    let truth_label = truth.str("/fields/label").expect("truth label");
    let truth_block_id = truth.str("/fields/block_id").expect("truth block id");
    let truth_text = truth.str("/fields/text").expect("truth text");
    let truth_mode = truth.str("/fields/mode").expect("truth mode");
    let (identity_type, identity_value) = truth.identity().expect("truth identity");
    assert_eq!(identity_type, "acars_reg");
    assert_eq!(identity_value, truth_reg.trim());

    eprintln!(
        "[{TAG}] RESULT emitter {:.4} MHz ({:.0} kHz); {} block frames ({ok} CRC-valid); \
         mode {mode:?} registration {registration:?} label {label:?} block_id {block_id:?} \
         text {text:?} vs truth mode {truth_mode:?} reg {truth_reg:?} label {truth_label:?} \
         block_id {truth_block_id:?} text {truth_text:?}",
        f_center / 1e6,
        bw / 1e3,
        all.len(),
    );

    let (reg_v, reg_share) = registration.expect("[{TAG}] no registration decoded");
    assert_eq!(reg_v, truth_reg, "[{TAG}] registration");
    assert!(reg_share > 0.5, "[{TAG}] registration share {reg_share}");
    let (label_v, _) = label.expect("[{TAG}] no label decoded");
    assert_eq!(label_v, truth_label, "[{TAG}] label");
    let (block_id_v, _) = block_id.expect("[{TAG}] no block id decoded");
    assert_eq!(block_id_v, truth_block_id, "[{TAG}] block id");
    let (mode_v, _) = mode.expect("[{TAG}] no mode decoded");
    assert_eq!(mode_v, truth_mode, "[{TAG}] mode");
    let (text_v, text_share) = text.expect("[{TAG}] no text decoded");
    assert_eq!(text_v, truth_text, "[{TAG}] text");
    assert!(text_share > 0.5, "[{TAG}] text share {text_share}");

    // The block check: CRC-16/KERMIT over the parity-bearing characters, as acarsdec checks it
    // (framing/tests.rs `acars_terminator_lsb_characters_and_crc16_kermit`); a 25 dB synthetic
    // recording should validate on nearly every decode once framing locks on.
    assert!(ok >= 3.0, "[{TAG}] too few CRC-valid blocks: {ok}");
    assert!(crc_rate >= 0.5, "[{TAG}] CRC-valid rate {crc_rate:.3}");

    let (code, stopped) = s.call("DELETE", &format!("/api/pipelines/{id}"), None);
    assert_eq!(code, 200, "[{TAG}] {stopped}");
    s.finish();
}
