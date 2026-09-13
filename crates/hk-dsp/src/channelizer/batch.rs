//! Batched polyphase filter bank: the [`Pfb`](super::Pfb) algorithm split into a *planner*
//! and an *executor* so a compute provider can run a whole block's frames at once (T-041).
//!
//! [`BatchPfb`] owns everything that defines the output — stream tracking, resets, the time
//! map, raster NCO phases, staging of the filter window — and produces, per input block, a
//! [`PfbJob`]: the staged samples plus one [`FrameSpec`] per output frame (window offset, fold
//! slot of the window's first sample, raster rotator). A [`PfbExecutor`] turns the job into
//! frame-major output. The executors are the provider seam:
//!
//! - [`SerialExecutor`]: the CPU reference arithmetic, frame by frame (bit-identical to `Pfb`);
//! - `MtExecutor` (`cpu-mt`): the same per-frame arithmetic across the shared rayon pool
//!   (bit-identical);
//! - `gpu_wgpu::WgpuPfbExecutor` (`gpu-wgpu`): fold, FFT and gather as compute kernels, with
//!   asynchronous readback;
//! - a CUDA executor later (T-026) plugs in here.
//!
//! **Asynchronous executors.** An executor may hold up to [`PfbExecutor::max_in_flight`]
//! blocks. [`BatchPfb`] then returns each block's output (with that block's own header) from a
//! later `process_*` call, one block per call, in order; a call with nothing ready returns an
//! empty output. [`PfbBackend::flush`] drains the rest. With a synchronous executor (the CPU
//! ones) every call returns its own block, exactly like `Pfb`.
//!
//! **Staging.** The planner keeps the newest `L − 1` samples and appends each block, so every
//! frame window of the block is one contiguous slice. Staging grows only when a longer block
//! than any before arrives.

use std::collections::VecDeque;

use hk_core::{Discontinuity, ProvenanceHandle};
use num_complex::{Complex, Complex32};

use super::{
    ChannelHeader, ChannelTime, ChannelizerError, DEFAULT_CHANNEL_RESET_ON, PfbBackend, PfbConfig,
    PfbOutput, Raster, StreamTracker, shifted_taps,
};
use crate::fft::{CpuFft, FftBackend};
use crate::filter::kernels::{fold, fold_complex};
use crate::filter::{FirDesign, pfb_prototype};
use crate::stft::{InputInfo, IqSample};

/// One output frame of a job.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameSpec {
    /// Offset of the frame's `L`-sample window in [`PfbJob::staged`].
    pub offset: usize,
    /// Fold slot of the window's first sample: `(absolute index) mod M`.
    pub first: usize,
    /// Raster rotator `e^{−j2π f_r a₀/fs}` (1 without a raster offset). The `(−1)^first` sign is
    /// *not* included: CPU executors pick the negated tap set, others multiply by the sign.
    pub rot: Complex32,
}

/// The prototype as the fold uses it.
#[derive(Clone, Copy, Debug)]
pub enum PfbTaps<'a> {
    /// No raster offset: `h[i]·(−1)^i` and its negation.
    Real(&'a [Vec<f32>; 2]),
    /// Raster offset: `h[i]·(−1)^i·e^{−j2π f_r i/fs}` and its negation.
    Complex(&'a [Vec<Complex32>; 2]),
}

/// Fixed geometry an executor is built for.
#[derive(Clone, Copy, Debug)]
pub struct PfbGeometry<'a> {
    /// Channels `M`.
    pub channels: usize,
    /// Prototype length `L`.
    pub taps: usize,
    /// Materialised channels, in slot order.
    pub active: &'a [usize],
    /// All `M` channels in index order are active.
    pub all_active: bool,
    /// The analysis prototype.
    pub design: &'a FirDesign,
}

/// One block's work.
#[derive(Clone, Copy, Debug)]
pub struct PfbJob<'a> {
    /// Staged samples; every frame window lies inside.
    pub staged: &'a [Complex32],
    /// The frames, in output order.
    pub frames: &'a [FrameSpec],
    /// The prototype taps in force.
    pub taps: PfbTaps<'a>,
}

/// Computes PFB frames. See the [module docs](self).
pub trait PfbExecutor: Send {
    /// Short provider name.
    fn name(&self) -> &'static str;

    /// Blocks the executor may hold before [`BatchPfb`] waits for the oldest (0 = synchronous).
    fn max_in_flight(&self) -> usize {
        0
    }

    /// The prototype taps changed (construction, and rate changes with a raster offset).
    /// Applies to jobs submitted afterwards.
    fn set_taps(&mut self, taps: PfbTaps<'_>) {
        let _ = taps;
    }

    /// Starts (or, synchronously, completes) one block with `job.frames.len() >= 1` frames.
    fn submit(&mut self, job: &PfbJob<'_>);

    /// Whether the oldest submitted block not yet taken is complete; with `wait`, blocks until
    /// it is (then always true). False when nothing is in flight.
    fn poll(&mut self, wait: bool) -> bool;

    /// Output of the oldest block, frame-major (`frames × active`), removing it from the
    /// in-flight set. Only after [`PfbExecutor::poll`] returned true.
    fn take(&mut self) -> &[Complex32];

    /// Blocks submitted and not yet taken.
    fn in_flight(&self) -> usize;
}

/// Header of a block whose output may be returned later.
struct Pending {
    time: ChannelTime,
    sample_rate_hz: f64,
    provenance: ProvenanceHandle,
    discontinuity: Discontinuity,
    dropped_before: u64,
    frames: usize,
    channel_spacing_hz: f64,
}

/// The batched PFB. See the [module docs](self).
pub struct BatchPfb {
    config: PfbConfig,
    design: FirDesign,
    shifted_taps: [Vec<f32>; 2],
    raster: Option<Raster>,
    m: usize,
    d: usize,
    len: usize,
    active: Vec<usize>,
    slot_of: Vec<u32>,
    staged: Vec<Complex32>,
    staged_base: u64,
    frames: Vec<FrameSpec>,
    countdown: usize,
    start: u64,
    frames_since_reset: u64,
    out_index: u64,
    tracker: StreamTracker,
    pending: VecDeque<Pending>,
    current: Option<Pending>,
    exec: Box<dyn PfbExecutor>,
}

impl BatchPfb {
    /// Designs the prototype and builds the executor with `make` (allocates).
    pub fn new(
        config: PfbConfig,
        make: impl FnOnce(&PfbGeometry<'_>) -> Result<Box<dyn PfbExecutor>, ChannelizerError>,
    ) -> Result<Self, ChannelizerError> {
        config.validate()?;
        let design = pfb_prototype(config.channels, config.stopband_db)?;
        let m = config.channels;
        let len = design.len();
        let active: Vec<usize> = config.active.clone().unwrap_or_else(|| (0..m).collect());
        let all_active = active.len() == m && active.iter().enumerate().all(|(j, &c)| j == c);
        let mut slot_of = vec![u32::MAX; m];
        for (j, &c) in active.iter().enumerate() {
            slot_of[c] = j as u32;
        }
        let shifted = shifted_taps(&design);
        let mut exec = make(&PfbGeometry {
            channels: m,
            taps: len,
            active: &active,
            all_active,
            design: &design,
        })?;
        exec.set_taps(PfbTaps::Real(&shifted));
        Ok(Self {
            shifted_taps: shifted,
            raster: None,
            m,
            d: m / 2,
            len,
            active,
            slot_of,
            staged: Vec::new(),
            staged_base: 0,
            frames: Vec::new(),
            countdown: len,
            start: 0,
            frames_since_reset: 0,
            out_index: 0,
            tracker: StreamTracker::new(DEFAULT_CHANNEL_RESET_ON),
            pending: VecDeque::with_capacity(16),
            current: None,
            exec,
            config,
            design,
        })
    }

    /// A batched PFB over the CPU reference arithmetic.
    pub fn serial(config: PfbConfig) -> Result<Self, ChannelizerError> {
        Self::new(config, |g| Ok(Box::new(SerialExecutor::new(g))))
    }

    /// Prototype taps `L`.
    pub fn taps(&self) -> usize {
        self.len
    }

    /// Blocks held by an asynchronous executor.
    pub fn in_flight(&self) -> usize {
        self.exec.in_flight()
    }

    fn restart(&mut self, start: u64) {
        self.staged.clear();
        self.staged_base = start;
        self.countdown = self.len;
        self.start = start;
        self.frames_since_reset = 0;
    }

    fn prepare_raster(&mut self, fs: f64) {
        if self.config.raster_offset_hz == 0.0 {
            if self.raster.take().is_some() {
                self.exec.set_taps(PfbTaps::Real(&self.shifted_taps));
            }
            return;
        }
        if self.raster.as_ref().is_some_and(|r| r.rate_hz == fs) {
            return;
        }
        let raster = Raster::new(&self.design, self.config.raster_offset_hz, fs);
        self.exec.set_taps(PfbTaps::Complex(&raster.taps));
        self.raster = Some(raster);
    }

    fn window_end(&self, frame: u64) -> u64 {
        self.start + (self.len - 1) as u64 + frame * self.d as u64
    }

    /// Channelises contiguous samples of either type. See [`PfbBackend::process_c32`] and the
    /// [module docs](self) for asynchronous executors.
    pub fn process<T: IqSample>(&mut self, info: InputInfo<'_>, samples: &[T]) -> PfbOutput<'_> {
        let begin = self.tracker.begin(&info, samples.len());
        let fs = info.provenance.tune.sample_rate_hz;
        if begin.rate_changed {
            self.prepare_raster(fs);
        }
        if begin.reset || begin.rate_changed {
            self.restart(info.time.sample_index);
        }
        let first_end = self.window_end(self.frames_since_reset);
        let first_out = self.out_index;

        self.staged.extend(samples.iter().map(|s| s.to_complex32()));

        let s = samples.len();
        let produced = if s >= self.countdown {
            1 + (s - self.countdown) / self.d
        } else {
            0
        };
        self.frames.clear();
        for f in 0..produced as u64 {
            let end = self.window_end(self.frames_since_reset + f);
            let a0 = end + 1 - self.len as u64;
            self.frames.push(FrameSpec {
                offset: (a0 - self.staged_base) as usize,
                first: (a0 % self.m as u64) as usize,
                rot: self
                    .raster
                    .as_ref()
                    .map_or(Complex32::new(1.0, 0.0), |r| r.nco.rotator(a0)),
            });
        }
        self.countdown = if produced > 0 {
            self.d - (s - self.countdown) % self.d
        } else {
            self.countdown - s
        };
        if produced > 0 {
            let taps = match &self.raster {
                None => PfbTaps::Real(&self.shifted_taps),
                Some(r) => PfbTaps::Complex(&r.taps),
            };
            self.exec.submit(&PfbJob {
                staged: &self.staged,
                frames: &self.frames,
                taps,
            });
        }
        self.frames_since_reset += produced as u64;
        self.out_index += produced as u64;
        let keep = self.len - 1;
        if self.staged.len() > keep {
            let drop = self.staged.len() - keep;
            self.staged.drain(..drop);
            self.staged_base += drop as u64;
        }

        let (discontinuity, dropped_before) = if produced > 0 {
            self.tracker.take_pending()
        } else {
            (Discontinuity::NONE, 0)
        };
        let delay = ((self.len - 1) / 2) as u64;
        let time = ChannelTime::new(
            first_out,
            (first_end - delay) as f64,
            self.d as f64,
            self.tracker.anchor(),
            fs,
        );
        self.pending.push_back(Pending {
            time,
            sample_rate_hz: self.config.output_rate_hz(fs),
            provenance: self.tracker.provenance().clone(),
            discontinuity,
            dropped_before,
            frames: produced,
            channel_spacing_hz: self.config.channel_spacing_hz(fs),
        });
        if self.head_ready(false) {
            self.output_head()
        } else {
            self.not_ready_output()
        }
    }

    /// Whether the oldest pending block can be returned now; `flush` waits for it. Waits
    /// anyway when the executor holds more blocks than it may.
    fn head_ready(&mut self, flush: bool) -> bool {
        match self.pending.front() {
            None => false,
            Some(head) if head.frames == 0 => true,
            Some(_) => {
                let wait = flush || self.exec.in_flight() > self.exec.max_in_flight();
                self.exec.poll(wait)
            }
        }
    }

    /// Pops the oldest pending block (ready per [`BatchPfb::head_ready`]) as an output.
    fn output_head(&mut self) -> PfbOutput<'_> {
        let head_frames = self.pending.front().map_or(0, |h| h.frames);
        let data: &[Complex32] = if head_frames == 0 {
            &[]
        } else {
            self.exec.take()
        };
        self.current = self.pending.pop_front();
        let cur = self.current.as_ref().expect("popped");
        PfbOutput {
            header: ChannelHeader {
                time: cur.time,
                sample_rate_hz: cur.sample_rate_hz,
                provenance: &cur.provenance,
                discontinuity: cur.discontinuity,
                dropped_before: cur.dropped_before,
            },
            frames: cur.frames,
            channels: self.m,
            channel_spacing_hz: cur.channel_spacing_hz,
            raster_offset_hz: self.config.raster_offset_hz,
            active: &self.active,
            slot_of: &self.slot_of,
            data,
        }
    }

    /// An empty output while an asynchronous executor still holds the oldest block.
    fn not_ready_output(&self) -> PfbOutput<'_> {
        let head = self.pending.front().expect("a block is pending");
        PfbOutput {
            header: ChannelHeader {
                time: head.time,
                sample_rate_hz: head.sample_rate_hz,
                provenance: &head.provenance,
                discontinuity: Discontinuity::NONE,
                dropped_before: 0,
            },
            frames: 0,
            channels: self.m,
            channel_spacing_hz: head.channel_spacing_hz,
            raster_offset_hz: self.config.raster_offset_hz,
            active: &self.active,
            slot_of: &self.slot_of,
            data: &[],
        }
    }
}

impl PfbBackend for BatchPfb {
    fn name(&self) -> &'static str {
        self.exec.name()
    }

    fn config(&self) -> &PfbConfig {
        &self.config
    }

    fn prototype(&self) -> &FirDesign {
        &self.design
    }

    fn set_reset_on(&mut self, flags: Discontinuity) {
        self.tracker.set_reset_on(flags);
    }

    fn process_c32(&mut self, info: InputInfo<'_>, samples: &[Complex32]) -> PfbOutput<'_> {
        self.process(info, samples)
    }

    fn process_ci8(&mut self, info: InputInfo<'_>, samples: &[Complex<i8>]) -> PfbOutput<'_> {
        self.process(info, samples)
    }

    fn flush(&mut self) -> Option<PfbOutput<'_>> {
        if self.head_ready(true) {
            Some(self.output_head())
        } else {
            None
        }
    }

    fn reset(&mut self) {
        while self.exec.in_flight() > 0 {
            self.exec.poll(true);
            let _ = self.exec.take();
        }
        self.pending.clear();
        self.current = None;
        self.tracker.clear();
        self.restart(0);
    }
}

/// Computes frame `spec` of `job` into `dst` (`active.len()` samples) with exactly the CPU
/// reference arithmetic of `Pfb` (fold kernel, rustfft plan, rotator).
#[inline]
pub(crate) fn compute_frame(
    dst: &mut [Complex32],
    acc: &mut [Complex32],
    fft: &mut CpuFft,
    job: &PfbJob<'_>,
    spec: &FrameSpec,
    taps_len: usize,
    active: Option<&[usize]>,
) {
    let window = &job.staged[spec.offset..spec.offset + taps_len];
    match job.taps {
        PfbTaps::Real(t) => {
            let taps = &t[spec.first & 1];
            match active {
                None => {
                    fold(dst, taps, window, spec.first);
                    fft.forward(dst);
                }
                Some(active) => {
                    fold(acc, taps, window, spec.first);
                    fft.forward(acc);
                    for (o, &c) in dst.iter_mut().zip(active) {
                        *o = acc[c];
                    }
                }
            }
        }
        PfbTaps::Complex(t) => {
            let taps = &t[spec.first & 1];
            let rot = spec.rot;
            match active {
                None => {
                    fold_complex(dst, taps, window, spec.first);
                    fft.forward(dst);
                    for v in dst.iter_mut() {
                        *v *= rot;
                    }
                }
                Some(active) => {
                    fold_complex(acc, taps, window, spec.first);
                    fft.forward(acc);
                    for (o, &c) in dst.iter_mut().zip(active) {
                        *o = acc[c] * rot;
                    }
                }
            }
        }
    }
}

/// Grows `out` to hold `frames × width` samples (allocates only on growth).
fn ensure_len(out: &mut Vec<Complex32>, need: usize) {
    if out.len() < need {
        out.resize(need, Complex32::default());
    }
}

/// The CPU reference executor, frame by frame. See the [module docs](self).
pub struct SerialExecutor {
    fft: CpuFft,
    acc: Vec<Complex32>,
    out: Vec<Complex32>,
    len: usize,
    taps: usize,
    active: Option<Vec<usize>>,
    width: usize,
    ready: bool,
}

impl SerialExecutor {
    /// An executor for `g`.
    pub fn new(g: &PfbGeometry<'_>) -> Self {
        Self {
            fft: CpuFft::new(g.channels),
            acc: vec![Complex32::default(); g.channels],
            out: Vec::new(),
            len: 0,
            taps: g.taps,
            active: (!g.all_active).then(|| g.active.to_vec()),
            width: g.active.len(),
            ready: false,
        }
    }
}

impl PfbExecutor for SerialExecutor {
    fn name(&self) -> &'static str {
        "cpu-pfb-batch"
    }

    fn submit(&mut self, job: &PfbJob<'_>) {
        assert!(!self.ready, "previous block not taken");
        let w = self.width;
        self.len = job.frames.len() * w;
        ensure_len(&mut self.out, self.len);
        for (dst, spec) in self.out.chunks_exact_mut(w).zip(job.frames) {
            compute_frame(
                dst,
                &mut self.acc,
                &mut self.fft,
                job,
                spec,
                self.taps,
                self.active.as_deref(),
            );
        }
        self.ready = true;
    }

    fn poll(&mut self, _wait: bool) -> bool {
        self.ready
    }

    fn take(&mut self) -> &[Complex32] {
        assert!(std::mem::take(&mut self.ready), "no block ready");
        &self.out[..self.len]
    }

    fn in_flight(&self) -> usize {
        usize::from(self.ready)
    }
}

#[cfg(feature = "cpu-mt")]
pub use mt::MtExecutor;

#[cfg(feature = "cpu-mt")]
mod mt {
    use std::sync::{Arc, Mutex};

    use num_complex::Complex32;
    use rayon::ThreadPool;
    use rayon::prelude::*;

    use super::{PfbExecutor, PfbGeometry, PfbJob, compute_frame, ensure_len};
    use crate::fft::CpuFft;

    struct Scratch {
        fft: CpuFft,
        acc: Vec<Complex32>,
    }

    /// Frames of a block in parallel on the shared rayon pool, each with the reference
    /// arithmetic (bit-identical to `Pfb`). Synchronous.
    pub struct MtExecutor {
        pool: Arc<ThreadPool>,
        scratch: Vec<Mutex<Scratch>>,
        out: Vec<Complex32>,
        len: usize,
        taps: usize,
        active: Option<Vec<usize>>,
        width: usize,
        min_len: usize,
        ready: bool,
    }

    impl MtExecutor {
        /// An executor for `g` on `pool`.
        pub fn new(g: &PfbGeometry<'_>, pool: Arc<ThreadPool>) -> Self {
            let m = g.channels;
            let scratch = (0..pool.current_num_threads())
                .map(|_| {
                    Mutex::new(Scratch {
                        fft: CpuFft::new(m),
                        acc: vec![Complex32::default(); m],
                    })
                })
                .collect();
            Self {
                // Group frames so one rayon task is ~0.5 M multiply-adds (~100 µs): finer
                // tasks lose to wake-up and stealing overhead, badly on a loaded machine.
                min_len: (524_288 / (g.taps + m)).max(1),
                pool,
                scratch,
                out: Vec::new(),
                len: 0,
                taps: g.taps,
                active: (!g.all_active).then(|| g.active.to_vec()),
                width: g.active.len(),
                ready: false,
            }
        }
    }

    impl PfbExecutor for MtExecutor {
        fn name(&self) -> &'static str {
            "cpu-mt-pfb"
        }

        fn submit(&mut self, job: &PfbJob<'_>) {
            assert!(!self.ready, "previous block not taken");
            let w = self.width;
            self.len = job.frames.len() * w;
            ensure_len(&mut self.out, self.len);
            let (scratch, taps, active, min_len) = (
                &self.scratch,
                self.taps,
                self.active.as_deref(),
                self.min_len,
            );
            let out = &mut self.out[..self.len];
            self.pool.install(|| {
                out.par_chunks_exact_mut(w)
                    .zip(job.frames.par_iter())
                    .with_min_len(min_len)
                    .for_each(|(dst, spec)| {
                        let t = rayon::current_thread_index().unwrap_or(0);
                        let mut guard = scratch[t % scratch.len()]
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        let sc = &mut *guard;
                        compute_frame(dst, &mut sc.acc, &mut sc.fft, job, spec, taps, active);
                    });
            });
            self.ready = true;
        }

        fn poll(&mut self, _wait: bool) -> bool {
            self.ready
        }

        fn take(&mut self) -> &[Complex32] {
            assert!(std::mem::take(&mut self.ready), "no block ready");
            &self.out[..self.len]
        }

        fn in_flight(&self) -> usize {
            usize::from(self.ready)
        }
    }
}
