//! T-892: `hk serve` refused a HackRF that another process held with libhackrf's verbatim
//! "Access denied (insufficient permissions) (-1000)" (T-356's HIL), which sends the user to fix
//! permissions when the device is merely in use. Driven through the mock SDR behind the real
//! device interface: `HK_MOCK_FAULT=open-access-denied` makes the open fail with that code, and
//! startup must report the device **in use** (or, honestly, not permitted), naming it.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use hk_model::Timestamp;
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};

/// Removes the scratch directory on drop, pass or fail.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "hk-cli-t892-{}-{}",
            std::process::id(),
            Timestamp::now().as_unix_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A tiny ci8 recording (the open fails before a sample is read).
fn recording(dir: &Path) -> PathBuf {
    std::fs::write(dir.join("rec.sigmf-data"), vec![0u8; 2 * 4096]).unwrap();
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(250e3);
    meta.captures.push(Capture {
        sample_start: 0,
        frequency: Some(100.8e6),
        datetime: Some("2026-09-24T12:00:00Z".into()),
        provenance: None,
        clip_count: None,
        extra: serde_json::Map::new(),
    });
    let path = dir.join("rec.sigmf-meta");
    meta.write(&path).unwrap();
    path
}

#[test]
fn hk_serve_reports_a_held_device_as_in_use_not_as_a_permissions_error() {
    let dir = Scratch::new();
    let meta = recording(&dir.0);
    let mut child = Command::new(env!("CARGO_BIN_EXE_hk"))
        .arg("serve")
        .arg("--device")
        .arg(format!("mock:{}", meta.display()))
        .arg("--data-dir")
        .arg(dir.0.join("data"))
        .arg("--bind")
        .arg("127.0.0.1:0")
        .env("HK_TOKEN", "t892-device-in-use-token-0123456789abcdef")
        .env("HK_MOCK_FAULT", "open-access-denied")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr_pipe = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr_pipe.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("`hk serve` did not exit on a refused open within 60 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stderr = reader.join().unwrap();
    assert!(!status.success(), "a refused open fails startup: {stderr}");
    assert!(
        stderr.contains("is in use by another process, or not permitted"),
        "startup names the likely cause, and admits the one the driver cannot rule out:\n{stderr}"
    );
    assert!(
        stderr.contains("mock:"),
        "startup names the device it could not open:\n{stderr}"
    );
    assert!(
        !stderr.contains("hackrf_open_by_serial failed: Access denied"),
        "libhackrf's verbatim permissions text must not be the reported error:\n{stderr}"
    );
}
