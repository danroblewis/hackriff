//! Shared helpers for the T-027 pipeline acceptance tests.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use hk_core::Pacing;
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use hk_model::{InventoryEntry, InventoryQuery, Repository, Timestamp};
use hk_pipeline::{
    Pipeline, PipelineConfig, PipelineHandle, Replay, RunSummary, TrackInventory, open_replay,
    replay_plan,
};

/// Writes `secs` of a ci8 tone (+50 kHz, amplitude 40) in noise at `fs` and `center_hz` as
/// `<name>.sigmf-meta/-data` in `dir`, starting at stream index `global_index` when given.
pub fn tone_recording(
    dir: &Path,
    name: &str,
    fs: f64,
    secs: f64,
    center_hz: f64,
    global_index: Option<u64>,
) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let n = (secs * fs) as usize;
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut noise = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        ((state >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 12.0
    };
    let mut data = Vec::with_capacity(2 * n);
    for i in 0..n {
        let ph = 2.0 * std::f64::consts::PI * 50e3 * i as f64 / fs;
        let re = (40.0 * ph.cos() + noise()).round().clamp(-128.0, 127.0) as i8;
        let im = (40.0 * ph.sin() + noise()).round().clamp(-128.0, 127.0) as i8;
        data.push(re as u8);
        data.push(im as u8);
    }
    std::fs::write(dir.join(format!("{name}.sigmf-data")), data).unwrap();
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(fs);
    let mut extra = serde_json::Map::new();
    if let Some(g) = global_index {
        extra.insert("core:global_index".into(), serde_json::json!(g));
    }
    meta.captures.push(Capture {
        sample_start: 0,
        frequency: Some(center_hz),
        datetime: Some("2026-09-13T12:00:00Z".into()),
        provenance: None,
        clip_count: None,
        extra,
    });
    let path = dir.join(format!("{name}.sigmf-meta"));
    meta.write(&path).unwrap();
    path
}

/// Waits for a run with a deadlock guard: a watchdog stops the run if it has not finished
/// within `limit`. Returns the summary and whether the watchdog had to stop it.
pub fn wait_guarded(handle: PipelineHandle, limit: Duration) -> (RunSummary, bool) {
    let stopper = handle.stopper();
    let (tx, rx) = mpsc::channel::<()>();
    let dog = std::thread::spawn(move || match rx.recv_timeout(limit) {
        Err(RecvTimeoutError::Timeout) => {
            stopper.stop();
            true
        }
        _ => false,
    });
    let summary = handle.wait().unwrap();
    drop(tx);
    let fired = dog.join().unwrap();
    (summary, fired)
}

/// A scratch data directory, removed on drop (kept with `HK_KEEP_DIRS=1`).
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "hk-pipeline-{tag}-{}-{}",
            std::process::id(),
            Timestamp::now().as_unix_nanos()
        ));
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

const LFS_POINTER: &[u8] = b"version https://git-lfs";

fn data_fetched(meta: &Path) -> bool {
    let data = meta.with_extension("sigmf-data");
    match std::fs::read(&data) {
        Ok(bytes) => bytes.len() > 4096 && !bytes.starts_with(LFS_POINTER),
        Err(_) => false,
    }
}

/// A real HackRF fixture whose LFS data is present: the repository's copy, else the first
/// ancestor checkout's (a git worktree often has only LFS pointers). `None` skips the test
/// (`HK_REQUIRE_FIXTURES=1` fails instead).
pub fn real_fixture(name: &str) -> Option<PathBuf> {
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
        panic!("{name}: fixture data not fetched (git lfs pull)");
    }
    eprintln!("SKIP {name}: fixture data is not fetched (git lfs pull)");
    None
}

/// A copy of `meta` with its annotations (the ground truth) removed, next to a link to its data,
/// in `dir`: the pipeline replays it blind while the test keeps the original's truth.
pub fn blind_meta(meta: &Path, dir: &Path) -> PathBuf {
    // The T-039 harness's stripping (annotations and description removed, data linked).
    hk_e2e::blind::strip_truth(meta, dir, "blind", 0.0).unwrap()
}

/// The one replay entry point of the tests below: [`replay_config`] over a blind copy of `meta`
/// (annotations stripped, [`blind_meta`]) kept in the returned [`TempDir`].
pub fn blind_replay_config(
    dir: &Path,
    meta: &Path,
    extra: serde_json::Value,
    pacing: Pacing,
) -> (PipelineConfig, Replay, TempDir) {
    let input = TempDir::new("blind-input");
    let blind = blind_meta(meta, &input.0);
    let (cfg, replay) = replay_config(dir, &blind, extra, pacing);
    (cfg, replay, input)
}

/// An unpaced, lossless replay configuration with `extra` as the plan's `extra`.
pub fn replay_config(
    dir: &Path,
    meta: &Path,
    extra: serde_json::Value,
    pacing: Pacing,
) -> (PipelineConfig, Replay) {
    let replay = open_replay(meta, pacing, false).unwrap();
    let info = replay.info;
    let mut plan = replay_plan(info.center_hz, info.sample_rate_hz, info.start_time);
    plan.extra = extra;
    let mut cfg = PipelineConfig::new(dir, plan).unwrap();
    cfg.source_class = replay.class;
    cfg.lossless = matches!(pacing, Pacing::Unpaced);
    (cfg, replay)
}

/// Starts a run.
pub fn start(cfg: PipelineConfig, replay: Replay) -> PipelineHandle {
    Pipeline::start(
        cfg,
        Box::new(replay.source),
        replay.info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap()
}

/// Runs `meta` once, unpaced, and prints the summary.
pub fn run(dir: &Path, meta: &Path, extra: serde_json::Value) -> RunSummary {
    let (cfg, replay) = replay_config(dir, meta, extra, Pacing::Unpaced);
    let summary = start(cfg, replay).wait().unwrap();
    eprintln!("{}", summary.to_text());
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    summary
}

/// The run's repository.
pub fn repo(dir: &Path) -> Repository {
    Repository::open(dir.join("hackriff.db")).unwrap()
}

/// Every live inventory entry (identities gated, standard access).
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

/// Bytes of every file under `dir` (database, WAL, tiles, recordings).
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

/// Payload sentinels: the hex (both cases) and raw bytes of each payload.
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

/// Sentinels found in `hay`.
pub fn count_found(hay: &[u8], sentinels: &[Vec<u8>]) -> usize {
    sentinels
        .iter()
        .filter(|s| !s.is_empty() && hay.windows(s.len()).any(|w| w == s.as_slice()))
        .count()
}
