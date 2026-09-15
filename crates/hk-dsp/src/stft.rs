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
//! **Partial frames (T-139, opt-in).** With [`StftConfig::partial`] set, a reset emits the
//! averaging in progress as a frame instead of discarding it, once the stream is *armed* (an
//! input carried one of [`PartialFrames::arm_on`], e.g. a retune, and no full frame has completed
//! since) and at least
//! [`PartialFrames::min_segments`] segments were averaged. The frame is the measurement before the
//! reset: its tuning, time and flags; its [`Resolution::n_avg`](crate::spectrum::Resolution) is
//! the segments actually averaged and its `sample_count` the samples they span, so reduced
//! averaging is explicit. A stream that never arms (fixed tuning, gaps and gain changes only)
//! emits exactly the frames it would without the option, and so does a tune held for a full frame
//! after a retune (its later gaps discard the partial averaging, as without the option).
//!
//! **Compute providers (T-041).** The per-segment rows (window → FFT → `|X|²`) come from a
//! [`SpectralBackend`]: the CPU reference by default, or a multi-threaded CPU, Accelerate or GPU
//! provider chosen through [`crate::compute::Compute`]. `push` stages samples, submits every
//! complete segment as one batch, and queues an *input event* (flags, drops, provenance, reset)
//! and a *segments event*. Rows are folded into the averages by replaying those events in order
//! as rows arrive, so frames, timestamps and flags do not depend on the provider or on when an
//! asynchronous provider delivers. With an asynchronous provider a frame can be emitted by a
//! later `push` than the one that completed it; call [`StftProcessor::flush`] at the end of a
//! stream to receive the rest.
//!
//! **Real-time path:** `push` allocates nothing once the first frame exists and the largest
//! block has been seen (tested under a counting allocator, CPU reference); per-segment work is
//! window + FFT + O(bins) accumulation.

use std::collections::VecDeque;

use hk_core::{BlockHeader, Discontinuity, ProvenanceHandle, ReadChunk, SampleBlock};
use hk_model::SampleTime;
use num_complex::{Complex, Complex32};

use crate::compute::spectral::{CpuSpectral, SpectralBackend};
use crate::fft::FftBackend;
use crate::persistence::{Persistence, PersistenceConfig};
use crate::spectrum::Spectrum;
use crate::welch::{Accumulators, ConfigError, WelchConfig};
use crate::window::Window;

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
    /// Emit a reset's partial frame (T-139; see the module docs). `None` (the default) discards
    /// it.
    pub partial: Option<PartialFrames>,
}

/// When a reset emits its partial frame instead of discarding it (T-139).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PartialFrames {
    /// Fewest averaged segments a partial frame needs (values below 1 count as 1).
    pub min_segments: usize,
    /// Input flags that arm partial frames. A full frame disarms them (the tuning held for a whole
    /// frame) until the next such input; [`StftProcessor::reset`] disarms too.
    pub arm_on: Discontinuity,
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
            partial: None,
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
    /// Frames emitted from a reset's partial averaging (T-139; included in `frames`).
    pub partial_frames: u64,
    /// Samples reported lost (gaps and overruns).
    pub samples_dropped: u64,
}

/// What happened to the stream, in order; replayed as rows arrive.
enum Event {
    /// The start of one `push`.
    Input {
        time: SampleTime,
        flags: Discontinuity,
        dropped: u64,
        provenance: ProvenanceHandle,
        reset: bool,
        rate_changed: bool,
    },
    /// `count` consecutive segments, the first starting at stream index `first_start`.
    Segments { first_start: u64, count: usize },
}

/// Accumulation side: averages, frame metadata, persistence and counters, driven by events.
struct Replay {
    config: StftConfig,
    acc: Accumulators,
    events: VecDeque<Event>,
    anchor: SampleTime,
    provenance: Option<ProvenanceHandle>,
    frame: Option<SpectrumFrame>,
    frame_start: u64,
    frame_provenance_changed: bool,
    pending_flags: Discontinuity,
    pending_dropped: u64,
    persistence: Option<Persistence>,
    stats: StftStats,
    emitted: usize,
    /// T-139: an input carried one of [`PartialFrames::arm_on`] and no full frame completed since.
    partial_armed: bool,
}

impl Replay {
    fn restart_averaging(&mut self) {
        if self.acc.count() > 0 {
            self.stats.segments_discarded += u64::from(self.acc.count());
        }
        self.acc.reset();
        self.frame_provenance_changed = false;
    }

    /// Applies input events at the head of the queue (no rows are owed before them).
    fn drain_inputs(&mut self, emit: &mut dyn FnMut(&SpectrumFrame)) {
        while matches!(self.events.front(), Some(Event::Input { .. })) {
            if let Some(Event::Input {
                time,
                flags,
                dropped,
                provenance,
                reset,
                rate_changed,
            }) = self.events.pop_front()
            {
                self.apply_input(time, flags, dropped, provenance, reset, rate_changed, emit);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_input(
        &mut self,
        time: SampleTime,
        flags: Discontinuity,
        dropped: u64,
        prov: ProvenanceHandle,
        reset: bool,
        rate_changed: bool,
        emit: &mut dyn FnMut(&SpectrumFrame),
    ) {
        // T-139: in an armed stream a reset first emits the averaging in progress, before this
        // input's provenance, anchor and flags apply (they belong to the next frame).
        if reset && let Some(p) = self.config.partial {
            if flags.bits() & p.arm_on.bits() != 0 {
                self.partial_armed = true;
            }
            if self.partial_armed && self.acc.count() as usize >= p.min_segments.max(1) {
                self.stats.partial_frames += 1;
                self.emit_frame(emit);
                self.emitted += 1;
            }
        }
        match &self.provenance {
            Some(old) if *old != prov => {
                if self.acc.count() > 0 {
                    self.frame_provenance_changed = true;
                }
                self.provenance = Some(prov.clone());
            }
            Some(_) => {}
            None => self.provenance = Some(prov.clone()),
        }
        if self.frame.is_none() {
            self.frame = Some(SpectrumFrame {
                seq: 0,
                t: time,
                sample_count: 0,
                provenance: prov.clone(),
                provenance_changed: false,
                discontinuity: Discontinuity::NONE,
                dropped_samples: 0,
                spectrum: self.acc.empty_spectrum(),
            });
        }
        self.anchor = time;
        self.pending_flags |= flags;
        self.pending_dropped += dropped;
        self.stats.samples_dropped += dropped;

        if reset {
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

    /// Folds in whole rows, replaying queued events in order.
    fn consume_rows(&mut self, rows: &[f32], emit: &mut dyn FnMut(&SpectrumFrame)) {
        let n = self.config.welch.fft_len;
        let hop = self.config.welch.hop() as u64;
        debug_assert_eq!(rows.len() % n, 0, "rows are not whole segments");
        for row in rows.chunks_exact(n) {
            self.drain_inputs(emit);
            let start = match self.events.front_mut() {
                Some(Event::Segments { first_start, count }) => {
                    let start = *first_start;
                    *first_start += hop;
                    *count -= 1;
                    if *count == 0 {
                        self.events.pop_front();
                    }
                    start
                }
                _ => panic!("spectral backend delivered a row with no pending segment"),
            };
            self.on_segment(start, row, emit);
        }
    }

    fn on_segment(&mut self, start: u64, row: &[f32], emit: &mut dyn FnMut(&SpectrumFrame)) {
        if self.acc.count() == 0 {
            self.frame_start = start;
            let prov = self.provenance.as_ref().expect("input seen");
            let frame = self.frame.as_mut().expect("frame created on first input");
            if frame.provenance != *prov {
                frame.provenance = prov.clone();
            }
        }
        self.acc.add(row);
        self.stats.segments += 1;
        if let Some(p) = &mut self.persistence {
            p.update(row, self.acc.per_rbw_offset_db());
        }
        if self.acc.count() as usize == self.config.averages {
            self.emit_frame(emit);
            self.emitted += 1;
            // T-139: a full frame on unchanged tuning disarms partial frames until the next
            // arming input, so a held tune's later gaps (USB overruns) discard as before.
            self.partial_armed = false;
        }
    }

    fn emit_frame(&mut self, emit: &mut dyn FnMut(&SpectrumFrame)) {
        let frame = self.frame.as_mut().expect("frame created on first input");
        let fs = frame.provenance.tune.sample_rate_hz;
        let fc = frame.provenance.tune.center_hz;
        self.acc.finish_into(fs, fc, &mut frame.spectrum);
        frame.seq = self.stats.frames;
        frame.t = SampleTime {
            sample_index: self.frame_start,
            host_time: self.anchor.time_of(self.frame_start, fs),
        };
        // `(count − 1)·hop + N`: `frame_samples()` for a full frame, less for a partial one.
        let count = self.acc.count() as usize;
        frame.sample_count =
            (count.saturating_sub(1) * self.config.welch.hop() + self.config.welch.fft_len) as u64;
        frame.provenance_changed = self.frame_provenance_changed;
        frame.discontinuity = self.pending_flags;
        frame.dropped_samples = self.pending_dropped;
        emit(frame);
        self.stats.frames += 1;
        self.pending_flags = Discontinuity::NONE;
        self.pending_dropped = 0;
        self.frame_provenance_changed = false;
        self.acc.reset();
    }
}

/// The streaming STFT. See the [module docs](self).
pub struct StftProcessor {
    config: StftConfig,
    backend: Box<dyn SpectralBackend>,
    /// Samples not yet consumed by a submitted segment, starting at stream index `staged_start`.
    staged: Vec<Complex32>,
    staged_start: u64,
    next_index: Option<u64>,
    /// Provenance as seen by the staging side (compared per input).
    seg_provenance: Option<ProvenanceHandle>,
    replay: Replay,
}

impl StftProcessor {
    /// A CPU reference processor.
    pub fn new(config: StftConfig) -> Result<Self, ConfigError> {
        config.validate()?;
        let window = Window::new(config.welch.window, config.welch.fft_len);
        Self::build(config, Box::new(CpuSpectral::new(&window)))
    }

    /// A processor over a given FFT backend (rows computed one segment at a time).
    pub fn with_backend(config: StftConfig, fft: Box<dyn FftBackend>) -> Result<Self, ConfigError> {
        config.validate()?;
        if fft.len() != config.welch.fft_len {
            return Err(ConfigError::BackendLength {
                backend: fft.len(),
                fft_len: config.welch.fft_len,
            });
        }
        let window = Window::new(config.welch.window, config.welch.fft_len);
        Self::build(config, Box::new(CpuSpectral::with_fft(&window, fft)))
    }

    /// A processor over a batched spectral provider (see [`crate::compute`]). The backend
    /// must have been built for this config's window and FFT length.
    pub fn with_spectral(
        config: StftConfig,
        backend: Box<dyn SpectralBackend>,
    ) -> Result<Self, ConfigError> {
        config.validate()?;
        if backend.fft_len() != config.welch.fft_len {
            return Err(ConfigError::BackendLength {
                backend: backend.fft_len(),
                fft_len: config.welch.fft_len,
            });
        }
        Self::build(config, backend)
    }

    fn build(config: StftConfig, backend: Box<dyn SpectralBackend>) -> Result<Self, ConfigError> {
        let n = config.welch.fft_len;
        let acc = Accumulators::new(config.welch)?;
        Ok(Self {
            config,
            backend,
            staged: Vec::with_capacity(2 * n),
            staged_start: 0,
            next_index: None,
            seg_provenance: None,
            replay: Replay {
                config,
                acc,
                events: VecDeque::with_capacity(64),
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
                emitted: 0,
                partial_armed: false,
            },
        })
    }

    /// Settings.
    pub fn config(&self) -> &StftConfig {
        &self.config
    }

    /// The spectral provider's name (e.g. `cpu-rustfft`, `cpu-mt-rustfft`, `gpu-wgpu`).
    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// Counters.
    pub fn stats(&self) -> StftStats {
        self.replay.stats
    }

    /// The persistence image, if configured.
    pub fn persistence(&self) -> Option<&Persistence> {
        self.replay.persistence.as_ref()
    }

    /// The most recent frame, if any.
    pub fn last_frame(&self) -> Option<&SpectrumFrame> {
        self.replay
            .frame
            .as_ref()
            .filter(|_| self.replay.stats.frames > 0)
    }

    /// Stream index of the next expected sample.
    pub fn next_sample(&self) -> Option<u64> {
        self.next_index
    }

    /// Batches submitted to an asynchronous provider whose rows have not arrived yet.
    pub fn in_flight(&self) -> usize {
        self.backend.in_flight()
    }

    /// Feeds contiguous samples; calls `emit` for each completed frame and returns how many.
    pub fn push<T: IqSample>(
        &mut self,
        info: InputInfo<'_>,
        samples: &[T],
        mut emit: impl FnMut(&SpectrumFrame),
    ) -> usize {
        let emit: &mut dyn FnMut(&SpectrumFrame) = &mut emit;
        self.replay.emitted = 0;
        let first = info.time.sample_index;
        let event = self.begin_input(&info);
        self.replay.events.push_back(event);
        if self.staged.is_empty() {
            self.staged_start = first;
        }
        self.staged.extend(samples.iter().map(|s| s.to_complex32()));
        self.run_segments(emit);
        self.replay.drain_inputs(emit);
        self.next_index = Some(first + samples.len() as u64);
        self.replay.emitted
    }

    /// Convenience for owned blocks.
    pub fn push_block(&mut self, block: &SampleBlock, emit: impl FnMut(&SpectrumFrame)) -> usize {
        self.push(InputInfo::from(block), &block.samples, emit)
    }

    /// Waits for an asynchronous provider's in-flight rows and emits the frames they
    /// complete. A no-op for synchronous providers. Returns frames emitted.
    pub fn flush(&mut self, mut emit: impl FnMut(&SpectrumFrame)) -> usize {
        let emit: &mut dyn FnMut(&SpectrumFrame) = &mut emit;
        self.replay.emitted = 0;
        let replay = &mut self.replay;
        self.backend
            .flush(&mut |rows| replay.consume_rows(rows, emit));
        self.replay.drain_inputs(emit);
        self.replay.emitted
    }

    /// Clears all state (as at creation), keeping the settings and counters. Rows still in
    /// flight on an asynchronous provider are discarded.
    pub fn reset(&mut self) {
        self.backend.flush(&mut |_| {});
        self.replay.events.clear();
        self.replay.restart_averaging();
        self.replay.partial_armed = false;
        self.staged.clear();
        self.next_index = None;
        self.seg_provenance = None;
        self.replay.provenance = None;
        if let Some(p) = &mut self.replay.persistence {
            p.clear();
        }
    }

    fn run_segments(&mut self, emit: &mut dyn FnMut(&SpectrumFrame)) {
        let n = self.config.welch.fft_len;
        let hop = self.config.welch.hop();
        while self.staged.len() >= n {
            let available = (self.staged.len() - n) / hop + 1;
            let batch = available.min(self.backend.max_batch().max(1));
            self.replay.events.push_back(Event::Segments {
                first_start: self.staged_start,
                count: batch,
            });
            let span = &self.staged[..(batch - 1) * hop + n];
            let replay = &mut self.replay;
            self.backend.submit(span, hop, batch, &mut |rows| {
                replay.consume_rows(rows, emit)
            });
            self.staged.drain(..batch * hop);
            self.staged_start += (batch * hop) as u64;
        }
    }

    /// Staging-side view of an input: flags, losses and whether it resets. The replay side
    /// applies the same decision when the event is reached.
    fn begin_input(&mut self, info: &InputInfo<'_>) -> Event {
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
        let rate_changed = match &self.seg_provenance {
            None => true,
            Some(old) if old != prov => {
                let d = Discontinuity::between(old, prov);
                flags |= d;
                self.seg_provenance = Some(prov.clone());
                d.contains(Discontinuity::RATE_CHANGE)
            }
            Some(_) => false,
        };
        if self.seg_provenance.is_none() {
            self.seg_provenance = Some(prov.clone());
        }
        let reset = flags.bits() & self.config.reset_on.bits() != 0;
        if reset {
            self.staged.clear();
        }
        Event::Input {
            time: info.time,
            flags,
            dropped,
            provenance: prov.clone(),
            reset,
            rate_changed,
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
