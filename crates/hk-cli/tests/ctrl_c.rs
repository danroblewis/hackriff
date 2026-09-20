//! T-037a item 2: Ctrl-C (SIGINT) stops `hk replay` and `hackriffd` gracefully: the source
//! stops, the pipeline drains, the Survey is closed with an end time, any recording left behind is
//! valid SigMF, and the process exits 0. Every wait is bounded; a hung child is killed.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use hk_model::{Repository, SurveyId, SurveyState, Timestamp};

const FS: f64 = 200e3;
const SECS: f64 = 20.0;

fn scratch(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "hk-cli-ctrlc-{tag}-{}-{}",
        std::process::id(),
        Timestamp::now().as_unix_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// `SECS` of noise plus a tone at 433.5 MHz (paced replay takes `SECS` to finish).
fn recording(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let n = (SECS * FS) as usize;
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut data = Vec::with_capacity(2 * n);
    for i in 0..n {
        let ph = 2.0 * std::f64::consts::PI * 40e3 * i as f64 / FS;
        for v in [ph.cos(), ph.sin()] {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let noise = ((state >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 12.0;
            data.push((30.0 * v + noise).round().clamp(-128.0, 127.0) as i8 as u8);
        }
    }
    std::fs::write(dir.join("long.sigmf-data"), data).unwrap();
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(FS);
    meta.captures.push(Capture {
        sample_start: 0,
        frequency: Some(433.5e6),
        datetime: Some("2026-09-13T12:00:00Z".into()),
        provenance: None,
        clip_count: None,
        extra: serde_json::Map::new(),
    });
    let path = dir.join("long.sigmf-meta");
    meta.write(&path).unwrap();
    path
}

/// Collects a pipe into a shared string on a thread (so the child never blocks on a full pipe).
fn collect(pipe: impl Read + Send + 'static) -> Arc<Mutex<String>> {
    let out = Arc::new(Mutex::new(String::new()));
    let o = Arc::clone(&out);
    std::thread::spawn(move || {
        for line in BufReader::new(pipe).lines().map_while(Result::ok) {
            let mut s = o.lock().unwrap();
            s.push_str(&line);
            s.push('\n');
        }
    });
    out
}

fn wait_until(limit: Duration, what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_exit(child: &mut Child, limit: Duration) -> ExitStatus {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the process did not exit within {limit:?} after Ctrl-C");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn sigint(child: &Child) {
    // SAFETY: sends SIGINT to our own child process.
    let rc = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    assert_eq!(rc, 0, "kill(SIGINT)");
}

/// Checks the closed Survey named in the printed summary and any recordings.
fn assert_graceful(stdout: &str, data: &Path) {
    let line = stdout
        .lines()
        .find(|l| l.starts_with("survey:"))
        .unwrap_or_else(|| panic!("no summary printed:\n{stdout}"));
    let id: SurveyId = line
        .trim_start_matches("survey:")
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let repo = Repository::open(data.join("hackriff.db")).unwrap();
    let survey = repo.survey(id).unwrap();
    assert_eq!(survey.state, SurveyState::Closed, "the Survey is closed");
    assert!(survey.t_end.is_some(), "with an end time");
    let samples: u64 = stdout
        .lines()
        .find(|l| l.starts_with("source:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no source line:\n{stdout}"));
    assert!(
        samples < (SECS * FS) as u64,
        "stopped early ({samples} samples)"
    );
    if let Ok(entries) = std::fs::read_dir(data.join("recordings")) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "sigmf-meta") {
                let meta = SigmfMeta::read(&p).expect("valid SigMF metadata");
                assert!(meta.global.sample_rate.is_some());
                assert!(p.with_extension("sigmf-data").is_file(), "{}", p.display());
            }
        }
    }
}

#[test]
fn hk_replay_stops_gracefully_on_ctrl_c() {
    let dir = scratch("replay");
    let meta = recording(&dir.join("src"));
    let data = dir.join("data");
    let mut child = Command::new(env!("CARGO_BIN_EXE_hk"))
        .arg("replay")
        .arg(&meta)
        .arg("--paced")
        .arg("--data-dir")
        .arg(&data)
        .env("HK_IQ_BUFFER_MAX", "16MiB")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = collect(child.stdout.take().unwrap());
    let stderr = collect(child.stderr.take().unwrap());
    wait_until(Duration::from_secs(60), "the run to start", || {
        data.join("hackriff.db").is_file()
    });
    std::thread::sleep(Duration::from_millis(1500));
    sigint(&child);
    let status = wait_exit(&mut child, Duration::from_secs(60));
    std::thread::sleep(Duration::from_millis(100));
    let (out, err) = (
        stdout.lock().unwrap().clone(),
        stderr.lock().unwrap().clone(),
    );
    assert!(
        status.success(),
        "{status:?}\nstdout:\n{out}\nstderr:\n{err}"
    );
    assert!(err.contains("stopping"), "{err}");
    assert_graceful(&out, &data);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn hackriffd_stops_gracefully_on_ctrl_c() {
    let dir = scratch("daemon");
    let meta = recording(&dir.join("src"));
    let data = dir.join("data");
    let mut child = Command::new(env!("CARGO_BIN_EXE_hackriffd"))
        .arg("--source")
        .arg(format!("sigmf:{}", meta.display()))
        .arg("--data-dir")
        .arg(&data)
        .arg("--bind")
        .arg("127.0.0.1:0")
        .env("HK_TOKEN", "t037a-ctrl-c-daemon-token-0123456789")
        .env("HK_IQ_BUFFER_MAX", "16MiB")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = collect(child.stdout.take().unwrap());
    let stderr = collect(child.stderr.take().unwrap());
    wait_until(Duration::from_secs(60), "the daemon to listen", || {
        stderr.lock().unwrap().contains("listening on")
    });
    std::thread::sleep(Duration::from_millis(1500));
    sigint(&child);
    let status = wait_exit(&mut child, Duration::from_secs(60));
    std::thread::sleep(Duration::from_millis(100));
    let (out, err) = (
        stdout.lock().unwrap().clone(),
        stderr.lock().unwrap().clone(),
    );
    assert!(
        status.success(),
        "{status:?}\nstdout:\n{out}\nstderr:\n{err}"
    );
    assert_graceful(&out, &data);
    let _ = std::fs::remove_dir_all(dir);
}

/// T-531: **shutdown is prompt and bounded.** A 40-minute live sweep took over 78 s to exit on
/// SIGTERM (T-525), which is an operational fault and not untidiness: `ops/stage.sh` restarts the
/// server on a failed health check and waits 10 s before `SIGKILL`, so a slow exit overlaps two
/// servers on one device, and only one process can open the HackRF.
///
/// The assertion is the promise a supervisor needs: **the process is gone within
/// [`hk_cli::signal::SHUTDOWN_BOUND`]** of the signal, on the graceful path if it can and on the
/// armed deadline if it cannot. It is deliberately *not* "the drain finished in X ms" — a machine
/// with four agents building on it makes that number meaningless — and it is measured from the
/// signal, not from the spawn.
#[test]
fn hk_serve_exits_within_the_shutdown_bound_on_sigterm() {
    let dir = scratch("serve-sigterm");
    let meta = recording(&dir.join("src"));
    let data = dir.join("data");
    let mut child = Command::new(env!("CARGO_BIN_EXE_hk"))
        .arg("serve")
        .arg("--replay")
        .arg(&meta)
        .arg("--loop")
        .arg("--data-dir")
        .arg(&data)
        .arg("--bind")
        .arg("127.0.0.1:0")
        .env("HK_TOKEN", "t531-serve-sigterm-token-0123456789")
        .env("HK_IQ_BUFFER_MAX", "16MiB")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = collect(child.stdout.take().unwrap());
    let stderr = collect(child.stderr.take().unwrap());
    wait_until(Duration::from_secs(60), "the server to listen", || {
        stderr.lock().unwrap().contains("listening on")
    });
    // Long enough that detection, the history writer and the IQ buffer are all doing work, so the
    // signal lands mid-drain rather than on an idle run.
    std::thread::sleep(Duration::from_secs(4));

    let t0 = Instant::now();
    // SAFETY: sends SIGTERM to our own child process.
    let rc = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(rc, 0, "kill(SIGTERM)");
    // The margin is process teardown and a loaded machine's scheduling, not slack in the bound.
    let limit = hk_cli::signal::SHUTDOWN_BOUND + Duration::from_secs(4);
    let status = wait_exit(&mut child, limit);
    let elapsed = t0.elapsed();
    std::thread::sleep(Duration::from_millis(100));
    let (out, err) = (
        stdout.lock().unwrap().clone(),
        stderr.lock().unwrap().clone(),
    );
    eprintln!("[T-531] hk serve exited {elapsed:?} after SIGTERM");
    assert!(
        elapsed <= limit,
        "SIGTERM to exit took {elapsed:?}, over {limit:?}\nstdout:\n{out}\nstderr:\n{err}"
    );
    assert!(
        status.success(),
        "{status:?}\nstdout:\n{out}\nstderr:\n{err}"
    );
    assert!(err.contains("stopping"), "{err}");
    let _ = std::fs::remove_dir_all(dir);
}
