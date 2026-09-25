//! `hk replay`, `hk run`, `hk serve` and `hackriffd`: the binaries' pipeline composition (T-027,
//! T-037a, T-042). The pipeline itself is `hk_pipeline`; this module picks the source, plan, data
//! directory, calibration and API server.
//!
//! - `hk replay <fixture> [--data-dir D] [--plan P] [--serve ADDR] [--paced] [--schedule]
//!   [--calibration C]` runs the recording once, unpaced and lossless by default, and prints the
//!   run summary. Without `--data-dir` a fresh directory under the system temp dir is used.
//! - `hk run [--source hackrf[:SERIAL]] [--center-hz F --rate R --lna L --vga V --amp]
//!   [--duration S] [--serve ADDR] [--schedule]` runs the whole pipeline over the **live HackRF
//!   One** (receive only) until `--duration` or Ctrl-C. Lossless mode is never used (a radio
//!   cannot pause; `Pipeline::start` refuses it).
//! - `hackriffd [--source hackrf[:SERIAL] | sigmf:<file> [--loop]] --data-dir D [--plan P]
//!   [--bind ADDR]` drives the attention scheduler (at the pipeline's one sample rate), publishes
//!   streams over the bridge and serves `/api/status`. The live HackRF is the default source.
//! - Ctrl-C (or SIGTERM) stops every run gracefully ([`crate::signal`]).
//! - **Content class of a live run:** band-derived from the tuned window (`band_class` over
//!   `centre ± rate/2`, the T-027 restricted paging/cellular bands), or, when the scheduler
//!   drives the radio, over every window that tiles the plan's regions. A [`LiveControl`] retune
//!   or rate change goes through the pipeline (T-050): a window of another class or rate
//!   re-plumbs the run with that window's class at a block boundary.
//! - **Control API** (T-050, [`serve_api`]): display, recording and bookmarks for every
//!   served run; device settings for live runs without the scheduler. Every control request is
//!   audited to `<data dir>/control-audit.jsonl`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use hk_api::{
    ApiState, AuditLog, LiveControl, LiveTuning, Server, ServerConfig, SourceLiveControl,
    StreamRegistry, Token, default_token_path,
};
use hk_core::{
    DeviceInfo, HackRfDriver, MockClock, MockEnd, MockOptions, MockSdrDriver, NamedGain,
    OpenRequest, Pacing, RtlSdrDriver, Source, SourceCapabilities, SourceControl, SourceDriver,
};
use hk_model::cluster::most_restrictive;
use hk_model::{ContentClass, Repository, ScanPlan, Timestamp};
use hk_pipeline::class::band_class;
use hk_pipeline::reports::ReportService;
use hk_pipeline::{
    PipelineConfig, PipelineHandle, RunSummary, SourceFactory, SourceInfo, TrackInventory,
    load_calibrations, open_mock_replay, open_replay, replay_plan,
};

use crate::control::{
    PipelineAnalyze, PipelineDatasets, PipelineIqBuffer, PipelineOutputs, PipelinePlayback,
    PipelineRecordings, PipelineRetuner, PipelineRunControl,
};
use crate::signal;

/// Live HackRF One settings (shared by `hk run`, `hk serve` and `hackriffd`).
#[derive(clap::Args, Clone, Debug, PartialEq)]
pub struct LiveArgs {
    /// Live centre frequency, Hz.
    #[arg(long = "center-hz", default_value_t = 100.8e6)]
    pub center_hz: f64,
    /// Live sample rate (span), Hz: 2–20 Msps.
    #[arg(long = "rate", default_value_t = 2.4e6)]
    pub sample_rate_hz: f64,
    /// LNA (IF) gain, dB: 0–40 in 8 dB steps.
    #[arg(long = "lna", default_value_t = 16.0)]
    pub lna_db: f64,
    /// VGA (baseband) gain, dB: 0–62 in 2 dB steps.
    #[arg(long = "vga", default_value_t = 20.0)]
    pub vga_db: f64,
    /// RF amplifier on (the `amp` stage at its maximum).
    #[arg(long)]
    pub amp: bool,
    /// Baseband filter bandwidth, Hz (default: 0.75 x rate).
    #[arg(long = "baseband-filter-hz")]
    pub baseband_filter_hz: Option<f64>,
    /// Named gain `STAGE=DB` (repeatable; stage names from the device's capabilities). Overrides
    /// --lna, --vga and --amp.
    #[arg(long = "gain", value_name = "STAGE=DB")]
    pub gains: Vec<String>,
}

impl LiveArgs {
    /// The requested gains as named stages of `caps` (`--lna`, `--vga`, `--amp`, then `--gain`).
    pub fn named_gains(&self, caps: &SourceCapabilities) -> anyhow::Result<Vec<NamedGain>> {
        let mut gains: Vec<NamedGain> = Vec::new();
        let mut set = |stage: &str, db: f64| match gains.iter_mut().find(|g| g.stage == stage) {
            Some(g) => g.db = db,
            None => gains.push(NamedGain::new(stage, db)),
        };
        for (stage, db) in [("lna", self.lna_db), ("vga", self.vga_db)] {
            if caps.gain_stage(stage).is_some() {
                set(stage, db);
            }
        }
        if let Some(amp) = caps.gain_stage("amp") {
            set("amp", if self.amp { amp.max_db } else { amp.min_db });
        }
        for spec in &self.gains {
            let (stage, db) = spec
                .split_once('=')
                .and_then(|(s, v)| v.trim().parse::<f64>().ok().map(|v| (s.trim(), v)))
                .with_context(|| format!("--gain {spec:?}: expected STAGE=DB"))?;
            if caps.gain_stage(stage).is_none() {
                anyhow::bail!(
                    "--gain {spec:?}: {} has no gain stage {stage:?} (stages: {})",
                    caps.driver,
                    caps.gain_stages
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            set(stage, db);
        }
        Ok(gains)
    }
}

impl Default for LiveArgs {
    fn default() -> Self {
        Self {
            center_hz: 100.8e6,
            sample_rate_hz: 2.4e6,
            lna_db: 16.0,
            vga_db: 20.0,
            amp: false,
            baseband_filter_hz: None,
            gains: Vec::new(),
        }
    }
}

/// `hk replay` options.
#[derive(Clone, Debug, Default)]
pub struct ReplayArgs {
    /// `.sigmf-meta` path.
    pub fixture: PathBuf,
    /// Data directory (default: a fresh temp directory).
    pub data_dir: Option<PathBuf>,
    /// ScanPlan JSON.
    pub plan: Option<PathBuf>,
    /// Iterative scan (T-406): step the tune across everything the source can tune, dwelling this
    /// many seconds per step. `None` keeps the single-region default plan.
    pub survey_dwell_s: Option<f64>,
    /// Serve the API while (and after) running.
    pub serve: Option<SocketAddr>,
    /// Real-time pacing (lossy) instead of unpaced lossless replay.
    pub paced: bool,
    /// Drive the attention scheduler (virtual tuning).
    pub schedule: bool,
    /// Offline feed cache for the correlator.
    pub feeds: Option<PathBuf>,
    /// Built UI directory.
    pub ui_dist: Option<PathBuf>,
    /// T-021 calibration JSON (file or directory).
    pub calibration: Option<PathBuf>,
    /// Compute provider (T-056).
    pub compute: ComputeArgs,
}

/// `hk run` options (live HackRF One).
#[derive(Clone, Debug, Default)]
pub struct RunArgs {
    /// `hackrf` or `hackrf:<serial>`.
    pub source: String,
    /// Tuning and gains.
    pub live: LiveArgs,
    /// Further devices (T-512), from [`resolve_devices`]; empty = one device.
    pub extra_devices: Vec<(String, LiveArgs)>,
    /// Stop after this long (default: until Ctrl-C).
    pub duration_s: Option<f64>,
    /// Data directory (default: a fresh temp directory).
    pub data_dir: Option<PathBuf>,
    /// ScanPlan JSON.
    pub plan: Option<PathBuf>,
    /// Iterative scan (T-406): step the tune across the device's tunable range, dwelling this
    /// many seconds per step. `None` keeps the single-region default plan.
    pub survey_dwell_s: Option<f64>,
    /// Serve the API while (and after) running.
    pub serve: Option<SocketAddr>,
    /// Drive the attention scheduler (it retunes the radio).
    pub schedule: bool,
    /// Offline feed cache for the correlator.
    pub feeds: Option<PathBuf>,
    /// Built UI directory.
    pub ui_dist: Option<PathBuf>,
    /// T-021 calibration JSON (file or directory).
    pub calibration: Option<PathBuf>,
    /// Compute provider (T-056).
    pub compute: ComputeArgs,
}

/// `hackriffd` options.
#[derive(Clone, Debug)]
pub struct DaemonArgs {
    /// `hackrf`, `hackrf:<serial>` (live, the default) or `sigmf:<path>`.
    pub source: String,
    /// Live tuning (initial window; the scheduler then follows the plan).
    pub live: LiveArgs,
    /// Further devices (T-512), from [`resolve_devices`]; empty = one device.
    pub extra_devices: Vec<(String, LiveArgs)>,
    /// Replay again at the end, continuing the stream (recordings only).
    pub loop_replay: bool,
    /// Data directory.
    pub data_dir: PathBuf,
    /// ScanPlan JSON.
    pub plan: Option<PathBuf>,
    /// Iterative scan (T-406): step the tune across the device's tunable range, dwelling this
    /// many seconds per step. `None` keeps the single-region default plan.
    pub survey_dwell_s: Option<f64>,
    /// API listen address.
    pub bind: SocketAddr,
    /// Built UI directory.
    pub ui_dist: Option<PathBuf>,
    /// Replay unpaced and lossless instead of in real time (recordings only).
    pub unpaced: bool,
    /// Offline feed cache for the correlator.
    pub feeds: Option<PathBuf>,
    /// API token (default: `HK_TOKEN`, else generated).
    pub token: Option<String>,
    /// T-021 calibration JSON (file or directory).
    pub calibration: Option<PathBuf>,
    /// Listen limits (T-066).
    pub listen: ListenArgs,
    /// Compute provider (T-056).
    pub compute: ComputeArgs,
    /// Rolling IQ capture buffer retention and cap (T-157).
    pub iq_buffer: IqBufferArgs,
}

/// Listen limits (T-066) for `hk serve` and `hackriffd`. Unset flags keep the plan's
/// (`extra.pipeline.listen`) or the default values.
#[derive(clap::Args, Clone, Debug, Default, PartialEq)]
pub struct ListenArgs {
    /// Most concurrent Listen chains (default 8).
    #[arg(long = "listen-max")]
    pub max_listeners: Option<usize>,
    /// Share of the CPU cores all Listen chains may use together, estimated from each chain's
    /// sample rate and mode (default 0.5).
    #[arg(long = "listen-cpu-fraction")]
    pub cpu_fraction: Option<f64>,
}

impl ListenArgs {
    /// Checks the flags.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.max_listeners == Some(0) {
            anyhow::bail!("--listen-max must be at least 1");
        }
        if let Some(f) = self.cpu_fraction
            && !(f.is_finite() && f > 0.0)
        {
            anyhow::bail!("--listen-cpu-fraction must be a positive number");
        }
        Ok(())
    }

    /// Applies the flags given to `handle`'s listen limits.
    pub fn apply(&self, handle: &PipelineHandle) {
        if self.max_listeners.is_none() && self.cpu_fraction.is_none() {
            return;
        }
        let mut s = handle.listen_settings();
        if let Some(n) = self.max_listeners {
            s.max_listeners = n;
        }
        if let Some(f) = self.cpu_fraction {
            s.cpu_fraction = f;
        }
        handle.set_listen_settings(s);
    }
}

/// The compute provider flag (T-056, ADR-0007) of `hk replay`, `hk run`, `hk serve` and
/// `hackriffd`. Unset keeps the plan's `extra.pipeline.compute` (default `auto`). The
/// `HK_COMPUTE*` environment variables win over both.
#[derive(clap::Args, Clone, Debug, Default, PartialEq)]
pub struct ComputeArgs {
    /// Compute provider for every DSP workload: auto (the measured best conformant provider
    /// compiled into this build), cpu, cpu-mt, accelerate or gpu. The GPU and Accelerate need a
    /// build with `--features gpu-wgpu` / `accelerate`; an unavailable one falls back and says why
    /// in /api/status (`compute`). HK_COMPUTE overrides it.
    #[arg(long = "compute", value_name = "PROVIDER")]
    pub provider: Option<hk_dsp::compute::Preference>,
}

impl ComputeArgs {
    /// Applies the flag to `settings`: the provider for every workload (the plan's per-workload
    /// overrides are cleared).
    pub fn apply(&self, settings: &mut hk_pipeline::PipelineSettings) {
        if let Some(p) = self.provider {
            settings.compute.provider = p;
            settings.compute.stft = None;
            settings.compute.pfb = None;
        }
    }
}

/// Rolling IQ capture buffer (T-157) for `hk serve` and `hackriffd`. Unset flags keep
/// `HK_IQ_RETENTION` / `HK_IQ_BUFFER_MAX` or the defaults.
#[derive(clap::Args, Clone, Debug, Default, PartialEq)]
pub struct IqBufferArgs {
    /// Rolling IQ capture buffer retention window, e.g. 90s, 2m, 1h (default 2m;
    /// HK_IQ_RETENTION). 0 or off disables the buffer.
    #[arg(
        long = "iq-retention",
        value_name = "DURATION",
        value_parser = hk_store::iqbuffer::parse_duration_s
    )]
    pub retention_s: Option<f64>,
    /// Hard size cap of the IQ capture buffer, e.g. 512MiB, 8GiB (HK_IQ_BUFFER_MAX). The quota
    /// is retention x the device's highest sample rate x 2 bytes/sample, or this cap if smaller.
    #[arg(
        long = "iq-buffer-max",
        value_name = "SIZE",
        value_parser = hk_store::iqbuffer::parse_size_bytes
    )]
    pub max_bytes: Option<u64>,
}

impl IqBufferArgs {
    /// Applies the flags over `cfg` (which already carries the environment) and sizes the implied
    /// quota by `max_rate_hz`, the device's highest configurable sample rate.
    pub fn apply(&self, cfg: &mut hk_store::iqbuffer::IqBufferConfig, max_rate_hz: Option<f64>) {
        if let Some(s) = self.retention_s {
            cfg.retention_s = s;
        }
        if let Some(b) = self.max_bytes {
            cfg.max_bytes = Some(b);
        }
        if max_rate_hz.is_some() {
            cfg.max_rate_hz = max_rate_hz;
        }
    }
}

/// Test-only override of the IQ capture ring's allocation ([`hk_store::iqbuffer::IqBufferHooks`]):
/// lets an hk-cli test mock a large ring's allocation (T-217) instead of actually preallocating
/// it, without weakening the trait object requirements (`Send + Sync`, no `Debug`) for
/// [`LiveOptions`] and [`ServeOptions`], which otherwise derive `Debug`. The CLI binaries never
/// set this: `hk_store::iqbuffer::OsHooks` (the real allocator) is always used when it is `None`.
#[derive(Clone)]
pub struct IqBufferHooksOverride(pub Arc<dyn hk_store::iqbuffer::IqBufferHooks>);

impl std::fmt::Debug for IqBufferHooksOverride {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IqBufferHooksOverride(..)")
    }
}

/// The highest sample rate a device can be configured to, Hz.
pub fn max_rate_hz(caps: &SourceCapabilities) -> Option<f64> {
    use hk_core::source::SampleRates;
    match &caps.sample_rates {
        SampleRates::Continuous { max_hz, .. } => Some(*max_hz),
        SampleRates::Discrete(rates) => rates.iter().copied().reduce(f64::max),
    }
}

/// The plan one run scans under: a `--plan` file, an **iterative scan** over everything the front
/// end can tune (`--survey-dwell`, T-406), or the single-region default around the source window.
///
/// `--survey-dwell` is the user's "step the tune to the next region after sampling enough per step".
/// It is a dwell policy over the scheduler that already exists — a `DwellOnly` plan over
/// [`SourceCapabilities::frequency_ranges`] with the given dwell — so every step writes its own
/// observation record and the coverage map keeps the shape of what was scanned
/// (`hk_core::scheduler::IterativeScan`, `docs/16` §7 step 6). It only takes effect with the
/// scheduler driving; a run that does not drive it never steps.
///
/// A `--plan` file wins if both are given, and says so rather than silently ignoring one.
pub fn plan_for_run(
    path: Option<&Path>,
    survey_dwell_s: Option<f64>,
    info: &SourceInfo,
    caps: &SourceCapabilities,
) -> anyhow::Result<ScanPlan> {
    match (path, survey_dwell_s) {
        (Some(p), Some(_)) => {
            anyhow::bail!(
                "--plan {} and --survey-dwell both given: pass one. A plan file can carry the \
                 iterative scan itself (policy \"dwell-only\", extra.scheduler.region_dwell_s).",
                p.display()
            )
        }
        (None, Some(dwell_s)) => {
            let scan = hk_core::scheduler::IterativeScan::from_seconds(dwell_s)?;
            let t = if info.start_time.as_unix_nanos() > 0 {
                info.start_time
            } else {
                Timestamp::now()
            };
            let plan = scan.plan_over_capabilities(caps, t);
            if !scan.recommended() {
                eprintln!(
                    "iterative scan: a {:.3} s dwell is outside the 10-30 s the survey is sized \
                     for; the pass is linear in it",
                    scan.dwell_s()
                );
            }
            // Say what a step of this length can and cannot catch, here — where the user turns the
            // policy on and the pass length is a surprise worth having before the run, not after.
            // A plan this source cannot compile is left to fail where plans normally fail.
            if let Ok(cfg) = hk_core::scheduler::SchedulerConfig::from_plan(&plan)
                && let Ok(compiled) = hk_core::scheduler::CompiledPlan::compile(&plan, &cfg, caps)
            {
                eprintln!("{}", scan.budget(&compiled).statement());
            }
            Ok(plan)
        }
        (path, None) => load_plan(path, info),
    }
}

/// Loads a ScanPlan JSON, or a single-region plan over the source window.
pub fn load_plan(path: Option<&Path>, info: &SourceInfo) -> anyhow::Result<ScanPlan> {
    match path {
        Some(p) => {
            let text =
                std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
            serde_json::from_str(&text).with_context(|| format!("parsing {}", p.display()))
        }
        None => Ok(replay_plan(
            info.center_hz,
            info.sample_rate_hz,
            if info.start_time.as_unix_nanos() > 0 {
                info.start_time
            } else {
                Timestamp::now()
            },
        )),
    }
}

/// A fresh data directory under the system temp dir (pid, time and a process-wide counter, so
/// concurrent callers never share one). Every call also runs (once per process) a sweep of stale
/// `hk-replay-*` orphans left by earlier crashed or killed processes (T-229).
pub fn temp_data_dir() -> PathBuf {
    sweep_stale_replay_dirs();
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "hk-replay-{}-{}-{n}",
        std::process::id(),
        Timestamp::now().as_unix_nanos()
    ))
}

/// A stale `hk-replay-*` orphan must be older than this on top of its owning pid being dead before
/// the sweep removes it (T-229). One hour: comfortably longer than any test or `hk replay`/`hk
/// serve` run in this repo (the slowest, `unpaced_replay_completes_on_large_global_index_fixtures`,
/// bounds itself to 20 minutes), so it never races a slow-but-live run whose pid happens to get
/// reused quickly after another process exits, while still reclaiming crashed runs promptly.
pub const STALE_REPLAY_DIR_AGE: Duration = Duration::from_secs(3600);

/// Removes orphaned `hk-replay-<pid>-<ts>-<n>` directories directly under `root` whose embedded
/// pid names no process that is currently alive, **and** whose contents were last modified more
/// than `min_age` ago. Both conditions gate every removal: a live pid is never touched regardless
/// of age (other agents and sessions run tests on this machine concurrently), and age alone is not
/// trusted because a pid can be reused, so a directory just past its birth could coincidentally
/// share a pid with an unrelated, live process. Errors reading an entry just skip it; this is a
/// best-effort sweep, not a correctness requirement.
///
/// `pub` (rather than only reachable through [`sweep_stale_replay_dirs`]) so tests can exercise it
/// against a private fixture directory instead of the live system temp dir, which other agents and
/// sessions use concurrently.
pub fn sweep_stale_replay_dirs_in(root: &Path, min_age: Duration) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(pid) = replay_dir_pid(&name) else {
            continue;
        };
        if pid_alive(pid) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if now.duration_since(modified).is_ok_and(|age| age >= min_age) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// The pid embedded in an `hk-replay-<pid>-<ts>-<n>` directory name, if `name` matches that shape.
fn replay_dir_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("hk-replay-")?;
    rest.split('-').next()?.parse().ok()
}

/// Whether `pid` names a process that is currently alive (`kill(pid, 0)`, POSIX's
/// existence-without-signalling probe): success or `EPERM` (it exists but is owned by someone
/// else) both mean alive; `ESRCH` (no such process) means dead. Any other, unexpected errno is
/// treated as "alive" so the sweep stays conservative and never removes a directory it isn't sure
/// about.
fn pid_alive(pid: u32) -> bool {
    // SAFETY: `kill` with signal 0 sends no signal; it only probes for the process's existence and
    // permission to signal it, per POSIX.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Runs [`sweep_stale_replay_dirs_in`] over the system temp dir exactly once per process.
fn sweep_stale_replay_dirs() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        sweep_stale_replay_dirs_in(&std::env::temp_dir(), STALE_REPLAY_DIR_AGE);
    });
}

/// RAII cleanup for a [`temp_data_dir`] (T-229): removes it recursively when dropped during a
/// normal return, tearing down anything inside it — including a pre-allocated IQ capture ring
/// (`ring.ci8`, which can be many GB) — with the directory itself. When the owning thread is
/// unwinding from a panic (a failed `assert!`/`assert_eq!`), the directory is left in place and
/// its path printed to stderr instead, so a developer can inspect the run's database, IQ ring and
/// recordings that produced the failure. A run that is killed outright (no unwind at all, e.g.
/// SIGKILL or a hard process abort) leaves its directory too; [`sweep_stale_replay_dirs`] reclaims
/// those later once their pid is dead and they've aged past [`STALE_REPLAY_DIR_AGE`].
///
/// **T-232 / T-236.** The orphans this guard was hardened against came from `hk-api`: its
/// `Server::shutdown` joined only the accept-loop thread, and every accepted connection ran on a
/// detached thread that was never joined, so a test dropping its server and this guard in the same
/// scope raced a handler still writing under the guarded directory (the audit log, the ring,
/// SQLite's WAL, the observation log) — `remove_dir_all` then failed on the entry that reappeared
/// mid-removal. Under load that hit every server-spawning test in this crate (26/26). T-232
/// absorbed it here with a long retry window; **T-236 removed the cause**, so `Server::shutdown`
/// now closes its connections and waits for their threads (bounded) before returning. The retry
/// stays only as a short backstop for the unjoined tail nothing else owns — see
/// [`REMOVE_RETRY_BACKOFF`].
pub struct TempDataDirGuard(PathBuf);

/// Backoff schedule for [`TempDataDirGuard`]'s removal retries: about 180 ms total.
///
/// T-232 needed ~7 s here because the dominant racer — a detached `hk_api::http::Server`
/// connection thread still writing the audit log, the database's WAL or the observation log — was
/// never waited for at all, so the guard had to outlast arbitrary scheduling delay. **T-236 fixed
/// that at the root**: `Server::shutdown` now closes every open connection and joins its handler
/// threads (bounded) before returning, so by the time a guard runs no HTTP handler can still be
/// inside this directory.
///
/// T-236 also re-measured the one residual T-232 had left (an `observations/` subtree, once per
/// full run): it was never a retry problem at all. The removal *succeeded* — with
/// `HK_DEBUG_TEMP_DIR_RETRY=1` no attempt ever failed — and the directory reappeared afterwards,
/// because the test bound its guard last in a `let` tuple (bindings drop in reverse order), so the
/// guard ran while the server and pipeline were still alive and their teardown recreated the dated
/// directory. No backoff length could have fixed that; the drop order was fixed instead.
///
/// What is left is the short tail nothing joins: the pipeline's own writer threads winding down,
/// SQLite releasing its WAL, and the odd filesystem-level lag on a loaded machine. Those are
/// sub-100 ms in every measurement, so the window shrank from ~7 s to ~180 ms; a directory that
/// outlives even this is left for [`sweep_stale_replay_dirs`] once its pid is dead and it has aged
/// past [`STALE_REPLAY_DIR_AGE`].
const REMOVE_RETRY_BACKOFF: &[Duration] = &[
    Duration::from_millis(10),
    Duration::from_millis(20),
    Duration::from_millis(50),
    Duration::from_millis(100),
];

impl TempDataDirGuard {
    /// Guards `dir` (typically a [`temp_data_dir`]) for cleanup on drop.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self(dir.into())
    }

    /// The guarded directory.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// Removes `dir` recursively, retrying on failure per [`REMOVE_RETRY_BACKOFF`] (T-232, shortened
/// by T-236): a lagging background thread that nothing joins can still be creating or writing
/// entries under `dir` for a short time after its owning run was told to stop, which races a
/// single-shot `remove_dir_all` on a loaded machine. Returns the last error if every attempt fails
/// (the directory is left for [`sweep_stale_replay_dirs`] to reclaim once its pid is dead and it
/// has aged out — a bounded lag, not a leak).
fn remove_dir_all_retrying(dir: &Path) -> std::io::Result<()> {
    let mut last = Ok(());
    for (i, backoff) in REMOVE_RETRY_BACKOFF.iter().enumerate() {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return Ok(()),
            Err(_) if !dir.exists() => return Ok(()),
            Err(e) => {
                if std::env::var_os("HK_DEBUG_TEMP_DIR_RETRY").is_some() {
                    eprintln!("T-232 DEBUG: attempt {i} failed: {e}");
                    if let Ok(rd) = std::fs::read_dir(dir) {
                        for entry in rd.flatten() {
                            eprintln!("T-232 DEBUG:   remaining: {}", entry.path().display());
                        }
                    }
                }
                last = Err(e);
                if i + 1 < REMOVE_RETRY_BACKOFF.len() {
                    std::thread::sleep(*backoff);
                }
            }
        }
    }
    last
}

impl Drop for TempDataDirGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "T-229: test failed; keeping temp data dir for inspection: {}",
                self.0.display()
            );
            return;
        }
        if let Err(e) = remove_dir_all_retrying(&self.0) {
            eprintln!(
                "T-232: temp data dir still busy after retrying removal for ~180ms (left for the \
                 stale-orphan sweep to reclaim): {} ({e})",
                self.0.display()
            );
        }
    }
}

/// The API token: `configured`, else `HK_TOKEN`, else the token file
/// ([`hk_api::default_token_path`]: created with mode 0600 on first run and reused after, so the
/// UI and tunnel URLs survive restarts), else a generated one for this run.
pub fn token(configured: Option<&str>) -> anyhow::Result<Token> {
    match configured
        .map(str::to_owned)
        .or_else(|| std::env::var("HK_TOKEN").ok())
    {
        Some(t) => Token::from_config(&t).map_err(|e| anyhow::anyhow!("API token: {e}")),
        None => match default_token_path() {
            Some(path) => {
                let (t, created) = Token::load_or_create(&path)
                    .with_context(|| format!("API token file {}", path.display()))?;
                if created {
                    eprintln!(
                        "hk: created the API token file {} (mode 0600)",
                        path.display()
                    );
                }
                Ok(t)
            }
            None => Token::generate().context("generating the API token"),
        },
    }
}

/// T-127: the pipeline's scheduler hub behind `/api/scheduler*`.
pub struct PipelineScheduler(pub Arc<hk_pipeline::control::SchedulerHub>);

impl hk_api::schedule::SchedulerControl for PipelineScheduler {
    fn view(&self) -> Option<hk_api::schedule::SchedulerView> {
        self.0.snapshot().map(|s| hk_api::schedule::SchedulerView {
            status: s.status,
            regions: s.regions.clone(),
            leases: s.leases.clone(),
            plan_version: s.plan_version,
        })
    }

    fn arms(&self) -> Option<Vec<hk_core::scheduler::ArmStatus>> {
        self.0.snapshot().map(|s| s.arms.clone())
    }

    fn add_lease(
        &self,
        lease: hk_core::scheduler::Lease,
    ) -> Result<hk_core::scheduler::Lease, hk_api::schedule::SchedulerFail> {
        self.0.add_lease(lease).map_err(scheduler_fail)
    }

    fn release_lease(&self, id: u64) -> Result<bool, hk_api::schedule::SchedulerFail> {
        self.0.release_lease(id).map_err(scheduler_fail)
    }
}

fn scheduler_fail(e: hk_pipeline::control::HubError) -> hk_api::schedule::SchedulerFail {
    use hk_api::schedule::SchedulerFail as F;
    use hk_pipeline::control::HubError as H;
    match e {
        H::NoScheduler => F::NoScheduler,
        H::Busy => F::Busy,
        H::Refused(m) => F::Refused(m),
        H::TableFull(m) => F::TableFull(m),
    }
}

/// T-121: the run's survey reports behind the API's [`hk_api::reports::ReportControl`] (public so
/// acceptance tests serve reports exactly as `hk serve` does).
pub struct PipelineReports(pub ReportService);

fn report_fail(e: hk_pipeline::reports::ReportError) -> hk_api::reports::ReportFail {
    use hk_pipeline::reports::ReportError as E;
    let (status, message) = match &e {
        E::Invalid(m) => (400, (*m).to_owned()),
        E::NoCoverage => (404, e.to_string()),
        E::Provider(_) | E::Validation(_) => (500, e.to_string()),
    };
    hk_api::reports::ReportFail { status, message }
}

impl hk_api::reports::ReportControl for PipelineReports {
    fn report(
        &self,
        q: &hk_api::reports::ReportQuery,
    ) -> Result<hk_model::attention::report::SurveyReport, hk_api::reports::ReportFail> {
        self.0
            .report_filtered(q.region, q.span, q.site, q.filter)
            .map_err(report_fail)
    }

    fn export(
        &self,
        q: &hk_api::reports::ReportQuery,
        format: hk_model::attention::report::ExportFormat,
    ) -> Result<(&'static str, Vec<u8>), hk_api::reports::ReportFail> {
        self.0
            .export_filtered(q.region, q.span, q.site, q.filter, format)
            .map_err(report_fail)
    }
}

/// T-088: the run's recipe runtime behind the API's [`hk_api::recipes::RecipeControl`] (public
/// so acceptance tests wire the recipe routes exactly as `hk serve` does, T-094).
/// The run's occupancy engine behind `/api/occupancy` and `/api/channels` (T-118).
pub struct PipelineOccupancy(pub Arc<hk_pipeline::occupancy::OccupancyService>);

impl hk_api::occupancy::OccupancyControl for PipelineOccupancy {
    fn occupancy(
        &self,
        req: &hk_api::occupancy::OccupancyRequest,
    ) -> Result<hk_api::occupancy::OccupancyAnswer, String> {
        use hk_api::occupancy::OccupancyInterval as I;
        let (plan_version, f_cell_hz) = self.0.plan_info();
        let kind_ok = |r: &hk_model::attention::occupancy::OccupancyStat| {
            use hk_model::attention::occupancy::OccupancySubject as S;
            use hk_store::occupancy::SubjectKind as K;
            matches!(
                (req.subject, r.subject),
                (None, _) | (Some(K::Channel), S::Channel { .. }) | (Some(K::Band), S::Band { .. })
            )
        };
        let (rows, truncated) = match req.interval {
            I::Span => {
                let mut rows = self.0.span_stats(req.freq, req.span)?;
                rows.retain(kind_ok);
                let t = rows.len() > req.limit;
                rows.truncate(req.limit);
                (rows, t)
            }
            I::Series(interval) => {
                let r = self.0.query(&hk_store::occupancy::OccupancyQuery {
                    freq: req.freq,
                    span: req.span,
                    interval,
                    subject: req.subject,
                    f_cell_hz,
                    limit: req.limit,
                })?;
                (r.rows, r.truncated)
            }
        };
        Ok(hk_api::occupancy::OccupancyAnswer {
            rows,
            truncated,
            f_cell_hz,
            plan_version,
        })
    }

    fn channels(&self, freq: hk_model::FreqRange) -> hk_api::occupancy::ChannelPlanAnswer {
        let (version, scheme, f_cell_hz, channels) = self.0.channels(freq);
        hk_api::occupancy::ChannelPlanAnswer {
            version,
            scheme,
            f_cell_hz,
            channels,
        }
    }
}

pub struct PipelineRecipes(pub Arc<hk_pipeline::recipes::runtime::RecipeRuntime>);

impl hk_api::recipes::RecipeControl for PipelineRecipes {
    fn call(
        &self,
        call: hk_api::recipes::RecipeCall,
    ) -> Result<serde_json::Value, hk_api::recipes::RecipeFail> {
        use hk_api::recipes::RecipeCall as C;
        let r = &self.0;
        match call {
            C::Blocks => Ok(r.blocks_json()),
            C::ListRecipes => Ok(r.recipes_json()),
            C::GetRecipe { id, version } => r.recipe_json(&id, version),
            C::SaveRecipe(doc) => r.save_json(doc),
            C::ValidateRecipe(doc) => Ok(r.validate_json(doc)),
            C::DeleteRecipe(id) => r.delete_recipe_json(&id),
            C::StartPipeline(body) => r.start_json(body),
            C::ListPipelines => Ok(r.pipelines_json()),
            C::GetPipeline(id) => r.pipeline_json(&id),
            C::EditPipeline { id, recipe } => r.edit_json(&id, recipe),
            C::SavePipeline(id) => r.save_pipeline_json(&id),
            C::StopPipeline(id) => r.stop_json(&id),
            C::SetChannels { id, channels_hz } => r.set_channels(&id, &channels_hz),
            C::RefreshChannels(id) => r.refresh_channels(&id),
        }
        .map_err(|e| hk_api::recipes::RecipeFail {
            detail: e.detail(),
            status: e.status,
            code: e.code,
            message: e.message,
        })
    }
}

/// T-119: the attention routes (sites, baselines, candidates, score weights) over the run's
/// `AttentionService` (`hk_pipeline::attention`).
pub struct PipelineAttention(pub Arc<hk_pipeline::attention::AttentionService>);

/// T-122: the run's novelty alarm service behind `/api/anomalies*`, offering the `anomalies`
/// stream. Dismissals use the engine's sample clock (stream time before any snapshot). T-131: the
/// run opens the service next to the attention loop that feeds it (`PipelineHandle::alarms`,
/// offering the stream through the run's sink); this opens a read-only stand-in only when the
/// run's service could not open.
fn alarm_control(
    handle: &PipelineHandle,
    registry: &StreamRegistry,
    db: &Arc<Mutex<Repository>>,
) -> anyhow::Result<Arc<hk_pipeline::alarms::AlarmService>> {
    if let Some(service) = handle.alarms() {
        return Ok(service);
    }
    let counters = handle.counters();
    let clock = Arc::new(move || {
        let ns = counters
            .stream_time_ns
            .load(std::sync::atomic::Ordering::Relaxed);
        if ns > 0 {
            hk_model::Timestamp::from_unix_nanos(ns)
        } else {
            hk_model::Timestamp::now()
        }
    });
    let reg = registry.clone();
    let sink: hk_pipeline::StreamSink = Arc::new(move |h, p| reg.register(h, p));
    let service = hk_pipeline::alarms::AlarmService::open(Arc::clone(db), Some(&sink), clock)
        .context("opening the novelty alarm service (T-122)")?;
    Ok(Arc::new(service))
}

/// T-122: the novelty alarm service behind the API's [`hk_api::anomalies::AnomalyControl`].
pub struct PipelineAnomalies(pub Arc<hk_pipeline::alarms::AlarmService>);

/// T-166: the same service behind the API's [`hk_api::selections::WatchControl`]. The region
/// watches live with the alarms because they share the run's repository and the `anomalies`
/// stream: a watch alert is an anomaly like any other.
pub struct PipelineWatch(pub Arc<hk_pipeline::alarms::AlarmService>);

impl hk_api::selections::WatchControl for PipelineWatch {
    fn report(
        &self,
        id: hk_model::SelectionId,
    ) -> Result<hk_api::selections::WatchReport, hk_api::selections::WatchFail> {
        use hk_api::selections::{WatchAlertView, WatchFail, WatchSkipView};
        let secs = |t: hk_model::Timestamp| t.as_unix_nanos() as f64 / 1e9;
        let r = self.0.watch_report(id).map_err(|e| match e {
            hk_pipeline::alarms::AlarmFail::NotFound => WatchFail::NotFound,
            hk_pipeline::alarms::AlarmFail::Conflict(m)
            | hk_pipeline::alarms::AlarmFail::Failed(m) => WatchFail::Failed(m),
        })?;
        Ok(hk_api::selections::WatchReport {
            armed: r.armed,
            alerts: r
                .alerts
                .iter()
                .map(|a| WatchAlertView {
                    anomaly_id: a.anomaly.to_string(),
                    emitter: a.emitter.to_string(),
                    f_lo: a.freq.lo_hz,
                    f_hi: a.freq.hi_hz,
                    t: secs(a.t),
                    reason: a.reason.clone(),
                })
                .collect(),
            skipped: r
                .skipped
                .iter()
                .map(|s| WatchSkipView {
                    emitter: s.emitter.to_string(),
                    reason: s.reason.as_str().to_string(),
                    relation: s.relation.as_ref().map(|x| x.kind.as_str().to_string()),
                    artifact: s
                        .relation
                        .as_ref()
                        .and_then(|x| x.artifact)
                        .map(|k| k.as_str().to_string()),
                    source: s.relation.as_ref().map(|x| x.source.to_string()),
                    t: secs(s.t),
                    explanation: s.text.clone(),
                })
                .collect(),
            alerted_total: r.alerted_total,
            suppressed_total: r.suppressed_total,
        })
    }
}

fn anomaly_fail(e: hk_pipeline::alarms::AlarmFail) -> hk_api::anomalies::AnomalyFail {
    use hk_api::anomalies::AnomalyFail as A;
    use hk_pipeline::alarms::AlarmFail as P;
    match e {
        P::NotFound => A::NotFound,
        P::Conflict(m) => A::Conflict(m),
        P::Failed(m) => A::Failed(m),
    }
}

impl hk_api::anomalies::AnomalyControl for PipelineAnomalies {
    fn list(
        &self,
        q: &hk_model::repo::alarms::AnomalyQuery,
    ) -> Result<
        (Vec<hk_model::repo::alarms::AnomalyView>, Option<usize>),
        hk_api::anomalies::AnomalyFail,
    > {
        self.0.list(q).map_err(anomaly_fail)
    }

    fn get(
        &self,
        id: hk_model::AnomalyId,
    ) -> Result<hk_model::repo::alarms::AnomalyView, hk_api::anomalies::AnomalyFail> {
        self.0.get(id).map_err(anomaly_fail)
    }

    fn dismiss(
        &self,
        id: hk_model::AnomalyId,
        note: Option<String>,
    ) -> Result<hk_model::repo::alarms::AnomalyView, hk_api::anomalies::AnomalyFail> {
        self.0.dismiss(id, note).map_err(anomaly_fail)
    }

    fn reopen(
        &self,
        id: hk_model::AnomalyId,
    ) -> Result<hk_model::repo::alarms::AnomalyView, hk_api::anomalies::AnomalyFail> {
        self.0.reopen(id).map_err(anomaly_fail)
    }

    fn suppressions(&self) -> serde_json::Value {
        self.0.suppressions()
    }
}

/// T-119: opens the run's attention service over its database and data directory. Stamps
/// API-created sites and weight rows with stream time (the wall clock before any frame).
fn attention_control(
    handle: &PipelineHandle,
    db: &Arc<Mutex<Repository>>,
) -> anyhow::Result<Arc<dyn hk_api::attention::AttentionControl>> {
    // T-128: the run's own service (fed by occupancy and the scheduler) when it opened.
    if let Some(service) = handle.attention() {
        return Ok(Arc::new(PipelineAttention(service)));
    }
    let counters = handle.counters();
    let clock_counters = Arc::clone(&counters);
    let clock = Arc::new(move || {
        let ns = clock_counters
            .stream_time_ns
            .load(std::sync::atomic::Ordering::Relaxed);
        if ns > 0 {
            hk_model::Timestamp::from_unix_nanos(ns)
        } else {
            hk_model::Timestamp::now()
        }
    });
    // T-303: this fallback opens a service for a handle that has none of its own, so no front end
    // has named itself; the run's service (the branch above) carries the run's chain.
    let service = hk_pipeline::attention::AttentionService::open(
        handle.data_dir(),
        Arc::clone(db),
        hk_model::attention::baseline::ChainKey::Unknown,
        Some(counters),
        clock,
    )
    .context("opening the attention service (T-119)")?;
    Ok(Arc::new(PipelineAttention(Arc::new(service))))
}

impl hk_api::attention::AttentionControl for PipelineAttention {
    fn call(
        &self,
        call: hk_api::attention::AttentionCall,
    ) -> Result<hk_api::attention::AttentionAnswer, hk_api::attention::AttentionFail> {
        use hk_api::attention::{AttentionAnswer as A, AttentionCall as C};
        let s = &self.0;
        let changed = |old: serde_json::Value, new: serde_json::Value| A {
            body: new.clone(),
            old,
            new,
        };
        match call {
            C::Sites => Ok(A::read(s.sites_json())),
            C::CurrentSite => Ok(A::read(s.current_site_json())),
            C::SelectSite(b) => {
                let old = s.current_site_json();
                s.set_current_site(hk_pipeline::attention::SiteSelect {
                    id: b.id,
                    name: b.name,
                    lat_deg: b.lat_deg,
                    lon_deg: b.lon_deg,
                    radius_m: b.radius_m,
                    utc_offset_min: b.utc_offset_min,
                    release: b.release,
                })
                .map(|new| changed(old, new))
            }
            C::UpdateSite {
                id,
                name,
                utc_offset_min,
            } => s
                .update_site(id, name, utc_offset_min)
                .map(|(old, new)| changed(old, new)),
            C::Baselines { site } => s.baselines_json(site).map(A::read),
            C::Slots {
                site,
                f_lo,
                f_hi,
                slot,
                resolution,
            } => s
                .slots_json(site, f_lo, f_hi, slot, resolution)
                .map(A::read),
            C::Refreeze { site, f_lo, f_hi } => s
                .refreeze(site, f_lo, f_hi)
                .map(|v| changed(serde_json::Value::Null, v)),
            C::Candidates { f_lo, f_hi, limit } => {
                Ok(A::read(s.candidates_json(f_lo, f_hi, limit)))
            }
            C::Weights => s.weights_json().map(A::read),
            C::SetWeights { weights, author } => {
                s.set_weights(weights, &author).map(|(old, new)| A {
                    body: serde_json::json!({ "weights": new }),
                    old: serde_json::json!(old),
                    new: serde_json::json!(new),
                })
            }
        }
        .map_err(|e| hk_api::attention::AttentionFail {
            status: e.status,
            code: e.code,
            message: e.message,
        })
    }
}

/// Starts the API server over a running pipeline: streams, history/floor, status, the inventory
/// the pipeline writes, the control API (display, recording and bookmarks, audited to
/// `<data dir>/control-audit.jsonl`), and (live runs without the scheduler) the live control
/// handles for device settings.
///
/// `live_controls` is **every** front end this run holds (T-511): none for a replay or a
/// scheduler-driven run, one for today's single-SDR run, N when N are composed. `Option<Arc<dyn
/// LiveControl>>` converts into it, so a single-device caller writes what it always did.
pub fn serve_api(
    bind: SocketAddr,
    ui_dist: Option<PathBuf>,
    registry: &StreamRegistry,
    handle: &PipelineHandle,
    token: Token,
    tag: &str,
    live_controls: impl Into<hk_api::LiveControls>,
) -> anyhow::Result<Server> {
    let live_controls = live_controls.into();
    let counters = handle.counters();
    // The pipeline writes the inventory (TrackInventory, chain record writers, plugin Ingest)
    // into this database; the API reads it through `query_inventory` only. Bookmarks live in the
    // same database.
    let db = Arc::new(Mutex::new(
        Repository::open(handle.data_dir().join("hackriff.db"))
            .context("opening the inventory database for the API")?,
    ));
    let report_db = Arc::clone(&db); // T-121
    let audit_path = handle.data_dir().join("control-audit.jsonl");
    let audit = AuditLog::open(&audit_path)
        .with_context(|| format!("opening the control audit log {}", audit_path.display()))?;
    let controller = handle.controller();
    let status_ctl = controller.clone();
    // On-demand streams (T-043 listen, T-060 burst bits and symbols, T-165 channelised IQ), over
    // WebSocket and TCP.
    let recipes = handle.recipe_runtime();
    // T-463: historical playback - the one playhead, reading raw IQ from the ring and the
    // persisted recordings; its opener re-runs demod/decode, never detection.
    let playback = Arc::new(hk_pipeline::playback::PlaybackService::new(
        Arc::new(hk_pipeline::playback::RunIq::new(
            handle.iq_buffer(),
            Some((
                handle.data_dir().join("hackriff.db"),
                handle.data_dir().to_path_buf(),
            )),
        )),
        hk_pipeline::playback::PlaybackConfig::default(),
    ));
    // T-859 (MAUTO M-8): region-analyze jobs over the run's IQ ring. No search backend yet
    // (`server_backend()` is `None`: no production IQ evaluator), so a job acquires and then says
    // it searched nothing.
    // T-860 (MAUTO M-9): a finished job attaches to the run's inventory and may confirm through
    // `ConfirmPolicy.synthesized`.
    let analyze = {
        let tuned_ctl = controller.clone();
        let reg = registry.clone();
        let attach_sink: hk_pipeline::StreamSink = Arc::new(move |h, p| reg.register(h, p));
        Arc::new(hk_pipeline::synth::jobs::AnalyzeJobs::with_attacher(
            Arc::new(hk_pipeline::synth::jobs::RingJobEnv::new(
                handle.iq_buffer(),
                Box::new(move || {
                    let s = tuned_ctl.status();
                    (!s.finished)
                        .then(|| hk_model::FreqRange::centered(s.center_hz, s.sample_rate_hz))
                }),
            )),
            // The one shared choice (T-863): the ADR-0015 §7 suite reads the same function.
            hk_pipeline::synth::jobs::server_backend(),
            hk_pipeline::synth::jobs::PowerPolicy::Mains,
            // T-884 item 1: the run's **configured** `ConfirmPolicy.synthesized`, read from the
            // inventory that holds it — never a fresh default, which would make the configuration
            // field unreadable on the one path that gates an irreversible confirm.
            // T-884 item 2: and a `messages` publisher, so a decode a job attaches to a known
            // emitter reaches the stream like every other stored decode.
            Some(Arc::new(
                hk_pipeline::synth::jobs::RepoAttacher::with_stream(
                    handle.data_dir().join("hackriff.db"),
                    handle.synthesized_confirm(),
                    Some(&attach_sink),
                    controller.status().content_class,
                ),
            )),
        ))
    };
    let openers = hk_api::stream::OpenerRegistry::new()
        .with("listen", handle.listen_service())
        .with("bits", handle.bits_service())
        .with("symbols", handle.symbols_service())
        .with("iq", handle.iq_service()) // T-165
        .with("stage", recipes.stage_service()) // T-088
        .with("inspector", recipes.inspector_service()) // T-088 (T-089/T-092 extend it)
        .with(
            "playback",
            Arc::clone(&playback) as Arc<dyn hk_api::stream::StreamOpener>,
        ) // T-463
        .with(
            "analyze",
            Arc::new(hk_pipeline::synth::jobs::AnalyzeOpener(Arc::clone(
                &analyze,
            ))) as Arc<dyn hk_api::stream::StreamOpener>,
        ); // T-859
    let tcp = start_stream_tcp(registry, &openers, &token)?;
    let attention = attention_control(handle, &db)?; // T-119
    let alarms = alarm_control(handle, registry, &db)?; // T-122
    let state = ApiState {
        streams: registry.clone(),
        history: None,
        // T-439: the de-welded view lattice, whose finest node is the growing edge.
        view_history: handle.view_history(),
        floor: Some(handle.floor_product()),
        inventory: Some(Arc::clone(&db)),
        trunking: Some(Arc::clone(&db)), // T-273: same run database, grant_event table (C23)
        // T-891: the run's accessory-fed VLF services (an empty list without an accessory).
        vlf: Some(Arc::new(PipelineVlf(handle.vlf()))),
        status: Some(Arc::new(move || {
            let mut v = counters.to_json();
            if let Some(o) = v.as_object_mut() {
                o.insert(
                    "control".into(),
                    serde_json::to_value(status_ctl.status()).unwrap_or_default(),
                );
            }
            v
        })),
        // T-452: the in-app survey sweep, over the same front end the five device routes move.
        // It exists exactly when a live front end does — a replay has nothing to retune — and it
        // is a driver over the interactive retune path, not the scheduler this run does not drive.
        // T-517: with the run's own bin width, so a coarse step widens the window only where the
        // detection/history bins stay exactly as wide.
        // T-511: the sweep drives **one** front end — the run's default. A per-device sweep is a
        // separate decision (`crate::scan`'s arbitration is written for one radio), and the wire
        // says which one it commissions.
        scan: live_controls.primary().cloned().map(|lc| {
            let fft = handle.detection_fft_len();
            Arc::new(
                hk_api::scan::ScanRunner::new(lc)
                    .with_bin_width(Arc::new(move |fs| hk_pipeline::detection_bin_hz(fs, fft))),
            )
        }),
        live_controls,
        run_control: Some(Arc::new(PipelineRunControl(controller))),
        bookmarks: Some(db),
        audit: Some(Arc::new(audit)),
        on_demand: openers,
        outputs: Some(Arc::new(PipelineOutputs(handle.output_recorders()))),
        recipes: Some(Arc::new(PipelineRecipes(recipes))),
        // T-092: the run's always-on decoded-stream capture store.
        captures: handle
            .decoded_captures()
            .map(|c| Arc::new(c) as Arc<dyn hk_api::stream::inspector::CaptureSource>),
        occupancy: Some(Arc::new(PipelineOccupancy(handle.occupancy()))), // T-118
        observations: handle.observation_store(),                         // T-115
        attention: Some(attention),                                       // T-119
        scheduler: Some(Arc::new(PipelineScheduler(handle.scheduler_hub()))), // T-127
        reports: Some(Arc::new(PipelineReports(
            ReportService::new(
                None,
                Some(handle.floor_product()),
                handle.observation_store(),
                Arc::clone(&report_db),
            )
            .with_attention(Some(handle.occupancy()), handle.attention()), // T-128
        ))), // T-121
        watch: Some(Arc::new(PipelineWatch(Arc::clone(&alarms)))),        // T-166
        anomalies: Some(Arc::new(PipelineAnomalies(alarms))),             // T-122
        iq_buffer: Some(Arc::new(PipelineIqBuffer(handle.iq_buffer()))),  // T-157
        analyze: Some(Arc::new(PipelineAnalyze(analyze))),                // T-859
        datasets: Some(Arc::new(PipelineDatasets::new(
            handle.data_dir().join("hackriff.db"),
            handle.iq_buffer(),
            handle.data_dir(),
        ))), // T-205
        // T-469: the persisted IQ recordings that extend the audio horizon past the ring.
        recordings: Some(Arc::new(PipelineRecordings::new(
            handle.data_dir().join("hackriff.db"),
            handle.data_dir().to_path_buf(),
        ))),
        // T-463: the one playhead of historical playback.
        playback: Some(Arc::new(PipelinePlayback(playback))),
        // T-438: the tile route's ingest-backpressure cap, per server.
        tile_admission: Default::default(),
        // T-572: the hot-tile LRU, on for a served run. A viewport that has not moved re-reads the
        // same SEALED tiles every poll, and a sealed tile can never change again. Live tiles at the
        // growing edge are never cached — see `HotTileCache`.
        tile_cache: Some(Arc::new(hk_api::tiles::HotTileCache::default())),
        row_feeds: Default::default(),
        // T-579: the tile route's memoised geometry, per server — the readable ceiling per
        // lattice and the coverage raster keyed on the tune-history evidence it is drawn from.
        ceiling_memo: Default::default(),
        coverage_raster: Default::default(),
    };
    let mut config = ServerConfig::new(bind, token.clone());
    config.ui_dist = ui_dist;
    config.stream_tcp = Some(tcp.local_addr());
    let mut server = Server::start(config, state).context("starting the HTTP server")?;
    let addr = server.local_addr();
    eprintln!("{tag}: listening on {addr}");
    eprintln!(
        "{tag}: streams over TCP on {} (send `<stream_id>?token=<token>` or \
         `open/bits?token=<token>` and a newline; discovery: GET /api/streams)",
        tcp.local_addr()
    );
    server.attach_stream_server(tcp);
    eprintln!(
        "{tag}: control API audited to {} (token id {})",
        audit_path.display(),
        token.id()
    );
    if !addr.ip().is_loopback() {
        eprintln!(
            "{tag}: WARNING: bound to a non-loopback address; anyone on the network who learns \
             the token (sent in cleartext, no TLS) can read the API"
        );
    }
    let host = if addr.ip().is_unspecified() {
        SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), addr.port())
    } else {
        addr
    };
    // The token rides in the fragment: never sent to the server or logged, stripped by the page.
    println!("open http://{host}/#token={}", token.expose());
    Ok(server)
}

/// The TCP stream server (T-060): `HK_STREAM_TCP` (e.g. `0.0.0.0:8788`) if set, else loopback
/// port 8788, or an ephemeral loopback port when 8788 is taken.
fn start_stream_tcp(
    registry: &StreamRegistry,
    openers: &hk_api::stream::OpenerRegistry,
    token: &Token,
) -> anyhow::Result<hk_api::StreamServer> {
    let start = |bind: SocketAddr| {
        hk_api::StreamServer::start(
            hk_api::StreamServerConfig::new(bind, token.clone()),
            registry.clone(),
            openers.clone(),
        )
    };
    if let Ok(v) = std::env::var("HK_STREAM_TCP") {
        let bind: SocketAddr = v
            .parse()
            .with_context(|| format!("HK_STREAM_TCP={v:?} is not an address"))?;
        return start(bind).with_context(|| format!("binding the TCP stream server on {bind}"));
    }
    let loopback = std::net::Ipv4Addr::LOCALHOST.into();
    start(SocketAddr::new(
        loopback,
        hk_api::tcp::DEFAULT_STREAM_TCP_PORT,
    ))
    .or_else(|_| start(SocketAddr::new(loopback, 0)))
    .context("binding the TCP stream server")
}

pub(crate) fn config_for(
    data_dir: PathBuf,
    plan: ScanPlan,
    registry: &StreamRegistry,
    feeds: Option<PathBuf>,
    calibration: Option<&Path>,
) -> anyhow::Result<PipelineConfig> {
    let mut cfg = PipelineConfig::new(data_dir, plan)?;
    // The IQ capture buffer is on for the composed daemon (the library default is off, T-178).
    cfg.iq_buffer = hk_store::iqbuffer::IqBufferConfig::from_env();
    // T-904: per-frame detection retention (1 hour, rolled up; HK_DETECTION_RETENTION, …) and the
    // `/api/status` `storage` figures. Off for the library default.
    cfg.retention = Some(hk_pipeline::retention::RetentionSettings::from_env());
    let reg = registry.clone();
    cfg.stream_sink = Some(Arc::new(move |h, p| reg.register(h, p)));
    let reg = registry.clone();
    cfg.stream_unsink = Some(Arc::new(move |id| {
        reg.unregister(id);
    }));
    cfg.feeds_dir = feeds;
    if let Some(path) = calibration {
        cfg.calibrations = load_calibrations(path)?;
    }
    Ok(cfg)
}

/// `hackrf` → the first device, `hackrf:<serial>` → that one; anything else is not a HackRF
/// source.
pub fn hackrf_serial(spec: &str) -> Option<Option<String>> {
    device_serial("hackrf", spec)
}

/// `rtlsdr` → the first dongle, `rtlsdr:<serial>` → that one (T-514).
///
/// **Prefer the serial form.** A USB index changes when anything else is plugged in, so `rtlsdr`
/// alone can open a different radio between two runs and file its measurements under the wrong
/// provenance; `rtl_test -t` lists the serials.
pub fn rtlsdr_serial(spec: &str) -> Option<Option<String>> {
    device_serial("rtlsdr", spec)
}

/// `<driver>` → the first device, `<driver>:<serial>` → that one; `None` for any other spec.
fn device_serial(driver: &str, spec: &str) -> Option<Option<String>> {
    if spec == driver {
        return Some(None);
    }
    spec.strip_prefix(driver)
        .and_then(|rest| rest.strip_prefix(':'))
        .filter(|serial| !serial.is_empty())
        .map(|serial| Some(serial.to_owned()))
}

/// The content class of every window tiling `plan`'s regions at rate `fs`.
pub fn plan_class(plan: &ScanPlan, fs: f64) -> ContentClass {
    let mut centres = Vec::new();
    for r in &plan.regions {
        let (lo, hi) = (r.freq.lo_hz, r.freq.hi_hz);
        if hi - lo <= fs {
            centres.push(0.5 * (lo + hi));
        } else {
            let mut c = lo + fs / 2.0;
            while c - fs / 2.0 < hi {
                centres.push(c);
                c += fs;
            }
        }
    }
    band_class(&centres, fs)
}

/// `GET /api/vlf` over the run's [`hk_pipeline::vlf::VlfServices`] (T-891).
struct PipelineVlf(Arc<hk_pipeline::vlf::VlfServices>);

impl hk_api::vlf::VlfControl for PipelineVlf {
    fn reports(&self, include_points: bool) -> Vec<serde_json::Value> {
        self.0
            .list()
            .iter()
            .map(|s| serde_json::to_value(s.report(include_points)).unwrap_or_default())
            .collect()
    }
}

/// `mock:<file.sigmf-meta>` → the recording behind the mock SDR device (T-049).
pub fn mock_path(spec: &str) -> Option<PathBuf> {
    spec.strip_prefix("mock:")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
}

/// T-891: `spec` names an **accessory** — `vlf-mock:<file.sigmf-meta>` (the mock VLF receiver,
/// a real-valued recording in real time) or `vlf[:<device>]` (a live soundcard receiver). An
/// accessory is attached beside the radio and feeds the VLF service (`hk_pipeline::vlf`, served at
/// `GET /api/vlf`), never the RF pipeline.
pub fn is_accessory_spec(spec: &str) -> bool {
    spec == "vlf" || spec.starts_with("vlf:") || spec.starts_with("vlf-mock:")
}

/// Opens the accessory `spec` names ([`is_accessory_spec`]).
pub fn open_accessory(spec: &str) -> anyhow::Result<Box<dyn Source>> {
    if let Some(path) = spec.strip_prefix("vlf-mock:").filter(|p| !p.is_empty()) {
        let driver = hk_core::source::AccessoryMockDriver::new(
            path,
            hk_core::source::AccessoryMockOptions {
                realtime: true,
                ..Default::default()
            },
        )
        .with_context(|| format!("opening the mock VLF accessory recording {path}"))?;
        return Ok(driver.open(&driver.default_request())?);
    }
    anyhow::bail!(
        "{spec}: a live VLF soundcard receiver needs an audio backend, and none is linked in this \
         build; use vlf-mock:<file.sigmf-meta> to replay a recording through the accessory"
    )
}

/// `spec` names a device (the live HackRF One or the mock SDR), not a recording played back.
pub fn is_device_spec(spec: &str) -> bool {
    hackrf_serial(spec).is_some() || rtlsdr_serial(spec).is_some() || mock_path(spec).is_some()
}

/// The mock device as the binaries run it: like the radio, in real time from the wall clock,
/// never ending (the recording loops).
pub fn cli_mock_options() -> MockOptions {
    MockOptions {
        block_len: 16_384,
        pacing: Pacing::RealTime { speed: 1.0 },
        end: MockEnd::Loop,
        clock: MockClock::Wall,
        ..MockOptions::default()
    }
}

/// [`cli_mock_options`] for the recording at `path`. A time-compressed scene (captures leaving
/// `core:global_index` gaps between them, T-125) keeps the recording's clock, so block times are
/// the scene's simulated times, not the wall clock at open.
pub fn cli_mock_options_for(path: &Path) -> MockOptions {
    let scene = hk_core::SigmfReplaySource::open(path, hk_core::ReplayOptions::default())
        .is_ok_and(|r| !r.recording_gaps().is_empty());
    MockOptions {
        clock: if scene {
            MockClock::Recording
        } else {
            MockClock::Wall
        },
        ..cli_mock_options()
    }
}

/// The environment variable that arms a **mock SDR** fault ([`hk_core::MockFault::parse`]):
/// `retune-apply-fails[:N|:always]` and `gone-on-retune` (T-508), plus `read-fails-every:N` and
/// `refuse-rate[:N|:always]` (T-541). It reaches only `--device mock:…`; a real radio has no such
/// switch. The browser tier sets it so a retune guard can go red — every one of them was green on
/// a mock that always lands where it is told — and `ops/fuzz-rig.sh` sets it so a soak run meets a
/// device that keeps misbehaving rather than one that cannot fail.
pub const MOCK_FAULT_ENV: &str = "HK_MOCK_FAULT";

/// The fault [`MOCK_FAULT_ENV`] arms, if any. An unparseable spec is an error, never ignored: a
/// harness that asked for a fault and silently got none would be a green guard over nothing.
pub fn mock_fault_from_env() -> anyhow::Result<Option<hk_core::MockFault>> {
    match std::env::var(MOCK_FAULT_ENV) {
        Ok(spec) => hk_core::MockFault::parse(&spec)
            .map_err(|e| anyhow::anyhow!("{MOCK_FAULT_ENV}={spec:?}: {e}")),
        Err(_) => Ok(None),
    }
}

/// The driver for a live source spec: `hackrf` / `hackrf:<serial>` (HackRF One),
/// `rtlsdr` / `rtlsdr:<serial>` (RTL-SDR, T-514) or `mock:<file.sigmf-meta>` (the mock SDR,
/// `None` if the recording cannot be opened; [`open_live`] reports why). SoapySDR plugs in here
/// later behind the same `SourceDriver` contract.
///
/// Each driver reports its own capabilities, and the open request is built from them
/// ([`LiveArgs::named_gains`] only sets stages the device has), so `--lna` reaches the RTL-SDR's
/// one combined gain knob and `--vga` / `--amp` are simply not offered there.
pub fn driver_for(spec: &str) -> Option<(Box<dyn SourceDriver>, Option<String>)> {
    if let Some(path) = mock_path(spec) {
        let options = cli_mock_options_for(&path);
        let driver = MockSdrDriver::new(path, options).ok()?;
        return Some((Box::new(driver), None));
    }
    if let Some(serial) = rtlsdr_serial(spec) {
        return Some((Box::new(RtlSdrDriver) as Box<dyn SourceDriver>, serial));
    }
    hackrf_serial(spec).map(|serial| (Box::new(HackRfDriver) as Box<dyn SourceDriver>, serial))
}

/// The mock's open request: the recording's own centre, rate and gains, with every `live` setting
/// given explicitly (different from its default) applied on top.
fn mock_request(live: &LiveArgs, driver: &MockSdrDriver) -> anyhow::Result<OpenRequest> {
    let d = LiveArgs::default();
    let mut request = driver.default_request();
    if live.center_hz != d.center_hz {
        request.center_hz = live.center_hz;
    }
    if live.sample_rate_hz != d.sample_rate_hz {
        request.sample_rate_hz = live.sample_rate_hz;
    }
    request.baseband_filter_hz = live.baseband_filter_hz;
    let named: Vec<&str> = live
        .gains
        .iter()
        .filter_map(|g| g.split_once('=').map(|(s, _)| s.trim()))
        .collect();
    for g in live.named_gains(&driver.capabilities())? {
        let explicit = named.contains(&g.stage.as_str())
            || match g.stage.as_str() {
                "lna" => live.lna_db != d.lna_db,
                "vga" => live.vga_db != d.vga_db,
                "amp" => live.amp != d.amp,
                _ => false,
            };
        if explicit {
            request.gains.retain(|r| r.stage != g.stage);
            request.gains.push(g);
        }
    }
    Ok(request)
}

/// A recording opened for a pipeline run.
pub struct OpenedRecording {
    /// The stream.
    pub source: Box<dyn Source>,
    /// Rate, centre, start.
    pub info: SourceInfo,
    /// The recording's class.
    pub class: ContentClass,
    /// Provenance device id, when known.
    pub device_id: Option<String>,
    /// SigMF `core:hw`, when known.
    pub device_hw: Option<String>,
    /// The device's control handle (mock only).
    pub control: Option<Arc<dyn SourceControl>>,
}

/// Opens a recording: played back as recorded, or — when the scheduler retunes it — behind the
/// mock SDR device, whose retunes really shift the IQ (T-057).
pub fn open_recording(
    path: &Path,
    pacing: Pacing,
    retuned: bool,
) -> anyhow::Result<OpenedRecording> {
    if retuned {
        let r = open_mock_replay(path, pacing, MockEnd::Stop)?;
        let control = r.source.control();
        return Ok(OpenedRecording {
            source: Box::new(r.source),
            info: r.info,
            class: r.class,
            device_id: Some(r.device.device_id),
            device_hw: Some(r.device.hw),
            control: Some(control),
        });
    }
    let r = open_replay(path, pacing, false)?;
    let hw = r.meta.global.hw.clone();
    Ok(OpenedRecording {
        source: Box::new(r.source),
        info: r.info,
        class: r.class,
        device_id: hw.as_ref().map(|hw| format!("sigmf:{hw}")),
        device_hw: hw,
        control: None,
    })
}

/// An opened live source.
pub struct LiveSource {
    /// The stream (moved into the pipeline).
    pub source: Box<dyn Source>,
    /// Its control handle (tuning, named gains, stats, identity).
    pub control: Arc<dyn SourceControl>,
    /// Rate, centre, start time.
    pub info: SourceInfo,
    /// Named gains in force (quantised by the capabilities).
    pub gains: Vec<NamedGain>,
    /// Bias-tee state the source was opened with (T-325): `Off`/`On` for a device that has a bias
    /// tee, `Unknown` for one that has none. Never a fabricated `Off`.
    pub bias_tee: hk_model::BiasTee,
    /// Identity (provenance `device_id`, SigMF `core:hw`).
    pub device: DeviceInfo,
}

/// Opens a live source (receive only) through its driver with `live`'s settings.
pub fn open_live(spec: &str, live: &LiveArgs) -> anyhow::Result<LiveSource> {
    // The stream's start: the wall clock for a radio, the recording's clock for a scene (T-125).
    let mut clock_start = None;
    let (driver, request): (Box<dyn SourceDriver>, OpenRequest) =
        if let Some(path) = mock_path(spec) {
            let options = MockOptions {
                fault: mock_fault_from_env()?,
                ..cli_mock_options_for(&path)
            };
            if let Some(f) = options.fault {
                eprintln!("mock SDR: fault armed from {MOCK_FAULT_ENV}: {f:?}");
            }
            let driver = MockSdrDriver::new(&path, options)
                .with_context(|| format!("opening the mock device over {}", path.display()))?;
            if driver.options().clock == MockClock::Recording {
                clock_start = Some(driver.recording().start_time);
            }
            let request = mock_request(live, &driver)?;
            (Box::new(driver), request)
        } else if let Some((driver, device)) = driver_for(spec) {
            let gains = live.named_gains(&driver.capabilities())?;
            let request = OpenRequest {
                device,
                center_hz: live.center_hz,
                sample_rate_hz: live.sample_rate_hz,
                gains,
                baseband_filter_hz: live.baseband_filter_hz,
                bias_tee: false,
            };
            (driver, request)
        } else {
            anyhow::bail!(
                "unsupported live source {spec:?}: use hackrf, hackrf:<serial>, rtlsdr, \
             rtlsdr:<serial> or mock:<file.sigmf-meta>"
            );
        };
    let caps = driver.capabilities();
    let gains = request.gains.clone();
    let source = driver
        .open(&request)
        .with_context(|| format!("opening the {} source", driver.name()))?;
    let control = source.control();
    let device = control.device_info().unwrap_or_else(|| DeviceInfo {
        driver: driver.name().into(),
        device_id: driver.name().into(),
        hw: driver.name().into(),
    });
    eprintln!("{}: {}", driver.name(), device.hw);
    let gains = gains
        .into_iter()
        .filter_map(|g| {
            let db = caps.gain_stage(&g.stage)?.quantise(g.db)?;
            Some(NamedGain::new(g.stage, db))
        })
        .collect();
    Ok(LiveSource {
        source,
        control,
        info: SourceInfo {
            sample_rate_hz: request.sample_rate_hz,
            center_hz: request.center_hz.round(),
            start_time: clock_start.unwrap_or_else(Timestamp::now),
        },
        gains,
        bias_tee: if caps.bias_tee {
            if request.bias_tee {
                hk_model::BiasTee::On
            } else {
                hk_model::BiasTee::Off
            }
        } else {
            hk_model::BiasTee::Unknown
        },
        device,
    })
}

/// Resolves repeatable `--device SPEC` and `--device-set SPEC:KEY=VALUE` into one
/// `(spec, settings)` per device (T-512). `base` is the shared `--center-hz`/`--rate`/gain
/// defaults; a `--device-set` overrides them for **one** device. Keys: `center-hz`, `rate`, `lna`,
/// `vga`, `amp` (`true`/`false`), `baseband-filter-hz`, `gain.STAGE` (dB). An empty, duplicated
/// or unknown device, or an unknown key, is an error naming it, never a silent drop.
pub fn resolve_devices(
    specs: &[String],
    sets: &[String],
    base: &LiveArgs,
) -> anyhow::Result<Vec<(String, LiveArgs)>> {
    let mut out: Vec<(String, LiveArgs)> = Vec::new();
    for spec in specs {
        if spec.trim().is_empty() {
            anyhow::bail!("--device: empty device spec");
        }
        if out.iter().any(|(s, _)| s == spec) {
            anyhow::bail!("--device {spec:?} given twice: each device may be named once");
        }
        out.push((spec.clone(), base.clone()));
    }
    for set in sets {
        let (lhs, value) = set
            .split_once('=')
            .with_context(|| format!("--device-set {set:?}: expected SPEC:KEY=VALUE"))?;
        let (spec, key) = lhs
            .rsplit_once(':')
            .with_context(|| format!("--device-set {set:?}: expected SPEC:KEY=VALUE"))?;
        let Some((_, live)) = out.iter_mut().find(|(s, _)| s == spec) else {
            anyhow::bail!(
                "--device-set {set:?}: no --device {spec:?} (devices: {})",
                out.iter()
                    .map(|(s, _)| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        };
        let num = |v: &str| {
            v.trim()
                .parse::<f64>()
                .with_context(|| format!("--device-set {set:?}: {v:?} is not a number"))
        };
        match key {
            "center-hz" => live.center_hz = num(value)?,
            "rate" => live.sample_rate_hz = num(value)?,
            "lna" => live.lna_db = num(value)?,
            "vga" => live.vga_db = num(value)?,
            "baseband-filter-hz" => live.baseband_filter_hz = Some(num(value)?),
            "amp" => {
                live.amp = value
                    .trim()
                    .parse::<bool>()
                    .with_context(|| format!("--device-set {set:?}: amp is true or false"))?
            }
            k if k.starts_with("gain.") => {
                num(value)?;
                live.gains.push(format!("{}={}", &k[5..], value.trim()));
            }
            _ => anyhow::bail!(
                "--device-set {set:?}: unknown key {key:?} (center-hz, rate, lna, vga, amp, \
                 baseband-filter-hz, gain.STAGE)"
            ),
        }
    }
    Ok(out)
}

/// [`resolve_devices`] with the single-device default folded in (T-512): with no `--device` the
/// run's one device is `default_spec` (`--source`, `--hackrf`), so `--device-set` addresses it too
/// and one device behaves exactly as before. Returns the primary (the first device) and the rest.
#[allow(clippy::type_complexity)]
pub fn resolve_primary(
    default_spec: String,
    devices: &[String],
    sets: &[String],
    base: &LiveArgs,
) -> anyhow::Result<(String, LiveArgs, Vec<(String, LiveArgs)>)> {
    if devices.is_empty() && sets.is_empty() {
        return Ok((default_spec, base.clone(), Vec::new()));
    }
    let specs = if devices.is_empty() {
        vec![default_spec]
    } else {
        devices.to_vec()
    };
    let mut all = resolve_devices(&specs, sets, base)?.into_iter();
    let (spec, live) = all.next().context("no device")?;
    Ok((spec, live, all.collect()))
}

/// A live pipeline start request.
#[derive(Clone, Debug)]
pub struct LiveOptions {
    /// `hackrf` or `hackrf:<serial>`.
    pub source: String,
    /// Tuning and gains.
    pub live: LiveArgs,
    /// Further front ends (T-512): each a device spec with its own resolved settings, opened
    /// beside the primary and composed by `Pipeline::start_multi` (T-510). Empty = one device.
    pub extra: Vec<(String, LiveArgs)>,
    /// Data directory.
    pub data_dir: PathBuf,
    /// ScanPlan JSON.
    pub plan: Option<PathBuf>,
    /// Iterative scan (T-406): step the tune across the device's tunable range, dwelling this
    /// many seconds per step. `None` keeps the single-region default plan.
    pub survey_dwell_s: Option<f64>,
    /// Drive the attention scheduler (then no live control handle is offered).
    pub schedule: bool,
    /// Offline feed cache for the correlator.
    pub feeds: Option<PathBuf>,
    /// T-021 calibration JSON.
    pub calibration: Option<PathBuf>,
    /// Spectrum stream FFT length override.
    pub spectrum_fft_len: Option<usize>,
    /// Spectrum stream row rate override.
    pub spectrum_rows_per_s: Option<f64>,
    /// Compute provider (T-056).
    pub compute: ComputeArgs,
    /// Rolling IQ capture buffer retention and cap (T-157).
    pub iq_buffer: IqBufferArgs,
    /// Test-only override of the ring's allocation (T-217); `None` uses the real allocator.
    pub iq_buffer_hooks: Option<IqBufferHooksOverride>,
}

/// A running live pipeline.
pub struct LivePipeline {
    /// The pipeline.
    pub handle: PipelineHandle,
    /// The source's control handle (stats, identity).
    pub control: Arc<dyn SourceControl>,
    /// The API control handle (`None` when the scheduler drives the radio).
    pub live_control: Option<Arc<dyn LiveControl>>,
    /// Every front end's control (T-512): the primary's first, then each further device's (which
    /// refuses rate changes, as the pipeline does: its ring is built for the rate it opened at).
    /// Empty when the scheduler drives the radio.
    pub live_controls: Vec<Arc<dyn LiveControl>>,
    /// The run's content class.
    pub class: ContentClass,
}

/// Starts the whole pipeline over the live HackRF One (see the module docs).
pub fn start_live(opts: &LiveOptions, registry: &StreamRegistry) -> anyhow::Result<LivePipeline> {
    let live = open_live(&opts.source, &opts.live)?;
    let caps = live.source.capabilities();
    let plan = plan_for_run(opts.plan.as_deref(), opts.survey_dwell_s, &live.info, caps)?;
    let fs = live.info.sample_rate_hz;
    let class = if opts.schedule {
        plan_class(&plan, fs)
    } else {
        band_class(&[live.info.center_hz], fs)
    };
    // The mock device's class is band-derived from the tuned window exactly as the radio's, so the
    // control API's retunes re-derive and re-plumb it the same way (T-050).
    let mut cfg = config_for(
        opts.data_dir.clone(),
        plan,
        registry,
        opts.feeds.clone(),
        opts.calibration.as_deref(),
    )?;
    if let Some(n) = opts.spectrum_fft_len {
        cfg.settings.spectrum_fft_len = n;
    }
    if let Some(r) = opts.spectrum_rows_per_s {
        cfg.settings.spectrum_rows_per_s = r;
    }
    opts.compute.apply(&mut cfg.settings);
    opts.iq_buffer
        .apply(&mut cfg.iq_buffer, max_rate_hz(live.control.capabilities()));
    if let Some(h) = &opts.iq_buffer_hooks {
        cfg.iq_buffer_hooks = Some(Arc::clone(&h.0));
    }
    cfg.source_class = class;
    // A radio cannot pause: never lossless (Pipeline::start would refuse it anyway).
    cfg.lossless = false;
    cfg.drive_scheduler = opts.schedule;
    // Without the scheduler the class follows the tuned window: retunes re-plumb into the
    // window's class, and blocks of any other class are dropped before the ring (T-050).
    cfg.live_window_class = !opts.schedule;
    cfg.device_id = live.device.device_id.clone();
    cfg.device_hw = Some(live.device.hw.clone());
    let initial = LiveTuning {
        center_hz: live.info.center_hz,
        sample_rate_hz: fs,
        gains: live.gains.clone(),
        // T-325: what the source was actually opened with, never a `false` fabricated from the
        // mere existence of the capability.
        bias_tee: live.bias_tee,
        // The device's own default (usually derived from the sample rate); unknown until a
        // control request sets it explicitly (T-067).
        baseband_filter_hz: None,
    };
    let control = Arc::clone(&live.control);
    // T-512: further front ends open before anything starts, so a missing one fails the run
    // whole rather than leaving a half-started pipeline.
    let mut extra = Vec::new();
    let mut extra_controls = Vec::new();
    // T-891: accessories open here too (a missing one fails the run whole), but feed the VLF
    // service, not the RF pipeline.
    let mut accessories = Vec::new();
    for (spec, _) in opts.extra.iter().filter(|(s, _)| is_accessory_spec(s)) {
        accessories.push(open_accessory(spec).with_context(|| format!("--device {spec}"))?);
    }
    for (spec, args) in opts.extra.iter().filter(|(s, _)| !is_accessory_spec(s)) {
        let l = open_live(spec, args).with_context(|| format!("--device {spec}"))?;
        extra_controls.push((
            Arc::clone(&l.control),
            LiveTuning {
                center_hz: l.info.center_hz,
                sample_rate_hz: l.info.sample_rate_hz,
                gains: l.gains.clone(),
                bias_tee: l.bias_tee,
                baseband_filter_hz: None,
            },
        ));
        extra.push(hk_pipeline::ExtraSource {
            source: l.source,
            info: l.info,
        });
    }
    let handle = hk_pipeline::Pipeline::start_multi(
        cfg,
        live.source,
        live.info,
        None,
        Box::new(TrackInventory::default()),
        extra,
    )?;
    for source in accessories {
        handle.vlf().attach(
            hk_pipeline::vlf::VlfService::start(source, hk_pipeline::vlf::VlfConfig::default())
                .context("starting the VLF accessory service")?,
        );
    }
    let live_control = (!opts.schedule).then(|| {
        Arc::new(
            SourceLiveControl::new(Arc::clone(&control), initial)
                .with_retuner(Arc::new(PipelineRetuner(handle.controller()))),
        ) as Arc<dyn LiveControl>
    });
    let mut live_controls: Vec<Arc<dyn LiveControl>> = live_control.iter().cloned().collect();
    if !opts.schedule {
        live_controls.extend(extra_controls.into_iter().map(|(c, t)| {
            Arc::new(SourceLiveControl::new(c, t).with_fixed_rate()) as Arc<dyn LiveControl>
        }));
    }
    Ok(LivePipeline {
        handle,
        control,
        live_control,
        live_controls,
        class,
    })
}

/// Runs `hk replay`.
pub fn run_replay(args: &ReplayArgs) -> anyhow::Result<RunSummary> {
    let pacing = if args.paced {
        Pacing::RealTime { speed: 1.0 }
    } else {
        Pacing::Unpaced
    };
    // A scheduled replay is retuned: the mock device serves it (T-057).
    let rec = open_recording(&args.fixture, pacing, args.schedule)?;
    let caps = rec.source.capabilities();
    let plan = plan_for_run(args.plan.as_deref(), args.survey_dwell_s, &rec.info, caps)?;
    let class = if args.schedule {
        most_restrictive(rec.class, plan_class(&plan, rec.info.sample_rate_hz))
    } else {
        rec.class
    };
    let data_dir = args.data_dir.clone().unwrap_or_else(temp_data_dir);
    let registry = StreamRegistry::new();
    let mut cfg = config_for(
        data_dir,
        plan,
        &registry,
        args.feeds.clone(),
        args.calibration.as_deref(),
    )?;
    args.compute.apply(&mut cfg.settings);
    cfg.source_class = class;
    // Explicit opt-in: `PipelineConfig` defaults to lossless off (live-source semantics), and a
    // recording can pause, so unpaced replay waits for slow readers instead of dropping.
    cfg.lossless = !args.paced;
    cfg.drive_scheduler = args.schedule;
    if let Some(id) = rec.device_id {
        cfg.device_id = id;
    }
    cfg.device_hw = rec.device_hw;
    let handle = hk_pipeline::Pipeline::start(
        cfg,
        rec.source,
        rec.info,
        None,
        Box::new(TrackInventory::default()),
    )?;
    let server = match args.serve {
        Some(bind) => Some(serve_api(
            bind,
            args.ui_dist.clone(),
            &registry,
            &handle,
            token(None)?,
            "hk replay",
            None,
        )?),
        None => None,
    };
    let watch = signal::stop_on_signal(handle.stopper());
    let summary = handle.wait()?;
    drop(watch);
    if server.is_some() && !signal::requested() {
        print!("{}", summary.to_text());
        eprintln!("hk replay: finished; still serving the API (Ctrl-C to stop)");
        signal::wait_for_signal();
    }
    drop(server);
    Ok(summary)
}

/// Runs `hk run` (live HackRF One) until `--duration` or Ctrl-C.
pub fn run_live(args: &RunArgs) -> anyhow::Result<RunSummary> {
    let registry = StreamRegistry::new();
    let lp = start_live(
        &LiveOptions {
            source: args.source.clone(),
            live: args.live.clone(),
            extra: args.extra_devices.clone(),
            data_dir: args.data_dir.clone().unwrap_or_else(temp_data_dir),
            plan: args.plan.clone(),
            survey_dwell_s: args.survey_dwell_s,
            schedule: args.schedule,
            feeds: args.feeds.clone(),
            calibration: args.calibration.clone(),
            spectrum_fft_len: None,
            spectrum_rows_per_s: None,
            compute: args.compute.clone(),
            iq_buffer: IqBufferArgs::default(),
            iq_buffer_hooks: None,
        },
        &registry,
    )?;
    let server = match args.serve {
        Some(bind) => Some(serve_api(
            bind,
            args.ui_dist.clone(),
            &registry,
            &lp.handle,
            token(None)?,
            "hk run",
            lp.live_control.clone(),
        )?),
        None => None,
    };
    let watch = match args.duration_s {
        Some(s) => signal::stop_on_signal_or_after(
            lp.handle.stopper(),
            Duration::from_secs_f64(s.max(0.0)),
        ),
        None => signal::stop_on_signal(lp.handle.stopper()),
    };
    let summary = lp.handle.wait()?;
    drop(watch);
    if let Some(stats) = lp.control.stats() {
        eprintln!("source: {stats:?}");
    }
    if server.is_some() && !signal::requested() {
        print!("{}", summary.to_text());
        eprintln!("hk run: finished; still serving the API (Ctrl-C to stop)");
        signal::wait_for_signal();
    }
    drop(server);
    Ok(summary)
}

/// A running daemon: the pipeline and its API server.
pub struct Daemon {
    /// The API server.
    pub server: Server,
    /// The pipeline.
    pub handle: PipelineHandle,
    /// The live source's control handle (stats, identity); `None` for recordings.
    pub source_control: Option<Arc<dyn SourceControl>>,
}

/// Starts `hackriffd`'s pipeline (scheduler driven) and API server.
pub fn start_daemon(args: &DaemonArgs) -> anyhow::Result<Daemon> {
    args.listen.validate()?;
    let registry = StreamRegistry::new();
    if is_device_spec(&args.source) {
        if args.unpaced || args.loop_replay {
            anyhow::bail!(
                "--unpaced and --loop apply to sigmf: recordings; a device (the live HackRF, or \
                 the mock, which streams in real time and loops by itself) can neither pause nor \
                 loop on request"
            );
        }
        let lp = start_live(
            &LiveOptions {
                source: args.source.clone(),
                live: args.live.clone(),
                extra: args.extra_devices.clone(),
                data_dir: args.data_dir.clone(),
                plan: args.plan.clone(),
                survey_dwell_s: args.survey_dwell_s,
                schedule: true,
                feeds: args.feeds.clone(),
                calibration: args.calibration.clone(),
                spectrum_fft_len: None,
                spectrum_rows_per_s: None,
                compute: args.compute.clone(),
                iq_buffer: args.iq_buffer.clone(),
                iq_buffer_hooks: None,
            },
            &registry,
        )?;
        // After the source is open (a missing driver fails first, before any token file is
        // touched); a token error stops the run it started.
        let token = match token(args.token.as_deref()) {
            Ok(t) => t,
            Err(e) => {
                lp.handle.stop();
                return Err(e);
            }
        };
        args.listen.apply(&lp.handle);
        let server = serve_api(
            args.bind,
            args.ui_dist.clone(),
            &registry,
            &lp.handle,
            token,
            "hackriffd",
            None,
        )?;
        return Ok(Daemon {
            server,
            handle: lp.handle,
            source_control: Some(lp.control),
        });
    }
    if let Some((spec, _)) = args.extra_devices.first() {
        anyhow::bail!(
            "--device {spec:?}: a sigmf: recording replays alone; further devices need the \
             primary to be a live device"
        );
    }
    let Some(path) = args.source.strip_prefix("sigmf:").map(PathBuf::from) else {
        anyhow::bail!(
            "unsupported --source {:?}: use hackrf, hackrf:<serial>, rtlsdr, rtlsdr:<serial>, \
             mock:<file.sigmf-meta> or sigmf:<file.sigmf-meta>",
            args.source
        );
    };
    let pacing = if args.unpaced {
        Pacing::Unpaced
    } else {
        Pacing::RealTime { speed: 1.0 }
    };
    // The scheduler retunes the recording: the mock device serves it truthfully (T-057).
    let rec = open_recording(&path, pacing, true)?;
    let caps = rec.source.capabilities();
    let plan = plan_for_run(args.plan.as_deref(), args.survey_dwell_s, &rec.info, caps)?;
    let class = most_restrictive(rec.class, plan_class(&plan, rec.info.sample_rate_hz));
    let mut cfg = config_for(
        args.data_dir.clone(),
        plan,
        &registry,
        args.feeds.clone(),
        args.calibration.as_deref(),
    )?;
    args.compute.apply(&mut cfg.settings);
    args.iq_buffer
        .apply(&mut cfg.iq_buffer, Some(rec.info.sample_rate_hz));
    cfg.source_class = class;
    // Explicit opt-in, as for `hk replay` (a live source leaves this off).
    cfg.lossless = args.unpaced;
    cfg.drive_scheduler = true;
    cfg.device_id = rec.device_id.unwrap_or_else(|| "sigmf-replay".into());
    cfg.device_hw = rec.device_hw;
    let reopen: Option<SourceFactory> = if args.loop_replay {
        let p = path.clone();
        Some(Box::new(move || -> anyhow::Result<Box<dyn Source>> {
            Ok(open_recording(&p, pacing, true)?.source)
        }))
    } else {
        None
    };
    let token = token(args.token.as_deref())?;
    let handle = hk_pipeline::Pipeline::start(
        cfg,
        rec.source,
        rec.info,
        reopen,
        Box::new(TrackInventory::default()),
    )?;
    args.listen.apply(&handle);
    let server = serve_api(
        args.bind,
        args.ui_dist.clone(),
        &registry,
        &handle,
        token,
        "hackriffd",
        None,
    )?;
    Ok(Daemon {
        server,
        handle,
        source_control: rec.control,
    })
}

/// Runs `hackriffd` until the source ends and a shutdown signal arrives, or until Ctrl-C.
pub fn run_daemon(args: &DaemonArgs) -> anyhow::Result<()> {
    let Daemon {
        server,
        handle,
        source_control,
    } = start_daemon(args)?;
    let watch = signal::stop_on_signal(handle.stopper());
    let summary = handle.wait()?;
    drop(watch);
    print!("{}", summary.to_text());
    if let Some(stats) = source_control.and_then(|c| c.stats()) {
        eprintln!("source: {stats:?}");
    }
    if !signal::requested() {
        eprintln!("hackriffd: source finished; still serving the API (Ctrl-C to stop)");
        signal::wait_for_signal();
    }
    drop(server);
    if !summary.errors.is_empty() {
        anyhow::bail!("the run reported {} error(s)", summary.errors.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    use super::*;

    /// T-512: a replay primary refuses further devices rather than dropping them.
    #[test]
    fn daemon_replay_refuses_extra_devices() {
        let mut a = daemon_args("sigmf:/nope.sigmf-meta".into(), temp_data_dir(), Some("t"));
        a.extra_devices = vec![("rtlsdr".into(), LiveArgs::default())];
        let err = start_daemon(&a).err().expect("must refuse");
        assert!(err.to_string().contains("replays alone"), "{err:#}");
    }

    /// T-512 (AWARE-011): per-device settings are addressable; bad devices are loud errors.
    #[test]
    fn device_flags_resolve_per_device_and_refuse_bad_ones() {
        let base = LiveArgs::default();
        let specs = ["mock:/a.sigmf-meta".to_string(), "rtlsdr".to_string()];
        let sets = [
            "mock:/a.sigmf-meta:center-hz=433.9e6".to_string(),
            "rtlsdr:gain.lna=30".to_string(),
        ];
        let r = resolve_devices(&specs, &sets, &base).unwrap();
        assert_eq!(r[0].1.center_hz, 433.9e6);
        assert_eq!(r[1].1.center_hz, base.center_hz);
        assert_eq!(r[1].1.gains, vec!["lna=30".to_string()]);
        assert!(r[0].1.gains.is_empty());
        let dup = resolve_devices(&[specs[1].clone(), specs[1].clone()], &[], &base);
        assert!(dup.unwrap_err().to_string().contains("twice"));
        let unk = resolve_devices(&specs, &["hackrf:rate=1e6".to_string()], &base);
        assert!(unk.unwrap_err().to_string().contains("no --device"));
        let key = resolve_devices(&specs, &["rtlsdr:nope=1".to_string()], &base);
        assert!(key.unwrap_err().to_string().contains("unknown key"));
        // One device with nothing else is today's behaviour exactly.
        let (s, l, x) = resolve_primary("hackrf".into(), &[], &[], &base).unwrap();
        assert_eq!((s.as_str(), &l, x.len()), ("hackrf", &base, 0));
    }

    /// Writes `secs` of a ci8 tone (+50 kHz) in noise at 250 kS/s, 433.5 MHz (no LFS data needed).
    fn tiny_recording(dir: &Path, secs: f64) -> PathBuf {
        use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
        std::fs::create_dir_all(dir).unwrap();
        let fs = 250e3;
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
        std::fs::write(dir.join("tiny.sigmf-data"), data).unwrap();
        let mut meta = SigmfMeta::new(Datatype::Ci8);
        meta.global.sample_rate = Some(fs);
        meta.captures.push(Capture {
            sample_start: 0,
            frequency: Some(433.5e6),
            datetime: Some("2026-09-13T12:00:00Z".into()),
            provenance: None,
            clip_count: None,
            extra: serde_json::Map::new(),
        });
        let path = dir.join("tiny.sigmf-meta");
        meta.write(&path).unwrap();
        path
    }

    /// A real HackRF fixture whose LFS data is fetched, searched from this crate up through the
    /// ancestor checkouts (a git worktree may hold only pointers). `None` skips
    /// (`HK_REQUIRE_FIXTURES=1` fails instead).
    fn lfs_fixture(name: &str) -> Option<PathBuf> {
        let rel = Path::new("fixtures/hackrf/2026-09-13").join(format!("{name}.sigmf-meta"));
        for dir in Path::new(env!("CARGO_MANIFEST_DIR")).ancestors() {
            let meta = dir.join(&rel);
            let fetched = std::fs::read(meta.with_extension("sigmf-data"))
                .is_ok_and(|b| b.len() > 4096 && !b.starts_with(b"version https://git-lfs"));
            if fetched {
                return Some(meta);
            }
        }
        if std::env::var("HK_REQUIRE_FIXTURES").is_ok_and(|v| v == "1") {
            panic!("{name}: fixture data not fetched (git lfs pull)");
        }
        eprintln!("SKIP {name}: fixture data is not fetched (git lfs pull)");
        None
    }

    fn daemon_args(source: String, data_dir: PathBuf, token: Option<&str>) -> DaemonArgs {
        DaemonArgs {
            source,
            live: LiveArgs::default(),
            extra_devices: Vec::new(),
            loop_replay: false,
            data_dir,
            plan: None,
            survey_dwell_s: None,
            bind: "127.0.0.1:0".parse().unwrap(),
            ui_dist: None,
            unpaced: true,
            feeds: None,
            token: token.map(str::to_owned),
            calibration: None,
            listen: ListenArgs::default(),
            compute: ComputeArgs::default(),
            iq_buffer: IqBufferArgs::default(),
        }
    }

    #[test]
    fn the_iq_buffer_flags_parse_and_set_retention_and_quota() {
        use clap::Parser;
        use hk_store::iqbuffer::IqBufferConfig;
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            iq: IqBufferArgs,
        }
        let parse = |args: &[&str]| {
            Cli::try_parse_from(std::iter::once("hk").chain(args.iter().copied())).map(|c| c.iq)
        };
        assert_eq!(parse(&[]).unwrap(), IqBufferArgs::default());
        let a = parse(&["--iq-retention", "90s", "--iq-buffer-max", "512MiB"]).unwrap();
        assert_eq!((a.retention_s, a.max_bytes), (Some(90.0), Some(512 << 20)));
        for (text, s) in [("2m", 120.0), ("1h", 3600.0), ("0", 0.0), ("off", 0.0)] {
            let r = parse(&["--iq-retention", text]).unwrap().retention_s;
            assert_eq!(r, Some(s), "{text}");
        }
        let m = parse(&["--iq-buffer-max", "8GiB"]).unwrap().max_bytes;
        assert_eq!(m, Some(8 << 30));
        assert!(parse(&["--iq-retention", "soon"]).is_err());
        assert!(parse(&["--iq-buffer-max", "lots"]).is_err());
        // quota = min(retention x highest rate x 2 bytes/sample, max).
        let mut cfg = IqBufferConfig::default();
        a.apply(&mut cfg, Some(20e6));
        assert_eq!(
            (cfg.retention_s, cfg.max_bytes, cfg.quota_bytes()),
            (90.0, Some(512 << 20), 512 << 20)
        );
        let mut cfg = IqBufferConfig::default();
        parse(&["--iq-retention", "1m"])
            .unwrap()
            .apply(&mut cfg, Some(2.4e6));
        assert_eq!(
            (cfg.max_bytes, cfg.quota_bytes()),
            (None, 60 * 2_400_000 * 2)
        );
        let mut cfg = IqBufferConfig::default();
        parse(&["--iq-retention", "off"])
            .unwrap()
            .apply(&mut cfg, Some(20e6));
        assert!(!cfg.active(false));
    }

    #[test]
    fn the_compute_flag_sets_the_provider_for_every_workload() {
        use clap::Parser;
        use hk_dsp::compute::Preference;
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            compute: ComputeArgs,
        }
        let cli = Cli::try_parse_from(["hk", "--compute", "gpu"]).unwrap();
        assert_eq!(cli.compute.provider, Some(Preference::Gpu));
        assert!(Cli::try_parse_from(["hk", "--compute", "fpga"]).is_err());
        let mut s = hk_pipeline::PipelineSettings::default();
        s.compute.stft = Some(Preference::Accelerate);
        ComputeArgs::default().apply(&mut s);
        assert_eq!(
            s.compute.stft,
            Some(Preference::Accelerate),
            "unset keeps the plan's"
        );
        cli.compute.apply(&mut s);
        assert_eq!(
            (s.compute.provider, s.compute.stft, s.compute.pfb),
            (Preference::Gpu, None, None)
        );
    }

    /// T-027 review fix: the HackRF fixtures start at `core:global_index` 423 000 000 (915 MHz)
    /// and 324 000 000 (433 MHz), far beyond the lossless gate's slack from the always-on
    /// readers' initial cursors. Unpaced `hk replay` must finish with every sample read.
    #[test]
    fn unpaced_replay_completes_on_large_global_index_fixtures() {
        const LIMIT: Duration = Duration::from_secs(1200);
        let runs: Vec<_> = [
            "ism_915M_10M_l24g30a1_t42p3_1p2s",
            "ism_433p62M_2M_l24g30a1_t162p0_6s",
        ]
        .into_iter()
        .filter_map(|name| lfs_fixture(name).map(|f| (name, f)))
        .map(|(name, fixture)| {
            let dir = temp_data_dir();
            let guard = TempDataDirGuard::new(dir.clone());
            let (tx, rx) = std::sync::mpsc::channel();
            let args = ReplayArgs {
                fixture,
                data_dir: Some(dir),
                ..ReplayArgs::default()
            };
            // A deadlocked run leaks its thread; the timeout fails the test instead of hanging.
            std::thread::spawn(move || {
                let _ = tx.send(run_replay(&args).map_err(|e| format!("{e:#}")));
            });
            (name, guard, rx)
        })
        .collect();
        for (name, _guard, rx) in runs {
            let summary = rx
                .recv_timeout(LIMIT)
                .unwrap_or_else(|_| panic!("{name}: hk replay did not finish (gate deadlock)"))
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            eprintln!("{name}\n{}", summary.to_text());
            assert!(summary.errors.is_empty(), "{name}: {:?}", summary.errors);
            let samples = summary.counter("/source/samples");
            assert_eq!(samples, 12_000_000, "{name}: every recorded sample");
            assert_eq!(summary.counter("/source/ring_errors"), 0, "{name}");
            assert_eq!(summary.always_on_lost_samples, 0, "{name}: no drops");
            for r in ["detect", "history", "spectrum"] {
                assert_eq!(
                    summary.counter(&format!("/readers/{r}/samples")),
                    samples,
                    "{name}: {r} read every sample"
                );
            }
        }
    }

    fn get(addr: SocketAddr, path: &str, token: Option<&str>) -> (u16, String) {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
        write!(
            s,
            "GET {path} HTTP/1.1\r\nHost: t\r\n{auth}Connection: close\r\n\r\n"
        )
        .unwrap();
        let mut raw = String::new();
        s.read_to_string(&mut raw).unwrap();
        let status = raw[9..12].parse().unwrap();
        let body = raw.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_owned();
        (status, body)
    }

    #[test]
    fn replay_runs_a_small_recording_end_to_end() {
        let dir = temp_data_dir();
        let _guard = TempDataDirGuard::new(dir.clone());
        let fixture = tiny_recording(&dir.join("src"), 0.25);
        let summary = run_replay(&ReplayArgs {
            fixture,
            data_dir: Some(dir.clone()),
            ..ReplayArgs::default()
        })
        .unwrap();
        assert!(summary.errors.is_empty(), "{:?}", summary.errors);
        assert!(summary.counter("/source/samples") > 0);
        assert_eq!(summary.always_on_lost_samples, 0);
        assert!(summary.counter("/readers/detect/frames") > 0);
        assert!(dir.join("hackriff.db").is_file());
        let text = summary.to_text();
        assert!(text.contains("detections:"), "{text}");
    }

    #[test]
    fn daemon_drives_the_scheduler_and_serves_token_gated_status() {
        const TOKEN: &str = "t027-daemon-status-token-0123456789";
        let dir = temp_data_dir();
        let _guard = TempDataDirGuard::new(dir.clone());
        let fixture = tiny_recording(&dir.join("src"), 3.0);
        // Paced (T-072): the control thread ticks the scheduler on wall time with the stream
        // time, so an unpaced replay could outrun it under load and apply too few steps (a
        // flake). Paced, the step count follows stream time.
        let mut args = daemon_args(
            format!("sigmf:{}", fixture.display()),
            dir.clone(),
            Some(TOKEN),
        );
        args.unpaced = false;
        let Daemon { server, handle, .. } = start_daemon(&args).unwrap();
        let addr = server.local_addr();
        let (unauth, _) = get(addr, "/api/status", None);
        assert_eq!(unauth, 401, "status needs the token");
        let (ok, body) = get(addr, "/api/status", Some(TOKEN));
        assert_eq!(ok, 200, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v.pointer("/readers/detect/lost_samples").is_some(), "{v}");
        assert!(v.pointer("/chains/attached").is_some(), "{v}");
        let summary = handle.wait().unwrap();
        // The scheduler really retunes the device (T-057), and the spectrum stream is re-offered
        // under the same id at each new centre. The registry keeps offered streams after the run,
        // so this is checked once the run has ended instead of polled with sleeps.
        let (_, streams) = get(addr, "/api/streams", Some(TOKEN));
        assert!(streams.contains("spectrum/live"), "{streams}");
        eprintln!("{}", summary.to_text());
        assert!(summary.errors.is_empty(), "{:?}", summary.errors);
        assert!(summary.counter("/scheduler/steps") > 10, "scheduler driven");
        assert_eq!(summary.always_on_lost_samples, 0);
        let (after, body) = get(addr, "/api/status", Some(TOKEN));
        assert_eq!(after, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            v.pointer("/source/samples")
                .and_then(serde_json::Value::as_u64),
            Some(summary.counter("/source/samples"))
        );
        drop(server);
    }

    #[test]
    fn daemon_loop_continues_the_stream_across_passes() {
        let dir = temp_data_dir();
        let _guard = TempDataDirGuard::new(dir.clone());
        let fixture = tiny_recording(&dir.join("src"), 0.25);
        let mut args = daemon_args(
            format!("sigmf:{}", fixture.display()),
            dir.clone(),
            Some("t027-daemon-loop-token-0123456789"),
        );
        args.loop_replay = true;
        let Daemon { server, handle, .. } = start_daemon(&args).unwrap();
        let counters = handle.counters();
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while counters
            .source
            .loops
            .load(std::sync::atomic::Ordering::Relaxed)
            < 2
        {
            assert!(std::time::Instant::now() < deadline, "no loop");
            std::thread::sleep(Duration::from_millis(10));
        }
        handle.stop();
        let s = handle.wait().unwrap();
        eprintln!("{}", s.to_text());
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        let loops = s.counter("/source/loops");
        assert!(loops >= 2);
        assert_eq!(
            s.counter("/source/ring_errors"),
            0,
            "indices continue across passes"
        );
        assert_eq!(s.always_on_lost_samples, 0);
        assert_eq!(
            s.counter("/readers/detect/samples"),
            s.counter("/source/samples"),
            "the detection reader read every pass"
        );
        assert!(
            s.counter("/readers/detect/stft_resets") >= loops,
            "each pass starts with a GAP reset"
        );
        drop(server);
    }

    #[test]
    fn daemon_rejects_unsupported_sources_and_live_loop_options() {
        let bogus_dir = temp_data_dir();
        let _bogus_guard = TempDataDirGuard::new(bogus_dir.clone());
        let err = start_daemon(&daemon_args("bogus".into(), bogus_dir, None))
            .err()
            .expect("rejected");
        assert!(err.to_string().contains("sigmf:"), "{err}");
        let live_dir = temp_data_dir();
        let _live_guard = TempDataDirGuard::new(live_dir.clone());
        let mut live = daemon_args("hackrf".into(), live_dir, None);
        live.loop_replay = true;
        let err = start_daemon(&live).err().expect("a radio cannot loop");
        assert!(err.to_string().contains("--loop"), "{err}");
        if !HackRfDriver.available() {
            live.loop_replay = false;
            live.unpaced = false;
            let err = start_daemon(&live).err().expect("no driver in this build");
            assert!(format!("{err:#}").contains("hackrf"), "{err:#}");
        }
    }

    #[test]
    fn live_gains_are_named_stages_of_the_device() {
        let caps = HackRfDriver.capabilities();
        let mut live = LiveArgs {
            lna_db: 32.0,
            vga_db: 30.0,
            amp: true,
            ..LiveArgs::default()
        };
        assert_eq!(
            live.named_gains(&caps).unwrap(),
            vec![
                NamedGain::new("lna", 32.0),
                NamedGain::new("vga", 30.0),
                NamedGain::new("amp", 11.0)
            ]
        );
        live.gains = vec!["vga=40".into()];
        assert_eq!(
            live.named_gains(&caps).unwrap()[1],
            NamedGain::new("vga", 40.0)
        );
        live.gains = vec!["mixer=3".into()];
        assert!(live.named_gains(&caps).is_err());
        live.gains = vec!["vga".into()];
        assert!(live.named_gains(&caps).is_err());
    }

    #[test]
    fn live_source_specs_and_window_classes() {
        assert_eq!(hackrf_serial("hackrf"), Some(None));
        assert_eq!(hackrf_serial("hackrf:abc"), Some(Some("abc".into())));
        assert_eq!(hackrf_serial("hackrf:"), None);
        assert_eq!(hackrf_serial("sigmf:x"), None);
    }

    /// Waits (bounded) until the pipeline has taken samples.
    fn wait_for_samples(handle: &PipelineHandle) {
        let counters = handle.counters();
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while counters
            .source
            .samples
            .load(std::sync::atomic::Ordering::Relaxed)
            == 0
        {
            assert!(std::time::Instant::now() < deadline, "no samples");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// T-891: accessory specs are recognised apart from radios, and a live soundcard receiver
    /// is refused with the reason rather than opened as something it is not.
    #[test]
    fn accessory_specs_route_to_the_vlf_service_not_the_radio_path() {
        for spec in ["vlf", "vlf:hw:1", "vlf-mock:/tmp/x.sigmf-meta"] {
            assert!(is_accessory_spec(spec) && !is_device_spec(spec), "{spec}");
        }
        for spec in ["hackrf", "mock:/tmp/x.sigmf-meta", "rtlsdr", "vlfx"] {
            assert!(!is_accessory_spec(spec), "{spec}");
        }
        let err = open_accessory("vlf:hw:1").err().unwrap().to_string();
        assert!(err.contains("audio backend"), "{err}");
        assert!(open_accessory("vlf-mock:/nonexistent.sigmf-meta").is_err());
    }

    /// T-514: `rtlsdr` / `rtlsdr:<serial>` names the RTL-SDR driver, and the serial form is what
    /// actually pins the radio — an index would move when anything else is plugged in.
    #[test]
    fn rtlsdr_specs_select_the_rtl_driver_by_serial() {
        assert_eq!(rtlsdr_serial("rtlsdr"), Some(None));
        assert_eq!(
            rtlsdr_serial("rtlsdr:7673444264"),
            Some(Some("7673444264".into()))
        );
        assert_eq!(rtlsdr_serial("rtlsdr:"), None);
        assert_eq!(rtlsdr_serial("rtlsdrx"), None);
        assert_eq!(rtlsdr_serial("hackrf"), None);
        assert_eq!(hackrf_serial("rtlsdr"), None);
        assert!(is_device_spec("rtlsdr") && is_device_spec("rtlsdr:7673444264"));
        let (driver, serial) = driver_for("rtlsdr:7673444264").expect("an RTL-SDR spec");
        assert_eq!(driver.name(), "rtlsdr");
        assert_eq!(serial.as_deref(), Some("7673444264"));
        // The capabilities the CLI would plan against are the RTL's, not the HackRF's.
        let caps = driver.capabilities();
        assert_eq!(caps.driver, "rtl-sdr");
        assert!(!caps.supports_frequency(2.4e9) && !caps.sample_rates.supports(20e6));
        // `--lna` reaches its one gain knob; `--vga` / `--amp` are not offered on this device.
        let gains = LiveArgs {
            lna_db: 28.0,
            vga_db: 20.0,
            amp: true,
            ..LiveArgs::default()
        }
        .named_gains(&caps)
        .expect("only the stages the device has");
        assert_eq!(gains, vec![NamedGain::new("lna", 28.0)]);
    }

    /// T-049: `mock:<file>` opens like a device, with the recording's own tuning unless given, and
    /// carries the band-derived class a live radio at that frequency would.
    #[test]
    fn mock_device_specs_open_like_a_radio_with_the_band_class() {
        let dir = temp_data_dir();
        let _guard = TempDataDirGuard::new(dir.clone());
        let fixture = tiny_recording(&dir.join("src"), 1.0);
        let spec = format!("mock:{}", fixture.display());
        assert!(is_device_spec(&spec) && is_device_spec("hackrf") && !is_device_spec("sigmf:x"));
        assert_eq!(mock_path("mock:"), None);

        let live = open_live(&spec, &LiveArgs::default()).unwrap();
        assert_eq!(
            (live.info.center_hz, live.info.sample_rate_hz),
            (433.5e6, 250e3),
            "defaults take the recording's tuning"
        );
        assert!(
            live.device.device_id.starts_with("mock:"),
            "{:?}",
            live.device
        );
        assert!(!live.source.pausable(), "real time, like the radio");
        let moved = open_live(
            &spec,
            &LiveArgs {
                center_hz: 433.6e6,
                ..LiveArgs::default()
            },
        )
        .unwrap();
        assert_eq!(moved.info.center_hz, 433.6e6, "explicit tuning applies");
        assert!(open_live("mock:/does/not/exist.sigmf-meta", &LiveArgs::default()).is_err());

        // The same recording at a paging frequency: restricted, and a retune to another class is
        // refused, exactly as for the HackRF.
        let mut meta = hk_model::sigmf::SigmfMeta::read(&fixture).unwrap();
        meta.captures[0].frequency = Some(930.5e6);
        meta.write(&fixture).unwrap();
        let lp = start_live(
            &LiveOptions {
                source: spec.clone(),
                live: LiveArgs::default(),
                extra: Vec::new(),
                data_dir: dir.join("live"),
                plan: None,
                survey_dwell_s: None,
                schedule: false,
                feeds: None,
                calibration: None,
                spectrum_fft_len: Some(1024),
                spectrum_rows_per_s: None,
                compute: ComputeArgs::default(),
                // An explicit small ring (allocated up front, T-178).
                iq_buffer: IqBufferArgs {
                    retention_s: None,
                    max_bytes: Some(16 << 20),
                },
                iq_buffer_hooks: None,
            },
            &StreamRegistry::new(),
        )
        .unwrap();
        let lc = lp.live_control.clone().expect("live control over the mock");
        wait_for_samples(&lp.handle);
        // A retune into another class re-plumbs the run into the window's class (T-050), exactly
        // as for the HackRF.
        lc.set_center(100.8e6).expect("the retune re-plumbs");
        let status = lp.handle.controller().status();
        assert_eq!(status.content_class, ContentClass::Unrestricted);
        assert_eq!(status.center_hz, 100.8e6);
        assert!(status.segment > 0, "a new segment");
        lp.handle.stop();
        let s = lp.handle.wait().unwrap();
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        assert!(s.counter("/source/samples") > 0);
    }

    #[test]
    fn daemon_runs_over_a_mock_device_spec() {
        let dir = temp_data_dir();
        let _guard = TempDataDirGuard::new(dir.clone());
        let fixture = tiny_recording(&dir.join("src"), 0.5);
        let mut args = daemon_args(
            format!("mock:{}", fixture.display()),
            dir.join("data"),
            Some("t049-daemon-mock-token-0123456789"),
        );
        assert!(
            start_daemon(&args).is_err(),
            "--unpaced is refused for a device"
        );
        args.unpaced = false;
        let Daemon {
            server,
            handle,
            source_control,
        } = start_daemon(&args).unwrap();
        let control = source_control.expect("device control");
        assert!(
            control
                .device_info()
                .unwrap()
                .device_id
                .starts_with("mock:")
        );
        wait_for_samples(&handle);
        handle.stop();
        let s = handle.wait().unwrap();
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        assert!(control.stats().unwrap().samples > 0);
        drop(server);
    }
}
