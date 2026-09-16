//! T-229: the replay/test harness's temp-directory hygiene.
//!
//! - `normal_runs_leave_no_net_new_replay_dirs` runs a representative set of `hk replay`s through
//!   [`TempDataDirGuard`] and asserts the system temp dir has no net new `hk-replay-*` entries
//!   afterwards — the incident this task fixes (2731 leaked directories, ~12 GB).
//! - `guard_keeps_the_directory_on_panic_and_prints_it` proves the "keep on failure" half: a
//!   directory guarded by a panicking thread survives, instead of being silently deleted along
//!   with the evidence.
//! - `stale_orphan_sweep_removes_only_dead_pid_and_old_dirs` exercises the sweep against a private
//!   fixture directory this file creates and destroys itself, per the task's safety rule: never
//!   run the sweep under test against the live system temp dir, which other agents and sessions
//!   use concurrently.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use hk_cli::pipeline::{
    ReplayArgs, TempDataDirGuard, run_replay, sweep_stale_replay_dirs_in, temp_data_dir,
};

/// Serializes this file's tests: `count_own_replay_dirs` counts every `hk-replay-<this pid>-*`
/// entry, so two of these tests running as threads in the same process (`cargo test`'s default,
/// as opposed to cargo-nextest's one-process-per-test) would otherwise see each other's
/// directories mid-measurement. Cheap and only affects this file.
static SEQUENTIAL: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    SEQUENTIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// A minimal recording: `secs` of a quiet ci8 tone at 100.8 MHz, 250 ksps.
fn tiny_recording(dir: &Path, secs: f64) -> PathBuf {
    use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
    std::fs::create_dir_all(dir).unwrap();
    let fs = 250e3;
    let n = (secs * fs) as usize;
    let mut data = vec![0u8; 2 * n];
    for i in 0..n {
        data[2 * i] = 40;
    }
    std::fs::write(dir.join("x.sigmf-data"), &data).unwrap();
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(fs);
    meta.captures.push(Capture {
        sample_start: 0,
        frequency: Some(100.8e6),
        datetime: Some("2026-09-13T12:00:00Z".into()),
        provenance: None,
        clip_count: None,
        extra: serde_json::Map::new(),
    });
    let path = dir.join("x.sigmf-meta");
    meta.write(&path).unwrap();
    path
}

/// The number of `hk-replay-<this test process's pid>-*` entries directly under the system temp
/// dir: scoped to this process's own pid (embedded in [`temp_data_dir`]'s naming), not every
/// `hk-replay-*` entry, because other agents' and sessions' test processes create and remove their
/// own concurrently on this shared machine (cargo-nextest runs each test in its own process) —
/// counting all of them would make this assertion flaky for reasons unrelated to this harness.
fn count_own_replay_dirs() -> usize {
    let prefix = format!("hk-replay-{}-", std::process::id());
    std::fs::read_dir(std::env::temp_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(&prefix))
        })
        .count()
}

#[test]
fn normal_runs_leave_no_net_new_replay_dirs() {
    let _l = lock();
    let before = count_own_replay_dirs();
    for i in 0..3 {
        let dir = temp_data_dir();
        let _guard = TempDataDirGuard::new(dir.clone());
        let fixture = tiny_recording(&dir.join("src"), 0.1);
        let summary = run_replay(&ReplayArgs {
            fixture,
            data_dir: Some(dir.clone()),
            ..ReplayArgs::default()
        })
        .unwrap_or_else(|e| panic!("run {i}: {e:#}"));
        assert!(summary.errors.is_empty(), "run {i}: {:?}", summary.errors);
    }
    let after = count_own_replay_dirs();
    assert_eq!(
        after, before,
        "no net new hk-replay-* dirs after 3 normal replay runs (before {before}, after {after})"
    );
}

#[test]
fn guard_keeps_the_directory_on_panic_and_prints_it() {
    let _l = lock();
    let dir = temp_data_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let probe = dir.clone();
    // The default panic hook still prints "thread panicked..." to stderr; that's expected test
    // output here, not a real failure.
    let result = std::panic::catch_unwind(move || {
        let _guard = TempDataDirGuard::new(probe);
        panic!("T-229 test: simulated test failure");
    });
    assert!(result.is_err(), "the simulated panic propagated");
    assert!(
        dir.exists(),
        "a panicking guard leaves its directory in place for inspection: {}",
        dir.display()
    );
    // This is a deliberately induced failure, not a real one: clean up now that we've checked it.
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stale_orphan_sweep_removes_only_dead_pid_and_old_dirs() {
    let root = std::env::temp_dir().join(format!("hk-t229-sweep-fixture-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    // A pid that is definitely dead: spawn a trivial child, wait for it to exit, and use its pid.
    let dead_pid = {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawning `true`");
        let pid = child.id();
        child.wait().expect("waiting for `true`");
        pid
    };

    let make = |name: &str| {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        d
    };
    let age = |d: &Path, secs: u64| {
        std::fs::File::open(d)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(secs))
            .unwrap();
    };

    let old_dead = make(&format!("hk-replay-{dead_pid}-1000-0"));
    age(&old_dead, 7200); // 2h old: past the 1h floor.

    let fresh_dead = make(&format!("hk-replay-{dead_pid}-1000-1"));
    // Left at its just-created mtime: too fresh, even though the pid is dead.

    let old_live = make(&format!("hk-replay-{}-1000-2", std::process::id()));
    age(&old_live, 7200); // 2h old, but the pid (this test process) is alive.

    let unrelated = make("not-hk-replay-something");
    age(&unrelated, 7200);

    sweep_stale_replay_dirs_in(&root, Duration::from_secs(3600));

    assert!(!old_dead.exists(), "dead pid, past the age floor: removed");
    assert!(fresh_dead.exists(), "dead pid, too fresh: kept");
    assert!(
        old_live.exists(),
        "live pid, regardless of age: never removed"
    );
    assert!(unrelated.exists(), "not an hk-replay-* name: never touched");

    let _ = std::fs::remove_dir_all(&root);
}
