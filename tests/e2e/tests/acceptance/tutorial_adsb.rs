//! T-097, M1 Tutorial 4 (docs/tutorials/04-adsb.md): the ADS-B / Mode S recipe
//! (`recipes/adsb.recipe.json`) decodes 1090 MHz extended squitters **blind, through the mock
//! SDR and the API**: `ppm_demod` (1 Mbit/s PPM, an 8 µs chip preamble, DF-decided 56/112-bit
//! length) → `crc` (CRC-24 0xFFF409 over the whole frame) → `fields` (DF, ICAO, and — for DF17 —
//! ME: identification, airborne position CPR + altitude, airborne velocity).
//!
//! **Finding a target blind.** Unlike the RDS tutorial's continuous carrier, ADS-B squitters are
//! ~120 µs bursts that never cluster into a stable inventory emitter on their own: the burst
//! detector's rows (`hk-detect/burst@…`) are stored but never reach the tracker
//! (`hk-pipeline/src/detect.rs` "untracked"; confirmed by `SIGNAL-001`'s burst-path test). So the
//! recipe here attaches to a [`hk_pipeline::recipes::runtime::Target::Band`] measured as the
//! median position of the burst detector's own blind detections over the live run — never a
//! frequency looked up anywhere — rather than to an emitter id.
//!
//! **Real 1090 MHz capture:** blocked on the antenna (T-025), same as `SIGNAL-001`; see the
//! `#[ignore]`d HIL placeholder at the bottom. Both tests below replay the synthetic
//! `adsb_squitter` scenario (4 aircraft × 4 DF17 squitters/aircraft, 2.4 Msps at 1090 MHz),
//! shared with `SIGNAL-001`, through a looping mock SDR (`hk serve`-shaped: inventory, recipe
//! routes, stage/inspector openers, TCP stream server — see `tutorial_rds.rs`'s `serve`, copied
//! here).
//!
//! - [`tutorial_adsb_recipe_decodes_blind_and_matches_truth`] always runs: whenever the recipe
//!   decodes a squitter CRC-valid, its DF/TC/ICAO plus altitude, raw CPR or velocity fields
//!   (whichever the message type carries) match the hidden truth exactly (0 field mismatches,
//!   every run) — the hard correctness bar. Coverage: every one of the 16 distinct truth
//!   squitters (unique by ICAO × type code × even/odd) decodes CRC-valid at least once.
//! - [`tutorial_adsb_recipe_agrees_with_readsb`] additionally runs the built-in `adsb-readsb`
//!   plugin chain over the *same* live run (it attaches automatically: default chains) and
//!   compares ICAOs and the altitude/velocity fields both sides decode, and that the recipe
//!   decodes every (ICAO, type code) readsb does; skips cleanly when `readsb` or the
//!   `hk-plugin-readsb` wrapper binary is unavailable, exactly as
//!   `signal_001_adsb_readsb_plugin_chain` does.
//!
//! **T-097's coverage gap and its root cause (T-110).** T-097 saw only 12 of 16 distinct
//! squitters, and a fixed four never decode. The cause was `ppm_demod` reading chips at whole
//! sample positions only (frame origin at an integer sample, one sample per chip at 2 Msps): a
//! squitter's sub-sample arrival phase (fixed per squitter in a looping replay) decided which
//! bit patterns survived the band-limited early/late comparison. `ppm_demod` now interpolates
//! chip centres from a fractional origin and picks each frame's phase by decision margin, and
//! the recipe runs at 2.4 Msps / 2 MHz (2 Msps forced a 1.6 MHz channel that cut the chip-rate
//! content). The block's regression test renders those four squitters at every tenth-sample
//! phase.
//!
//! **Known gap found by this task (not fixed here — a runtime feature, not a block bug).** The
//! recipe's `aircraft` output (`kind: "messages"`) does not yet ingest decodes into the
//! Repository: `hk-pipeline/src/recipes/runtime.rs` `build_sink` still answers
//! `OutputKind::Messages => Ok((OutputSink::Idle, None))`, and `recipes/graph.rs` warns "messages
//! outputs are served once field-map evaluation lands (T-089)" even though T-089 (the field-map
//! evaluator) landed. So unlike readsb's own decodes, the recipe's decodes never reach
//! `/api/inventory` on their own; both tests below read the recipe's `frames` inspector stream
//! instead (as the RDS tutorial reads `groups`).

use std::collections::{BTreeMap, BTreeSet};
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
use hk_e2e::{Fixture, SynthRequest, TruthItem};
use hk_model::{DecodedIdentity, FreqRange, IdentityScheme, Region};
use hk_stream::{OpenerRegistry, Record, StreamReader};
use serde_json::{Value, json};

use crate::blind::{BlindLive, BlindSource, blind_live_streams};
use crate::common::*;

const TAG: &str = "SIGNAL-001/T-097 adsb recipe";
const LIMIT: Duration = Duration::from_secs(600);
/// Burst detections to have blindly before deriving the target band from their median position
/// (half a loop's 16 squitters).
const MIN_BURST_HITS: usize = 8;
/// Half-width of the derived band, Hz: only the target's centre matters (`channel_plan` clamps
/// the actual channel to the recipe's own fixed `input.bandwidth_hz`, 2 MHz).
const BAND_HALF_WIDTH_HZ: f64 = 0.4e6;

// --- API wiring and helpers (copied from tutorial_rds.rs's `serve`/`Served`/`Tail`) ------------

/// A blind looping run served like `hk serve`: HTTP API plus the TCP stream server.
struct Served {
    live: BlindLive,
    server: Server,
    tcp: SocketAddr,
}

fn serve(meta: &Path, tag: &str) -> Served {
    let streams = StreamRegistry::new();
    let live = blind_live_streams(meta, tag, BlindSource::default(), &streams);
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

    /// Waits until the pipeline has read `samples` ring samples.
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
    join: JoinHandle<Result<Vec<Value>, String>>,
}

fn tail(tcp: SocketAddr, target: &str) -> Tail {
    let mut s = TcpStream::connect(tcp).unwrap();
    s.set_read_timeout(Some(LIMIT)).unwrap();
    let sep = if target.contains('?') { '&' } else { '?' };
    s.write_all(format!("{target}{sep}token={API_TOKEN}\n").as_bytes())
        .unwrap();
    let sock = s.try_clone().unwrap();
    let target = target.to_owned();
    let join = std::thread::spawn(move || {
        let mut r = StreamReader::new(s);
        r.read_header()
            .map_err(|e| format!("{target}: header: {e:?}"))?;
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
        Ok(out)
    });
    Tail { sock, join }
}

impl Tail {
    fn finish(self) -> Vec<Value> {
        let _ = self.sock.shutdown(Shutdown::Both);
        self.join
            .join()
            .unwrap()
            .unwrap_or_else(|e| panic!("[{TAG}] {e}"))
    }
}

// --- Frame records (§14.2), flattened: leaf `value` and `text` by path -------------------------

#[derive(Debug)]
struct Frame {
    valid: bool,
    values: BTreeMap<String, Value>,
    texts: BTreeMap<String, String>,
}

fn frames(records: &[Value]) -> Vec<Frame> {
    records
        .iter()
        .filter(|v| v["type"] == "frame")
        .map(|v| {
            let mut values = BTreeMap::new();
            let mut texts = BTreeMap::new();
            for n in v["content"]["layers"]["nodes"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let path = n["path"].as_str().unwrap_or("").to_owned();
                if !n["value"].is_null() {
                    values.insert(path.clone(), n["value"].clone());
                }
                if let Some(t) = n["text"].as_str() {
                    texts.insert(path, t.to_owned());
                }
            }
            Frame {
                valid: v["crc_status"] == "valid",
                values,
                texts,
            }
        })
        .collect()
}

/// The message kind a decoded frame belongs to (`ADSB_CYCLE` in `py/hkpy/synth/scenarios.py`):
/// `identification`, `velocity`, or `position-even`/`position-odd` (TC 11 covers both; the CPR
/// format bit disambiguates). `None` for anything else.
fn frame_kind(f: &Frame) -> Option<String> {
    let tc = f.values.get("me.tc")?.as_u64()?;
    Some(match tc {
        4 => "identification".to_owned(),
        19 => "velocity".to_owned(),
        9..=18 => format!("position-{}", f.texts.get("me.airborne_position.f")?),
        _ => return None,
    })
}

/// The physical barometric altitude, ft, the recipe's field map already scales
/// (`scale: 25, add: -1000` over the Q-bit-stripped AC12 value).
fn recipe_altitude(f: &Frame) -> Option<f64> {
    f.values
        .get("me.airborne_position.altitude")
        .and_then(Value::as_f64)
}

/// Signed (east/west, north/south, up/down) velocity fields, kt/kt/ft-per-min, combining the
/// recipe's raw magnitude and direction-enum fields exactly as `hk-plugin-readsb`'s own decode
/// does (`ew_velocity_kt`, `ns_velocity_kt`, `vertical_rate_fpm`).
fn recipe_velocity(f: &Frame) -> Option<(f64, f64, f64)> {
    let signed = |text: &str, pos: &str, neg: &str, mag: Option<&Value>| -> Option<f64> {
        let m = mag?.as_f64()?;
        if text == pos {
            Some(m)
        } else if text == neg {
            Some(-m)
        } else {
            None
        }
    };
    let ew = signed(
        f.texts.get("me.velocity.dew")?,
        "east",
        "west",
        f.values.get("me.velocity.vew"),
    )?;
    let ns = signed(
        f.texts.get("me.velocity.dns")?,
        "north",
        "south",
        f.values.get("me.velocity.vns"),
    )?;
    let vr = signed(
        f.texts.get("me.velocity.svr")?,
        "up",
        "down",
        f.values.get("me.velocity.vertical_rate"),
    )?;
    Some((ew, ns, vr))
}

// --- Fixture and truth ---------------------------------------------------------------------

/// The scene: 4 aircraft × 4 DF17 squitters (identification, position-even, position-odd,
/// velocity), 2.4 Msps at 1090 MHz — shared with `SIGNAL-001`.
fn scenario(duration_s: f64) -> Option<Fixture> {
    match SynthRequest::new("adsb_squitter")
        .seed(5)
        .param("duration_s", duration_s)
        .param("messages_per_aircraft", 16)
        .param("noise_dbfs", -45.0)
        .generate()
    {
        Ok(out) => Some(out.fixture(0).unwrap()),
        Err(e) if e.is_unavailable() && !hk_e2e::synth::require_synth() => {
            eprintln!("SKIP {TAG}: {e}");
            None
        }
        Err(e) => panic!("[{TAG}] synthetic scenario generation failed: {e}"),
    }
}

/// Every truth squitter, keyed by (ICAO hex, type code, message kind) — unique in this scenario
/// (each aircraft sends exactly one of each kind per cycle).
fn truth_index(fx: &Fixture) -> BTreeMap<(String, u64, String), TruthItem> {
    fx.of_kind("adsb-df17")
        .into_iter()
        .map(|t| {
            let icao = t.str("icao").expect("truth icao").to_owned();
            let tc = t.f64("tc").expect("truth tc") as u64;
            let kind = t
                .str("message_kind")
                .expect("truth message_kind")
                .to_owned();
            ((icao, tc, kind), t.clone())
        })
        .collect()
}

/// Checks a CRC-valid recipe frame's fields against its matched truth item's `metadata`
/// (`py/hkpy/synth/scenarios.py`'s per-kind `fields` dict). `Err` names the mismatch.
fn check_fields(f: &Frame, kind: &str, truth: &TruthItem) -> Result<(), String> {
    let want = |k: &str| {
        truth
            .get("metadata")
            .and_then(|m| m.get(k))
            .and_then(Value::as_f64)
    };
    match kind {
        "identification" => {
            let cat = f
                .values
                .get("me.identification.category")
                .and_then(Value::as_u64);
            if cat != Some(0) {
                return Err(format!("category {cat:?} != Some(0)"));
            }
            if f.texts
                .get("me.identification.callsign")
                .is_none_or(|t| t.len() < 14)
            {
                return Err(format!(
                    "no 48-bit callsign bytes: {:?}",
                    f.texts.get("me.identification.callsign")
                ));
            }
            Ok(())
        }
        "position-even" | "position-odd" => {
            let alt = recipe_altitude(f);
            if alt != want("altitude_ft") {
                return Err(format!("altitude {alt:?} != {:?}", want("altitude_ft")));
            }
            let lat = f
                .values
                .get("me.airborne_position.cpr_lat")
                .and_then(Value::as_u64);
            let want_lat = truth
                .get("metadata")
                .and_then(|m| m.get("cpr_lat"))
                .and_then(Value::as_u64);
            if lat != want_lat {
                return Err(format!("cpr_lat {lat:?} != {want_lat:?}"));
            }
            let lon = f
                .values
                .get("me.airborne_position.cpr_lon")
                .and_then(Value::as_u64);
            let want_lon = truth
                .get("metadata")
                .and_then(|m| m.get("cpr_lon"))
                .and_then(Value::as_u64);
            if lon != want_lon {
                return Err(format!("cpr_lon {lon:?} != {want_lon:?}"));
            }
            Ok(())
        }
        "velocity" => {
            let Some((ew, ns, vr)) = recipe_velocity(f) else {
                return Err("velocity fields did not decode".into());
            };
            let (want_ew, want_ns, want_vr) = (
                want("ew_velocity_kt"),
                want("ns_velocity_kt"),
                want("vertical_rate_fpm"),
            );
            if Some(ew) != want_ew || Some(ns) != want_ns || Some(vr) != want_vr {
                return Err(format!(
                    "velocity ({ew},{ns},{vr}) != ({want_ew:?},{want_ns:?},{want_vr:?})"
                ));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

// --- Finding the target blind: the burst detector's own median position, no lookup -------------

/// Waits for at least `min_hits` `hk-detect/burst@…` detections (the composed pipeline's blind
/// burst path, running as part of the live serve regardless of any recipe) and returns a
/// `{f_lo, f_hi}` band centred on their median position — never the fixture's truth or its
/// recorded tuning, only the system's own blind measurement.
fn blind_band(dir: &Path, min_hits: usize) -> (f64, f64) {
    let repo = repo(dir);
    let deadline = Instant::now() + LIMIT;
    loop {
        let all = repo
            .detections_in_region(&Region::new(FreqRange::new(0.0, 7.0e9), ever()))
            .unwrap();
        let mut bursts: Vec<f64> = all
            .iter()
            .filter(|d| d.detector_version.starts_with("hk-detect/burst@"))
            .map(|d| d.f_center_hz)
            .collect();
        if bursts.len() >= min_hits {
            bursts.sort_by(f64::total_cmp);
            let median = bursts[bursts.len() / 2];
            eprintln!(
                "[{TAG}] {} burst detections blind (of {} total), median centre {:.4} MHz",
                bursts.len(),
                all.len(),
                median / 1e6
            );
            return (median - BAND_HALF_WIDTH_HZ, median + BAND_HALF_WIDTH_HZ);
        }
        assert!(
            Instant::now() < deadline,
            "[{TAG}] only {} burst detections found blind after {LIMIT:?}",
            bursts.len()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Starts the built-in ADS-B recipe on the blindly measured `band` and checks the pipeline the
/// API answers.
fn start_adsb(s: &Served, band: (f64, f64)) -> (String, Value) {
    let (code, list) = s.call("GET", "/api/recipes", None);
    assert_eq!(code, 200, "[{TAG}] {list}");
    assert!(
        list["recipes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == "adsb" && r["builtin"] == json!(true)),
        "[{TAG}] the built-in adsb recipe is listed: {list}"
    );
    let (code, p) = s.call(
        "POST",
        "/api/pipelines",
        Some(json!({
            "recipe_id": "adsb",
            "target": {"band": {"f_lo": band.0, "f_hi": band.1}},
        })),
    );
    assert_eq!(code, 201, "[{TAG}] start: {p}");
    assert_eq!(p["state"], json!("running"), "[{TAG}] {p}");
    assert_eq!(p["content_class"], json!("unrestricted"), "[{TAG}] {p}");
    eprintln!(
        "[{TAG}] pipeline {} on band {:.4}-{:.4} MHz: channel {}",
        p["id"],
        band.0 / 1e6,
        band.1 / 1e6,
        p["channel"]
    );
    (p["id"].as_str().unwrap().to_owned(), p)
}

fn crc_counts(status: &Value) -> (f64, f64, f64) {
    (
        status["crc.frames_ok"].as_f64().unwrap_or(0.0),
        status["crc.frames_bad"].as_f64().unwrap_or(0.0),
        status["crc.corrected_bits"].as_f64().unwrap_or(0.0),
    )
}

// --- Blind acceptance vs the hidden truth -------------------------------------------------------

#[test]
fn tutorial_adsb_recipe_decodes_blind_and_matches_truth() {
    let Some(fx) = scenario(1.0) else { return };
    let expected = truth_index(&fx);
    assert_eq!(
        expected.len(),
        16,
        "[{TAG}] {} distinct truth squitters",
        expected.len()
    );
    let n = fx.n_samples().unwrap();
    let s = serve(&fx.meta_path, "t097-adsb");
    let band = blind_band(&s.live.dir.0, MIN_BURST_HITS);
    let (id, _) = start_adsb(&s, band);
    let tap = tail(s.tcp, &format!("inspector/{id}/frames"));
    let start = s.pipeline(&id)["stats"]["samples"].as_u64().unwrap();
    // Two loop passes over the 64-squitter scene (16 messages/aircraft): comfortable margin
    // against a squitter split by the loop seam, and enough independent tries per (aircraft,
    // kind) combination for the coverage bar below.
    s.wait_samples(&id, start + 2 * n);
    let status = s.pipeline(&id)["status"].clone();
    let (ok, bad, corrected) = crc_counts(&status);
    eprintln!(
        "[{TAG}] crc: {ok:.0} ok / {bad:.0} bad ({:.3} valid), corrected_bits {corrected:.0} \
         (correction is disabled: `correct_burst_bits` unset, matching readsb's own `--no-fix`)",
        ok / (ok + bad).max(1.0)
    );
    assert!(ok > 0.0, "[{TAG}] no CRC-valid frames decoded");

    let recs = frames(&tap.finish());
    let valid: Vec<&Frame> = recs.iter().filter(|f| f.valid).collect();
    let mut seen: BTreeMap<(String, u64, String), usize> = BTreeMap::new();
    let mut mismatches = Vec::new();
    for f in &valid {
        assert_eq!(
            f.values.get("df").and_then(Value::as_u64),
            Some(17),
            "[{TAG}] every squitter here is DF17: {f:?}"
        );
        let Some(icao) = f.values.get("icao").and_then(Value::as_u64) else {
            mismatches.push("CRC-valid frame without an icao field".to_owned());
            continue;
        };
        let icao = format!("{icao:06x}");
        let Some(kind) = frame_kind(f) else {
            mismatches.push(format!(
                "icao {icao}: no recognisable message kind (tc missing?)"
            ));
            continue;
        };
        let tc = f.values.get("me.tc").and_then(Value::as_u64).unwrap();
        let key = (icao.clone(), tc, kind.clone());
        let Some(truth) = expected.get(&key) else {
            mismatches.push(format!("decoded {key:?}, not in the truth's 16 squitters"));
            continue;
        };
        *seen.entry(key).or_insert(0) += 1;
        if let Err(e) = check_fields(f, &kind, truth) {
            mismatches.push(format!("icao {icao} tc {tc} {kind}: {e}"));
        }
    }
    let missing: Vec<_> = expected.keys().filter(|k| !seen.contains_key(*k)).collect();
    let icaos_seen: BTreeSet<&String> = seen.keys().map(|(icao, _, _)| icao).collect();
    eprintln!(
        "[{TAG}] RESULT {} CRC-valid frames read; {} of 16 distinct truth squitters decoded at \
         least once (counts {:?}); {} field mismatches; missing {missing:?}",
        valid.len(),
        seen.len(),
        seen.values().collect::<Vec<_>>(),
        mismatches.len()
    );
    // Whenever a squitter *is* CRC-valid, its fields always match the hidden truth exactly: this
    // is the hard correctness bar (never relaxed).
    assert!(mismatches.is_empty(), "[{TAG}] {mismatches:?}");
    // Coverage bar: every aircraft is found (blind identity, not just blind detection) and every
    // one of the 16 distinct truth squitters decodes CRC-valid with exactly correct fields (T-110
    // restored this after T-097's 12/16; see the module doc).
    assert_eq!(
        icaos_seen.len(),
        4,
        "[{TAG}] every aircraft has at least one CRC-valid decode: seen {icaos_seen:?}"
    );
    assert!(
        missing.is_empty(),
        "[{TAG}] only {} of 16 distinct truth squitters decoded; missing {missing:?}",
        seen.len()
    );

    let (code, stopped) = s.call("DELETE", &format!("/api/pipelines/{id}"), None);
    assert_eq!(code, 200, "[{TAG}] {stopped}");
    s.finish();
}

// --- Agreement with the readsb plugin chain, on the same live run -------------------------------

#[test]
fn tutorial_adsb_recipe_agrees_with_readsb() {
    if !readsb_available() {
        eprintln!(
            "SKIP {TAG} readsb agreement: readsb not found on PATH (CI has none; the blind \
             acceptance test above still ran)"
        );
        return;
    }
    let wrapper_built = std::env::current_exe().ok().is_some_and(|exe| {
        let dir = exe.parent();
        dir.is_some_and(|d| d.join("hk-plugin-readsb").is_file())
            || dir
                .and_then(Path::parent)
                .is_some_and(|d| d.join("hk-plugin-readsb").is_file())
    });
    if !wrapper_built {
        eprintln!(
            "SKIP {TAG} readsb agreement: hk-plugin-readsb is not built next to the test binary \
             (`cargo build -p hk-plugins --bins`; `just acceptance` does this)"
        );
        return;
    }
    let Some(fx) = scenario(1.0) else { return };
    let icaos: BTreeSet<String> = fx
        .of_kind("adsb-df17")
        .iter()
        .map(|t| t.str("icao").unwrap().to_owned())
        .collect();
    let n = fx.n_samples().unwrap();
    let s = serve(&fx.meta_path, "t097-rb");
    let band = blind_band(&s.live.dir.0, MIN_BURST_HITS);
    let (id, _) = start_adsb(&s, band);
    let tap = tail(s.tcp, &format!("inspector/{id}/frames"));
    let start = s.pipeline(&id)["stats"]["samples"].as_u64().unwrap();
    s.wait_samples(&id, start + 3 * n);
    let status = s.pipeline(&id)["status"].clone();
    let (ok, bad, corrected) = crc_counts(&status);
    let valid: Vec<Frame> = frames(&tap.finish())
        .into_iter()
        .filter(|f| f.valid)
        .collect();

    // readsb's own decodes: the built-in `adsb-readsb` chain attaches automatically (default
    // chains) to the same live run and stores its decodes through `Ingest`, independently of the
    // recipe above.
    let repo = repo(&s.live.dir.0);
    let deadline = Instant::now() + LIMIT;
    let readsb = loop {
        let mut all = Vec::new();
        for icao in &icaos {
            let identity = DecodedIdentity {
                scheme: IdentityScheme::AdsbIcao,
                value: icao.clone(),
            };
            all.extend(repo.decodes_for_identity(&identity).unwrap());
        }
        if all.len() >= icaos.len() * 2 {
            break all;
        }
        assert!(
            Instant::now() < deadline,
            "[{TAG}] readsb decoded only {} rows for {} aircraft after {LIMIT:?}",
            all.len(),
            icaos.len()
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    let readsb_icaos: BTreeSet<String> = readsb
        .iter()
        .filter_map(|d| {
            d.metadata
                .get("icao")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect();
    eprintln!(
        "[{TAG}] readsb decoded {} rows over {} ICAOs: {readsb_icaos:?}",
        readsb.len(),
        readsb_icaos.len()
    );
    assert_eq!(
        &readsb_icaos, &icaos,
        "[{TAG}] readsb sees every aircraft blind too"
    );

    // Field agreement: for every readsb decode that names a (icao, tc) the recipe also decoded
    // CRC-valid, compare whichever of altitude/velocity both sides carry.
    let (mut total, mut agree) = (0usize, 0usize);
    for d in &readsb {
        let Some(icao) = d.metadata.get("icao").and_then(Value::as_str) else {
            continue;
        };
        let Some(tc) = d.metadata.get("tc").and_then(Value::as_u64) else {
            continue;
        };
        let Some(f) = valid.iter().find(|f| {
            f.values
                .get("icao")
                .and_then(Value::as_u64)
                .map(|v| format!("{v:06x}"))
                .as_deref()
                == Some(icao)
                && f.values.get("me.tc").and_then(Value::as_u64) == Some(tc)
        }) else {
            continue;
        };
        if let Some(want_alt) = d.metadata.get("alt").and_then(Value::as_f64) {
            total += 1;
            let got = recipe_altitude(f);
            if got == Some(want_alt) {
                agree += 1;
            } else {
                eprintln!(
                    "[{TAG}] altitude disagreement icao {icao} tc {tc}: recipe {got:?} readsb {want_alt}"
                );
            }
        }
        if d.metadata.get("ew_velocity_kt").is_some() {
            let want = (
                d.metadata["ew_velocity_kt"].as_f64(),
                d.metadata["ns_velocity_kt"].as_f64(),
                d.metadata.get("vertical_rate_fpm").and_then(Value::as_f64),
            );
            total += 1;
            let got = recipe_velocity(f).map(|(ew, ns, vr)| (Some(ew), Some(ns), Some(vr)));
            if got == Some(want) {
                agree += 1;
            } else {
                eprintln!(
                    "[{TAG}] velocity disagreement icao {icao} tc {tc}: recipe {got:?} readsb {want:?}"
                );
            }
        }
    }
    let rate = agree as f64 / total.max(1) as f64;
    eprintln!(
        "[{TAG}] RESULT readsb agreement: {agree}/{total} = {rate:.3}; recipe crc {ok:.0} ok / \
         {bad:.0} bad, corrected_bits {corrected:.0}",
    );
    assert!(total >= 8, "[{TAG}] only {total} comparable fields");
    assert_eq!(
        agree, total,
        "[{TAG}] recipe and readsb disagree on a field both decoded"
    );
    // Coverage: the recipe decodes (CRC-valid) every (ICAO, type code) readsb decodes.
    let readsb_kinds: BTreeSet<(String, u64)> = readsb
        .iter()
        .filter_map(|d| {
            Some((
                d.metadata.get("icao")?.as_str()?.to_owned(),
                d.metadata.get("tc")?.as_u64()?,
            ))
        })
        .collect();
    let recipe_kinds: BTreeSet<(String, u64)> = valid
        .iter()
        .filter_map(|f| {
            Some((
                format!("{:06x}", f.values.get("icao")?.as_u64()?),
                f.values.get("me.tc")?.as_u64()?,
            ))
        })
        .collect();
    let readsb_only: Vec<_> = readsb_kinds.difference(&recipe_kinds).collect();
    eprintln!(
        "[{TAG}] RESULT coverage: readsb {} (icao, tc), recipe {}, readsb-only {readsb_only:?}",
        readsb_kinds.len(),
        recipe_kinds.len()
    );
    assert!(
        readsb_only.is_empty(),
        "[{TAG}] readsb decoded (icao, tc) the recipe did not: {readsb_only:?}"
    );

    let (code, stopped) = s.call("DELETE", &format!("/api/pipelines/{id}"), None);
    assert_eq!(code, 200, "[{TAG}] {stopped}");
    s.finish();
}

// --- Live HIL placeholder (deferred: needs a 1090 MHz antenna) ---------------------------------

/// Deferred (T-097, T-025): a live 1090 MHz HIL run on the real HackRF, receive-only, once a
/// 1090 MHz antenna is available. Shape (once unblocked): open the real device
/// (`HK_DEVICE=hackrf`, `hardware_skip`/`hackrf_info` free-check as `hil_hackrf.rs` does), serve
/// the `adsb` recipe exactly as the tests above do (find the band blind from burst detections,
/// `POST /api/pipelines {recipe_id: "adsb", target: {band}}`), and check against a HIL truth list
/// captured alongside a reference receiver (e.g. dump1090/readsb on the same antenna) rather than
/// the synthetic truth used above. Never opens the device: `#[ignore]`d until the antenna lands.
#[test]
#[ignore = "needs a 1090 MHz antenna (T-025); receive-only when unblocked"]
fn hil_1090mhz_adsb_recipe_live_on_the_hackrf() {
    unimplemented!(
        "blocked on a 1090 MHz antenna (docs/planning-log.md open question); receive-only, one \
         agent/user at a time on the shared HackRF per CLAUDE.md"
    );
}
