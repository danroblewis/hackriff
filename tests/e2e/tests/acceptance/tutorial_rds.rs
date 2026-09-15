//! T-094, M1 Tutorial 1 (docs/tutorials/01-rds.md): the RDS worked recipe
//! (`recipes/rds.recipe.json`) decodes RDS **blind, through the mock SDR and the API**, and its
//! fields agree with the existing Rust RDS decoder (`hk_demod::rds`, the oracle).
//!
//! Shape (both tests), exactly as a user drives `hk serve`:
//! 1. The recording replays in a loop through the mock SDR with its truth stripped
//!    ([`blind_live_streams`]); the API is wired like `hk serve` (inventory, recipe routes, stage
//!    and inspector openers, TCP stream server).
//! 2. The station is found by matching `/api/inventory` against the private truth
//!    ([`found_blind`]); `POST /api/pipelines {recipe_id: "rds", target: {emitter_id}}` attaches
//!    the built-in recipe to that emitter. No frequency is looked up or configured.
//! 3. §14 frame records are read over TCP: `inspector/<pipeline>/groups` (one record per group,
//!    with its layer tree) and on-demand stage taps on the `ps` and `rt` text nodes.
//!
//! - **Real fixture** (`fm_100p8M_2p4M_l32g30a1_t1p5_5s`): PI, PTY and TP against the truth, PS
//!   frames within the truth's set; the oracle runs on the same recording at the blindly found
//!   centre and must agree on PI, PTY and PS, with a group-level agreement rate (CRC-valid oracle
//!   groups the recipe also produced, field for field, at the same recording position). Then the
//!   tutorial's hot edit (RadioText `emit: on-change`, a hot parameter) and save, over the API.
//! - **Synthetic** (`fm_broadcast_rds` with RadioText): PI, PTY, PS and RadioText exact.

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
use hk_core::{Discontinuity, ProvenanceHandle};
use hk_demod::rds::{RdsGroup, RdsReport};
use hk_demod::{MPX_RATE_HZ, WfmConfig, WfmDemod};
use hk_dsp::{Ddc, DdcSpec, InputInfo};
use hk_e2e::{Fixture, SynthRequest, TruthItem};
use hk_model::{Provenance, SampleTime, Timestamp};
use hk_stream::{OpenerRegistry, Record, StreamReader};
use num_complex::Complex32;
use serde_json::{Value, json};

use crate::blind::{BlindLive, BlindSource, blind_live_streams, private_truth};
use crate::common::*;
use crate::listen::found_blind;
use crate::signal_062::FM_FIXTURE;

const TAG: &str = "SIGNAL-062/T-094 rds recipe";
const LIMIT: Duration = Duration::from_secs(600);
/// Synthetic RadioText (no commas: synth parameters are comma-separated lists).
const SYNTH_RT: &str = "HACKRIFF TUTORIAL 1 - RDS BUILT FROM BLOCKS";

// --- API wiring and helpers -------------------------------------------------------------------

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
        // Mutating routes (pipeline start/edit/save/stop) are audited, as `hk serve` does.
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

/// A §14.2 frame record, flattened: leaf values by path.
#[derive(Debug)]
struct Frame {
    sample_index: u64,
    valid: bool,
    /// T-210: the check passed only after ≤2-bit block correction while synced.
    corrected: bool,
    edit_rev: u64,
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
            Frame {
                sample_index: v["metadata"]["sample_index"].as_u64().unwrap_or(0),
                valid: v["crc_status"] == "valid",
                corrected: v["crc_status"] == "corrected",
                edit_rev: v["metadata"]["edit_rev"].as_u64().unwrap_or(0),
                values,
            }
        })
        .collect()
}

/// The assembled strings of a text node's frames (`<name>.text`).
fn strings(frames: &[Frame], name: &str) -> Vec<String> {
    let key = format!("{name}.text");
    frames
        .iter()
        .filter_map(|f| f.values.get(&key)?.as_str().map(str::to_owned))
        .collect()
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

/// The fields both decoders produce for one group.
#[derive(Clone, Debug, PartialEq)]
struct GroupFields {
    pi: u64,
    group_type: u64,
    version_b: bool,
    tp: bool,
    pty: u64,
    ps: Option<(u64, String)>,
}

fn recipe_fields(f: &Frame) -> Option<GroupFields> {
    let u = |k: &str| f.values.get(k).and_then(Value::as_u64);
    let group_type = u("group_type")?;
    Some(GroupFields {
        pi: u("pi")?,
        group_type,
        version_b: u("version")? == 1,
        tp: u("tp")? != 0,
        pty: u("pty")?,
        ps: if group_type == 0 {
            Some((
                u("ps.segment")?,
                f.values.get("ps.chars")?.as_str()?.to_owned(),
            ))
        } else {
            None
        },
    })
}

/// A group the oracle decoded with all four blocks CRC-valid.
fn oracle_fields(g: &RdsGroup) -> Option<GroupFields> {
    if !g.blocks_ok.iter().all(|&ok| ok) {
        return None;
    }
    let (group_type, version_b) = g.group_type?;
    Some(GroupFields {
        pi: u64::from(g.pi?),
        group_type: u64::from(group_type),
        version_b,
        tp: g.tp?,
        pty: u64::from(g.pty?),
        ps: g
            .ps_segment
            .map(|(s, c)| (u64::from(s), c.iter().map(|&b| b as char).collect())),
    })
}

/// The oracle over the whole recording: `hk_demod`'s WFM receiver with RDS on a 240 kS/s
/// channel at `center_hz` (the blindly found centre). Returns the groups with their positions
/// as recording sample indexes, and the report.
fn oracle(fx: &Fixture, center_hz: f64) -> (Vec<(f64, RdsGroup)>, RdsReport) {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&fx.meta_path).unwrap()).unwrap();
    let prov: Provenance =
        serde_json::from_value(v["global"]["hackriff:provenance"].clone()).unwrap();
    let prov = ProvenanceHandle::new(prov);
    let fs = fx.sample_rate;
    let offset = center_hz - fx.center_hz_at(0).unwrap();
    let iq: Vec<Complex32> = fx
        .samples()
        .unwrap()
        .into_iter()
        .map(|s| Complex32::new(s.re, s.im))
        .collect();
    let mut ddc = Ddc::new(
        DdcSpec::new(offset, 200e3).with_output_rate(MPX_RATE_HZ),
        fs,
    )
    .unwrap();
    let mut wfm = WfmDemod::new(WfmConfig::default(), MPX_RATE_HZ).unwrap();
    let (mut groups, mut map, mut idx) = (Vec::new(), None, 0u64);
    for (k, chunk) in iq.chunks(1 << 16).enumerate() {
        let info = InputInfo {
            time: SampleTime {
                sample_index: idx,
                host_time: Timestamp::from_unix_nanos(1_789_000_000_000_000_000),
            },
            discontinuity: if k == 0 {
                Discontinuity::STREAM_START
            } else {
                Discontinuity::NONE
            },
            dropped_before: 0,
            provenance: &prov,
        };
        let block = ddc.process(info, chunk).unwrap();
        if map.is_none() && !block.samples.is_empty() {
            map = Some((
                block.header.time.source_index as f64,
                block.header.time.source_per_output as f64,
            ));
        }
        wfm.process(block.samples);
        let (s0, per) = map.unwrap_or((0.0, fs / MPX_RATE_HZ));
        groups.extend(
            wfm.take_rds_groups()
                .into_iter()
                .map(|g| (s0 + g.position * per, g)),
        );
        idx += chunk.len() as u64;
    }
    (groups, wfm.report().rds.expect("oracle RDS enabled"))
}

fn station(fx: &Fixture) -> TruthItem {
    fx.of_kind("wfm-broadcast")
        .first()
        .map(|t| (*t).clone())
        .unwrap_or_else(|| panic!("[{TAG}] fixture truth has no wfm-broadcast station"))
}

/// Starts the built-in RDS recipe on `emitter` and checks the pipeline the API answers.
fn start_rds(s: &Served, emitter: &str) -> (String, Value) {
    let (code, list) = s.call("GET", "/api/recipes", None);
    assert_eq!(code, 200, "[{TAG}] {list}");
    assert!(
        list["recipes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == "rds" && r["builtin"] == json!(true)),
        "[{TAG}] the built-in RDS recipe is listed: {list}"
    );
    let (code, p) = s.call(
        "POST",
        "/api/pipelines",
        Some(json!({"recipe_id": "rds", "target": {"emitter_id": emitter}})),
    );
    assert_eq!(code, 201, "[{TAG}] start: {p}");
    assert_eq!(p["state"], json!("running"), "[{TAG}] {p}");
    assert_eq!(p["content_class"], json!("unrestricted"), "[{TAG}] {p}");
    eprintln!(
        "[{TAG}] pipeline {} on emitter {emitter}: channel {}",
        p["id"], p["channel"]
    );
    (p["id"].as_str().unwrap().to_owned(), p)
}

fn crc_valid_rate(status: &Value) -> (f64, f64, f64) {
    let ok = status["crc.frames_ok"].as_f64().unwrap_or(0.0);
    let bad = status["crc.frames_bad"].as_f64().unwrap_or(0.0);
    (ok, bad, ok / (ok + bad).max(1.0))
}

// --- Real fixture: truth + oracle + hot edit and save ------------------------------------------

#[test]
fn signal_062_rds_recipe_decodes_blind_through_the_mock_sdr_and_agrees_with_the_oracle() {
    let Some((meta, fx)) = private_truth(FM_FIXTURE) else {
        return;
    };
    let truth = station(&fx);
    let n = fx.n_samples().unwrap();
    let s = serve(&meta, "t094-rds");
    let (emitter, f_center, bw) = found_blind(s.addr(), &truth, 0.0);
    let (id, _) = start_rds(&s, &emitter);
    let groups = tail(s.tcp, &format!("inspector/{id}/groups"));
    let ps = tail(s.tcp, &format!("open/stage?pipeline={id}&node=ps"));

    // Two passes over the recording: every position decoded at least once after sync.
    let start = s.pipeline(&id)["stats"]["samples"].as_u64().unwrap();
    s.wait_samples(&id, start + 2 * n);
    let group_frames = frames(&groups.finish());
    let ps_frames = frames(&ps.finish());
    // Stage readouts: pilot-locked subcarrier, timing and block sync locked. The status is a
    // ~250 ms snapshot and the looping recording resets the chain at its seam, so wait for a
    // locked tick.
    let deadline = Instant::now() + LIMIT;
    let status = loop {
        let p = s.pipeline(&id);
        let st = p["status"].clone();
        if st["sync.lock"] == "locked"
            && st["rds57.lock"] == "locked"
            && st["clock.lock"] == "locked"
        {
            eprintln!("[{TAG}] stats {} status {st}", p["stats"]);
            break st;
        }
        assert!(
            Instant::now() < deadline,
            "[{TAG}] stages never locked: {st}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(status["rds57.pilot_locked"], json!(1.0), "[{TAG}] {status}");
    // CRC-valid rate over every group frame of the passes (status counters restart at seams).
    let ok = group_frames.iter().filter(|f| f.valid).count();
    let crc_rate = ok as f64 / group_frames.len().max(1) as f64;
    let (s_ok, s_bad, s_rate) = crc_valid_rate(&status);
    eprintln!(
        "[{TAG}] crc status since last reset: {s_ok}/{} = {s_rate:.3}",
        s_ok + s_bad
    );
    let bad = group_frames.len() - ok;
    assert!(
        crc_rate >= 0.6,
        "[{TAG}] CRC-valid groups {ok}/{} = {crc_rate:.3}",
        ok + bad
    );

    // Truth: PI, PTY, TP from the CRC-valid groups; PS frames from the truth's set.
    let valid: Vec<GroupFields> = group_frames
        .iter()
        .filter(|f| f.valid)
        .filter_map(recipe_fields)
        .collect();
    assert!(valid.len() >= 50, "[{TAG}] {} valid groups", valid.len());
    let (pi, pi_share) = majority(valid.iter().map(|g| g.pi)).unwrap();
    let (pty, _) = majority(valid.iter().map(|g| g.pty)).unwrap();
    let (tp, _) = majority(valid.iter().map(|g| g.tp)).unwrap();
    let truth_pi = truth.str("/rds/pi_hex").expect("truth PI");
    assert_eq!(format!("{pi:04X}"), truth_pi.to_uppercase(), "[{TAG}] PI");
    assert!(pi_share > 0.99, "[{TAG}] PI share {pi_share}");
    assert_eq!(Some(pty as f64), truth.f64("/rds/pty"), "[{TAG}] PTY");
    assert_eq!(Some(tp), truth.bool("/rds/tp"), "[{TAG}] TP");
    let names = strings(&ps_frames, "ps");
    let known_ps: Vec<String> = truth
        .get("/rds/ps_frames")
        .and_then(Value::as_object)
        .expect("truth PS frames")
        .keys()
        .cloned()
        .collect();
    assert!(names.len() >= 2, "[{TAG}] PS frames {names:?}");
    assert!(
        names.iter().all(|n| known_ps.contains(n)),
        "[{TAG}] PS frames {names:?} not all in {known_ps:?}"
    );
    let (top_ps, _) = majority(names.iter().cloned()).unwrap();

    // Oracle on the same recording at the same (blindly found) centre.
    let (oracle_groups, report) = oracle(&fx, f_center);
    let oracle_pi = report.pi.expect("oracle PI");
    eprintln!(
        "[{TAG}] oracle PI {} PS {:?} PTY {:?} groups {}/{} BLER {:?}",
        oracle_pi.hex(),
        report.ps_frames,
        report.pty,
        report.groups_ok,
        report.groups_total,
        report.block_error_rate
    );
    assert_eq!(u64::from(oracle_pi.pi), pi, "[{TAG}] PI vs oracle");
    assert_eq!(
        report.pty.map(u64::from),
        Some(pty),
        "[{TAG}] PTY vs oracle"
    );
    assert_eq!(report.ps(), Some(top_ps.as_str()), "[{TAG}] PS vs oracle");

    // Group-level agreement, matched by position in the recording (the looping device's sample
    // counter modulo its length). Recipe frames carry their first bit's source index (§14.2,
    // checked on the exact synthetic lattice below); the oracle's group positions count from the
    // stream's start the same way (T-106: `hk_demod::rds` used to count from pilot lock, since
    // `WfmDemod` only feeds RDS once the pilot PLL has locked), so both should agree directly,
    // within filter/timing-recovery jitter.
    let len = n as f64;
    let per_bit = fx.sample_rate / 1187.5;
    let wrap = |d: f64| (d + len / 2.0).rem_euclid(len) - len / 2.0;
    let recipe_at: Vec<(f64, GroupFields)> = group_frames
        .iter()
        .filter(|f| f.valid && f.edit_rev == 0)
        .filter_map(|f| Some(((f.sample_index as f64) % len, recipe_fields(f)?)))
        .collect();
    let oracle_ok: Vec<(f64, GroupFields)> = oracle_groups
        .iter()
        .filter_map(|(p, g)| Some((p.rem_euclid(len), oracle_fields(g)?)))
        .collect();
    let tol = 4.0 * per_bit;
    let (total, mut agree, mut conflict, mut missing) = (oracle_ok.len(), 0usize, 0usize, 0usize);
    for (po, want) in &oracle_ok {
        let near: Vec<&GroupFields> = recipe_at
            .iter()
            .filter(|(pr, _)| wrap(pr - po).abs() <= tol)
            .map(|(_, f)| f)
            .collect();
        if near.is_empty() {
            missing += 1;
        } else if near.contains(&want) {
            agree += 1;
        } else {
            conflict += 1;
            eprintln!("[{TAG}] conflict at {po:.0}: oracle {want:?} recipe {near:?}");
        }
    }
    let agreement = agree as f64 / total.max(1) as f64;
    eprintln!(
        "[{TAG}] RESULT emitter {:.4} MHz ({:.0} kHz); PI {pi:04X} PTY {pty} TP {tp}; PS {names:?}; \
         CRC-valid groups {ok}/{} = {crc_rate:.3}; oracle CRC-valid groups {total}: agree {agree}, \
         conflict {conflict}, no CRC-valid recipe group there {missing} → agreement {agreement:.3}",
        f_center / 1e6,
        bw / 1e3,
        ok + bad,
    );
    assert!(total >= 30, "[{TAG}] oracle valid groups {total}");
    assert_eq!(
        conflict, 0,
        "[{TAG}] recipe and oracle disagree on a CRC-valid group"
    );
    assert!(agreement >= 0.9, "[{TAG}] group agreement {agreement:.3}");

    // The tutorial's hot edit: RadioText emitted on every change (a hot parameter, applied at a
    // chunk boundary without stopping capture), then saved as the recipe's next version.
    let (code, mut draft) = s.call("GET", "/api/recipes/rds", None);
    assert_eq!(code, 200, "[{TAG}] {draft}");
    let rt_node = draft["nodes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|n| n["id"] == "rt")
        .unwrap();
    rt_node["params"]["emit"] = json!("on-change");
    let (code, edit) = s.call("PUT", &format!("/api/pipelines/{id}/recipe"), Some(draft));
    assert_eq!(code, 200, "[{TAG}] edit: {edit}");
    assert_eq!(edit["edit_rev"], json!(1), "[{TAG}] {edit}");
    let rt_plan = edit["plan"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == "rt")
        .cloned()
        .unwrap();
    assert_eq!(rt_plan["change"], json!("params-hot"), "[{TAG}] {edit}");
    let rt = tail(s.tcp, &format!("open/stage?pipeline={id}&node=rt"));
    let at = s.pipeline(&id)["stats"]["samples"].as_u64().unwrap();
    s.wait_samples(&id, at + n);
    let texts = strings(&frames(&rt.finish()), "radiotext");
    assert!(!texts.is_empty(), "[{TAG}] no RadioText after the edit");
    // Each 4-character segment, where received, reads the same in (nearly) every string.
    let mut consensus = String::new();
    for seg in 0..16 {
        let seen = texts
            .iter()
            .filter_map(|t| t.get(4 * seg..4 * seg + 4))
            .filter(|c| !c.trim().is_empty())
            .map(str::to_owned);
        match majority(seen) {
            Some((c, share)) => {
                assert!(
                    share >= 0.8,
                    "[{TAG}] RadioText segment {seg}: {c:?} share {share}"
                );
                consensus.push_str(&c);
            }
            None => consensus.push_str("    "),
        }
    }
    eprintln!(
        "[{TAG}] RESULT RadioText ({} on-change strings): {:?}",
        texts.len(),
        consensus.trim_end()
    );
    let (code, saved) = s.call("POST", &format!("/api/pipelines/{id}/save"), None);
    assert_eq!(code, 201, "[{TAG}] save: {saved}");
    assert_eq!(saved["version"], json!(2), "[{TAG}] {saved}");
    let (_, latest) = s.call("GET", "/api/recipes/rds", None);
    assert_eq!(latest["version"], json!(2), "[{TAG}] {latest}");
    let (code, stopped) = s.call("DELETE", &format!("/api/pipelines/{id}"), None);
    assert_eq!(code, 200, "[{TAG}] {stopped}");
    s.finish();
}

// --- Real air (T-185): CRC-valid rate, PI and complete PS, no fields from invalid groups -------

/// T-185 real-air RDS acceptance on the HackRF capture through the mock SDR, blind: the station
/// is found from the inventory, the built-in recipe attaches to it, and three passes of the
/// looping recording are read over TCP. A-priori bounds (set before measuring): CRC-valid group
/// rate ≥ 0.7 over every group frame (seams included), PI equal to the truth's, at least one
/// complete (8-character) PS name from the `ps` text node and every one in the truth's set, and
/// no CRC-invalid group carrying field values (`fields.skip_invalid`).
#[test]
fn signal_062_rds_real_air_acceptance_crc_valid_pi_and_complete_ps() {
    let tag = "SIGNAL-062/T-185 rds real air";
    let Some((meta, fx)) = private_truth(FM_FIXTURE) else {
        return;
    };
    let truth = station(&fx);
    let n = fx.n_samples().unwrap();
    let s = serve(&meta, "t185-rds");
    let (emitter, f_center, _) = found_blind(s.addr(), &truth, 0.0);
    let (id, _) = start_rds(&s, &emitter);
    let groups = tail(s.tcp, &format!("inspector/{id}/groups"));
    let ps = tail(s.tcp, &format!("open/stage?pipeline={id}&node=ps"));
    let start = s.pipeline(&id)["stats"]["samples"].as_u64().unwrap();
    s.wait_samples(&id, start + 3 * n);
    let status = s.pipeline(&id)["status"].clone();
    let group_frames = frames(&groups.finish());
    let names = strings(&frames(&ps.finish()), "ps");

    let ok = group_frames.iter().filter(|f| f.valid).count();
    let total = group_frames.len();
    let crc_rate = ok as f64 / total.max(1) as f64;
    // T-210: a `corrected` group (block correction while synced) legitimately carries fields; it
    // is counted separately and never as CRC-valid.
    let corrected = group_frames.iter().filter(|f| f.corrected).count();
    let invalid_with_fields = group_frames
        .iter()
        .filter(|f| !f.valid && !f.corrected && !f.values.is_empty())
        .count();
    let pis: Vec<u64> = group_frames
        .iter()
        .filter(|f| f.valid)
        .filter_map(|f| f.values.get("pi").and_then(Value::as_u64))
        .collect();
    let (pi, pi_share) = majority(pis.iter().copied()).expect("CRC-valid groups with PI");
    let known_ps: Vec<String> = truth
        .get("/rds/ps_frames")
        .and_then(Value::as_object)
        .expect("truth PS frames")
        .keys()
        .cloned()
        .collect();
    eprintln!(
        "[{tag}] RESULT emitter {:.4} MHz; CRC-valid groups {ok}/{total} = {crc_rate:.3}; \
         corrected groups {corrected}; PI {pi:04X} (share {pi_share:.3}); PS {names:?}; invalid \
         groups with fields {invalid_with_fields}; sync acquisitions {}, blocks ok/bad {}/{}",
        f_center / 1e6,
        status["sync.acquisitions"],
        status["sync.blocks_ok"],
        status["sync.blocks_bad"],
    );
    assert!(
        total >= 80,
        "[{tag}] {total} group frames over three passes"
    );
    assert!(crc_rate >= 0.7, "[{tag}] CRC-valid rate {crc_rate:.3}");
    let truth_pi = u64::from_str_radix(truth.str("/rds/pi_hex").expect("truth PI"), 16).unwrap();
    assert_eq!(pi, truth_pi, "[{tag}] PI");
    assert_eq!(pi_share, 1.0, "[{tag}] every CRC-valid group names the PI");
    assert!(!names.is_empty(), "[{tag}] no complete PS name");
    assert!(
        names
            .iter()
            .all(|n| n.chars().count() == 8 && known_ps.contains(n)),
        "[{tag}] PS {names:?} not all complete names in {known_ps:?}"
    );
    assert_eq!(
        invalid_with_fields, 0,
        "[{tag}] CRC-invalid groups surfaced field values"
    );
    let (code, stopped) = s.call("DELETE", &format!("/api/pipelines/{id}"), None);
    assert_eq!(code, 200, "[{tag}] {stopped}");
    s.finish();
}

// --- Synthetic: exact PI, PTY, PS and RadioText ------------------------------------------------

#[test]
fn signal_062_rds_recipe_synthetic_pi_pty_ps_and_radiotext_exact() {
    let fx = match SynthRequest::new("fm_broadcast_rds")
        .seed(94)
        .param("duration_s", 3.0)
        .param("radiotext", SYNTH_RT)
        .generate()
    {
        Ok(out) => out.fixture(0).unwrap(),
        Err(e) if e.is_unavailable() && !hk_e2e::synth::require_synth() => {
            eprintln!("SKIP {TAG}: {e}");
            return;
        }
        Err(e) => panic!("[{TAG}] synthetic scenario generation failed: {e}"),
    };
    let truth = station(&fx);
    let n = fx.n_samples().unwrap();
    let s = serve(&fx.meta_path, "t094-syn");
    let (emitter, _, _) = found_blind(s.addr(), &truth, 0.0);
    let (id, _) = start_rds(&s, &emitter);
    let groups = tail(s.tcp, &format!("inspector/{id}/groups"));
    let ps = tail(s.tcp, &format!("open/stage?pipeline={id}&node=ps"));
    let rt = tail(s.tcp, &format!("open/stage?pipeline={id}&node=rt"));
    let start = s.pipeline(&id)["stats"]["samples"].as_u64().unwrap();
    s.wait_samples(&id, start + 3 * n);
    let status = s.pipeline(&id)["status"].clone();
    let group_frames = frames(&groups.finish());
    let names = strings(&frames(&ps.finish()), "ps");
    let texts = strings(&frames(&rt.finish()), "radiotext");
    let (ok, bad, crc_rate) = crc_valid_rate(&status);

    let valid: Vec<GroupFields> = group_frames
        .iter()
        .filter(|f| f.valid)
        .filter_map(recipe_fields)
        .collect();
    let truth_pi = u64::from_str_radix(truth.str("/rds/pi_hex").unwrap(), 16).unwrap();
    let truth_pty = truth.f64("/rds/pty").unwrap() as u64;
    let truth_ps = truth.str("/rds/ps").unwrap();
    let truth_rt = truth
        .str("/rds/radiotext")
        .expect("synthetic RadioText truth");

    // §14.2: a frame's `sample_index` is its first bit. The synthetic lattice is exact: group g
    // starts g × 104 bits after the recording's first sample, carrying cycle[g % len] (PS
    // segments 0–3, then the RadioText segments).
    let spg = 104.0 * fx.sample_rate / 1187.5;
    let rt_segments = (truth_rt.len() as u64 + 1).div_ceil(4);
    let cycle: Vec<(u64, u64)> = (0..4)
        .map(|s| (0, s))
        .chain((0..rt_segments).map(|s| (2, s)))
        .collect();
    let slot = |pos: f64| {
        let g = (pos / spg).round();
        (cycle[g as usize % cycle.len()], pos / spg - g)
    };
    let (mut on_lattice, mut checked) = (0usize, 0usize);
    for f in group_frames.iter().filter(|f| f.valid) {
        let u = |k: &str| f.values.get(k).and_then(Value::as_u64);
        let (Some(gt), Some(seg)) = (u("group_type"), u("ps.segment").or(u("radiotext.segment")))
        else {
            continue;
        };
        let pos = (f.sample_index % n) as f64;
        let (want, frac) = slot(pos);
        checked += 1;
        if want == (gt, seg) && frac.abs() < 2.0 / 104.0 {
            on_lattice += 1;
        } else {
            eprintln!(
                "[{TAG}] frame at {pos}: slot offset {frac:.3} groups, got {:?} want {want:?}",
                (gt, seg)
            );
        }
    }
    eprintln!("[{TAG}] recipe frames on the first-bit lattice: {on_lattice}/{checked}");
    assert!(checked >= 20, "[{TAG}] {checked} frames checked");
    assert_eq!(
        on_lattice, checked,
        "[{TAG}] frame sample_index is the group's first bit (±2 bits)"
    );
    eprintln!(
        "[{TAG}] RESULT synthetic: CRC-valid groups {ok}/{} = {crc_rate:.3}; PS {:?}; RT {:?}",
        ok + bad,
        majority(names.iter().cloned()),
        majority(texts.iter().cloned())
    );
    assert!(valid.len() >= 20, "[{TAG}] {} valid groups", valid.len());
    assert!(crc_rate >= 0.9, "[{TAG}] CRC-valid rate {crc_rate:.3}");
    assert!(valid.iter().all(|g| g.pi == truth_pi && g.pty == truth_pty));
    assert!(
        !names.is_empty() && names.iter().all(|p| p == truth_ps),
        "[{TAG}] PS {names:?}"
    );
    assert!(
        !texts.is_empty() && texts.iter().all(|t| t == truth_rt),
        "[{TAG}] RadioText {texts:?} vs {truth_rt:?}"
    );

    // T-111: the recipe's messages outputs ingest decodes like a plugin's. CRC-valid
    // `recipe:rds` rows naming the truth PI are attached to the blindly found station emitter
    // (the pipeline's target is the sighting context; a PI does not share its channel).
    let pi = format!("{truth_pi:04X}");
    let station: hk_model::EmitterId = emitter.parse().unwrap();
    let repo = repo(&s.live.dir.0);
    let deadline = Instant::now() + LIMIT;
    loop {
        let live = repo.live_emitter_id(station).unwrap();
        let mut models: Vec<String> = repo
            .emitter_links(live)
            .unwrap()
            .into_iter()
            .filter_map(|l| match l.target {
                hk_model::LinkTarget::Decode(d) => repo.decode(d).ok(),
                _ => None,
            })
            .filter(|d| {
                d.decoder_id == "recipe:rds"
                    && d.crc_status == hk_model::CrcStatus::Valid
                    && d.identity.as_ref().is_some_and(|i| {
                        i.scheme == hk_model::IdentityScheme::RdsPi && i.value == pi
                    })
            })
            .map(|d| d.frame_model)
            .collect();
        models.sort();
        models.dedup();
        if models.iter().any(|m| m == "rds-group") {
            eprintln!(
                "[{TAG}] RESULT T-111: recipe decode rows with PI {pi} attached to station \
                 emitter {live}: frame models {models:?}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "[{TAG}] no recipe:rds decode with PI {pi} attached to {live} after {LIMIT:?} \
             (attached models {models:?})"
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    let (code, stopped) = s.call("DELETE", &format!("/api/pipelines/{id}"), None);
    assert_eq!(code, 200, "[{TAG}] {stopped}");
    s.finish();
}
