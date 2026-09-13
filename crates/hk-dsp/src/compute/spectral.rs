//! Batched spectral rows: the per-segment work of the STFT (window → FFT → DC-centred `|X|²`)
//! behind one seam, so a provider can compute many segments at once.
//!
//! [`SpectralBackend::submit`] takes a contiguous span of samples and a hop; segment `s` is
//! `span[s·hop .. s·hop + N]`. Rows are handed to a sink **in submission order**. A synchronous
//! backend (CPU) calls the sink before `submit` returns; an asynchronous one (GPU) queues the
//! work and hands over rows of earlier batches as they complete, and [`SpectralBackend::flush`]
//! blocks for the rest. The accumulation (averages, SK, holds, persistence) stays in
//! [`crate::stft::StftProcessor`], which replays input events and rows in order, so every
//! provider gives the same frames, timestamps and discontinuity flags as the CPU reference.

use num_complex::Complex32;

use crate::fft::{CpuFft, FftBackend};
use crate::welch::power_row;
use crate::window::Window;

/// Windowed DC-centred `|X|²` rows for hop-spaced segments of a span. See the
/// [module docs](self).
pub trait SpectralBackend: Send {
    /// Short provider name for logs and benchmarks.
    fn name(&self) -> &'static str;

    /// Segment length `N` (rows have `N` bins).
    fn fft_len(&self) -> usize;

    /// Most segments accepted by one [`SpectralBackend::submit`].
    fn max_batch(&self) -> usize {
        usize::MAX
    }

    /// Computes rows for `count >= 1` segments of `span` (`span.len() >= (count − 1)·hop + N`).
    /// The sink receives whole rows (`k·N` values, `k >= 1`) in submission order, possibly
    /// across several calls and possibly rows of earlier batches.
    fn submit(
        &mut self,
        span: &[Complex32],
        hop: usize,
        count: usize,
        sink: &mut dyn FnMut(&[f32]),
    );

    /// Delivers the rows of every batch still in flight (blocking).
    fn flush(&mut self, sink: &mut dyn FnMut(&[f32])) {
        let _ = sink;
    }

    /// Batches submitted whose rows were not yet delivered.
    fn in_flight(&self) -> usize {
        0
    }
}

/// The CPU reference: one segment at a time through an [`FftBackend`], with exactly the
/// arithmetic of [`crate::welch::SegmentEngine::process`] (bit-identical rows).
pub struct CpuSpectral {
    fft: Box<dyn FftBackend>,
    window: Vec<f32>,
    buf: Vec<Complex32>,
    row: Vec<f32>,
}

impl CpuSpectral {
    /// Over rustfft ([`CpuFft`]).
    pub fn new(window: &Window) -> Self {
        Self::with_fft(window, Box::new(CpuFft::new(window.len())))
    }

    /// Over any FFT backend of the window's length.
    pub fn with_fft(window: &Window, fft: Box<dyn FftBackend>) -> Self {
        let n = window.len();
        assert_eq!(fft.len(), n, "FFT length != window length");
        Self {
            fft,
            window: window.coefficients().to_vec(),
            buf: vec![Complex32::default(); n],
            row: vec![0.0; n],
        }
    }
}

impl SpectralBackend for CpuSpectral {
    fn name(&self) -> &'static str {
        self.fft.name()
    }

    fn fft_len(&self) -> usize {
        self.window.len()
    }

    fn submit(
        &mut self,
        span: &[Complex32],
        hop: usize,
        count: usize,
        sink: &mut dyn FnMut(&[f32]),
    ) {
        let n = self.window.len();
        assert!(span.len() >= (count.max(1) - 1) * hop + n, "span too short");
        for s in 0..count {
            let seg = &span[s * hop..s * hop + n];
            power_row(
                self.fft.as_mut(),
                &self.window,
                seg,
                &mut self.buf,
                &mut self.row,
            );
            sink(&self.row);
        }
    }
}

#[cfg(feature = "cpu-mt")]
pub use mt::CpuMtSpectral;

#[cfg(feature = "cpu-mt")]
mod mt {
    use std::sync::{Arc, Mutex};

    use num_complex::Complex32;
    use rayon::ThreadPool;
    use rayon::prelude::*;

    use super::SpectralBackend;
    use crate::fft::CpuFft;
    use crate::welch::power_row;
    use crate::window::Window;

    /// Per-thread FFT plan and scratch.
    struct Scratch {
        fft: CpuFft,
        buf: Vec<Complex32>,
    }

    /// Multi-threaded CPU rows: segments of a batch run in parallel on the shared rayon pool,
    /// each through the reference [`power_row`] with its own rustfft plan, so rows are
    /// bit-identical to [`super::CpuSpectral`]. Rows are delivered synchronously.
    pub struct CpuMtSpectral {
        pool: Arc<ThreadPool>,
        window: Vec<f32>,
        scratch: Vec<Mutex<Scratch>>,
        local: Scratch,
        rows: Vec<f32>,
        min_parallel: usize,
    }

    impl CpuMtSpectral {
        /// Rows for `window` on `pool`.
        pub fn new(window: &Window, pool: Arc<ThreadPool>) -> Self {
            let n = window.len();
            let make = || Scratch {
                fft: CpuFft::new(n),
                buf: vec![Complex32::default(); n],
            };
            let scratch = (0..pool.current_num_threads())
                .map(|_| Mutex::new(make()))
                .collect();
            Self {
                window: window.coefficients().to_vec(),
                scratch,
                local: make(),
                rows: Vec::new(),
                // Below this many segments a batch runs on the caller's thread.
                min_parallel: 2,
                pool,
            }
        }
    }

    impl SpectralBackend for CpuMtSpectral {
        fn name(&self) -> &'static str {
            "cpu-mt-rustfft"
        }

        fn fft_len(&self) -> usize {
            self.window.len()
        }

        fn submit(
            &mut self,
            span: &[Complex32],
            hop: usize,
            count: usize,
            sink: &mut dyn FnMut(&[f32]),
        ) {
            let n = self.window.len();
            assert!(span.len() >= (count.max(1) - 1) * hop + n, "span too short");
            let need = count * n;
            if self.rows.len() < need {
                // Grows only when a larger batch than ever before arrives.
                self.rows.resize(need, 0.0);
            }
            let window = &self.window;
            let rows = &mut self.rows[..need];
            if count < self.min_parallel {
                for (s, row) in rows.chunks_exact_mut(n).enumerate() {
                    let sc = &mut self.local;
                    power_row(
                        &mut sc.fft,
                        window,
                        &span[s * hop..s * hop + n],
                        &mut sc.buf,
                        row,
                    );
                }
            } else {
                let scratch = &self.scratch;
                // ~256k samples of transform work per rayon task.
                let min_len = ((1usize << 18) / n).max(1);
                self.pool.install(|| {
                    rows.par_chunks_exact_mut(n)
                        .with_min_len(min_len)
                        .enumerate()
                        .for_each(|(s, row)| {
                            let t = rayon::current_thread_index().unwrap_or(0);
                            let mut guard = scratch[t % scratch.len()]
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            let sc = &mut *guard;
                            power_row(
                                &mut sc.fft,
                                window,
                                &span[s * hop..s * hop + n],
                                &mut sc.buf,
                                row,
                            );
                        });
                });
            }
            sink(&self.rows[..need]);
        }
    }
}
