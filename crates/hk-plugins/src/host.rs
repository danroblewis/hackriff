//! Plugin host (ADR-0003; docs/stream-contract.md §9): one supervised subprocess per decoder
//! instance.
//!
//! - **Data plane:** the producer calls [`PluginInstance::push`] with input records. They go
//!   into a bounded [`DecoderFeed`] ring and a writer thread copies them to the plugin's stdin
//!   (contract framing with header and drop markers, or raw bytes). `push` never waits: a
//!   full queue or a missing process drops the record and counts it. Capture never blocks on
//!   a plugin.
//! - **Message plane:** a reader thread splits stdout into lines (bounded length), parses them
//!   ([`crate::output`]) under the **ceiling** `clamp(manifest class, input channel class)`,
//!   applies the metadata allowlist, and stores them through [`Ingest`]. Under a restricted class
//!   a line whose `sample_index` lies outside the input offered to this instance is dropped and
//!   counted (`sample_index_out_of_range`).
//! - **Log ring:** host events, plus plugin `log` lines and stderr **only when the ceiling
//!   permits content**. Under a content-forbidding ceiling those are counted, never stored. The
//!   ring is tagged with the ceiling ([`LogTail::content_class`]).
//! - **Supervision:** each plugin runs in its own process group. When the leader exits, the whole
//!   group is SIGKILLed before the leader is reaped (no pid/pgid reuse window), so descendants
//!   holding the pipes cannot hang the host. The plugin is restarted with exponential backoff; a
//!   run longer than `backoff_max` resets the backoff; more than `max_restarts` exits within
//!   `window` is a crash loop (`Failed`). A plugin whose input stays full for `stall_timeout`
//!   has its group killed as hung. Output readers are joined with a timeout: a descendant that
//!   escaped the group (setsid) cannot block restart or shutdown. Shutdown kills the group.
//!
//! Records queued for a process that exits are discarded with its queue.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use hk_model::sigmf::Datatype;
use hk_model::{
    ContentClass, DemodulationId, DetectionId, EmitterId, ProvenanceId, RecordingId, Region,
    SampleTime,
};
use hk_stream::{
    BinaryRecord, DEFAULT_MAX_FRAME_LEN, DecoderFeed, FeedAttacher, PublisherConfig, StreamError,
    StreamHeader, gate,
};

use crate::ingest::Ingest;
use crate::manifest::{ManifestError, PluginManifest};
use crate::output::{PluginOutput, SAMPLE_INDEX_OUT_OF_RANGE, parse_line};

/// How long the host waits for a plugin's output readers after killing its process group.
const READER_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// The input stream a plugin instance is fed.
#[derive(Clone, Debug, PartialEq)]
pub struct InputStreamDesc {
    /// Sample datatype.
    pub datatype: Datatype,
    /// Sample rate, Hz.
    pub sample_rate_hz: f64,
    /// RF centre, Hz.
    pub center_hz: Option<f64>,
    /// Bandwidth, Hz.
    pub bandwidth_hz: Option<f64>,
    /// Class of the channel, as classified. The data plane is not gated (decoders need the
    /// samples), but this class is a **ceiling on the plugin's output**: the effective ceiling
    /// is `clamp(manifest.output.content_class, content_class)`.
    pub content_class: ContentClass,
    /// Timing anchor for converting sample indices to time.
    pub anchor: SampleTime,
    /// Emitter the channel belongs to, if resolved.
    pub emitter_id: Option<EmitterId>,
    /// Provenance of the samples.
    pub provenance_ref: Option<ProvenanceId>,
}

/// Where the plugin's output rows point.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PluginContext {
    /// Emitter (republished messages carry it).
    pub emitter_ref: Option<EmitterId>,
    /// Detection the channel came from (annotation target).
    pub detection_ref: Option<DetectionId>,
    /// Demodulation session (decode `demodulation_ref`).
    pub demodulation_ref: Option<DemodulationId>,
    /// Recording replayed (decode `recording_ref`).
    pub recording_ref: Option<RecordingId>,
    /// Provenance (republished messages carry it).
    pub provenance_ref: Option<ProvenanceId>,
    /// Time-frequency box (annotation target when there is no detection).
    pub region: Option<Region>,
}

/// Host errors.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    /// Manifest or input mismatch.
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    /// Data-plane setup.
    #[error(transparent)]
    Stream(#[from] StreamError),
    /// Thread spawn.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Supervisor state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginState {
    /// Spawning.
    Starting,
    /// Process running.
    Running,
    /// Waiting before a restart.
    Backoff,
    /// Crash loop: no more restarts.
    Failed,
    /// Shut down.
    Stopped,
}

/// What happened to one pushed record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushOutcome {
    /// Queued for the running plugin.
    Enqueued,
    /// Dropped: the plugin's queue was full.
    DroppedFull,
    /// Dropped: no plugin process attached (starting, backoff, failed).
    DroppedDetached,
}

/// Instance counters. `records_offered = records_enqueued + records_dropped_full +
/// records_dropped_detached` exactly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginStats {
    /// Supervisor state.
    pub state: PluginState,
    /// Effective output ceiling (manifest class clamped to the input class).
    pub content_ceiling: ContentClass,
    /// Records pushed.
    pub records_offered: u64,
    /// Records queued for a process.
    pub records_enqueued: u64,
    /// Records dropped because the queue was full.
    pub records_dropped_full: u64,
    /// Records dropped because no process was attached.
    pub records_dropped_detached: u64,
    /// Processes started.
    pub starts: u64,
    /// Restarts after an exit.
    pub restarts: u64,
    /// Non-zero or signalled exits (including hang kills).
    pub crashes: u64,
    /// Zero exits.
    pub clean_exits: u64,
    /// Spawn failures.
    pub spawn_failures: u64,
    /// Kills by the hang watchdog (input stayed full past `stall_timeout`).
    pub stall_kills: u64,
    /// Decode lines stored.
    pub decodes: u64,
    /// Annotation lines stored.
    pub annotations: u64,
    /// Lines whose class was clamped to the ceiling.
    pub class_clamped: u64,
    /// Lines with an unknown class string (failed closed).
    pub class_unknown: u64,
    /// Lines whose content was refused by the repository and stored metadata-only.
    pub content_gated: u64,
    /// Metadata keys, frame models, labels and identities removed by the allowlist.
    pub metadata_sanitized: u64,
    /// Restricted lines dropped because their `sample_index` was outside the input offered.
    pub sample_index_out_of_range: u64,
    /// Plugin `log` lines not stored because the ceiling forbids content.
    pub log_lines_withheld: u64,
    /// stderr lines not stored because the ceiling forbids content.
    pub stderr_lines_withheld: u64,
    /// Runs whose output readers were abandoned (a descendant escaped the process group).
    pub readers_abandoned: u64,
    /// Unparseable or overlong lines.
    pub malformed: u64,
    /// Parsed lines that could not be stored.
    pub store_errors: u64,
    /// Last exit description.
    pub last_exit: Option<String>,
}

/// The log ring, tagged with the plugin's output ceiling. Under a content-forbidding ceiling it
/// holds only host-generated lines (no plugin text); a control API exposing it must still apply
/// the class.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogTail {
    /// The effective output ceiling of the plugin that produced the lines.
    pub content_class: ContentClass,
    /// Lines, oldest first.
    pub lines: Vec<String>,
}

#[derive(Default)]
struct Counters {
    offered: AtomicU64,
    enqueued: AtomicU64,
    dropped_full: AtomicU64,
    dropped_detached: AtomicU64,
    starts: AtomicU64,
    restarts: AtomicU64,
    crashes: AtomicU64,
    clean_exits: AtomicU64,
    spawn_failures: AtomicU64,
    stall_kills: AtomicU64,
    decodes: AtomicU64,
    annotations: AtomicU64,
    class_clamped: AtomicU64,
    class_unknown: AtomicU64,
    content_gated: AtomicU64,
    metadata_sanitized: AtomicU64,
    sample_index_out_of_range: AtomicU64,
    log_lines_withheld: AtomicU64,
    stderr_lines_withheld: AtomicU64,
    readers_abandoned: AtomicU64,
    malformed: AtomicU64,
    store_errors: AtomicU64,
}

fn bump(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// SIGKILLs process group `pid` (the plugin leader's pid is its pgid).
fn kill_group(pid: u32) {
    // SAFETY: plain syscall. The caller holds the pid registration: the leader is not reaped, so
    // the pid, and therefore the pgid, cannot have been reused.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
}

struct Proc {
    pid: Option<u32>,
    state: PluginState,
    last_exit: Option<String>,
}

struct Shared {
    manifest: PluginManifest,
    ceiling: ContentClass,
    context: PluginContext,
    sample_rate_hz: f64,
    /// Bytes per input element (for the sample-index range).
    element_bytes: usize,
    /// Inclusive sample-index range of the input offered (`lo > hi`: none yet).
    input_lo: AtomicU64,
    input_hi: AtomicU64,
    anchor: Mutex<SampleTime>,
    proc: Mutex<Proc>,
    wake: Condvar,
    shutdown: AtomicBool,
    counters: Counters,
    log: Mutex<VecDeque<String>>,
    ingest: Arc<Mutex<Ingest>>,
}

impl Shared {
    /// Stores a host-generated line (never plugin text).
    fn log(&self, line: String) {
        let cap = self.manifest.limits.stderr_lines;
        if cap == 0 {
            return;
        }
        let mut log = lock(&self.log);
        if log.len() == cap {
            log.pop_front();
        }
        log.push_back(line);
    }

    /// Stores plugin-originated text only when the ceiling permits content; otherwise counts it.
    fn plugin_text(&self, text: String, withheld: &AtomicU64) {
        if self.ceiling.permits_content() {
            self.log(text);
        } else {
            bump(withheld);
        }
    }

    fn set_state(&self, state: PluginState) {
        lock(&self.proc).state = state;
    }

    /// Kills the running plugin's process group, if any. The pid stays registered until the
    /// supervisor has killed the group after the leader's exit, and the leader is reaped only
    /// after that, so the pgid cannot name a reused process.
    fn kill(&self) {
        let g = lock(&self.proc);
        if let Some(pid) = g.pid {
            kill_group(pid);
        }
    }

    fn stats(&self) -> PluginStats {
        let c = &self.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let proc = lock(&self.proc);
        PluginStats {
            state: proc.state,
            content_ceiling: self.ceiling,
            records_offered: get(&c.offered),
            records_enqueued: get(&c.enqueued),
            records_dropped_full: get(&c.dropped_full),
            records_dropped_detached: get(&c.dropped_detached),
            starts: get(&c.starts),
            restarts: get(&c.restarts),
            crashes: get(&c.crashes),
            clean_exits: get(&c.clean_exits),
            spawn_failures: get(&c.spawn_failures),
            stall_kills: get(&c.stall_kills),
            decodes: get(&c.decodes),
            annotations: get(&c.annotations),
            class_clamped: get(&c.class_clamped),
            class_unknown: get(&c.class_unknown),
            content_gated: get(&c.content_gated),
            metadata_sanitized: get(&c.metadata_sanitized),
            sample_index_out_of_range: get(&c.sample_index_out_of_range),
            log_lines_withheld: get(&c.log_lines_withheld),
            stderr_lines_withheld: get(&c.stderr_lines_withheld),
            readers_abandoned: get(&c.readers_abandoned),
            malformed: get(&c.malformed),
            store_errors: get(&c.store_errors),
            last_exit: proc.last_exit.clone(),
        }
    }
}

/// A clonable read-only view of an instance, for health reporting and tests.
#[derive(Clone)]
pub struct PluginMonitor {
    shared: Arc<Shared>,
}

impl PluginMonitor {
    /// Counters and state.
    pub fn stats(&self) -> PluginStats {
        self.shared.stats()
    }

    /// The log ring, tagged with the output ceiling.
    pub fn log_tail(&self) -> LogTail {
        LogTail {
            content_class: self.shared.ceiling,
            lines: lock(&self.shared.log).iter().cloned().collect(),
        }
    }

    /// Polls (every 2 ms) until `pred` holds or `timeout` passes. For tests and shutdown paths.
    pub fn wait_for(&self, timeout: Duration, mut pred: impl FnMut(&PluginStats) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if pred(&self.stats()) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
}

/// One running decoder plugin.
pub struct PluginInstance {
    feed: DecoderFeed,
    shared: Arc<Shared>,
    supervisor: Option<JoinHandle<()>>,
}

impl PluginInstance {
    /// Validates the input against the manifest and starts supervising the plugin.
    pub fn spawn(
        manifest: PluginManifest,
        input: InputStreamDesc,
        context: PluginContext,
        ingest: Arc<Mutex<Ingest>>,
    ) -> Result<Self, HostError> {
        manifest.check_input(&input)?;
        let args = manifest.render_args(&input)?;
        let program = manifest.resolve_executable();
        let ceiling = gate::clamp(manifest.output.content_class, input.content_class);

        let mut header = StreamHeader::new(
            format!("plugin-input/{}", manifest.id),
            manifest.input.kind.stream_kind(),
            input.content_class,
            "hackriff-plugin-host",
        );
        header.datatype = Some(input.datatype.as_str().to_owned());
        header.sample_rate_hz = Some(input.sample_rate_hz);
        header.center_hz = input.center_hz;
        header.bandwidth_hz = input.bandwidth_hz;
        header.emitter_id = input.emitter_id;
        header.provenance_ref = input.provenance_ref;
        header.max_frame_len =
            DEFAULT_MAX_FRAME_LEN.min((manifest.limits.input_queue_bytes / 4) as u32);
        let feed = DecoderFeed::new(
            header,
            manifest.input.framing,
            PublisherConfig {
                queue_bytes: manifest.limits.input_queue_bytes,
                disconnect_after_drops: u64::MAX,
                disconnect_after: manifest.limits.stall_timeout,
                max_consumers: 2,
                drain_timeout: Duration::from_secs(1),
            },
        )?;

        let shared = Arc::new(Shared {
            sample_rate_hz: input.sample_rate_hz,
            element_bytes: input.datatype.bytes_per_sample(),
            input_lo: AtomicU64::new(u64::MAX),
            input_hi: AtomicU64::new(0),
            anchor: Mutex::new(input.anchor),
            manifest,
            ceiling,
            context,
            proc: Mutex::new(Proc {
                pid: None,
                state: PluginState::Starting,
                last_exit: None,
            }),
            wake: Condvar::new(),
            shutdown: AtomicBool::new(false),
            counters: Counters::default(),
            log: Mutex::new(VecDeque::new()),
            ingest,
        });
        for warning in &shared.manifest.warnings {
            shared.log(format!("host: manifest warning: {warning}"));
        }
        let attacher = feed.attacher();
        let for_thread = Arc::clone(&shared);
        let supervisor = thread::Builder::new()
            .name(format!("hk-plugin-{}", shared.manifest.id))
            .spawn(move || supervise(for_thread, attacher, program, args))?;
        Ok(Self {
            feed,
            shared,
            supervisor: Some(supervisor),
        })
    }

    /// Offers one input record. Never waits. Errors only for a record larger than the input
    /// stream's `max_frame_len` (not counted).
    pub fn push(&mut self, record: BinaryRecord<'_>) -> Result<PushOutcome, StreamError> {
        // The restricted-class output bound covers every record offered. It is widened before the
        // record can reach the plugin, so a prompt reply is never refused. Two atomics, no wait.
        let elements = (record.payload.len() / self.shared.element_bytes.max(1)) as u64;
        self.shared.input_hi.fetch_max(
            record.sample_index.saturating_add(elements),
            Ordering::Relaxed,
        );
        self.shared
            .input_lo
            .fetch_min(record.sample_index, Ordering::Relaxed);
        let outcome = self.feed.push(record)?;
        let c = &self.shared.counters;
        bump(&c.offered);
        Ok(if outcome.consumers == 0 {
            bump(&c.dropped_detached);
            PushOutcome::DroppedDetached
        } else if outcome.enqueued > 0 {
            bump(&c.enqueued);
            PushOutcome::Enqueued
        } else {
            bump(&c.dropped_full);
            PushOutcome::DroppedFull
        })
    }

    /// Re-anchors sample time (after a retune or discontinuity).
    pub fn set_anchor(&self, anchor: SampleTime) {
        *lock(&self.shared.anchor) = anchor;
    }

    /// The input stream header sent to the plugin.
    pub fn input_header(&self) -> &StreamHeader {
        self.feed.header()
    }

    /// A monitor handle.
    pub fn monitor(&self) -> PluginMonitor {
        PluginMonitor {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Counters and state.
    pub fn stats(&self) -> PluginStats {
        self.shared.stats()
    }

    /// Kills the plugin, stops supervising, and returns the final counters.
    pub fn shutdown(mut self) -> PluginStats {
        self.stop();
        self.shared.stats()
    }

    fn stop(&mut self) {
        let Some(supervisor) = self.supervisor.take() else {
            return;
        };
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.kill();
        self.shared.wake.notify_all();
        let _ = supervisor.join();
    }
}

impl Drop for PluginInstance {
    fn drop(&mut self) {
        self.stop();
    }
}

fn supervise(shared: Arc<Shared>, attacher: FeedAttacher, program: PathBuf, args: Vec<String>) {
    let policy = shared.manifest.restart;
    let mut backoff = policy.backoff_initial;
    let mut exits: VecDeque<Instant> = VecDeque::new();
    while !shared.shutdown.load(Ordering::Acquire) {
        shared.set_state(PluginState::Starting);
        let started = Instant::now();
        run_once(&shared, &attacher, &program, &args);
        if shared.shutdown.load(Ordering::Acquire) {
            break;
        }
        let now = Instant::now();
        exits.push_back(now);
        while exits
            .front()
            .is_some_and(|t| now.duration_since(*t) > policy.window)
        {
            exits.pop_front();
        }
        if exits.len() > policy.max_restarts as usize {
            shared.log(format!(
                "host: crash loop ({} exits within {:?}); not restarting",
                exits.len(),
                policy.window
            ));
            shared.set_state(PluginState::Failed);
            return;
        }
        if started.elapsed() >= policy.backoff_max {
            backoff = policy.backoff_initial;
        }
        let guard = {
            let mut g = lock(&shared.proc);
            g.state = PluginState::Backoff;
            g
        };
        let (guard, _) = shared
            .wake
            .wait_timeout_while(guard, backoff, |_| !shared.shutdown.load(Ordering::Acquire))
            .unwrap_or_else(PoisonError::into_inner);
        drop(guard);
        if shared.shutdown.load(Ordering::Acquire) {
            break;
        }
        backoff = (backoff * 2).min(policy.backoff_max);
        bump(&shared.counters.restarts);
    }
    shared.set_state(PluginState::Stopped);
}

fn describe(status: &ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit code {code}"),
        (None, Some(sig)) => format!("signal {sig}"),
        _ => "unknown exit".into(),
    }
}

/// Runs one process (group) to completion.
fn run_once(shared: &Arc<Shared>, attacher: &FeedAttacher, program: &PathBuf, args: &[String]) {
    let c = &shared.counters;
    let spawned = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Own process group (pgid = pid): the whole plugin tree can be killed at once.
        .process_group(0)
        .spawn();
    let mut child: Child = match spawned {
        Ok(child) => child,
        Err(e) => {
            bump(&c.spawn_failures);
            shared.log(format!("host: spawn {} failed: {e}", program.display()));
            lock(&shared.proc).last_exit = Some(format!("spawn failed: {e}"));
            return;
        }
    };
    let pid = child.id();
    {
        let mut g = lock(&shared.proc);
        g.pid = Some(pid);
        if shared.shutdown.load(Ordering::Acquire) {
            kill_group(pid);
        }
    }
    bump(&c.starts);
    if let Some(nice) = shared.manifest.limits.nice {
        // SAFETY: plain syscall; failure (e.g. permission) is ignored, limits are best-effort.
        unsafe {
            libc::setpriority(libc::PRIO_PGRP, pid as libc::id_t, nice);
        }
    }

    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let weak: Weak<Shared> = Arc::downgrade(shared);
    let consumer = attacher.attach_child_stdin(
        format!("plugin:{}:{pid}", shared.manifest.id),
        stdin,
        Box::new(move || {
            if let Some(s) = weak.upgrade() {
                bump(&s.counters.stall_kills);
                s.log("host: input stayed full past stall_timeout; killing plugin group".into());
                s.kill();
            }
        }),
    );
    match consumer {
        // Running means input is attached: pushes from now on reach this process.
        Ok(_) => shared.set_state(PluginState::Running),
        Err(_) => shared.kill(),
    }

    // Readers signal completion by dropping their sender; `abandon` stops a reader that outlives
    // the join timeout from ingesting anything more.
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let abandon = Arc::new(AtomicBool::new(false));
    let spawn_reader = |name: &str, body: Box<dyn FnOnce() + Send>| {
        let tx = done_tx.clone();
        thread::Builder::new()
            .name(format!("hk-plugin-{}-{name}", shared.manifest.id))
            .spawn(move || {
                body();
                drop(tx);
            })
    };
    let (out_shared, out_abandon) = (Arc::clone(shared), Arc::clone(&abandon));
    let _ = spawn_reader(
        "out",
        Box::new(move || read_stdout(&out_shared, stdout, &out_abandon)),
    );
    let (err_shared, err_abandon) = (Arc::clone(shared), Arc::clone(&abandon));
    let _ = spawn_reader(
        "err",
        Box::new(move || read_stderr(&err_shared, stderr, &err_abandon)),
    );
    drop(done_tx);

    wait_exit_unreaped(pid);
    {
        // Kill the rest of the group while the leader is still a zombie, then reap.
        let mut g = lock(&shared.proc);
        kill_group(pid);
        g.pid = None;
    }
    let status = child.wait();
    if let Ok(id) = consumer {
        attacher.detach(id);
    }
    match done_rx.recv_timeout(READER_JOIN_TIMEOUT) {
        Err(RecvTimeoutError::Disconnected) | Ok(()) => {}
        Err(RecvTimeoutError::Timeout) => {
            abandon.store(true, Ordering::Release);
            bump(&c.readers_abandoned);
            shared.log(
                "host: plugin output pipes still held by a descendant outside the process group; readers abandoned"
                    .into(),
            );
        }
    }
    let desc = match &status {
        // Killed by `shutdown`: not a crash.
        _ if shared.shutdown.load(Ordering::Acquire) => "stopped by host".to_owned(),
        Ok(s) if s.success() => {
            bump(&c.clean_exits);
            describe(s)
        }
        Ok(s) => {
            bump(&c.crashes);
            describe(s)
        }
        Err(e) => {
            bump(&c.crashes);
            format!("wait failed: {e}")
        }
    };
    shared.log(format!("host: plugin pid {pid} ended: {desc}"));
    lock(&shared.proc).last_exit = Some(desc);
}

/// Blocks until `pid` has exited, without reaping it, so its pid cannot be reused while a kill
/// may still target it.
fn wait_exit_unreaped(pid: u32) {
    loop {
        // SAFETY: `info` is a valid, zeroed siginfo_t for waitid to fill.
        let rc = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if rc == 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return;
        }
    }
}

/// Calls `f` for each line (without the newline). Lines longer than `max` are skipped and
/// reported to `overlong` once.
fn for_each_line(r: impl Read, max: usize, mut f: impl FnMut(&[u8]), mut overlong: impl FnMut()) {
    let mut reader = BufReader::with_capacity(64 * 1024, r);
    let mut line = Vec::new();
    let mut skipping = false;
    loop {
        let buf = match reader.fill_buf() {
            Ok([]) => break,
            Ok(buf) => buf,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let (chunk, newline) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (&buf[..i], true),
            None => (buf, false),
        };
        if !skipping {
            if line.len() + chunk.len() > max {
                skipping = true;
                line.clear();
                overlong();
            } else {
                line.extend_from_slice(chunk);
            }
        }
        let consumed = chunk.len() + usize::from(newline);
        if newline {
            if !skipping {
                f(&line);
            }
            line.clear();
            skipping = false;
        }
        reader.consume(consumed);
    }
    if !skipping && !line.is_empty() {
        f(&line);
    }
}

fn read_stdout(shared: &Arc<Shared>, stdout: impl Read, abandon: &AtomicBool) {
    let c = &shared.counters;
    for_each_line(
        stdout,
        shared.manifest.limits.max_message_bytes,
        |line| {
            if abandon.load(Ordering::Acquire) || line.iter().all(u8::is_ascii_whitespace) {
                return;
            }
            let timing = Some((*lock(&shared.anchor), shared.sample_rate_hz));
            let (lo, hi) = (
                shared.input_lo.load(Ordering::Relaxed),
                shared.input_hi.load(Ordering::Relaxed),
            );
            let input_range = (lo <= hi).then_some((lo, hi));
            let parsed = match parse_line(
                &shared.manifest,
                shared.ceiling,
                &shared.context,
                timing,
                input_range,
                line,
            ) {
                Ok(p) => p,
                Err(reason) if reason == SAMPLE_INDEX_OUT_OF_RANGE => {
                    bump(&c.sample_index_out_of_range);
                    return;
                }
                Err(reason) => {
                    // `reason` never contains values from the line (output.rs contract).
                    bump(&c.malformed);
                    shared.log(format!("host: malformed plugin output: {reason}"));
                    return;
                }
            };
            if parsed.clamped {
                bump(&c.class_clamped);
            }
            if parsed.unknown_class {
                bump(&c.class_unknown);
            }
            c.metadata_sanitized
                .fetch_add(parsed.sanitized, Ordering::Relaxed);
            let ctx = &shared.context;
            let result = match parsed.output {
                PluginOutput::Log(msg) => {
                    let mut msg = msg;
                    let mut end = msg.len().min(4096);
                    while !msg.is_char_boundary(end) {
                        end -= 1;
                    }
                    msg.truncate(end);
                    shared.plugin_text(format!("plugin: {msg}"), &c.log_lines_withheld);
                    return;
                }
                PluginOutput::Decode(d) => lock(&shared.ingest)
                    .store_decode(d, ctx.emitter_ref, ctx.provenance_ref)
                    .map(|s| (s, &c.decodes)),
                PluginOutput::Annotation(a) => lock(&shared.ingest)
                    .store_annotation(a, ctx.emitter_ref, ctx.provenance_ref)
                    .map(|s| (s, &c.annotations)),
            };
            match result {
                Ok((stored, counter)) => {
                    bump(counter);
                    if stored.content_gated {
                        bump(&c.content_gated);
                    }
                }
                Err(e) => {
                    bump(&c.store_errors);
                    if shared.ceiling.permits_content() {
                        shared.log(format!("host: storing plugin output failed: {e}"));
                    } else {
                        shared.log("host: storing plugin output failed".into());
                    }
                }
            }
        },
        || {
            bump(&c.malformed);
            shared.log("host: plugin output line exceeded max_message_bytes".into());
        },
    );
}

fn read_stderr(shared: &Arc<Shared>, stderr: impl Read, abandon: &AtomicBool) {
    let withheld = &shared.counters.stderr_lines_withheld;
    for_each_line(
        stderr,
        4096,
        |line| {
            if !abandon.load(Ordering::Acquire) {
                shared.plugin_text(String::from_utf8_lossy(line).into_owned(), withheld);
            }
        },
        || shared.plugin_text("(stderr line over 4096 bytes omitted)".into(), withheld),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_splitter_bounds_lines_and_handles_eof() {
        let input = b"one\n\ntoo-long-line\nthree";
        let mut lines = Vec::new();
        let mut overlong = 0;
        for_each_line(
            &input[..],
            8,
            |l| lines.push(String::from_utf8(l.to_vec()).unwrap()),
            || overlong += 1,
        );
        assert_eq!(lines, ["one", "", "three"]);
        assert_eq!(overlong, 1);
    }
}
