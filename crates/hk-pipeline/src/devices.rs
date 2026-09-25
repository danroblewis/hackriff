//! Further front ends of one run (T-510, milestone MSDR).
//!
//! # The composition
//!
//! A run has **one primary front end** — the source [`super::Pipeline::start`] has always taken,
//! with its segments, re-plumbs, control thread, chains, scheduler, spectrum stream and manual
//! recorder — and **zero or more further ones** ([`ExtraSource`]), added by
//! [`super::Pipeline::start_multi`]. Each further front end is its own
//! `{source, ring, capture thread}` set plus the readers that write the run's **shared,
//! already-per-device stores**:
//!
//! | Per front end (never shared) | Shared by every front end of the run |
//! |---|---|
//! | the source and its capture thread | the floor product (both scheme-1 pyramids and their state log) |
//! | the sample ring and its flow gate | the view-scheme pyramid (the unified canvas's growing edge) |
//! | the history reader: its STFT and its `NoiseFloorTracker` | the SQLite repository (detections, tracks, provenance) |
//! | the detector and its track inventory | the observation log (whose records name *their own* device) |
//! | the IQ capture ring on disk ([`crate::iqbuffer::device_dir`]) | the attention service, the compute registry, the Survey |
//! | its [`Counters`] and its interactive observer | the data directory |
//!
//! **Nothing here keys a store.** Every store already keys what it holds by the *frame's own*
//! provenance `device_id`: T-304's history source key, T-377's per-origin pyramid floors, the
//! detections' `provenance_ref -> Provenance.device_id`, and
//! [`hk_store::coverage::record_device`] over the observation records. What was single-device was
//! only the **composition** — `Pipeline::start` built one ring for one source — and a ring carries
//! exactly one stream's sample indices, which is why [`crate::history`]'s floor tracker need not
//! be keyed by origin: one ring per front end keeps that true at any N.
//!
//! # What a further front end does not get (yet)
//!
//! The Candidate/Confirmed inventory surface, chains and demod, the spectrum stream, the
//! scheduler, re-plumbing and the manual recorder stay with the primary. A further front end
//! **collects passively** — CLAUDE.md's model, "additional SDRs collect passively in the
//! background and widen the coverage available to display; they never add view windows" — and its
//! only control is an in-place **centre** change ([`RunDevice::control`]); a rate change would
//! need its ring and STFT re-plumbed and is refused rather than half-applied. Serving N front ends
//! over the control API is T-511/T-512, the full two-device e2e is T-513, and the RTL-SDR driver
//! that makes device 2 real hardware is T-514.
//!
//! # USB, and why N is a hardware question before it is a software one
//!
//! 20 Msps of ci8 is ~40 MB/s, which saturates one USB 2.0 controller on its own. Two front ends
//! at full rate therefore want **separate USB controllers**; on one controller they will both
//! lose samples, which shows up here honestly as `source_dropped` / `lost_samples` on each front
//! end's own counters rather than as a silent gap. Nothing in this module can fix that, and
//! nothing in it pretends to: the composition is N-capable, the bus is not.
//!
//! # One clock, and the seal that waits for every front end
//!
//! The shared pyramids have **one monotonic watermark**, advanced by the newest frame of *any*
//! front end less the seal lag. So the front ends of one run must share a clock: a front end whose
//! block times trail another's by more than that lag has its frames counted `frames_late` instead
//! of folded. Live radios stamp host time and share it; replays stamp their recording's clock and
//! generally do not, so [`validate`] refuses a further front end whose first-sample time is more
//! than [`MAX_START_SKEW`] from the primary's.
//!
//! For the same reason the **end-of-run seal** cannot be taken by whichever history reader
//! finishes first: it would make every later frame of the others late. With further front ends no
//! reader seals (`Shared::seal_at_end` is false); [`finish`] stops and joins them all and then
//! seals both products once, through the newest frame any front end folded. A single-device run
//! keeps its reader's own seal, byte for byte.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use hk_core::{
    DeviceInfo, Gains, RingConfig, Source, SourceCapabilities, SourceControl, SourceError,
    SourceStats, ring_buffer,
};
use hk_model::Repository;
use num_complex::Complex;

use super::{Calibrated, Common, Shared, SourceInfo, Worker, replay_block_len};
use crate::class::window_class;
use crate::config::{PipelineConfig, detection_resolution};
use crate::gate::FlowGate;
use crate::iqbuffer::IqBufferService;
use crate::stats::Counters;

/// Largest first-sample time difference between a further front end and the primary (see the
/// module docs: the shared pyramids have one watermark, so the front ends must share a clock).
/// Generous for live radios that open seconds apart (an R820T takes ~10 s to lock); a replay on
/// its recording's own clock is days away and is refused.
pub const MAX_START_SKEW: Duration = Duration::from_secs(60);

/// How often a further front end's observer thread looks for new blocks (T-510). One relaxed
/// atomic load per wake, off the capture thread, so it never gates the ring; it ticks the observer
/// only when capture has actually moved (see the spawn site for why that matters).
const OBSERVE_POLL: Duration = Duration::from_millis(1);

/// A further front end for [`super::Pipeline::start_multi`].
pub struct ExtraSource {
    /// The opened source (any driver behind the generic device interface: the mock SDR today,
    /// an RTL-SDR at T-514, a second HackRF, a SoapySDR device later).
    pub source: Box<dyn Source>,
    /// Its rate, centre and first-sample time.
    pub info: SourceInfo,
}

impl std::fmt::Debug for ExtraSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtraSource")
            .field("driver", &self.source.capabilities().driver)
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

/// One front end of a running pipeline, as [`super::PipelineHandle::devices`] lists it.
#[derive(Clone)]
pub struct RunDevice {
    /// The front end's identity: the provenance `device_id` every block, history frame, detection
    /// and observation record it produced carries. `None` only for a primary that states none.
    pub device_id: Option<String>,
    /// The run's primary front end (the one with segments, detection surfaces and the control
    /// plane).
    pub primary: bool,
    /// Its control handle. For a further front end, centre changes apply in place and a rate
    /// change is refused (see the module docs); the primary's retunes go through
    /// [`super::PipelineController::retune`], never this handle, because they may re-plumb.
    pub control: Arc<dyn SourceControl>,
    /// Its counters (capture, history reader, detector). The primary's are the run's.
    pub counters: Arc<Counters>,
    /// Its IQ capture ring.
    pub iq_buffer: Arc<IqBufferService>,
}

impl std::fmt::Debug for RunDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunDevice")
            .field("device_id", &self.device_id)
            .field("primary", &self.primary)
            .finish_non_exhaustive()
    }
}

/// A running further front end.
pub(super) struct AuxDevice {
    pub device_id: String,
    pub control: Arc<dyn SourceControl>,
    pub shared: Arc<Shared>,
    pub iq_buffer: Arc<IqBufferService>,
    pub workers: Mutex<Vec<Worker>>,
}

impl AuxDevice {
    pub fn run_device(&self) -> RunDevice {
        RunDevice {
            device_id: Some(self.device_id.clone()),
            primary: false,
            control: Arc::clone(&self.control),
            counters: Arc::clone(&self.shared.counters),
            iq_buffer: Arc::clone(&self.iq_buffer),
        }
    }
}

/// Checks the further front ends **before anything opens**: each states an identity distinct from
/// every other front end's of this run (two front ends under one id would pool their floors,
/// baselines, detections and coverage — exactly what the per-device stores exist to prevent), can
/// run under the run's lossless setting, and shares the primary's clock ([`MAX_START_SKEW`]).
pub(super) fn validate(
    cfg: &PipelineConfig,
    primary: Option<&str>,
    primary_info: &SourceInfo,
    extra: &[ExtraSource],
) -> anyhow::Result<Vec<DeviceInfo>> {
    let mut seen: Vec<String> = primary.map(str::to_owned).into_iter().collect();
    let mut out = Vec::with_capacity(extra.len());
    for (i, e) in extra.iter().enumerate() {
        let Some(dev) = e.source.control().device_info() else {
            anyhow::bail!(
                "further front end {} ({}) states no identity: its frames, detections and \
                 coverage could not be told from another front end's",
                i + 1,
                e.source.capabilities().driver
            );
        };
        if seen.contains(&dev.device_id) {
            anyhow::bail!(
                "front end {} is already part of this run: one device id per front end, or one \
                 radio's coverage would answer for another's",
                dev.device_id
            );
        }
        if cfg.lossless && !e.source.pausable() {
            anyhow::bail!(
                "lossless mode needs sources that can pause, and front end {} cannot",
                dev.device_id
            );
        }
        let skew = e
            .info
            .start_time
            .as_unix_nanos()
            .abs_diff(primary_info.start_time.as_unix_nanos());
        if u128::from(skew) > MAX_START_SKEW.as_nanos() {
            anyhow::bail!(
                "front end {} starts {:.1} s from the primary: the run's shared history has one \
                 watermark, and frames that far behind it would be counted late, not stored",
                dev.device_id,
                skew as f64 / 1e9
            );
        }
        seen.push(dev.device_id.clone());
        out.push(dev);
    }
    Ok(out)
}

/// Starts one further front end: its ring, capture thread, history reader, detector, IQ ring
/// feeder and coverage observer.
pub(super) fn start(
    common: &Common,
    template: &PipelineConfig,
    extra: ExtraSource,
    device: DeviceInfo,
) -> anyhow::Result<AuxDevice> {
    let ExtraSource { source, info } = extra;
    let fs = info.sample_rate_hz;
    let mut cfg = template.clone();
    cfg.device_id = device.device_id.clone();
    cfg.device_hw = Some(device.hw.clone());
    cfg.source_class = window_class(info.center_hz, fs);
    // A further front end has no control plane of its own: it does not re-plumb, does not drive
    // the scheduler and runs no chains (the run-wide `chains::MAX_RUNTIME_CHAINS` cap is a
    // property of the run, and T-558 made it so; N front ends must not multiply it).
    cfg.live_window_class = false;
    cfg.drive_scheduler = false;
    cfg.settings.chains = Some(Vec::new());
    // Its ring holds its own rate: a rate change is refused (`FixedRate`), so this is its highest.
    cfg.iq_buffer.max_rate_hz = Some(fs);
    let control: Arc<dyn SourceControl> = Arc::new(FixedRate {
        inner: source.control(),
    });
    let source = Calibrated::wrap(source, &common.pins);
    let (fft_len, averages) = detection_resolution(fs, &cfg.settings);
    let min_block = replay_block_len(fs).min(1024);
    let ring_cfg = RingConfig::for_duration(fs, cfg.settings.ring_s.max(0.5), min_block);
    let (writer, ring) = ring_buffer::<Complex<i8>>(ring_cfg);
    let gate = Arc::new(FlowGate::new(cfg.lossless, ring.sample_capacity()));
    // Its own database connection: SQLite serialises writers, and sharing the primary's `Mutex`
    // would put this front end's detector behind the primary's.
    let repo = Repository::open(&common.db_path)?;
    let iq_buffer = Arc::new(IqBufferService::open_in(
        &cfg,
        common.db_path.clone(),
        crate::iqbuffer::device_dir(&common.data_dir, &device.device_id),
    ));
    let counters = Arc::new(Counters::default());
    let shared = Arc::new(Shared {
        dc_twin: super::dc_twin_rule(common),
        receiver: Arc::clone(&common.receiver),
        counters: Arc::clone(&counters),
        ring,
        gate,
        repo: Mutex::new(repo),
        db_path: common.db_path.clone(),
        stop: Arc::new(AtomicBool::new(false)),
        // T-505/T-510 collision: T-505 added `view_queue` to `Shared` while T-510 added this
        // second construction site, so neither branch could see the other's half.
        //
        // A further front end gets its OWN queue, mirroring `run::start_segment`. That follows
        // T-510's own split: the history reader is per front end (its own STFT and noise floor),
        // while the view PYRAMID they feed is shared and already keys what it holds by each
        // frame's provenance. Sharing one queue across devices would interleave two readers'
        // frames into a single drop-oldest buffer, so a fast device could starve a slow one of
        // view rows - and an under-fed view lattice greys time the radio really looked at.
        view_queue: common.view.is_some().then(|| {
            Arc::new(crate::history::ViewQueue::new(
                crate::history::VIEW_QUEUE_FRAMES,
            ))
        }),
        cc_verdicts: Arc::clone(&common.cc_verdicts),
        survey_id: common.survey_id,
        fs,
        fft_len,
        averages,
        inventory: Mutex::new(Box::new(crate::inventory::TrackInventory::default())),
        specs: Vec::new(),
        display: Arc::clone(&common.display),
        continues: AtomicBool::new(false),
        successor_grace_ms: AtomicU64::new(hk_stream::BETWEEN_WINDOWS_GRACE.as_millis() as u64),
        seal_at_end: false,
        bursts: Arc::clone(&common.bursts),
        claims: crate::chains::EmissionClaims::default(),
        track_decodes: Arc::default(),
        compute: common.compute.clone(),
        cfg,
    });
    // T-115/T-378: this front end's own coverage record, under **its** device id.
    let observer = common.observations.as_ref().map(|log| {
        log.interactive_for(
            Some(device.device_id.clone()),
            fft_len,
            Some(common.survey_id),
            Arc::clone(&counters),
        )
    });
    let tag = |what: &str| format!("hk-{what}:{}", device.device_id);
    let mut workers: Vec<Worker> = Vec::new();
    let spawned = (|| -> anyhow::Result<()> {
        {
            let (s, p, a, v) = (
                Arc::clone(&shared),
                Arc::clone(&common.product),
                common.attention.clone(),
                common.view.clone(),
            );
            let h = thread::Builder::new()
                .name(tag("history"))
                .spawn(move || crate::history::run(s, p, a, v))?;
            workers.push(("hk-history (further front end)", h));
        }
        {
            // Detection is always-on per CLAUDE.md, and it is what makes this front end's
            // `Detection` rows carry **its** provenance. Its `ControlEvent`s have no control
            // thread to reach (`DetectNode::send` already tolerates a closed channel), so this
            // front end detects and records but opens no chains.
            let (s, t) = (Arc::clone(&shared), std::sync::mpsc::channel().0);
            let h = thread::Builder::new()
                .name(tag("detect"))
                .spawn(move || crate::detect::run(s, t))?;
            workers.push(("hk-detect (further front end)", h));
        }
        if iq_buffer.active() {
            let (s, b) = (Arc::clone(&shared), Arc::clone(&iq_buffer));
            let reader = shared.ring.reader_at(0);
            let cursor = shared.gate.register(0);
            let h = thread::Builder::new()
                .name(tag("iqbuffer"))
                .spawn(move || b.feed(s, reader, cursor))?;
            workers.push(("hk-iqbuffer (further front end)", h));
        }
        if let Some(mut observer) = observer {
            // **Driven by capture progress, not by the wall clock.** The observer records an
            // interval between two polls, so polling on a timer would record a dwell as long as
            // the *wall* time the poll happened to straddle — which for a fast replay (the whole
            // stream in under a second) is a fraction of what the front end actually observed,
            // and for a stream that lands between two polls is nothing at all. Coverage that
            // under-claims is coverage that greys a band this radio really looked at. Ticking
            // when `blocks` moves ties the record to the samples instead, at any replay speed.
            let s = Arc::clone(&shared);
            let h = thread::Builder::new().name(tag("observe")).spawn(move || {
                let mut seen = 0u64;
                loop {
                    let stopped = s.stop.load(Ordering::SeqCst);
                    let blocks = s.counters.source.blocks.load(Ordering::Relaxed);
                    if blocks != seen {
                        seen = blocks;
                        observer.tick();
                    }
                    if stopped {
                        break;
                    }
                    thread::sleep(OBSERVE_POLL);
                }
                // The open dwell closes at the last stream time seen, on drop.
                Ok(())
            })?;
            workers.push(("hk-observe (further front end)", h));
        }
        let s = Arc::clone(&shared);
        let capture = hk_core::rt::spawn_capture_thread(tag("capture"), move |_priority| {
            let (writer, s) = (writer, s);
            let r = crate::capture::run(source, None, writer, Arc::clone(&s));
            if r.is_err() {
                s.stop.store(true, Ordering::SeqCst);
            }
            r
        })?;
        workers.insert(0, ("hk-capture (further front end)", capture));
        Ok(())
    })();
    if let Err(e) = spawned {
        // The readers that did start end when the ring closes, which dropping the unspawned
        // writer did.
        shared.stop.store(true, Ordering::SeqCst);
        let mut errors = Vec::new();
        join_aux(&mut workers, &mut errors);
        return Err(e.context(format!("starting front end {}", device.device_id)));
    }
    Ok(AuxDevice {
        device_id: device.device_id,
        control,
        shared,
        iq_buffer,
        workers: Mutex::new(workers),
    })
}

/// Joins a further front end's threads, collecting their errors. Deliberately *not*
/// [`super::join_workers`]: that one reports the capture thread's error as "the front end stopped
/// delivering", which drives the primary's recovery state machine. A further front end has no
/// recovery yet (T-512); its failure is reported and ends that front end alone, never the run.
fn join_aux(workers: &mut Vec<Worker>, errors: &mut Vec<String>) {
    for (name, h) in workers.drain(..) {
        match h.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => errors.push(format!("{name}: {e:#}")),
            Err(_) => errors.push(format!("{name}: panicked")),
        }
    }
}

/// Ends the further front ends and, when the run deferred it to here, takes the one end-of-run
/// seal (module docs). Called once from the run's single end (`run::end_run`); idempotent.
pub(super) fn finish(common: &Common, errors: &mut Vec<String>) {
    for d in &common.aux {
        d.shared.stop.store(true, Ordering::SeqCst);
    }
    for d in &common.aux {
        let mut workers =
            std::mem::take(&mut *d.workers.lock().unwrap_or_else(PoisonError::into_inner));
        let mut errs = Vec::new();
        join_aux(&mut workers, &mut errs);
        errors.extend(errs.into_iter().map(|e| format!("{}: {e}", d.device_id)));
    }
    if !common.defer_seal || common.sealed.swap(true, Ordering::SeqCst) {
        return;
    }
    {
        let mut p = common
            .product
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let latest = [
            p.uncalibrated_pyramid().latest_frame_end(),
            p.calibrated_pyramid().latest_frame_end(),
        ]
        .into_iter()
        .flatten()
        .max();
        // The reader's own rule (`crate::history::run`), taken once for every front end —
        // including T-942's: through the last frame, never past it.
        if let Some(t) = latest
            && let Err(e) = p.seal_all(t)
        {
            errors.push(format!("sealing history: {e}"));
        }
        if let Err(e) = p.checkpoint() {
            errors.push(format!("history checkpoint: {e}"));
        }
        crate::history::update_tile_counters(&common.counters, &p);
    }
    if let Some(v) = &common.view {
        let mut p = v.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(t) = p.latest_frame_end() {
            let _ = p.seal_through(t);
        }
        let _ = p.checkpoint();
        // T-901: the view pyramid defers its writes; every writer has joined, so this lands the
        // run's last files inline.
        let _ = p.flush_writes();
        crate::history::update_view_tile_counters(&common.counters, &p);
    }
}

/// A further front end's control: everything passes through except a **sample-rate** change,
/// which would need its ring, STFT and detection resolution rebuilt — a re-plumb this composition
/// does not do yet (T-512). Refused rather than half-applied, because a source silently running
/// at a rate its reader is not built for produces measurements that are wrong rather than absent.
struct FixedRate {
    inner: Arc<dyn SourceControl>,
}

impl SourceControl for FixedRate {
    fn capabilities(&self) -> &SourceCapabilities {
        self.inner.capabilities()
    }

    fn tune(&self, center_hz: f64) -> Result<(), SourceError> {
        self.inner.tune(center_hz)
    }

    fn set_sample_rate(&self, _: f64) -> Result<(), SourceError> {
        Err(SourceError::Unsupported {
            source_name: "a further front end of a multi-source run",
            operation: "a sample-rate change (its ring, STFT and detection resolution are built \
                        for its opening rate; T-512)",
        })
    }

    fn set_gains(&self, gains: &Gains) -> Result<(), SourceError> {
        self.inner.set_gains(gains)
    }

    fn set_baseband_filter(&self, bandwidth_hz: f64) -> Result<(), SourceError> {
        self.inner.set_baseband_filter(bandwidth_hz)
    }

    fn set_bias_tee(&self, enabled: bool) -> Result<(), SourceError> {
        self.inner.set_bias_tee(enabled)
    }

    fn start(&self) -> Result<(), SourceError> {
        self.inner.start()
    }

    fn stop(&self) -> Result<(), SourceError> {
        self.inner.stop()
    }

    fn set_gain(&self, stage: &str, db: f64) -> Result<(), SourceError> {
        self.inner.set_gain(stage, db)
    }

    fn stats(&self) -> Option<SourceStats> {
        self.inner.stats()
    }

    fn device_info(&self) -> Option<DeviceInfo> {
        self.inner.device_info()
    }
}
