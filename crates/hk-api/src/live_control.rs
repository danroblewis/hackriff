//! Live front-end control (T-042, T-050): a typed, device-generic handle from the running
//! pipeline's source into [`crate::ApiState::live_controls`], for the authenticated control API
//! ([`crate::control`]) to change the tuned centre, sample rate, named gains and (optionally) the
//! bias tee. **Receive-only controls**: there is no transmit operation, and none can be added
//! through this trait (C37 stays gated).
//!
//! - [`LiveControl`] is the contract: read the capabilities and current [`LiveTuning`], then
//!   `set_center` / `set_rate` / `set_gains` (named stages, e.g. `lna`, `vga`, `amp` on a HackRF
//!   One; any stage a device's capabilities list) / `set_bias_tee` (optional capability). Each
//!   returns the tuning now in force, or a [`LiveControlError`] with an HTTP status
//!   ([`LiveControlError::http_status`]).
//! - [`SourceLiveControl`] implements it over any `hk_core::SourceControl`: values are checked
//!   against the source's `SourceCapabilities` (frequency ranges, sample rates, named gain stages
//!   quantised by `GainStage::quantise`, bias-tee capability) before the command is posted; the
//!   source applies it at its next block boundary. Requests are serialised.
//! - **Window changes go through the pipeline** when the composition sets a [`WindowRetuner`]
//!   ([`SourceLiveControl::with_retuner`], what `hk serve` does since T-050): centre and rate
//!   changes are handed to the running pipeline, which re-derives the window's content class and
//!   re-plumbs itself at a block boundary when the class or rate changes (so tuning anywhere is
//!   allowed and gating always matches the window), or tunes in place otherwise.
//! - **Older policy hooks** (no retuner):
//!   - [`SourceLiveControl::with_window_policy`] refuses a `(centre, rate)` window the run may
//!     not tune to.
//!   - [`SourceLiveControl::with_fixed_rate`] refuses rate changes.
//!
//! # Device actions (T-343)
//!
//! Six of this trait's operations **reach the front end**; everything else in the control API
//! (display, recording, bookmarks, selections) only changes what is shown. That
//! asymmetry is now a type, [`DeviceAction`], not a comment: `set_center`, `set_rate`,
//! `set_window`, `set_gains`, `set_bias_tee` and `set_baseband_filter` each name their variant,
//! the control API classifies its routes into the same enum (`hk_api::control`'s
//! `Action::device_action`), and a reader can see which paths are device actions from the enum
//! alone. A view change cannot reach the device because a view change has no `DeviceAction` to
//! name.
//!
//! [`LiveControl::set_window`] (T-529) is the one a **user retune** takes: a region names a centre
//! *and* a span, and commanding them separately commands a window nobody asked for in between. See
//! that method for what was observable in the gap.
//!
//! Every device action passes through one [`DeviceGate`], which:
//!
//! - **serialises** device actions against each other, so a retune cannot interleave with a gain
//!   change or a second retune (the front end is one shared resource — the HackRF
//!   one-agent-at-a-time rule as it applies inside this process);
//! - **fails cleanly rather than racing**: a device action that cannot take the gate within
//!   [`DEVICE_GATE_WAIT`] answers 409 [`LiveControlError::DeviceBusy`], naming the device, the
//!   action that holds it and for how long, instead of blocking the caller for a whole re-plumb;
//! - **carries the device identity**: [`DeviceGate::device_id`] is the source's own
//!   `SourceControl::device_info().device_id`, which the source contract's `device-info`
//!   conformance check requires to equal the `device_id` on that source's provenance. So the id a
//!   retune is recorded against is the *same string* that appears on every frame and detection the
//!   front end produces — not a parallel name for it. `None` means the source reports no identity:
//!   nothing is written rather than a placeholder (the T-325 rule, "nothing said" is never a
//!   value).
//!
//! Reads ([`LiveControl::tuning`]) never enter the gate, so a slow re-plumb cannot block the
//! state poll.
//!
//! # N front ends (T-511)
//!
//! A server holds [`LiveControls`], not one handle: a collection keyed by `device_id`, with **one
//! gate per device** (each handle carries its own), and a device selector on the control routes
//! that may be omitted only when the run holds exactly one front end. See [`LiveControls`].
//!
//! **Cross-process** exclusion is the device open itself: only one process can open a HackRF, so
//! a second server fails at open, not here. This gate is the in-process half.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use hk_core::{NamedGain, SourceCapabilities, SourceControl, SourceError};
use hk_model::ContentClass;

/// The front-end settings in force (as last accepted).
#[derive(Clone, Debug, PartialEq)]
pub struct LiveTuning {
    /// Centre frequency, Hz.
    pub center_hz: f64,
    /// Sample rate, Hz (the displayed span).
    pub sample_rate_hz: f64,
    /// Named gains, one per stage set so far.
    pub gains: Vec<NamedGain>,
    /// Bias-tee state (T-325). `Unknown` until this server has set it, so the panel never claims
    /// the DC is off on no evidence — whether the control exists at all is the device's
    /// `capabilities.bias_tee`, not this field.
    pub bias_tee: hk_model::BiasTee,
    /// Baseband (anti-alias) filter bandwidth, Hz; `None` when never set explicitly (the device's
    /// default, usually derived from the sample rate) or the device has no selectable filter.
    pub baseband_filter_hz: Option<f64>,
}

/// A control operation that **reaches the front end** (T-343).
///
/// This enum is the boundary between changing the world and changing the view. Holding the view,
/// scrubbing, zooming, display settings, recording and bookmarks have no variant here because they
/// never touch the device (T-339's invariant) — and the first three reach no route at all (T-347);
/// the six that do each name themselves, so a reader of a call site can see it is a device action
/// without tracing it to the driver.
///
/// A retune in particular is not a view change: `hk_pipeline::PipelineController::retune` tunes
/// in place only when the window class and sample rate are both unchanged, and otherwise stops
/// and re-plumbs the running segment. Nothing that only changes what is shown may reach it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeviceAction {
    /// Move the front end's centre frequency (`POST /api/control/center`).
    Retune,
    /// Change the sample rate / instantaneous span (`POST /api/control/rate`).
    Rate,
    /// Commit a whole window — centre **and** sample rate — as one action
    /// (`POST /api/control/window`, T-529). See [`LiveControl::set_window`].
    Window,
    /// Set named gain stages (`POST /api/control/gains`).
    Gains,
    /// Switch the antenna-port bias tee (`POST /api/control/bias_tee`).
    BiasTee,
    /// Select the baseband anti-alias filter (`POST /api/control/baseband_filter`).
    BasebandFilter,
}

impl DeviceAction {
    /// The stable name used in the control response and the audit log.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Retune => "retune",
            Self::Rate => "rate",
            Self::Window => "window",
            Self::Gains => "gains",
            Self::BiasTee => "bias_tee",
            Self::BasebandFilter => "baseband_filter",
        }
    }
}

impl fmt::Display for DeviceAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How long a device action waits for the front end before answering
/// [`LiveControlError::DeviceBusy`]. Long enough to absorb a tune-in-place (microseconds) or a
/// gain write, far shorter than a re-plumb (`hk_pipeline::REPLUMB_TIMEOUT`, tens of seconds): a
/// caller that arrives during a re-plumb is told the device is busy instead of being parked.
pub const DEVICE_GATE_WAIT: Duration = Duration::from_millis(250);

/// The one front end, held by at most one device action at a time (T-343; see the [module
/// docs](self)).
#[derive(Debug)]
pub struct DeviceGate {
    device_id: Option<String>,
    held: Mutex<Option<Held>>,
    free: Condvar,
}

#[derive(Clone, Copy, Debug)]
struct Held {
    action: DeviceAction,
    since: Instant,
}

impl DeviceGate {
    /// A gate over the front end whose provenance `device_id` is `device_id` (`None` when the
    /// source reports no identity).
    pub fn new(device_id: Option<String>) -> Self {
        Self {
            device_id,
            held: Mutex::new(None),
            free: Condvar::new(),
        }
    }

    /// The front end's provenance `device_id`, or `None` when the source does not report one.
    ///
    /// `None` is **not** an identity: callers omit the field rather than writing a placeholder,
    /// so "this retune moved an unnamed device" is never confusable with "this retune moved
    /// device X" (the T-325 rule applied to device identity).
    pub fn device_id(&self) -> Option<&str> {
        self.device_id.as_deref()
    }

    /// Claims the front end for `action`, waiting at most [`DEVICE_GATE_WAIT`].
    ///
    /// The guard releases it on drop. A contended claim fails with
    /// [`LiveControlError::DeviceBusy`] naming the holder — it never races the holder to the
    /// driver, and never blocks for the length of a re-plumb.
    pub fn enter(&self, action: DeviceAction) -> Result<DeviceGuard<'_>, LiveControlError> {
        let held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let (mut held, wait) = self
            .free
            .wait_timeout_while(held, DEVICE_GATE_WAIT, |h| h.is_some())
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(h) = *held {
            debug_assert!(wait.timed_out());
            return Err(LiveControlError::DeviceBusy {
                device_id: self.device_id.clone(),
                holder: h.action,
                held_for_s: h.since.elapsed().as_secs_f64(),
                requested: action,
            });
        }
        *held = Some(Held {
            action,
            since: Instant::now(),
        });
        Ok(DeviceGuard { gate: self })
    }
}

/// The front end, claimed for one [`DeviceAction`] ([`DeviceGate::enter`]); released on drop.
#[derive(Debug)]
pub struct DeviceGuard<'a> {
    gate: &'a DeviceGate,
}

impl Drop for DeviceGuard<'_> {
    fn drop(&mut self) {
        *self
            .gate
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        self.gate.free.notify_one();
    }
}

/// Why a control request was not applied.
#[derive(Debug)]
pub enum LiveControlError {
    /// The value is outside the device's capabilities (HTTP 400).
    OutOfRange {
        /// Which setting (e.g. `centre frequency (Hz)`, `gain stage lna`).
        what: String,
        /// The rejected value.
        value: f64,
    },
    /// The device lacks the capability, e.g. a bias tee (HTTP 501).
    Unsupported(&'static str),
    /// The running pipeline refuses it: another content class, or a fixed rate (HTTP 409).
    Refused(String),
    /// The source rejected the command (HTTP 502; 400 for its own range checks).
    Source(SourceError),
    /// A malformed request value (HTTP 400).
    Invalid(String),
    /// Device settings on a run that is not live, e.g. a replayed recording (HTTP 409).
    NotLive(String),
    /// Conflicts with the run's state: a re-plumb in progress, a recording already running
    /// (HTTP 409).
    Conflict(String),
    /// Another device action holds the front end (T-343; HTTP 409, code `device_busy`). The
    /// front end is one shared resource: this request did not race the holder to the driver.
    DeviceBusy {
        /// The front end's provenance `device_id`, or `None` when the source reports none.
        device_id: Option<String>,
        /// The device action holding it.
        holder: DeviceAction,
        /// How long the holder has held it, seconds.
        held_for_s: f64,
        /// The device action that was refused.
        requested: DeviceAction,
    },
    /// Did not finish in time (HTTP 504).
    Timeout(String),
    /// The run has finished (HTTP 409).
    Finished(String),
    /// Anything else (HTTP 500).
    Failed(String),
}

impl LiveControlError {
    /// The HTTP status an endpoint should answer with.
    pub fn http_status(&self) -> u16 {
        match self {
            Self::OutOfRange { .. } | Self::Source(SourceError::OutOfRange { .. }) => 400,
            Self::Invalid(_) => 400,
            Self::Unsupported(_) | Self::Source(SourceError::Unsupported { .. }) => 501,
            Self::Refused(_)
            | Self::NotLive(_)
            | Self::Conflict(_)
            | Self::DeviceBusy { .. }
            | Self::Finished(_) => 409,
            Self::Source(_) => 502,
            Self::Timeout(_) => 504,
            Self::Failed(_) => 500,
        }
    }

    /// A stable machine-readable code for clients (the UI switches on it).
    pub fn code(&self) -> &'static str {
        match self {
            Self::OutOfRange { .. } | Self::Source(SourceError::OutOfRange { .. }) => {
                "out_of_range"
            }
            Self::Invalid(_) => "invalid",
            Self::Unsupported(_) | Self::Source(SourceError::Unsupported { .. }) => "unsupported",
            Self::Refused(_) => "refused",
            Self::NotLive(_) => "not_live",
            Self::Conflict(_) => "conflict",
            Self::DeviceBusy { .. } => "device_busy",
            Self::Timeout(_) => "timeout",
            Self::Finished(_) => "finished",
            Self::Source(_) => "device_error",
            Self::Failed(_) => "failed",
        }
    }
}

impl fmt::Display for LiveControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange { what, value } => write!(f, "{what} {value} is out of range"),
            Self::Unsupported(what) => write!(f, "the device has no {what}"),
            Self::Refused(why)
            | Self::Invalid(why)
            | Self::NotLive(why)
            | Self::Conflict(why)
            | Self::Timeout(why)
            | Self::Finished(why)
            | Self::Failed(why) => f.write_str(why),
            Self::DeviceBusy {
                device_id,
                holder,
                held_for_s,
                requested,
            } => {
                // No device id is written when the source reports none: "the front end" is not a
                // claim about which one (T-325: nothing said is never a value).
                let which = match device_id {
                    Some(id) => format!("the front end ({id})"),
                    None => "the front end".to_owned(),
                };
                write!(
                    f,
                    "{which} is busy: a {holder} has held it for {held_for_s:.1} s, so this \
                     {requested} was not applied; try again when it settles"
                )
            }
            Self::Source(e) => write!(f, "{e}"),
        }
    }
}

/// Moves the running pipeline to a new window (T-050): re-derives the content class and
/// re-plumbs at a block boundary when the class or rate changes, or tunes in place. Implemented
/// by the composition over `hk_pipeline::PipelineController`.
pub trait WindowRetuner: Send + Sync {
    /// Retunes to `(center_hz, sample_rate_hz)`; returns the content class in force afterwards.
    /// [`LiveControlError::Timeout`] means the change continues in the background.
    fn retune(&self, center_hz: f64, sample_rate_hz: f64)
    -> Result<ContentClass, LiveControlError>;

    /// The window the pipeline runs (or is moving to), read back after a timed-out retune;
    /// `None` when the retuner cannot tell.
    fn applied_window(&self) -> Option<AppliedWindow> {
        None
    }
}

/// A window as the pipeline reports it ([`WindowRetuner::applied_window`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AppliedWindow {
    /// Centre, Hz.
    pub center_hz: f64,
    /// Sample rate, Hz.
    pub sample_rate_hz: f64,
    /// A re-plumb is still in progress (the values may still change).
    pub settling: bool,
    /// How many times the pipeline has changed the window **by itself** — a capture recovery
    /// (T-508) — rather than at a retune's request. A change in it is adopted even when nothing is
    /// pending. 0 for a retuner that never moves the window on its own.
    pub pipeline_moves: u64,
}

impl std::error::Error for LiveControlError {}

/// Live control of the running source (see the [module docs](self)).
pub trait LiveControl: Send + Sync {
    /// What the source can do (ranges, gain stages, bias tee).
    fn capabilities(&self) -> &SourceCapabilities;
    /// The settings in force.
    fn tuning(&self) -> LiveTuning;
    /// The front end's provenance `device_id` (T-343), e.g. `hackrf:<serial>`, or `None` when the
    /// source reports no identity.
    ///
    /// Every [`DeviceAction`] is recorded against this id — a retune that cannot say which device
    /// it moved is the same gap T-302/T-303/T-304/T-305 closed for artifacts, baselines, history
    /// and the source layer. `None` is not an identity: callers omit the field rather than
    /// writing a placeholder.
    fn device_id(&self) -> Option<&str> {
        None
    }
    /// Retunes the centre frequency, Hz ([`DeviceAction::Retune`]).
    fn set_center(&self, center_hz: f64) -> Result<LiveTuning, LiveControlError>;
    /// Changes the sample rate (span), Hz ([`DeviceAction::Rate`]).
    fn set_rate(&self, sample_rate_hz: f64) -> Result<LiveTuning, LiveControlError>;
    /// Commits a whole window — centre **and** sample rate — as **one** device action
    /// ([`DeviceAction::Window`], T-529).
    ///
    /// # Why this is not `set_rate` then `set_center`
    ///
    /// A user retune to a region names a capture *configuration*: the centre the radio will sit on
    /// and the span it will open to. The pipeline has always taken it that way —
    /// [`WindowRetuner::retune`] is `(center_hz, sample_rate_hz)`, one call, one class derivation,
    /// one re-plumb. Only the HTTP surface split it, and [`SourceLiveControl::retune_window`] then
    /// filled the half the caller had not named from the tuning *in force*. Two posts therefore
    /// commanded two windows, the first of which — the **old centre at the new rate** — is a window
    /// no user ever asked for and which nothing on the wire distinguishes from one they did:
    ///
    /// - it is what [`LiveControl::tuning`] (and so `GET /api/control/state`) reports in between;
    /// - a whole segment is captured at it, with spectrum headers and provenance to match, so the
    ///   coverage map records "we observed here" for a band chosen by an HTTP artefact — the exact
    ///   dishonesty the grey-is-unobserved rule exists to prevent;
    /// - it costs a re-plumb of its own, so one press tears the readers down twice;
    /// - and the second request races the first segment's first block: a device refusal that lands
    ///   while the second re-plumb is already queued is swallowed by the supervisor's
    ///   request-first branch and counted as neither failure nor recovery.
    ///
    /// One call, one window, one class, at most one re-plumb. There is **no half-applied window to
    /// observe**, because the intermediate one is never commanded.
    ///
    /// The single-field routes keep working: `set_center` means "this centre, keep the rate", which
    /// is exactly what a nudge, a bookmark or a typed frequency asks for, and the sweep's steps are
    /// centre-only by design (`crate::scan`).
    fn set_window(
        &self,
        center_hz: f64,
        sample_rate_hz: f64,
    ) -> Result<LiveTuning, LiveControlError>;
    /// Sets named gain stages ([`DeviceAction::Gains`]; all validated before any is applied, each
    /// quantised to its stage).
    fn set_gains(&self, gains: &[NamedGain]) -> Result<LiveTuning, LiveControlError>;
    /// Switches the bias tee ([`DeviceAction::BiasTee`]; optional capability).
    fn set_bias_tee(&self, enabled: bool) -> Result<LiveTuning, LiveControlError>;
    /// Selects the baseband (anti-alias) filter bandwidth, Hz ([`DeviceAction::BasebandFilter`];
    /// optional capability; T-067).
    fn set_baseband_filter(&self, bandwidth_hz: f64) -> Result<LiveTuning, LiveControlError>;
}

/// **The live front ends this server holds** (T-511), keyed by `device_id`.
///
/// # Why a collection, and why keyed
///
/// `ApiState::live_controls` was one `Option<Arc<dyn LiveControl>>`, and every place that needed
/// "the device" took it. That was true of a run with one front end and of nothing else: the
/// product direction is several SDRs collecting at once (CLAUDE.md, "Allow several SDRs at once"),
/// `/api/navigation`'s `windows` has spoken in lists since T-340, and T-510 made one `Pipeline`
/// spawn N `{source, ring, capture thread}` sets. This is the serving half of that seam.
///
/// Three properties it must have, and the reasons they are properties rather than conventions:
///
/// - **One [`DeviceGate`] per device, not one per server.** The gate exists because a front end is
///   one shared resource that must not be raced (T-343). Two radios are two resources: a retune of
///   A has no business waiting on, or being refused by, a gain write to B. Each [`LiveControl`]
///   carries its own gate ([`SourceLiveControl::gate`]), so holding this as a collection of
///   handles gives per-device serialisation by construction — there is no server-wide gate left to
///   accidentally share.
/// - **`device_id` is the key, and `None` is not one.** The id here is the source's own
///   `SourceControl::device_info().device_id`, the same string on every frame and detection it
///   produces. A source that reports no identity said *nothing* (the T-325 rule); nothing cannot
///   be addressed, so at most one such handle may be held — with one front end the selector is
///   omitted anyway, and with two an unnamed one could not be named.
/// - **Order is stable and the first is the default.** [`Self::primary`] is what a single-device
///   run means by "the device", so with exactly one handle every caller behaves exactly as it did
///   before this type existed.
#[derive(Clone, Default)]
pub struct LiveControls {
    controls: Vec<Arc<dyn LiveControl>>,
}

/// Why a set of live controls could not be formed ([`LiveControls::new`]).
///
/// Both variants are the *same* defect seen twice: a handle that cannot be addressed unambiguously
/// by `device_id`. Composing a server with one is refused at composition time rather than
/// answering an ambiguous selector at request time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveControlsError {
    /// Two handles report the same `device_id`.
    DuplicateDeviceId(String),
    /// More than one handle reports no `device_id` at all, so neither can be selected.
    UnnamedDevices(usize),
}

impl fmt::Display for LiveControlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateDeviceId(id) => write!(
                f,
                "two live front ends report device_id {id:?}: a device selector could not \
                 distinguish them"
            ),
            Self::UnnamedDevices(n) => write!(
                f,
                "{n} live front ends report no device_id: at most one unnamed front end can be \
                 held, because an unnamed one cannot be selected"
            ),
        }
    }
}

impl std::error::Error for LiveControlsError {}

/// Why a device selector did not resolve to a front end ([`LiveControls::select`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectError {
    /// This server runs no live front end (a replay).
    NotLive,
    /// No selector was given and this run holds more than one front end, so there is no default.
    /// Carries the ids that would resolve.
    Ambiguous(Vec<String>),
    /// The selector names no front end this run holds. Carries the request and the ids that would
    /// resolve.
    Unknown {
        /// The `device_id` the caller asked for.
        requested: String,
        /// The ids this run does hold.
        available: Vec<String>,
    },
}

impl fmt::Display for SelectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let list = |ids: &[String]| {
            if ids.is_empty() {
                "none (no front end reports an identity)".to_owned()
            } else {
                ids.join(", ")
            }
        };
        match self {
            Self::NotLive => f.write_str(
                "device settings apply to a live source; this server is not running one (a \
                 replayed recording accepts display, recording and bookmark requests only)",
            ),
            Self::Ambiguous(ids) => write!(
                f,
                "this run holds {} live front ends, so \"the device\" is not defined: name one \
                 with \"device_id\" (available: {})",
                ids.len(),
                list(ids)
            ),
            Self::Unknown {
                requested,
                available,
            } => write!(
                f,
                "no live front end with device_id {requested:?} on this server (available: {})",
                list(available)
            ),
        }
    }
}

impl std::error::Error for SelectError {}

impl SelectError {
    /// The HTTP status a control route answers with.
    pub fn http_status(&self) -> u16 {
        match self {
            // Not a client mistake: this server has no front end at all.
            Self::NotLive => 409,
            // The request is under-specified for *this* run.
            Self::Ambiguous(_) => 400,
            // The named device is not here.
            Self::Unknown { .. } => 404,
        }
    }

    /// The stable error `code` on the wire.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotLive => "not_live",
            Self::Ambiguous(_) => "device_required",
            Self::Unknown { .. } => "unknown_device",
        }
    }
}

impl LiveControls {
    /// The front ends `controls` addresses, in order.
    ///
    /// Refused when two report the same `device_id`, or when more than one reports none: either
    /// way a selector could not address them (see the [type docs](Self)).
    pub fn new(controls: Vec<Arc<dyn LiveControl>>) -> Result<Self, LiveControlsError> {
        let mut seen: Vec<&str> = Vec::new();
        let mut unnamed = 0usize;
        for c in &controls {
            match c.device_id() {
                Some(id) => {
                    if seen.contains(&id) {
                        return Err(LiveControlsError::DuplicateDeviceId(id.to_owned()));
                    }
                    seen.push(id);
                }
                None => unnamed += 1,
            }
        }
        if unnamed > 1 {
            return Err(LiveControlsError::UnnamedDevices(unnamed));
        }
        Ok(Self { controls })
    }

    /// No live front end (a replay, or a scheduler-driven run).
    pub fn none() -> Self {
        Self::default()
    }

    /// Exactly one front end — today's single-SDR run, where the selector may be omitted.
    pub fn one(control: Arc<dyn LiveControl>) -> Self {
        Self {
            controls: vec![control],
        }
    }

    /// True when this server runs no live front end.
    pub fn is_empty(&self) -> bool {
        self.controls.is_empty()
    }

    /// How many live front ends this run holds — **measured, never assumed**.
    pub fn len(&self) -> usize {
        self.controls.len()
    }

    /// The **default** front end: the first, and meaningful only when there is exactly one.
    ///
    /// Routes that act on a device must use [`Self::select`], which refuses to guess when the run
    /// holds more than one. This is for the reads whose contract is explicitly "one device's
    /// state" (`/api/navigation`'s `frequency.current`, which `docs/api.md` already documents as
    /// one device's tuned state and not an enumeration).
    pub fn primary(&self) -> Option<&Arc<dyn LiveControl>> {
        self.controls.first()
    }

    /// Every front end, in order.
    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn LiveControl>> {
        self.controls.iter()
    }

    /// Every `device_id` that resolves, in order (a front end reporting none contributes nothing —
    /// "nothing said" is never a key).
    pub fn device_ids(&self) -> Vec<String> {
        self.controls
            .iter()
            .filter_map(|c| c.device_id().map(str::to_owned))
            .collect()
    }

    /// **Resolves a device selector** (T-511).
    ///
    /// `None` means "the device": allowed only when this run holds exactly one front end, so a
    /// single-SDR client keeps working unchanged and a multi-SDR one is told to name a device
    /// rather than silently moving whichever radio happened to be composed first.
    pub fn select(&self, device_id: Option<&str>) -> Result<&Arc<dyn LiveControl>, SelectError> {
        match device_id {
            None => match self.controls.as_slice() {
                [] => Err(SelectError::NotLive),
                [one] => Ok(one),
                _ => Err(SelectError::Ambiguous(self.device_ids())),
            },
            Some(want) => {
                if self.controls.is_empty() {
                    return Err(SelectError::NotLive);
                }
                self.controls
                    .iter()
                    .find(|c| c.device_id() == Some(want))
                    .ok_or_else(|| SelectError::Unknown {
                        requested: want.to_owned(),
                        available: self.device_ids(),
                    })
            }
        }
    }
}

impl fmt::Debug for LiveControls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveControls")
            .field("devices", &self.device_ids())
            .field("len", &self.controls.len())
            .finish()
    }
}

impl From<Option<Arc<dyn LiveControl>>> for LiveControls {
    fn from(control: Option<Arc<dyn LiveControl>>) -> Self {
        control.map_or_else(Self::none, Self::one)
    }
}

impl From<Arc<dyn LiveControl>> for LiveControls {
    fn from(control: Arc<dyn LiveControl>) -> Self {
        Self::one(control)
    }
}

/// Decides whether the run may tune to `(center_hz, sample_rate_hz)`; `Err` explains why not.
pub type WindowPolicy = Arc<dyn Fn(f64, f64) -> Result<(), String> + Send + Sync>;

/// [`LiveControl`] over an `hk_core::SourceControl`.
///
/// With a retuner, window changes are serialised on their own lock and the tuning lock is held
/// only briefly, so reads ([`LiveControl::tuning`]) and gain changes never wait out a slow
/// re-plumb. After a timed-out retune the stored window is the requested one, marked pending,
/// and is refreshed from [`WindowRetuner::applied_window`] once the re-plumb settles.
pub struct SourceLiveControl {
    control: Arc<dyn SourceControl>,
    tuning: Mutex<LiveTuning>,
    gate: DeviceGate,
    pending: AtomicBool,
    /// The last [`AppliedWindow::pipeline_moves`] seen (T-508).
    pipeline_moves: AtomicU64,
    policy: Option<WindowPolicy>,
    fixed_rate: bool,
    retuner: Option<Arc<dyn WindowRetuner>>,
}

impl SourceLiveControl {
    /// Controls `control`, whose source currently runs with `initial`.
    ///
    /// The front end's identity comes from the source itself (`SourceControl::device_info`), so
    /// every device action is recorded against the device that actually answered it rather than
    /// against a name the caller supplied.
    pub fn new(control: Arc<dyn SourceControl>, initial: LiveTuning) -> Self {
        let device_id = control.device_info().map(|i| i.device_id);
        Self {
            control,
            tuning: Mutex::new(initial),
            gate: DeviceGate::new(device_id),
            pending: AtomicBool::new(false),
            pipeline_moves: AtomicU64::new(0),
            policy: None,
            fixed_rate: false,
            retuner: None,
        }
    }

    /// The gate every device action passes through (T-343).
    pub fn gate(&self) -> &DeviceGate {
        &self.gate
    }

    /// Takes the pipeline's window once its re-plumb has settled: after a timed-out retune, and
    /// (T-508) whenever the pipeline has **moved the window by itself** since the last look
    /// ([`AppliedWindow::pipeline_moves`]). A capture recovery can put a front end that refused a
    /// retune back on the last window that delivered, and the tuning this control reports must be
    /// that window, not the one the device refused. Other reports are not re-read.
    fn refresh(&self) {
        let Some(r) = &self.retuner else { return };
        let pending = self.pending.load(Ordering::SeqCst);
        match r.applied_window() {
            Some(w) if w.settling => {}
            Some(w) => {
                let moved = self.pipeline_moves.swap(w.pipeline_moves, Ordering::SeqCst)
                    != w.pipeline_moves;
                if pending || moved {
                    let mut t = self.lock();
                    t.center_hz = w.center_hz;
                    t.sample_rate_hz = w.sample_rate_hz;
                    self.pending.store(false, Ordering::SeqCst);
                }
            }
            None => self.pending.store(false, Ordering::SeqCst),
        }
    }

    /// A window change through the retuner (one device action at a time; the tuning lock is not
    /// held while the pipeline re-plumbs, so reads never wait out a slow one).
    fn retune_window(
        &self,
        r: &dyn WindowRetuner,
        action: DeviceAction,
        center_hz: Option<f64>,
        sample_rate_hz: Option<f64>,
    ) -> Result<LiveTuning, LiveControlError> {
        let _op = self.gate.enter(action)?;
        self.refresh();
        let (c, s) = {
            let t = self.lock();
            (
                center_hz.unwrap_or(t.center_hz),
                sample_rate_hz.unwrap_or(t.sample_rate_hz),
            )
        };
        let result = r.retune(c, s);
        if matches!(result, Ok(_) | Err(LiveControlError::Timeout(_))) {
            let mut t = self.lock();
            t.center_hz = c;
            t.sample_rate_hz = s;
            // A timeout leaves the re-plumb running towards `(c, s)`: refresh once it settles.
            self.pending.store(result.is_err(), Ordering::SeqCst);
            return result.map(|_| t.clone());
        }
        result.map(|_| self.lock().clone())
    }

    /// Hands centre and rate changes to the pipeline (see the module docs); window policies and
    /// the fixed rate no longer apply to them.
    pub fn with_retuner(mut self, retuner: Arc<dyn WindowRetuner>) -> Self {
        self.retuner = Some(retuner);
        self
    }

    /// Refuses windows `policy` rejects.
    pub fn with_window_policy(mut self, policy: WindowPolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Refuses any change of sample rate.
    pub fn with_fixed_rate(mut self) -> Self {
        self.fixed_rate = true;
        self
    }

    /// The centre is one the device can reach.
    fn check_center(&self, center_hz: f64) -> Result<(), LiveControlError> {
        if center_hz.is_finite() && self.capabilities().supports_frequency(center_hz) {
            return Ok(());
        }
        Err(LiveControlError::OutOfRange {
            what: "centre frequency (Hz)".into(),
            value: center_hz,
        })
    }

    /// The sample rate is one the device offers.
    fn check_rate(&self, sample_rate_hz: f64) -> Result<(), LiveControlError> {
        if sample_rate_hz.is_finite() && self.capabilities().sample_rates.supports(sample_rate_hz) {
            return Ok(());
        }
        Err(LiveControlError::OutOfRange {
            what: "sample rate (Hz)".into(),
            value: sample_rate_hz,
        })
    }

    /// Why a run with a fixed rate refuses to change it.
    fn fixed_rate_refusal(&self, in_force: f64) -> LiveControlError {
        LiveControlError::Refused(format!(
            "the sample rate is fixed at {in_force} Hz for this run (detection resolution, ring \
             and history geometry); restart with the new rate"
        ))
    }

    fn check_window(&self, center_hz: f64, rate_hz: f64) -> Result<(), LiveControlError> {
        match &self.policy {
            Some(p) => p(center_hz, rate_hz).map_err(LiveControlError::Refused),
            None => Ok(()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, LiveTuning> {
        self.tuning.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Checks named gains against the capabilities' stages and quantises each.
pub fn validate_gains(
    caps: &SourceCapabilities,
    gains: &[NamedGain],
) -> Result<Vec<NamedGain>, LiveControlError> {
    gains
        .iter()
        .map(|g| {
            caps.gain_stage(&g.stage)
                .and_then(|s| s.quantise(g.db))
                .map(|db| NamedGain::new(g.stage.clone(), db))
                .ok_or_else(|| LiveControlError::OutOfRange {
                    what: format!("gain stage {}", g.stage),
                    value: g.db,
                })
        })
        .collect()
}

impl LiveControl for SourceLiveControl {
    fn capabilities(&self) -> &SourceCapabilities {
        self.control.capabilities()
    }

    fn tuning(&self) -> LiveTuning {
        // A read, not a device action: it never enters the gate, so the state poll cannot be
        // blocked by a re-plumb in flight.
        self.refresh();
        self.lock().clone()
    }

    fn device_id(&self) -> Option<&str> {
        self.gate.device_id()
    }

    fn set_center(&self, center_hz: f64) -> Result<LiveTuning, LiveControlError> {
        self.check_center(center_hz)?;
        if let Some(r) = &self.retuner {
            return self.retune_window(r.as_ref(), DeviceAction::Retune, Some(center_hz), None);
        }
        let _op = self.gate.enter(DeviceAction::Retune)?;
        let mut t = self.lock();
        self.check_window(center_hz, t.sample_rate_hz)?;
        self.control
            .tune(center_hz)
            .map_err(LiveControlError::Source)?;
        t.center_hz = center_hz;
        Ok(t.clone())
    }

    fn set_rate(&self, sample_rate_hz: f64) -> Result<LiveTuning, LiveControlError> {
        self.check_rate(sample_rate_hz)?;
        if let Some(r) = &self.retuner {
            return self.retune_window(r.as_ref(), DeviceAction::Rate, None, Some(sample_rate_hz));
        }
        let _op = self.gate.enter(DeviceAction::Rate)?;
        let mut t = self.lock();
        if self.fixed_rate && sample_rate_hz != t.sample_rate_hz {
            return Err(self.fixed_rate_refusal(t.sample_rate_hz));
        }
        self.check_window(t.center_hz, sample_rate_hz)?;
        self.control
            .set_sample_rate(sample_rate_hz)
            .map_err(LiveControlError::Source)?;
        t.sample_rate_hz = sample_rate_hz;
        Ok(t.clone())
    }

    /// One window, one device action (T-529) — see [`LiveControl::set_window`].
    ///
    /// With a retuner this is [`retune_window`](SourceLiveControl::retune_window) with **both**
    /// halves named, so the pipeline derives one content class and performs at most one re-plumb;
    /// nothing is filled in from the tuning in force, which is where the phantom intermediate
    /// window came from. Without one, both driver calls are made under a single
    /// [`DeviceGate`] hold and a single tuning-lock hold, so no reader can catch the pair apart.
    fn set_window(
        &self,
        center_hz: f64,
        sample_rate_hz: f64,
    ) -> Result<LiveTuning, LiveControlError> {
        self.check_center(center_hz)?;
        self.check_rate(sample_rate_hz)?;
        if let Some(r) = &self.retuner {
            return self.retune_window(
                r.as_ref(),
                DeviceAction::Window,
                Some(center_hz),
                Some(sample_rate_hz),
            );
        }
        let _op = self.gate.enter(DeviceAction::Window)?;
        let mut t = self.lock();
        if self.fixed_rate && sample_rate_hz != t.sample_rate_hz {
            return Err(self.fixed_rate_refusal(t.sample_rate_hz));
        }
        self.check_window(center_hz, sample_rate_hz)?;
        // Rate first, then centre — the order `apply_window` uses, so the device is never asked to
        // sit on a centre under a rate that is about to change underneath it. Both happen under the
        // one lock: a partial application is a failure, not a state anything may read.
        if sample_rate_hz != t.sample_rate_hz {
            self.control
                .set_sample_rate(sample_rate_hz)
                .map_err(LiveControlError::Source)?;
            t.sample_rate_hz = sample_rate_hz;
        }
        if center_hz != t.center_hz {
            self.control
                .tune(center_hz)
                .map_err(LiveControlError::Source)?;
            t.center_hz = center_hz;
        }
        Ok(t.clone())
    }

    fn set_gains(&self, gains: &[NamedGain]) -> Result<LiveTuning, LiveControlError> {
        let _op = self.gate.enter(DeviceAction::Gains)?;
        let mut t = self.lock();
        let accepted = validate_gains(self.capabilities(), gains)?;
        for g in accepted {
            self.control
                .set_gain(&g.stage, g.db)
                .map_err(LiveControlError::Source)?;
            match t.gains.iter_mut().find(|x| x.stage == g.stage) {
                Some(x) => x.db = g.db,
                None => t.gains.push(g),
            }
        }
        Ok(t.clone())
    }

    fn set_bias_tee(&self, enabled: bool) -> Result<LiveTuning, LiveControlError> {
        let _op = self.gate.enter(DeviceAction::BiasTee)?;
        let mut t = self.lock();
        if !self.capabilities().bias_tee {
            return Err(LiveControlError::Unsupported("bias tee"));
        }
        self.control
            .set_bias_tee(enabled)
            .map_err(LiveControlError::Source)?;
        t.bias_tee = if enabled {
            hk_model::BiasTee::On
        } else {
            hk_model::BiasTee::Off
        };
        Ok(t.clone())
    }

    fn set_baseband_filter(&self, bandwidth_hz: f64) -> Result<LiveTuning, LiveControlError> {
        let _op = self.gate.enter(DeviceAction::BasebandFilter)?;
        let mut t = self.lock();
        let filters = self
            .capabilities()
            .baseband_filter
            .as_ref()
            .ok_or(LiveControlError::Unsupported("baseband filter"))?;
        if !(bandwidth_hz.is_finite() && filters.supports(bandwidth_hz)) {
            return Err(LiveControlError::OutOfRange {
                what: "baseband filter bandwidth (Hz)".into(),
                value: bandwidth_hz,
            });
        }
        self.control
            .set_baseband_filter(bandwidth_hz)
            .map_err(LiveControlError::Source)?;
        t.baseband_filter_hz = Some(bandwidth_hz);
        Ok(t.clone())
    }
}

#[cfg(test)]
mod tests {
    use hk_core::Gains;

    use super::*;

    const DEVICE_ID: &str = "hackrf:0000000000000000fake0000000000ab";

    struct Recorder {
        caps: SourceCapabilities,
        calls: Mutex<Vec<String>>,
    }

    impl Recorder {
        fn push(&self, s: String) -> Result<(), SourceError> {
            self.calls.lock().unwrap().push(s);
            Ok(())
        }
    }

    impl SourceControl for Recorder {
        fn capabilities(&self) -> &SourceCapabilities {
            &self.caps
        }
        fn device_info(&self) -> Option<hk_core::source::DeviceInfo> {
            Some(hk_core::source::DeviceInfo {
                driver: "hackrf-one".into(),
                device_id: DEVICE_ID.into(),
                hw: "HackRF One (test)".into(),
            })
        }
        fn tune(&self, hz: f64) -> Result<(), SourceError> {
            self.push(format!("tune {hz}"))
        }
        fn set_sample_rate(&self, hz: f64) -> Result<(), SourceError> {
            self.push(format!("rate {hz}"))
        }
        fn set_gains(&self, _: &Gains) -> Result<(), SourceError> {
            unreachable!("the generic path uses set_gain")
        }
        fn set_gain(&self, stage: &str, db: f64) -> Result<(), SourceError> {
            self.push(format!("gain {stage} {db}"))
        }
        fn set_baseband_filter(&self, hz: f64) -> Result<(), SourceError> {
            self.push(format!("filter {hz}"))
        }
        fn set_bias_tee(&self, on: bool) -> Result<(), SourceError> {
            self.push(format!("bias {on}"))
        }
        fn start(&self) -> Result<(), SourceError> {
            Ok(())
        }
        fn stop(&self) -> Result<(), SourceError> {
            Ok(())
        }
    }

    fn live(caps: SourceCapabilities) -> (SourceLiveControl, Arc<Recorder>) {
        let rec = Arc::new(Recorder {
            caps,
            calls: Mutex::new(Vec::new()),
        });
        let initial = LiveTuning {
            center_hz: 100.8e6,
            sample_rate_hz: 2.4e6,
            gains: vec![NamedGain::new("lna", 32.0), NamedGain::new("vga", 30.0)],
            bias_tee: hk_model::BiasTee::Off,
            baseband_filter_hz: None,
        };
        (
            SourceLiveControl::new(Arc::clone(&rec) as Arc<dyn SourceControl>, initial),
            rec,
        )
    }

    #[test]
    fn values_are_validated_against_the_capabilities() {
        let (lc, rec) = live(SourceCapabilities::hackrf_one());
        assert_eq!(lc.set_center(500e3).unwrap_err().http_status(), 400);
        assert_eq!(lc.set_center(f64::NAN).unwrap_err().http_status(), 400);
        assert_eq!(lc.set_rate(40e6).unwrap_err().http_status(), 400);
        let bad = [NamedGain::new("vga", 20.0), NamedGain::new("lna", 48.0)];
        assert_eq!(lc.set_gains(&bad).unwrap_err().http_status(), 400);
        let unknown = [NamedGain::new("mixer", 3.0)];
        assert!(
            lc.set_gains(&unknown)
                .unwrap_err()
                .to_string()
                .contains("mixer")
        );
        assert!(
            rec.calls.lock().unwrap().is_empty(),
            "nothing invalid reached the source"
        );
        let t = lc.set_center(101.1e6).unwrap();
        assert_eq!(t.center_hz, 101.1e6);
        let t = lc
            .set_gains(&[NamedGain::new("lna", 30.0), NamedGain::new("amp", 11.0)])
            .unwrap();
        assert_eq!(
            t.gains,
            vec![
                NamedGain::new("lna", 24.0),
                NamedGain::new("vga", 30.0),
                NamedGain::new("amp", 11.0)
            ],
            "quantised, merged by stage"
        );
        let t = lc.set_bias_tee(true).unwrap();
        assert_eq!(t.bias_tee, hk_model::BiasTee::On);
        assert_eq!(lc.tuning(), t);
        assert_eq!(
            lc.set_baseband_filter(9.5e6).unwrap_err().http_status(),
            400
        );
        let t = lc.set_baseband_filter(7.0e6).unwrap();
        assert_eq!(t.baseband_filter_hz, Some(7.0e6));
        assert_eq!(
            *rec.calls.lock().unwrap(),
            vec![
                "tune 101100000".to_string(),
                "gain lna 24".to_string(),
                "gain amp 11".to_string(),
                "bias true".to_string(),
                "filter 7000000".to_string(),
            ]
        );
    }

    #[test]
    fn a_device_without_a_bias_tee_reports_unsupported() {
        let mut caps = SourceCapabilities::hackrf_one();
        caps.bias_tee = false;
        let (lc, rec) = live(caps);
        assert_eq!(lc.set_bias_tee(true).unwrap_err().http_status(), 501);
        assert!(rec.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn a_device_without_a_baseband_filter_reports_unsupported() {
        let mut caps = SourceCapabilities::hackrf_one();
        caps.baseband_filter = None;
        let (lc, rec) = live(caps);
        let e = lc.set_baseband_filter(7.0e6).unwrap_err();
        assert_eq!((e.http_status(), e.code()), (501, "unsupported"));
        assert!(rec.calls.lock().unwrap().is_empty());
    }

    struct FakeRetuner(Mutex<Vec<(f64, f64)>>);

    impl WindowRetuner for FakeRetuner {
        fn retune(&self, center: f64, rate: f64) -> Result<ContentClass, LiveControlError> {
            if center == 1.0e9 {
                return Err(LiveControlError::Conflict("busy".into()));
            }
            self.0.lock().unwrap().push((center, rate));
            Ok(ContentClass::RestrictedPaging)
        }
    }

    #[test]
    fn a_retuner_receives_window_changes_and_overrides_the_old_policies() {
        let (lc, rec) = live(SourceCapabilities::hackrf_one());
        let retuner = Arc::new(FakeRetuner(Mutex::new(Vec::new())));
        let lc = lc
            .with_window_policy(Arc::new(|_, _| Err("never".into())))
            .with_fixed_rate()
            .with_retuner(Arc::clone(&retuner) as Arc<dyn WindowRetuner>);
        assert_eq!(lc.set_center(930.5e6).unwrap().center_hz, 930.5e6);
        assert_eq!(lc.set_rate(10e6).unwrap().sample_rate_hz, 10e6);
        assert_eq!(lc.set_center(500e3).unwrap_err().http_status(), 400);
        let e = lc.set_center(1.0e9).unwrap_err();
        assert_eq!((e.http_status(), e.code()), (409, "conflict"));
        assert_eq!(
            lc.tuning().center_hz,
            930.5e6,
            "a failed retune changes nothing"
        );
        assert_eq!(
            *retuner.0.lock().unwrap(),
            vec![(930.5e6, 2.4e6), (930.5e6, 10e6)]
        );
        assert!(
            rec.calls.lock().unwrap().is_empty(),
            "window changes go through the pipeline, not straight to the device"
        );
    }

    /// **T-529, the evidence.** A user retune names a centre *and* a span. Committed as two
    /// single-field calls it commands **two** windows, and the first of them — the old centre at
    /// the new rate — is a window no user asked for: a whole segment is captured there, and
    /// `tuning()` (so `/api/control/state`) reports it in between. Committed as one `set_window`
    /// the pipeline is asked for the window that was actually requested, once.
    #[test]
    fn a_window_is_one_command_where_two_single_field_calls_are_two() {
        let split = Arc::new(FakeRetuner(Mutex::new(Vec::new())));
        let (lc, _) = live(SourceCapabilities::hackrf_one());
        let lc = lc.with_retuner(Arc::clone(&split) as Arc<dyn WindowRetuner>);
        // What the UI used to do: post the rate, then the centre.
        lc.set_rate(10e6).unwrap();
        let between = lc.tuning();
        lc.set_center(930.5e6).unwrap();
        assert_eq!(
            *split.0.lock().unwrap(),
            vec![(100.8e6, 10e6), (930.5e6, 10e6)],
            "two posts command two windows; the first is the OLD centre at the NEW rate"
        );
        assert_eq!(
            (between.center_hz, between.sample_rate_hz),
            (100.8e6, 10e6),
            "and it is observable on /api/control/state between the two calls, so the coverage \
             map records a band the user never asked to observe"
        );

        let one = Arc::new(FakeRetuner(Mutex::new(Vec::new())));
        let (lc, _) = live(SourceCapabilities::hackrf_one());
        let lc = lc.with_retuner(Arc::clone(&one) as Arc<dyn WindowRetuner>);
        let t = lc.set_window(930.5e6, 10e6).unwrap();
        assert_eq!((t.center_hz, t.sample_rate_hz), (930.5e6, 10e6));
        assert_eq!(
            *one.0.lock().unwrap(),
            vec![(930.5e6, 10e6)],
            "one window, one command: there is no intermediate window to observe"
        );
    }

    /// A window is validated whole before anything reaches the driver: an unreachable half refuses
    /// the pair rather than applying the other one. (Two posts could not do this — the first would
    /// already have moved the radio.)
    #[test]
    fn an_unachievable_half_refuses_the_whole_window() {
        let retuner = Arc::new(FakeRetuner(Mutex::new(Vec::new())));
        let (lc, rec) = live(SourceCapabilities::hackrf_one());
        let lc = lc.with_retuner(Arc::clone(&retuner) as Arc<dyn WindowRetuner>);
        assert_eq!(lc.set_window(930.5e6, 40e6).unwrap_err().http_status(), 400);
        assert_eq!(lc.set_window(500e3, 10e6).unwrap_err().http_status(), 400);
        assert!(retuner.0.lock().unwrap().is_empty());
        assert!(rec.calls.lock().unwrap().is_empty());
        assert_eq!(
            (lc.tuning().center_hz, lc.tuning().sample_rate_hz),
            (100.8e6, 2.4e6),
            "a refused window leaves the front end exactly where it was"
        );
    }

    /// Without a retuner both driver calls happen under one [`DeviceGate`] hold, in the order
    /// `apply_window` uses (rate, then centre), and a half already in force is not re-sent.
    #[test]
    fn a_window_without_a_retuner_is_one_gated_pair_of_driver_calls() {
        let (lc, rec) = live(SourceCapabilities::hackrf_one());
        lc.set_window(930.5e6, 10e6).unwrap();
        assert_eq!(
            *rec.calls.lock().unwrap(),
            vec!["rate 10000000", "tune 930500000"]
        );
        rec.calls.lock().unwrap().clear();
        lc.set_window(930.5e6, 10e6).unwrap();
        assert!(
            rec.calls.lock().unwrap().is_empty(),
            "re-committing the window in force sends nothing to the driver"
        );
    }

    /// Blocks its first retune until released, then times out; reports a settable window.
    struct SlowRetuner {
        calls: Mutex<Vec<(f64, f64)>>,
        entered: Mutex<std::sync::mpsc::Sender<()>>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
        block: AtomicBool,
        applied: Mutex<Option<AppliedWindow>>,
    }

    impl WindowRetuner for SlowRetuner {
        fn retune(&self, center: f64, rate: f64) -> Result<ContentClass, LiveControlError> {
            self.calls.lock().unwrap().push((center, rate));
            if self.block.swap(false, Ordering::SeqCst) {
                self.entered.lock().unwrap().send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
                return Err(LiveControlError::Timeout("it continues".into()));
            }
            Ok(ContentClass::Unrestricted)
        }
        fn applied_window(&self) -> Option<AppliedWindow> {
            *self.applied.lock().unwrap()
        }
    }

    #[test]
    fn a_timed_out_retune_keeps_the_window_consistent_without_blocking_reads() {
        use std::sync::mpsc::channel;
        use std::time::Duration;

        let (entered_tx, entered_rx) = channel();
        let (release_tx, release_rx) = channel();
        let retuner = Arc::new(SlowRetuner {
            calls: Mutex::new(Vec::new()),
            entered: Mutex::new(entered_tx),
            release: Mutex::new(release_rx),
            block: AtomicBool::new(true),
            applied: Mutex::new(Some(AppliedWindow {
                center_hz: 100.8e6,
                sample_rate_hz: 2.4e6,
                settling: true,
                pipeline_moves: 0,
            })),
        });
        let (lc, _rec) = live(SourceCapabilities::hackrf_one());
        let lc = Arc::new(lc.with_retuner(Arc::clone(&retuner) as Arc<dyn WindowRetuner>));

        let worker = {
            let lc = Arc::clone(&lc);
            std::thread::spawn(move || lc.set_center(930.5e6))
        };
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        // While the retune holds the front end, a read still does not block, and a second device
        // action fails cleanly (T-343) instead of racing the retune to the driver.
        let (tx, rx) = channel();
        {
            let lc = Arc::clone(&lc);
            std::thread::spawn(move || {
                let t = lc.tuning();
                let g = lc.set_gains(&[NamedGain::new("vga", 10.0)]);
                tx.send((t, g)).unwrap();
            });
        }
        let (during, gains) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("tuning() must not wait for the retune, and set_gains() must not park on it");
        assert_eq!(
            during.center_hz, 100.8e6,
            "unchanged until the retune answers"
        );
        let e = gains.expect_err("the front end is held by the retune");
        assert_eq!((e.http_status(), e.code()), (409, "device_busy"));
        assert!(e.to_string().contains(DEVICE_ID), "{e}");

        release_tx.send(()).unwrap();
        let e = worker.join().unwrap().unwrap_err();
        assert_eq!((e.http_status(), e.code()), (504, "timeout"));
        let t = lc.tuning();
        assert_eq!(
            (t.center_hz, t.sample_rate_hz),
            (930.5e6, 2.4e6),
            "the re-plumb continues towards the requested window"
        );

        // It settles elsewhere (e.g. the re-plumb failed): the stored window follows the pipeline.
        *retuner.applied.lock().unwrap() = Some(AppliedWindow {
            center_hz: 100.8e6,
            sample_rate_hz: 2.4e6,
            settling: false,
            pipeline_moves: 0,
        });
        assert_eq!(lc.tuning().center_hz, 100.8e6);
        // The next window change starts from the refreshed window.
        assert_eq!(lc.set_rate(10e6).unwrap().center_hz, 100.8e6);
        assert_eq!(
            *retuner.calls.lock().unwrap(),
            vec![(930.5e6, 2.4e6), (100.8e6, 10e6)]
        );
        // Once settled, later pipeline reports are not re-read (nothing pending).
        *retuner.applied.lock().unwrap() = Some(AppliedWindow {
            center_hz: 1.0,
            sample_rate_hz: 1.0,
            settling: false,
            pipeline_moves: 0,
        });
        assert_eq!(lc.tuning().sample_rate_hz, 10e6);

        // T-508: a window the pipeline moved to BY ITSELF (a capture recovery falling back to the
        // last window that delivered) is adopted with nothing pending — once.
        *retuner.applied.lock().unwrap() = Some(AppliedWindow {
            center_hz: 100.8e6,
            sample_rate_hz: 2.4e6,
            settling: false,
            pipeline_moves: 1,
        });
        let t = lc.tuning();
        assert_eq!((t.center_hz, t.sample_rate_hz), (100.8e6, 2.4e6));
        *retuner.applied.lock().unwrap() = Some(AppliedWindow {
            center_hz: 1.0,
            sample_rate_hz: 1.0,
            settling: false,
            pipeline_moves: 1,
        });
        assert_eq!(
            lc.tuning().center_hz,
            100.8e6,
            "the same move is not re-read"
        );
    }

    /// T-343: the front end is one shared resource, so device actions serialise on one gate and a
    /// contended one fails cleanly, naming the device — it never races the holder to the driver.
    #[test]
    fn device_actions_serialise_on_one_gate_and_name_the_device_when_busy() {
        let gate = DeviceGate::new(Some(DEVICE_ID.to_owned()));
        assert_eq!(gate.device_id(), Some(DEVICE_ID));

        let held = gate.enter(DeviceAction::Retune).expect("free");
        let e = gate
            .enter(DeviceAction::Gains)
            .expect_err("the retune holds the front end");
        assert_eq!((e.http_status(), e.code()), (409, "device_busy"));
        let text = e.to_string();
        assert!(text.contains(DEVICE_ID), "names the device: {text}");
        assert!(text.contains("retune"), "names the holder: {text}");
        assert!(text.contains("gains"), "names what was refused: {text}");

        drop(held);
        let _next = gate
            .enter(DeviceAction::Gains)
            .expect("released on drop, so the next device action gets the front end");
    }

    /// A gate over a source that reports no identity must not invent one (the T-325 rule): the
    /// message says "the front end", never a placeholder id.
    #[test]
    fn an_unidentified_front_end_is_not_given_a_placeholder_id() {
        let gate = DeviceGate::new(None);
        assert_eq!(gate.device_id(), None);
        let _held = gate.enter(DeviceAction::Retune).unwrap();
        let text = gate.enter(DeviceAction::Retune).unwrap_err().to_string();
        assert!(text.starts_with("the front end is busy"), "{text}");
        assert!(
            !text.contains("unknown") && !text.contains("None"),
            "{text}"
        );
    }

    /// T-343: the retune is recorded against the device that answered it, taken from the source
    /// itself rather than from anything the caller supplied.
    #[test]
    fn the_live_control_reports_the_source_s_own_device_id() {
        let (lc, _rec) = live(SourceCapabilities::hackrf_one());
        assert_eq!(LiveControl::device_id(&lc), Some(DEVICE_ID));
    }

    #[test]
    fn policies_refuse_other_windows_and_rate_changes() {
        let (lc, rec) = live(SourceCapabilities::hackrf_one());
        let lc = lc
            .with_window_policy(Arc::new(|center, _rate| {
                if (88e6..108e6).contains(&center) {
                    Ok(())
                } else {
                    Err("another content class".into())
                }
            }))
            .with_fixed_rate();
        let e = lc.set_center(930.5e6).unwrap_err();
        assert_eq!(e.http_status(), 409);
        assert!(e.to_string().contains("content class"));
        let e = lc.set_rate(10e6).unwrap_err();
        assert_eq!(e.http_status(), 409);
        assert!(lc.set_rate(2.4e6).is_ok(), "the current rate is accepted");
        assert!(lc.set_center(99.5e6).is_ok());
        assert_eq!(lc.tuning().center_hz, 99.5e6);
        assert!(
            !rec.calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.contains("930500000"))
        );
    }

    /// A front end that only has to answer "who are you" — enough to key a [`LiveControls`].
    struct Named(Option<String>, SourceCapabilities);

    impl Named {
        fn handle(id: Option<&str>) -> Arc<dyn LiveControl> {
            Arc::new(Self(
                id.map(str::to_owned),
                SourceCapabilities::hackrf_one(),
            ))
        }
    }

    impl LiveControl for Named {
        fn capabilities(&self) -> &SourceCapabilities {
            &self.1
        }
        fn tuning(&self) -> LiveTuning {
            LiveTuning {
                center_hz: 100.8e6,
                sample_rate_hz: 2.4e6,
                gains: Vec::new(),
                bias_tee: hk_model::BiasTee::Unknown,
                baseband_filter_hz: None,
            }
        }
        fn device_id(&self) -> Option<&str> {
            self.0.as_deref()
        }
        fn set_center(&self, _: f64) -> Result<LiveTuning, LiveControlError> {
            unreachable!()
        }
        fn set_rate(&self, _: f64) -> Result<LiveTuning, LiveControlError> {
            unreachable!()
        }
        fn set_window(&self, _: f64, _: f64) -> Result<LiveTuning, LiveControlError> {
            unreachable!()
        }
        fn set_gains(&self, _: &[NamedGain]) -> Result<LiveTuning, LiveControlError> {
            unreachable!()
        }
        fn set_bias_tee(&self, _: bool) -> Result<LiveTuning, LiveControlError> {
            unreachable!()
        }
        fn set_baseband_filter(&self, _: f64) -> Result<LiveTuning, LiveControlError> {
            unreachable!()
        }
    }

    /// T-511: a selector may be omitted only when "the device" is defined — with one front end.
    /// With none it is a replay; with several the caller must name one, because choosing for them
    /// would move a radio nobody asked about.
    #[test]
    fn a_device_selector_is_optional_only_when_there_is_one_device() {
        // A `&Arc<dyn LiveControl>` is not `Debug`, so compare on the identity a selector resolves
        // to — which is the property under test anyway.
        let id = |r: Result<&Arc<dyn LiveControl>, SelectError>| {
            r.map(|c| c.device_id().map(str::to_owned))
        };
        let none = LiveControls::none();
        assert!(none.is_empty() && none.primary().is_none());
        assert_eq!(id(none.select(None)).unwrap_err(), SelectError::NotLive);
        assert_eq!(
            id(none.select(Some("a"))).unwrap_err(),
            SelectError::NotLive
        );
        assert_eq!(id(none.select(None)).unwrap_err().http_status(), 409);

        let one = LiveControls::one(Named::handle(Some("a")));
        assert_eq!(one.len(), 1);
        assert_eq!(one.select(None).unwrap().device_id(), Some("a"));
        assert_eq!(one.select(Some("a")).unwrap().device_id(), Some("a"));
        let e = id(one.select(Some("b"))).unwrap_err();
        assert_eq!(e.http_status(), 404);
        assert_eq!(e.code(), "unknown_device");
        assert!(e.to_string().contains('a'), "{e}");

        let two =
            LiveControls::new(vec![Named::handle(Some("a")), Named::handle(Some("b"))]).unwrap();
        assert_eq!(two.device_ids(), vec!["a".to_owned(), "b".to_owned()]);
        let e = id(two.select(None)).unwrap_err();
        assert_eq!((e.http_status(), e.code()), (400, "device_required"));
        assert!(
            e.to_string().contains('a') && e.to_string().contains('b'),
            "{e}"
        );
        assert_eq!(two.select(Some("b")).unwrap().device_id(), Some("b"));
        // The default exists for the reads whose contract is one device's state; it is never what
        // an omitted selector resolves to.
        assert_eq!(two.primary().unwrap().device_id(), Some("a"));

        // An unnamed front end is today's single-device run: the selector is omitted, and nothing
        // is fabricated to key it by.
        let unnamed = LiveControls::one(Named::handle(None));
        assert_eq!(unnamed.select(None).unwrap().device_id(), None);
        assert!(unnamed.device_ids().is_empty());
        assert_eq!(
            id(unnamed.select(Some("a"))).unwrap_err(),
            SelectError::Unknown {
                requested: "a".to_owned(),
                available: Vec::new()
            }
        );
    }

    /// T-511: a set that could not be addressed unambiguously is refused where it is **composed**,
    /// not discovered at request time as an ambiguous selector.
    #[test]
    fn live_controls_refuse_a_set_a_selector_could_not_address() {
        assert_eq!(
            LiveControls::new(vec![Named::handle(Some("a")), Named::handle(Some("a"))])
                .unwrap_err(),
            LiveControlsError::DuplicateDeviceId("a".to_owned())
        );
        assert_eq!(
            LiveControls::new(vec![Named::handle(None), Named::handle(None)]).unwrap_err(),
            LiveControlsError::UnnamedDevices(2)
        );
        // One unnamed beside a named one is still addressable: the named one by id, and the
        // unnamed one by nothing — which is what "nothing said" means.
        assert!(LiveControls::new(vec![Named::handle(Some("a")), Named::handle(None)]).is_ok());
    }

    /// T-511: each front end carries **its own** gate, so one busy radio never refuses another.
    #[test]
    fn each_device_has_its_own_gate() {
        let (a, _) = live(SourceCapabilities::hackrf_one());
        let (b, _) = live(SourceCapabilities::hackrf_one());
        let held = a.gate().enter(DeviceAction::Retune).expect("free");
        assert_eq!(
            a.gate().enter(DeviceAction::Gains).unwrap_err().code(),
            "device_busy"
        );
        assert!(
            b.gate().enter(DeviceAction::Gains).is_ok(),
            "a second radio is a second resource"
        );
        drop(held);
    }
}
