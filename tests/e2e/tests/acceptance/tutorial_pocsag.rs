//! T-095, M1 Tutorial 2 (docs/tutorials/02-pocsag.md): the POCSAG worked recipe
//! (`recipes/pocsag.recipe.json`) decodes a multi-channel pager net **blind, through the mock SDR
//! and the API**, using `follow_hops` with the channel set discovered blindly from the pipeline's
//! own `detections` source (never a hard-coded frequency list).
//!
//! Shape, as a user drives `hk serve`:
//! 1. A synthetic multi-channel POCSAG scene (`hkpy.synth.pocsag_pagers`, three distinct pages —
//!    numeric and alphanumeric — plus one message simulcast on two channels, all 1200 Bd) replays
//!    in a loop through the mock SDR with its truth stripped ([`crate::blind::blind_live_streams`]).
//! 2. `/api/inventory` finds all four channels blind (matched against the private truth only by
//!    the test).
//! 3. `POST /api/pipelines {recipe: <pocsag, content-vouched>, target: {band}}` starts the recipe
//!    over the whole captured window; `follow_hops` resolves its channel set from the blind
//!    `detections` source over that band, never from a list this test supplies.
//!
//!    The test starts a draft of the same recipe labelled `unrestricted` so it can assert on RIC,
//!    function and message text.
//! 4. §14 frame records read over TCP (`inspector/<pipeline>/pages`) are checked against the
//!    hidden truth: RIC, function, text and channel tag, with the simulcast message deduplicated
//!    to one output (`follow_hops`'s `dups` counter).
//! 5. `multimon-ng` cross-checks the POCSAG encoder/framing independently (T-098), when installed.

use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use hk_api::{
    ApiState, AuditLog, Server, ServerConfig, StreamRegistry, StreamServer, StreamServerConfig,
    Token,
};
use hk_cli::pipeline::PipelineRecipes;
use hk_cli::record::http;
use hk_e2e::blind::matching;
use hk_e2e::{SynthRequest, TruthItem};
use hk_stream::OpenerRegistry;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

use crate::blind::{BlindLive, BlindSource, blind_live_streams, center_tol_hz};
use crate::common::*;

const TAG: &str = "SIGNAL-062/T-095 pocsag recipe";
const LIMIT: Duration = Duration::from_secs(600);

/// Scene parameters (public: what a user tuning the device would also know -- never the hidden
/// per-message truth). `hkpy.synth.pocsag_pagers` with 4 channels: 3 distinct pages (numeric and
/// alphanumeric) and one message simulcast on channels 1 and 3, all 1200 Bd.
const CENTER_HZ: f64 = 152.360e6;
const SAMPLE_RATE_HZ: f64 = 132_300.0;
/// A 25 kHz raster (T-109): every channel's 16 kHz recipe DDC fits the usable ±0.49 fs window
/// (T-095's +57 kHz channel reached +65.1 kHz, past it, and the start was refused).
const OFFSETS_HZ: [f64; 4] = [-50_000.0, -25_000.0, 25_000.0, 50_000.0];
const RICS: [u64; 4] = [1_234_560, 1_876_544, 654_320, 1_876_544];
const FUNCTIONS: [u64; 4] = [0, 3, 3, 3];
const MESSAGES: [&str; 4] = [
    "911234",
    "STANDBY AT GATE 12",
    "HACKRIFF PAGE TEST",
    "STANDBY AT GATE 12",
];
/// Channels 1 and 3 carry the same RIC and message: a simulcast pair for `follow_hops` dedupe.
const SIMULCAST: (usize, usize) = (1, 3);
/// A decode's channel tag (the blind lane centre) must be within this of its channel, Hz.
const TAG_TOL_HZ: f64 = 3_000.0;
/// Per-channel key-up offsets (s): independent channels key at their own times, the simulcast
/// pair together (T-109: a real net never keys every channel in lockstep).
const START_OFFSETS_S: [f64; 4] = [0.0, 0.21, 0.37, 0.21];

// --- API wiring (as `serve()` in tutorial_rds.rs / tutorial_01) --------------------------------

struct Served {
    live: BlindLive,
    server: Server,
    tcp: SocketAddr,
}

/// Serves `meta` blind and looping, labelled `unrestricted`.
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

// --- Frame records -------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Frame {
    sample_index: u64,
    channel: u16,
    channel_hz: f64,
    valid: bool,
    ric: Option<u64>,
    function: Option<u64>,
    text: Option<String>,
}

fn frames(records: &[Value]) -> Vec<Frame> {
    records
        .iter()
        .filter(|v| v["type"] == "frame")
        .map(|v| {
            let mut values = std::collections::BTreeMap::new();
            for n in v["content"]["layers"]["nodes"]
                .as_array()
                .into_iter()
                .flatten()
            {
                if !n["value"].is_null() {
                    values.insert(
                        n["path"].as_str().unwrap_or("").to_owned(),
                        n["value"].clone(),
                    );
                }
            }
            let text = values
                .get("numeric")
                .or_else(|| values.get("alpha"))
                .and_then(Value::as_str)
                .map(|s| {
                    // POCSAG pads an alphanumeric message's last codeword with NUL characters; the
                    // ascii field renders a control byte as a `\x00` escape.
                    let mut s = s.trim_end();
                    while let Some(t) = s.strip_suffix("\\x00").or_else(|| s.strip_suffix('\u{0}'))
                    {
                        s = t.trim_end();
                    }
                    s.to_owned()
                });
            Frame {
                sample_index: v["metadata"]["sample_index"].as_u64().unwrap_or(0),
                channel: v["metadata"]["channel"].as_u64().unwrap_or(0) as u16,
                channel_hz: v["metadata"]["channel_hz"].as_f64().unwrap_or(f64::NAN),
                valid: v["crc_status"] == "valid",
                ric: values.get("ric").and_then(Value::as_u64),
                function: values.get("function").and_then(Value::as_u64),
                text,
            }
        })
        .collect()
}

// --- Blind channel discovery ---------------------------------------------------------------

/// Waits until every one of `truths` (one per pager channel) matches an `/api/inventory` row,
/// blind: never a lookup into the hidden truth's frequency, only matching what detection found.
fn found_blind_all(addr: SocketAddr, truths: &[&TruthItem]) -> Vec<(String, f64, f64)> {
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        let (_, rows) = api_inventory(addr);
        let mut out = Vec::with_capacity(truths.len());
        for t in truths {
            let hits = matching(
                t,
                0.0,
                &rows,
                |r| {
                    (
                        r["f_center_hz"].as_f64().unwrap_or(f64::NAN),
                        r["bandwidth_hz"].as_f64().unwrap_or(0.0),
                    )
                },
                center_tol_hz(t),
            );
            if let Some(best) = hits.iter().max_by_key(|r| r["count"].as_u64().unwrap_or(0)) {
                out.push((
                    best["id"].as_str().unwrap().to_owned(),
                    best["f_center_hz"].as_f64().unwrap(),
                    best["bandwidth_hz"].as_f64().unwrap(),
                ));
            }
        }
        if out.len() == truths.len() {
            eprintln!(
                "[{TAG}] all {} pager channels found blind in {} inventory rows",
                truths.len(),
                rows.len()
            );
            return out;
        }
        assert!(
            Instant::now() < deadline,
            "[{TAG}] only {}/{} pager channels found blind; inventory (MHz, kHz): {:?}",
            out.len(),
            truths.len(),
            rows.iter()
                .map(|r| (
                    r["f_center_hz"].as_f64().unwrap_or(0.0) / 1e6,
                    r["bandwidth_hz"].as_f64().unwrap_or(0.0) / 1e3
                ))
                .collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Starts a draft of the shipped `pocsag` recipe labelled `unrestricted` on `band` so
/// RIC/function/text can be asserted. The channel set is left
/// to `follow_hops`'s blind `detections` source: `band` is the whole captured window, never a
/// per-channel list.
fn start_pocsag(s: &Served, band: (f64, f64)) -> (String, Value) {
    let (code, list) = s.call("GET", "/api/recipes", None);
    assert_eq!(code, 200, "[{TAG}] {list}");
    assert!(
        list["recipes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == "pocsag" && r["builtin"] == json!(true)),
        "[{TAG}] the built-in POCSAG recipe is listed: {list}"
    );
    let (code, mut draft) = s.call("GET", "/api/recipes/pocsag", None);
    assert_eq!(code, 200, "[{TAG}] {draft}");
    assert_eq!(
        draft["input"]["channels"]["mode"],
        json!("follow-hops"),
        "[{TAG}] {draft}"
    );
    draft["output_policy"]["content_class"] = json!("unrestricted");
    let (code, p) = s.call(
        "POST",
        "/api/pipelines",
        Some(json!({
            "recipe": draft,
            "target": {"band": {"f_lo": band.0, "f_hi": band.1}},
        })),
    );
    if code != 201 {
        let (_, rows) = api_inventory(s.addr());
        panic!(
            "[{TAG}] start: {code} {p}; inventory (MHz, kHz, count): {:?}",
            rows.iter()
                .map(|r| (
                    r["f_center_hz"].as_f64().unwrap_or(0.0) / 1e6,
                    r["bandwidth_hz"].as_f64().unwrap_or(0.0) / 1e3,
                    r["count"].as_u64().unwrap_or(0)
                ))
                .collect::<Vec<_>>()
        );
    }
    assert_eq!(p["state"], json!("running"), "[{TAG}] {p}");
    eprintln!(
        "[{TAG}] pipeline {}: follow_hops {}",
        p["id"], p["follow_hops"]
    );
    (p["id"].as_str().unwrap().to_owned(), p)
}

// --- multimon-ng oracle (T-098) -------------------------------------------------------------

/// `Some(true/false)` when `multimon-ng` is installed and the general POCSAG oracle test passed
/// or failed; `None` when it is not installed (skipped, as the tutorial allows).
fn multimon_oracle() -> Option<bool> {
    if Command::new("multimon-ng").arg("--help").output().is_err() {
        eprintln!("[{TAG}] SKIP oracle: multimon-ng not installed");
        return None;
    }
    let py = hk_e2e::paths::py_project();
    // pytest resolves the test path from the py project (where `tests/` lives), not the repo root.
    let out = Command::new("uv")
        .current_dir(&py)
        .args(["run", "--locked", "--quiet", "--project"])
        .arg(&py)
        .args([
            "pytest",
            "-q",
            "tests/test_synth.py::test_multimon_ng_decodes_the_tutorial_pager_net_per_channel",
        ])
        .output();
    match out {
        Ok(o) => {
            eprintln!(
                "[{TAG}] multimon-ng oracle (py/tests/test_synth.py, this tutorial's net per channel): {}\n{}{}",
                if o.status.success() { "PASS" } else { "FAIL" },
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            Some(o.status.success())
        }
        Err(e) => {
            eprintln!("[{TAG}] SKIP oracle: could not run uv/pytest: {e}");
            None
        }
    }
}

// --- The test --------------------------------------------------------------------------------

// T-109: T-095 found only 1/4 channels blind. Root cause: the tracker's contiguous hop rule
// (`hk_detect::track::Tracker::hop_check`) linked each loop's burst on one channel to the next
// loop's burst on another (they abut), forming a hop set whose inventory row (118 kHz wide) hid
// the four channel tracks. Fixed with a concurrency veto (a hopper is on one channel at a time);
// see `close_packed_co_keyed_channels_stay_separate_emitters_not_a_hop_set` in hk-detect.
#[test]
fn signal_062_pocsag_recipe_multi_channel_net_decodes_blind_and_dedupes_simulcast() {
    let fx = match SynthRequest::new("pocsag_pagers")
        .seed(95)
        .param("center_hz", CENTER_HZ)
        .param("sample_rate", SAMPLE_RATE_HZ)
        .param(
            "channel_offsets_hz",
            OFFSETS_HZ
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(","),
        )
        .param("bauds_bd", "1200,1200,1200,1200")
        .param(
            "start_offsets_s",
            START_OFFSETS_S
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(","),
        )
        .param(
            "rics",
            RICS.iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(","),
        )
        .param(
            "functions",
            FUNCTIONS
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(","),
        )
        .param("messages", MESSAGES.join(","))
        .generate()
    {
        Ok(out) => out.fixture(0).unwrap(),
        Err(e) if e.is_unavailable() && !hk_e2e::synth::require_synth() => {
            eprintln!("SKIP {TAG}: {e}");
            return;
        }
        Err(e) => panic!("[{TAG}] synthetic scenario generation failed: {e}"),
    };
    let truth = fx.of_kind("pocsag-page");
    assert_eq!(
        truth.len(),
        4,
        "[{TAG}] {} pocsag-page truth items",
        truth.len()
    );
    let n = fx.n_samples().unwrap();

    let s = serve(&fx.meta_path, "t095-pocsag");
    // The usable tuned window (a DDC target must sit inside ±0.49 fs), never a channel list.
    let band = (
        CENTER_HZ - 0.49 * SAMPLE_RATE_HZ,
        CENTER_HZ + 0.49 * SAMPLE_RATE_HZ,
    );
    let refs: Vec<&TruthItem> = truth.clone();
    found_blind_all(s.addr(), &refs);

    let (id, _) = start_pocsag(&s, band);
    let pages = tail(s.tcp, &format!("inspector/{id}/pages"), TAG);
    let start = s.pipeline(&id)["stats"]["samples"].as_u64().unwrap();
    // Several loops of the (~1 s) recording: enough for the simulcast pair to collide and be
    // deduplicated more than once.
    s.wait_samples(&id, start + 6 * n);
    let status = s.pipeline(&id)["status"].clone();
    let record_values = pages.finish();
    let frs = frames(&record_values);
    s.call("DELETE", &format!("/api/pipelines/{id}"), None);
    s.finish();

    let valid: Vec<&Frame> = frs.iter().filter(|f| f.valid).collect();
    eprintln!(
        "[{TAG}] {} frame records, {} CRC-valid; status {status}",
        frs.len(),
        valid.len()
    );
    assert!(
        valid.len() >= 8,
        "[{TAG}] {} valid pages: {frs:?}",
        valid.len()
    );

    // Every channel's message: found, tagged with its channel, RIC and function correct. Truth items
    // come in annotation (key-up) order, not channel order: each names its own channel offset.
    for t in &truth {
        let want_ric = t.f64("ric").unwrap() as u64;
        let want_function = t.f64("function").unwrap() as u64;
        let want_text = t.str("message_text").unwrap();
        let off = t.f64("offset_hz").unwrap();
        let i = OFFSETS_HZ
            .iter()
            .position(|o| (o - off).abs() < 1.0)
            .unwrap_or_else(|| panic!("[{TAG}] truth offset {off} is not a scene channel"));
        let want_hz = CENTER_HZ + OFFSETS_HZ[i];
        let matches: Vec<&&Frame> = valid
            .iter()
            .filter(|f| f.ric == Some(want_ric) && f.function == Some(want_function))
            .filter(|f| f.text.as_deref().is_some_and(|s| s == want_text))
            .collect();
        let indices: Vec<u16> = matches.iter().map(|f| f.channel).collect();
        assert!(
            !matches.is_empty(),
            "[{TAG}] channel {i} (RIC {want_ric}, {want_text:?}) never decoded: {frs:?}"
        );
        eprintln!(
            "[{TAG}] channel {i} (RIC {want_ric}, {want_text:?}): {} decodes on lane(s) {indices:?}",
            matches.len()
        );
        if i != SIMULCAST.0 && i != SIMULCAST.1 {
            // A non-simulcast channel: every decode is tagged with its own channel.
            for f in &matches {
                // The lane centre is the blind detection's, not the truth's: it must be this
                // channel's lane (well under half the 25 kHz spacing), not an exact frequency.
                assert!(
                    (f.channel_hz - want_hz).abs() < TAG_TOL_HZ,
                    "[{TAG}] channel {i} decode tagged {} Hz, want {want_hz} Hz",
                    f.channel_hz
                );
            }
        }
    }

    // The simulcast pair (channels 1 and 3, same RIC and message): `follow_hops` emits it once
    // per net loop (never twice), tagged with one of the two channels it was sent on, and its
    // `dups` counter shows the duplicate was actually removed (not just one channel silent).
    let (a, b) = SIMULCAST;
    let (sim_ric, sim_text) = (RICS[a], MESSAGES[a]);
    assert_eq!((sim_ric, sim_text), (RICS[b], MESSAGES[b]));
    assert!(
        truth
            .iter()
            .filter(
                |t| t.f64("ric") == Some(sim_ric as f64) && t.str("message_text") == Some(sim_text)
            )
            .count()
            == 2,
        "[{TAG}] the hidden truth carries the simulcast pair"
    );
    let sim_hz = [CENTER_HZ + OFFSETS_HZ[a], CENTER_HZ + OFFSETS_HZ[b]];
    let sim_frames: Vec<&Frame> = valid
        .iter()
        .filter(|f| f.ric == Some(sim_ric) && f.text.as_deref() == Some(sim_text))
        .copied()
        .collect();
    assert!(
        !sim_frames.is_empty(),
        "[{TAG}] simulcast message (RIC {sim_ric}, {sim_text:?}) never decoded: {frs:?}"
    );
    for f in &sim_frames {
        assert!(
            sim_hz
                .iter()
                .any(|&hz| (f.channel_hz - hz).abs() < TAG_TOL_HZ),
            "[{TAG}] simulcast decode tagged {} Hz, want one of {sim_hz:?}",
            f.channel_hz
        );
    }
    // No two simulcast decodes within one bit interval (~ half a batch) came from *both*
    // channels at once: the merge deduplicated rather than passing both through.
    let mut by_time: Vec<u64> = sim_frames.iter().map(|f| f.sample_index).collect();
    by_time.sort_unstable();
    let min_gap = 0.2 * fx.sample_rate; // batches are ~0.45 s apart; require well under one
    for w in by_time.windows(2) {
        assert!(
            (w[1] - w[0]) as f64 >= min_gap,
            "[{TAG}] simulcast decoded twice for the same transmission at {w:?} (not deduped)"
        );
    }
    let dups = status["hops.dups"].as_f64().unwrap_or(0.0);
    assert!(dups >= 1.0, "[{TAG}] follow_hops dups counted: {status}");

    // BCH / CRC stats.
    // Upstream stage status is per lane (`ch<k>.bch.*`): sum it over the followed channels.
    let lanes = |key: &str| -> f64 {
        status
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(k, _)| k.starts_with("ch") && k.ends_with(&format!(".bch.{key}")))
            .filter_map(|(_, v)| v.as_f64())
            .sum()
    };
    let (ok, corrected_words, bad) = (
        lanes("words_ok"),
        lanes("words_corrected"),
        lanes("words_bad"),
    );
    let corrected = lanes("corrected_bits");
    assert!(
        ok + corrected_words > 0.0 && bad <= 0.05 * (ok + corrected_words),
        "[{TAG}] BCH words ok {ok} corrected {corrected_words} bad {bad}: {status}"
    );
    eprintln!(
        "[{TAG}] RESULT {} frames, {} CRC-valid; BCH words ok+corrected {ok} bad {bad} \
         (corrected bits {corrected}); follow_hops dups {dups}",
        frs.len(),
        valid.len()
    );

    // multimon-ng oracle (T-098), where installed.
    let oracle = multimon_oracle();
    match oracle {
        Some(true) => eprintln!("[{TAG}] oracle: multimon-ng agrees (py/tests/test_synth.py)"),
        Some(false) => panic!("[{TAG}] multimon-ng oracle failed"),
        None => eprintln!("[{TAG}] oracle: skipped (multimon-ng not installed)"),
    }
}
