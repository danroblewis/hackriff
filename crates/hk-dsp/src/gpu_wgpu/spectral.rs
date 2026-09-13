//! STFT rows on the GPU: a [`SpectralBackend`] with asynchronous delivery.

use std::sync::Arc;

use num_complex::Complex32;

use super::GpuContext;
use super::engine::{Engine, Load, PlanSpec, Post};
use crate::compute::SpectralBackend;
use crate::window::Window;

/// Windowed DC-centred `|X|²` rows computed on the GPU. Up to `max_in_flight` batches run
/// ahead of delivery; `0` makes every `submit` synchronous.
pub struct WgpuSpectral {
    engine: Engine,
    n: usize,
    hop: usize,
    max_batch: usize,
    max_in_flight: usize,
}

impl WgpuSpectral {
    /// Rows for `window` at segment hop `hop`.
    pub fn new(ctx: Arc<GpuContext>, window: &Window, hop: usize, max_in_flight: usize) -> Self {
        let n = window.len();
        assert!(hop >= 1 && hop <= n, "hop must be in 1..=N");
        let p = if n.is_power_of_two() {
            n
        } else {
            (2 * n - 1).next_power_of_two()
        };
        // About 32 MB of working buffer per slot, at most 1024 segments per batch.
        let max_batch = ((1usize << 22) / p).clamp(1, 1024);
        let engine = Engine::new(
            ctx,
            &PlanSpec {
                load: Load::Stft,
                post: Post::Power,
                n,
                hop,
                window: Some(window.coefficients()),
                taps_len: 0,
                active: &[],
                items_hint: max_batch,
                input_hint: (max_batch - 1) * hop + n,
            },
        );
        Self {
            engine,
            n,
            hop,
            max_batch,
            max_in_flight,
        }
    }

    fn deliver_head(&mut self, sink: &mut dyn FnMut(&[f32])) {
        let rows = self.engine.take_head();
        sink(rows);
    }
}

impl SpectralBackend for WgpuSpectral {
    fn name(&self) -> &'static str {
        "gpu-wgpu"
    }

    fn fft_len(&self) -> usize {
        self.n
    }

    fn max_batch(&self) -> usize {
        self.max_batch
    }

    fn submit(
        &mut self,
        span: &[Complex32],
        hop: usize,
        count: usize,
        sink: &mut dyn FnMut(&[f32]),
    ) {
        assert_eq!(hop, self.hop, "hop differs from the plan");
        assert!(count >= 1 && count <= self.max_batch, "batch size");
        let used = (count - 1) * hop + self.n;
        self.engine.submit(count, &span[..used], &[]);
        while self.engine.in_flight() > self.max_in_flight {
            self.engine.poll_head(true);
            self.deliver_head(sink);
        }
        while self.engine.poll_head(false) {
            self.deliver_head(sink);
        }
    }

    fn flush(&mut self, sink: &mut dyn FnMut(&[f32])) {
        while self.engine.in_flight() > 0 {
            self.engine.poll_head(true);
            self.deliver_head(sink);
        }
    }

    fn in_flight(&self) -> usize {
        self.engine.in_flight()
    }
}
