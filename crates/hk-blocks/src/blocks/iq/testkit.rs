//! Test harness for the T-086 blocks: builds blocks through the registry (so params are
//! schema-validated), runs a linear chain chunk by chunk like the recipe runtime, and collects
//! every stage's outputs.

use std::collections::BTreeMap;

use hk_recipe::{Params, PortType};
use num_complex::Complex32;

use crate::block::{Block, Io, ParamUpdate, PortInfo, TapMask};
use crate::buffer::{ChunkFlags, ChunkMeta, FrameInfo, Input, Output, PortSlice, PortVec};
use crate::registry::{BuildCtx, Registry};

/// Params from a JSON object literal.
pub(crate) fn params(v: serde_json::Value) -> Params {
    v.as_object().cloned().unwrap_or_default()
}

/// Builds `name` through the builtin registry (schema validation included).
pub(crate) fn build(name: &str, p: serde_json::Value, input: PortType) -> Box<dyn Block> {
    let maps = BTreeMap::new();
    let types = [input];
    let ctx = BuildCtx {
        field_maps: &maps,
        input_types: &types,
    };
    Registry::builtin()
        .build(name, &params(p), &ctx)
        .unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// A hot/cold update through the contract.
pub(crate) fn update(block: &mut dyn Block, p: serde_json::Value, input: PortType) -> ParamUpdate {
    let maps = BTreeMap::new();
    let types = [input];
    let ctx = BuildCtx {
        field_maps: &maps,
        input_types: &types,
    };
    block.update_params(&params(p), &ctx).unwrap()
}

/// Collected items of one output port.
#[derive(Clone, Debug, PartialEq, Default)]
pub(crate) struct Collected {
    pub iq: Vec<Complex32>,
    pub real: Vec<f32>,
    pub bits: Vec<u8>,
    pub frames: Vec<(Vec<u8>, FrameInfo)>,
    /// Chunk metadata of every non-empty output chunk.
    pub metas: Vec<ChunkMeta>,
    /// Item count of every non-empty output chunk (parallel to `metas`).
    pub lens: Vec<usize>,
}

impl Collected {
    fn append(&mut self, out: &Output) {
        if !out.data.is_empty() {
            self.metas.push(out.meta);
            self.lens.push(out.data.len());
        }
        match &out.data {
            PortVec::Iq(v) => self.iq.extend_from_slice(v),
            PortVec::Real(v) | PortVec::Soft(v) => self.real.extend_from_slice(v),
            PortVec::Bits(v) => self.bits.extend_from_slice(v),
            PortVec::Frames(f) => self
                .frames
                .extend(f.iter().map(|x| (x.bytes.to_vec(), x.info.clone()))),
        }
    }
}

struct Stage {
    block: Box<dyn Block>,
    outputs: Vec<Output>,
    collected: Vec<Collected>,
}

/// A linear chain: output 0 of each stage feeds the next.
pub(crate) struct Chain {
    stages: Vec<Stage>,
    input: PortInfo,
    index: u64,
}

impl Chain {
    /// Inits every block (`input.max_items` is the chunk size bound).
    pub(crate) fn new(blocks: Vec<Box<dyn Block>>, input: PortInfo) -> Self {
        let mut info = input;
        let stages = blocks
            .into_iter()
            .map(|mut block| {
                let outs = block.init(&[info]).expect("init");
                info = outs[0];
                Stage {
                    block,
                    outputs: outs.iter().map(Output::for_port).collect(),
                    collected: vec![Collected::default(); outs.len()],
                }
            })
            .collect();
        Self {
            stages,
            input,
            index: 0,
        }
    }

    /// Output port infos of stage `i`.
    pub(crate) fn feed(&mut self, data: PortSlice<'_>, flags: ChunkFlags) {
        let mut meta = ChunkMeta {
            index: self.index,
            source_index: self.index as f64,
            source_per_item: 1.0,
            rate_hz: self.input.rate_hz,
            channel: 0,
            flags,
        };
        if self.index == 0 {
            meta.flags |= ChunkFlags::DISCONTINUITY;
        }
        self.index += data.len() as u64;
        let first = Input { meta, data };
        let mut prev: Option<&mut Stage> = None;
        for stage in self.stages.iter_mut() {
            let input = match &prev {
                None => first,
                Some(p) => Input {
                    meta: p.outputs[0].meta,
                    data: p.outputs[0].data.as_slice(),
                },
            };
            for o in &mut stage.outputs {
                o.begin_chunk();
            }
            let inputs = [input];
            stage
                .block
                .process(&mut Io::new(&inputs, &mut stage.outputs).with_taps(TapMask(u32::MAX)))
                .expect("process");
            for (c, o) in stage.collected.iter_mut().zip(&stage.outputs) {
                c.append(o);
            }
            prev = Some(stage);
        }
    }

    /// Feeds `data` in chunks of `chunk` items (the last chunk flagged `END`).
    pub(crate) fn run(&mut self, data: &PortVec, chunk: usize) {
        let n = data.len();
        let mut k = 0;
        while k < n {
            let e = (k + chunk).min(n);
            let flags = if e == n {
                ChunkFlags::END
            } else {
                ChunkFlags::NONE
            };
            self.feed(slice(data, k, e), flags);
            k = e;
        }
    }

    /// What stage `stage` emitted on output `port`.
    pub(crate) fn out(&self, stage: usize, port: usize) -> &Collected {
        &self.stages[stage].collected[port]
    }

    /// The block of stage `stage`.
    pub(crate) fn block(&self, stage: usize) -> &dyn Block {
        self.stages[stage].block.as_ref()
    }
}

/// Items `a..b` of a sample-rate buffer.
pub(crate) fn slice(data: &PortVec, a: usize, b: usize) -> PortSlice<'_> {
    match data {
        PortVec::Iq(v) => PortSlice::Iq(&v[a..b]),
        PortVec::Real(v) => PortSlice::Real(&v[a..b]),
        PortVec::Soft(v) => PortSlice::Soft(&v[a..b]),
        PortVec::Bits(v) => PortSlice::Bits(&v[a..b]),
        PortVec::Frames(_) => panic!("frames are not sliced"),
    }
}

/// Runs a chain built by `make` over `data` with each chunk size and asserts every stage's
/// outputs are identical (ADR-0011 §1.6 chunking invariance). Returns the first run.
pub(crate) fn assert_chunk_invariant(
    make: impl Fn() -> Vec<Box<dyn Block>>,
    ty: PortType,
    rate_hz: f64,
    data: &PortVec,
    chunks: &[usize],
) -> Chain {
    let run = |chunk: usize| {
        let info = PortInfo {
            ty,
            rate_hz,
            max_items: chunk,
            hold_items: 0,
        };
        let mut c = Chain::new(make(), info);
        c.run(data, chunk);
        c
    };
    let first = run(chunks[0]);
    for &chunk in &chunks[1..] {
        let other = run(chunk);
        for (i, (a, b)) in first.stages.iter().zip(&other.stages).enumerate() {
            for (p, (x, y)) in a.collected.iter().zip(&b.collected).enumerate() {
                assert_eq!(x.iq, y.iq, "stage {i} port {p} iq, chunk {chunk}");
                assert_eq!(x.real, y.real, "stage {i} port {p} real, chunk {chunk}");
                assert_eq!(x.bits, y.bits, "stage {i} port {p} bits, chunk {chunk}");
                assert_eq!(
                    x.frames, y.frames,
                    "stage {i} port {p} frames, chunk {chunk}"
                );
            }
        }
    }
    first
}

/// Deterministic uniform/gaussian source.
pub(crate) struct Lcg(u64);

impl Lcg {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1))
    }
    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    pub(crate) fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub(crate) fn bit(&mut self) -> u8 {
        (self.next_u64() >> 63) as u8
    }
    pub(crate) fn gauss(&mut self) -> f64 {
        let u = self.unit().max(1e-300);
        let v = self.unit();
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }
    /// Complex gaussian noise of total variance `var`.
    pub(crate) fn cnoise(&mut self, var: f64) -> Complex32 {
        let s = (var / 2.0).sqrt();
        Complex32::new((self.gauss() * s) as f32, (self.gauss() * s) as f32)
    }
}

/// Aligns `got` against `truth` (searching output offsets `0..max_shift`, both polarities not
/// allowed) after skipping `skip` truth bits of acquisition, and returns (errors, compared).
pub(crate) fn bit_errors(
    truth: &[u8],
    got: &[u8],
    skip: usize,
    max_shift: usize,
) -> (usize, usize) {
    let mut best = (usize::MAX, 0);
    for shift in 0..max_shift.min(got.len()) {
        // got[i + shift] is truth[i + lag] for some lag; search lags around the start.
        for lag in 0..max_shift.min(truth.len()) {
            let n = (truth.len() - lag).min(got.len() - shift);
            if n <= skip + 16 {
                continue;
            }
            let errs = (skip..n)
                .filter(|&i| truth[i + lag] != got[i + shift])
                .count();
            if errs < best.0 {
                best = (errs, n - skip);
            }
        }
    }
    best
}
