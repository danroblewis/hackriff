//! Regressions for the T-014 review probes, driving the real `hk-dummy-plugin`:
//! P3 (orphaned grandchild holding the pipes), P3b (stall kill of a wrapper whose child holds
//! stdin), bounded shutdown.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hk_model::sigmf::Datatype;
use hk_model::{ContentClass, Repository, SampleTime, Timestamp};
use hk_plugins::{
    Ingest, InputStreamDesc, PluginContext, PluginInstance, PluginManifest, PluginMonitor,
    PluginState,
};
use hk_stream::{BinaryRecord, RecordFlags};
use serde_json::{Value, json};

mod common;

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

fn rec(i: u64, payload: &[u8]) -> BinaryRecord<'_> {
    BinaryRecord {
        t: Timestamp::now(),
        sample_index: i,
        flags: RecordFlags::empty(),
        payload,
    }
}

fn wait(mon: &PluginMonitor, what: &str, pred: impl FnMut(&hk_plugins::PluginStats) -> bool) {
    assert!(
        mon.wait_for(Duration::from_secs(15), pred),
        "timed out waiting for {what}: {:?} {:?}",
        mon.stats(),
        mon.log_tail()
    );
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
    // Off the clock: see `common`.
    common::wait_started(&mon);
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
    // Off the clock: see `common`.
    common::wait_started(&mon);
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
    // Off the clock: see `common`.
    common::wait_started(&mon);
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
