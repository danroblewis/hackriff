//! Decoder workbench block library (ADR-0011 §1, T-085). **Core interface and real-time path**:
//! changes are reviewed before merge.
//!
//! Blocks are the generic DSP / symbol / framing / FEC / parse primitives recipes chain
//! together ([`hk_recipe::Recipe`]). Each block has typed ports ([`hk_recipe::PortType`]),
//! schema-validated parameters, a status readout ([`Status`]) and stage outputs the UI can tap.
//!
//! - [`block`]: the [`Block`] trait, [`Io`], [`PortInfo`], [`BlockError`].
//! - [`buffer`]: per-port chunk buffers ([`PortSlice`], [`PortVec`], [`FrameBuf`]) and
//!   [`ChunkMeta`] (element index, source time map, channel, flags).
//! - [`status`]: [`Status`], a `Copy` readout (lock, SNR, error rate, quality, extras).
//! - [`registry`]: [`BlockFactory`] and the [`Registry`] (an [`hk_recipe::Catalogue`]).
//! - [`sink`]: what a sink block (`audio_out`) hands the runtime ([`AudioFrames`]).
//! - [`catalogue`]: the M1 library's pinned descriptors ([`catalogue::planned`]).
//! - [`blocks`]: implementations, one module per group (ownership in ADR-0011 §7).
//! - [`evidence`]: the per-block evidence accumulators behind [`Block::evidence`] (ADR-0015
//!   §2.1, T-853).
//! - [`window`]: [`run_window`], the batch driver the synthesis search evaluates a recipe prefix
//!   with (ADR-0015 §3.1, T-853).
//!
//! # Real-time rules (ADR-0011 §1.4)
//! 1. `init` may allocate and design filters; `process` on a sample-rate port must not allocate
//!    in steady state (outputs are pre-sized from [`PortInfo::max_items`]).
//! 2. `process` never blocks, sleeps or does I/O; it consumes the whole input chunk.
//! 3. A block holds at most [`PortInfo::hold_items`] items of history before emitting output.
//! 4. Loss is signalled, never waited out: a [`ChunkFlags::DISCONTINUITY`] input resets state.
//!
//! ```
//! use std::collections::BTreeMap;
//! use hk_blocks::{BuildCtx, ChunkMeta, Input, Io, Output, PortInfo, PortSlice, Registry};
//! use hk_recipe::{Params, PortType};
//!
//! let registry = Registry::builtin();
//! let maps = BTreeMap::new();
//! let ctx = BuildCtx { field_maps: &maps, input_types: &[PortType::Bits] };
//! let mut block = registry.build("identity", &Params::new(), &ctx).unwrap();
//!
//! let info = PortInfo { ty: PortType::Bits, rate_hz: 1187.5, max_items: 64, hold_items: 0 };
//! let out_info = block.init(&[info]).unwrap();
//! let mut outputs = vec![Output::for_port(&out_info[0])];
//!
//! let bits = [1u8, 0, 1, 1];
//! let inputs = [Input { meta: ChunkMeta::start(1187.5), data: PortSlice::Bits(&bits) }];
//! block.process(&mut Io::new(&inputs, &mut outputs)).unwrap();
//! assert_eq!(outputs[0].data.as_slice(), PortSlice::Bits(&bits));
//! assert_eq!(block.status().items_out, 4);
//! ```

pub mod block;
pub mod blocks;
pub mod buffer;
pub mod catalogue;
pub mod evidence;
pub mod registry;
pub mod schema;
pub mod sink;
pub mod status;
pub mod window;

pub use block::{Block, BlockError, Io, ParamUpdate, PortInfo, TapMask};
pub use buffer::{
    ChunkFlags, ChunkMeta, Frame, FrameBuf, FrameInfo, Input, Output, PortSlice, PortVec,
};
pub use hk_model::synth::{Evidence, EvidenceSet, GroupId, MetricId, Stage};
pub use registry::{BlockFactory, BuildCtx, Registry};
pub use sink::{AudioFrame, AudioFrames};
pub use status::{Extras, Lock, MAX_EXTRAS, Status};
pub use window::{NodeEvidence, WindowError, WindowRun, run_window};
