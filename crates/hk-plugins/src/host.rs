//! Plugin host (ADR-0003; docs/stream-contract.md §9): one supervised subprocess per decoder
//! instance.
//!
//! - **Data plane:** the producer calls [`PluginInstance::push`] with input records. They go
//!   into a bounded [`DecoderFeed`] ring and a writer thread copies them to the plugin's stdin
//!   (contract framing with header and drop markers, or raw bytes). `push` never waits: a
//!   full queue or a missing process drops the record and counts it. Capture never blocks on
//!   a plugin.
//! - **Message plane:** a reader thread splits stdout into lines (bounded length), parses them
//!   ([`crate::output`]), clamps the class to the manifest, and stores them through
//!   [`Ingest`]. stderr goes to a bounded log ring.
//! - **Supervision:** when the plugin exits it is restarted with exponential backoff. A run
//!   longer than `backoff_max` resets the backoff. More than `max_restarts` exits within
//!   `window` is a crash loop, and the instance goes `Failed`. A plugin whose input stays full
//!   for `stall_timeout` is killed as hung, then restarted. Shutdown kills the process.
//!
//! Records queued for a process that exits are discarded with its queue.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use hk_api::stream::{
    BinaryRecord, DEFAULT_MAX_FRAME_LEN, DecoderFeed, FeedAttacher, PublisherConfig, StreamError,
    StreamHeader,
};
use hk_model::sigmf::Datatype;
use hk_model::{
    ContentClass, DemodulationId, DetectionId, EmitterId, ProvenanceId, RecordingId, Region,
    SampleTime,
};

use crate::ingest::Ingest;
use crate::manifest::{ManifestError, PluginManifest};
use crate::output::{PluginOutput, parse_line};

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
    /// Class of the channel, as classified (informational for the plugin; the data plane is
    /// not gated, its output is).
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
    /// Lines whose class was clamped to the manifest ceiling.
    pub class_clamped: u64,
    /// Lines with an unknown class string (failed closed).
    pub class_unknown: u64,
    /// Lines whose content was refused by the repository and stored metadata-only.
    pub content_gated: u64,
    /// Unparseable or overlong lines.
    pub malformed: u64,
    /// Parsed lines that could not be stored.
    pub store_errors: u64,
    /// Last exit description.
    pub last_exit: Option<String>,
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
    malformed: AtomicU64,
    store_errors: AtomicU64,
}

fn bump(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Proc {
    pid: Option<u32>,
    state: PluginState,
    last_exit: Option<String>,
}

struct Shared {
    manifest: PluginManifest,
    context: PluginContext,
    sample_rate_hz: f64,
    anchor: Mutex<SampleTime>,
    proc: Mutex<Proc>,
    wake: Condvar,
    shutdown: AtomicBool,
    counters: Counters,
    log: Mutex<VecDeque<String>>,
    ingest: Arc<Mutex<Ingest>>,
}

impl Shared {
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

    fn set_state(&self, state: PluginState) {
        lock(&self.proc).state = state;
    }

    /// Kills the running process, if any. The pid stays registered until the supervisor has
    /// observed the exit (the process is not reaped yet), so it cannot name a reused pid.
    fn kill(&self) {
        let g = lock(&self.proc);
        if let Some(pid) = g.pid {
            // SAFETY: plain syscall on a pid we spawned and have not reaped.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }

    fn stats(&self) -> PluginStats {
        let c = &self.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let proc = lock(&self.proc);
        PluginStats {
            state: proc.state,
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

    /// The stderr/log ring, oldest first.
    pub fn log_tail(&self) -> Vec<String> {
        lock(&self.shared.log).iter().cloned().collect()
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
            },
        )?;

        let shared = Arc::new(Shared {
            sample_rate_hz: input.sample_rate_hz,
            anchor: Mutex::new(input.anchor),
            manifest,
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

/// Runs one process to completion.
fn run_once(shared: &Arc<Shared>, attacher: &FeedAttacher, program: &PathBuf, args: &[String]) {
    let c = &shared.counters;
    let spawned = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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
            // SAFETY: plain syscall on the pid just spawned.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
    bump(&c.starts);
    if let Some(nice) = shared.manifest.limits.nice {
        // SAFETY: plain syscall; failure (e.g. permission) is ignored, limits are best-effort.
        unsafe {
            libc::setpriority(libc::PRIO_PROCESS, pid as libc::id_t, nice);
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
                s.log("host: input stayed full past stall_timeout; killing plugin".into());
                s.kill();
            }
        }),
    );
    match consumer {
        // Running means input is attached: pushes from now on reach this process.
        Ok(_) => shared.set_state(PluginState::Running),
        Err(_) => shared.kill(),
    }
    let out_shared = Arc::clone(shared);
    let out_reader = thread::Builder::new()
        .name(format!("hk-plugin-{}-out", shared.manifest.id))
        .spawn(move || read_stdout(&out_shared, stdout));
    let err_shared = Arc::clone(shared);
    let err_reader = thread::Builder::new()
        .name(format!("hk-plugin-{}-err", shared.manifest.id))
        .spawn(move || read_stderr(&err_shared, stderr));

    wait_exit_unreaped(pid);
    lock(&shared.proc).pid = None;
    let status = child.wait();
    if let Ok(id) = consumer {
        attacher.detach(id);
    }
    for reader in [out_reader, err_reader].into_iter().flatten() {
        let _ = reader.join();
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

fn read_stdout(shared: &Arc<Shared>, stdout: impl Read) {
    let c = &shared.counters;
    for_each_line(
        stdout,
        shared.manifest.limits.max_message_bytes,
        |line| {
            if line.iter().all(u8::is_ascii_whitespace) {
                return;
            }
            let timing = Some((*lock(&shared.anchor), shared.sample_rate_hz));
            let parsed = match parse_line(&shared.manifest, &shared.context, timing, line) {
                Ok(p) => p,
                Err(reason) => {
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
            let ctx = &shared.context;
            let result = match parsed.output {
                PluginOutput::Log(msg) => {
                    shared.log(format!("plugin: {msg}"));
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
                    shared.log(format!("host: storing plugin output failed: {e}"));
                }
            }
        },
        || {
            bump(&c.malformed);
            shared.log("host: plugin output line exceeded max_message_bytes".into());
        },
    );
}

fn read_stderr(shared: &Arc<Shared>, stderr: impl Read) {
    for_each_line(
        stderr,
        4096,
        |line| shared.log(String::from_utf8_lossy(line).into_owned()),
        || shared.log("(stderr line over 4096 bytes omitted)".into()),
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
