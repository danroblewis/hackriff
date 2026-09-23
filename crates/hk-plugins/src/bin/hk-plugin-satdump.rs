//! `hk-plugin-satdump`: the T-275 satellite-imagery decoder plugin wrapper
//! (`plugins/satdump-137/manifest.json`, `plugins/satdump-lband/manifest.json`), fitting SatDump
//! (GPLv3; docs/adr/0010) behind the T-014 plugin contract (docs/stream-contract.md §9; ADR-0003).
//!
//! SatDump is exec'd as a child, never linked (ADR-0010's process boundary). Unlike readsb
//! (`hk-plugin-readsb`), this wrapper does no protocol decoding of its own — SatDump owns the
//! whole QPSK/FM demod -> frame sync -> Reed-Solomon -> image chain for every pipeline it lists
//! (`meteor_m2-lrpt`, `noaa_apt`, `goes_hrit`, `metop_ahrpt`, ...), which is the point: SIGNAL-027
//! (Meteor-M LRPT), SIGNAL-029 (NOAA POES APT), SIGNAL-025 (GOES HRIT) and SIGNAL-028 (Metop/
//! FengYun AHRPT) are one reused decoder tiered by antenna/dish requirement
//! (`docs/use-cases.yaml`'s `hardware_fit`), not four reimplementations. This wrapper's whole job
//! is: feed it hackriff's framed IQ, and turn the product files it drops into decode lines.
//!
//! - **Format.** The channel carries `ci8` (signed 8-bit interleaved I/Q, HackRF-native), which is
//!   exactly SatDump's `s8` raw baseband format — no sample conversion, unlike readsb's `ci8 ->
//!   uc8` offset flip. Bytes are forwarded unchanged.
//! - **Transport.** hackriff has no file to hand SatDump, and SatDump has no stdin-IQ mode this
//!   wrapper can rely on, so it creates a POSIX FIFO in a private temp directory, opens it for
//!   writing on a dedicated thread (which blocks until SatDump opens the read end — ordinary FIFO
//!   rendezvous), and points SatDump at it with `--file_path`/`--source file`. A `--out_dir` under
//!   the same temp directory is where SatDump is told to drop products.
//! - **Products, not messages.** SatDump has no notion of hackriff's NDJSON contract: it writes
//!   files (images, text products) to its output directory as a pass completes. A watcher thread
//!   polls that directory and reports a file once its size has held steady across
//!   [`STABLE_SCANS`] consecutive scans (SatDump is still writing it before then), as one `decode`
//!   line (`frame_model: "satdump-product"`, `content.path` the absolute file path,
//!   `metadata.kind` its extension). `sample_index` is the input stream's last forwarded sample —
//!   the only sample time available, since SatDump does not report which of its output covers
//!   which slice of a long-running live capture.
//! - **No readiness handshake.** Unlike readsb, this wrapper cannot attest to when SatDump has
//!   actually locked or is merely running, so the manifest declares no `ready_signal` and the host
//!   treats the plugin as ready as soon as it is attached (docs/stream-contract.md §9.3). SatDump
//!   buffers its own input regardless, and the FIFO rendezvous already holds the writer thread
//!   until SatDump has opened its end.
//! - **CLI is manifest-configurable, not hardcoded.** The pipeline id and any other SatDump flags
//!   come from `plugins/satdump-*/manifest.json`'s `args`/`params` (`{param.pipeline}`), resolved
//!   by the host before this wrapper ever sees them; this wrapper only appends the FIFO and output
//!   directory it created. **The exact SatDump CLI (subcommand name, flag spelling) has not been
//!   verified against an installed SatDump build** — CLAUDE.md's hard requirement is that a
//!   pipeline can be corrected by editing the manifest, not by rebuilding this wrapper, and that is
//!   what the `args`/`params` split gives: fix the invocation in the manifest.
//! - **Crash and stall isolation**, mirroring readsb: a waiter thread ends this wrapper non-zero if
//!   SatDump exits before this wrapper asked it to (crash, signal, or a real exit); the plugin
//!   host's supervisor then restarts the pair. `HK_SATDUMP` overrides the executable path (tests
//!   use `hk-fake-satdump`, which never spawns the real tool).

use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio, exit};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use hk_stream::{Record, StreamReader};
use serde_json::json;

/// Converted input records the writer thread may have queued before this wrapper stops reading
/// its own stdin (the same backpressure shape as readsb's `WRITER_BACKLOG_CHUNKS`).
const WRITER_BACKLOG_CHUNKS: usize = 16;
/// How often the watcher thread re-scans SatDump's output directory.
const SCAN_INTERVAL: Duration = Duration::from_millis(500);
/// Consecutive stable scans (unchanged, non-zero size) before a product file is reported. At
/// [`SCAN_INTERVAL`] this is ~1s of no growth.
const STABLE_SCANS: u32 = 2;
/// How long this wrapper waits for SatDump to exit on its own after a clean shutdown before it is
/// killed outright.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Path to the real `satdump` binary; overridable for tests (and for a build not on `PATH`).
fn satdump_path() -> String {
    std::env::var("HK_SATDUMP").unwrap_or_else(|_| "satdump".into())
}

/// Ends this process because SatDump is gone, or something else makes continuing unsafe. Never
/// returns: the host's supervisor treats a non-zero wrapper exit as a crash and restarts the pair.
fn crash_exit(reason: &str) -> ! {
    eprintln!("hk-plugin-satdump: {reason}");
    exit(101)
}

fn describe_status(status: &ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit code {code}"),
        (None, Some(sig)) => format!("signal {sig}"),
        _ => "unknown exit".into(),
    }
}

/// Creates a POSIX FIFO at `path` (mode `0600`).
fn mkfifo(path: &Path) -> io::Result<()> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Per-file state the output-directory watcher tracks across scans.
#[derive(Default)]
struct Watched {
    size: u64,
    stable_scans: u32,
    reported: bool,
}

/// Collects every regular file under `dir`, recursively (SatDump nests products by pipeline
/// stage).
fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            walk_files(&path, out);
        } else if meta.is_file() {
            out.push(path);
        }
    }
}

/// One pass of the output-directory watcher: updates `seen` from the files currently under `dir`
/// and returns the ones that just became stable (size unchanged for [`STABLE_SCANS`] consecutive
/// scans, not already reported). Split out from the watcher thread so the debounce logic is
/// unit-testable without spawning a real filesystem watcher or SatDump.
fn scan_once(dir: &Path, seen: &mut HashMap<PathBuf, Watched>) -> Vec<PathBuf> {
    let mut files = Vec::new();
    walk_files(dir, &mut files);
    let mut newly_stable = Vec::new();
    for path in files {
        let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let w = seen.entry(path.clone()).or_default();
        if size > 0 && w.size == size {
            w.stable_scans += 1;
        } else {
            w.size = size;
            w.stable_scans = 0;
        }
        if w.stable_scans >= STABLE_SCANS && !w.reported {
            w.reported = true;
            newly_stable.push(path);
        }
    }
    newly_stable
}

/// Builds one `decode` NDJSON line (docs/stream-contract.md §9.3) for a product file.
fn product_line(path: &Path, sample_index: u64) -> String {
    let kind = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin")
        .to_ascii_lowercase();
    json!({
        "type": "decode",
        "sample_index": sample_index,
        "frame_model": "satdump-product",
        "crc_status": "no-crc",
        "metadata": {"kind": kind},
        "content": {"path": path.display().to_string()},
    })
    .to_string()
}

/// Writes one NDJSON line to our own stdout under a fresh lock (readsb's convention: the lock is
/// never held across threads, since the main thread and the watcher/stderr threads all write
/// lines independently). `false` if our stdout is closed (the host is detaching us).
fn emit_line(line: &str) -> bool {
    let mut out = io::stdout().lock();
    writeln!(out, "{line}").is_ok() && out.flush().is_ok()
}

/// Owns the FIFO's write end. Blocks opening it until SatDump opens the read end (ordinary FIFO
/// rendezvous), then forwards chunks from `rx` until the sender is dropped (clean shutdown) or a
/// write fails (SatDump died).
fn feed_satdump(fifo_path: &Path, rx: mpsc::Receiver<(Vec<u8>, u64)>, last_index: &AtomicU64) {
    let mut file = match fs::OpenOptions::new().write(true).open(fifo_path) {
        Ok(f) => f,
        Err(e) => crash_exit(&format!("opening input fifo {fifo_path:?} failed: {e}")),
    };
    loop {
        match rx.recv() {
            Ok((chunk, last_sample)) => {
                if file.write_all(&chunk).is_err() {
                    crash_exit("write to satdump's input fifo failed; it likely died");
                }
                last_index.store(last_sample, Ordering::Relaxed);
            }
            Err(_) => return, // clean shutdown: the main thread dropped its sender
        }
    }
}

/// Polls `out_dir` for new SatDump products until `stop` is set, then does one last scan (a
/// product finishing exactly at shutdown must still be reported) before returning.
fn watch_products(out_dir: &Path, last_index: &AtomicU64, stop: &AtomicBool) {
    let mut seen: HashMap<PathBuf, Watched> = HashMap::new();
    loop {
        let stopping = stop.load(Ordering::SeqCst);
        for path in scan_once(out_dir, &mut seen) {
            let idx = last_index.load(Ordering::Relaxed);
            if !emit_line(&product_line(&path, idx)) {
                return; // our own stdout closed
            }
        }
        if stopping {
            return;
        }
        thread::sleep(SCAN_INTERVAL);
    }
}

/// Copies SatDump's stderr into our own, tagged, so it lands in the host's log ring.
fn pump_stderr(stderr: impl Read) {
    let mut r = BufReader::new(stderr);
    let mut line = String::new();
    while r.read_line(&mut line).unwrap_or(0) > 0 {
        eprint!("satdump: {line}");
        line.clear();
    }
}

fn spawn_satdump(extra_args: &[String], fifo_path: &Path, out_dir: &Path) -> io::Result<Child> {
    Command::new(satdump_path())
        .args(extra_args)
        .args([
            "--source",
            "file",
            "--file_path",
            fifo_path.to_string_lossy().as_ref(),
            "--baseband_format",
            "s8",
            "--out_dir",
            out_dir.to_string_lossy().as_ref(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

fn main() {
    let extra_args: Vec<String> = std::env::args().skip(1).collect();

    let mut reader = StreamReader::new(io::stdin().lock());
    let header = match reader.read_header() {
        Ok(h) => h.clone(),
        Err(e) => {
            eprintln!("hk-plugin-satdump: bad input header: {e}");
            exit(3);
        }
    };
    if header.datatype.as_deref() != Some("ci8") {
        eprintln!(
            "hk-plugin-satdump: header datatype {:?}, expected ci8",
            header.datatype
        );
        exit(4);
    }

    let workspace = std::env::temp_dir().join(format!("hk-plugin-satdump-{}", std::process::id()));
    let out_dir = workspace.join("out");
    let fifo_path = workspace.join("in.fifo");
    if let Err(e) = fs::create_dir_all(&out_dir) {
        eprintln!("hk-plugin-satdump: creating workspace {workspace:?} failed: {e}");
        exit(7);
    }
    if let Err(e) = mkfifo(&fifo_path) {
        eprintln!("hk-plugin-satdump: mkfifo {fifo_path:?} failed: {e}");
        exit(8);
    }

    let mut child = match spawn_satdump(&extra_args, &fifo_path, &out_dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("hk-plugin-satdump: spawning {} failed: {e}", satdump_path());
            let _ = fs::remove_dir_all(&workspace);
            exit(5);
        }
    };
    eprintln!("hk-plugin-satdump: satdump pid {}", child.id());
    let pid = child.id() as libc::pid_t;

    let satdump_stdout = child.stdout.take().expect("piped stdout");
    let satdump_stderr = child.stderr.take().expect("piped stderr");

    let clean_shutdown = Arc::new(AtomicBool::new(false));
    let last_index = Arc::new(AtomicU64::new(0));
    let (tx, rx) = mpsc::sync_channel::<(Vec<u8>, u64)>(WRITER_BACKLOG_CHUNKS);

    let writer_last_index = Arc::clone(&last_index);
    let writer_fifo = fifo_path.clone();
    let writer = thread::spawn(move || feed_satdump(&writer_fifo, rx, &writer_last_index));

    let waiter_clean = Arc::clone(&clean_shutdown);
    let waiter = thread::spawn(move || {
        let status = child.wait();
        if !waiter_clean.load(Ordering::SeqCst) {
            match &status {
                Ok(s) => crash_exit(&format!(
                    "satdump exited unexpectedly: {}",
                    describe_status(s)
                )),
                Err(e) => crash_exit(&format!("waiting for satdump failed: {e}")),
            }
        }
        status
    });

    let err_thread = thread::spawn(move || pump_stderr(satdump_stderr));
    // SatDump's own stdout carries nothing this wrapper parses; drain it so it never blocks on a
    // full pipe.
    let out_drain = thread::spawn(move || {
        let mut r = BufReader::new(satdump_stdout);
        let mut buf = String::new();
        while r.read_line(&mut buf).unwrap_or(0) > 0 {
            buf.clear();
        }
    });

    let stop_watch = Arc::new(AtomicBool::new(false));
    let watch_last_index = Arc::clone(&last_index);
    let watch_out_dir = out_dir.clone();
    let watch_stop = Arc::clone(&stop_watch);
    let watcher =
        thread::spawn(move || watch_products(&watch_out_dir, &watch_last_index, &watch_stop));

    // Feed loop: forward ci8 bytes to the writer thread unchanged (module doc: ci8 == SatDump's
    // s8). Never blocks capture: this process's own stdin is the bounded `DecoderFeed` ring.
    let mut dropped_seen = 0u64;
    let mut input_error = false;
    loop {
        match reader.next_record() {
            Ok(Some(Record::Binary(b))) => {
                let last_sample = b.header.sample_index + (b.payload.len() as u64 / 2).max(1) - 1;
                if tx.send((b.payload, last_sample)).is_err() {
                    input_error = true;
                    break;
                }
            }
            Ok(Some(Record::Dropped(d))) => dropped_seen += d.count,
            Ok(Some(_)) => {}
            Ok(None) => break, // our own stdin at EOF: the host is detaching us (clean)
            Err(e) => {
                eprintln!("hk-plugin-satdump: input error: {e}");
                input_error = true;
                break;
            }
        }
    }
    if dropped_seen > 0 {
        eprintln!("hk-plugin-satdump: {dropped_seen} input records were dropped upstream");
    }
    if input_error {
        crash_exit("stopping after an input error on our own stdin");
    }

    // Clean shutdown: stop feeding SatDump (closes the fifo, which it reads EOF from), give it a
    // grace period to flush its last products and exit, then SIGKILL it if it hasn't. The waiter
    // thread already holds a blocking `child.wait()`; killing by pid (not through `Child`, which
    // that thread owns) still lets it reap the process normally once it dies.
    clean_shutdown.store(true, Ordering::SeqCst);
    drop(tx);
    let _ = writer.join();
    let deadline = Instant::now() + SHUTDOWN_GRACE;
    while !waiter.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    if !waiter.is_finished() {
        eprintln!(
            "hk-plugin-satdump: satdump did not exit within the shutdown grace period; killing it"
        );
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    let _ = waiter.join();
    stop_watch.store(true, Ordering::SeqCst);
    let _ = watcher.join();
    let _ = out_drain.join();
    let _ = err_thread.join();
    let _ = fs::remove_dir_all(&workspace);
    eprintln!("hk-plugin-satdump: clean shutdown");
    exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_once_reports_a_file_only_once_it_stops_growing() {
        let dir = std::env::temp_dir().join(format!("hk-satdump-scan-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("nested").join("product.png");
        fs::create_dir_all(file_path.parent().unwrap()).unwrap();

        let mut seen = HashMap::new();
        // Nothing written yet.
        assert!(scan_once(&dir, &mut seen).is_empty());

        let mut f = fs::File::create(&file_path).unwrap();
        f.write_all(b"partial").unwrap();
        drop(f);
        // First sighting: not stable yet.
        assert!(scan_once(&dir, &mut seen).is_empty());
        // Same size again: one stable scan, still short of STABLE_SCANS.
        assert!(scan_once(&dir, &mut seen).is_empty());
        // STABLE_SCANS-th unchanged scan: now reported.
        let stable = scan_once(&dir, &mut seen);
        assert_eq!(stable, vec![file_path.clone()]);
        // Reported once, never again even though it is still sitting there unchanged.
        assert!(scan_once(&dir, &mut seen).is_empty());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn scan_once_resets_the_stability_count_when_a_file_grows() {
        let dir =
            std::env::temp_dir().join(format!("hk-satdump-scan-test-grow-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("growing.bin");
        let mut seen = HashMap::new();

        fs::write(&file_path, b"a").unwrap();
        scan_once(&dir, &mut seen);
        scan_once(&dir, &mut seen); // one scan short of stable
        fs::write(&file_path, b"ab").unwrap(); // grew: resets the count
        assert!(scan_once(&dir, &mut seen).is_empty());
        scan_once(&dir, &mut seen);
        let stable = scan_once(&dir, &mut seen);
        assert_eq!(stable, vec![file_path]);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn product_line_carries_path_kind_and_sample_index() {
        let line = product_line(Path::new("/tmp/out/METEOR-M2_lrpt.png"), 42);
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["type"], "decode");
        assert_eq!(v["sample_index"], 42);
        assert_eq!(v["frame_model"], "satdump-product");
        assert_eq!(v["metadata"]["kind"], "png");
        assert_eq!(v["content"]["path"], "/tmp/out/METEOR-M2_lrpt.png");
    }

    #[test]
    fn mkfifo_creates_a_named_pipe_and_refuses_to_clobber_a_normal_file() {
        let dir = std::env::temp_dir().join(format!("hk-satdump-fifo-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let fifo_path = dir.join("in.fifo");
        mkfifo(&fifo_path).unwrap();
        let meta = fs::symlink_metadata(&fifo_path).unwrap();
        use std::os::unix::fs::FileTypeExt;
        assert!(meta.file_type().is_fifo());
        assert!(
            mkfifo(&fifo_path).is_err(),
            "a second mkfifo at the same path must fail"
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
