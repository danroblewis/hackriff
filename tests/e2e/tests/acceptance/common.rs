//! Shared helpers for the T-024 M0 slice acceptance suite.
//!
//! Every acceptance test drives the composed pipeline **through the SDR device interface** (the
//! mock SDR, T-049/T-047) via `blind.rs`, the one place a run is configured and started, and
//! asserts on docs/07 objects read back through the Repository's gated getters and
//! `query_inventory`.
//!
//! Skips: `synth_or_skip!` (no `uv`) and [`real_fixture`] (LFS data not fetched) print `SKIP` and
//! return; the CI acceptance job sets `HK_E2E_REQUIRE_SYNTH=1` and `HK_REQUIRE_FIXTURES=1` so both
//! fail instead. `readsb` is absent in CI; its skip is explicit and logged ([`readsb_available`]).
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hk_api::{ApiState, Server, ServerConfig, Token};
use hk_model::{InventoryEntry, InventoryQuery, Repository, TimeRange, Timestamp};
use hk_pipeline::{Counters, PipelineConfig, PipelineHandle, RunSummary};
use hk_stream::{
    Declared, Listener, PublisherHandle, Record, StreamHeader, StreamKind, StreamReader,
};

/// A scratch directory, removed on drop (kept with `HK_KEEP_DIRS=1`). Short names: Unix socket
/// paths inside it must stay under ~104 bytes on macOS.
pub struct TempDir(pub PathBuf);

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "hkacc-{tag}-{}-{}",
            std::process::id(),
            DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if std::env::var_os("HK_KEEP_DIRS").is_none() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// All of time, for region queries.
pub fn ever() -> TimeRange {
    TimeRange::new(
        Timestamp::from_unix_nanos(0),
        Timestamp::from_unix_nanos(i64::MAX / 2),
    )
}

const LFS_POINTER: &[u8] = b"version https://git-lfs";

fn data_fetched(meta: &Path) -> bool {
    let data = meta.with_extension("sigmf-data");
    match std::fs::File::open(&data) {
        Ok(mut f) => {
            let mut head = [0u8; 64];
            let n = f.read(&mut head).unwrap_or(0);
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            len > 4096 && !head[..n].starts_with(LFS_POINTER)
        }
        Err(_) => false,
    }
}

/// A real HackRF fixture whose LFS data is present: this checkout's copy, else the first ancestor
/// checkout's (a git worktree often has only LFS pointers). `None` skips the test;
/// `HK_REQUIRE_FIXTURES=1` (the CI acceptance job) fails instead.
pub fn real_fixture(name: &str) -> Option<PathBuf> {
    if hardware_skip(name) {
        return None;
    }
    let rel = Path::new("fixtures/hackrf/2026-09-13").join(format!("{name}.sigmf-meta"));
    let mut dir = Some(hk_e2e::paths::repo_root());
    while let Some(d) = dir {
        let meta = d.join(&rel);
        if data_fetched(&meta) {
            return Some(meta);
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    if std::env::var("HK_REQUIRE_FIXTURES").is_ok_and(|v| v == "1") {
        panic!("{name}: fixture data not fetched (git lfs pull) and HK_REQUIRE_FIXTURES=1");
    }
    eprintln!("SKIP {name}: fixture data is not fetched (git lfs pull)");
    None
}

/// `HK_DEVICE=hackrf` (T-053): fixture truth is not on the air, so fixture-based tests skip; the
/// live run with truth from an FM survey is `hil_hackrf.rs`. Never opens the device.
pub fn hardware_skip(what: &str) -> bool {
    if hk_e2e::synth::hardware_device_selected() {
        eprintln!(
            "SKIP {what}: HK_DEVICE=hackrf selects the real HackRF; fixture truth is not on the air \
             (the live-air HIL run is hil_blind_fm_survey_on_the_hackrf)"
        );
        return true;
    }
    false
}

/// Whether `readsb` is installed (`$HK_READSB` pins a build, as in the wrapper).
pub fn readsb_available() -> bool {
    if let Ok(path) = std::env::var("HK_READSB") {
        return Path::new(&path).is_file();
    }
    std::process::Command::new("readsb")
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

/// A run [`finish`] can wait for: a bare [`PipelineHandle`], or the blind harness's handle, which
/// also runs its truth-isolation checks once the run has ended.
pub trait Finishable {
    /// Waits for the run.
    fn wait_summary(self) -> RunSummary;
}

impl Finishable for PipelineHandle {
    fn wait_summary(self) -> RunSummary {
        self.wait().unwrap()
    }
}

/// Waits for a run, prints its summary and requires no thread errors.
pub fn finish(handle: impl Finishable) -> RunSummary {
    let s = handle.wait_summary();
    eprintln!("{}", s.to_text());
    assert!(
        s.errors.is_empty(),
        "pipeline thread errors: {:?}",
        s.errors
    );
    s
}

/// The run's repository.
pub fn repo(dir: &Path) -> Repository {
    Repository::open(dir.join("hackriff.db")).unwrap()
}

/// Every live inventory entry through `query_inventory` (identities gated, standard access).
pub fn inventory(repo: &Repository, q: InventoryQuery) -> Vec<InventoryEntry> {
    let mut q = InventoryQuery { limit: 500, ..q };
    let mut out = Vec::new();
    loop {
        let page = repo.query_inventory(&q).unwrap();
        out.extend(page.entries);
        match page.next_offset {
            Some(o) => q.offset = o,
            None => return out,
        }
    }
}

/// Bytes of every file under `dir` (database, WAL, history tiles, SigMF recordings, feeds).
pub fn all_bytes(dir: &Path) -> Vec<u8> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(b) = std::fs::read(&p) {
                out.extend_from_slice(&b);
            }
        }
    }
    out
}

/// Files under `dir` whose name ends with `suffix`.
pub fn files_with_suffix(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.to_string_lossy().ends_with(suffix) {
                out.push(p);
            }
        }
    }
    out
}

/// Payload sentinels (the existing T-027 scanner): lower/upper hex and the raw bytes of each
/// payload. 48-bit payloads, so chance matches in unrelated bytes are negligible.
pub fn sentinels(payload_hex: &[String]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for h in payload_hex {
        out.push(h.to_lowercase().into_bytes());
        out.push(h.to_uppercase().into_bytes());
        let raw: Vec<u8> = (0..h.len() / 2)
            .map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap())
            .collect();
        out.push(raw);
    }
    out
}

/// How many `sentinels` occur in `hay`.
pub fn count_found(hay: &[u8], sentinels: &[Vec<u8>]) -> usize {
    sentinels
        .iter()
        .filter(|s| !s.is_empty() && hay.windows(s.len()).any(|w| w == s.as_slice()))
        .count()
}

/// A `Write` sink shared with the test.
#[derive(Clone, Default)]
pub struct Buf(pub Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What a socket consumer read from one stream.
#[derive(Debug, Default)]
pub struct TapResult {
    pub header: Option<StreamHeader>,
    pub records: Vec<Record>,
    pub error: Option<String>,
}

/// Taps the pipeline's streams as external consumers would.
///
/// - Every stream offered through `PipelineConfig::stream_sink` gets an in-process consumer that
///   keeps the raw wire bytes (for sentinel scans).
/// - Streams of `socket_kinds` are also served on a Unix socket (`hk_stream::Listener`), and a
///   reader thread connects with the reference `StreamReader` and parses header + records (the
///   framing check). The sink waits until the socket consumer is attached before returning, so
///   no record is published before it can be seen.
#[derive(Clone, Default)]
pub struct StreamTap {
    dir: PathBuf,
    socket_kinds: Vec<StreamKind>,
    pub raw: Arc<Mutex<Vec<(String, Buf)>>>,
    listeners: Arc<Mutex<Vec<Listener>>>,
    readers: Arc<Mutex<Vec<TapReader>>>,
    n: Arc<AtomicU64>,
}

/// A stream id and the thread reading it from its socket.
type TapReader = (String, JoinHandle<TapResult>);

impl StreamTap {
    pub fn new(dir: &Path, socket_kinds: &[StreamKind]) -> Self {
        Self {
            dir: dir.to_path_buf(),
            socket_kinds: socket_kinds.to_vec(),
            ..Self::default()
        }
    }

    /// Installs the tap as the run's stream sink.
    pub fn install(&self, cfg: &mut PipelineConfig) {
        let tap = self.clone();
        cfg.stream_sink = Some(Arc::new(move |h, handle| tap.offer(h, handle)));
    }

    /// Taps one stream (what the sink does for each stream the pipeline offers).
    pub fn offer(&self, h: &StreamHeader, handle: PublisherHandle) {
        let buf = Buf::default();
        let n = self.n.fetch_add(1, Ordering::Relaxed);
        handle
            .subscribe(
                format!("acceptance-raw-{n}"),
                Declared::local(buf.clone()),
                Box::new(|_| {}),
            )
            .unwrap();
        self.raw.lock().unwrap().push((h.stream_id.clone(), buf));
        if !self.socket_kinds.contains(&h.kind) {
            return;
        }
        let path = self.dir.join(format!("s{n}.sock"));
        let listener = Listener::bind_uds(&path, handle.clone()).unwrap();
        self.listeners.lock().unwrap().push(listener);
        let before = handle.open_consumers();
        let reader = std::thread::spawn(move || {
            let mut out = TapResult::default();
            let mut r = match StreamReader::connect_uds(&path) {
                Ok(r) => r,
                Err(e) => {
                    out.error = Some(format!("connect: {e}"));
                    return out;
                }
            };
            let _ = r.get_ref().set_read_timeout(Some(Duration::from_secs(60)));
            match r.read_header() {
                Ok(hd) => out.header = Some(hd.clone()),
                Err(e) => {
                    out.error = Some(format!("header: {e}"));
                    return out;
                }
            }
            loop {
                match r.next_record() {
                    Ok(Some(rec)) => out.records.push(rec),
                    Ok(None) => break,
                    Err(e) => {
                        out.error = Some(format!("record: {e}"));
                        break;
                    }
                }
            }
            out
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while handle.open_consumers() <= before && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            handle.open_consumers() > before,
            "socket consumer for {} did not attach",
            h.stream_id
        );
        self.readers
            .lock()
            .unwrap()
            .push((h.stream_id.clone(), reader));
    }

    /// Stream ids offered so far.
    pub fn stream_ids(&self) -> Vec<String> {
        self.raw
            .lock()
            .unwrap()
            .iter()
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Every stream's raw wire bytes, concatenated.
    pub fn all_raw(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (_, b) in self.raw.lock().unwrap().iter() {
            out.extend_from_slice(&b.0.lock().unwrap());
        }
        out
    }

    /// Joins the socket readers (after the run finished) and drops the listeners.
    pub fn socket_results(&self) -> Vec<(String, TapResult)> {
        let readers: Vec<_> = self.readers.lock().unwrap().drain(..).collect();
        let out = readers
            .into_iter()
            .map(|(id, j)| (id, j.join().expect("stream reader thread")))
            .collect();
        self.listeners.lock().unwrap().clear();
        out
    }
}

pub const API_TOKEN: &str = "t024-acceptance-token-0123456789abcdef";

/// Starts `hk-api` over a finished run's data directory, wired as `hk replay --serve` does:
/// `/api/inventory` reads `query_inventory` on the run's database, `/api/status` the counters.
pub fn serve_api(dir: &Path, counters: Arc<Counters>) -> Server {
    let config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        Token::from_config(API_TOKEN).unwrap(),
    );
    let state = ApiState {
        inventory: Some(Arc::new(Mutex::new(repo(dir)))),
        status: Some(Arc::new(move || counters.to_json())),
        ..ApiState::default()
    };
    Server::start(config, state).unwrap()
}

/// An authenticated GET; returns `(status, body)`.
pub fn api_get(addr: SocketAddr, path: &str) -> (u16, Vec<u8>) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: test\r\nAuthorization: Bearer {API_TOKEN}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]);
    let status = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap();
    (status, raw[split + 4..].to_vec())
}

/// `/api/inventory` pages concatenated (raw bodies) and parsed entries.
pub fn api_inventory(addr: SocketAddr) -> (Vec<u8>, Vec<serde_json::Value>) {
    let mut raw = Vec::new();
    let mut rows = Vec::new();
    let mut path = "/api/inventory?limit=500".to_owned();
    loop {
        let (status, body) = api_get(addr, &path);
        assert_eq!(status, 200, "{path}: {}", String::from_utf8_lossy(&body));
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        raw.extend_from_slice(&body);
        rows.extend(v["entries"].as_array().cloned().unwrap_or_default());
        match v.get("next_cursor").and_then(|c| c.as_u64()) {
            Some(c) => path = format!("/api/inventory?limit=500&cursor={c}"),
            None => return (raw, rows),
        }
    }
}
