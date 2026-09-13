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
use hk_demod::audio::{AudioConfig, AudioDemod, AudioPlan, LISTEN_DEMOD_VERSION, probe};
use hk_dsp::InputInfo;
use hk_estimate::SnippetRequest;
use hk_model::{ContentClass, EmitterId, SampleTime, Timestamp};
use hk_stream::audio::{
    AUDIO_DATATYPE, AUDIO_FRAME_SAMPLES, AUDIO_MAX_FRAME_LEN, AUDIO_SAMPLE_RATE_HZ, AgcInfo,
    AudioInfo, AudioStatus, ListenTarget, SquelchInfo, encode_pcm,
};
use hk_stream::{
    BinaryRecord, OpenRefusal, OpenRequest, OpenedStream, Publisher, PublisherConfig,
    PublisherHandle, RecordFlags, StreamHeader, StreamKind, StreamOpener,
};
use num_complex::Complex;

use super::{ChainReader, Next};
use crate::class::{ClassRule, class_name, classify_emitter, is_restricted, restricted_band};
use crate::config::ListenSettings;
use crate::run::Shared;
use crate::stats::{Counters, ListenCounters, add, inc};

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

fn mcores(cores: f64) -> u64 {
    if cores.is_finite() && cores > 0.0 {
        (cores * 1e3).round() as u64
    } else {
        0
    }
}

fn cores(mcores: u64) -> f64 {
    mcores as f64 / 1e3
}

/// Publishes `config`'s admission limits into the run's counters (`/api/status`).
pub(crate) fn publish_limits(counters: &Counters, config: &ListenConfig) {
    let lc = &counters.listen;
    lc.limit_listeners
        .store(config.max_listeners as u64, Ordering::Relaxed);
    lc.budget_mcores
        .store(config.budget_mcores(), Ordering::Relaxed);
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

/// Opens listen streams on a running pipeline (`PipelineHandle::listen_service`).
pub struct ListenManager {
    counters: Arc<Counters>,
    segment: SegmentFn,
    /// The run's listen limits (changeable at runtime), read at each request.
    settings: Arc<std::sync::Mutex<ListenSettings>>,
    /// Pinned by [`Self::with_config`] instead of following `settings`.
    config: Option<ListenConfig>,
}

static STREAM_SEQ: AtomicU64 = AtomicU64::new(1);

/// One admitted chain's share of the listener cap and the CPU budget (T-066). Released once, by
/// whichever comes first: the session guard dropping (the client left) or the chain ending, so a
/// client that stops and immediately listens again is never refused by its own old chain. Holds
/// the run's counters only, never a segment.
struct SlotInner {
    counters: Arc<Counters>,
    mcores: AtomicU64,
    released: AtomicBool,
}

impl SlotInner {
    fn release(&self) {
        if self.released.swap(true, Ordering::SeqCst) {
            return;
        }
        let lc = &self.counters.listen;
        lc.active.fetch_sub(1, Ordering::SeqCst);
        let m = self.mcores.load(Ordering::SeqCst);
        let _ = lc
            .budget_used_mcores
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |u| {
                Some(u.saturating_sub(m))
            });
    }
}

impl Drop for SlotInner {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Clone)]
struct Slot(Arc<SlotInner>);

impl Slot {
    /// Admits one chain estimated at `need` millicores on a source at `rate_hz`, or refuses with
    /// 503 `busy` naming both the listener count and the CPU budget.
    fn claim(
        counters: &Arc<Counters>,
        cfg: &ListenConfig,
        need: u64,
        rate_hz: f64,
    ) -> Result<Self, OpenRefusal> {
        let lc = &counters.listen;
        let max = cfg.max_listeners as u64;
        let budget = cfg.budget_mcores();
        let mut running = lc.active.load(Ordering::SeqCst);
        loop {
            if running >= max {
                return Err(OpenRefusal::new(
                    503,
                    "busy",
                    format!(
                        "listener limit: {running} of {max} listeners running ({:.2} of {:.2} \
                         CPU cores in use); stop one and try again",
                        cores(lc.budget_used_mcores.load(Ordering::SeqCst)),
                        cores(budget)
                    ),
                ));
            }
            match lc.active.compare_exchange(
                running,
                running + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(now) => running = now,
            }
        }
        // The first chain is always admitted: a small device can still listen at a high rate.
        let mut used = lc.budget_used_mcores.load(Ordering::SeqCst);
        loop {
            if running > 0 && used + need > budget {
                lc.active.fetch_sub(1, Ordering::SeqCst);
                return Err(OpenRefusal::new(
                    503,
                    "busy",
                    format!(
                        "CPU budget: this chain needs about {:.2} cores at {:.2} Msps; {:.2} of \
                         {:.2} cores in use by {running} of {max} listeners; stop one and try \
                         again",
                        cores(need),
                        rate_hz / 1e6,
                        cores(used),
                        cores(budget)
                    ),
                ));
            }
            match lc.budget_used_mcores.compare_exchange(
                used,
                used + need,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(now) => used = now,
            }
        }
        Ok(Self(Arc::new(SlotInner {
            counters: Arc::clone(counters),
            mcores: AtomicU64::new(need),
            released: AtomicBool::new(false),
        })))
    }

    /// Re-costs the chain once its mode is known (before the chain starts, so no release races).
    fn set_mcores(&self, need: u64) {
        let old = self.0.mcores.swap(need, Ordering::SeqCst);
        let _ = self.0.counters.listen.budget_used_mcores.fetch_update(
            Ordering::SeqCst,
            Ordering::SeqCst,
            |u| Some((u + need).saturating_sub(old)),
        );
    }

    fn release(&self) {
        self.0.release();
    }
}

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    Client,
    Idle,
    Squelch,
    Retune,
    Segment,
    Source,
    Error,
}

impl End {
    fn count(self, lc: &ListenCounters) {
        inc(match self {
            End::Client => &lc.closed_client,
            End::Idle => &lc.closed_idle,
            End::Squelch => &lc.closed_squelch,
            End::Retune => &lc.closed_retune,
            End::Segment => &lc.closed_segment,
            End::Source => &lc.closed_source,
            End::Error => &lc.closed_error,
        });
    }
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
        let target = ListenTarget::from_request(req)?;
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
            cfg,
            cfg.chain_mcores(rate_now, None),
            rate_now,
        )?;
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
        let mut reader = ChainReader::new(Arc::clone(shared), start, cursor);
        let want = ((cfg.probe_s * fs) as usize).max(1);
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
        let request = SnippetRequest {
            start_index: time.sample_index,
            end_index: time.sample_index + iq.len() as u64,
            center_offset_hz: fc - tune.center_hz,
            bandwidth_hz: probe_bw,
        };
        let pr = probe(info, &iq, &request)
            .map_err(|e| OpenRefusal::new(422, "probe-failed", format!("estimation: {e}")))?;
        let plan = AudioPlan::from_probe(&pr, &cfg.audio).map_err(|why| {
            OpenRefusal::new(
                422,
                "no-analog-mode",
                format!("no analog modulation recognised: {why}"),
            )
        })?;
        let (clo, chi) = plan.channel_extent_hz();
        if (plan.channel_center_hz - fc).abs() > 0.5 * probe_bw {
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
        let demod = AudioDemod::new(
            plan.clone(),
            cfg.audio.clone(),
            tune.sample_rate_hz,
            tune.center_hz,
        )
        .map_err(|e| OpenRefusal::new(500, "demod", e.to_string()))?;

        slot.set_mcores(cfg.chain_mcores(
            tune.sample_rate_hz,
            Some(plan.mode == hk_demod::AnalogMode::Wfm),
        ));
        let mut params = pr.params.estimated_params();
        if plan.mode == hk_demod::AnalogMode::Wfm {
            params.pilot_hz = pr
                .mode
                .features
                .pilot
                .filter(|p| p.found)
                .and_then(|p| p.frequency_hz);
        }
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
        header.max_frame_len = AUDIO_MAX_FRAME_LEN;
        header.audio = Some(AudioInfo {
            channels: 1,
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
        let stop = Arc::new(AtomicBool::new(false));
        let session = Session {
            shared: Arc::clone(shared),
            reader,
            demod,
            publisher,
            handle: handle.clone(),
            stop: Arc::clone(&stop),
            config: cfg.clone(),
            tune: (tune.center_hz, tune.sample_rate_hz),
            t0: time.host_time,
            _slot: slot.clone(),
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
            "params": ["emitter", "detection", "f_lo", "f_hi"],
            "records": format!(
                "data (type 1, {AUDIO_FRAME_SAMPLES} i16 LE mono samples) and status (type 3: \
                 level_dbfs, snr_db, squelch_open, agc_gain_db, ...); mode and parameters are \
                 estimated (header audio profile)"
            ),
        })
    }
}

struct Session {
    shared: Arc<Shared>,
    reader: ChainReader,
    demod: AudioDemod,
    publisher: Publisher,
    handle: PublisherHandle,
    stop: Arc<AtomicBool>,
    config: ListenConfig,
    /// Tuned centre and rate the demodulator was built for.
    tune: (f64, f64),
    /// Host time of the first probed sample (audio time origin).
    t0: Timestamp,
    _slot: Slot,
}

impl Session {
    fn run(mut self) {
        let counters = Arc::clone(&self.shared.counters);
        let lc = &counters.listen;
        let fs = self.shared.fs;
        let frame = AUDIO_FRAME_SAMPLES;
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
        let end = loop {
            if self.stop.load(Ordering::SeqCst) {
                break End::Client;
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
                        match AudioDemod::new(
                            self.demod.plan().clone(),
                            self.config.audio.clone(),
                            tune.1,
                            tune.0,
                        ) {
                            Ok(d) => self.demod = d,
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
                    let processed = self.demod.process(info, &self.reader.buf[..chunk.len]);
                    self.reader.release_to(chunk.end_sample());
                    if processed.is_err() {
                        inc(&lc.errors);
                        break End::Error;
                    }
                    self.demod.drain_audio_into(&mut pending);
                    let mut offset = 0;
                    let mut failed = false;
                    while pending.len() - offset >= frame {
                        let samples = &pending[offset..offset + frame];
                        offset += frame;
                        let index = audio_index;
                        audio_index += frame as u64;
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

    const UNRESTRICTED: ContentClass = ContentClass::Unrestricted;
    const METADATA: ContentClass = ContentClass::MetadataOnly;

    fn rule(lo: f64, hi: f64) -> ClassRule {
        ClassRule {
            freq_hz: [lo, hi],
            content_class: UNRESTRICTED,
            by: "test: tries to open content".into(),
        }
    }

    #[test]
    fn restricted_bands_refuse_audio_whatever_the_source_class_or_rules() {
        let open_all = [rule(0.0, 7e9)];
        for class in ContentClass::ALL.iter().copied() {
            for (lo, hi) in [
                (930.4e6, 930.6e6),     // narrowband PCS paging
                (152.235e6, 152.245e6), // VHF paging
                (880.0e6, 880.2e6),     // cellular
                (928.9e6, 929.05e6),    // partly inside 929-930 MHz paging
            ] {
                let e = listen_class(class, &open_all, lo, hi).unwrap_err();
                assert_eq!(e.status, 403, "{class:?} {lo}");
                assert!(
                    matches!(
                        e.content_class,
                        Some(ContentClass::RestrictedPaging | ContentClass::RestrictedCellular)
                    ),
                    "{e:?}"
                );
            }
        }
    }

    #[test]
    fn unclassified_and_restricted_sources_are_refused_unrestricted_permits() {
        // FM broadcast under the band prior: audio allowed.
        assert_eq!(
            listen_class(UNRESTRICTED, &[], 101.2e6, 101.4e6).unwrap(),
            UNRESTRICTED
        );
        // Unclassified (fail-closed source, no rule): refused.
        let e = listen_class(METADATA, &[], 433.9e6, 433.95e6).unwrap_err();
        assert_eq!(e.status, 403);
        assert!(e.reason.contains("unclassified"), "{e:?}");
        // A user rule vouching for the extent opens a metadata-only band (the FSK rule).
        assert_eq!(
            listen_class(METADATA, &[rule(433e6, 435e6)], 433.9e6, 433.95e6).unwrap(),
            UNRESTRICTED
        );
        // Restricted source classes refuse even outside restricted bands and with rules.
        for class in [
            ContentClass::RestrictedPaging,
            ContentClass::RestrictedCellular,
        ] {
            assert!(listen_class(class, &[rule(0.0, 7e9)], 101.2e6, 101.4e6).is_err());
        }
        assert!(listen_class(UNRESTRICTED, &[], f64::NAN, 1.0).is_err());
    }

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
        let a = Slot::claim(&counters, &cfg, 600, 12e6).unwrap();
        let e = Slot::claim(&counters, &cfg, 600, 12e6).err().unwrap();
        assert_eq!((e.status, e.code.as_str()), (503, "busy"));
        assert!(e.reason.contains("CPU budget"), "{e}");
        assert!(e.reason.contains("1 of 3 listeners"), "{e}");
        a.set_mcores(300);
        let b = Slot::claim(&counters, &cfg, 600, 12e6).unwrap();
        let c = Slot::claim(&counters, &cfg, 100, 1e6).unwrap();
        let e = Slot::claim(&counters, &cfg, 0, 1e6).err().unwrap();
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
