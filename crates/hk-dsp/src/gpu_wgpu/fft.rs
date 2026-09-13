//! A synchronous [`FftBackend`] on the GPU. Mostly for the conformance suite and one-off
//! transforms: per call it uploads, waits and reads back, so it only pays off for large
//! `forward_batch` calls. The STFT and PFB use [`super::WgpuSpectral`] and
//! [`super::WgpuPfbExecutor`], which batch and deliver asynchronously.

use std::sync::Arc;

use num_complex::Complex32;

use super::GpuContext;
use super::engine::{Engine, GpuFrame, Load, PlanSpec, Post};
use crate::fft::FftBackend;

/// GPU forward FFT (unnormalised, FFT order), any length `>= 2`.
pub struct WgpuFft {
    engine: Engine,
    n: usize,
    frames: Vec<GpuFrame>,
}

impl WgpuFft {
    /// Plans a forward transform of length `n`.
    pub fn new(ctx: Arc<GpuContext>, n: usize) -> Result<Self, String> {
        if n < 2 {
            return Err(format!("GPU FFT length {n} is below 2"));
        }
        let active: Vec<usize> = (0..n).collect();
        let engine = Engine::new(
            ctx,
            &PlanSpec {
                load: Load::Stft,
                post: Post::Gather,
                n,
                hop: n,
                window: None,
                taps_len: 0,
                active: &active,
                items_hint: 1,
                input_hint: n,
            },
        );
        Ok(Self {
            engine,
            n,
            frames: Vec::new(),
        })
    }

    /// Whether the transform uses Bluestein's algorithm (non-power-of-two length).
    pub fn is_bluestein(&self) -> bool {
        self.engine.is_bluestein()
    }

    /// The power-of-two working length.
    pub fn working_len(&self) -> usize {
        self.engine.working_len()
    }
}

impl FftBackend for WgpuFft {
    fn len(&self) -> usize {
        self.n
    }

    fn name(&self) -> &'static str {
        "gpu-wgpu-fft"
    }

    fn forward(&mut self, buf: &mut [Complex32]) {
        assert_eq!(buf.len(), self.n, "buffer length != FFT length");
        self.forward_batch(buf);
    }

    fn forward_batch(&mut self, buf: &mut [Complex32]) {
        let n = self.n;
        assert_eq!(buf.len() % n, 0, "batch is not a whole number of segments");
        let count = buf.len() / n;
        if count == 0 {
            return;
        }
        self.frames.resize(
            count,
            GpuFrame {
                offset: 0,
                first: 0,
                rot_re: 1.0,
                rot_im: 0.0,
            },
        );
        self.engine.submit(count, buf, &self.frames);
        self.engine.poll_head(true);
        let values = self.engine.take_head();
        for (o, v) in buf.iter_mut().zip(values.chunks_exact(2)) {
            *o = Complex32::new(v[0], v[1]);
        }
    }
}
