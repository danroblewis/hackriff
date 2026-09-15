//! T-061 (SIGNAL-062, AWARE-036): record outputs to files, through the **mock SDR device**
//! (`open_mock_replay`), blind (annotations stripped; only the test holds the truth), driven over
//! the authenticated HTTP API and the `hk record` client.
//!
//! - **FSK burst train.** A selection over the user's own sensor band records `bits` and
//!   `symbols`: the bits file's located payloads match the private truth, the symbols file has
//!   sane statistics, sidecars carry capture settings, band, framing (sync 2DD4, CRC, bit order),
//!   timestamps, software version and the Bitstream row, and the rows are saved on the
//!   selection's links. IQ of this `metadata-only` window is refused (the recording rule).
//!   Downloads answer the file bytes (Bearer and `?token=`). A full quota is refused (507).
//!   `hk record --band --kinds bits` downloads its files.
//! - **FM fixture.** The station is found blind from `/api/inventory`; recording its audio and
//!   stopping yields a valid 48 kHz mono 16-bit WAV with audio content, a Recording row, and a
//!   sidecar with the estimated mode (and the refined tuning when the listen chain refined). An
//!   IQ slice of the station's band stops at its `max_bytes`, is finalised (SigMF meta with the
//!   band annotation) and re-opens through the SigMF replay reader with the right centre and rate.

#[path = "acceptance/common.rs"]
mod common;

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use hk_api::{ApiState, AuditLog, Server, ServerConfig, Token};
use hk_cli::control::PipelineOutputs;
use hk_cli::record::{RecordOptions, RecordTarget, http};
use hk_core::{MockEnd, Pacing};
use hk_e2e::blind::{matching, strip_truth};
use hk_e2e::{Fixture, SynthRequest, synth_or_skip};
use hk_model::sigmf::SigmfMeta;
use hk_pipeline::{
    OutputLimits, OutputRecorders, Pipeline, PipelineConfig, PipelineHandle, TrackInventory,
    open_mock_replay, open_replay, replay_plan,
};
use hk_store::outputs::{OutputSidecar, WavInfo};
use serde_json::{Value, json};

const FM_FIXTURE: &str = "fm_100p8M_2p4M_l32g30a1_t1p5_5s";
const LIMIT: Duration = Duration::from_secs(900);

fn start_mock_loop(meta: &Path, dir: &Path, extra: Value) -> (PipelineHandle, f64, f64) {
    let replay = open_mock_replay(meta, Pacing::Unpaced, MockEnd::Loop).unwrap();
    let info = replay.info;
    let mut plan = replay_plan(info.center_hz, info.sample_rate_hz, info.start_time);
    plan.extra = extra;
    let mut cfg = PipelineConfig::new(dir, plan).unwrap();
    cfg.source_class = replay.class;
    cfg.device_id = replay.device.device_id.clone();
    cfg.device_hw = Some(replay.device.hw.clone());
    cfg.lossless = true;
    let (c, fs) = (info.center_hz, info.sample_rate_hz);
    let handle = Pipeline::start(
        cfg,
        Box::new(replay.source),
        info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    (handle, c, fs)
}

fn api_server(dir: &Path, outputs: &Arc<OutputRecorders>) -> Server {
    let db = Arc::new(Mutex::new(repo(dir)));
    let state = ApiState {
        inventory: Some(Arc::clone(&db)),
        bookmarks: Some(db),
        audit: Some(Arc::new(
            AuditLog::open(&dir.join("control-audit.jsonl")).unwrap(),
        )),
        outputs: Some(Arc::new(PipelineOutputs(Arc::clone(outputs)))),
        ..ApiState::default()
    };
    Server::start(
        ServerConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            Token::from_config(API_TOKEN).unwrap(),
        ),
        state,
    )
    .unwrap()
}

fn call(addr: SocketAddr, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let (status, bytes) = http(addr, method, path, API_TOKEN, body.as_ref()).unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// GET with the token in the query only (download links).
fn get_query_token(addr: SocketAddr, path: &str) -> (u16, Vec<u8>) {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(
        s,
        "GET {path}?token={API_TOKEN} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let status = std::str::from_utf8(&raw[9..12]).unwrap().parse().unwrap();
    (status, raw[split + 4..].to_vec())
}

fn session(addr: SocketAddr, id: &str) -> Value {
    let (code, list) = call(addr, "GET", "/api/outputs", None);
    assert_eq!(code, 200, "{list}");
    list["recordings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("session {id} not listed"))
}

fn file<'a>(s: &'a Value, kind: &str) -> &'a Value {
    s["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["kind"] == kind)
        .unwrap_or_else(|| panic!("no {kind} file in {s}"))
}

fn wait_for(what: &str, limit: Duration, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < limit, "[T-061] timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The payload bytes packed from the bits the status record locates.
fn payload_hex(bits: &[u8], status: &Value) -> Option<String> {
    let at = status["payload_bit"].as_u64()? as usize;
    let n = status["payload_bits"].as_u64()? as usize;
    let lsb = status["bit_order"] == "lsb-first";
    Some(
        bits.get(at..at + n)?
            .chunks_exact(8)
            .map(|c| {
                let byte = c.iter().enumerate().fold(0u8, |a, (i, &b)| {
                    a | ((b & 1) << if lsb { i } else { 7 - i })
                });
                format!("{byte:02x}")
            })
            .collect(),
    )
}

fn index(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn fsk_bits_and_symbols_record_to_files_linked_to_the_selection() {
    let out = synth_or_skip!(
        SynthRequest::new("fsk_burst_train")
            .seed(61)
            .param("snr_db", 20.0)
            .param("duration_s", 2.4)
    );
    let fx = out.fixture(0).unwrap();
    let truth: BTreeSet<String> = fx
        .of_kind("fsk-burst")
        .iter()
        .map(|t| t.value["frame"]["payload_hex"].as_str().unwrap().to_owned())
        .collect();
    let n = truth.len();
    assert!(n >= 15, "{n} truth bursts");
    let src = TempDir::new("t061fsk-src");
    let blind = strip_truth(&fx.meta_path, &src.0, "blind", 0.0).unwrap();
    let dir = TempDir::new("t061fsk");
    let (handle, center, fs) = start_mock_loop(
        &blind,
        &dir.0,
        json!({ "pipeline": { "classify": [{
            "freq_hz": [433.8e6, 434.1e6],
            "content_class": "unrestricted",
            "by": "test: own synthetic T-061 sensor"
        }] } }),
    );
    let recorders = handle.output_recorders();
    let server = api_server(&dir.0, &recorders);
    let addr = server.local_addr();

    let (code, sel) = call(
        addr,
        "POST",
        "/api/selections",
        Some(json!({ "name": "sensor", "f_lo": 433.8e6, "f_hi": 434.1e6 })),
    );
    assert_eq!(code, 201, "{sel}");
    let sel_id = sel["id"].as_str().unwrap().to_owned();

    // Bits and symbols for the selection.
    let (code, started) = call(
        addr,
        "POST",
        "/api/outputs/record/start",
        Some(json!({ "selection_id": sel_id, "kinds": ["bits", "symbols"], "max_s": 600 })),
    );
    assert_eq!(code, 200, "{started}");
    let id = started["recording"]["id"].as_str().unwrap().to_owned();
    assert_eq!(started["recording"]["active"], true);

    // IQ of a window whose source class forbids stored content follows the recording rule.
    let (code, iq) = call(
        addr,
        "POST",
        "/api/outputs/record/start",
        Some(json!({ "band": { "f_lo": 433.8e6, "f_hi": 434.1e6 }, "kinds": ["iq"] })),
    );
    assert_eq!((code, iq["code"].as_str()), (403, Some("refused")), "{iq}");

    wait_for("bursts in the bits and symbols files", LIMIT, || {
        let s = session(addr, &id);
        file(&s, "bits")["records"].as_u64().unwrap() >= 2 * n as u64
            && file(&s, "symbols")["records"].as_u64().unwrap() >= n as u64
    });
    let (code, stopped) = call(
        addr,
        "POST",
        "/api/outputs/record/stop",
        Some(json!({ "id": id })),
    );
    assert_eq!(code, 200, "{stopped}");
    let s = &stopped["recording"];
    assert_eq!(s["active"], false);
    assert_eq!(s["ended"], "stopped");
    let sdir = dir.0.join("outputs").join(&id);

    // Bits: every CRC-valid burst's located payload is a truth payload; most truth recovered.
    let bits = std::fs::read(sdir.join("bits.ru8")).unwrap();
    let bursts = index(&sdir.join("bits.bursts.jsonl"));
    assert_eq!(
        bursts
            .iter()
            .map(|b| b["len"].as_u64().unwrap())
            .sum::<u64>(),
        bits.len() as u64
    );
    assert!(bits.iter().all(|&b| b <= 1));
    let mut matched = BTreeSet::new();
    for b in &bursts {
        let (off, len) = (
            b["offset"].as_u64().unwrap() as usize,
            b["len"].as_u64().unwrap() as usize,
        );
        if b["status"]["crc"] != "valid" {
            continue;
        }
        let hex = payload_hex(&bits[off..off + len], &b["status"]).expect("a located payload");
        assert!(truth.contains(&hex), "[T-061] {hex} is not a truth payload");
        matched.insert(hex);
    }
    eprintln!(
        "[T-061] bits file: {} bursts, {} of {n} truth payloads",
        bursts.len(),
        matched.len()
    );
    assert!(
        matched.len() * 10 >= n * 8,
        "[T-061] recovered {}",
        matched.len()
    );

    // Symbols: finite soft values, both signs, magnitude, lengths agree with the index.
    let raw = std::fs::read(sdir.join("symbols.rf32_le")).unwrap();
    let sym: Vec<f32> = raw
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    let sidx = index(&sdir.join("symbols.bursts.jsonl"));
    assert_eq!(
        sidx.iter().map(|b| b["len"].as_u64().unwrap()).sum::<u64>(),
        sym.len() as u64
    );
    let pos = sym.iter().filter(|v| **v > 0.0).count() as f64 / sym.len() as f64;
    let mean_abs = sym.iter().map(|v| f64::from(v.abs())).sum::<f64>() / sym.len() as f64;
    eprintln!(
        "[T-061] symbols file: {} symbols, {pos:.2} positive, mean |x| {mean_abs:.3}",
        sym.len()
    );
    assert!(sym.iter().all(|v| v.is_finite()));
    assert!((0.2..0.8).contains(&pos), "{pos}");
    assert!(mean_abs > 1e-3);

    // Sidecars.
    let sc = OutputSidecar::read(&sdir.join("bits.json")).unwrap();
    assert_eq!(sc.schema, "hackriff-output-sidecar");
    assert_eq!((sc.kind.as_str(), sc.datatype.as_str()), ("bits", "ru8"));
    assert!((sc.capture.center_hz - center).abs() < 1.0);
    assert!((sc.capture.sample_rate_hz - fs).abs() < 1.0);
    assert!(
        sc.capture.device_id.starts_with("mock"),
        "{}",
        sc.capture.device_id
    );
    assert_eq!((sc.band.f_lo_hz, sc.band.f_hi_hz), (433.8e6, 434.1e6));
    assert_eq!(sc.target["selection_id"], sel_id.as_str());
    let framing = sc.framing.as_ref().unwrap();
    assert_eq!(framing["sync_word_hex"], "2dd4");
    assert!(framing["crc"]["valid"].as_u64().unwrap() > 0);
    assert!(framing["bit_order"].is_string());
    assert!(
        sc.estimated.as_ref().unwrap()["symbol_rate_bd"]
            .as_f64()
            .unwrap()
            > 0.0
    );
    assert!(sc.time.start_ns.is_some() && sc.time.end.is_some());
    assert!(!sc.software.version.is_empty());
    assert_eq!(sc.ended, "stopped");
    let bits_row = sc.rows.bitstream_id.clone().expect("a Bitstream row");
    let sym_sc = OutputSidecar::read(&sdir.join("symbols.json")).unwrap();
    let sym_row = sym_sc.rows.bitstream_id.clone().expect("a Bitstream row");
    assert_eq!(file(s, "bits")["bitstream_id"], bits_row.as_str());

    // Links on the selection.
    let (_, sel) = call(addr, "GET", &format!("/api/selections/{sel_id}"), None);
    let links: Vec<(String, String)> = sel["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            (
                l["kind"].as_str().unwrap().to_owned(),
                l["target"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert!(
        links.contains(&("bitstream".into(), bits_row.clone())),
        "{links:?}"
    );
    assert!(links.contains(&("bitstream".into(), sym_row)), "{links:?}");
    assert_eq!(s["links_saved"], 2);

    // Downloads: bearer and query token answer the file's bytes; a traversal name does not.
    let url = file(s, "bits")["sidecar_url"].as_str().unwrap();
    let (code, body) = http(addr, "GET", url, API_TOKEN, None).unwrap();
    assert_eq!(code, 200);
    assert_eq!(body, std::fs::read(sdir.join("bits.json")).unwrap());
    let (code, body) = get_query_token(addr, file(s, "bits")["url"].as_str().unwrap());
    assert_eq!((code, body.len()), (200, bits.len()));
    let (code, _) = get_query_token(addr, &format!("/api/outputs/{id}/files/..%2Fhackriff.db"));
    assert_eq!(code, 404);

    // `hk record` over the same API: a short band recording, downloaded.
    let cli_out = TempDir::new("t061cli");
    let outcome = hk_cli::record::run(
        &RecordOptions {
            server: addr,
            token: API_TOKEN.into(),
            target: RecordTarget::Band(433.8e6, 434.1e6),
            kinds: vec!["bits".into()],
            max_s: Some(3.0),
            max_bytes: None,
            out: cli_out.0.clone(),
        },
        || false,
    )
    .unwrap();
    assert_eq!(outcome.session["active"], false);
    assert_eq!(outcome.session["ended"], "max_s reached");
    for name in ["bits.ru8", "bits.json", "bits.bursts.jsonl"] {
        assert!(
            cli_out.0.join(name).is_file(),
            "hk record downloaded {name}"
        );
    }

    // A full quota is refused with a clear error.
    recorders.set_limits(OutputLimits {
        quota_bytes: 1024,
        ..OutputLimits::default()
    });
    let (code, refused) = call(
        addr,
        "POST",
        "/api/outputs/record/start",
        Some(json!({ "selection_id": sel_id, "kinds": ["bits"] })),
    );
    assert_eq!(code, 507, "{refused}");
    assert_eq!(refused["code"], "quota");
    assert!(refused["error"].as_str().unwrap().contains("quota"));
    assert_eq!(recorders.refused_quota(), 1);

    drop(server);
    drop(recorders);
    handle.stop();
    finish(handle);
}

#[test]
fn fm_audio_wav_and_iq_slice_record_to_files_with_sidecars() {
    let Some(meta) = real_fixture(FM_FIXTURE) else {
        return;
    };
    let truth = Fixture::load(&meta)
        .unwrap()
        .of_kind("wfm-broadcast")
        .first()
        .map(|t| (*t).clone())
        .expect("a wfm-broadcast truth item");
    let src = TempDir::new("t061fm-src");
    let blind = strip_truth(&meta, &src.0, "blind", 0.0).unwrap();
    let dir = TempDir::new("t061fm");
    let (handle, center, fs) = start_mock_loop(&blind, &dir.0, json!({}));
    let recorders = handle.output_recorders();
    let server = api_server(&dir.0, &recorders);
    let addr = server.local_addr();

    let t0 = Instant::now();
    let emitter = loop {
        let (_, rows) = api_inventory(addr);
        let hits = matching(
            &truth,
            0.0,
            &rows,
            |r| {
                (
                    r["f_center_hz"].as_f64().unwrap_or(f64::NAN),
                    r["bandwidth_hz"].as_f64().unwrap_or(0.0),
                )
            },
            100e3,
        );
        if let Some(best) = hits
            .iter()
            .max_by_key(|r| r["bandwidth_hz"].as_f64().unwrap_or(0.0) as u64)
        {
            break best["id"].as_str().unwrap().to_owned();
        }
        assert!(
            t0.elapsed() < LIMIT,
            "[T-061] the station was not found blind"
        );
        std::thread::sleep(Duration::from_millis(250));
    };

    let (code, started) = call(
        addr,
        "POST",
        "/api/outputs/record/start",
        Some(json!({ "emitter_id": emitter, "kinds": ["audio"], "max_s": 600 })),
    );
    assert_eq!(code, 200, "{started}");
    let id = started["recording"]["id"].as_str().unwrap().to_owned();
    wait_for("3 s of audio", LIMIT, || {
        file(&session(addr, &id), "audio")["records"]
            .as_u64()
            .unwrap()
            >= 150
    });
    let (code, stopped) = call(
        addr,
        "POST",
        "/api/outputs/record/stop",
        Some(json!({ "id": id })),
    );
    assert_eq!(code, 200, "{stopped}");
    let f = file(&stopped["recording"], "audio");
    assert_eq!(f["state"], "done", "{f}");

    let sdir = dir.0.join("outputs").join(&id);
    let wav_path = sdir.join("audio.wav");
    let info = WavInfo::read(&wav_path).unwrap();
    let len = std::fs::metadata(&wav_path).unwrap().len();
    assert_eq!(
        (info.sample_rate, info.channels, info.bits_per_sample),
        (48_000, 1, 16)
    );
    assert_eq!(
        u64::from(info.data_bytes),
        len - 44,
        "header patched on stop"
    );
    assert_eq!(u64::from(info.riff_bytes), len - 8);
    let pcm = std::fs::read(&wav_path).unwrap();
    let samples: Vec<f64> = pcm[44..]
        .chunks_exact(2)
        .map(|c| f64::from(i16::from_le_bytes([c[0], c[1]])) / 32767.0)
        .collect();
    assert!(samples.len() >= 150 * 960);
    let rms = (samples.iter().map(|v| v * v).sum::<f64>() / samples.len() as f64).sqrt();
    eprintln!("[T-061] wav: {} samples, rms {rms:.3}", samples.len());
    assert!(rms > 0.01, "[T-061] the WAV has audio content");

    let sc = OutputSidecar::read(&sdir.join("audio.json")).unwrap();
    assert_eq!(sc.datatype, "wav-s16le-48k-mono");
    assert!((sc.capture.center_hz - center).abs() < 1.0);
    assert_eq!(sc.target["emitter_id"], emitter.as_str());
    let audio = &sc.estimated.as_ref().unwrap()["audio"];
    assert!(
        audio["mode"].as_str().is_some_and(|m| !m.is_empty()),
        "{audio}"
    );
    assert!(sc.time.start_ns.is_some());
    assert!(sc.rows.recording_id.is_some(), "a Recording row");
    let stored = repo(&dir.0)
        .refined_tuning(emitter.parse().unwrap())
        .unwrap();
    eprintln!(
        "[T-061] mode {}, refinement in header: {}, sidecar refined tuning: {}",
        audio["mode"],
        !audio["refinement"].is_null(),
        sc.refined_tuning.is_some()
    );
    if !audio["refinement"].is_null() {
        assert!(stored.is_some(), "the listen refinement is stored");
        let r = sc
            .refined_tuning
            .as_ref()
            .expect("refined tuning in the sidecar");
        assert_eq!(r["provenance"], "refined by output analysis");
    }

    // IQ slice of the station's band: stops at its byte budget, finalised with the band
    // annotated, and re-opens through the SigMF replay reader with the right centre and rate.
    let (f_lo, f_hi) = (sc.band.f_lo_hz, sc.band.f_hi_hz);
    let iq_budget = 4u64 << 20;
    let (code, iq) = call(
        addr,
        "POST",
        "/api/outputs/record/start",
        Some(
            json!({ "band": { "f_lo": f_lo, "f_hi": f_hi }, "kinds": ["iq"], "max_bytes": iq_budget }),
        ),
    );
    assert_eq!(code, 200, "{iq}");
    let iq_id = iq["recording"]["id"].as_str().unwrap().to_owned();
    wait_for("the IQ slice to reach max_bytes", LIMIT, || {
        session(addr, &iq_id)["active"] == false
    });
    let iq = session(addr, &iq_id);
    let iq_file = file(&iq, "iq");
    assert_eq!(iq_file["state"], "done", "{iq_file}");
    assert!(iq_file["recording_id"].is_string());
    let idir = dir.0.join("outputs").join(&iq_id);
    let meta_path = idir.join("iq.sigmf-meta");
    let meta = SigmfMeta::read(&meta_path).unwrap();
    let data_len = std::fs::metadata(idir.join("iq.sigmf-data")).unwrap().len();
    assert!(data_len > 0 && data_len <= iq_budget && data_len % 2 == 0);
    assert_eq!(meta.global.sample_rate, Some(fs));
    assert_eq!(meta.captures[0].frequency, Some(center));
    assert_eq!(meta.annotations[0].freq_lower_edge, Some(f_lo));
    assert_eq!(meta.annotations[0].freq_upper_edge, Some(f_hi));
    assert_eq!(meta.annotations[0].sample_count, Some(data_len / 2));
    let reopened = open_replay(&meta_path, Pacing::Unpaced, false).unwrap();
    assert!((reopened.info.center_hz - center).abs() < 1.0);
    assert!((reopened.info.sample_rate_hz - fs).abs() < 1.0);
    let isc = OutputSidecar::read(&idir.join("iq.json")).unwrap();
    assert_eq!(isc.ended, "max_bytes reached");
    assert_eq!(isc.stats.bytes, data_len);
    let (code, body) = get_query_token(addr, iq_file["sidecar_url"].as_str().unwrap());
    assert_eq!(code, 200);
    assert_eq!(body, std::fs::read(idir.join("iq.json")).unwrap());

    drop(server);
    drop(recorders);
    handle.stop();
    finish(handle);
}
