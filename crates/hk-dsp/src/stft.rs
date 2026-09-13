//! Streaming STFT: [`SpectrumFrame`]s from a stream of sample blocks or ring-reader chunks.
//!
//! [`StftProcessor::push`] takes any run of contiguous samples ([`SampleBlock`] samples, a
//! [`ReadChunk`] from a ring reader, `f32` or native `i8`) with its [`InputInfo`]. Segments carry
//! over block boundaries, so output is bit-identical however the stream is chopped. Every `K`
//! (`averages`) segments it emits one frame, timestamped with the [`SampleTime`] of the frame's
//! first sample.
//!
//! **Resets.** Averaging (and the partial segment) restarts, discarding the partial frame, when:
//! - an input carries a [`Discontinuity`] flag in [`StftConfig::reset_on`] (default: stream
//!   start, retune, rate change, gain change, gap);
//! - `dropped_before > 0`, or the input's first sample index is not the one expected (a ring
//!   overrun or source gap: flagged GAP even when the chunk carries no flag);
//! - the provenance's centre, rate or gains change (detected by comparing records, so chunks
//!   that start mid-block are covered).
//!
//! A reset for anything but a pure gap also clears the persistence image. The next frame's
//! [`SpectrumFrame::discontinuity`] carries the reasons and [`SpectrumFrame::dropped_samples`]
//! the loss.
//!
//! **Real-time path:** `push` allocates nothing once the first frame exists (tested under a
//! counting allocator); per-segment work is window + FFT + O(bins) accumulation.

use hk_core::{BlockHeader, Discontinuity, ProvenanceHandle, ReadChunk, SampleBlock};
use hk_model::SampleTime;
use num_complex::{Complex, Complex32};

use crate::fft::FftBackend;
use crate::persistence::{Persistence, PersistenceConfig};
use crate::spectrum::Spectrum;
use crate::welch::{ConfigError, SegmentEngine, WelchConfig};

/// A complex sample type the STFT accepts, normalised to full scale 1 on conversion.
pub trait IqSample: Copy + Send + Sync + 'static {
    /// The sample as `Complex32` in [−1, 1).
    fn to_complex32(self) -> Complex32;
}

impl IqSample for Complex32 {
    #[inline]
    fn to_complex32(self) -> Complex32 {
        self
    }
}

impl IqSample for Complex<i8> {
    /// `x / 128`, matching hk-core's ci8 normalisation.
    #[inline]
    fn to_complex32(self) -> Complex32 {
        const INV: f32 = 1.0 / 128.0;
        Complex32::new(f32::from(self.re) * INV, f32::from(self.im) * INV)
    }
}

/// Metadata for a run of contiguous input samples.
#[derive(Clone, Copy, Debug)]
pub struct InputInfo<'a> {
    /// Time of the first sample; `sample_index` is the stream counter.
    pub time: SampleTime,
    /// Discontinuity flags (normally set on a block's first samples only).
    pub discontinuity: Discontinuity,
    /// Samples dropped immediately before these.
    pub dropped_before: u64,
    /// Provenance in force for these samples.
    pub provenance: &'a ProvenanceHandle,
}

impl<'a> From<&'a BlockHeader> for InputInfo<'a> {
    fn from(h: &'a BlockHeader) -> Self {
        Self {
            time: h.time,
            discontinuity: h.discontinuity,
            dropped_before: h.dropped_before,
            provenance: &h.provenance,
        }
    }
}

impl<'a> From<&'a SampleBlock> for InputInfo<'a> {
    fn from(b: &'a SampleBlock) -> Self {
        Self::from(&b.header)
    }
}

impl<'a> From<&'a ReadChunk> for InputInfo<'a> {
    fn from(c: &'a ReadChunk) -> Self {
        Self {
            time: c.time,
            discontinuity: c.discontinuity,
            dropped_before: c.dropped_before,
            provenance: &c.provenance,
        }
    }
}

/// STFT settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StftConfig {
    /// Per-segment settings (FFT length, overlap, window, holds, SK).
    pub welch: WelchConfig,
    /// Segments per frame `K` (>= 1; >= 2 with SK). Also the SK `M`.
    pub averages: usize,
    /// Optional persistence image, updated every segment.
    pub persistence: Option<PersistenceConfig>,
    /// Discontinuity flags that reset averaging.
    pub reset_on: Discontinuity,
}

/// Flags that reset averaging by default.
pub const DEFAULT_RESET_ON: Discontinuity = Discontinuity::from_bits_truncate(
    Discontinuity::STREAM_START.bits()
        | Discontinuity::RETUNE.bits()
        | Discontinuity::RATE_CHANGE.bits()
        | Discontinuity::GAIN_CHANGE.bits()
        | Discontinuity::GAP.bits(),
);

impl Default for StftConfig {
    /// 4096 bins, Hann, 50% overlap, K = 16, holds and SK on, no persistence.
    fn default() -> Self {
        Self::new(WelchConfig::default(), 16)
    }
}

impl StftConfig {
    /// `welch` segments averaged `averages` at a time, no persistence, default resets.
    pub fn new(welch: WelchConfig, averages: usize) -> Self {
        Self {
            welch,
            averages,
            persistence: None,
            reset_on: DEFAULT_RESET_ON,
        }
    }

    /// The smallest power-of-two FFT whose bins are no wider than `max_bin_hz` at
    /// `sample_rate_hz`, with defaults otherwise. E.g. 20 Msps and 1 kHz gives 32768 bins
    /// (610 Hz).
    pub fn for_bin_width(sample_rate_hz: f64, max_bin_hz: f64, averages: usize) -> Self {
        let n = ((sample_rate_hz / max_bin_hz).ceil() as usize)
            .max(4)
            .next_power_of_two();
        Self::new(WelchConfig::new(n), averages)
    }

    /// Frame duration in samples: `(K − 1)·hop + N`.
    pub fn frame_samples(&self) -> u64 {
        (self.averages.saturating_sub(1) * self.welch.hop() + self.welch.fft_len) as u64
    }

    /// Checks the settings.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.welch.validate()?;
        if self.averages == 0 {
            return Err(ConfigError::ZeroAverages);
        }
        if self.welch.spectral_kurtosis && self.averages < 2 {
            return Err(ConfigError::SkNeedsTwoAverages);
        }
        if let Some(p) = &self.persistence {
            p.validate().map_err(ConfigError::Persistence)?;
        }
        Ok(())
    }
}

/// One STFT output frame: a [`Spectrum`] plus time, provenance and discontinuity context. Maps
/// onto docs/07 §2.4 SpectrumFrame (`t`, `f_center`, `span`, `bins`, `psd[]`, `sk[]`,
/// `provenance_ref`); persistence is read from [`StftProcessor::persistence`].
#[derive(Clone, Debug, PartialEq)]
pub struct SpectrumFrame {
    /// Frame counter since the processor was created.
    pub seq: u64,
    /// Time of the first sample of the first segment.
    pub t: SampleTime,
    /// Samples spanned by the frame's segments.
    pub sample_count: u64,
    /// Provenance at the frame's first segment.
    pub provenance: ProvenanceHandle,
    /// The provenance record changed (without a reset) during the frame.
    pub provenance_changed: bool,
    /// Discontinuities seen since the previous frame (reset reasons and passed-through flags).
    pub discontinuity: Discontinuity,
    /// Samples lost since the previous frame.
    pub dropped_samples: u64,
    /// The spectrum.
    pub spectrum: Spectrum,
}

/// Processor counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StftStats {
    /// Frames emitted.
    pub frames: u64,
    /// Segments transformed.
    pub segments: u64,
    /// Averaging resets.
    pub resets: u64,
    /// Segments thrown away in partial frames at a reset.
    pub segments_discarded: u64,
    /// Samples reported lost (gaps and overruns).
    pub samples_dropped: u64,
}

/// The streaming STFT. See the [module docs](self).
pub struct StftProcessor {
    config: StftConfig,
    engine: SegmentEngine,
    seg: Vec<Complex32>,
    fill: usize,
    seg_start: u64,
    next_index: Option<u64>,
    anchor: SampleTime,
    provenance: Option<ProvenanceHandle>,
    frame: Option<SpectrumFrame>,
    frame_start: u64,
    frame_provenance_changed: bool,
    pending_flags: Discontinuity,
    pending_dropped: u64,
    persistence: Option<Persistence>,
    stats: StftStats,
}

impl StftProcessor {
    /// A CPU processor.
    pub fn new(config: StftConfig) -> Result<Self, ConfigError> {
        config.validate()?;
        Self::build(config, SegmentEngine::new(config.welch)?)
    }

    /// A processor over a given FFT backend.
    pub fn with_backend(config: StftConfig, fft: Box<dyn FftBackend>) -> Result<Self, ConfigError> {
        config.validate()?;
        Self::build(config, SegmentEngine::with_backend(config.welch, fft)?)
    }

    fn build(config: StftConfig, engine: SegmentEngine) -> Result<Self, ConfigError> {
        let n = config.welch.fft_len;
        Ok(Self {
            config,
            engine,
            seg: vec![Complex32::default(); n],
            fill: 0,
            seg_start: 0,
            next_index: None,
            anchor: SampleTime {
                sample_index: 0,
                host_time: hk_model::Timestamp::UNIX_EPOCH,
            },
            provenance: None,
            frame: None,
            frame_start: 0,
            frame_provenance_changed: false,
            pending_flags: Discontinuity::NONE,
            pending_dropped: 0,
            // Real period set on the first input.
            persistence: config.persistence.map(|p| Persistence::new(n, p, 0.0)),
            stats: StftStats::default(),
        })
    }

    /// Settings.
    pub fn config(&self) -> &StftConfig {
        &self.config
    }

    /// Counters.
    pub fn stats(&self) -> StftStats {
        self.stats
    }

    /// The persistence image, if configured.
    pub fn persistence(&self) -> Option<&Persistence> {
        self.persistence.as_ref()
    }

    /// The most recent frame, if any.
    pub fn last_frame(&self) -> Option<&SpectrumFrame> {
        self.frame.as_ref().filter(|_| self.stats.frames > 0)
    }

    /// Stream index of the next expected sample.
    pub fn next_sample(&self) -> Option<u64> {
        self.next_index
    }

    /// Feeds contiguous samples; calls `emit` for each completed frame and returns how many.
    pub fn push<T: IqSample>(
        &mut self,
        info: InputInfo<'_>,
        samples: &[T],
        mut emit: impl FnMut(&SpectrumFrame),
    ) -> usize {
        self.begin_input(&info);
        let n = self.config.welch.fft_len;
        let overlap = self.config.welch.overlap;
        let hop = n - overlap;
        let first = info.time.sample_index;
        let mut emitted = 0;
        let mut pos = 0;
        while pos < samples.len() {
            if self.fill == 0 {
                self.seg_start = first + pos as u64;
            }
            let take = (n - self.fill).min(samples.len() - pos);
            for (d, &s) in self.seg[self.fill..self.fill + take]
                .iter_mut()
                .zip(&samples[pos..pos + take])
            {
                *d = s.to_complex32();
            }
            self.fill += take;
            pos += take;
            if self.fill == n {
                self.process_segment(&mut emit, &mut emitted);
                self.seg.copy_within(hop.., 0);
                self.fill = overlap;
                self.seg_start += hop as u64;
            }
        }
        self.next_index = Some(first + samples.len() as u64);
        emitted
    }

    /// Convenience for owned blocks.
    pub fn push_block(&mut self, block: &SampleBlock, emit: impl FnMut(&SpectrumFrame)) -> usize {
        self.push(InputInfo::from(block), &block.samples, emit)
    }

    /// Clears all state (as at creation), keeping the settings and counters.
    pub fn reset(&mut self) {
        self.restart_averaging();
        self.next_index = None;
        self.provenance = None;
        if let Some(p) = &mut self.persistence {
            p.clear();
        }
    }

    fn process_segment(&mut self, emit: &mut impl FnMut(&SpectrumFrame), emitted: &mut usize) {
        if self.engine.count() == 0 {
            self.frame_start = self.seg_start;
            let prov = self.provenance.as_ref().expect("input seen");
            let frame = self.frame.as_mut().expect("frame created on first input");
            if frame.provenance != *prov {
                frame.provenance = prov.clone();
            }
        }
        self.engine.process(&self.seg);
        self.stats.segments += 1;
        if let Some(p) = &mut self.persistence {
            p.update(self.engine.last_power(), self.engine.per_rbw_offset_db());
        }
        if self.engine.count() as usize == self.config.averages {
            self.emit_frame(emit);
            *emitted += 1;
        }
    }

    fn emit_frame(&mut self, emit: &mut impl FnMut(&SpectrumFrame)) {
        let frame = self.frame.as_mut().expect("frame created on first input");
        let fs = frame.provenance.tune.sample_rate_hz;
        let fc = frame.provenance.tune.center_hz;
        self.engine.finish_into(fs, fc, &mut frame.spectrum);
        frame.seq = self.stats.frames;
        frame.t = SampleTime {
            sample_index: self.frame_start,
            host_time: self.anchor.time_of(self.frame_start, fs),
        };
        frame.sample_count = self.config.frame_samples();
        frame.provenance_changed = self.frame_provenance_changed;
        frame.discontinuity = self.pending_flags;
        frame.dropped_samples = self.pending_dropped;
        emit(frame);
        self.stats.frames += 1;
        self.pending_flags = Discontinuity::NONE;
        self.pending_dropped = 0;
        self.frame_provenance_changed = false;
        self.engine.reset();
    }

    fn restart_averaging(&mut self) {
        if self.engine.count() > 0 {
            self.stats.segments_discarded += u64::from(self.engine.count());
        }
        self.engine.reset();
        self.fill = 0;
        self.frame_provenance_changed = false;
    }

    fn begin_input(&mut self, info: &InputInfo<'_>) {
        let mut flags = info.discontinuity;
        let mut dropped = 0;
        match self.next_index {
            None => {
                flags |= Discontinuity::STREAM_START;
                dropped = info.dropped_before;
            }
            Some(expected) => {
                let idx = info.time.sample_index;
                if idx != expected {
                    flags |= Discontinuity::GAP;
                    dropped = idx.saturating_sub(expected);
                } else if info.dropped_before > 0 {
                    dropped = info.dropped_before;
                }
            }
        }
        if dropped > 0 {
            flags |= Discontinuity::GAP;
        }

        let prov = info.provenance;
        let rate_changed = match &self.provenance {
            None => true,
            Some(old) if old != prov => {
                let d = Discontinuity::between(old, prov);
                flags |= d;
                let rate = d.contains(Discontinuity::RATE_CHANGE);
                self.provenance = Some(prov.clone());
                if self.engine.count() > 0 {
                    self.frame_provenance_changed = true;
                }
                rate
            }
            Some(_) => false,
        };
        if self.provenance.is_none() {
            self.provenance = Some(prov.clone());
        }
        if self.frame.is_none() {
            self.frame = Some(SpectrumFrame {
                seq: 0,
                t: info.time,
                sample_count: 0,
                provenance: prov.clone(),
                provenance_changed: false,
                discontinuity: Discontinuity::NONE,
                dropped_samples: 0,
                spectrum: self.engine.empty_spectrum(),
            });
        }
        self.anchor = info.time;
        self.pending_flags |= flags;
        self.pending_dropped += dropped;
        self.stats.samples_dropped += dropped;

        if flags.bits() & self.config.reset_on.bits() != 0 {
            self.stats.resets += 1;
            self.restart_averaging();
            let only_gap = flags.bits() & !Discontinuity::GAP.bits() == 0;
            if !only_gap {
                if let Some(p) = &mut self.persistence {
                    p.clear();
                }
            }
        }
        if rate_changed {
            let fs = prov.tune.sample_rate_hz;
            if let Some(p) = &mut self.persistence {
                p.set_frame_period(self.config.welch.hop() as f64 / fs);
            }
        }
    }
}

/// Which tier of a [`DualResolution`] a frame came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tier {
    /// Coarse bins, short frames: bursts (e.g. ~1 kHz bins).
    Burst,
    /// Fine bins, long frames: narrow carriers (e.g. ~25 Hz bins).
    Carrier,
}

/// Two STFTs over the same stream: a burst-oriented and a carrier-oriented resolution
/// (`Δf·Δt ≈ 1`; docs/04 §3.5 via card C07).
pub struct DualResolution {
    /// The coarse, burst-oriented processor.
    pub burst: StftProcessor,
    /// The fine, carrier-oriented processor.
    pub carrier: StftProcessor,
}

impl DualResolution {
    /// Two CPU processors.
    pub fn new(burst: StftConfig, carrier: StftConfig) -> Result<Self, ConfigError> {
        Ok(Self {
            burst: StftProcessor::new(burst)?,
            carrier: StftProcessor::new(carrier)?,
        })
    }

    /// Feeds both tiers; `emit` receives each frame with its tier. Returns frames emitted.
    pub fn push<T: IqSample>(
        &mut self,
        info: InputInfo<'_>,
        samples: &[T],
        mut emit: impl FnMut(Tier, &SpectrumFrame),
    ) -> usize {
        self.burst.push(info, samples, |f| emit(Tier::Burst, f))
            + self.carrier.push(info, samples, |f| emit(Tier::Carrier, f))
    }
}
