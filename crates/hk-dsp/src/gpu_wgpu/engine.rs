//! The batched GPU transform engine: plan tables, a slot pool and the in-flight FIFO. See the
//! [module docs](super).

use std::collections::VecDeque;
use std::f64::consts::PI;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use num_complex::{Complex32, Complex64};
use rustfft::FftPlanner;
use wgpu::util::DeviceExt;

use super::{GpuContext, WAIT_TIMEOUT};

const WORKGROUP: u32 = 256;
const MAX_GROUPS: u32 = 65_535;
const PARAMS_BYTES: usize = 32;
const FRAME_BYTES: usize = 16;

/// How items enter the transform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Load {
    /// Hop-spaced segments of the input times a window (STFT) or ones (plain FFT).
    Stft,
    /// The polyphase fold of a filter window per frame (PFB).
    Pfb,
}

/// What comes back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Post {
    /// DC-centred `|X|²` rows of `n` values.
    Power,
    /// `width` complex values per item (selected bins × per-frame rotator).
    Gather,
}

/// A plan's shape.
pub(crate) struct PlanSpec<'a> {
    pub load: Load,
    pub post: Post,
    /// Transform length (`N`, or `M` for a PFB).
    pub n: usize,
    /// STFT hop (ignored for PFB).
    pub hop: usize,
    /// STFT window (`None`: ones).
    pub window: Option<&'a [f32]>,
    /// PFB prototype length.
    pub taps_len: usize,
    /// Gathered bins, in output order (`Post::Gather`).
    pub active: &'a [usize],
    /// Initial slot capacity: items per batch.
    pub items_hint: usize,
    /// Initial slot capacity: input samples per batch.
    pub input_hint: usize,
}

/// Per-batch parameters, mirrored by `struct Batch` in the shader.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct BatchParams {
    count: u32,
    hop: u32,
    n: u32,
    p: u32,
    log_p: u32,
    width: u32,
    taps: u32,
    _pad: u32,
}

/// Per-frame PFB parameters, mirrored by `struct Frame` in the shader.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct GpuFrame {
    pub offset: u32,
    pub first: u32,
    pub rot_re: f32,
    pub rot_im: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct StageParams {
    half: u32,
    log_half: u32,
    log_step: u32,
    _pad: u32,
}

// SAFETY: `#[repr(C)]` structs of `u32`/`f32` fields only: no padding, every bit pattern valid.
unsafe impl bytemuck::Zeroable for BatchParams {}
unsafe impl bytemuck::Pod for BatchParams {}
unsafe impl bytemuck::Zeroable for GpuFrame {}
unsafe impl bytemuck::Pod for GpuFrame {}
unsafe impl bytemuck::Zeroable for StageParams {}
unsafe impl bytemuck::Pod for StageParams {}

/// `Complex32` samples as bytes.
fn complex_bytes(x: &[Complex32]) -> &[u8] {
    // SAFETY: `num_complex::Complex<f32>` is `#[repr(C)]` with two `f32` fields (no padding);
    // the byte view covers exactly the slice and lives as long as it.
    unsafe { std::slice::from_raw_parts(x.as_ptr().cast::<u8>(), std::mem::size_of_val(x)) }
}

fn interleave(values: impl IntoIterator<Item = Complex64>) -> Vec<f32> {
    values
        .into_iter()
        .flat_map(|c| [c.re as f32, c.im as f32])
        .collect()
}

/// Plan tables shared by every slot.
struct Tables {
    rev: wgpu::Buffer,
    twiddle: wgpu::Buffer,
    kernel: wgpu::Buffer,
    coef: wgpu::Buffer,
    slot_coef: wgpu::Buffer,
    post_index: wgpu::Buffer,
    post_coef: wgpu::Buffer,
    active: wgpu::Buffer,
    stages: Vec<wgpu::BindGroup>,
    /// Keeps the stage uniform buffers alive.
    _stage_buffers: Vec<wgpu::Buffer>,
}

struct Slot {
    items_cap: usize,
    input_cap: usize,
    frames_bytes: usize,
    upload: wgpu::Buffer,
    input: wgpu::Buffer,
    _a: wgpu::Buffer,
    _b: Option<wgpu::Buffer>,
    out: wgpu::Buffer,
    params: wgpu::Buffer,
    frames: wgpu::Buffer,
    readback: wgpu::Buffer,
    bg_load: wgpu::BindGroup,
    bg_fft_a: wgpu::BindGroup,
    bg_fft_b: Option<wgpu::BindGroup>,
    bg_mid: Option<wgpu::BindGroup>,
    bg_post: wgpu::BindGroup,
    upload_ready: Arc<AtomicBool>,
    readback_ready: Arc<AtomicBool>,
    submission: Option<wgpu::SubmissionIndex>,
    out_values: usize,
}

/// See the [module docs](super).
pub(crate) struct Engine {
    ctx: Arc<GpuContext>,
    load: Load,
    post: Post,
    n: usize,
    p: usize,
    log_p: u32,
    bluestein: bool,
    hop: usize,
    width: usize,
    taps_len: usize,
    tables: Tables,
    slots: Vec<Slot>,
    free: Vec<usize>,
    fifo: VecDeque<usize>,
    host: Vec<f32>,
    hints: (usize, usize),
}

fn bind(
    ctx: &GpuContext,
    pipeline: &wgpu::ComputePipeline,
    group: u32,
    entries: &[(u32, &wgpu::Buffer)],
) -> wgpu::BindGroup {
    let entries: Vec<wgpu::BindGroupEntry<'_>> = entries
        .iter()
        .map(|&(binding, buf)| wgpu::BindGroupEntry {
            binding,
            resource: buf.as_entire_binding(),
        })
        .collect();
    ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &pipeline.get_bind_group_layout(group),
        entries: &entries,
    })
}

fn dispatch(pass: &mut wgpu::ComputePass<'_>, total: usize) {
    let groups = u32::try_from(total.div_ceil(WORKGROUP as usize)).expect("dispatch too large");
    if groups <= MAX_GROUPS {
        pass.dispatch_workgroups(groups, 1, 1);
    } else {
        pass.dispatch_workgroups(MAX_GROUPS, groups.div_ceil(MAX_GROUPS), 1);
    }
}

impl Engine {
    pub(crate) fn new(ctx: Arc<GpuContext>, spec: &PlanSpec<'_>) -> Self {
        let n = spec.n;
        assert!(n >= 2, "transform length must be at least 2");
        let bluestein = !n.is_power_of_two();
        let p = if bluestein {
            (2 * n - 1).next_power_of_two()
        } else {
            n
        };
        let log_p = p.trailing_zeros();
        let dev = &ctx.device;
        let storage = |label: &str, bytes: &[u8]| {
            // Pad to at least 16 bytes so any binding is valid even when unused.
            let mut v = bytes.to_vec();
            v.resize(v.len().max(16).next_multiple_of(4), 0);
            dev.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(label),
                contents: &v,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            })
        };

        let rev: Vec<u32> = (0..p as u32)
            .map(|i| {
                if log_p == 0 {
                    0
                } else {
                    i.reverse_bits() >> (32 - log_p)
                }
            })
            .collect();
        let twiddle = interleave(
            (0..p / 2).map(|m| Complex64::from_polar(1.0, -2.0 * PI * m as f64 / p as f64)),
        );
        // Chirp c_k = e^{jπk²/N} (Bluestein) or 1.
        let chirp: Vec<Complex64> = (0..n)
            .map(|k| {
                if bluestein {
                    let kk = (k as f64) * (k as f64);
                    Complex64::from_polar(1.0, (PI * kk / n as f64) % (2.0 * PI))
                } else {
                    Complex64::new(1.0, 0.0)
                }
            })
            .collect();
        let kernel = if bluestein {
            let mut g = vec![Complex64::default(); p];
            g[0] = chirp[0];
            for m in 1..n {
                g[m] = chirp[m];
                g[p - m] = chirp[m];
            }
            FftPlanner::<f64>::new().plan_fft_forward(p).process(&mut g);
            interleave(g)
        } else {
            vec![0.0; 2]
        };
        let coef = match spec.load {
            Load::Stft => interleave((0..n).map(|j| {
                let w = spec.window.map_or(1.0, |w| f64::from(w[j]));
                chirp[j].conj() * w
            })),
            Load::Pfb => vec![0.0; 2 * spec.taps_len.max(1)],
        };
        let slot_coef = match spec.load {
            Load::Pfb => interleave(chirp.iter().map(|c| c.conj())),
            Load::Stft => vec![0.0; 2],
        };
        // FFT-order output tables: X_k = post_coef[k]·D[post_index[k]].
        let index_of = |k: usize| {
            if bluestein {
                ((p - k) % p) as u32
            } else {
                k as u32
            }
        };
        let coef_of = |k: usize| {
            if bluestein {
                chirp[k].conj() / p as f64
            } else {
                Complex64::new(1.0, 0.0)
            }
        };
        let (post_index, post_coef): (Vec<u32>, Vec<f32>) = match spec.post {
            Post::Power => {
                let split = n - n / 2;
                let ks: Vec<usize> = (0..n).map(|i| (i + split) % n).collect();
                (
                    ks.iter().map(|&k| index_of(k)).collect(),
                    interleave(ks.iter().map(|&k| coef_of(k))),
                )
            }
            Post::Gather => (
                (0..n).map(index_of).collect(),
                interleave((0..n).map(coef_of)),
            ),
        };
        let active: Vec<u32> = match spec.post {
            Post::Gather => spec.active.iter().map(|&c| c as u32).collect(),
            Post::Power => vec![0],
        };
        let width = match spec.post {
            Post::Gather => spec.active.len(),
            Post::Power => n,
        };

        let mut stage_buffers = Vec::new();
        let mut stages = Vec::new();
        for lh in 0..log_p {
            let params = StageParams {
                half: 1 << lh,
                log_half: lh,
                log_step: log_p - 1 - lh,
                _pad: 0,
            };
            let buf = dev.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("fft stage"),
                contents: bytemuck::bytes_of(&params),
                usage: wgpu::BufferUsages::UNIFORM,
            });
            stages.push(bind(&ctx, &ctx.pipelines.fft_stage, 1, &[(0, &buf)]));
            stage_buffers.push(buf);
        }
        let tables = Tables {
            rev: storage("rev", bytemuck::cast_slice(&rev)),
            twiddle: storage("twiddle", bytemuck::cast_slice(&twiddle)),
            kernel: storage("bluestein kernel", bytemuck::cast_slice(&kernel)),
            coef: storage("coef", bytemuck::cast_slice(&coef)),
            slot_coef: storage("slot coef", bytemuck::cast_slice(&slot_coef)),
            post_index: storage("post index", bytemuck::cast_slice(&post_index)),
            post_coef: storage("post coef", bytemuck::cast_slice(&post_coef)),
            active: storage("active", bytemuck::cast_slice(&active)),
            stages,
            _stage_buffers: stage_buffers,
        };
        Self {
            ctx,
            load: spec.load,
            post: spec.post,
            n,
            p,
            log_p,
            bluestein,
            hop: spec.hop,
            width,
            taps_len: spec.taps_len,
            tables,
            slots: Vec::new(),
            free: Vec::new(),
            fifo: VecDeque::new(),
            host: Vec::new(),
            hints: (spec.items_hint.max(1), spec.input_hint.max(1)),
        }
    }

    /// Working (power-of-two) transform length.
    pub(crate) fn working_len(&self) -> usize {
        self.p
    }

    /// Whether Bluestein's algorithm is in use.
    pub(crate) fn is_bluestein(&self) -> bool {
        self.bluestein
    }

    /// f32 values per item in the output.
    pub(crate) fn out_per_item(&self) -> usize {
        match self.post {
            Post::Power => self.n,
            Post::Gather => 2 * self.width,
        }
    }

    /// Replaces the PFB taps (`taps_len` values). Applies to batches submitted afterwards.
    pub(crate) fn set_taps(&mut self, taps: &[Complex32]) {
        assert_eq!(self.load, Load::Pfb);
        assert_eq!(taps.len(), self.taps_len, "tap count");
        self.ctx
            .queue
            .write_buffer(&self.tables.coef, 0, complex_bytes(taps));
    }

    pub(crate) fn in_flight(&self) -> usize {
        self.fifo.len()
    }

    fn make_slot(&self, items_cap: usize, input_cap: usize) -> Slot {
        let ctx = &self.ctx;
        let dev = &ctx.device;
        let usage_storage = wgpu::BufferUsages::STORAGE
            | wgpu::BufferUsages::COPY_DST
            | wgpu::BufferUsages::COPY_SRC;
        let buffer = |label: &str, bytes: usize, usage: wgpu::BufferUsages, mapped: bool| {
            dev.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: bytes.max(16).next_multiple_of(4) as u64,
                usage,
                mapped_at_creation: mapped,
            })
        };
        let gather_frames = self.load == Load::Pfb || self.post == Post::Gather;
        let frames_bytes = if gather_frames {
            items_cap * FRAME_BYTES
        } else {
            0
        };
        let out_bytes = items_cap * self.out_per_item() * 4;
        let upload = buffer(
            "upload",
            PARAMS_BYTES + frames_bytes + input_cap * 8,
            wgpu::BufferUsages::MAP_WRITE | wgpu::BufferUsages::COPY_SRC,
            true,
        );
        let input = buffer("input", input_cap * 8, usage_storage, false);
        let a = buffer("a", items_cap * self.p * 8, usage_storage, false);
        let b = self
            .bluestein
            .then(|| buffer("b", items_cap * self.p * 8, usage_storage, false));
        let out = buffer("out", out_bytes, usage_storage, false);
        let params = buffer("params", PARAMS_BYTES, usage_storage, false);
        let frames = buffer("frames", frames_bytes, usage_storage, false);
        let readback = buffer(
            "readback",
            out_bytes,
            wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            false,
        );
        let t = &self.tables;
        let pl = &ctx.pipelines;
        let bg_load = match self.load {
            Load::Stft => bind(
                ctx,
                &pl.load_stft,
                0,
                &[
                    (0, &a),
                    (2, &input),
                    (4, &params),
                    (6, &t.rev),
                    (7, &t.coef),
                ],
            ),
            Load::Pfb => bind(
                ctx,
                &pl.load_pfb,
                0,
                &[
                    (0, &a),
                    (2, &input),
                    (4, &params),
                    (5, &frames),
                    (6, &t.rev),
                    (7, &t.coef),
                    (8, &t.slot_coef),
                ],
            ),
        };
        let fft_on = |buf: &wgpu::Buffer| {
            bind(
                ctx,
                &pl.fft_stage,
                0,
                &[(0, buf), (4, &params), (13, &t.twiddle)],
            )
        };
        let bg_fft_a = fft_on(&a);
        let bg_fft_b = b.as_ref().map(fft_on);
        let bg_mid = b.as_ref().map(|b| {
            bind(
                ctx,
                &pl.bluestein_mid,
                0,
                &[(0, &a), (1, b), (4, &params), (6, &t.rev), (9, &t.kernel)],
            )
        });
        let src = b.as_ref().unwrap_or(&a);
        let bg_post = match self.post {
            Post::Power => bind(
                ctx,
                &pl.post_power,
                0,
                &[
                    (0, src),
                    (3, &out),
                    (4, &params),
                    (10, &t.post_index),
                    (11, &t.post_coef),
                ],
            ),
            Post::Gather => bind(
                ctx,
                &pl.post_gather,
                0,
                &[
                    (0, src),
                    (3, &out),
                    (4, &params),
                    (5, &frames),
                    (10, &t.post_index),
                    (11, &t.post_coef),
                    (12, &t.active),
                ],
            ),
        };
        Slot {
            items_cap,
            input_cap,
            frames_bytes,
            upload,
            input,
            _a: a,
            _b: b,
            out,
            params,
            frames,
            readback,
            bg_load,
            bg_fft_a,
            bg_fft_b,
            bg_mid,
            bg_post,
            upload_ready: Arc::new(AtomicBool::new(true)),
            readback_ready: Arc::new(AtomicBool::new(false)),
            submission: None,
            out_values: 0,
        }
    }

    /// A free slot with room for `items` and `samples` (waiting for upload re-maps, growing or
    /// adding a slot only when no existing one fits).
    fn acquire(&mut self, items: usize, samples: usize) -> usize {
        let start = Instant::now();
        loop {
            let fits = |s: &Slot| s.items_cap >= items && s.input_cap >= samples;
            if let Some(pos) = self.free.iter().position(|&i| {
                let s = &self.slots[i];
                s.upload_ready.load(Ordering::Acquire) && fits(s)
            }) {
                return self.free.swap_remove(pos);
            }
            if let Some(pos) = self
                .free
                .iter()
                .position(|&i| self.slots[i].upload_ready.load(Ordering::Acquire))
            {
                let i = self.free.swap_remove(pos);
                let old = &self.slots[i];
                let slot = self.make_slot(items.max(old.items_cap), samples.max(old.input_cap));
                self.slots[i] = slot;
                return i;
            }
            if self.free.is_empty() {
                let slot = self.make_slot(items.max(self.hints.0), samples.max(self.hints.1));
                self.slots.push(slot);
                return self.slots.len() - 1;
            }
            // Free slots exist but their upload buffers are still being re-mapped: block on the
            // submission that used one of them (its map callback fires when it completes).
            let remaining = WAIT_TIMEOUT.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                panic!("hk-dsp gpu-wgpu: upload buffer re-map timed out");
            }
            let submission = self
                .free
                .iter()
                .find_map(|&i| self.slots[i].submission.clone());
            let _ = self.ctx.device.poll(wgpu::PollType::Wait {
                submission_index: submission,
                timeout: Some(remaining),
            });
            self.ctx.check();
        }
    }

    /// Queues one batch of `count` items over `samples` (and PFB/gather `frames`).
    pub(crate) fn submit(&mut self, count: usize, samples: &[Complex32], frames: &[GpuFrame]) {
        assert!(count >= 1, "empty batch");
        let i = self.acquire(count, samples.len());
        let params = BatchParams {
            count: count as u32,
            hop: self.hop as u32,
            n: self.n as u32,
            p: self.p as u32,
            log_p: self.log_p,
            width: self.width as u32,
            taps: self.taps_len as u32,
            _pad: 0,
        };
        let ctx = self.ctx.clone();
        let pl = &ctx.pipelines;
        let out_values = count * self.out_per_item();
        let slot = &mut self.slots[i];
        let frame_bytes: &[u8] = bytemuck::cast_slice(frames);
        let sample_bytes = complex_bytes(samples);
        let samples_at = PARAMS_BYTES + slot.frames_bytes;
        {
            let mut view = slot
                .upload
                .get_mapped_range_mut(..)
                .expect("upload buffer is mapped");
            view.slice(..PARAMS_BYTES)
                .copy_from_slice(bytemuck::bytes_of(&params));
            if !frame_bytes.is_empty() {
                view.slice(PARAMS_BYTES..PARAMS_BYTES + frame_bytes.len())
                    .copy_from_slice(frame_bytes);
            }
            if !sample_bytes.is_empty() {
                view.slice(samples_at..samples_at + sample_bytes.len())
                    .copy_from_slice(sample_bytes);
            }
        }
        slot.upload.unmap();
        slot.upload_ready.store(false, Ordering::Release);

        let mut enc = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        enc.copy_buffer_to_buffer(&slot.upload, 0, &slot.params, 0, PARAMS_BYTES as u64);
        if !frame_bytes.is_empty() {
            enc.copy_buffer_to_buffer(
                &slot.upload,
                PARAMS_BYTES as u64,
                &slot.frames,
                0,
                frame_bytes.len() as u64,
            );
        }
        if !sample_bytes.is_empty() {
            enc.copy_buffer_to_buffer(
                &slot.upload,
                samples_at as u64,
                &slot.input,
                0,
                sample_bytes.len() as u64,
            );
        }
        {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: None,
                timestamp_writes: None,
            });
            pass.set_pipeline(match self.load {
                Load::Stft => &pl.load_stft,
                Load::Pfb => &pl.load_pfb,
            });
            pass.set_bind_group(0, &slot.bg_load, &[]);
            dispatch(&mut pass, count * self.p);
            let stages = |pass: &mut wgpu::ComputePass<'_>, bg: &wgpu::BindGroup| {
                pass.set_pipeline(&pl.fft_stage);
                pass.set_bind_group(0, bg, &[]);
                for stage in &self.tables.stages {
                    pass.set_bind_group(1, stage, &[]);
                    dispatch(pass, count * (self.p / 2));
                }
            };
            stages(&mut pass, &slot.bg_fft_a);
            if let (Some(mid), Some(fft_b)) = (&slot.bg_mid, &slot.bg_fft_b) {
                pass.set_pipeline(&pl.bluestein_mid);
                pass.set_bind_group(0, mid, &[]);
                dispatch(&mut pass, count * self.p);
                stages(&mut pass, fft_b);
            }
            pass.set_pipeline(match self.post {
                Post::Power => &pl.post_power,
                Post::Gather => &pl.post_gather,
            });
            pass.set_bind_group(0, &slot.bg_post, &[]);
            dispatch(&mut pass, count * self.width);
        }
        let out_bytes = (out_values * 4) as u64;
        enc.copy_buffer_to_buffer(&slot.out, 0, &slot.readback, 0, out_bytes);
        let submission = ctx.queue.submit([enc.finish()]);

        let ready = slot.readback_ready.clone();
        slot.readback
            .map_async(wgpu::MapMode::Read, 0..out_bytes, move |r| {
                if r.is_ok() {
                    ready.store(true, Ordering::Release);
                }
            });
        let ready = slot.upload_ready.clone();
        slot.upload.map_async(wgpu::MapMode::Write, .., move |r| {
            if r.is_ok() {
                ready.store(true, Ordering::Release);
            }
        });
        slot.submission = Some(submission);
        slot.out_values = out_values;
        self.fifo.push_back(i);
        // Nudge the device so completed work is noticed promptly.
        let _ = ctx.device.poll(wgpu::PollType::Poll);
        ctx.check();
    }

    /// Whether the oldest batch's output is readable; with `wait`, blocks until it is.
    pub(crate) fn poll_head(&mut self, wait: bool) -> bool {
        let Some(&i) = self.fifo.front() else {
            return false;
        };
        let ready = self.slots[i].readback_ready.clone();
        if ready.load(Ordering::Acquire) {
            return true;
        }
        if !wait {
            let _ = self.ctx.device.poll(wgpu::PollType::Poll);
            self.ctx.check();
            return ready.load(Ordering::Acquire);
        }
        let start = Instant::now();
        while !ready.load(Ordering::Acquire) {
            let remaining = WAIT_TIMEOUT.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                panic!("hk-dsp gpu-wgpu: batch did not complete within {WAIT_TIMEOUT:?}");
            }
            let _ = self.ctx.device.poll(wgpu::PollType::Wait {
                submission_index: self.slots[i].submission.clone(),
                timeout: Some(remaining),
            });
            self.ctx.check();
        }
        true
    }

    /// Copies out the oldest batch (after [`Engine::poll_head`] returned true) and frees its
    /// slot. Returns `count × out_per_item` values.
    pub(crate) fn take_head(&mut self) -> &[f32] {
        let i = self.fifo.pop_front().expect("a batch is in flight");
        let slot = &mut self.slots[i];
        assert!(
            slot.readback_ready.load(Ordering::Acquire),
            "batch not complete"
        );
        let values = slot.out_values;
        if self.host.len() < values {
            self.host.resize(values, 0.0);
        }
        {
            let view = slot
                .readback
                .get_mapped_range(..(values * 4) as u64)
                .expect("readback mapped");
            bytemuck::cast_slice_mut::<f32, u8>(&mut self.host[..values]).copy_from_slice(&view);
        }
        slot.readback.unmap();
        slot.readback_ready.store(false, Ordering::Release);
        // Keep `submission`: `acquire` waits on it while the upload buffer re-maps.
        self.free.push(i);
        &self.host[..values]
    }
}
