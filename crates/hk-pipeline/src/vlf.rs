//! The VLF/LF science service (T-891): runs T-277's `hk_dsp::vlf` analyses on an
//! **accessory-fed** source and publishes what they found, so SPACE-001 (SID flare monitor),
//! SPACE-041 (ELF/VLF broadband receiver) and PROP-019 (VLF/LF phase → reflection height) are
//! reachable end to end through the device interface — with the accessory attached.
//!
//! # Only through the accessory
//!
//! The three use cases stay `needs-accessory`: the HackRF tunes no lower than 1 MHz. The service
//! refuses a stream whose provenance does not say it came through an accessory
//! ([`hk_core::source::AccessoryKind::from_provenance`]) and reports `failed` with the reason, so
//! nothing here can claim VLF science from the base device.
//!
//! # Blind first
//!
//! Which transmitters are audible is **measured**: the first [`VlfConfig::discovery_s`] of
//! samples go through [`hk_dsp::vlf::find_carriers`], and every carrier found is tracked from the
//! stream's first sample on ([`hk_dsp::vlf::CarrierTracker`], absolute-index referenced, so a gap
//! never resets the phase). No transmitter list is consulted. The only external knowledge is
//! optional **path geometry** ([`VlfPath`]: the great-circle distance to a transmitter the user
//! knows), which PROP-019 needs to turn a phase change into a height change; without it the phase
//! step is still reported, just not converted.
//!
//! # What comes out, in data-model terms
//!
//! - **SPACE-001:** amplitude steps of each tracked carrier ([`VlfAmplitudeStep`]), at absolute
//!   capture time.
//! - **PROP-019:** phase steps with the tracker's linear drift removed ([`VlfPhaseStep`]), plus the
//!   implied reflection-height change where a [`VlfPath`] matches; `phase_disciplined` says whether
//!   the stream's clock was GNSS/externally referenced, which is what makes absolute phase
//!   meaningful.
//! - **SPACE-041:** each sferic is a [`Detection`] — a time–frequency region with its own time
//!   extent, `flags.impulsive`, the accessory stream's `provenance_ref` — spanning the receiver's
//!   whole band (a sferic is broadband; `obw_hz` is the receiver band, not a measured occupancy,
//!   and `xdb_bandwidth_hz` stays unset for that reason).
//!
//! Everything is bounded: [`VlfConfig::max_points`] per carrier and [`VlfConfig::max_sferics`]
//! (with a running total), so a week-long run holds the most recent window, not an accumulator.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hk_core::source::AccessoryKind;
use hk_core::{BlockHeader, Discontinuity, ProvenanceHandle, Source, SourceControl};
use hk_dsp::vlf::{
    CarrierPeak, CarrierTracker, TrackPoint, VlfPoint, detect_amplitude_steps, detect_phase_steps,
    detect_sferic_events, find_carriers, phase_drift_per_point, reflection_height_change_km,
};
use hk_model::{
    ClockSource, Detection, DetectionFlags, DetectionId, Provenance, ProvenanceId, SampleTime,
    SurveyId, TimeRange, Timestamp,
};
use num_complex::Complex32;
use serde::Serialize;

/// A transmitter-to-receiver path the user knows (PROP-019 geometry). Applied to a discovered
/// carrier within `tolerance_hz` of `carrier_hz`; it never creates a carrier.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct VlfPath {
    /// Transmitter frequency, Hz.
    pub carrier_hz: f64,
    /// Match tolerance, Hz.
    pub tolerance_hz: f64,
    /// Ground path length, km.
    pub path_km: f64,
    /// Undisturbed reflection height, km (≈70 by day, ≈85 by night).
    pub base_height_km: f64,
}

/// Service settings.
#[derive(Clone, Debug, PartialEq)]
pub struct VlfConfig {
    /// Seconds of audio the blind carrier search uses.
    pub discovery_s: f64,
    /// Lowest carrier considered, Hz (mains harmonics and the audio low end sit below).
    pub min_carrier_hz: f64,
    /// Carrier SNR over the band median, dB.
    pub min_carrier_snr_db: f32,
    /// At most this many carriers are tracked.
    pub max_carriers: usize,
    /// Seconds per amplitude/phase point.
    pub point_s: f64,
    /// Points either side a step is judged over.
    pub step_window_points: usize,
    /// Smallest relative amplitude change reported (SPACE-001).
    pub min_relative_step: f32,
    /// Smallest phase change reported, radians (PROP-019).
    pub min_phase_step_rad: f64,
    /// Sferic threshold, × the median |x| of non-zero samples (SPACE-041).
    pub sferic_k: f32,
    /// Sferic refractory time, s.
    pub sferic_dead_s: f64,
    /// Seconds per sferic-threshold chunk.
    pub chunk_s: f64,
    /// Known path geometry (optional).
    pub paths: Vec<VlfPath>,
    /// Points kept per carrier (the most recent).
    pub max_points: usize,
    /// Sferic detections kept (the most recent).
    pub max_sferics: usize,
}

impl Default for VlfConfig {
    fn default() -> Self {
        Self {
            discovery_s: 10.0,
            min_carrier_hz: 1_000.0,
            min_carrier_snr_db: 20.0,
            max_carriers: 8,
            point_s: 0.1,
            step_window_points: 30,
            min_relative_step: 0.2,
            min_phase_step_rad: 0.3,
            sferic_k: 20.0,
            sferic_dead_s: 0.005,
            chunk_s: 1.0,
            paths: Vec::new(),
            max_points: 36_000,
            max_sferics: 10_000,
        }
    }
}

/// Where the service is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum VlfState {
    /// No block yet.
    Waiting,
    /// Collecting the carrier-search window.
    Discovering,
    /// Tracking the carriers found.
    Tracking,
    /// The source ended.
    Finished,
    /// Refused or failed; see `error`.
    Failed,
}

/// One amplitude/phase point at absolute capture time.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct VlfTrackPoint {
    /// Block start.
    pub t_ns: Timestamp,
    /// Carrier amplitude, full scale.
    pub amplitude: f32,
    /// Unwrapped phase, radians (raw: the tracker's drift is not removed here).
    pub phase_rad: f64,
}

/// SPACE-001: a sudden amplitude change of a carrier.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct VlfAmplitudeStep {
    /// When.
    pub t_ns: Timestamp,
    /// (after − before) / before.
    pub relative_change: f32,
}

/// PROP-019: a sudden phase change of a carrier.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct VlfPhaseStep {
    /// When.
    pub t_ns: Timestamp,
    /// Phase after − before, radians, drift removed. Positive = advance = shorter path.
    pub dphi_rad: f64,
    /// Implied reflection-height change, km, when a [`VlfPath`] matched.
    pub reflection_height_change_km: Option<f64>,
    /// The matched path length, km.
    pub path_km: Option<f64>,
}

/// One carrier found blind and tracked.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct VlfCarrierReport {
    /// Frequency from the blind search, Hz (the tracker mixes here).
    pub carrier_hz: f64,
    /// That frequency corrected by the track's median phase drift, Hz.
    pub carrier_hz_refined: f64,
    /// Search SNR, dB.
    pub snr_db: f32,
    /// Points held (bounded).
    pub point_count: usize,
    /// The points, when asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub points: Option<Vec<VlfTrackPoint>>,
    /// SPACE-001.
    pub amplitude_steps: Vec<VlfAmplitudeStep>,
    /// PROP-019.
    pub phase_steps: Vec<VlfPhaseStep>,
}

/// What the service has found so far.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct VlfReport {
    /// State.
    pub state: VlfState,
    /// Why it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Provenance `device_id` of the accessory stream.
    pub device_id: Option<String>,
    /// Accessory kind, e.g. `vlf-receiver`.
    pub accessory: Option<&'static str>,
    /// Provenance of the stream (the latest).
    pub provenance_ref: Option<ProvenanceId>,
    /// The provenance itself.
    pub provenance: Option<Provenance>,
    /// Audio rate, Hz.
    pub sample_rate_hz: f64,
    /// The stream's clock is GNSS- or externally referenced and locked (absolute phase is then
    /// meaningful — PROP-019).
    pub phase_disciplined: bool,
    /// First to last analysed sample.
    pub window: Option<TimeRange>,
    /// Samples analysed.
    pub samples: u64,
    /// Gaps seen (overruns).
    pub gaps: u64,
    /// Samples lost in them.
    pub dropped_samples: u64,
    /// Carriers.
    pub carriers: Vec<VlfCarrierReport>,
    /// SPACE-041 sferics (the most recent, bounded).
    pub sferics: Vec<Detection>,
    /// Every sferic seen, including those no longer held.
    pub sferic_total: u64,
}

struct Carrier {
    peak: CarrierPeak,
    tracker: CarrierTracker,
    points: VecDeque<VlfTrackPoint>,
}

/// The analysis state machine, fed block by block ([`VlfService`] drives it on a thread;
/// [`analyze_source`] runs it to the end of a finite source).
pub struct VlfAnalyzer {
    cfg: VlfConfig,
    survey_id: SurveyId,
    state: VlfState,
    error: Option<String>,
    accessory: Option<AccessoryKind>,
    provenance: Option<ProvenanceHandle>,
    fs: f64,
    anchor: Option<SampleTime>,
    next_index: Option<u64>,
    first_t: Option<Timestamp>,
    last_t: Option<Timestamp>,
    samples: u64,
    gaps: u64,
    dropped: u64,
    discovery: Vec<f32>,
    discovery_start: u64,
    carriers: Vec<Carrier>,
    chunk: Vec<f32>,
    chunk_start: u64,
    dead_until: u64,
    sferics: VecDeque<Detection>,
    sferic_total: u64,
    real: Vec<f32>,
    scratch: Vec<TrackPoint>,
}

impl VlfAnalyzer {
    /// A fresh analyzer.
    pub fn new(cfg: VlfConfig) -> Self {
        Self {
            cfg,
            survey_id: SurveyId::new(),
            state: VlfState::Waiting,
            error: None,
            accessory: None,
            provenance: None,
            fs: 0.0,
            anchor: None,
            next_index: None,
            first_t: None,
            last_t: None,
            samples: 0,
            gaps: 0,
            dropped: 0,
            discovery: Vec::new(),
            discovery_start: 0,
            carriers: Vec::new(),
            chunk: Vec::new(),
            chunk_start: 0,
            dead_until: 0,
            sferics: VecDeque::new(),
            sferic_total: 0,
            real: Vec::new(),
            scratch: Vec::new(),
        }
    }

    /// The analyzer has stopped taking blocks (finished or failed).
    pub fn done(&self) -> bool {
        matches!(self.state, VlfState::Finished | VlfState::Failed)
    }

    /// Marks the analyzer failed with `why`.
    pub fn fail(&mut self, why: impl Into<String>) {
        self.state = VlfState::Failed;
        self.error = Some(why.into());
    }

    /// Feeds one block. A stream not from an accessory fails the analyzer and is never analysed.
    pub fn push(&mut self, h: &BlockHeader, samples: &[Complex32]) {
        if self.done() {
            return;
        }
        let Some(kind) = AccessoryKind::from_provenance(&h.provenance) else {
            self.fail(format!(
                "{} is not an accessory-fed source: the VLF analyses (SPACE-001, SPACE-041, \
                 PROP-019) need the VLF receiver accessory, below the base front end's range",
                h.provenance.device_id
            ));
            return;
        };
        self.accessory = Some(kind);
        let fs = h.sample_rate_hz();
        if fs != self.fs {
            // A new rate starts the analysis over (points and sferics already found stay).
            self.flush_chunk();
            self.fs = fs;
            self.carriers.clear();
            self.discovery.clear();
            self.discovery_start = h.first_sample();
            self.chunk_start = h.first_sample();
            self.state = VlfState::Discovering;
        }
        let first = h.first_sample();
        let gap = h.discontinuity.contains(Discontinuity::GAP)
            || self.next_index.is_some_and(|n| n != first);
        if gap {
            self.gaps += 1;
            self.dropped += h.dropped_before;
            self.flush_chunk();
            self.chunk_start = first;
            if self.state == VlfState::Discovering {
                // The search wants contiguous samples.
                self.discovery.clear();
                self.discovery_start = first;
            }
        }
        self.provenance = Some(h.provenance.clone());
        self.anchor = Some(h.time);
        self.first_t.get_or_insert(h.time.host_time);
        self.real.clear();
        self.real.extend(samples.iter().map(|c| c.re));
        let n = self.real.len() as u64;
        self.last_t = Some(h.time.time_of(first + n, fs));
        self.next_index = Some(first + n);
        self.samples += n;

        let real = std::mem::take(&mut self.real);
        match self.state {
            VlfState::Discovering => {
                if self.discovery.is_empty() {
                    self.discovery_start = first;
                }
                self.discovery.extend_from_slice(&real);
                if self.discovery.len() as f64 >= self.cfg.discovery_s * fs {
                    self.discover();
                }
            }
            VlfState::Tracking => self.track(first, &real),
            _ => {}
        }
        if self.chunk.is_empty() {
            self.chunk_start = first;
        }
        self.chunk.extend_from_slice(&real);
        if self.chunk.len() as f64 >= self.cfg.chunk_s * fs {
            self.flush_chunk();
        }
        self.real = real;
    }

    /// The source ended: analyse what is buffered.
    pub fn finish(&mut self) {
        if self.done() {
            return;
        }
        if self.state == VlfState::Discovering && !self.discovery.is_empty() {
            self.discover();
        }
        self.flush_chunk();
        self.state = VlfState::Finished;
    }

    fn block_len(&self) -> usize {
        ((self.cfg.point_s * self.fs).round() as usize).max(1)
    }

    fn discover(&mut self) {
        let peaks = find_carriers(
            &self.discovery,
            self.fs,
            self.cfg.min_carrier_hz,
            self.cfg.min_carrier_snr_db,
            self.cfg.max_carriers,
        );
        let block = self.block_len();
        self.carriers = peaks
            .into_iter()
            .map(|peak| Carrier {
                peak,
                tracker: CarrierTracker::new(self.fs, peak.carrier_hz, block, self.discovery_start),
                points: VecDeque::new(),
            })
            .collect();
        self.state = VlfState::Tracking;
        let buf = std::mem::take(&mut self.discovery);
        self.track(self.discovery_start, &buf);
    }

    fn track(&mut self, start: u64, x: &[f32]) {
        let (Some(anchor), fs) = (self.anchor, self.fs) else {
            return;
        };
        for c in &mut self.carriers {
            self.scratch.clear();
            c.tracker.push(start, x, &mut self.scratch);
            for p in &self.scratch {
                if c.points.len() == self.cfg.max_points {
                    c.points.pop_front();
                }
                c.points.push_back(VlfTrackPoint {
                    t_ns: anchor.time_of(p.start_index, fs),
                    amplitude: p.amplitude,
                    phase_rad: p.phase_rad,
                });
            }
        }
    }

    fn flush_chunk(&mut self) {
        if self.chunk.is_empty() || self.fs <= 0.0 {
            self.chunk.clear();
            return;
        }
        let (Some(anchor), Some(prov)) = (self.anchor, self.provenance.clone()) else {
            self.chunk.clear();
            return;
        };
        let fs = self.fs;
        let dead = ((self.cfg.sferic_dead_s * fs).round() as usize).max(1);
        let k = self.cfg.sferic_k;
        for e in detect_sferic_events(&self.chunk, k, dead) {
            let abs = self.chunk_start + e.index as u64;
            if abs < self.dead_until {
                continue;
            }
            self.dead_until = abs + dead as u64;
            let span = &self.chunk[e.index..e.index + e.len];
            let clip = span.iter().filter(|v| v.abs() >= 1.0).count() as u32;
            let median = (e.threshold / k).max(f32::MIN_POSITIVE);
            let mean = span.iter().map(|v| v.abs()).sum::<f32>() / span.len() as f32;
            let det = Detection {
                id: DetectionId::new(),
                survey_id: self.survey_id,
                time: TimeRange::new(
                    anchor.time_of(abs, fs),
                    anchor.time_of(abs + e.len as u64, fs),
                ),
                f_center_hz: fs / 4.0,
                obw_hz: fs / 2.0,
                xdb_bandwidth_hz: None,
                xdb_level_db: None,
                snr_peak_db: 20.0 * f64::from(e.peak / median).log10(),
                snr_mean_db: 20.0 * f64::from(mean / median).log10(),
                peak_level_dbfs: 20.0 * e.peak.max(f32::MIN_POSITIVE).log10(),
                peak_level_dbm: None,
                sk: None,
                clip_count: clip,
                detector_version: format!(
                    "hk-dsp/vlf-sferic@{};k={k};dead_s={}",
                    env!("CARGO_PKG_VERSION"),
                    self.cfg.sferic_dead_s
                ),
                provenance_ref: prov.id(),
                flags: DetectionFlags {
                    impulsive: true,
                    clipped: clip > 0,
                    ..DetectionFlags::default()
                },
            };
            if self.sferics.len() == self.cfg.max_sferics {
                self.sferics.pop_front();
            }
            self.sferics.push_back(det);
            self.sferic_total += 1;
        }
        self.chunk.clear();
    }

    /// What has been found so far; `include_points` adds every held track point.
    pub fn report(&self, include_points: bool) -> VlfReport {
        let prov = self.provenance.as_ref();
        let carriers = self
            .carriers
            .iter()
            .map(|c| self.carrier_report(c, include_points))
            .collect();
        VlfReport {
            state: self.state,
            error: self.error.clone(),
            device_id: prov.map(|p| p.device_id.clone()),
            accessory: self.accessory.map(AccessoryKind::as_str),
            provenance_ref: prov.map(ProvenanceHandle::id),
            provenance: prov.map(|p| p.get().clone()),
            sample_rate_hz: self.fs,
            phase_disciplined: prov.is_some_and(|p| {
                p.clock_locked
                    && matches!(p.clock_source, ClockSource::Gpsdo | ClockSource::External)
            }),
            window: self
                .first_t
                .zip(self.last_t)
                .map(|(a, b)| TimeRange::new(a, b)),
            samples: self.samples,
            gaps: self.gaps,
            dropped_samples: self.dropped,
            carriers,
            sferics: self.sferics.iter().cloned().collect(),
            sferic_total: self.sferic_total,
        }
    }

    fn carrier_report(&self, c: &Carrier, include_points: bool) -> VlfCarrierReport {
        // The step detectors take seconds-addressed points; the index stands in for the time so
        // each hit maps back to its absolute timestamp exactly.
        let pts: Vec<VlfPoint> = c
            .points
            .iter()
            .enumerate()
            .map(|(i, p)| VlfPoint {
                t_s: i as f64,
                amplitude: p.amplitude,
                phase_rad: p.phase_rad,
            })
            .collect();
        let at = |t_s: f64| c.points[t_s as usize].t_ns;
        let drift = phase_drift_per_point(&pts);
        let refined = c.peak.carrier_hz + drift / (2.0 * std::f64::consts::PI * self.cfg.point_s);
        let path = self
            .cfg
            .paths
            .iter()
            .find(|p| (p.carrier_hz - refined).abs() <= p.tolerance_hz);
        let win = self.cfg.step_window_points;
        VlfCarrierReport {
            carrier_hz: c.peak.carrier_hz,
            carrier_hz_refined: refined,
            snr_db: c.peak.snr_db,
            point_count: c.points.len(),
            points: include_points.then(|| c.points.iter().copied().collect()),
            amplitude_steps: detect_amplitude_steps(&pts, win, self.cfg.min_relative_step)
                .into_iter()
                .map(|s| VlfAmplitudeStep {
                    t_ns: at(s.t_s),
                    relative_change: s.relative_change,
                })
                .collect(),
            phase_steps: detect_phase_steps(&pts, win, self.cfg.min_phase_step_rad)
                .into_iter()
                .map(|s| VlfPhaseStep {
                    t_ns: at(s.t_s),
                    dphi_rad: s.dphi_rad,
                    reflection_height_change_km: path.map(|p| {
                        reflection_height_change_km(
                            s.dphi_rad,
                            refined,
                            p.path_km,
                            p.base_height_km,
                        )
                    }),
                    path_km: path.map(|p| p.path_km),
                })
                .collect(),
        }
    }
}

/// Runs the analyses over a finite source to its end (offline use and tests).
pub fn analyze_source(mut source: Box<dyn Source>, cfg: VlfConfig) -> VlfReport {
    let mut a = VlfAnalyzer::new(cfg);
    let mut buf = Vec::new();
    loop {
        match source.read_block(&mut buf) {
            Ok(Some(h)) => {
                a.push(&h, &buf);
                if a.done() {
                    break;
                }
            }
            Ok(None) => {
                a.finish();
                break;
            }
            Err(e) => {
                a.fail(format!("source: {e}"));
                break;
            }
        }
    }
    a.report(false)
}

/// The analyses running on their own thread over one accessory source, for a live run.
pub struct VlfService {
    analyzer: Arc<Mutex<VlfAnalyzer>>,
    stop: Arc<AtomicBool>,
    control: Arc<dyn SourceControl>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl VlfService {
    /// Starts reading `source` on a `hk-vlf` thread.
    pub fn start(mut source: Box<dyn Source>, cfg: VlfConfig) -> std::io::Result<Arc<Self>> {
        let analyzer = Arc::new(Mutex::new(VlfAnalyzer::new(cfg)));
        let stop = Arc::new(AtomicBool::new(false));
        let control = source.control();
        let (a, s) = (Arc::clone(&analyzer), Arc::clone(&stop));
        let thread = std::thread::Builder::new()
            .name("hk-vlf".into())
            .spawn(move || {
                let mut buf = Vec::new();
                while !s.load(Ordering::SeqCst) {
                    let r = source.read_block(&mut buf);
                    let mut a = a.lock().unwrap_or_else(PoisonError::into_inner);
                    match r {
                        Ok(Some(h)) => a.push(&h, &buf),
                        Ok(None) => a.finish(),
                        Err(e) => a.fail(format!("source: {e}")),
                    }
                    if a.done() {
                        break;
                    }
                }
            })?;
        Ok(Arc::new(Self {
            analyzer,
            stop,
            control,
            thread: Mutex::new(Some(thread)),
        }))
    }

    /// The current report.
    pub fn report(&self, include_points: bool) -> VlfReport {
        self.analyzer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .report(include_points)
    }

    /// The source ended or the service failed.
    pub fn is_done(&self) -> bool {
        self.analyzer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .done()
    }

    /// Waits up to `timeout` for [`Self::is_done`].
    pub fn wait_done(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !self.is_done() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    /// Stops the source and joins the thread.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.control.stop();
        if let Some(t) = self
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = t.join();
        }
    }
}

impl Drop for VlfService {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The run's accessory services (none unless an accessory was attached).
#[derive(Default)]
pub struct VlfServices(Mutex<Vec<Arc<VlfService>>>);

impl VlfServices {
    /// Adds a running service.
    pub fn attach(&self, service: Arc<VlfService>) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(service);
    }

    /// Every service.
    pub fn list(&self) -> Vec<Arc<VlfService>> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Stops every service.
    pub fn stop_all(&self) {
        for s in self.list() {
            s.stop();
        }
    }
}
