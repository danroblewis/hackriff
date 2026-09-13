//! Regressions for the T-014 review probes, driving the real `hk-dummy-plugin`:
//! P1 (input class ceiling), P2 (content smuggled outside `content`), P3 (orphaned grandchild
//! holding the pipes), P3b (stall kill of a wrapper whose child holds stdin), bounded shutdown.

use std::io::Read;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hk_model::sigmf::Datatype;
use hk_model::{
    ContentClass, DecodeId, DecodedIdentity, FreqRange, IdentityScheme, Region, Repository,
    SampleTime, TimeRange, Timestamp,
};
use hk_plugins::{
    Ingest, InputStreamDesc, PluginContext, PluginInstance, PluginManifest, PluginMonitor,
    PluginState,
};
use hk_stream::{
    BinaryRecord, ListenAddr, Listener, Publisher, PublisherConfig, Record, RecordFlags,
    StreamHeader, StreamKind, StreamReader,
};
use serde_json::{Value, json};

const DUMMY: &str = env!("CARGO_BIN_EXE_hk-dummy-plugin");
const RATE: f64 = 250_000.0;

fn manifest(output: Value, args: &[&str], extra: Value) -> PluginManifest {
    let mut v = json!({
        "manifest_version": 1, "id": "dummy", "version": "0.1.0",
        "licence": "LicenseRef-hackriff-undecided",
        "executable": DUMMY, "args": args,
        "input": {"kind": "channel", "datatype": "cf32_le"},
        "output": output,
        "restart": {"backoff_initial_ms": 10, "backoff_max_ms": 100, "max_restarts": 3, "window_s": 60}
    });
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    PluginManifest::from_json_str(&v.to_string()).unwrap()
}

fn unrestricted() -> Value {
    json!({"schema_id": "hackriff.dummy/1", "content_class": "unrestricted"})
}

fn pager_output() -> Value {
    json!({
        "schema_id": "hackriff.dummy/1", "content_class": "restricted-paging",
        "metadata_keys": {"capcode": {"type": "digits", "max_len": 7}, "function": {"type": "integer"}},
        "frame_models": ["pocsag"], "labels": ["pocsag"],
        "identity": {"scheme": "other:pocsag-capcode", "charset": "digits", "max_len": 7}
    })
}

fn input(class: ContentClass) -> InputStreamDesc {
    InputStreamDesc {
        datatype: Datatype::Cf32Le,
        sample_rate_hz: RATE,
        center_hz: Some(152e6),
        bandwidth_hz: Some(25e3),
        content_class: class,
        anchor: SampleTime {
            sample_index: 0,
            host_time: Timestamp::now(),
        },
        emitter_id: None,
        provenance_ref: None,
    }
}

fn region_context() -> PluginContext {
    PluginContext {
        region: Some(Region {
            freq: FreqRange {
                lo_hz: 151.9e6,
                hi_hz: 152.1e6,
            },
            time: TimeRange {
                start: Timestamp::from_unix_nanos(0),
                end: Timestamp::from_unix_nanos(i64::MAX / 2),
            },
        }),
        ..PluginContext::default()
    }
}

fn rec(i: u64, payload: &[u8]) -> BinaryRecord<'_> {
    BinaryRecord {
        t: Timestamp::now(),
        sample_index: i,
        flags: RecordFlags::empty(),
        payload,
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hkq{tag}{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

/// A TCP messages stream (unrestricted header, so only record classes gate) plus a consumer
/// that reads every raw byte until EOF.
struct Wire {
    listener: Listener,
    consumer: thread::JoinHandle<Vec<u8>>,
}

fn tcp_republish() -> (Publisher, Wire) {
    let mut header = StreamHeader::new(
        "decodes/review",
        StreamKind::Messages,
        ContentClass::Unrestricted,
        "review",
    );
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
    let listener = Listener::bind_tcp("127.0.0.1:0", handle.clone()).unwrap();
    let ListenAddr::Tcp(addr) = listener.addr().clone() else {
        unreachable!()
    };
    let consumer = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = TcpStream::connect(addr).unwrap().read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while handle.open_consumers() == 0 {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    (publisher, Wire { listener, consumer })
}

/// Stops the host, finishes the stream and returns (ingest, wire bytes, database bytes).
fn finish(
    inst: PluginInstance,
    sink: Arc<Mutex<Ingest>>,
    wire: Wire,
    dir: &std::path::Path,
) -> (Ingest, Vec<u8>, Vec<u8>) {
    inst.shutdown();
    let mut ingest = Arc::try_unwrap(sink)
        .ok()
        .expect("host released the ingest")
        .into_inner()
        .unwrap();
    drop(ingest.take_publisher());
    let bytes = wire.consumer.join().unwrap();
    drop(wire.listener);
    ingest.repo_mut().checkpoint().unwrap();
    let mut db = std::fs::read(dir.join("hk.sqlite")).unwrap();
    if let Ok(wal) = std::fs::read(dir.join("hk.sqlite-wal")) {
        db.extend(wal);
    }
    (ingest, bytes, db)
}

fn republished_decode_ids(wire: &[u8]) -> Vec<DecodeId> {
    let mut reader = StreamReader::new(wire);
    let mut ids = Vec::new();
    while let Some(r) = reader.next_record().unwrap() {
        if let Record::Message(m) = r
            && let Some(id) = m.value["decode_id"].as_str()
        {
            ids.push(id.parse().unwrap());
        }
    }
    ids
}

fn wait(mon: &PluginMonitor, what: &str, pred: impl FnMut(&hk_plugins::PluginStats) -> bool) {
    assert!(
        mon.wait_for(Duration::from_secs(15), pred),
        "timed out waiting for {what}: {:?} {:?}",
        mon.stats(),
        mon.log_tail()
    );
}

/// P1: a restricted-paging channel fed to a plugin whose manifest says unrestricted. The input
/// class is a ceiling: no content, and (no allowlist) no free metadata, reaches SQLite or TCP.
#[test]
fn p1_restricted_input_class_is_a_ceiling_for_an_unrestricted_manifest() {
    const SENT: &str = "SENTINEL-PAGER-XYZ";
    let dir = temp_dir("p1");
    let (publisher, wire) = tcp_republish();
    let sink = Arc::new(Mutex::new(Ingest::with_republish(
        Repository::open(dir.join("hk.sqlite")).unwrap(),
        publisher,
    )));
    let m = manifest(
        unrestricted(),
        &["--every", "1", "--profile", "adsb-like", "--content", SENT],
        json!({}),
    );
    let mut inst = PluginInstance::spawn(
        m,
        input(ContentClass::RestrictedPaging),
        region_context(),
        Arc::clone(&sink),
    )
    .unwrap();
    let mon = inst.monitor();
    wait(&mon, "running", |s| s.state == PluginState::Running);
    for i in 0..5 {
        inst.push(rec(i, &[0u8; 64])).unwrap();
    }
    wait(&mon, "5 decodes", |s| s.decodes == 5);
    let stats = mon.stats();
    let log = mon.log_tail();
    drop(mon);
    let (ingest, bytes, db) = finish(inst, sink, wire, &dir);

    assert_eq!(stats.content_ceiling, ContentClass::RestrictedPaging);
    assert_eq!(stats.content_gated, 5);
    assert!(stats.metadata_sanitized >= 5 * 6, "{stats:?}");
    assert!(!contains(&db, SENT), "content persisted");
    assert!(!contains(&bytes, SENT), "content streamed");
    assert!(
        !contains(&db, "a1b2c0") && !contains(&bytes, "a1b2c0"),
        "unallowlisted identity"
    );
    assert_eq!(log.content_class, ContentClass::RestrictedPaging);
    assert!(stats.stderr_lines_withheld >= 1);
    assert!(
        !log.lines.iter().any(|l| l.contains("hk-dummy-plugin")),
        "{log:?}"
    );
    let ids = republished_decode_ids(&bytes);
    assert_eq!(ids.len(), 5);
    for id in ids {
        let d = ingest.repo().decode(id).unwrap();
        assert_eq!(d.content_class, ContentClass::RestrictedPaging);
        assert_eq!(d.content, None);
        assert_eq!(d.metadata, json!({}));
        assert_eq!(d.frame_model, "hackriff.dummy/1");
        assert_eq!(d.identity, None);
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// P2: under a restricted-paging manifest, text placed in metadata, frame_model, identity,
/// annotation label, log lines, error echoes and stderr never reaches SQLite, the socket or the
/// log ring; allowlisted typed metadata and identity do. The unrestricted run is the positive
/// control that the scans can see the token.
#[test]
fn p2_content_smuggled_outside_content_never_leaves() {
    const TOKEN: &str = "SMUGTOK";
    for gated in [true, false] {
        let dir = temp_dir(if gated { "p2g" } else { "p2u" });
        let (publisher, wire) = tcp_republish();
        let sink = Arc::new(Mutex::new(Ingest::with_republish(
            Repository::open(dir.join("hk.sqlite")).unwrap(),
            publisher,
        )));
        let output = if gated {
            pager_output()
        } else {
            unrestricted()
        };
        let m = manifest(output, &["--smuggle", TOKEN], json!({}));
        let inst = PluginInstance::spawn(
            m,
            input(ContentClass::Unrestricted),
            region_context(),
            Arc::clone(&sink),
        )
        .unwrap();
        let mon = inst.monitor();
        wait(&mon, "smuggle lines", |s| {
            s.decodes == 2 && s.annotations == 1 && s.malformed == 3
        });
        // stderr is read asynchronously: wait for both stderr lines (started + token).
        if gated {
            wait(&mon, "stderr withheld", |s| {
                s.stderr_lines_withheld >= 2 && s.log_lines_withheld == 1
            });
        } else {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !mon.log_tail().lines.iter().any(|l| l.contains(TOKEN)) {
                assert!(Instant::now() < deadline, "{:?}", mon.log_tail());
                thread::sleep(Duration::from_millis(2));
            }
        }
        let stats = mon.stats();
        let log = mon.log_tail();
        drop(mon);
        let (ingest, bytes, db) = finish(inst, sink, wire, &dir);
        let ring = log.lines.join("\n");

        if gated {
            assert!(!contains(&db, TOKEN), "token persisted");
            assert!(!contains(&bytes, TOKEN), "token streamed");
            assert!(!ring.contains(TOKEN), "token in log ring: {ring}");
            assert_eq!(log.content_class, ContentClass::RestrictedPaging);
            assert!(stats.metadata_sanitized >= 6, "{stats:?}");
            assert_eq!(
                stats.content_gated, 2,
                "decode + annotation content refused"
            );
            // The allowlisted, typed values survive storage (checked on the file: T-036 gates
            // restricted rows on every repository read, and a lookup by the restricted identity
            // finds nothing).
            let rows = ingest
                .repo()
                .decodes_for_identity(&DecodedIdentity {
                    scheme: IdentityScheme::Other("pocsag-capcode".into()),
                    value: "1234567".into(),
                })
                .unwrap();
            assert!(
                rows.is_empty(),
                "a restricted identity lookup confirms nothing"
            );
            for kept in [
                "\"frame_model\":\"pocsag\"",
                "\"function\":2",
                "\"capcode\":\"1234567\"",
                "\"value\":\"1234567\"",
            ] {
                assert!(contains(&db, kept), "{kept} stored");
            }
            let ids = republished_decode_ids(&bytes);
            let metas: Vec<Value> = ids
                .iter()
                .map(|id| ingest.repo().decode(*id).unwrap().metadata)
                .collect();
            assert!(metas.iter().all(|m| *m == json!({})), "{metas:?}");
        } else {
            assert!(contains(&db, TOKEN), "positive control: DB scan");
            assert!(contains(&bytes, TOKEN), "positive control: wire scan");
            assert!(ring.contains(TOKEN), "positive control: log ring");
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}

fn grandchild_pids(mon: &PluginMonitor) -> Vec<i32> {
    mon.log_tail()
        .lines
        .iter()
        .filter_map(|l| l.strip_prefix("hk-dummy-plugin: grandchild pid "))
        .map(|p| p.trim().parse().unwrap())
        .collect()
}

fn assert_gone(pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    // SAFETY: signal 0 only checks for existence.
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(Instant::now() < deadline, "grandchild {pid} still alive");
        thread::sleep(Duration::from_millis(10));
    }
}

/// P3: a wrapper that crashes leaving a grandchild holding stdin/stdout/stderr. The host kills
/// the process group, restarts promptly, and shuts down in bounded time.
#[test]
fn p3_orphaned_grandchild_does_not_hang_restart_or_shutdown() {
    let m = manifest(unrestricted(), &["--orphan"], json!({}));
    let sink = Arc::new(Mutex::new(Ingest::new(
        Repository::open_in_memory().unwrap(),
    )));
    let inst = PluginInstance::spawn(
        m,
        input(ContentClass::Unrestricted),
        PluginContext::default(),
        sink,
    )
    .unwrap();
    let mon = inst.monitor();
    let t0 = Instant::now();
    wait(&mon, "crash loop", |s| s.state == PluginState::Failed);
    let restart_time = t0.elapsed();
    let s = mon.stats();
    assert_eq!((s.starts, s.crashes, s.restarts), (4, 4, 3), "{s:?}");
    assert_eq!(s.readers_abandoned, 0, "group kill released the pipes");
    assert!(restart_time < Duration::from_secs(5), "{restart_time:?}");
    let pids = grandchild_pids(&mon);
    assert_eq!(pids.len(), 4, "{:?}", mon.log_tail());
    for pid in pids {
        assert_gone(pid);
    }
    let t1 = Instant::now();
    inst.shutdown();
    assert!(t1.elapsed() < Duration::from_secs(2));
    println!("P3: 4 orphaning runs to crash-loop cap in {restart_time:?}");
}

/// P3b: the stall watchdog kills a wrapper whose grandchild holds stdin and never reads; the
/// blocked writer is released, the plugin restarts, and pushes never block.
#[test]
fn p3b_stall_kill_of_a_wrapper_restarts_promptly() {
    let m = manifest(
        unrestricted(),
        &["--stall-child"],
        json!({
            "restart": {"backoff_initial_ms": 10, "backoff_max_ms": 100, "max_restarts": 1, "window_s": 60},
            "limits": {"input_queue_bytes": 65536, "stall_timeout_ms": 300}
        }),
    );
    let sink = Arc::new(Mutex::new(Ingest::new(
        Repository::open_in_memory().unwrap(),
    )));
    let mut inst = PluginInstance::spawn(
        m,
        input(ContentClass::Unrestricted),
        PluginContext::default(),
        sink,
    )
    .unwrap();
    let mon = inst.monitor();
    wait(&mon, "running", |s| s.state == PluginState::Running);
    let payload = [0u8; 4096];
    let t0 = Instant::now();
    let mut worst = Duration::ZERO;
    let mut i = 0;
    while mon.stats().state != PluginState::Failed {
        assert!(t0.elapsed() < Duration::from_secs(15), "{:?}", mon.stats());
        let t = Instant::now();
        inst.push(rec(i, &payload)).unwrap();
        worst = worst.max(t.elapsed());
        i += 1;
        thread::sleep(Duration::from_micros(500));
    }
    let s = mon.stats();
    assert_eq!((s.starts, s.stall_kills, s.crashes), (2, 2, 2), "{s:?}");
    assert_eq!(
        s.records_offered,
        s.records_enqueued + s.records_dropped_full + s.records_dropped_detached
    );
    assert!(
        worst < Duration::from_millis(100),
        "push blocked: {worst:?}"
    );
    for pid in grandchild_pids(&mon) {
        assert_gone(pid);
    }
    let t1 = Instant::now();
    inst.shutdown();
    assert!(t1.elapsed() < Duration::from_secs(2));
    println!(
        "P3b: 2 stall kills to crash-loop cap in {:?}, worst push {worst:?}",
        t0.elapsed()
    );
}

/// Shutdown of a running wrapper with a grandchild holding every pipe is bounded.
#[test]
fn shutdown_with_a_live_grandchild_is_bounded() {
    let m = manifest(unrestricted(), &["--stall-child"], json!({}));
    let sink = Arc::new(Mutex::new(Ingest::new(
        Repository::open_in_memory().unwrap(),
    )));
    let inst = PluginInstance::spawn(
        m,
        input(ContentClass::Unrestricted),
        PluginContext::default(),
        sink,
    )
    .unwrap();
    let mon = inst.monitor();
    wait(&mon, "grandchild started", |_| {
        !grandchild_pids(&mon).is_empty()
    });
    let pids = grandchild_pids(&mon);
    let t0 = Instant::now();
    let s = inst.shutdown();
    assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    assert_eq!(s.state, PluginState::Stopped);
    for pid in pids {
        assert_gone(pid);
    }
}
