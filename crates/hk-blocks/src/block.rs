//! The block contract (ADR-0011 §1). **Core interface.**

use hk_model::synth::EvidenceSet;
use hk_recipe::{Params, PortType};

use crate::buffer::{Input, Output};
use crate::registry::BuildCtx;
use crate::sink::AudioFrames;
use crate::status::Status;

/// What a port carries, negotiated at `init`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PortInfo {
    /// Port type.
    pub ty: PortType,
    /// Item rate, Hz.
    pub rate_hz: f64,
    /// Most items one chunk can carry on this port. Outputs are pre-sized to it, so a block
    /// must compute its outputs' bound from its inputs' (e.g. `ceil(n · up / down) + taps`).
    pub max_items: usize,
    /// Items of history the block holds before its output reflects an input item (filter
    /// delay, symbols per decision, bits per frame). The runtime sums these into the
    /// pipeline's latency bound.
    pub hold_items: usize,
}

/// How a parameter update was applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamUpdate {
    /// Applied in place at this chunk boundary; state kept.
    Applied,
    /// Cannot apply in place: the runtime builds a fresh instance off the real-time thread and
    /// swaps it in (downstream nodes see `RESET`).
    Rebuild,
}

/// Which output ports have a stage-stream consumer (bit `i` = output `i`). Diagnostic outputs
/// are computed only while tapped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TapMask(pub u32);

impl TapMask {
    /// Whether output `i` is tapped.
    pub const fn is_tapped(self, i: usize) -> bool {
        i < 32 && self.0 & (1 << i) != 0
    }
}

/// A block error. Messages never contain signal content.
#[derive(Clone, Debug, PartialEq)]
pub enum BlockError {
    /// Parameters invalid for this block (after schema validation: cross-parameter rules).
    Params(String),
    /// Wrong number of ports at `init`, or a port index out of range.
    Ports(String),
    /// Input `port` carries `got`, the block expected `expected` (a runtime wiring bug).
    PortType {
        /// Input or output index.
        port: usize,
        /// Expected type.
        expected: PortType,
        /// Actual type.
        got: PortType,
    },
    /// The parameters cannot be realised at the negotiated rates.
    Unrealisable(String),
}

impl std::fmt::Display for BlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlockError::Params(m) => write!(f, "parameters: {m}"),
            BlockError::Ports(m) => write!(f, "ports: {m}"),
            BlockError::PortType {
                port,
                expected,
                got,
            } => write!(f, "port {port}: expected {expected}, got {got}"),
            BlockError::Unrealisable(m) => write!(f, "unrealisable: {m}"),
        }
    }
}

impl std::error::Error for BlockError {}

/// One `process` call's ports.
pub struct Io<'a> {
    inputs: &'a [Input<'a>],
    outputs: &'a mut [Output],
    taps: TapMask,
}

impl<'a> Io<'a> {
    /// Ports for one call (no taps).
    pub fn new(inputs: &'a [Input<'a>], outputs: &'a mut [Output]) -> Self {
        Self {
            inputs,
            outputs,
            taps: TapMask::default(),
        }
    }

    /// With a tap mask.
    pub fn with_taps(mut self, taps: TapMask) -> Self {
        self.taps = taps;
        self
    }

    /// Input `i` by value (inputs are borrowed views, so this does not borrow `self`).
    pub fn input(&self, i: usize) -> Result<Input<'a>, BlockError> {
        self.inputs
            .get(i)
            .copied()
            .ok_or_else(|| BlockError::Ports(format!("no input {i}")))
    }

    /// Number of inputs.
    pub fn input_count(&self) -> usize {
        self.inputs.len()
    }

    /// Output `i`.
    pub fn output(&mut self, i: usize) -> Result<&mut Output, BlockError> {
        self.outputs
            .get_mut(i)
            .ok_or_else(|| BlockError::Ports(format!("no output {i}")))
    }

    /// Whether output `i` has a stage-stream consumer.
    pub fn tapped(&self, i: usize) -> bool {
        self.taps.is_tapped(i)
    }
}

/// A block instance. Built by a [`crate::BlockFactory`] from validated params, owned by one
/// pipeline thread (`Send`, not `Sync`).
///
/// **Lifecycle.** `build` → `init` (once per negotiation; may allocate) → `process` per chunk →
/// `update_params` / `reset` between chunks → drop. Everything runs on the pipeline's thread;
/// the runtime never calls two methods concurrently.
pub trait Block: Send {
    /// Negotiates ports: given the input ports, returns the output ports (same order as the
    /// descriptor's outputs, diagnostic ones included). Allocates buffers and designs filters.
    /// Called again after an upstream rate change (the block may re-plan).
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError>;

    /// Consumes every input item of the chunk and appends to the outputs (which the runtime has
    /// `begin_chunk`ed). Must not block or do I/O; must not allocate on sample-rate ports in
    /// steady state; must honour `DISCONTINUITY`/`RESET` input flags by dropping history and
    /// propagating the flag to its outputs.
    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError>;

    /// Drops all history (as on `DISCONTINUITY`); parameters and negotiated ports are kept.
    fn reset(&mut self);

    /// New parameter values (the full, schema-validated set) and the new recipe's build
    /// context. Called when a param changed **or** a field map a `field-map` param names changed
    /// content (`EditPlan` lists that param's key), so a `fields` block re-resolves its map from
    /// `ctx.field_maps`. Returns `Applied` when every change could be applied in place (its
    /// schema says `hot`), else `Rebuild`, leaving the instance unchanged. Runs between chunks
    /// on the pipeline thread; anything expensive (a map compiled into an evaluation plan) is
    /// the runtime's to prepare off the real-time thread before the swap.
    fn update_params(
        &mut self,
        params: &Params,
        ctx: &BuildCtx<'_>,
    ) -> Result<ParamUpdate, BlockError>;

    /// Current readout: cheap (`Copy`), callable after any `process`. Polled by the runtime
    /// about every 250 ms for status records and the objective of output-driven refinement.
    fn status(&self) -> Status;

    /// An audio sink's finished frames (ADR-0011 §8.4, `audio_out`): the product of a block
    /// with no output port. The runtime reads them after `process` and clears them once
    /// published; every other block answers `None` (the default).
    fn audio_frames(&mut self) -> Option<&mut AudioFrames> {
        None
    }

    /// Synthesis evidence (ADR-0015 §2.1, T-853): at most four [`hk_model::synth::Evidence`]
    /// summaries of everything processed since the last [`Block::reset`], appended to `out`.
    /// Called between chunks; must not allocate. **Optional**: the default emits nothing, and a
    /// block with nothing to say is scored as zero evidence, never as a failure.
    ///
    /// Analytic metrics carry their bits; calibrated metrics carry `raw` and `n` with `bits =
    /// 0.0`, which `hk-synth` scores against the block's calibration table (see
    /// [`crate::evidence`]). Not a port: diagnostic outputs stay the visual path.
    fn evidence(&self, out: &mut EvidenceSet) {
        let _ = out;
    }
}
