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
//! - `every_temp_data_dir_call_site_is_guarded` (T-232) is a structural regression guard: it greps
//!   every `.rs` file in the repo and fails if a `let ... = temp_data_dir()` binding isn't
//!   followed by a `TempDataDirGuard::new` within a few lines. T-217 added a call site
//!   (`iq_buffer_allocation_http.rs`) that skipped the guard and cleaned up with a bare
//!   `remove_dir_all` a panic could skip entirely; this test stops that pattern from landing again
//!   without anyone noticing during a parallel task's review.

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

/// Every `.rs` file under `root`, skipping VCS/build/dependency directories.
fn rust_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            if matches!(
                name.as_ref(),
                "target" | ".git" | "node_modules" | ".claude"
            ) {
                continue;
            }
            rust_files(&path, out);
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
}

/// T-232 regression guard: every `let ... = temp_data_dir();` binding (the test-scratch-dir
/// convention throughout this crate) must be followed within a few lines by a
/// `TempDataDirGuard::new` construction, so a killed test or a race with a lagging background
/// thread still leaves the directory owned by a guard that retries cleanup (or keeps it on
/// failure) instead of leaking it silently or relying on a bare `remove_dir_all` a panic can skip.
///
/// Deliberately excludes `fn temp_data_dir()` itself and the two production call sites
/// (`unwrap_or_else(temp_data_dir)` in `pipeline.rs`/`serve.rs`, matched by shape, not by file):
/// those aren't a local `let` binding, and a real `hk serve`/`hackriffd` run without `--data-dir`
/// owns its directory for the run's lifetime, not a test scratch dir a guard should delete.
#[test]
fn every_temp_data_dir_call_site_is_guarded() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    rust_files(&repo_root, &mut files);
    assert!(
        files.len() > 50,
        "sanity: found suspiciously few .rs files under {}: {}",
        repo_root.display(),
        files.len()
    );

    const WINDOW: usize = 8;
    let mut violations = Vec::new();
    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            // Real call sites are code, not comments or doc examples quoting the pattern (this
            // file's own doc comments above reference `= temp_data_dir()` in prose); require an
            // actual `let ... = temp_data_dir();` statement.
            if trimmed.starts_with("//") || !trimmed.starts_with("let ") {
                continue;
            }
            if !line.contains("= temp_data_dir();") {
                continue;
            }
            let guarded = lines[i..lines.len().min(i + 1 + WINDOW)]
                .iter()
                .any(|l| l.contains("TempDataDirGuard::new("));
            if !guarded {
                violations.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "temp_data_dir() call site(s) without a TempDataDirGuard within {WINDOW} lines \
         (T-232 - a killed test or a slow background thread will leak these):\n{}",
        violations.join("\n")
    );
}
