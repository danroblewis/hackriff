//! T-556 spike: the wrapped gr-lora_sdr driven through the REAL plugin host.
//!
//! `t556-host-check <fixture-stem> [--realtime]` loads `../manifest.json`, spawns it with
//! `hk_plugins::PluginInstance`, pushes the cf32 fixture as host records, SIGKILLs the child
//! mid-run to exercise supervision, pushes the fixture again, ends input with `finish`, and scores
//! the host-stamped, republished decodes BLIND against `<stem>.truth.json`: right payload, CRC
//! valid, and host `t_ns` within half a symbol of the true frame start. Prints one JSON object.
//!
//! SPIKE CODE. Not product, not a test in the workspace.

use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hk_model::sigmf::Datatype;
use hk_model::{ContentClass, Repository, SampleTime, Timestamp};
use hk_plugins::{
    Ingest, InputStreamDesc, PluginContext, PluginInstance, PluginManifest, PluginState,
};
use hk_stream::{
    BinaryRecord, Listener, Publisher, PublisherConfig, Record, RecordFlags, StreamHeader,
    StreamKind, StreamReader,
};
use serde_json::{Value, json};

const RECORD_SAMPLES: usize = 8192;

fn push_fixture(
    inst: &mut PluginInstance,
    x: &[u8],
    start: u64,
    anchor: SampleTime,
    fs: f64,
    realtime: bool,
) -> (u64, u64) {
    let t0 = Instant::now();
    let mut sample = start;
    let mut dropped = 0;
    for chunk in x.chunks(RECORD_SAMPLES * 8) {
        let out = inst
            .push(BinaryRecord {
                t: anchor.time_of(sample, fs),
                sample_index: sample,
                flags: RecordFlags::empty(),
                payload: chunk,
            })
            .expect("push");
        if !matches!(out, hk_plugins::PushOutcome::Enqueued) {
            dropped += 1;
        }
        sample += (chunk.len() / 8) as u64;
        if realtime {
            let due = Duration::from_secs_f64((sample - start) as f64 / fs);
            if let Some(d) = due.checked_sub(t0.elapsed()) {
                thread::sleep(d);
            }
        }
    }
    (sample, dropped)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let stem = &args[1];
    let realtime = args.iter().any(|a| a == "--realtime");
    let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let manifest = PluginManifest::load(here.join("manifest.json")).expect("manifest loads");

    let truth: Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{stem}.truth.json")).unwrap())
            .unwrap();
    let fs = truth["sample_rate_hz"].as_f64().unwrap();
    let sf = truth["sf"].as_u64().unwrap();
    let bw = truth["bw_hz"].as_f64().unwrap();
    let x = std::fs::read(format!("{stem}.cf32")).unwrap();
    let n = (x.len() / 8) as u64;

    // Republish decodes on a local stream so we see exactly what a consumer of the host sees.
    let dir = std::env::temp_dir().join(format!("t556{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("lora.sock");
    let mut header = StreamHeader::new(
        "decodes/lora",
        StreamKind::Messages,
        ContentClass::Unrestricted,
        "hk-plugins:gr-lora-sdr@spike",
    );
    header.message_schema = Some("hackriff.decode/1".into());
    header.max_frame_len = 64 * 1024;
    let publisher = Publisher::new(
        header,
        PublisherConfig {
            queue_bytes: 4 << 20,
            ..PublisherConfig::default()
        },
    )
    .unwrap();
    let handle = publisher.handle();
    let listener = Listener::bind_uds(&sock, handle.clone()).unwrap();
    let sock2 = sock.clone();
    let consumer = thread::spawn(move || {
        let mut reader = StreamReader::connect_uds(&sock2).unwrap();
        reader.read_header().unwrap();
        let mut out = Vec::new();
        while let Some(r) = reader.next_record().unwrap() {
            if let Record::Message(m) = r {
                out.push((Instant::now(), m.value));
            }
        }
        out
    });
    while handle.open_consumers() == 0 {
        thread::sleep(Duration::from_millis(1));
    }
    let sink = Arc::new(Mutex::new(Ingest::with_republish(
        Repository::open_in_memory().unwrap(),
        publisher,
    )));

    let anchor = SampleTime {
        sample_index: 0,
        host_time: Timestamp::now(),
    };
    let input = InputStreamDesc {
        datatype: Datatype::Cf32Le,
        sample_rate_hz: fs,
        center_hz: Some(868.1e6),
        bandwidth_hz: Some(fs),
        content_class: ContentClass::Unrestricted,
        anchor,
        emitter_id: None,
        provenance_ref: None,
    };
    let t_spawn = Instant::now();
    let mut inst =
        PluginInstance::spawn(manifest, input, PluginContext::default(), Arc::clone(&sink))
            .expect("spawn");
    let mon = inst.monitor();
    assert!(
        mon.wait_for(Duration::from_secs(120), |s| s.ready),
        "{:?}",
        mon.log_tail()
    );
    let ready_s = t_spawn.elapsed().as_secs_f64();
    let first_byte_s = inst.first_output_at().map(|t| (t - t_spawn).as_secs_f64());

    // Pass 1.
    let feed1 = Instant::now();
    let (end1, dropped1) = push_fixture(&mut inst, &x, 0, anchor, fs, realtime);
    let feed1_s = feed1.elapsed().as_secs_f64();
    let got1 = mon.wait_for(Duration::from_secs(30), |s| s.decodes >= 11);
    let first_decode_s = t_spawn.elapsed().as_secs_f64(); // upper bound; refined from consumer
    let decodes1 = mon.stats().decodes;

    // Kill the child (SIGKILL, as an OOM or crash would) and time the supervised restart.
    let t_kill = Instant::now();
    let _ = Command::new("pkill")
        .args(["-9", "-f", "hk_gr_lora.py"])
        .status();
    let restarted = mon.wait_for(Duration::from_secs(60), |s| {
        s.restarts >= 1 && s.ready && s.state == PluginState::Running
    });
    let restart_ready_s = t_kill.elapsed().as_secs_f64();

    // Pass 2: continue the host sample index where pass 1 ended.
    let (_end2, dropped2) = push_fixture(&mut inst, &x, end1, anchor, fs, realtime);
    let got2 = mon.wait_for(Duration::from_secs(30), |s| s.decodes >= decodes1 + 11);
    let stats = inst.finish(Duration::from_secs(20));
    let log_tail: Vec<String> = mon.log_tail().lines.iter().rev().take(6).cloned().collect();
    drop(mon); // the monitor holds the host's Arc<Ingest>

    let mut ingest = Arc::try_unwrap(sink).ok().unwrap().into_inner().unwrap();
    drop(ingest.take_publisher());
    let msgs = consumer.join().unwrap();
    drop(listener);

    // Blind scoring against host time.
    let tol_ns = (0.5 * (1u64 << sf) as f64 / bw * 1e9) as i64;
    let mut found = 0;
    let mut errs_ns = Vec::new();
    let mut expected = 0;
    for pass in 0..2u64 {
        for f in truth["frames"].as_array().unwrap() {
            expected += 1;
            let host_start = pass * end1 + f["start_sample"].as_u64().unwrap();
            let t_true = anchor.time_of(host_start, fs).as_unix_nanos();
            let hit = msgs.iter().find(|(_, m)| {
                m["content"]["payload_text"] == f["payload"]
                    && m["crc_status"] == "valid"
                    && (m["t_ns"].as_i64().unwrap_or(0) - t_true).abs() <= tol_ns
            });
            if let Some((_, m)) = hit {
                found += 1;
                errs_ns.push(m["t_ns"].as_i64().unwrap() - t_true);
            }
        }
    }
    let _ = n;
    let out = json!({
        "realtime": realtime,
        "ready_s": ready_s, "first_byte_s": first_byte_s,
        "feed1_s": feed1_s, "pass1_decodes": decodes1, "pass1_all": got1,
        "first_decode_upper_bound_s": first_decode_s,
        "restart_to_ready_s": restart_ready_s, "restarted": restarted, "pass2_all": got2,
        "records_dropped": dropped1 + dropped2,
        "republished": msgs.len(), "expected": expected, "found_blind": found,
        "t_ns_err": {"min": errs_ns.iter().min(), "max": errs_ns.iter().max(), "tol": tol_ns},
        "stats": format!("{stats:?}"),
        "log_tail": log_tail,
        "sample_msg": msgs.first().map(|(_, m)| m.clone()),
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
}
