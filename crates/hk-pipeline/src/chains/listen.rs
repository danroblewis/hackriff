//! On-demand listening (T-043, SIGNAL-062): click a signal → auto analog demodulation → audio
//! stream. [`ListenManager`] is a [`StreamOpener`]: each request attaches one audio chain (a ring
//! reader on its own thread, ADR-0001) to the **running segment** and returns its gated audio
//! stream ([`hk_stream::audio`]).
//!
//! # Request → chain
//! 1. **Target** ([`ListenTarget`]): an inventory emitter, a detection, or a selected extent. No
//!    mode or parameter is accepted.
//! 2. **Legal gate** ([`listen_class`]) on the requested extent under the running segment's
//!    class, **before any ring read**.
//! 3. **Listener cap** ([`ListenConfig::max_listeners`]) and the tuned window.
//! 4. **Probe**: reads `probe_s` from the live edge; C13 estimate + T-012 mode selection
//!    ([`hk_demod::audio::probe`]) choose mode, channel centre and bandwidth. The gate runs again
//!    on the probe box and on the chosen channel; nothing is demodulated before both pass.
//! 5. **Stream**: [`AudioDemod`] (squelch, AGC) → 20 ms `ri16_le` records at 48 kS/s plus status
//!    records, through a [`Publisher`] with a small per-consumer queue (drop-not-block, ≈ 0.6 s).
//!
//! # Legal gating (fail closed)
//! Audio exists only when [`listen_class`] returns a class that permits content:
//! - an extent overlapping a restricted band ([`crate::class::restricted_band`]: paging,
//!   cellular) is refused **whatever the source class** (a recording tagged `unrestricted` does
//!   not open a paging band);
//! - a restricted source class is refused;
//! - otherwise [`classify_emitter`] decides, the rule FSK content already follows: an
//!   `unrestricted` source (a positively chosen band prior such as FM broadcast) permits content;
//!   **unclassified content** (a fail-closed `metadata-only` source with no user classification
//!   rule vouching for the extent) is **refused**; a user rule may open a `metadata-only` band
//!   (never a restricted one).
//!
//! # Segments (T-050)
//! A chain belongs to the segment it was attached to and sees only that segment's class. A
//! re-plumb (a retune or rate change into another class) closes the old segment's ring: the chain
//! drains, ends (`/listen/retune_ends`), finishes its publisher (closing the consumer) and
//! releases the segment's state so the re-plumb can proceed. A new request is gated against the
//! new segment's class. An in-place retune (same class and rate) keeps the chain while the window
//! still covers its channel.
//!
//! # Detach
//! The chain ends when the session guard drops (the consumer disconnected or stopped), when no
//! consumer has been attached for `idle_timeout`, when the window no longer covers the channel,
//! when its segment ends, or when the source ends.
//!
//! # Latency and loss
//! Processing latency (chunk read → record published) is tracked per record (`/listen/
//! latency_us_*`). On a live source a chain more than `max_backlog_s` behind the writer skips to
//! the live edge (counted, next record flagged `DISCONTINUITY`); ring overruns are counted as
//! lost. In lossless replay the chain's gate cursor holds capture instead.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_demod::audio::{AudioConfig, AudioDemod, LISTEN_DEMOD_VERSION, probe};
use hk_demod::refine::RefinementOutcome;
use hk_dsp::InputInfo;
use hk_estimate::SnippetRequest;
use hk_model::{ContentClass, EmitterId, SampleTime, Timestamp};
use hk_stream::audio::{
    AUDIO_DATATYPE, AUDIO_FRAME_SAMPLES, AUDIO_SAMPLE_RATE_HZ, AgcInfo, AudioInfo, AudioStatus,
    ListenRequest, ListenTarget, SquelchInfo, audio_max_frame_len, encode_pcm,
};
use hk_stream::{
    BinaryRecord, OpenRefusal, OpenRequest, OpenedStream, Publisher, PublisherConfig,
    PublisherHandle, RecordFlags, SessionEnd, SessionEndSlot, StreamHeader, StreamKind,
    StreamOpener,
};
use num_complex::Complex;

use super::budget::{self, BudgetLimits, ChainKind, Slot, mcores};
use super::{ChainReader, Next};
use crate::class::{ClassRule, class_name, classify_emitter, is_restricted, restricted_band};
use crate::config::ListenSettings;
use crate::refine::{ListenProbe, LiveRefiner, RefineSettings, SOURCE_LISTEN, listen_probe};
use crate::run::Shared;
use crate::stats::{ChainStatGuard, Counters, ListenCounters, add, inc};

/// Listen settings.
#[derive(Clone, Debug)]
pub struct ListenConfig {
    /// Most audio chains at once; further requests are refused (503 `busy`).
    pub max_listeners: usize,
    /// Share of the CPU cores all audio chains may use together (the admission budget).
    pub cpu_fraction: f64,
    /// Estimated cost of a WFM chain, cores per Msps of tuned sample rate.
    pub wfm_cores_per_msps: f64,
    /// Estimated cost of an NBFM/AM/SSB/CW chain, cores per Msps of tuned sample rate.
    pub narrow_cores_per_msps: f64,
    /// Cores the budget is a fraction of; `None` uses `std::thread::available_parallelism`.
    pub cores: Option<usize>,
    /// Probe length, s.
    pub probe_s: f64,
    /// Longest wait for the probe's samples.
    pub probe_timeout: Duration,
    /// A chain with no consumer for this long detaches.
    pub idle_timeout: Duration,
    /// A chain that published no audio (squelch closed) for this long detaches; `None` never.
    pub squelch_timeout: Option<Duration>,
    /// Live sources: largest backlog behind the writer before skipping to the live edge, s.
    pub max_backlog_s: f64,
    /// Status record period.
    pub status_interval: Duration,
    /// Probe box width limits, Hz (the box is twice the requested extent).
    pub probe_bandwidth_hz: (f64, f64),
    /// Per-consumer queue, bytes (bounds server-side latency; overflow drops, never blocks).
    pub queue_bytes: usize,
    /// Demodulator settings.
    pub audio: AudioConfig,
    /// Output-driven refinement of the channel (T-070, [`crate::refine`]).
    pub refine: RefineSettings,
    /// Most on-demand chains (listeners and burst taps) at once (T-071 budget).
    pub max_chains: usize,
    /// Most burst taps at once (T-071 budget).
    pub max_taps: usize,
    /// Estimated cost of one burst tap, cores.
    pub tap_cores: f64,
}

impl Default for ListenConfig {
    fn default() -> Self {
        let mut c = Self {
            max_listeners: 0,
            cpu_fraction: 0.0,
            wfm_cores_per_msps: 0.0,
            narrow_cores_per_msps: 0.0,
            cores: None,
            probe_s: 0.5,
            probe_timeout: Duration::from_secs(20),
            idle_timeout: Duration::from_secs(10),
            squelch_timeout: None,
            max_backlog_s: 0.25,
            status_interval: Duration::from_millis(250),
            probe_bandwidth_hz: (16e3, 300e3),
            queue_bytes: 64 * 1024,
            audio: AudioConfig::default(),
            refine: RefineSettings::default(),
            max_chains: 0,
            max_taps: 0,
            tap_cores: 0.0,
        };
        c.apply(&ListenSettings::default());
        c
    }
}

impl ListenConfig {
    /// The defaults with `settings` applied.
    pub fn from_settings(settings: &ListenSettings) -> Self {
        let mut c = Self::default();
        c.apply(settings);
        c
    }

    /// Applies the configurable limits of `settings` (T-066).
    pub fn apply(&mut self, s: &ListenSettings) {
        self.max_listeners = s.max_listeners;
        self.cpu_fraction = s.cpu_fraction;
        self.wfm_cores_per_msps = s.wfm_cores_per_msps;
        self.narrow_cores_per_msps = s.narrow_cores_per_msps;
        self.idle_timeout = seconds(s.idle_timeout_s).unwrap_or(Duration::from_secs(10));
        self.squelch_timeout = seconds(s.squelch_timeout_s);
        self.max_chains = s.max_chains;
        self.max_taps = s.max_taps;
        self.tap_cores = s.tap_cores;
    }

    /// The run's on-demand chain budget these settings give (T-071).
    pub fn limits(&self) -> BudgetLimits {
        BudgetLimits {
            max_chains: self.max_chains,
            max_listeners: self.max_listeners,
            max_taps: self.max_taps,
            budget_mcores: self.budget_mcores(),
        }
    }

    /// The admission budget, millicores: `cpu_fraction` of the cores.
    pub fn budget_mcores(&self) -> u64 {
        let cores = self
            .cores
            .unwrap_or_else(|| thread::available_parallelism().map_or(1, usize::from));
        mcores(cores as f64 * self.cpu_fraction)
    }

    /// Estimated cost of one chain on a source at `rate_hz`, millicores. `wfm` is `None` before
    /// the mode is known (the dearer estimate).
    pub fn chain_mcores(&self, rate_hz: f64, wfm: Option<bool>) -> u64 {
        let per_msps = match wfm {
            Some(true) => self.wfm_cores_per_msps,
            Some(false) => self.narrow_cores_per_msps,
            None => self.wfm_cores_per_msps.max(self.narrow_cores_per_msps),
        };
        mcores(rate_hz / 1e6 * per_msps)
    }
}

fn seconds(s: f64) -> Option<Duration> {
    (s.is_finite() && s > 0.0).then(|| Duration::from_secs_f64(s))
}

/// Publishes `config`'s admission limits into the run's counters (`/api/status`).
pub(crate) fn publish_limits(counters: &Counters, config: &ListenConfig) {
    budget::publish(counters, &config.limits());
}

/// The class audio over `[lo, hi]` would carry under `source` and the user's classification
/// `rules`, or the legal refusal (see the module docs). Fails closed.
pub fn listen_class(
    source: ContentClass,
    rules: &[ClassRule],
    lo: f64,
    hi: f64,
) -> Result<ContentClass, OpenRefusal> {
    if !(lo.is_finite() && hi.is_finite() && hi >= lo) {
        return Err(OpenRefusal::gated(
            ContentClass::FAIL_CLOSED,
            "invalid extent (fail closed)",
        ));
    }
    if !hk_model::content_gating_enabled() {
        // Gating off (the default, T-143): nothing is refused; the class is informational.
        return Ok(classify_emitter(rules, source, lo, hi).map_or(source, |(class, _)| class));
    }
    if let Some(b) = restricted_band(lo, hi) {
        return Err(OpenRefusal::gated(
            b.class,
            format!(
                "{} band ({}): audio is never streamed",
                class_name(b.class),
                b.source
            ),
        ));
    }
    if is_restricted(source) {
        return Err(OpenRefusal::gated(
            source,
            format!(
                "source class {}: audio is never streamed",
                class_name(source)
            ),
        ));
    }
    match classify_emitter(rules, source, lo, hi) {
        Some((class, _)) if class.permits_content() => Ok(class),
        Some((class, by)) => Err(OpenRefusal::gated(
            class,
            format!("class {} ({by}): content withheld", class_name(class)),
        )),
        None => Err(OpenRefusal::gated(
            source,
            format!(
                "unclassified: source class {} withholds content and no classification rule \
                 vouches for this extent (fail closed)",
                class_name(source)
            ),
        )),
    }
}

/// The running segment's state (`None` while re-plumbing or after the run).
pub(crate) type SegmentFn = Arc<dyn Fn() -> Option<Arc<Shared>> + Send + Sync>;

/// The run's recipe runtime, built on first use (the opener holds no runtime until the switch
/// below actually sends a request down the recipe path).
pub type RecipesFn = Arc<dyn Fn() -> Arc<crate::recipes::runtime::RecipeRuntime> + Send + Sync>;

/// Environment switch of ADR-0015 §12.9 stage 4: `HK_LISTEN_PIPELINE=1` makes `/ws/open/listen`
/// run the chooser and, when a recipe fits, serve an ephemeral audio pipeline instead of this
/// chain. Default **off**: unset it and the opener is byte-for-byte today's.
pub const PIPELINE_SWITCH_ENV: &str = "HK_LISTEN_PIPELINE";

/// Whether [`PIPELINE_SWITCH_ENV`] is set to something truthy.
fn pipeline_switch_default() -> bool {
    std::env::var(PIPELINE_SWITCH_ENV)
        .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// Opens listen streams on a running pipeline (`PipelineHandle::listen_service`).
pub struct ListenManager {
    counters: Arc<Counters>,
    segment: SegmentFn,
    /// The run's listen limits (changeable at runtime), read at each request.
    settings: Arc<std::sync::Mutex<ListenSettings>>,
    /// Pinned by [`Self::with_config`] instead of following `settings`.
    config: Option<ListenConfig>,
    /// The recipe runtime, for the chooser's recipe path (T-869). `None` wires the opener to
    /// this chain only, whatever the switch says.
    recipes: Option<RecipesFn>,
    /// The stage-4 switch in force for this opener ([`PIPELINE_SWITCH_ENV`]).
    pipeline_audio: AtomicBool,
}

static STREAM_SEQ: AtomicU64 = AtomicU64::new(1);

/// The session guard: dropping it (the client went away or stopped) stops the chain and frees
/// its slot at once.
struct StopOnDrop {
    stop: Arc<AtomicBool>,
    slot: Slot,
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.slot.release();
    }
}

/// Why a chain ended (`/listen/closed_*`).
///
/// **T-633.** The session guard is a bare `drop`, so every end that arrived through it used to be
/// counted `closed_client` — "the client went away" — including a peer the *server* reaped for
/// silence, a transport fault, and a drop nobody attributed. `closed_client` is the counter an
/// operator reads to blame their own client, so those now have their own answers, and a chain
/// that ended for a **source** reason is counted under that reason whoever dropped the guard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    /// The client affirmatively went away: a close frame, a FIN, a data message, or Stop.
    Client,
    /// The server stopped hearing the peer and reaped it.
    Unresponsive,
    /// A reset, or a read/write error on the connection. The connection was torn down; whether
    /// the client went away is not something this says.
    Transport,
    /// The guard was dropped and the transport reported no reason.
    Unattributed,
    Idle,
    Squelch,
    Retune,
    Segment,
    Source,
    Error,
}

impl End {
    /// The end of a session whose guard was dropped, by the same rule [`Session::session_end`]
    /// uses: a **source** reason outranks the session, whichever thread got there first.
    fn of_session(shared: Option<&Shared>, end: SessionEnd) -> Self {
        if let Some(shared) = shared
            && (shared.ring.is_closed() || shared.stop.load(Ordering::SeqCst))
        {
            return if shared.continues.load(Ordering::SeqCst) {
                End::Segment
            } else {
                End::Source
            };
        }
        match end {
            SessionEnd::Client => End::Client,
            SessionEnd::Unresponsive => End::Unresponsive,
            SessionEnd::Transport => End::Transport,
            SessionEnd::Unattributed => End::Unattributed,
        }
    }

    fn count(self, lc: &ListenCounters) {
        inc(match self {
            End::Client => &lc.closed_client,
            End::Unresponsive => &lc.closed_unresponsive,
            End::Transport => &lc.closed_transport,
            End::Unattributed => &lc.closed_unattributed,
            End::Idle => &lc.closed_idle,
            End::Squelch => &lc.closed_squelch,
            End::Retune => &lc.closed_retune,
            End::Segment => &lc.closed_segment,
            End::Source => &lc.closed_source,
            End::Error => &lc.closed_error,
        });
    }
}

/// Counts how one listening session ended (T-633), for a session served by an **audio pipeline**
/// rather than by this chain (ADR-0015 §12.3): the pipeline's own thread has no session guard, so
/// the attachment counts the end here under exactly the legacy chain's rule.
///
/// `pipeline_end` is the pipeline's own end reason once it has one. It is what decides a
/// **source** end, because the segment's state is already gone by then: a re-plumb drops the old
/// `Shared`, so a chain that ended with it would otherwise be counted as "the client went away"
/// — the very mis-attribution T-633 fixed on the legacy path.
pub(crate) fn count_session_end(
    counters: &Counters,
    shared: Option<&Shared>,
    pipeline_end: Option<&str>,
    end: SessionEnd,
) {
    let lc = &counters.listen;
    let by_pipeline = pipeline_end.and_then(|r| match r {
        "segment-ended" => {
            inc(&lc.retune_ends);
            Some(End::Segment)
        }
        "source-ended" => Some(End::Source),
        r if r.starts_with("retune") || r.starts_with("rate-change") => {
            inc(&lc.retune_ends);
            Some(End::Retune)
        }
        // `stopped` is this session's own detach (or an operator's): the transport says why.
        _ => None,
    });
    by_pipeline
        .unwrap_or_else(|| End::of_session(shared, end))
        .count(lc);
}

/// The refusal for a request whose segment is ending: `503 replumbing` while the run continues
/// in a new window (retry), `410 source-ended` once the run is over.
fn segment_ended(continues: bool) -> OpenRefusal {
    if continues {
        OpenRefusal::new(
            503,
            "replumbing",
            "the run is moving to a new window; try again",
        )
    } else {
        OpenRefusal::new(410, "source-ended", "the source has ended")
    }
}

fn in_window(center: f64, rate: f64, lo: f64, hi: f64) -> bool {
    rate.is_finite() && rate > 0.0 && lo >= center - 0.49 * rate && hi <= center + 0.49 * rate
}

fn gate(shared: &Shared, lo: f64, hi: f64) -> Result<ContentClass, OpenRefusal> {
    listen_class(
        shared.cfg.source_class,
        &shared.cfg.settings.classify,
        lo,
        hi,
    )
}

/// `(lo, hi, emitter)` of a target.
fn resolve(
    shared: &Shared,
    target: ListenTarget,
) -> Result<(f64, f64, Option<EmitterId>), OpenRefusal> {
    let not_found = |what: &str| OpenRefusal::new(404, "not-found", format!("no such {what}"));
    match target {
        ListenTarget::Range { f_lo_hz, f_hi_hz } => Ok((f_lo_hz, f_hi_hz, None)),
        ListenTarget::Emitter(id) => {
            let e = shared
                .repo()
                .emitter(id)
                .map_err(|_| not_found("emitter"))?;
            let h = 0.5 * e.bandwidth_hz.max(0.0);
            Ok((e.f_center_hz - h, e.f_center_hz + h, Some(id)))
        }
        ListenTarget::Detection(id) => {
            let d = shared
                .repo()
                .detection(id)
                .map_err(|_| not_found("detection"))?;
            let h = 0.5 * d.obw_hz.max(0.0);
            Ok((d.f_center_hz - h, d.f_center_hz + h, None))
        }
    }
}

impl ListenManager {
    pub(crate) fn new(
        counters: Arc<Counters>,
        segment: SegmentFn,
        settings: Arc<std::sync::Mutex<ListenSettings>>,
    ) -> Self {
        let m = Self {
            counters,
            segment,
            settings,
            config: None,
            recipes: None,
            pipeline_audio: AtomicBool::new(pipeline_switch_default()),
        };
        publish_limits(&m.counters, &m.config());
        m
    }

    /// Pins the settings instead of following the run's listen settings.
    #[must_use]
    pub fn with_config(mut self, config: ListenConfig) -> Self {
        publish_limits(&self.counters, &config);
        self.config = Some(config);
        self
    }

    /// Wires the chooser's recipe path (ADR-0015 §12.9 stage 4) to the run's recipe runtime.
    /// The runtime is built on first use, so an opener that never takes the recipe path never
    /// creates one.
    #[must_use]
    pub fn with_recipes(mut self, recipes: RecipesFn) -> Self {
        self.recipes = Some(recipes);
        self
    }

    /// Turns the stage-4 switch on or off for this opener, whatever [`PIPELINE_SWITCH_ENV`]
    /// says. A test (and an operator changing the flag without a restart) needs this: the
    /// environment is process-wide, and the decision is per opener.
    pub fn set_pipeline_audio(&self, on: bool) {
        self.pipeline_audio.store(on, Ordering::SeqCst);
    }

    /// Whether this opener runs the chooser's recipe path.
    pub fn pipeline_audio(&self) -> bool {
        self.recipes.is_some() && self.pipeline_audio.load(Ordering::SeqCst)
    }

    /// The run's recipe runtime. Only called once [`Self::pipeline_audio`] is true, so a run
    /// whose opener never takes the recipe path never builds one.
    fn runtime(&self) -> Arc<crate::recipes::runtime::RecipeRuntime> {
        (self.recipes.as_ref().expect("the recipe path is wired"))()
    }

    /// The settings a request is admitted under now.
    pub fn config(&self) -> ListenConfig {
        match &self.config {
            Some(c) => c.clone(),
            None => ListenConfig::from_settings(
                &self
                    .settings
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            ),
        }
    }

    fn open_inner(&self, req: &OpenRequest) -> Result<OpenedStream, OpenRefusal> {
        let cfg = &self.config();
        publish_limits(&self.counters, cfg);
        // T-874: `channels=2` asks for stereo (opt-in); absent, the stream is today's mono.
        let ListenRequest {
            target,
            channels: want_channels,
        } = ListenRequest::from_request(req)?;
        let shared = (self.segment)().ok_or_else(|| {
            OpenRefusal::new(
                503,
                "replumbing",
                "the run is changing window or has ended; try again",
            )
        })?;
        let shared = &shared;
        if shared.ring.is_closed() || shared.stop.load(Ordering::SeqCst) {
            return Err(segment_ended(shared.continues.load(Ordering::SeqCst)));
        }
        let (lo, hi, emitter) = resolve(shared, target)?;
        // Legal gate first: nothing reads the ring for a refused extent.
        gate(shared, lo, hi)?;
        // Admission (T-066): the cap and the CPU budget, costed at the dearer mode until the
        // probe has chosen one.
        let rate_now = Some(shared.counters.tune().1)
            .filter(|r| r.is_finite() && *r > 0.0)
            .unwrap_or(shared.fs);
        let slot = Slot::claim(
            &self.counters,
            &cfg.limits(),
            ChainKind::Listen,
            cfg.chain_mcores(rate_now, None),
            rate_now,
        )?;
        // T-071: the chain's own counters, from its probe on.
        let stat = self.counters.chain_stats.register("listen");
        let fc = 0.5 * (lo + hi);
        let probe_bw = (2.0 * (hi - lo)).clamp(cfg.probe_bandwidth_hz.0, cfg.probe_bandwidth_hz.1);
        let (plo, phi) = (fc - 0.5 * probe_bw, fc + 0.5 * probe_bw);
        gate(shared, plo, phi)?;
        let (center, rate) = shared.counters.tune();
        if !in_window(center, rate, plo, phi) {
            return Err(OpenRefusal::new(
                409,
                "outside-window",
                "the selection is not inside the tuned window",
            ));
        }

        // Probe from the live edge.
        inc(&shared.counters.listen.probes);
        let fs = shared.fs;
        let start = shared.ring.next_sample().unwrap_or(0);
        let cursor = shared.gate.register(start);
        let mut reader = ChainReader::new(Arc::clone(shared), start, cursor).with_stat(stat.stat());
        let probe_len = ((cfg.probe_s * fs) as usize).max(1);
        // T-070: refinement measures on a longer window than the mode probe.
        let want = if cfg.refine.enabled {
            probe_len.max((cfg.refine.window_s * fs) as usize)
        } else {
            probe_len
        };
        let mut iq: Vec<Complex<i8>> = Vec::with_capacity(want);
        let mut head: Option<(SampleTime, ProvenanceHandle)> = None;
        let deadline = Instant::now() + cfg.probe_timeout;
        while iq.len() < want {
            if Instant::now() > deadline {
                return Err(OpenRefusal::new(
                    504,
                    "probe-timeout",
                    "no samples for the probe",
                ));
            }
            match reader.next() {
                Next::Data(c) => {
                    let contiguous = head.as_ref().is_some_and(|(t, p)| {
                        c.first_sample() == t.sample_index + iq.len() as u64
                            && c.provenance.id() == p.id()
                    });
                    if !contiguous {
                        iq.clear();
                        head = Some((c.time, c.provenance.clone()));
                    }
                    let take = (want - iq.len()).min(c.len);
                    iq.extend_from_slice(&reader.buf[..take]);
                    reader.release_to(c.end_sample());
                }
                Next::Lost => iq.clear(),
                Next::Idle => {}
                Next::Closed => {
                    return Err(segment_ended(shared.continues.load(Ordering::SeqCst)));
                }
            }
        }
        let (time, prov) = head.expect("probe head");
        let tune = prov.tune.clone();
        if !in_window(tune.center_hz, tune.sample_rate_hz, plo, phi) {
            return Err(OpenRefusal::new(
                409,
                "outside-window",
                "the window moved during the probe",
            ));
        }
        let info = InputInfo {
            time,
            discontinuity: Discontinuity::NONE,
            dropped_before: 0,
            provenance: &prov,
        };
        let probe_iq = &iq[..probe_len.min(iq.len())];
        let request = SnippetRequest {
            start_index: time.sample_index,
            end_index: time.sample_index + probe_iq.len() as u64,
            center_offset_hz: fc - tune.center_hz,
            bandwidth_hz: probe_bw,
        };
        let pr = probe(info, probe_iq, &request)
            .map_err(|e| OpenRefusal::new(422, "probe-failed", format!("estimation: {e}")))?;
        // T-070: refine the channel from the demodulated output ([`crate::refine`]).
        let ListenProbe {
            probe: pr,
            plan,
            refined,
        } = listen_probe(
            &cfg.refine,
            &cfg.audio,
            info,
            &iq,
            &request,
            (lo, hi),
            cfg.probe_bandwidth_hz,
            pr,
        );
        // **The chooser** (T-869, ADR-0015 §12.2): probe → mode → *which* pipeline. It is the one
        // place a mode is decided, so it decides for both paths: which recipe (if any) fits what
        // was measured, whether today's chain serves it, and — the weak-carrier rule — whether a
        // narrow selection with measured energy is a channel at all.
        let take_recipes = self.pipeline_audio();
        let recipes = take_recipes.then(|| self.runtime().audio_entries());
        let probe_params = plan
            .as_ref()
            .ok()
            .map(|p| estimated_params(&pr, p, refined.as_ref(), fc))
            .unwrap_or_default();
        let choice = crate::audio::choose(&crate::audio::Ask {
            selection: (lo, hi),
            channels: want_channels,
            probe: &pr,
            plan,
            params: &probe_params,
            recipes: recipes.as_deref().unwrap_or(&[]),
            audio: &cfg.audio,
        });
        if crate::debug_enabled() {
            eprintln!("hk-pipeline: listen chooser: {choice:?}");
        }
        let plan = match &choice {
            crate::audio::Choice::Refuse { why } => {
                return Err(OpenRefusal::new(
                    422,
                    "no-analog-mode",
                    format!("no analog modulation recognised: {why}"),
                ));
            }
            chosen => chosen
                .plan()
                .expect("a chosen answer carries its plan")
                .clone(),
        };
        let (clo, chi) = plan.channel_extent_hz();
        // A refined emission only has to overlap the selection.
        let reach = match &refined {
            Some(_) => (0.5 * (hi - lo) + 0.5 * plan.channel_bandwidth_hz).max(0.5 * probe_bw),
            None => 0.5 * probe_bw,
        };
        if (plan.channel_center_hz - fc).abs() > reach {
            return Err(OpenRefusal::new(
                422,
                "no-analog-mode",
                "the strongest emission lies outside the selection",
            ));
        }
        // The chosen channel is what is demodulated: gate it too.
        let class = gate(shared, clo.min(lo), chi.max(hi))?;
        if !in_window(tune.center_hz, tune.sample_rate_hz, clo, chi) {
            return Err(OpenRefusal::new(
                409,
                "outside-window",
                "the demodulated channel is not inside the tuned window",
            ));
        }
        let params = estimated_params(&pr, &plan, refined.as_ref(), fc);
        // T-070: the refined tuning goes on the target emitter (or the inventory emitter at the
        // refined channel), whichever path serves the audio — it is the probe's measurement, not
        // the chain's.
        let refine_emitter = refined.as_ref().and_then(|o| {
            crate::refine::store_and_explain(shared, emitter, o, SOURCE_LISTEN, time.host_time)
        });

        // Stage 4 (ADR-0015 §12.9): the chosen recipe runs as an ephemeral audio pipeline, or
        // attaches to the one already serving this target ([`crate::recipes::session`]). A
        // pipeline that cannot be started is not a refusal: the legacy chain below serves it.
        if let crate::audio::Choice::Recipe {
            id, version, seed, ..
        } = &choice
        {
            let measured = crate::recipes::audio::Measured {
                mode: plan.mode_name().into(),
                mode_confidence: pr.mode.confidence,
                mode_rules: pr.mode.rules_version.clone(),
                params: params.clone(),
                snr_db: pr.params.snr_box_db.value(),
                noise_dbfs: plan.noise_power.map(|n| 10.0 * n.log10()),
                refinement: refined.as_ref().map(crate::refine::audio_refinement),
                provenance_ref: Some(prov.id()),
            };
            match self
                .runtime()
                .open_listen_audio((id, *version), emitter, seed, measured)
            {
                Ok(opened) => {
                    inc(&shared.counters.listen.attached);
                    inc(&shared.counters.listen.open);
                    return Ok(OpenedStream {
                        session: Box::new(PipelineSession {
                            _attached: opened.session,
                            slot: slot.clone(),
                        }),
                        ..opened
                    });
                }
                Err(crate::recipes::session::NoPipeline::Refuse(r)) => return Err(*r),
                Err(crate::recipes::session::NoPipeline::Legacy(why)) => {
                    if crate::debug_enabled() {
                        eprintln!("hk-pipeline: listen falls back to the legacy chain: {why}");
                    }
                }
            }
        }

        let demod = build_demod(
            plan.clone(),
            &cfg.audio,
            tune.sample_rate_hz,
            tune.center_hz,
            want_channels,
        )
        .map_err(|e| OpenRefusal::new(500, "demod", e.to_string()))?;
        // What the stream carries is what the demodulator delivers, not what was asked: only
        // broadcast FM has a second channel (T-874).
        let channels = demod.channels();

        slot.set_mcores(cfg.chain_mcores(
            tune.sample_rate_hz,
            Some(plan.mode == hk_demod::AnalogMode::Wfm),
        ));
        let mut header = StreamHeader::new(
            format!("listen/{}", STREAM_SEQ.fetch_add(1, Ordering::Relaxed)),
            StreamKind::Audio,
            class,
            "hk-pipeline:listen",
        );
        header.datatype = Some(AUDIO_DATATYPE.into());
        header.sample_rate_hz = Some(AUDIO_SAMPLE_RATE_HZ);
        header.center_hz = Some(plan.channel_center_hz);
        header.bandwidth_hz = Some(plan.channel_bandwidth_hz);
        header.emitter_id = emitter;
        header.provenance_ref = Some(prov.id());
        header.max_frame_len = audio_max_frame_len(channels);
        header.audio = Some(AudioInfo {
            channels,
            frame_samples: AUDIO_FRAME_SAMPLES as u32,
            mode: plan.mode_name().into(),
            mode_confidence: pr.mode.confidence,
            mode_rules: pr.mode.rules_version.clone(),
            params,
            snr_db: pr.params.snr_box_db.value(),
            squelch: SquelchInfo {
                open_snr_db: cfg.audio.squelch_open_snr_db,
                hysteresis_db: cfg.audio.squelch_hysteresis_db,
                noise_dbfs: plan.noise_power.map(|n| 10.0 * n.log10()),
            },
            agc: AgcInfo {
                enabled: plan.agc,
                target_dbfs: cfg.audio.agc_target_dbfs,
                max_gain_db: cfg.audio.agc_max_gain_db,
            },
            deemphasis_s: plan.deemphasis_s,
            demod: LISTEN_DEMOD_VERSION.into(),
            refinement: refined.as_ref().map(crate::refine::audio_refinement),
            ..AudioInfo::default()
        });
        let publisher = Publisher::new(
            header.clone(),
            PublisherConfig {
                queue_bytes: cfg.queue_bytes,
                disconnect_after_drops: u64::MAX,
                disconnect_after: Duration::from_secs(5),
                max_consumers: 1,
                drain_timeout: Duration::from_millis(500),
            },
        )
        .map_err(|e| OpenRefusal::new(500, "publisher", e.to_string()))?;
        let handle = publisher.handle();
        stat.set_channel(plan.channel_center_hz, plan.channel_bandwidth_hz);
        stat.set_stream(&header.stream_id, handle.clone());
        let stop = Arc::new(AtomicBool::new(false));
        // T-633: the transport records how the session ended here before dropping the guard.
        let end_slot = SessionEndSlot::default();
        // The refined tuning keeps being refined in the background while streaming (T-070).
        let refined_tuning = refined
            .as_ref()
            .map(|o| (o.tuning.center_hz, o.tuning.bandwidth_hz));
        let refiner = refined.and_then(|o| LiveRefiner::new(&cfg.refine, plan.mode, o, fs));
        let session = Session {
            shared: Arc::clone(shared),
            reader,
            demod,
            channels: want_channels,
            stereo_losses: 0,
            publisher,
            handle: handle.clone(),
            stop: Arc::clone(&stop),
            config: cfg.clone(),
            tune: (tune.center_hz, tune.sample_rate_hz),
            t0: time.host_time,
            t0_anchored: false,
            refiner,
            refine_emitter: refine_emitter.or(emitter),
            refined: refined_tuning,
            end_slot: end_slot.clone(),
            _slot: slot.clone(),
            stat,
        };
        inc(&shared.counters.listen.running);
        if let Err(e) = thread::Builder::new()
            .name("hk-listen".into())
            .spawn(move || session.run())
        {
            shared
                .counters
                .listen
                .running
                .fetch_sub(1, Ordering::SeqCst);
            return Err(OpenRefusal::new(500, "spawn", e.to_string()));
        }
        inc(&shared.counters.listen.attached);
        inc(&shared.counters.listen.open);
        Ok(OpenedStream {
            header,
            handle,
            session: Box::new(StopOnDrop { stop, slot }),
            end: end_slot,
        })
    }
}

impl StreamOpener for ListenManager {
    fn open(&self, request: &OpenRequest) -> Result<OpenedStream, OpenRefusal> {
        let c = &self.counters.listen;
        inc(&c.requests);
        let result = self.open_inner(request);
        if let Err(e) = &result {
            match (e.status, e.code.as_str()) {
                (403, _) => inc(&c.refused_class),
                (503, "busy") => inc(&c.refused_busy),
                _ => inc(&c.refused_other),
            }
            if crate::debug_enabled() {
                eprintln!("hk-pipeline: listen refused: {e}");
            }
        }
        result
    }

    fn describe(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": "audio",
            "datatype": AUDIO_DATATYPE,
            "sample_rate_hz": AUDIO_SAMPLE_RATE_HZ,
            "params": ["emitter", "detection", "f_lo", "f_hi", "channels"],
            "records": format!(
                "data (type 1, {AUDIO_FRAME_SAMPLES} i16 LE samples per channel; mono unless \
                 channels=2 was asked and the header's audio.channels is 2, then interleaved L, R) \
                 and status (type 3: level_dbfs, snr_db, squelch_open, agc_gain_db, ..., stereo \
                 on two-channel streams); mode and parameters are estimated (header audio profile)"
            ),
        })
    }
}

/// The parameters the header reports: what the probe estimated, plus what refinement measured.
fn estimated_params(
    pr: &hk_demod::audio::ProbeResult,
    plan: &hk_demod::audio::AudioPlan,
    refined: Option<&RefinementOutcome>,
    fc: f64,
) -> hk_model::EstimatedParams {
    let mut params = pr.params.estimated_params();
    if plan.mode == hk_demod::AnalogMode::Wfm {
        params.pilot_hz = pr
            .mode
            .features
            .pilot
            .filter(|p| p.found)
            .and_then(|p| p.frequency_hz);
    }
    if let Some(o) = refined {
        params.bandwidth_hz = Some(o.tuning.bandwidth_hz);
        params.cfo_hz = Some(o.tuning.center_hz - fc);
        if let Some(p) = o.mode_params.get("pilot_hz") {
            params.pilot_hz = Some(*p);
        }
    }
    params
}

/// The session guard of a listener served by an **audio pipeline** (T-869): the attachment —
/// dropping it detaches, and the last listener to leave stops a session-owned pipeline — and
/// this request's admission slot, so an audio pipeline counts against the listener budget for
/// exactly as long as somebody is listening (§12.4: audio pipelines count as listeners).
struct PipelineSession {
    _attached: Box<dyn std::any::Any + Send>,
    slot: Slot,
}

impl Drop for PipelineSession {
    fn drop(&mut self) {
        self.slot.release();
    }
}

/// The demodulator for `plan`, with the L−R path when `channels` is 2 (T-874; a no-op on modes
/// with no second channel).
fn build_demod(
    plan: hk_demod::audio::AudioPlan,
    cfg: &AudioConfig,
    rate_hz: f64,
    center_hz: f64,
    channels: u32,
) -> Result<AudioDemod, hk_demod::DemodError> {
    let d = AudioDemod::new(plan, cfg.clone(), rate_hz, center_hz)?;
    Ok(if channels == 2 { d.with_stereo() } else { d })
}

struct Session {
    shared: Arc<Shared>,
    reader: ChainReader,
    demod: AudioDemod,
    /// Channels the client asked for (T-874): every rebuilt demodulator gets the same.
    channels: u32,
    /// Pilot lock losses of demodulators already replaced (a rebuild that drops a locked pilot is
    /// one), so the status count covers the whole stream.
    stereo_losses: u64,
    publisher: Publisher,
    handle: PublisherHandle,
    stop: Arc<AtomicBool>,
    config: ListenConfig,
    /// Tuned centre and rate the demodulator was built for.
    tune: (f64, f64),
    /// Audio time origin: the capture time of the first sample the demodulator processes. Until
    /// that chunk arrives it holds the probe head's time; `run` re-anchors it on the first chunk
    /// (T-868: anchoring on the probe head stamped every record one probe window — ~1 s — early,
    /// because audio starts after the probe and refinement window, not at its head).
    t0: Timestamp,
    /// `t0` has been anchored on the first processed chunk.
    t0_anchored: bool,
    /// Background re-refinement (T-070).
    refiner: Option<LiveRefiner>,
    /// Emitter refined tunings are stored on.
    refine_emitter: Option<EmitterId>,
    /// Refined centre and bandwidth in force.
    refined: Option<(f64, f64)>,
    /// How the transport said the session ended (T-633), read when `stop` is seen.
    end_slot: SessionEndSlot,
    _slot: Slot,
    /// The chain's own counters (T-071).
    stat: ChainStatGuard,
}

impl Session {
    /// Applies an accepted live refinement (T-070): the demodulator is rebuilt on the refined
    /// channel when it stays inside the tuned window and passes the gate, and the tuning is
    /// stored on the emitter.
    fn retune_refined(&mut self, next: &RefinementOutcome, t: Timestamp, gap: &mut bool) {
        let mut plan = self.demod.plan().clone();
        crate::refine::apply_to_plan(&mut plan, next);
        let (lo, hi) = plan.channel_extent_hz();
        if !in_window(self.tune.0, self.tune.1, lo, hi) || gate(&self.shared, lo, hi).is_err() {
            return;
        }
        let Ok(demod) = build_demod(
            plan,
            &self.config.audio,
            self.tune.1,
            self.tune.0,
            self.channels,
        ) else {
            return;
        };
        self.replace_demod(demod);
        self.refined = Some((next.tuning.center_hz, next.tuning.bandwidth_hz));
        *gap = true;
        if self.refine_emitter.is_some() {
            crate::refine::store_and_explain(
                &self.shared,
                self.refine_emitter,
                next,
                SOURCE_LISTEN,
                t,
            );
        }
    }

    /// Swaps in a rebuilt demodulator, carrying its stereo lock losses over (T-874): the new one
    /// starts unlocked, so dropping a locked pilot is itself a loss.
    fn replace_demod(&mut self, demod: AudioDemod) {
        self.stereo_losses +=
            self.demod.stereo_lock_losses() + u64::from(self.demod.stereo_locked());
        self.demod = demod;
    }

    /// Why this chain stopped when its session guard was dropped (T-633).
    ///
    /// A **source** reason outranks the session: a chain whose segment has already closed did not
    /// end because the client went away, whichever thread got there first — and the guard is
    /// dropped by the very socket shutdown a finished producer causes, so the race is real, not
    /// theoretical. Otherwise the end is the one the transport reported, and an end the transport
    /// did not attribute stays unattributed rather than resolving to the convenient label.
    fn session_end(&self) -> End {
        if self.shared.ring.is_closed() || self.shared.stop.load(Ordering::SeqCst) {
            return if self.shared.continues.load(Ordering::SeqCst) {
                End::Segment
            } else {
                End::Source
            };
        }
        match self.end_slot.get() {
            SessionEnd::Client => End::Client,
            SessionEnd::Unresponsive => End::Unresponsive,
            SessionEnd::Transport => End::Transport,
            SessionEnd::Unattributed => End::Unattributed,
        }
    }

    fn run(mut self) {
        let counters = Arc::clone(&self.shared.counters);
        let lc = &counters.listen;
        let fs = self.shared.fs;
        // A record is AUDIO_FRAME_SAMPLES sample frames of `channels` interleaved samples each;
        // `sample_index` counts frames (time), whatever the channel count (T-874).
        let channels = self.demod.channels() as usize;
        let frame = AUDIO_FRAME_SAMPLES * channels;
        let mut pending: Vec<f32> = Vec::with_capacity(4 * frame);
        let mut payload: Vec<u8> = Vec::with_capacity(2 * frame);
        let mut audio_index: u64 = 0;
        let mut gap = false;
        let mut lost: u64 = 0;
        let mut frames: u64 = 0;
        let mut squelched: u64 = 0;
        let mut latency_ms = 0.0;
        let mut backlog_s = 0.0;
        let mut last_consumer = Instant::now();
        let mut last_status = Instant::now()
            .checked_sub(self.config.status_interval)
            .unwrap_or_else(Instant::now);
        let lost_before = counters.chains.lost_samples.load(Ordering::Relaxed);
        let mut last_audio = Instant::now();
        // Readers re-created on this thread (skips to the live edge) account to the same stat.
        super::set_thread_stat(Some(self.stat.stat()));
        let end = loop {
            if self.stop.load(Ordering::SeqCst) {
                break self.session_end();
            }
            if self.handle.open_consumers() > 0 {
                last_consumer = Instant::now();
            } else if last_consumer.elapsed() > self.config.idle_timeout {
                inc(&lc.idle_ends);
                break End::Idle;
            }
            if self
                .config
                .squelch_timeout
                .is_some_and(|t| last_audio.elapsed() > t)
            {
                break End::Squelch;
            }
            match self.reader.next() {
                Next::Data(chunk) => {
                    let read_at = Instant::now();
                    let tune = (
                        chunk.provenance.tune.center_hz,
                        chunk.provenance.tune.sample_rate_hz,
                    );
                    if tune != self.tune {
                        let (lo, hi) = self.demod.plan().channel_extent_hz();
                        if !in_window(tune.0, tune.1, lo, hi) {
                            inc(&lc.retune_ends);
                            break End::Retune;
                        }
                        match build_demod(
                            self.demod.plan().clone(),
                            &self.config.audio,
                            tune.1,
                            tune.0,
                            self.channels,
                        ) {
                            Ok(d) => self.replace_demod(d),
                            Err(_) => {
                                inc(&lc.errors);
                                break End::Error;
                            }
                        }
                        self.tune = tune;
                        gap = true;
                    }
                    if !self.shared.gate.enabled() {
                        let head = self.shared.ring.next_sample().unwrap_or(0);
                        let behind = head.saturating_sub(chunk.end_sample());
                        backlog_s = behind as f64 / fs;
                        if backlog_s > self.config.max_backlog_s {
                            add(&lc.skipped_samples, behind);
                            add(&self.stat.lost_samples, behind);
                            lost += behind;
                            let cursor = self.shared.gate.register(head);
                            self.reader = ChainReader::new(Arc::clone(&self.shared), head, cursor);
                            gap = true;
                            continue;
                        }
                    }
                    let info = InputInfo {
                        time: chunk.time,
                        discontinuity: if chunk.block_start {
                            chunk.discontinuity
                        } else {
                            Discontinuity::NONE
                        },
                        dropped_before: if chunk.block_start {
                            chunk.dropped_before
                        } else {
                            0
                        },
                        provenance: &chunk.provenance,
                    };
                    if !self.t0_anchored {
                        self.t0 = chunk.time.host_time;
                        self.t0_anchored = true;
                    }
                    let processed = self.demod.process(info, &self.reader.buf[..chunk.len]);
                    if let Some(r) = self.refiner.as_mut() {
                        r.feed(chunk.time, &chunk.provenance, &self.reader.buf[..chunk.len]);
                    }
                    self.reader.release_to(chunk.end_sample());
                    if processed.is_err() {
                        inc(&lc.errors);
                        break End::Error;
                    }
                    if let Some(next) = self.refiner.as_mut().and_then(LiveRefiner::poll) {
                        self.retune_refined(&next, chunk.time.host_time, &mut gap);
                    }
                    self.demod.drain_audio_into(&mut pending);
                    let mut offset = 0;
                    let mut failed = false;
                    while pending.len() - offset >= frame {
                        let samples = &pending[offset..offset + frame];
                        offset += frame;
                        let index = audio_index;
                        audio_index += AUDIO_FRAME_SAMPLES as u64;
                        if !self.demod.squelch_open() {
                            squelched += 1;
                            inc(&lc.squelched_frames);
                            gap = true;
                            continue;
                        }
                        payload.clear();
                        encode_pcm(samples, &mut payload);
                        let t = self.t0.saturating_add_nanos(
                            (index as f64 * 1e9 / AUDIO_SAMPLE_RATE_HZ).round() as i64,
                        );
                        let flags = if gap {
                            RecordFlags::DISCONTINUITY
                        } else {
                            RecordFlags::empty()
                        };
                        match self.publisher.publish_binary(BinaryRecord {
                            t,
                            sample_index: index,
                            flags,
                            payload: &payload,
                        }) {
                            Ok(_) => {
                                gap = false;
                                last_audio = Instant::now();
                                frames += 1;
                                inc(&lc.frames);
                                let us = read_at.elapsed().as_micros() as u64;
                                latency_ms = us as f64 / 1e3;
                                lc.latency_us_last.store(us, Ordering::Relaxed);
                                lc.latency_us_max.fetch_max(us, Ordering::Relaxed);
                                self.stat.latency(us);
                                inc(&self.stat.records);
                            }
                            // Fail closed: a gated audio record means the class check was
                            // bypassed somewhere; stop the chain.
                            Err(_) => {
                                inc(&lc.errors);
                                failed = true;
                                break;
                            }
                        }
                    }
                    pending.drain(..offset);
                    if failed {
                        break End::Error;
                    }
                }
                Next::Lost => gap = true,
                Next::Idle => {}
                Next::Closed => {
                    // The segment ended: a re-plumb into another window, or the end of the run.
                    if self.shared.continues.load(Ordering::SeqCst) {
                        inc(&lc.retune_ends);
                        break End::Segment;
                    }
                    break End::Source;
                }
            }
            if last_status.elapsed() >= self.config.status_interval {
                last_status = Instant::now();
                let status = AudioStatus {
                    level_dbfs: round2(self.demod.level_dbfs()),
                    snr_db: self.demod.snr_db().map(round2),
                    squelch_open: self.demod.squelch_open(),
                    agc_gain_db: round2(self.demod.agc_gain_db()),
                    frames,
                    squelched_frames: squelched,
                    lost_samples: lost
                        + counters
                            .chains
                            .lost_samples
                            .load(Ordering::Relaxed)
                            .saturating_sub(lost_before),
                    latency_ms: round2(latency_ms),
                    backlog_s: round2(backlog_s),
                    refined_center_hz: self.refined.map(|r| r.0),
                    refined_bandwidth_hz: self.refined.map(|r| r.1),
                    refine_updates: self.refiner.as_ref().map_or(0, LiveRefiner::updates),
                    // T-874: two-channel streams say whether L−R is really being decoded.
                    stereo: (channels == 2).then(|| self.demod.stereo_locked()),
                    stereo_lock_losses: (channels == 2)
                        .then(|| self.stereo_losses + self.demod.stereo_lock_losses()),
                };
                let t = self.t0.saturating_add_nanos(
                    (audio_index as f64 * 1e9 / AUDIO_SAMPLE_RATE_HZ).round() as i64,
                );
                if self
                    .publisher
                    .publish_status(t, audio_index, &status.to_value())
                    .is_ok()
                {
                    inc(&lc.status_records);
                }
            }
        };
        // Free the slot now, even while a (local) consumer still holds the session guard.
        self._slot.release();
        super::set_thread_stat(None);
        let dropped: u64 = self
            .handle
            .consumer_stats()
            .iter()
            .map(|s| s.records_dropped)
            .sum();
        add(&lc.consumer_dropped, dropped);
        end.count(lc);
        inc(&lc.detached);
        lc.running.fetch_sub(1, Ordering::SeqCst);
        if crate::debug_enabled() {
            eprintln!(
                "hk-pipeline: listen {} detached ({end:?}) after {frames} frames ({squelched} \
                 squelched, {dropped} dropped for consumers)",
                self.publisher.header().stream_id
            );
        }
    }
}

fn round2(v: f64) -> f64 {
    if v.is_finite() {
        (v * 100.0).round() / 100.0
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replumbing_is_503_and_a_finished_run_is_410() {
        let r = segment_ended(true);
        assert_eq!((r.status, r.code.as_str()), (503, "replumbing"));
        let r = segment_ended(false);
        assert_eq!((r.status, r.code.as_str()), (410, "source-ended"));
    }

    #[test]
    fn budget_is_a_fraction_of_the_cores_and_chains_cost_by_rate_and_mode() {
        let cfg = ListenConfig {
            cores: Some(4),
            ..ListenConfig::default()
        };
        assert_eq!(cfg.max_listeners, 8, "default cap");
        assert_eq!(cfg.budget_mcores(), 2000, "half of 4 cores");
        assert_eq!(cfg.chain_mcores(20e6, Some(true)), 1000);
        assert_eq!(cfg.chain_mcores(20e6, Some(false)), 800);
        assert_eq!(
            cfg.chain_mcores(20e6, None),
            1000,
            "dearer mode before the probe"
        );
        let s = ListenSettings {
            idle_timeout_s: 0.0,
            squelch_timeout_s: 0.0,
            ..ListenSettings::default()
        };
        let c = ListenConfig::from_settings(&s);
        assert_eq!(
            c.idle_timeout,
            Duration::from_secs(10),
            "0 keeps the idle default"
        );
        assert_eq!(c.squelch_timeout, None, "0 disables the squelch timeout");
    }

    #[test]
    fn slots_admit_by_count_and_budget_and_release_once() {
        let counters = Arc::new(Counters::default());
        let cfg = ListenConfig {
            cores: Some(1),
            cpu_fraction: 1.0,
            max_listeners: 3,
            ..ListenConfig::default()
        };
        let claim =
            |need, rate| Slot::claim(&counters, &cfg.limits(), ChainKind::Listen, need, rate);
        let a = claim(600, 12e6).unwrap();
        let e = claim(600, 12e6).err().unwrap();
        assert_eq!((e.status, e.code.as_str()), (503, "busy"));
        assert!(e.reason.contains("CPU budget"), "{e}");
        assert!(e.reason.contains("1 of 3 listeners"), "{e}");
        a.set_mcores(300);
        let b = claim(600, 12e6).unwrap();
        let c = claim(100, 1e6).unwrap();
        let e = claim(0, 1e6).err().unwrap();
        assert!(e.reason.contains("3 of 3 listeners"), "{e}");
        let lc = &counters.listen;
        assert_eq!(lc.budget_used_mcores.load(Ordering::SeqCst), 1000);
        let guard = StopOnDrop {
            stop: Arc::new(AtomicBool::new(false)),
            slot: b.clone(),
        };
        drop(guard);
        assert_eq!(
            lc.active.load(Ordering::SeqCst),
            2,
            "the guard frees the slot at once"
        );
        b.release();
        drop(b);
        assert_eq!(lc.active.load(Ordering::SeqCst), 2, "released only once");
        drop((a, c));
        assert_eq!(lc.active.load(Ordering::SeqCst), 0);
        assert_eq!(lc.budget_used_mcores.load(Ordering::SeqCst), 0);
    }
}
