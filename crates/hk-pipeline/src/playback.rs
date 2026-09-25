//! Historical playback (T-463, MPLAY; AWARE-011, AWARE-042): play from a chosen past time, with
//! play and pause, and "now" moves forward over recorded history as if it were live.
//!
//! **Assembly, not new machinery.** Every piece already existed; this module wires them:
//!
//! | Piece | What it is |
//! |---|---|
//! | Raw IQ at a time | the ring-window extract ([`IqBufferService::read_window`], the same plan as a clip) and the persisted recordings catalogue ([`hk_store::recordings`], T-469) — the **two** places raw IQ exists, exactly the ones `GET /api/recordings`' `iq_available` names |
//! | Through the device interface | [`SigmfReplaySource`] over that IQ (in memory for a ring window; the file plus [`SigmfReplaySource::seek_to_time`] for a recording), read through the [`Source`] trait like every other front end |
//! | Demod | [`hk_demod::audio`]'s probe → plan → [`AudioDemod`], the same blocks Listen runs, parameters estimated from the signal (no mode is accepted) |
//! | Decode | RDS on a WFM channel ([`AudioDemod::with_rds`]) |
//! | Analysis over the window | **nothing new**: the windowed read routes (`/api/inventory?at=`, `/api/events`, `/api/tiles`, `/api/inventory/{id}/decode?t0&t1`, T-384) already answer any past window from the records as they were written |
//!
//! # The split: analysis is immutable, demod and decode re-run
//!
//! Detections, emitters, presence, events and history are **records**, and playback replays them
//! exactly as they were written: the client reads them through the windowed routes it already
//! uses, positioned at the playhead. **This module has no path to a detector and no store
//! write**: it opens the repository read-only for the recordings catalogue and nothing else, so a
//! second, disagreeing analysis of the same air cannot be produced by construction (the drift
//! family T-420/T-388/T-397/T-412 closed). Demod and decode are functions of raw IQ, so they are
//! **re-run** here, from the IQ the ring or a recording still holds.
//!
//! # One playhead
//!
//! [`Playhead`] is the single "now" a playback moves forward: a capture-clock position that
//! advances at `speed` × real elapsed time while playing and holds while paused. Its **position
//! is capture time** (the one shared time axis); only its *rate* comes from a monotonic clock,
//! which is what "as if live" means. Pausing freezes the playhead, never the capture: the device,
//! the ring and detection keep running (CLAUDE.md, "Pause freezes the view, not the capture").
//! The demod thread reads a block, then waits until the playhead reaches the block's end before
//! demodulating it, so audio is paced by the playhead, stops when it pauses, and a seek
//! (a new [`Playhead::generation`]) reopens the IQ at the new position.
//!
//! **Per-pane playheads are deferred** (the user, 2026-09-17). Where they would go: a
//! `Playhead` per pane id, held in a map beside this one, with the opener taking a `pane=`
//! parameter to choose which one it follows. Nothing below assumes there is only one except
//! [`PlaybackService`] holding a single `Arc<Playhead>`.
//!
//! # The IQ horizon (T-464's rule, respected rather than discovered)
//!
//! Beyond raw IQ, playback is **waterfall-only with no audio**. That is decided up front, never
//! discovered when the sound stops:
//! - opening audio at a position with no raw IQ is **refused** (409 `no-iq`) with the reason and,
//!   when there is one, the next time IQ exists;
//! - the answer comes from what actually holds IQ — the ring's own window and the complete IQ
//!   recordings — never from spectrum coverage, which answers a different question over a much
//!   longer horizon;
//! - the ring rolls, so nothing about the horizon is cached: every window is asked for afresh,
//!   and a window the ring overwrote meanwhile is simply not there (`Evicted`);
//! - while playing, each status record says whether the playhead has IQ (`iq`), from where
//!   (`iq_source`), and until when the current window runs (`iq_until_ns`), so the boundary is
//!   predictable; crossing it publishes `iq: false` and the stream carries no audio past it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use hk_core::{ReplayOptions, SigmfReplaySource, Source};
use hk_demod::audio::{AudioConfig, AudioDemod, AudioPlan, LISTEN_DEMOD_VERSION, probe};
use hk_dsp::InputInfo;
use hk_estimate::SnippetRequest;
use hk_model::{ContentClass, RecordingKind, Timestamp};
use hk_store::recordings::{RecordingsQuery, catalogue};
use hk_stream::audio::{
    AUDIO_DATATYPE, AUDIO_FRAME_SAMPLES, AUDIO_MAX_FRAME_LEN, AUDIO_SAMPLE_RATE_HZ, AgcInfo,
    AudioInfo, SquelchInfo, encode_pcm,
};
use hk_stream::{
    BinaryRecord, OpenRefusal, OpenRequest, OpenedStream, Publisher, PublisherConfig, RecordFlags,
    SessionEndSlot, StreamHeader, StreamKind, StreamOpener,
};
use num_complex::Complex32;
use serde::Serialize;
use serde_json::{Value, json};

use crate::chains::listen::listen_class;
use crate::class::ClassRule;
use crate::iqbuffer::{ClipError, IqBufferService};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Fastest playback, × real time.
pub const MAX_SPEED: f64 = 16.0;

// ---------------------------------------------------------------------------------------------
// The playhead
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct HeadState {
    /// Capture-clock position at `anchor_at`, Unix ns; `None` until the first seek.
    anchor_ns: Option<i64>,
    anchor_at: Instant,
    playing: bool,
    speed: f64,
    generation: u64,
}

impl HeadState {
    fn position_at(&self, now: Instant) -> Option<i64> {
        let a = self.anchor_ns?;
        if !self.playing {
            return Some(a);
        }
        let dt = now.saturating_duration_since(self.anchor_at).as_secs_f64();
        Some(a.saturating_add((dt * self.speed * 1e9).round() as i64))
    }

    fn snapshot(&self, now: Instant) -> PlayheadState {
        PlayheadState {
            position_ns: self.position_at(now),
            playing: self.playing,
            speed: self.speed,
            generation: self.generation,
        }
    }
}

/// A snapshot of the playhead (`GET /api/playback`'s `playhead`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct PlayheadState {
    /// Position on the capture clock, Unix ns (`None` until the first seek).
    pub position_ns: Option<i64>,
    /// Whether it is advancing.
    pub playing: bool,
    /// × real time.
    pub speed: f64,
    /// Increases on every seek.
    pub generation: u64,
}

/// Why a playhead change was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayheadError(pub String);

impl std::fmt::Display for PlayheadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What [`Playhead::wait_until`] saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// The playhead reached the time.
    Reached,
    /// It was moved (a seek): the waiter's position is stale.
    Seeked,
    /// The waiter was asked to stop.
    Stopped,
}

/// The one playhead: "now" moving forward over recorded history (see the module docs).
pub struct Playhead {
    state: Mutex<HeadState>,
    changed: Condvar,
}

impl Default for Playhead {
    fn default() -> Self {
        Self::new()
    }
}

impl Playhead {
    /// A playhead with no position, paused, at 1×.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(HeadState {
                anchor_ns: None,
                anchor_at: Instant::now(),
                playing: false,
                speed: 1.0,
                generation: 0,
            }),
            changed: Condvar::new(),
        }
    }

    /// The current state.
    pub fn state(&self) -> PlayheadState {
        lock(&self.state).snapshot(Instant::now())
    }

    /// The position now, Unix ns on the capture clock.
    pub fn position_ns(&self) -> Option<i64> {
        lock(&self.state).position_at(Instant::now())
    }

    /// Applies `change` under the lock at one instant and returns the state **at that instant**
    /// (so `play` answers exactly the position it resumed from).
    fn apply(
        &self,
        change: impl FnOnce(&mut HeadState, Instant) -> Result<(), PlayheadError>,
    ) -> Result<PlayheadState, PlayheadError> {
        let snap = {
            let mut s = lock(&self.state);
            let now = Instant::now();
            change(&mut s, now)?;
            s.snapshot(now)
        };
        self.changed.notify_all();
        Ok(snap)
    }

    /// Moves the playhead to `t_ns` (playing or paused as before). Bumps the generation.
    pub fn seek(&self, t_ns: i64) -> Result<PlayheadState, PlayheadError> {
        if t_ns < 0 {
            return Err(PlayheadError("a position is Unix time, ≥ 0".into()));
        }
        self.apply(|s, now| {
            s.anchor_ns = Some(t_ns);
            s.anchor_at = now;
            s.generation += 1;
            Ok(())
        })
    }

    /// Plays (advances) from the current position. Refused without a position.
    pub fn play(&self) -> Result<PlayheadState, PlayheadError> {
        self.apply(|s, now| {
            if s.anchor_ns.is_none() {
                return Err(PlayheadError(
                    "the playhead has no position: seek to a past time first".into(),
                ));
            }
            if !s.playing {
                s.anchor_at = now;
                s.playing = true;
            }
            Ok(())
        })
    }

    /// Pauses: the position freezes where it is. Capture is unaffected.
    pub fn pause(&self) -> PlayheadState {
        let r = self.apply(|s, now| {
            if s.playing {
                s.anchor_ns = s.position_at(now);
                s.anchor_at = now;
                s.playing = false;
            }
            Ok(())
        });
        r.unwrap_or_else(|_| self.state())
    }

    /// Sets the speed (0 < speed ≤ [`MAX_SPEED`]) without moving the position.
    pub fn set_speed(&self, speed: f64) -> Result<PlayheadState, PlayheadError> {
        if !(speed.is_finite() && speed > 0.0 && speed <= MAX_SPEED) {
            return Err(PlayheadError(format!(
                "speed must be > 0 and ≤ {MAX_SPEED} (× real time)"
            )));
        }
        self.apply(|s, now| {
            s.anchor_ns = s.position_at(now);
            s.anchor_at = now;
            s.speed = speed;
            Ok(())
        })
    }

    /// Blocks until the position reaches `t_ns` (while paused, indefinitely), the playhead is
    /// seeked away from `generation`, or `stop` is set (checked at least every 50 ms).
    pub fn wait_until(&self, t_ns: i64, generation: u64, stop: &AtomicBool) -> Wait {
        let mut s = lock(&self.state);
        loop {
            if stop.load(Ordering::SeqCst) {
                return Wait::Stopped;
            }
            if s.generation != generation {
                return Wait::Seeked;
            }
            let now = Instant::now();
            let pos = s.position_at(now);
            if pos.is_some_and(|p| p >= t_ns) {
                return Wait::Reached;
            }
            let mut wait = Duration::from_millis(50);
            if let (true, Some(p)) = (s.playing, pos) {
                let left = (t_ns - p) as f64 / (s.speed * 1e9);
                wait =
                    wait.min(Duration::from_secs_f64(left.max(0.0)) + Duration::from_micros(200));
            }
            s = self
                .changed
                .wait_timeout(s, wait)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Where raw IQ is: the ring and the recordings
// ---------------------------------------------------------------------------------------------

/// Where a window of raw IQ came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IqOrigin {
    /// The rolling IQ ring.
    Ring,
    /// A persisted recording (its id).
    Recording(String),
}

impl IqOrigin {
    /// Wire token.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ring => "ring",
            Self::Recording(_) => "recording",
        }
    }
}

/// Raw IQ from a time on, as a device-interface source.
pub struct IqWindow {
    /// The samples, through the [`Source`] trait.
    pub source: Box<dyn Source>,
    /// Content class the IQ was captured under.
    pub content_class: ContentClass,
    /// Where it came from.
    pub origin: IqOrigin,
    /// End of the window, Unix ns (the next window is asked for from here).
    pub t1_ns: i64,
}

/// Why there is no raw IQ at a time: the IQ horizon, stated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoIq {
    /// Why, in words.
    pub reason: String,
    /// The next time raw IQ exists after the asked-for one, when known.
    pub next_iq_ns: Option<i64>,
}

/// What holds raw IQ. Implemented over the run's ring and recordings ([`RunIq`]); a trait so the
/// demod loop is testable against a fake.
pub trait IqArchive: Send + Sync {
    /// Raw IQ covering `band` from `t_ns` on (the first sample at or after it), or the horizon.
    fn open_at(&self, t_ns: i64, band: (f64, f64)) -> Result<IqWindow, NoIq>;
}

/// The run's raw IQ: its ring, then its complete IQ recordings.
pub struct RunIq {
    ring: Arc<IqBufferService>,
    /// `(database, data directory)` for the recordings catalogue; `None`: ring only.
    recordings: Option<(PathBuf, PathBuf)>,
    /// Longest ring window read at once, s.
    pub window_s: f64,
    /// Largest ring window read at once, bytes.
    pub window_max_bytes: u64,
}

impl RunIq {
    /// Over `ring` and, when given, the recordings under `(db_path, data_dir)`.
    pub fn new(ring: Arc<IqBufferService>, recordings: Option<(PathBuf, PathBuf)>) -> Self {
        Self {
            ring,
            recordings,
            window_s: 1.0,
            window_max_bytes: 48 << 20,
        }
    }

    fn ring_window(&self, t_ns: i64, band: (f64, f64)) -> Result<IqWindow, ClipError> {
        let mut t1 = t_ns.saturating_add((self.window_s * 1e9) as i64);
        // A rate change inside the window: stop at it (one SigMF rate per window); a window the
        // cap cannot hold: halve it. Bounded, so a pathological ring cannot spin here.
        for _ in 0..8 {
            match self
                .ring
                .read_window(t_ns, t1, Some(band), self.window_max_bytes)
            {
                Ok((meta, data, class)) => {
                    let source = SigmfReplaySource::from_reader(
                        meta,
                        std::io::Cursor::new(data),
                        ReplayOptions::default(),
                    )
                    .map_err(|e| ClipError::Io(std::io::Error::other(e.to_string())))?;
                    return Ok(IqWindow {
                        source: Box::new(source),
                        content_class: class,
                        origin: IqOrigin::Ring,
                        t1_ns: t1,
                    });
                }
                Err(ClipError::MixedRates { t_ns: at }) if at > t_ns => t1 = at,
                Err(ClipError::TooLarge { .. }) if t1 - t_ns > 1_000_000 => {
                    t1 = t_ns + (t1 - t_ns) / 2;
                }
                Err(e) => return Err(e),
            }
        }
        Err(ClipError::Empty)
    }

    fn recording_at(&self, t_ns: i64, band: (f64, f64)) -> Option<IqWindow> {
        let (db, data_dir) = self.recordings.as_ref()?;
        let repo = hk_model::Repository::open(db).ok()?;
        let page = catalogue(
            &repo,
            data_dir,
            &RecordingsQuery {
                t0_ns: Some(t_ns),
                t1_ns: Some(t_ns.saturating_add(1)),
                kind: None,
                limit: 50,
            },
        )
        .ok()?;
        let entry = page.entries.into_iter().find(|e| {
            let (lo, hi) = e.window_hz();
            e.is_iq() && e.availability.available() && lo <= band.0 && band.1 <= hi
        })?;
        let rec = &entry.recording;
        let mut source =
            SigmfReplaySource::open(data_dir.join(&rec.meta_uri), ReplayOptions::default()).ok()?;
        source.seek_to_time(Timestamp::from_unix_nanos(t_ns)).ok()?;
        Some(IqWindow {
            source: Box::new(source),
            content_class: rec.content_class,
            origin: IqOrigin::Recording(rec.id.to_string()),
            t1_ns: rec.time.end.as_unix_nanos(),
        })
    }

    /// The oldest ring sample and any recording starting after `t_ns`: the next IQ.
    fn next_iq_after(&self, t_ns: i64) -> Option<i64> {
        let status = self.ring.status(None, None, 0);
        let ring_t0 = status
            .t0
            .map(|s| (s * 1e9).round() as i64)
            .filter(|&r| r > t_ns);
        let rec = self.recordings.as_ref().and_then(|(db, data_dir)| {
            let repo = hk_model::Repository::open(db).ok()?;
            let page = catalogue(
                &repo,
                data_dir,
                &RecordingsQuery {
                    t0_ns: Some(t_ns),
                    t1_ns: None,
                    kind: Some(RecordingKind::IqSnippet),
                    limit: 200,
                },
            )
            .ok()?;
            page.spans
                .iter()
                .map(|s| s.t0_ns)
                .filter(|&s| s > t_ns)
                .min()
        });
        match (ring_t0, rec) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

impl IqArchive for RunIq {
    fn open_at(&self, t_ns: i64, band: (f64, f64)) -> Result<IqWindow, NoIq> {
        let ring = match self.ring_window(t_ns, band) {
            Ok(w) => return Ok(w),
            Err(e) => e,
        };
        if let Some(w) = self.recording_at(t_ns, band) {
            return Ok(w);
        }
        let status = self.ring.status(None, None, 0);
        let ring_span = match (status.t0, status.t1) {
            (Some(a), Some(b)) => format!("the IQ ring holds {a:.3}..{b:.3} s"),
            _ => match status.reason {
                Some(r) => format!("there is no IQ ring ({r})"),
                None => "the IQ ring is empty".into(),
            },
        };
        Err(NoIq {
            reason: format!(
                "no raw IQ at {:.3} s for {:.0}..{:.0} Hz ({ring}; {ring_span}, and no complete IQ \
                 recording covers it): playback here is waterfall-only, with no audio",
                t_ns as f64 / 1e9,
                band.0,
                band.1,
            ),
            next_iq_ns: self.next_iq_after(t_ns),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The opener
// ---------------------------------------------------------------------------------------------

/// Playback settings.
#[derive(Clone, Debug)]
pub struct PlaybackConfig {
    /// Leading IQ the mode probe reads, s.
    pub probe_s: f64,
    /// Audio settings (squelch, AGC).
    pub audio: AudioConfig,
    /// Status record cadence.
    pub status_interval: Duration,
    /// Per-consumer queue, bytes.
    pub queue_bytes: usize,
    /// User classification rules for the content gate.
    pub rules: Vec<ClassRule>,
}

impl Default for PlaybackConfig {
    fn default() -> Self {
        Self {
            probe_s: 0.25,
            audio: AudioConfig::default(),
            status_interval: Duration::from_millis(250),
            queue_bytes: 1 << 20,
            rules: Vec::new(),
        }
    }
}

/// Playback counters.
#[derive(Debug, Default)]
pub struct PlaybackCounters {
    /// Streams opened.
    pub opened: AtomicU64,
    /// Opens refused.
    pub refused: AtomicU64,
    /// Audio frames published.
    pub frames: AtomicU64,
    /// Source samples demodulated.
    pub samples: AtomicU64,
    /// Windows of raw IQ opened.
    pub windows: AtomicU64,
    /// Times the playhead crossed into a span with no raw IQ.
    pub no_iq: AtomicU64,
    /// Streams running now.
    pub running: AtomicU64,
}

/// The playback service: the one playhead, the archive it reads, and the `playback` opener.
pub struct PlaybackService {
    playhead: Arc<Playhead>,
    archive: Arc<dyn IqArchive>,
    config: PlaybackConfig,
    counters: Arc<PlaybackCounters>,
    seq: AtomicU64,
}

/// A wire snapshot: the playhead plus what playback is and is not.
pub fn state_json(state: &PlayheadState, counters: &PlaybackCounters) -> Value {
    json!({
        "playhead": {
            "position_ns": state.position_ns,
            "position_s": state.position_ns.map(|n| n as f64 / 1e9),
            "playing": state.playing,
            "speed": state.speed,
            "generation": state.generation,
        },
        "max_speed": MAX_SPEED,
        "playheads": 1,
        "analysis": "recorded",
        "rerun": ["demod", "decode"],
        "iq_horizon": "iq-ring + recordings",
        "streams": {
            "opened": counters.opened.load(Ordering::Relaxed),
            "refused": counters.refused.load(Ordering::Relaxed),
            "running": counters.running.load(Ordering::Relaxed),
            "frames": counters.frames.load(Ordering::Relaxed),
            "windows": counters.windows.load(Ordering::Relaxed),
            "no_iq": counters.no_iq.load(Ordering::Relaxed),
        },
    })
}

impl PlaybackService {
    /// A service reading `archive`.
    pub fn new(archive: Arc<dyn IqArchive>, config: PlaybackConfig) -> Self {
        Self {
            playhead: Arc::new(Playhead::new()),
            archive,
            config,
            counters: Arc::new(PlaybackCounters::default()),
            seq: AtomicU64::new(0),
        }
    }

    /// The one playhead.
    pub fn playhead(&self) -> &Arc<Playhead> {
        &self.playhead
    }

    /// Counters.
    pub fn counters(&self) -> &Arc<PlaybackCounters> {
        &self.counters
    }

    /// `GET /api/playback`'s body.
    pub fn state_json(&self) -> Value {
        state_json(&self.playhead.state(), &self.counters)
    }

    fn open_inner(&self, request: &OpenRequest) -> Result<OpenedStream, OpenRefusal> {
        let num = |k: &str| -> Result<Option<f64>, OpenRefusal> {
            request
                .param(k)
                .map(|v| {
                    v.parse::<f64>()
                        .ok()
                        .filter(|x| x.is_finite())
                        .ok_or_else(|| {
                            OpenRefusal::new(400, "bad-request", format!("{k} must be a number"))
                        })
                })
                .transpose()
        };
        for (k, _) in &request.params {
            if !matches!(k.as_str(), "f_lo" | "f_hi" | "t" | "speed") {
                return Err(OpenRefusal::new(
                    400,
                    "bad-request",
                    format!("unknown parameter {k} (playback takes f_lo, f_hi, t, speed)"),
                ));
            }
        }
        let (Some(lo), Some(hi)) = (num("f_lo")?, num("f_hi")?) else {
            return Err(OpenRefusal::new(
                400,
                "bad-request",
                "f_lo and f_hi (Hz) name the extent to demodulate; no mode is accepted",
            ));
        };
        if !(lo < hi && hi - lo <= hk_stream::audio::MAX_LISTEN_SPAN_HZ) {
            return Err(OpenRefusal::new(
                400,
                "bad-request",
                "the extent needs f_lo < f_hi and at most 1 MHz",
            ));
        }
        if let Some(speed) = num("speed")? {
            self.playhead
                .set_speed(speed)
                .map_err(|e| OpenRefusal::new(400, "bad-request", e.0))?;
        }
        if let Some(t) = num("t")? {
            if !(0.0..9.2e9).contains(&t) {
                return Err(OpenRefusal::new(
                    400,
                    "bad-request",
                    "t is Unix seconds on the capture clock",
                ));
            }
            self.playhead
                .seek((t * 1e9).round() as i64)
                .map_err(|e| OpenRefusal::new(400, "bad-request", e.0))?;
        }
        let Some(pos) = self.playhead.position_ns() else {
            return Err(OpenRefusal::new(
                409,
                "no-playhead",
                "the playhead has no position: give t, or seek it first (POST /api/playback)",
            ));
        };
        // The horizon, decided now rather than discovered when the sound stops.
        let mut window = self.archive.open_at(pos, (lo, hi)).map_err(|n| {
            let mut r = OpenRefusal::new(409, "no-iq", n.reason);
            if let Some(next) = n.next_iq_ns {
                r.reason.push_str(&format!(
                    "; raw IQ next exists from {:.3} s",
                    next as f64 / 1e9
                ));
            }
            r
        })?;
        // The legal gate on the requested extent, before any sample is read (as Listen does).
        listen_class(window.content_class, &self.config.rules, lo, hi)?;
        // Probe: estimate the mode, channel and noise from the IQ itself (as Listen does).
        let (plan, probe_result, fc, fs) = self.probe(&mut window, lo, hi)?;
        // Gated again on the channel the probe chose, under the class the IQ was captured with.
        let (clo, chi) = plan.channel_extent_hz();
        let class = listen_class(window.content_class, &self.config.rules, clo, chi)?;
        let mut header = StreamHeader::new(
            format!("playback/{}", self.seq.fetch_add(1, Ordering::Relaxed)),
            StreamKind::Audio,
            class,
            "hk-pipeline:playback",
        );
        header.datatype = Some(AUDIO_DATATYPE.into());
        header.sample_rate_hz = Some(AUDIO_SAMPLE_RATE_HZ);
        header.center_hz = Some(plan.channel_center_hz);
        header.bandwidth_hz = Some(plan.channel_bandwidth_hz);
        header.max_frame_len = AUDIO_MAX_FRAME_LEN;
        let cfg = &self.config.audio;
        header.audio = Some(AudioInfo {
            channels: 1,
            frame_samples: AUDIO_FRAME_SAMPLES as u32,
            mode: plan.mode_name().into(),
            mode_confidence: probe_result.mode.confidence,
            mode_rules: probe_result.mode.rules_version.clone(),
            params: probe_result.params.estimated_params(),
            snr_db: probe_result.params.snr_box_db.value(),
            squelch: SquelchInfo {
                open_snr_db: cfg.squelch_open_snr_db,
                hysteresis_db: cfg.squelch_hysteresis_db,
                noise_dbfs: plan.noise_power.map(|n| 10.0 * n.log10()),
            },
            agc: AgcInfo {
                enabled: plan.agc,
                target_dbfs: cfg.agc_target_dbfs,
                max_gain_db: cfg.agc_max_gain_db,
            },
            deemphasis_s: plan.deemphasis_s,
            demod: LISTEN_DEMOD_VERSION.into(),
            refinement: None,
            ..AudioInfo::default()
        });
        let publisher = Publisher::new(
            header.clone(),
            PublisherConfig {
                queue_bytes: self.config.queue_bytes,
                disconnect_after_drops: u64::MAX,
                disconnect_after: Duration::from_secs(5),
                max_consumers: 1,
                drain_timeout: Duration::from_millis(500),
            },
        )
        .map_err(|e| OpenRefusal::new(500, "publisher", e.to_string()))?;
        let handle = publisher.handle();
        let stop = Arc::new(AtomicBool::new(false));
        let session = Session {
            playhead: Arc::clone(&self.playhead),
            archive: Arc::clone(&self.archive),
            publisher,
            plan,
            band: (lo, hi),
            audio: self.config.audio.clone(),
            status_interval: self.config.status_interval,
            counters: Arc::clone(&self.counters),
            stop: Arc::clone(&stop),
            tuned: (fc, fs),
        };
        self.counters.running.fetch_add(1, Ordering::SeqCst);
        if let Err(e) = thread::Builder::new()
            .name("hk-playback".into())
            .spawn(move || session.run())
        {
            self.counters.running.fetch_sub(1, Ordering::SeqCst);
            return Err(OpenRefusal::new(500, "spawn", e.to_string()));
        }
        Ok(OpenedStream {
            header,
            handle,
            session: Box::new(StopOnDrop(stop)),
            end: SessionEndSlot::default(),
        })
    }

    /// Reads `probe_s` of the window (unpaced: it is already recorded) and chooses the plan.
    fn probe(
        &self,
        window: &mut IqWindow,
        lo: f64,
        hi: f64,
    ) -> Result<(AudioPlan, hk_demod::audio::ProbeResult, f64, f64), OpenRefusal> {
        let mut buf = Vec::new();
        let mut iq: Vec<Complex32> = Vec::new();
        let mut first = None;
        loop {
            let h = window
                .source
                .read_block(&mut buf)
                .map_err(|e| OpenRefusal::new(500, "read", e.to_string()))?;
            let Some(h) = h else { break };
            let (fc, fs) = (h.provenance.get().tune.center_hz, h.sample_rate_hz());
            match &first {
                None => first = Some((h.clone(), fc, fs)),
                // Contiguous and under one tuning only.
                Some((f, c, r)) => {
                    let expect = f.first_sample() + iq.len() as u64;
                    if h.first_sample() != expect || fc != *c || fs != *r {
                        break;
                    }
                }
            }
            iq.extend_from_slice(&buf);
            if iq.len() as f64 >= self.config.probe_s * fs {
                break;
            }
        }
        let Some((h0, fc, fs)) = first else {
            return Err(OpenRefusal::new(
                409,
                "no-iq",
                "the IQ window holds no samples",
            ));
        };
        if !(fc - fs / 2.0 <= lo && hi <= fc + fs / 2.0) {
            return Err(OpenRefusal::new(
                409,
                "outside-window",
                format!(
                    "the extent is outside the window captured then ({:.0}..{:.0} Hz)",
                    fc - fs / 2.0,
                    fc + fs / 2.0
                ),
            ));
        }
        let start = h0.first_sample();
        let request = SnippetRequest {
            start_index: start,
            end_index: start + iq.len() as u64,
            center_offset_hz: 0.5 * (lo + hi) - fc,
            bandwidth_hz: hi - lo,
        };
        let result = probe(InputInfo::from(&h0), &iq, &request)
            .map_err(|e| OpenRefusal::new(422, "probe", e.to_string()))?;
        let plan = AudioPlan::from_probe(&result, &self.config.audio)
            .map_err(|r| OpenRefusal::new(422, "no-analog-modulation", r))?;
        Ok((plan, result, fc, fs))
    }
}

impl StreamOpener for PlaybackService {
    fn open(&self, request: &OpenRequest) -> Result<OpenedStream, OpenRefusal> {
        let r = self.open_inner(request);
        let c = match &r {
            Ok(_) => &self.counters.opened,
            Err(_) => &self.counters.refused,
        };
        c.fetch_add(1, Ordering::Relaxed);
        r
    }

    fn describe(&self) -> Value {
        json!({
            "kind": "audio",
            "datatype": AUDIO_DATATYPE,
            "params": ["f_lo", "f_hi", "t", "speed"],
            "playhead": "one (GET/POST /api/playback)",
        })
    }
}

struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct Session {
    playhead: Arc<Playhead>,
    archive: Arc<dyn IqArchive>,
    publisher: Publisher,
    plan: AudioPlan,
    band: (f64, f64),
    audio: AudioConfig,
    status_interval: Duration,
    counters: Arc<PlaybackCounters>,
    stop: Arc<AtomicBool>,
    /// Tuning the current demodulator was built for.
    tuned: (f64, f64),
}

/// The status a playback stream publishes (flat metadata).
#[derive(Default)]
struct Status {
    iq: bool,
    origin: &'static str,
    iq_until_ns: Option<i64>,
    next_iq_ns: Option<i64>,
    frames: u64,
    squelched: u64,
    windows: u64,
    rds_groups: u64,
    rds_pi: Option<u16>,
}

impl Session {
    fn demod_for(&self, fc: f64, fs: f64) -> Option<AudioDemod> {
        AudioDemod::new(self.plan.clone(), self.audio.clone(), fs, fc)
            .and_then(AudioDemod::with_rds)
            .ok()
    }

    fn contains(&self, fc: f64, fs: f64) -> bool {
        let (lo, hi) = self.plan.channel_extent_hz();
        fc - fs / 2.0 <= lo.min(self.band.0) && hi.max(self.band.1) <= fc + fs / 2.0
    }

    fn publish_status(&mut self, st: &Status, demod: Option<&AudioDemod>, t: Timestamp) {
        let head = self.playhead.state();
        let mut v = json!({
            "position_ns": head.position_ns,
            "playing": head.playing,
            "speed": head.speed,
            "generation": head.generation,
            "analysis": "recorded",
            "iq": st.iq,
            "iq_source": st.origin,
            "frames": st.frames,
            "squelched_frames": st.squelched,
            "windows": st.windows,
            "rds_groups": st.rds_groups,
        });
        let m = v.as_object_mut().expect("object");
        if let Some(u) = st.iq_until_ns {
            m.insert("iq_until_ns".into(), u.into());
        }
        if let Some(n) = st.next_iq_ns {
            m.insert("next_iq_ns".into(), n.into());
        }
        if let Some(pi) = st.rds_pi {
            m.insert("rds_pi".into(), pi.into());
        }
        if let Some(d) = demod {
            m.insert(
                "level_dbfs".into(),
                json!((d.level_dbfs() * 100.0).round() / 100.0),
            );
            m.insert("squelch_open".into(), d.squelch_open().into());
            m.insert(
                "agc_gain_db".into(),
                json!((d.agc_gain_db() * 100.0).round() / 100.0),
            );
            if let Some(snr) = d.snr_db() {
                m.insert("snr_db".into(), json!((snr * 100.0).round() / 100.0));
            }
        }
        let _ = self.publisher.publish_status(t, st.frames, &v);
    }

    fn run(mut self) {
        let mut st = Status::default();
        let mut buf: Vec<Complex32> = Vec::new();
        let mut pending: Vec<f32> = Vec::new();
        let mut payload = Vec::new();
        let mut window: Option<IqWindow> = None;
        let mut demod: Option<AudioDemod> = None;
        let mut generation = self.playhead.state().generation;
        // Capture time of audio sample 0 of the current contiguous run, and the audio index.
        let mut audio_t0: Option<Timestamp> = None;
        let mut audio_index: u64 = 0;
        let mut gap = false;
        // Capture time where the last demodulated block ended (the next contiguous sample), and
        // half a sample period at that rate. Within one playhead generation the next IQ window
        // opens HERE, never at the playhead: the playhead runs ahead of the demodulator by the
        // time a block takes to demodulate, and opening at it would skip that IQ while the audio
        // clock (`audio_t0` + index) kept counting - records stamped earlier than their air.
        let mut last_end_ns: Option<i64> = None;
        let mut half_sample_ns: i64 = 0;
        let mut last_status = Instant::now()
            .checked_sub(self.status_interval)
            .unwrap_or_else(Instant::now);
        let mut announced_no_iq = false;
        let mut read_any = false;
        'run: loop {
            if self.stop.load(Ordering::SeqCst) {
                break;
            }
            let head = self.playhead.state();
            if head.generation != generation {
                generation = head.generation;
                window = None;
                demod = None;
                pending.clear();
                audio_t0 = None;
                last_end_ns = None;
                gap = true;
            }
            if window.is_none() {
                let Some(pos) = head.position_ns else {
                    thread::sleep(Duration::from_millis(20));
                    continue;
                };
                // Continue exactly where the last block ended; the half-sample margin makes the
                // "first sample at or after" rule land on that very sample despite per-window
                // rounding of sample times. Only with nothing demodulated yet in this
                // generation (a seek, a fresh stream, or after the IQ horizon) does the
                // playhead decide.
                let from = last_end_ns.map_or(pos, |e| e - half_sample_ns);
                match self.archive.open_at(from, self.band) {
                    Ok(w) => {
                        st.iq = true;
                        st.origin = w.origin.as_str();
                        st.iq_until_ns = Some(w.t1_ns);
                        st.next_iq_ns = None;
                        st.windows += 1;
                        announced_no_iq = false;
                        self.counters.windows.fetch_add(1, Ordering::Relaxed);
                        window = Some(w);
                        read_any = false;
                    }
                    Err(n) => {
                        // Beyond the IQ horizon: waterfall-only. Say so once, then keep
                        // following the playhead in case IQ resumes (a later ring segment or a
                        // recording).
                        st.iq = false;
                        st.origin = "none";
                        st.iq_until_ns = None;
                        st.next_iq_ns = n.next_iq_ns;
                        if !announced_no_iq {
                            announced_no_iq = true;
                            self.counters.no_iq.fetch_add(1, Ordering::Relaxed);
                            self.publish_status(&st, None, Timestamp::from_unix_nanos(from));
                        }
                        gap = true;
                        demod = None;
                        audio_t0 = None;
                        pending.clear();
                        // Nothing continues across the horizon: when IQ resumes, it resumes at
                        // the playhead, re-anchored.
                        last_end_ns = None;
                        // Wait for the playhead to move on (100 ms of capture time), then ask
                        // again from wherever it is: the ring may have grown, or a recording
                        // may begin there.
                        let until = from.max(pos).saturating_add(100_000_000);
                        match self.playhead.wait_until(until, generation, &self.stop) {
                            Wait::Stopped => break 'run,
                            Wait::Seeked | Wait::Reached => continue,
                        }
                    }
                }
            }
            let Some(w) = window.as_mut() else { continue };
            let header = match w.source.read_block(&mut buf) {
                Ok(Some(h)) => h,
                Ok(None) => {
                    // The window is done. The next one starts where its last block ended — not
                    // at the window's nominal end, which may lie past the ring's live edge and
                    // would skip samples that arrive meanwhile. A window that yielded nothing
                    // moves on by its whole span, so this cannot spin.
                    if !read_any {
                        last_end_ns = Some(last_end_ns.map_or(w.t1_ns, |e| e.max(w.t1_ns)));
                        half_sample_ns = 0;
                    }
                    window = None;
                    continue;
                }
                Err(_) => {
                    window = None;
                    gap = true;
                    continue;
                }
            };
            let fs = header.sample_rate_hz();
            let fc = header.provenance.get().tune.center_hz;
            let t_start = header.time.host_time.as_unix_nanos();
            let t_end = header
                .time
                .time_of(header.first_sample() + buf.len() as u64, fs)
                .as_unix_nanos();
            read_any = true;
            // Paced by the playhead: demodulate a block once "now" has reached its end.
            match self.playhead.wait_until(t_end, generation, &self.stop) {
                Wait::Stopped => break,
                Wait::Seeked => continue,
                Wait::Reached => {}
            }
            // Continuity is judged by capture time, not by a window's first-block flags: each
            // ring window is its own replay source whose first block says STREAM_START even when
            // the IQ runs on without a break. A block that does not start where the last one
            // ended is a real break: re-anchor the audio clock there and drop the partial frame.
            let contiguous = last_end_ns.is_some_and(|e| (t_start - e).abs() <= half_sample_ns)
                && header.dropped_before == 0;
            if !contiguous {
                gap = true;
                audio_t0 = None;
                pending.clear();
            }
            last_end_ns = Some(t_end);
            half_sample_ns = (0.5e9 / fs).round() as i64;
            if !self.contains(fc, fs) {
                // Tuned elsewhere then: no audio for this block, and honestly so.
                demod = None;
                gap = true;
                audio_t0 = None;
                continue;
            }
            if demod.is_none() || self.tuned != (fc, fs) {
                self.tuned = (fc, fs);
                demod = self.demod_for(fc, fs);
                pending.clear();
                audio_t0 = None;
            }
            let Some(d) = demod.as_mut() else { continue };
            if d.process(InputInfo::from(&header), &buf).is_err() {
                demod = None;
                gap = true;
                continue;
            }
            self.counters
                .samples
                .fetch_add(buf.len() as u64, Ordering::Relaxed);
            for g in d.take_rds_groups() {
                st.rds_groups += 1;
                if let Some(pi) = g.pi {
                    st.rds_pi = Some(pi);
                }
            }
            if audio_t0.is_none() {
                audio_t0 = Some(Timestamp::from_unix_nanos(t_start));
                audio_index = 0;
            }
            d.drain_audio_into(&mut pending);
            let frame = AUDIO_FRAME_SAMPLES;
            let mut offset = 0;
            while pending.len() - offset >= frame {
                let samples = &pending[offset..offset + frame];
                offset += frame;
                let index = audio_index;
                audio_index += frame as u64;
                if !d.squelch_open() {
                    st.squelched += 1;
                    gap = true;
                    continue;
                }
                payload.clear();
                encode_pcm(samples, &mut payload);
                // The capture time the audio came from: the one shared time axis.
                let t = audio_t0
                    .unwrap_or(Timestamp::from_unix_nanos(t_start))
                    .saturating_add_nanos(
                        (index as f64 * 1e9 / AUDIO_SAMPLE_RATE_HZ).round() as i64
                    );
                let flags = if gap {
                    RecordFlags::DISCONTINUITY
                } else {
                    RecordFlags::empty()
                };
                if self
                    .publisher
                    .publish_binary(BinaryRecord {
                        t,
                        sample_index: index,
                        flags,
                        payload: &payload,
                    })
                    .is_err()
                {
                    // Fail closed: a gated record means the gate was bypassed somewhere.
                    break 'run;
                }
                gap = false;
                st.frames += 1;
                self.counters.frames.fetch_add(1, Ordering::Relaxed);
            }
            pending.drain(..offset);
            if last_status.elapsed() >= self.status_interval {
                last_status = Instant::now();
                self.publish_status(&st, demod.as_ref(), Timestamp::from_unix_nanos(t_end));
            }
        }
        self.counters.running.fetch_sub(1, Ordering::SeqCst);
        self.publisher.finish();
    }
}
