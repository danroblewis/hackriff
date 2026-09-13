//! Mock SDR device (T-049): a SigMF recording (real ci8 capture or synthetic cf32/ci8) served
//! behind the generic device contract ([`super`]) as if it were live air seen by a HackRF One.
//! E2E and acceptance tests drive the system through it exactly as they would drive the radio
//! (docs/10 §1.1); `hk serve --device mock:<meta>`, `hk run --source mock:<meta>` and
//! `hackriffd --source mock:<meta>` select it like `hackrf`.
//!
//! # Behaviour model
//!
//! - **Capabilities:** the HackRF One descriptor (1 MHz–6 GHz, LNA/VGA/amp stages, MAX2837
//!   filters, bias tee, 8-bit ci8), driver `mock-sdr`, kind `Hardware` (it streams and retunes like
//!   a radio), receive only (`tx_capable` false), rates from `min(2 Msps, recording rate)` to
//!   `max(20 Msps, recording rate)`. No sweep mode ([`SourceControl::sweep_capability`] is `None`,
//!   `start_sweep` is `Unsupported`, as on the HackRF source today).
//! - **Retune / rate inside coverage:** coverage is the recording's usable band, `centre ±
//!   filter/2` ([`Recording::usable_bandwidth_hz`]: its baseband filter, so the recorded roll-off
//!   never reads as a floor step). It is shifted to the new centre, band-selected and resampled to
//!   the requested rate ([`dsp`]); absolute frequencies are preserved. Tuned to the recording's
//!   own centre and rate the whole recording passes through bit-exact, as the radio captured it.
//! - **Outside coverage (whole or part of the window, or a rate wider than the recording):** the
//!   uncovered spectrum is complex white Gaussian noise at the recording's estimated floor PSD
//!   (Welch median, [`Recording::floor_power`]). The provenance `antenna_port` says what was
//!   served: `mock:recording`, `mock:recording+noise` or `mock:noise` ([`Coverage`]); a coverage
//!   change is a `PROVENANCE_CHANGE`, and [`MockStats::uncovered_samples`] counts noise-filled
//!   output.
//! - **Gain:** output = served IQ × 10^((G − G_rec)/20), G = LNA + VGA + 11 dB when the amp is on
//!   (G_rec from the recording's provenance; unknown gains read as 0 dB), then rounded to int8
//!   with saturation like the HackRF ADC. A block whose clipped components (codes −128/127) exceed
//!   [`MockOptions::overload_clip_fraction`] marks the tune state `overload` (sticky until the next
//!   tune/gain change, a `PROVENANCE_CHANGE`), and a recording captured overloaded stays
//!   overloaded wherever recorded IQ is served. `quantisation_limited` is set when the scaled
//!   floor is under twice the rounding noise.
//! - **Settle:** every accepted control change skips [`MockOptions::settle_blocks`] blocks of
//!   output (like the HackRF's discarded in-flight transfer): a `GAP` with exact `dropped_before`,
//!   counted in `discarded_samples`. Stream time (and the recording) advance through it.
//! - **Losses:** [`MockSdrControl::inject_overrun`] and [`MockOptions::overrun_every`] drop output
//!   as overruns; with real-time pacing, a reader later than [`MockOptions::queue_blocks`] loses
//!   whole blocks as a radio's full queue would; a `core:global_index` gap in the recording is an
//!   overrun of the same duration. Each is a `GAP` counted in `overruns` / `dropped_samples`.
//! - **Pacing:** [`Pacing::RealTime`] (wall clock × speed, never pausable) or [`Pacing::Unpaced`]
//!   (accelerated and lossless: [`Source::pausable`] is `true` only then).
//! - **Time:** block times are an anchor plus the sample counter at the tuned rate (re-anchored on
//!   a rate change). [`MockClock::Recording`] anchors at the recording's first sample time and
//!   keeps its `timestamp_method`; [`MockClock::Wall`] anchors at open, method `synthetic`.
//! - **End:** [`MockEnd::Stop`] ends the stream with the recording; [`MockEnd::Loop`] splices it
//!   again (the first block after the splice carries `GAP` with `dropped_before` 0).
//! - **Bias tee:** a flag (accepted, settles, no effect on samples). **Baseband filter:** recorded
//!   in provenance only.
//! - **Legal class:** the mock carries no class of its own. Callers derive it from the tuned window
//!   exactly as for the live radio (`hk_pipeline::class::band_class`), keeping any stricter class
//!   the recording declares.
//!
//! # Limits (unverified until HIL)
//!
//! Gain is digital scaling of what was recorded: raising it amplifies the recorded quantisation
//! noise and cannot model the front end's noise figure, LNA compression or intermodulation;
//! lowering it cannot undo clipping baked into the recording. The noise fill is white at the
//! floor measured in the recording's central half, so spurs, DC and the baseband-filter roll-off
//! are not synthesised outside coverage, and the floor dips ≈ 3 dB over the transition band at a
//! coverage edge. The band-select filter (≈ 60 dB, transition 8 % of the output rate) adds about
//! half its length in recording samples of latency after a retune. Multi-centre recordings are
//! refused.

mod dsp;

pub use dsp::Coverage;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use hk_model::sigmf::SigmfMeta;
use hk_model::{Provenance, SampleTime, Timestamp, TimestampMethod, Tune};
use num_complex::{Complex, Complex32};

use self::dsp::{Feed, Plan, Render, estimate_floor_power};
use super::sigmf_replay::{Pacing, ReplayOptions, SigmfReplaySource};
use super::{
    BasebandFilters, ControlMailbox, DeviceInfo, Duplex, FrequencyRange, Gains, NamedGain,
    OpenRequest, SampleRates, Source, SourceCapabilities, SourceControl, SourceDriver, SourceError,
    SourceKind, SourceStats,
};
use crate::block::{BlockHeader, Discontinuity, ProvenanceHandle};

/// Driver/model name in errors and capabilities.
pub const NAME: &str = "mock-sdr";
/// Nominal HackRF RF amplifier gain used by the gain model, dB.
pub const AMP_GAIN_DB: f64 = 11.0;

impl Coverage {
    /// The provenance `antenna_port` naming this coverage.
    pub fn antenna_port(self) -> &'static str {
        match self {
            Coverage::Recorded => "mock:recording",
            Coverage::Partial => "mock:recording+noise",
            Coverage::Noise => "mock:noise",
        }
    }

    /// The coverage a mock block's provenance records; `None` for other sources.
    pub fn from_provenance(p: &Provenance) -> Option<Self> {
        [Coverage::Recorded, Coverage::Partial, Coverage::Noise]
            .into_iter()
            .find(|c| p.antenna_port.as_deref() == Some(c.antenna_port()))
    }
}

/// What happens when the recording ends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MockEnd {
    /// The stream ends (`Ok(None)`).
    #[default]
    Stop,
    /// The recording is spliced again; the stream continues.
    Loop,
}

/// Where block times come from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MockClock {
    /// The recording's first-sample time plus the sample counter.
    #[default]
    Recording,
    /// The wall clock at open plus the sample counter (method `synthetic`).
    Wall,
}

/// Mock device options.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MockOptions {
    /// Samples per block (> 0; HackRF transfers are 131 072).
    pub block_len: usize,
    /// Real time (not pausable) or accelerated (pausable).
    pub pacing: Pacing,
    /// End of recording behaviour.
    pub end: MockEnd,
    /// Time base.
    pub clock: MockClock,
    /// Blocks of output skipped after each control change.
    pub settle_blocks: u32,
    /// Clipped fraction of a block's I/Q components that marks overload.
    pub overload_clip_fraction: f64,
    /// Real-time pacing: blocks a reader may fall behind before whole blocks are dropped.
    pub queue_blocks: usize,
    /// Band-select transition width as a fraction of the output rate (0 < t ≤ 0.25).
    pub transition: f64,
    /// Noise seed (deterministic output).
    pub seed: u64,
    /// Injects an overrun of `.1` samples every `.0` blocks.
    pub overrun_every: Option<(u64, u64)>,
}

impl Default for MockOptions {
    fn default() -> Self {
        Self {
            block_len: 65_536,
            pacing: Pacing::Unpaced,
            end: MockEnd::Stop,
            clock: MockClock::Recording,
            settle_blocks: 1,
            overload_clip_fraction: 1e-3,
            queue_blocks: 64,
            transition: 0.08,
            seed: 0x6d6f_636b_5344_5221,
            overrun_every: None,
        }
    }
}

impl MockOptions {
    fn validate(&self) -> Result<(), SourceError> {
        let bad = |what, value| Err(SourceError::OutOfRange { what, value });
        if self.block_len == 0 {
            return bad("mock block_len", 0.0);
        }
        if let Pacing::RealTime { speed } = self.pacing {
            if !(speed.is_finite() && speed > 0.0) {
                return bad("mock pacing speed", speed);
            }
        }
        if !(self.transition > 0.0 && self.transition <= 0.25) {
            return bad("mock transition", self.transition);
        }
        if !(0.0..=1.0).contains(&self.overload_clip_fraction) {
            return bad("mock overload clip fraction", self.overload_clip_fraction);
        }
        if matches!(self.overrun_every, Some((0, _))) {
            return bad("mock overrun_every blocks", 0.0);
        }
        Ok(())
    }
}

/// The recording behind a mock device, resolved at open.
#[derive(Clone, Debug)]
pub struct Recording {
    /// `.sigmf-meta` path.
    pub path: PathBuf,
    /// Metadata (as read; the mock never interprets annotations).
    pub meta: SigmfMeta,
    /// Recorded centre, Hz.
    pub center_hz: f64,
    /// Recorded rate, Hz.
    pub sample_rate_hz: f64,
    /// The recording's provenance (replay rules: capture, global or synthesised).
    pub provenance: Provenance,
    /// Time of the first recorded sample.
    pub start_time: Timestamp,
    /// Estimated floor power, full scale² per sample at the recording rate.
    pub floor_power: f64,
}

/// Rounding noise of an 8-bit ADC, full scale² per complex sample (1/6 code²).
const QUANTISATION_POWER: f64 = 1.0 / 6.0 / (128.0 * 128.0);

impl Recording {
    /// Reads the metadata, checks the recording has one centre, and estimates its floor.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SourceError> {
        let path = path.as_ref().to_path_buf();
        let mut replay = SigmfReplaySource::open(
            &path,
            ReplayOptions {
                block_len: 65_536,
                pacing: Pacing::Unpaced,
            },
        )?;
        let meta = replay.meta().clone();
        let sample_rate_hz = replay.sample_rate_hz();
        let mut samples = Vec::new();
        let mut buf = Vec::new();
        let mut first: Option<BlockHeader> = None;
        while samples.len() < 64 * 1024 {
            let Some(h) = replay.read_block(&mut buf)? else {
                break;
            };
            if let Some(f) = &first {
                if f.provenance.tune.center_hz != h.provenance.tune.center_hz {
                    return Err(SourceError::InvalidRecording(
                        "the mock SDR needs a single-centre recording".into(),
                    ));
                }
            }
            first.get_or_insert(h);
            samples.extend_from_slice(&buf);
        }
        let first = first.ok_or_else(|| {
            SourceError::InvalidRecording("the mock SDR needs a non-empty recording".into())
        })?;
        let centres: Vec<f64> = meta.captures.iter().filter_map(|c| c.frequency).collect();
        if centres.windows(2).any(|w| w[0] != w[1]) {
            return Err(SourceError::InvalidRecording(
                "the mock SDR needs a single-centre recording".into(),
            ));
        }
        let floor_power = estimate_floor_power(&samples)
            .filter(|p| p.is_finite() && *p > 0.0)
            .unwrap_or(QUANTISATION_POWER);
        Ok(Self {
            path,
            meta,
            center_hz: first.provenance.tune.center_hz,
            sample_rate_hz,
            provenance: first.provenance.get().clone(),
            start_time: first.time.host_time,
            floor_power,
        })
    }

    /// Total front-end gain the recording was made with, dB (the gain model's reference).
    pub fn gain_db(&self) -> f64 {
        total_gain(&self.provenance.tune)
    }

    /// The recorded bandwidth a retuned window may use, Hz: the recording's baseband filter
    /// (`tune.bandwidth_hz`) when narrower than its rate, else the rate. Beyond the filter the
    /// recorded floor rolls off, which would read as a floor step next to the noise fill.
    pub fn usable_bandwidth_hz(&self) -> f64 {
        let bw = self.provenance.tune.bandwidth_hz;
        if bw.is_finite() && bw > 0.0 && bw < self.sample_rate_hz {
            bw
        } else {
            self.sample_rate_hz
        }
    }

    /// The recorded band.
    pub fn band(&self) -> FrequencyRange {
        FrequencyRange {
            min_hz: self.center_hz - self.sample_rate_hz / 2.0,
            max_hz: self.center_hz + self.sample_rate_hz / 2.0,
        }
    }

    /// Floor PSD, dBFS/Hz (full scale = 1.0 normalised).
    pub fn floor_dbfs_per_hz(&self) -> f64 {
        10.0 * (self.floor_power / self.sample_rate_hz).log10()
    }
}

fn total_gain(t: &Tune) -> f64 {
    t.lna_db + t.vga_db + if t.amp_on { AMP_GAIN_DB } else { 0.0 }
}

/// The mock's capability descriptor for `recording`.
pub fn mock_capabilities(recording: &Recording) -> SourceCapabilities {
    let mut caps = SourceCapabilities::hackrf_one();
    let band = recording.band();
    caps.driver = NAME.into();
    caps.kind = SourceKind::Hardware;
    caps.frequency_ranges = vec![FrequencyRange {
        min_hz: band.min_hz.min(1e6),
        max_hz: band.max_hz.max(6e9),
    }];
    caps.sample_rates = SampleRates::Continuous {
        min_hz: recording.sample_rate_hz.min(2e6),
        max_hz: recording.sample_rate_hz.max(20e6),
    };
    caps.tx_capable = false;
    caps.duplex = Duplex::ReceiveOnly;
    caps
}

/// The widest selectable baseband filter at or below `0.75 · rate` (else the narrowest).
fn default_filter(caps: &SourceCapabilities, rate: f64) -> f64 {
    match &caps.baseband_filter {
        Some(BasebandFilters::Discrete(v)) => v
            .iter()
            .copied()
            .filter(|w| *w <= 0.75 * rate)
            .fold(None, |m: Option<f64>, w| Some(m.map_or(w, |m| m.max(w))))
            .unwrap_or_else(|| v.iter().copied().fold(f64::INFINITY, f64::min)),
        Some(BasebandFilters::Continuous { min_hz, max_hz }) => {
            (0.75 * rate).clamp(*min_hz, *max_hz)
        }
        None => rate,
    }
}

fn checked(ok: bool, what: &'static str, value: f64) -> Result<f64, SourceError> {
    if ok && value.is_finite() {
        Ok(value)
    } else {
        Err(SourceError::OutOfRange { what, value })
    }
}

fn quantised_stage(caps: &SourceCapabilities, stage: &str, db: f64) -> Result<f64, SourceError> {
    caps.gain_stage(stage)
        .and_then(|s| s.quantise(db))
        .ok_or(SourceError::OutOfRange {
            what: "named gain (stage lna, vga or amp)",
            value: db,
        })
}

fn merge_gain(g: &mut Gains, stage: &str, db: f64) {
    match stage {
        "lna" => g.lna_db = db,
        "vga" => g.vga_db = db,
        _ => g.amp_on = db > 0.0,
    }
}

/// Opens mock devices over one recording.
pub struct MockSdrDriver {
    recording: Arc<Recording>,
    options: MockOptions,
    capabilities: SourceCapabilities,
    last: Mutex<Option<Arc<MockSdrControl>>>,
}

impl MockSdrDriver {
    /// A driver replaying `meta_path` with `options`.
    pub fn new(meta_path: impl AsRef<Path>, options: MockOptions) -> Result<Self, SourceError> {
        options.validate()?;
        let recording = Recording::open(meta_path)?;
        let capabilities = mock_capabilities(&recording);
        Ok(Self {
            recording: Arc::new(recording),
            options,
            capabilities,
            last: Mutex::new(None),
        })
    }

    /// The recording.
    pub fn recording(&self) -> &Recording {
        &self.recording
    }

    /// The options.
    pub fn options(&self) -> &MockOptions {
        &self.options
    }

    /// The device's power-on request: the recording's centre, rate and gains.
    pub fn default_request(&self) -> OpenRequest {
        let t = &self.recording.provenance.tune;
        let caps = &self.capabilities;
        let gains = [("lna", t.lna_db), ("vga", t.vga_db)]
            .into_iter()
            .chain([("amp", if t.amp_on { AMP_GAIN_DB } else { 0.0 })])
            .filter_map(|(s, db)| {
                let stage = caps.gain_stage(s)?;
                Some(NamedGain::new(s, db.clamp(stage.min_db, stage.max_db)))
            })
            .collect();
        OpenRequest {
            device: None,
            center_hz: self.recording.center_hz,
            sample_rate_hz: self.recording.sample_rate_hz,
            gains,
            baseband_filter_hz: None,
            bias_tee: false,
        }
    }

    /// The control handle of the most recently opened source (tests inject overruns through it).
    pub fn last_control(&self) -> Option<Arc<MockSdrControl>> {
        self.last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Opens a mock source configured by `request` (validated like the HackRF's).
    pub fn open_mock(&self, request: &OpenRequest) -> Result<MockSdrSource, SourceError> {
        let caps = &self.capabilities;
        let rec = &self.recording;
        let center = checked(
            caps.supports_frequency(request.center_hz),
            "centre frequency (Hz)",
            request.center_hz,
        )?;
        let rate = checked(
            caps.sample_rates.supports(request.sample_rate_hz),
            "sample rate (Hz)",
            request.sample_rate_hz,
        )?;
        let t = &rec.provenance.tune;
        let mut gains = Gains {
            lna_db: t.lna_db,
            vga_db: t.vga_db,
            amp_on: t.amp_on,
        };
        for g in &request.gains {
            let db = quantised_stage(caps, &g.stage, g.db)?;
            merge_gain(&mut gains, &g.stage, db);
        }
        let filter = match request.baseband_filter_hz {
            Some(hz) => checked(
                caps.baseband_filter
                    .as_ref()
                    .is_some_and(|f| f.supports(hz)),
                "baseband filter bandwidth (Hz)",
                hz,
            )?,
            None => default_filter(caps, rate),
        };
        let tune = Tune {
            center_hz: center,
            sample_rate_hz: rate,
            lna_db: gains.lna_db,
            vga_db: gains.vga_db,
            amp_on: gains.amp_on,
            bandwidth_hz: filter,
        };
        let file = rec
            .path
            .file_name()
            .map_or_else(String::new, |f| f.to_string_lossy().into_owned());
        let device = DeviceInfo {
            driver: NAME.into(),
            device_id: format!("mock:{}", rec.provenance.device_id),
            hw: format!(
                "mock SDR (HackRF One model) replaying {file} (recorded by {}), antenna recording",
                rec.meta.global.hw.as_deref().unwrap_or("unknown hardware")
            ),
        };
        let control = Arc::new(MockSdrControl {
            capabilities: caps.clone(),
            mailbox: ControlMailbox::new(),
            stopped: AtomicBool::new(false),
            requested: Mutex::new(gains),
            device,
            counters: MockCounters::default(),
            injected: Mutex::new(Vec::new()),
        });
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&control));
        MockSdrSource::new(
            Arc::clone(&self.recording),
            self.options,
            control,
            tune,
            request.baseband_filter_hz.is_some(),
            request.bias_tee,
        )
    }
}

impl SourceDriver for MockSdrDriver {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn available(&self) -> bool {
        true
    }

    fn capabilities(&self) -> SourceCapabilities {
        self.capabilities.clone()
    }

    fn open(&self, request: &OpenRequest) -> Result<Box<dyn Source>, SourceError> {
        Ok(Box::new(self.open_mock(request)?))
    }
}

#[derive(Debug, Default)]
struct MockCounters {
    blocks: AtomicU64,
    samples: AtomicU64,
    overruns: AtomicU64,
    dropped: AtomicU64,
    discarded: AtomicU64,
    clipped: AtomicU64,
    overload_blocks: AtomicU64,
    uncovered: AtomicU64,
    loops: AtomicU64,
    control_changes: AtomicU64,
}

/// Mock-specific counters on top of [`SourceStats`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MockStats {
    /// The generic stream counters.
    pub source: SourceStats,
    /// Clipped I/Q components after the gain model.
    pub clipped_components: u64,
    /// Blocks over the overload clip fraction.
    pub overload_blocks: u64,
    /// Samples delivered under partial or no coverage (noise-filled).
    pub uncovered_samples: u64,
    /// Times the recording was spliced again ([`MockEnd::Loop`]).
    pub loops: u64,
    /// Control changes applied.
    pub control_changes: u64,
}

/// The mock's control handle: validates like the HackRF's, applies at the next block boundary.
pub struct MockSdrControl {
    capabilities: SourceCapabilities,
    mailbox: ControlMailbox,
    stopped: AtomicBool,
    requested: Mutex<Gains>,
    device: DeviceInfo,
    counters: MockCounters,
    injected: Mutex<Vec<u64>>,
}

impl MockSdrControl {
    /// Drops `samples` output samples as a device overrun at the next block boundary.
    pub fn inject_overrun(&self, samples: u64) {
        if samples > 0 {
            self.injected
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(samples);
        }
    }

    /// Counters, including the mock-specific ones.
    pub fn mock_stats(&self) -> MockStats {
        let c = &self.counters;
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        MockStats {
            source: SourceStats {
                blocks: g(&c.blocks),
                samples: g(&c.samples),
                overruns: g(&c.overruns),
                dropped_samples: g(&c.dropped),
                discarded_samples: g(&c.discarded),
            },
            clipped_components: g(&c.clipped),
            overload_blocks: g(&c.overload_blocks),
            uncovered_samples: g(&c.uncovered),
            loops: g(&c.loops),
            control_changes: g(&c.control_changes),
        }
    }

    fn gains_locked(&self) -> std::sync::MutexGuard<'_, Gains> {
        self.requested
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl SourceControl for MockSdrControl {
    fn capabilities(&self) -> &SourceCapabilities {
        &self.capabilities
    }

    fn tune(&self, center_hz: f64) -> Result<(), SourceError> {
        let hz = checked(
            self.capabilities.supports_frequency(center_hz),
            "centre frequency (Hz)",
            center_hz,
        )?
        .round();
        self.mailbox.post(|p| p.center_hz = Some(hz));
        Ok(())
    }

    fn set_sample_rate(&self, sample_rate_hz: f64) -> Result<(), SourceError> {
        let hz = checked(
            self.capabilities.sample_rates.supports(sample_rate_hz),
            "sample rate (Hz)",
            sample_rate_hz,
        )?;
        self.mailbox.post(|p| p.sample_rate_hz = Some(hz));
        Ok(())
    }

    fn set_gains(&self, gains: &Gains) -> Result<(), SourceError> {
        let g = Gains {
            lna_db: quantised_stage(&self.capabilities, "lna", gains.lna_db)?,
            vga_db: quantised_stage(&self.capabilities, "vga", gains.vga_db)?,
            amp_on: gains.amp_on,
        };
        *self.gains_locked() = g;
        self.mailbox.post(|p| p.gains = Some(g));
        Ok(())
    }

    fn set_gain(&self, stage: &str, db: f64) -> Result<(), SourceError> {
        let q = quantised_stage(&self.capabilities, stage, db)?;
        let mut requested = self.gains_locked();
        merge_gain(&mut requested, stage, q);
        let g = *requested;
        self.mailbox.post(|p| p.gains = Some(g));
        Ok(())
    }

    fn set_baseband_filter(&self, bandwidth_hz: f64) -> Result<(), SourceError> {
        let ok = self
            .capabilities
            .baseband_filter
            .as_ref()
            .is_some_and(|f| f.supports(bandwidth_hz));
        let hz = checked(ok, "baseband filter bandwidth (Hz)", bandwidth_hz)?;
        self.mailbox.post(|p| p.baseband_filter_hz = Some(hz));
        Ok(())
    }

    fn set_bias_tee(&self, enabled: bool) -> Result<(), SourceError> {
        self.mailbox.post(|p| p.bias_tee = Some(enabled));
        Ok(())
    }

    fn start(&self) -> Result<(), SourceError> {
        Ok(())
    }

    fn stop(&self) -> Result<(), SourceError> {
        self.stopped.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn stats(&self) -> Option<SourceStats> {
        Some(self.mock_stats().source)
    }

    fn device_info(&self) -> Option<DeviceInfo> {
        Some(self.device.clone())
    }
}

/// Recording samples for the renderer, spliced again on [`MockEnd::Loop`].
struct ReplayFeed {
    path: PathBuf,
    looping: bool,
    inner: SigmfReplaySource,
    buf: Vec<Complex32>,
    first: bool,
    /// A splice happened since the last block.
    wrapped: bool,
    /// Recording samples lost to `core:global_index` gaps since the last block.
    gap_samples: u64,
}

impl ReplayFeed {
    fn open(path: &Path) -> Result<SigmfReplaySource, SourceError> {
        SigmfReplaySource::open(
            path,
            ReplayOptions {
                block_len: 65_536,
                pacing: Pacing::Unpaced,
            },
        )
    }
}

impl Feed for ReplayFeed {
    fn fill(&mut self, dst: &mut Vec<Complex32>) -> Result<bool, SourceError> {
        let mut reopened = false;
        loop {
            match self.inner.read_block(&mut self.buf)? {
                Some(h) => {
                    if !self.first {
                        self.gap_samples += h.dropped_before;
                    }
                    self.first = false;
                    dst.extend_from_slice(&self.buf);
                    return Ok(true);
                }
                None if self.looping && !reopened => {
                    self.inner = Self::open(&self.path)?;
                    self.first = true;
                    self.wrapped = true;
                    reopened = true;
                }
                None => return Ok(false),
            }
        }
    }
}

/// The mock stream. See the [module docs](self).
pub struct MockSdrSource {
    control: Arc<MockSdrControl>,
    recording: Arc<Recording>,
    options: MockOptions,
    feed: ReplayFeed,
    render: Render,
    tune: Tune,
    bias_tee: bool,
    filter_explicit: bool,
    overloaded: bool,
    provenance: ProvenanceHandle,
    pending_flags: Discontinuity,
    mailbox_seen: u64,
    next_index: u64,
    anchor: SampleTime,
    f32buf: Vec<Complex32>,
    pace_origin: Option<Instant>,
    /// Pacing: stream seconds (× speed) of the next sample.
    due_s: f64,
    blocks_since_overrun: u64,
    finished: bool,
}

impl MockSdrSource {
    fn new(
        recording: Arc<Recording>,
        options: MockOptions,
        control: Arc<MockSdrControl>,
        tune: Tune,
        filter_explicit: bool,
        bias_tee: bool,
    ) -> Result<Self, SourceError> {
        let feed = ReplayFeed {
            path: recording.path.clone(),
            looping: options.end == MockEnd::Loop,
            inner: ReplayFeed::open(&recording.path)?,
            buf: Vec::new(),
            first: true,
            wrapped: false,
            gap_samples: 0,
        };
        let anchor = SampleTime {
            sample_index: 0,
            host_time: match options.clock {
                MockClock::Recording => recording.start_time,
                MockClock::Wall => Timestamp::now(),
            },
        };
        let plan = plan_for(&recording, &tune, options.transition);
        let mut source = Self {
            render: Render::new(plan, options.seed),
            provenance: ProvenanceHandle::new(recording.provenance.clone()),
            control,
            recording,
            options,
            feed,
            tune,
            bias_tee,
            filter_explicit,
            overloaded: false,
            pending_flags: Discontinuity::STREAM_START,
            mailbox_seen: 0,
            next_index: 0,
            anchor,
            f32buf: Vec::new(),
            pace_origin: None,
            due_s: 0.0,
            blocks_since_overrun: 0,
            finished: false,
        };
        source.provenance = ProvenanceHandle::new(source.record());
        Ok(source)
    }

    /// The recording.
    pub fn recording(&self) -> &Recording {
        &self.recording
    }

    /// The concrete control handle (overrun injection, mock stats).
    pub fn mock_control(&self) -> Arc<MockSdrControl> {
        Arc::clone(&self.control)
    }

    /// The provenance in force for the next block (before pending changes).
    pub fn provenance(&self) -> ProvenanceHandle {
        self.provenance.clone()
    }

    /// Time of the stream's first sample.
    pub fn start_time(&self) -> Timestamp {
        self.anchor.time_of(0, self.tune.sample_rate_hz)
    }

    /// The bias tee is on (a flag only).
    pub fn bias_tee(&self) -> bool {
        self.bias_tee
    }

    fn gain_scale(&self) -> f64 {
        10f64.powf((total_gain(&self.tune) - self.recording.gain_db()) / 20.0)
    }

    fn record(&self) -> Provenance {
        let rec = &self.recording.provenance;
        let coverage = self.render.plan().coverage();
        let g = self.gain_scale();
        let floor_psd = self.recording.floor_power * g * g / self.recording.sample_rate_hz;
        let quant_psd = QUANTISATION_POWER / self.tune.sample_rate_hz;
        let (method, budget) = match self.options.clock {
            MockClock::Recording => (rec.timestamp_method, rec.timestamp_error_budget_ns),
            MockClock::Wall => (TimestampMethod::Synthetic, None),
        };
        Provenance {
            device_id: self.control.device.device_id.clone(),
            tune: self.tune.clone(),
            overload: self.overloaded || (rec.overload && coverage != Coverage::Noise),
            quantisation_limited: floor_psd < 2.0 * quant_psd
                || (rec.quantisation_limited && g >= 1.0),
            temperature_c: None,
            antenna_port: Some(coverage.antenna_port().into()),
            clock_source: rec.clock_source,
            clock_locked: rec.clock_locked,
            calibration_state_ref: None,
            spur_mask_ref: None,
            timestamp_method: method,
            timestamp_error_budget_ns: budget,
        }
    }

    fn mint(&mut self) -> Discontinuity {
        let prev = self.provenance.clone();
        self.provenance = ProvenanceHandle::new(self.record());
        Discontinuity::between(prev.get(), self.provenance.get())
    }

    /// Applies posted controls; returns output samples to skip for settling.
    fn apply_pending(&mut self) -> u64 {
        let Some(change) = self.control.mailbox.take(&mut self.mailbox_seen) else {
            return 0;
        };
        let before = self.tune.clone();
        if let Some(hz) = change.sample_rate_hz {
            self.tune.sample_rate_hz = hz;
            if !self.filter_explicit {
                self.tune.bandwidth_hz = default_filter(&self.control.capabilities, hz);
            }
        }
        if let Some(bw) = change.baseband_filter_hz {
            self.tune.bandwidth_hz = bw;
            self.filter_explicit = true;
        }
        if let Some(g) = change.gains {
            self.tune.lna_db = g.lna_db;
            self.tune.vga_db = g.vga_db;
            self.tune.amp_on = g.amp_on;
        }
        if let Some(hz) = change.center_hz {
            self.tune.center_hz = hz;
        }
        if let Some(on) = change.bias_tee {
            self.bias_tee = on;
        }
        self.control
            .counters
            .control_changes
            .fetch_add(1, Ordering::Relaxed);
        if self.tune != before {
            if self.tune.sample_rate_hz != before.sample_rate_hz {
                self.anchor = SampleTime {
                    sample_index: self.next_index,
                    host_time: self.anchor.time_of(self.next_index, before.sample_rate_hz),
                };
            }
            self.overloaded = false;
            self.render.retarget(plan_for(
                &self.recording,
                &self.tune,
                self.options.transition,
            ));
            let flags = self.mint();
            self.pending_flags |= flags;
        }
        u64::from(self.options.settle_blocks) * self.options.block_len as u64
    }

    fn speed(&self) -> f64 {
        match self.options.pacing {
            Pacing::RealTime { speed } => speed,
            Pacing::Unpaced => 1.0,
        }
    }

    /// Renders the next block into `out` (int8, gain applied).
    fn next_block(
        &mut self,
        out: &mut Vec<Complex<i8>>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        out.clear();
        if self.control.stopped.load(Ordering::SeqCst) || self.finished {
            return Ok(None);
        }
        let control = Arc::clone(&self.control);
        let c = &control.counters;
        let block = self.options.block_len;
        let mut gap = 0u64;

        // Skipped output: settle, injected and periodic overruns, a late real-time reader.
        let settle = self.apply_pending();
        let fs = self.tune.sample_rate_hz;
        if settle > 0 {
            self.render.skip(settle);
            gap += settle;
            c.discarded.fetch_add(settle, Ordering::Relaxed);
        }
        let mut losses: Vec<u64> = std::mem::take(
            &mut *control
                .injected
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        if let Some((every, n)) = self.options.overrun_every {
            self.blocks_since_overrun += 1;
            if self.blocks_since_overrun >= every {
                self.blocks_since_overrun = 0;
                losses.push(n);
            }
        }
        if let Pacing::RealTime { speed } = self.options.pacing {
            let origin = *self.pace_origin.get_or_insert_with(Instant::now);
            let queue_s = (self.options.queue_blocks * block) as f64 / (fs * speed);
            let lag_s = origin.elapsed().as_secs_f64() - self.due_s;
            if lag_s > queue_s {
                let blocks = ((lag_s - queue_s) * fs * speed / block as f64).ceil() as u64;
                losses.push(blocks.max(1) * block as u64);
            }
        }
        for n in losses.into_iter().filter(|n| *n > 0) {
            self.render.skip(n);
            gap += n;
            c.overruns.fetch_add(1, Ordering::Relaxed);
            c.dropped.fetch_add(n, Ordering::Relaxed);
        }
        // A gap in the recording: the counter jumps, the content is spliced.
        if self.feed.gap_samples > 0 {
            let n = ((self.feed.gap_samples as f64 * fs / self.recording.sample_rate_hz).round()
                as u64)
                .max(1);
            self.feed.gap_samples = 0;
            gap += n;
            c.overruns.fetch_add(1, Ordering::Relaxed);
            c.dropped.fetch_add(n, Ordering::Relaxed);
        }
        self.next_index += gap;
        self.due_s += gap as f64 / (fs * self.speed());

        self.f32buf.clear();
        let made = self
            .render
            .render(&mut self.feed, &mut self.f32buf, block)?;
        if made == 0 {
            self.finished = true;
            return Ok(None);
        }
        let g = self.gain_scale() as f32 * 128.0;
        let mut clipped = 0u64;
        out.extend(self.f32buf.iter().map(|z| {
            let q = |x: f32| (x * g).round().clamp(-128.0, 127.0) as i8;
            let s = Complex::new(q(z.re), q(z.im));
            clipped +=
                u64::from(s.re == -128 || s.re == 127) + u64::from(s.im == -128 || s.im == 127);
            s
        }));
        c.clipped.fetch_add(clipped, Ordering::Relaxed);
        let mut flags = std::mem::replace(&mut self.pending_flags, Discontinuity::NONE);
        if gap > 0 {
            flags |= Discontinuity::GAP;
        }
        if std::mem::take(&mut self.feed.wrapped) {
            flags |= Discontinuity::GAP;
            c.loops.fetch_add(1, Ordering::Relaxed);
        }
        if clipped as f64 > self.options.overload_clip_fraction * (2 * made) as f64 {
            c.overload_blocks.fetch_add(1, Ordering::Relaxed);
            if !self.overloaded {
                self.overloaded = true;
                flags |= self.mint();
            }
        }
        if self.render.plan().coverage() != Coverage::Recorded {
            c.uncovered.fetch_add(made as u64, Ordering::Relaxed);
        }
        let header = BlockHeader {
            time: SampleTime {
                sample_index: self.next_index,
                host_time: self.anchor.time_of(self.next_index, fs),
            },
            provenance: self.provenance.clone(),
            discontinuity: flags,
            dropped_before: gap,
        };
        self.next_index += made as u64;
        c.blocks.fetch_add(1, Ordering::Relaxed);
        c.samples.fetch_add(made as u64, Ordering::Relaxed);
        if let Pacing::RealTime { speed } = self.options.pacing {
            self.due_s += made as f64 / (fs * speed);
            let origin = *self.pace_origin.get_or_insert_with(Instant::now);
            let due = Duration::from_secs_f64(self.due_s);
            let elapsed = origin.elapsed();
            if due > elapsed {
                std::thread::sleep(due - elapsed);
            }
        }
        Ok(Some(header))
    }
}

fn plan_for(recording: &Recording, tune: &Tune, transition: f64) -> Plan {
    Plan {
        rec_center_hz: recording.center_hz,
        rec_rate_hz: recording.sample_rate_hz,
        rec_usable_hz: recording.usable_bandwidth_hz(),
        center_hz: tune.center_hz,
        rate_hz: tune.sample_rate_hz,
        floor_power: recording.floor_power,
        transition,
    }
}

impl Source for MockSdrSource {
    fn capabilities(&self) -> &SourceCapabilities {
        &self.control.capabilities
    }

    fn control(&self) -> Arc<dyn SourceControl> {
        self.control.clone()
    }

    /// Only accelerated (unpaced) replay can wait for a reader; real-time pacing drops like a
    /// radio.
    fn pausable(&self) -> bool {
        matches!(self.options.pacing, Pacing::Unpaced)
    }

    fn read_block(
        &mut self,
        samples: &mut Vec<Complex32>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        samples.clear();
        let mut ci8 = Vec::with_capacity(self.options.block_len);
        let h = self.next_block(&mut ci8)?;
        samples.extend(
            ci8.iter()
                .map(|s| Complex32::new(f32::from(s.re) / 128.0, f32::from(s.im) / 128.0)),
        );
        Ok(h)
    }

    fn read_block_ci8(
        &mut self,
        samples: &mut Vec<Complex<i8>>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        self.next_block(samples)
    }
}
