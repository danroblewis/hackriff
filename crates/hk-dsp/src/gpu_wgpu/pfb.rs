//! PFB frames on the GPU: a [`PfbExecutor`] for [`crate::channelizer::batch::BatchPfb`].

use std::sync::Arc;

use num_complex::Complex32;

use super::GpuContext;
use super::engine::{Engine, GpuFrame, Load, PlanSpec, Post};
use crate::channelizer::batch::{PfbExecutor, PfbGeometry, PfbJob, PfbTaps};

/// Fold, FFT (Bluestein for non-power-of-two `M`) and channel gather as compute kernels, with
/// asynchronous readback. Holds up to `max_in_flight` blocks.
pub struct WgpuPfbExecutor {
    engine: Engine,
    frames: Vec<GpuFrame>,
    taps: Vec<Complex32>,
    out: Vec<Complex32>,
    out_len: usize,
    max_in_flight: usize,
}

impl WgpuPfbExecutor {
    /// An executor for `g`.
    pub fn new(ctx: Arc<GpuContext>, g: &PfbGeometry<'_>, max_in_flight: usize) -> Self {
        let engine = Engine::new(
            ctx,
            &PlanSpec {
                load: Load::Pfb,
                post: Post::Gather,
                n: g.channels,
                hop: 0,
                window: None,
                taps_len: g.taps,
                active: g.active,
                items_hint: (65_536 / (g.channels / 2)).max(1),
                input_hint: 65_536 + g.taps,
            },
        );
        Self {
            engine,
            frames: Vec::new(),
            taps: vec![Complex32::default(); g.taps],
            out: Vec::new(),
            out_len: 0,
            max_in_flight,
        }
    }
}

impl PfbExecutor for WgpuPfbExecutor {
    fn name(&self) -> &'static str {
        "gpu-wgpu-pfb"
    }

    fn max_in_flight(&self) -> usize {
        self.max_in_flight
    }

    fn set_taps(&mut self, taps: PfbTaps<'_>) {
        // The even tap set; odd window starts multiply by −1 in the frame rotator.
        match taps {
            PfbTaps::Real(t) => {
                for (o, &h) in self.taps.iter_mut().zip(&t[0]) {
                    *o = Complex32::new(h, 0.0);
                }
            }
            PfbTaps::Complex(t) => self.taps.copy_from_slice(&t[0]),
        }
        self.engine.set_taps(&self.taps);
    }

    fn submit(&mut self, job: &PfbJob<'_>) {
        self.frames.clear();
        self.frames.extend(job.frames.iter().map(|f| {
            let sign = if f.first & 1 == 0 { 1.0 } else { -1.0 };
            GpuFrame {
                offset: u32::try_from(f.offset).expect("staging offset fits u32"),
                first: f.first as u32,
                rot_re: sign * f.rot.re,
                rot_im: sign * f.rot.im,
            }
        }));
        let last = job.frames.last().expect("at least one frame");
        let used = (last.offset + self.taps.len()).min(job.staged.len());
        self.engine
            .submit(job.frames.len(), &job.staged[..used], &self.frames);
    }

    fn poll(&mut self, wait: bool) -> bool {
        self.engine.poll_head(wait)
    }

    fn take(&mut self) -> &[Complex32] {
        let values = self.engine.take_head();
        let len = values.len() / 2;
        if self.out.len() < len {
            self.out.resize(len, Complex32::default());
        }
        for (o, v) in self.out.iter_mut().zip(values.chunks_exact(2)) {
            *o = Complex32::new(v[0], v[1]);
        }
        self.out_len = len;
        &self.out[..len]
    }

    fn in_flight(&self) -> usize {
        self.engine.in_flight()
    }
}
