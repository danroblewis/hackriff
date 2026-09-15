//! Plugin host tests (T-014, ADR-0003, docs/stream-contract.md §9), driving the real
//! `hk-dummy-plugin` subprocess. Use case: SIGNAL-001.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hk_model::sigmf::Datatype;
use hk_model::{
    ContentClass, CrcStatus, Decode, DecodedIdentity, EmitterId, IdentityScheme, Repository,
    SampleTime, Timestamp,
};
use hk_plugins::{
    HostError, Ingest, InputStreamDesc, ManifestError, PluginContext, PluginInstance,
    PluginManifest, PluginState, PushOutcome, RestartPolicy,
};
use hk_stream::{
    BinaryRecord, Listener, Publisher, PublisherConfig, Record, RecordFlags, StreamHeader,
    StreamKind, StreamReader,
};
use serde_json::json;

const DUMMY: &str = env!("CARGO_BIN_EXE_hk-dummy-plugin");
const RATE: f64 = 250_000.0;
const ANCHOR_NS: i64 = 1_757_000_000_000_000_000;
const ICAOS: [&str; 4] = ["a1b2c0", "a1b2c1", "a1b2c2", "a1b2c3"];

/// The repository's dummy manifest, pointed at the freshly built binary.
fn repo_manifest() -> PluginManifest {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/dummy/manifest.json");
    let mut m = PluginManifest::load(path).unwrap();
    m.executable = DUMMY.into();
    m
}

fn add_args(m: &mut PluginManifest, args: &[&str]) {
    m.args.extend(args.iter().map(|a| a.to_string()));
}

fn anchor() -> SampleTime {
    SampleTime {
        sample_index: 0,
        host_time: Timestamp::from_unix_nanos(ANCHOR_NS),
    }
}

fn input() -> InputStreamDesc {
    InputStreamDesc {
        datatype: Datatype::Cf32Le,
        sample_rate_hz: RATE,
        center_hz: Some(433.92e6),
        bandwidth_hz: Some(200e3),
        content_class: ContentClass::Unrestricted,
        anchor: anchor(),
        emitter_id: None,
        provenance_ref: None,
    }
}

/// `samples` cf32_le samples of a complex tone starting at sample `start`.
fn tone(start: u64, samples: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples * 8);
    for k in 0..samples as u64 {
        let phase = (2.0 * std::f64::consts::PI * 0.01 * (start + k) as f64) as f32;
        out.extend_from_slice(&phase.cos().to_le_bytes());
        out.extend_from_slice(&phase.sin().to_le_bytes());
    }
    out
}

fn push(
    inst: &mut PluginInstance,
    record: u64,
    payload: &[u8],
    samples: u64,
) -> (PushOutcome, Duration) {
    let t0 = Instant::now();
    let outcome = inst
        .push(BinaryRecord {
            t: anchor().time_of(record * samples, RATE),
            sample_index: record * samples,
            flags: RecordFlags::empty(),
            payload,
        })
        .unwrap();
    (outcome, t0.elapsed())
}

fn shared_ingest(repo: Repository) -> Arc<Mutex<Ingest>> {
    Arc::new(Mutex::new(Ingest::new(repo)))
}

fn wait_running(inst: &PluginInstance) {
    let mon = inst.monitor();
    assert!(
        mon.wait_for(Duration::from_secs(10), |s| s.state == PluginState::Running),
        "{:?} {:?}",
        mon.stats(),
        mon.log_tail()
    );
}

fn all_adsb_decodes(repo: &Repository) -> Vec<Decode> {
    let mut all: Vec<Decode> = ICAOS
        .iter()
        .flat_map(|icao| {
            repo.decodes_for_identity(&DecodedIdentity {
                scheme: IdentityScheme::AdsbIcao,
                value: (*icao).to_owned(),
            })
            .unwrap()
        })
        .collect();
    all.sort_by_key(|d| d.t);
    all
}

/// A short fresh directory (Unix socket paths are limited to ~104 bytes on macOS).
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hkp{tag}{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// T-223 (readiness): a manifest that declares `input.ready_signal` is **not** ready when its
/// process attaches, only when the plugin sends its `ready` line — the gap a decoder needs to set
/// itself up (the readsb wrapper's Beast connection). The host counts the records offered before
/// that line, so a producer can prove it held back. A manifest without the declaration is ready
/// as soon as it is attached, and nothing waits.
#[test]
fn readiness_waits_for_the_plugin_ready_line_and_counts_records_offered_early() {
    const SAMPLES: usize = 256;
    let mut m = repo_manifest();
    m.input.ready_signal = true;
    add_args(&mut m, &["--ready-after-ms", "700"]);
    let sink = shared_ingest(Repository::open_in_memory().unwrap());
    let mut inst =
        PluginInstance::spawn(m, input(), PluginContext::default(), Arc::clone(&sink)).unwrap();
    wait_running(&inst);
    assert!(
        !inst.stats().ready,
        "an attached process is not a ready decoder: {:?}",
        inst.stats()
    );
    // A producer that cannot pause (a live chain past its bounded wait) offers a record anyway.
    push(&mut inst, 0, &tone(0, SAMPLES), SAMPLES as u64);
    assert!(
        inst.wait_ready(Duration::from_secs(15)),
        "{:?} {:?}",
        inst.stats(),
        inst.monitor().log_tail()
    );
    let stats = inst.stats();
    assert!(stats.ready);
    assert_eq!(
        stats.records_offered_before_ready, 1,
        "the record offered before the ready line is counted: {stats:?}"
    );
    inst.shutdown();

    // No declaration: ready as soon as the process is attached, and `wait_ready` returns at once.
    let mut plain = repo_manifest();
    assert!(!plain.input.ready_signal);
    add_args(&mut plain, &["--every", "1"]);
    let inst = PluginInstance::spawn(
        plain,
        input(),
        PluginContext::default(),
        shared_ingest(Repository::open_in_memory().unwrap()),
    )
    .unwrap();
    wait_running(&inst);
    assert!(inst.wait_ready(Duration::from_secs(5)));
    assert_eq!(inst.stats().records_offered_before_ready, 0);
    inst.shutdown();
}

/// Dummy round trip: synthetic channel samples in, messages out, Decode rows in an in-memory
/// repository with the metadata/content split, host-stamped time and manifest identity.
#[test]
fn dummy_round_trip_into_repository() {
    const SAMPLES: usize = 1024;
    let mut m = repo_manifest();
    add_args(&mut m, &["--profile", "adsb-like", "--content", "hello"]);
    let sink = shared_ingest(Repository::open_in_memory().unwrap());
    let mut inst =
        PluginInstance::spawn(m, input(), PluginContext::default(), Arc::clone(&sink)).unwrap();
    wait_running(&inst);
    for i in 0..100u64 {
        let payload = tone(i * SAMPLES as u64, SAMPLES);
        assert_eq!(
            push(&mut inst, i, &payload, SAMPLES as u64).0,
            PushOutcome::Enqueued
        );
    }
    let mon = inst.monitor();
    assert!(
        mon.wait_for(Duration::from_secs(10), |s| s.decodes == 10),
        "{:?} {:?}",
        mon.stats(),
        mon.log_tail()
    );
    let stats = inst.shutdown();
    assert_eq!(stats.records_offered, 100);
    assert_eq!(stats.records_enqueued, 100);
    assert_eq!(
        (
            stats.starts,
            stats.crashes,
            stats.malformed,
            stats.class_clamped,
            stats.content_gated
        ),
        (1, 0, 0, 0, 0)
    );
    assert_eq!(stats.state, PluginState::Stopped);
    assert!(
        mon.log_tail()
            .lines
            .iter()
            .any(|l| l.contains("hk-dummy-plugin: started")),
        "stderr captured: {:?}",
        mon.log_tail()
    );
    drop(mon);

    let sink = sink.lock().unwrap();
    let decodes = all_adsb_decodes(sink.repo());
    assert_eq!(decodes.len(), 10);
    for (k, d) in decodes.iter().enumerate() {
        let n = k as u64 + 1;
        let icao = ICAOS[k % 4];
        assert_eq!(d.decoder_id, "dummy");
        assert_eq!(d.decoder_version, "0.1.0");
        assert_eq!(d.frame_model, "adsb-df17");
        assert_eq!(d.crc_status, CrcStatus::Valid);
        assert_eq!(d.content_class, ContentClass::Unrestricted);
        assert_eq!(d.identity.as_ref().unwrap().value, icao);
        assert_eq!(
            d.metadata,
            json!({"icao": icao, "df": 17, "crc": "ok", "records": n * 10})
        );
        assert_eq!(d.content, Some(json!({"text": "hello", "n": n})));
        // Record 10n (1-based) starts at sample (10n - 1) * SAMPLES.
        let index = (n * 10 - 1) * SAMPLES as u64;
        assert_eq!(d.t, anchor().time_of(index, RATE), "host-stamped time");
    }
}

/// T-103: `finish` ends the input without losing it. A plugin that has not started reading yet
/// still gets every queued record, then EOF; the host waits for it to flush and exit (a clean
/// exit, not restarted) instead of killing it after a fixed settle time.
#[test]
fn finish_delivers_queued_input_to_a_slow_starting_plugin_then_waits_for_its_exit() {
    const SAMPLES: usize = 1024;
    let mut m = repo_manifest();
    add_args(
        &mut m,
        &[
            "--profile",
            "adsb-like",
            "--every",
            "1",
            "--start-delay-ms",
            "2500",
        ],
    );
    let sink = shared_ingest(Repository::open_in_memory().unwrap());
    let mut inst =
        PluginInstance::spawn(m, input(), PluginContext::default(), Arc::clone(&sink)).unwrap();
    wait_running(&inst);
    for i in 0..20u64 {
        let payload = tone(i * SAMPLES as u64, SAMPLES);
        assert_eq!(
            push(&mut inst, i, &payload, SAMPLES as u64).0,
            PushOutcome::Enqueued
        );
    }
    let t0 = Instant::now();
    let stats = inst.finish(Duration::from_secs(30));
    assert_eq!(stats.decodes, 20, "{stats:?}");
    assert_eq!(
        (
            stats.starts,
            stats.restarts,
            stats.crashes,
            stats.clean_exits
        ),
        (1, 0, 0, 1),
        "{stats:?}"
    );
    assert_eq!(stats.state, PluginState::Stopped);
    assert!(
        t0.elapsed() < Duration::from_secs(20),
        "finish returned on the plugin's exit, not on the idle timeout: {:?}",
        t0.elapsed()
    );
    let sink = sink.lock().unwrap();
    let decodes = all_adsb_decodes(sink.repo());
    assert_eq!(decodes.len(), 20);
    assert_eq!(
        decodes[0].metadata["records"],
        json!(1),
        "first record decoded"
    );
}

/// A plugin that crashes mid-stream is restarted with backoff until the crash-loop cap; the
/// producer never blocks, and every pushed record is counted in exactly one bucket.
#[test]
fn plugin_crash_restarts_and_capture_never_blocks() {
    const SAMPLES: usize = 256;
    let mut m = repo_manifest();
    m.params.insert("every".into(), "5".into());
    add_args(&mut m, &["--profile", "adsb-like", "--crash-after", "25"]);
    m.restart = RestartPolicy {
        backoff_initial: Duration::from_millis(10),
        backoff_max: Duration::from_millis(200),
        max_restarts: 3,
        window: Duration::from_secs(60),
    };
    let sink = shared_ingest(Repository::open_in_memory().unwrap());
    let mut inst =
        PluginInstance::spawn(m, input(), PluginContext::default(), Arc::clone(&sink)).unwrap();
    let mon = inst.monitor();
    let payload = tone(0, SAMPLES);
    let start = Instant::now();
    let mut worst = Duration::ZERO;
    let mut pushed = 0u64;
    while mon.stats().state != PluginState::Failed {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "{:?} {:?}",
            mon.stats(),
            mon.log_tail()
        );
        let (_, dt) = push(&mut inst, pushed, &payload, SAMPLES as u64);
        worst = worst.max(dt);
        pushed += 1;
        if pushed % 32 == 0 {
            thread::sleep(Duration::from_millis(1));
        }
    }
    // Crash loop reached: no process, so every further record is dropped-detached, exactly.
    let before = inst.stats();
    for j in 0..1000 {
        let (outcome, dt) = push(&mut inst, pushed + j, &payload, SAMPLES as u64);
        assert_eq!(outcome, PushOutcome::DroppedDetached);
        worst = worst.max(dt);
    }
    let after = inst.shutdown();
    drop(mon);
    assert_eq!(
        after.records_dropped_detached - before.records_dropped_detached,
        1000
    );
    assert_eq!(after.records_offered, pushed + 1000);
    assert_eq!(
        after.records_offered,
        after.records_enqueued + after.records_dropped_full + after.records_dropped_detached
    );
    assert_eq!(
        (
            after.starts,
            after.crashes,
            after.restarts,
            after.clean_exits
        ),
        (4, 4, 3, 0)
    );
    assert_eq!(after.state, PluginState::Failed);
    assert_eq!(after.last_exit.as_deref(), Some("exit code 101"));
    assert_eq!(after.decodes, 20, "5 decodes from each of the 4 runs");
    assert_eq!(all_adsb_decodes(sink.lock().unwrap().repo()).len(), 20);
    println!(
        "pushed {} records across 4 crashes in {:?}; worst push {worst:?}",
        after.records_offered,
        start.elapsed()
    );
    assert!(
        worst < Duration::from_millis(100),
        "push blocked: {worst:?}"
    );
}

/// A plugin that never reads its input: records are dropped and counted, the producer keeps
/// going, and the hang watchdog kills the plugin after `stall_timeout`.
#[test]
fn stalled_plugin_is_dropped_counted_and_killed() {
    const SAMPLES: usize = 512;
    let mut m = repo_manifest();
    add_args(&mut m, &["--stall"]);
    m.limits.input_queue_bytes = 64 * 1024;
    m.limits.stall_timeout = Duration::from_millis(200);
    m.restart = RestartPolicy {
        backoff_initial: Duration::from_millis(10),
        backoff_max: Duration::from_millis(100),
        max_restarts: 0,
        window: Duration::from_secs(60),
    };
    let sink = shared_ingest(Repository::open_in_memory().unwrap());
    let mut inst = PluginInstance::spawn(m, input(), PluginContext::default(), sink).unwrap();
    wait_running(&inst);
    let mon = inst.monitor();
    let payload = tone(0, SAMPLES);
    let start = Instant::now();
    let mut worst = Duration::ZERO;
    let mut i = 0u64;
    while mon.stats().state != PluginState::Failed {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "{:?}",
            mon.stats()
        );
        let (_, dt) = push(&mut inst, i, &payload, SAMPLES as u64);
        worst = worst.max(dt);
        i += 1;
        thread::sleep(Duration::from_micros(200));
    }
    let s = inst.shutdown();
    assert!(s.records_enqueued > 0);
    assert!(s.records_dropped_full > 0, "{s:?}");
    assert_eq!(
        s.records_offered,
        s.records_enqueued + s.records_dropped_full + s.records_dropped_detached
    );
    assert_eq!((s.stall_kills, s.crashes), (1, 1));
    assert!(
        worst < Duration::from_millis(100),
        "push blocked: {worst:?}"
    );
    assert!(
        mon.log_tail()
            .lines
            .iter()
            .any(|l| l.contains("stall_timeout")),
        "{:?}",
        mon.log_tail()
    );
}

/// SIGNAL-001 (ADS-B / Mode S baseline): an ADS-B-like plugin's ICAO-hex decodes land as
/// Decode rows and are republished on a T-016 messages stream to an external consumer.
#[test]
fn signal_001_adsb_like_plugin_decodes_land_and_are_republished() {
    let dir = temp_dir("s1");
    let path = dir.join("adsb.sock");
    let mut header = StreamHeader::new(
        "decodes/adsb",
        StreamKind::Messages,
        ContentClass::Unrestricted,
        "hk-plugins:dummy@0.1.0",
    );
    header.message_schema = Some("hackriff.decode/1".into());
    header.max_frame_len = 64 * 1024;
    let publisher = Publisher::new(
        header,
        PublisherConfig {
            queue_bytes: 1024 * 1024,
            ..PublisherConfig::default()
        },
    )
    .unwrap();
    let handle = publisher.handle();
    let listener = Listener::bind_uds(&path, handle.clone()).unwrap();
    let consumer_path = path.clone();
    let consumer = thread::spawn(move || {
        let mut reader = StreamReader::connect_uds(&consumer_path).unwrap();
        let header = reader.read_header().unwrap().clone();
        let mut records = Vec::new();
        while let Some(r) = reader.next_record().unwrap() {
            records.push(r);
        }
        (header, records)
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while handle.open_consumers() == 0 {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }

    let sink = Arc::new(Mutex::new(Ingest::with_republish(
        Repository::open_in_memory().unwrap(),
        publisher,
    )));
    let emitter = EmitterId::new();
    let mut m = repo_manifest();
    add_args(&mut m, &["--profile", "adsb-like"]);
    let context = PluginContext {
        emitter_ref: Some(emitter),
        ..PluginContext::default()
    };
    let mut inst = PluginInstance::spawn(m, input(), context, Arc::clone(&sink)).unwrap();
    wait_running(&inst);
    for i in 0..80 {
        assert_eq!(
            push(&mut inst, i, &tone(i * 512, 512), 512).0,
            PushOutcome::Enqueued
        );
    }
    assert!(
        inst.monitor()
            .wait_for(Duration::from_secs(10), |s| s.decodes == 8)
    );
    inst.shutdown();
    let mut ingest = Arc::try_unwrap(sink)
        .ok()
        .expect("host released the ingest")
        .into_inner()
        .unwrap();
    drop(ingest.take_publisher());
    let (stream_header, records) = consumer.join().unwrap();
    drop(listener);

    let decodes = all_adsb_decodes(ingest.repo());
    assert_eq!(decodes.len(), 8);
    for icao in ICAOS {
        let rows = ingest
            .repo()
            .decodes_for_identity(&DecodedIdentity {
                scheme: IdentityScheme::AdsbIcao,
                value: icao.into(),
            })
            .unwrap();
        assert_eq!(rows.len(), 2, "{icao}");
        assert!(rows.iter().all(|d| d.crc_status == CrcStatus::Valid));
    }

    assert_eq!(stream_header.kind, StreamKind::Messages);
    assert_eq!(stream_header.content_class, ContentClass::Unrestricted);
    assert_eq!(records.len(), 8);
    for (i, r) in records.iter().enumerate() {
        let Record::Message(msg) = r else {
            panic!("{r:?}")
        };
        assert_eq!(msg.seq, i as u64);
        assert_eq!(msg.content_class, ContentClass::Unrestricted);
        assert_eq!(msg.value["emitter_id"], emitter.to_string());
        assert_eq!(msg.value["identity"]["scheme"], "adsb-icao");
        assert_eq!(msg.value["decoder"], "dummy@0.1.0");
        let stored = decodes
            .iter()
            .find(|d| msg.value["decode_id"] == d.id.to_string())
            .expect("republished message names a stored Decode row");
        assert_eq!(
            msg.value["identity"]["value"],
            stored.identity.as_ref().unwrap().value
        );
        assert_eq!(msg.value["metadata"]["icao"], stored.metadata["icao"]);
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// Inputs the manifest does not accept are refused before anything is spawned.
#[test]
fn input_not_accepted_by_the_manifest_is_refused() {
    let mut wrong_rate = input();
    wrong_rate.sample_rate_hz = 2e6;
    let err = PluginInstance::spawn(
        repo_manifest(),
        wrong_rate,
        PluginContext::default(),
        shared_ingest(Repository::open_in_memory().unwrap()),
    )
    .err()
    .expect("refused");
    assert!(matches!(
        err,
        HostError::Manifest(ManifestError::Mismatch(_))
    ));
}

/// A missing executable counts spawn failures, hits the crash-loop cap, and drops input.
#[test]
fn missing_executable_fails_without_blocking_input() {
    let mut m = repo_manifest();
    m.executable = "/nonexistent/hk-no-such-plugin".into();
    m.restart = RestartPolicy {
        backoff_initial: Duration::from_millis(5),
        backoff_max: Duration::from_millis(20),
        max_restarts: 1,
        window: Duration::from_secs(60),
    };
    let mut inst = PluginInstance::spawn(
        m,
        input(),
        PluginContext::default(),
        shared_ingest(Repository::open_in_memory().unwrap()),
    )
    .unwrap();
    assert!(
        inst.monitor()
            .wait_for(Duration::from_secs(10), |s| s.state == PluginState::Failed)
    );
    let payload = tone(0, 64);
    assert_eq!(
        push(&mut inst, 0, &payload, 64).0,
        PushOutcome::DroppedDetached
    );
    let s = inst.shutdown();
    assert_eq!((s.spawn_failures, s.starts), (2, 0));
    assert_eq!(s.records_dropped_detached, 1);
}
