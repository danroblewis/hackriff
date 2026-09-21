//! T-540: the hang watchdog's two budgets.
//!
//! The host used to have one budget for "the input queue is full and staying full", ~10 s, and
//! killed the plugin group when it expired. That reading is wrong for a child that has not run
//! yet: on macOS a freshly linked binary is `posix_spawn`ed in ~193 µs and then executes
//! **nothing** for up to ~30 s while the loader/code-signing path warms (measured in T-493, see
//! `common/mod.rs`). The single budget therefore killed healthy decoders on their first launch
//! after a rebuild or install — and a blind bump to 60 s would have bought that back by delaying
//! every genuine hang by 50 s.
//!
//! The fix is observational: one byte on the child's stdout or stderr is evidence it reached its
//! first instruction, and the budgets are separated on it. These tests pin both halves and the
//! boundary between them — a slow starter is not killed, a plugin that starts and then wedges is
//! killed on the **short** budget, and one that never runs at all is killed on the long one, each
//! reported by name.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hk_model::sigmf::Datatype;
use hk_model::{ContentClass, Repository, SampleTime, Timestamp};
use hk_plugins::{
    HangBudget, Ingest, InputStreamDesc, PluginContext, PluginInstance, PluginManifest,
    PluginMonitor, PluginState,
};
use hk_stream::{BinaryRecord, RecordFlags};
use serde_json::{Value, json};

mod common;

const DUMMY: &str = env!("CARGO_BIN_EXE_hk-dummy-plugin");
const RATE: f64 = 250_000.0;
/// Long enough that the queue fills in a few records.
const PAYLOAD: [u8; 4096] = [7u8; 4096];

fn manifest(args: &[&str], limits: Value) -> PluginManifest {
    let v = json!({
        "manifest_version": 1, "id": "dummy", "version": "0.1.0",
        "licence": "LicenseRef-hackriff-undecided",
        "executable": DUMMY, "args": args,
        "input": {"kind": "channel", "datatype": "cf32_le"},
        "output": {"schema_id": "hackriff.dummy/1", "content_class": "unrestricted"},
        // No restart: the first kill leaves the instance `Failed`, so a test that must show
        // "not killed" cannot be fooled by a quick restart papering over one.
        "restart": {"backoff_initial_ms": 10, "backoff_max_ms": 100, "max_restarts": 0, "window_s": 60},
        "limits": limits,
    });
    PluginManifest::from_json_str(&v.to_string()).unwrap()
}

fn input() -> InputStreamDesc {
    InputStreamDesc {
        datatype: Datatype::Cf32Le,
        sample_rate_hz: RATE,
        center_hz: Some(152e6),
        bandwidth_hz: Some(25e3),
        content_class: ContentClass::Unrestricted,
        anchor: SampleTime {
            sample_index: 0,
            host_time: Timestamp::now(),
        },
        emitter_id: None,
        provenance_ref: None,
    }
}

fn rec(i: u64) -> BinaryRecord<'static> {
    BinaryRecord {
        t: Timestamp::now(),
        sample_index: i,
        flags: RecordFlags::empty(),
        payload: &PAYLOAD,
    }
}

fn spawn(m: PluginManifest) -> PluginInstance {
    let sink = Arc::new(Mutex::new(Ingest::new(
        Repository::open_in_memory().unwrap(),
    )));
    PluginInstance::spawn(m, input(), PluginContext::default(), sink).unwrap()
}

/// Pushes 4 KiB records until `done` or `deadline`, filling (and keeping full) the input queue
/// exactly as a live producer would. Returns whether `done` was reached.
fn push_until(
    inst: &mut PluginInstance,
    deadline: Duration,
    mut done: impl FnMut(&PluginMonitor) -> bool,
) -> bool {
    let mon = inst.monitor();
    let t0 = Instant::now();
    let mut i = 0u64;
    while t0.elapsed() < deadline {
        if done(&mon) {
            return true;
        }
        inst.push(rec(i)).unwrap();
        i += 1;
        thread::sleep(Duration::from_micros(200));
    }
    done(&mon)
}

fn log(mon: &PluginMonitor) -> String {
    mon.log_tail().lines.join("\n")
}

/// A plugin that is slow to reach its first instruction is **not** killed: its input queue is
/// full the whole time and stays full well past `stall_timeout`, which under the old single
/// budget was the whole test. Nothing about a child that has not run yet says it is hung.
#[test]
fn a_cold_starting_plugin_is_not_killed_and_then_decodes() {
    let m = manifest(
        &["--cold-start-ms", "1500", "--every", "4"],
        json!({
            "input_queue_bytes": 65536,
            "stall_timeout_ms": 300,
            // Not a bound this test measures anything against: it is the cap on a quantity
            // outside this repo's control (see `common`), so it is set past the worst observed.
            "startup_timeout_ms": 120_000,
        }),
    );
    let mut inst = spawn(m);
    let mon = inst.monitor();
    // The deadline covers the machine's own start-up stall as well as the plugin's deliberate
    // 1.5 s, so it is generous by design; the assertions below are what the test is about.
    let decoded = push_until(&mut inst, common::START_GRACE, |mon| {
        let s = mon.stats();
        s.decodes > 0 || s.state == PluginState::Failed
    });
    let s = mon.stats();
    assert!(
        decoded,
        "no decode and no failure in {:?}: {s:?}",
        common::START_GRACE
    );
    assert_eq!(
        (s.stall_kills, s.startup_kills, s.starts, s.crashes),
        (0, 0, 1, 0),
        "a plugin that was merely slow to start was killed: {s:?}\n{}",
        log(&mon)
    );
    assert_eq!(s.last_hang, None, "{s:?}");
    assert_ne!(s.state, PluginState::Failed, "{s:?}\n{}", log(&mon));
    assert!(s.decodes > 0, "{s:?}");
    assert!(
        s.records_dropped_full > 0,
        "the queue never filled, so this run never exercised the watchdog at all: {s:?}"
    );
    assert!(
        !log(&mon).contains("killing the plugin group"),
        "{}",
        log(&mon)
    );
    let s = inst.shutdown();
    println!("A: cold start survived; {} decodes, {s:?}", s.decodes);
}

/// A plugin that **has** started (it wrote to stderr) and then stops reading its input is killed
/// on the short responsiveness budget, not the long startup one: the fix must not slow down
/// detection of a real hang.
#[test]
fn b_a_plugin_that_wedges_after_starting_is_killed_on_the_responsiveness_budget() {
    let m = manifest(
        &["--stall"],
        json!({
            "input_queue_bytes": 65536,
            "stall_timeout_ms": 300,
            "startup_timeout_ms": 30_000,
        }),
    );
    let mut inst = spawn(m);
    let mon = inst.monitor();
    // Off the clock: getting the binary running is not what this bound is about (see `common`).
    common::wait_started(&mon);
    // From here the child has shown its sign of life, so the 300 ms budget is the one in force.
    let t0 = Instant::now();
    let killed = push_until(&mut inst, Duration::from_secs(20), |mon| {
        mon.stats().state == PluginState::Failed
    });
    let took = t0.elapsed();
    let s = mon.stats();
    assert!(killed, "never killed: {s:?}\n{}", log(&mon));
    assert_eq!(
        (s.stall_kills, s.startup_kills, s.crashes),
        (1, 0, 1),
        "{s:?}\n{}",
        log(&mon)
    );
    assert_eq!(s.last_hang, Some(HangBudget::Unresponsive), "{s:?}");
    assert!(
        took < Duration::from_secs(10),
        "killed after {took:?}: a started-then-wedged plugin was judged against the startup \
         budget (30 s), not the 300 ms responsiveness budget"
    );
    let text = log(&mon);
    assert!(
        text.contains("responsiveness budget") && text.contains("stall_timeout"),
        "the kill was not reported against a named budget: {text}"
    );
    assert!(
        text.contains("produced its first output at +"),
        "the kill did not say what had been observed of the child: {text}"
    );
    assert!(
        s.last_exit
            .as_deref()
            .is_some_and(|e| e.contains("responsiveness budget")),
        "last_exit is a generic hang: {:?}",
        s.last_exit
    );
    inst.shutdown();
    println!("B: wedged after start, killed in {took:?} on the 300 ms budget");
}

/// A plugin that never produces a byte is still killed — on the startup budget, named as such,
/// and only after it, never on the short one.
#[test]
fn c_a_plugin_that_never_runs_is_killed_on_the_startup_budget() {
    const STARTUP: Duration = Duration::from_secs(3);
    let m = manifest(
        // Ten minutes of silence: the process exists and holds its pipes, and has not run.
        &["--cold-start-ms", "600000"],
        json!({
            "input_queue_bytes": 65536,
            "stall_timeout_ms": 300,
            "startup_timeout_ms": 3000,
        }),
    );
    let mut inst = spawn(m);
    let mon = inst.monitor();
    let t0 = Instant::now();
    let killed = push_until(&mut inst, Duration::from_secs(40), |mon| {
        mon.stats().state == PluginState::Failed
    });
    let took = t0.elapsed();
    let s = mon.stats();
    assert!(killed, "never killed: {s:?}\n{}", log(&mon));
    assert_eq!(
        (s.stall_kills, s.startup_kills, s.crashes),
        (0, 1, 1),
        "{s:?}\n{}",
        log(&mon)
    );
    assert_eq!(s.last_hang, Some(HangBudget::NeverStarted), "{s:?}");
    // A lower bound only: it cannot have been the 300 ms budget. (Starvation can stretch the
    // measurement, never shrink it, so this direction is safe to assert.)
    assert!(
        took >= STARTUP.mul_f64(0.8),
        "killed after {took:?}, before its own {STARTUP:?} startup budget: the short \
         responsiveness budget was applied to a child that had never run"
    );
    let text = log(&mon);
    assert!(
        text.contains("startup budget") && text.contains("startup_timeout"),
        "the kill was not reported against a named budget: {text}"
    );
    assert!(
        text.contains("may never have reached its first instruction"),
        "the kill did not say why the budget was the long one: {text}"
    );
    assert!(
        s.last_exit
            .as_deref()
            .is_some_and(|e| e.contains("startup budget")),
        "last_exit is a generic hang: {:?}",
        s.last_exit
    );
    inst.shutdown();
    println!("C: never-started plugin killed after {took:?} on the {STARTUP:?} startup budget");
}
