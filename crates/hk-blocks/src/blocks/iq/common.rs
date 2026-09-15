//! Helpers shared by the T-086 blocks (iq and symbol groups): a function-pointer factory,
//! parameter readers, port checks and the chunk time map.

use std::sync::Arc;

use hk_recipe::{BlockDescriptor, Params, PortType, parse_hex};
use num_complex::Complex32;
use serde_json::Value;

use crate::block::{Block, BlockError, PortInfo};
use crate::buffer::{ChunkFlags, ChunkMeta, Input, Output, PortSlice, PortVec};
use crate::registry::{BlockFactory, BuildCtx, Registry};

/// Builds one block kind from validated params.
pub(crate) type BuildFn = fn(&Params, &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError>;

/// A factory whose descriptor is the pinned one (`planned()`), so the implementation can't
/// drift from the catalogue.
pub(crate) struct FnFactory {
    descriptor: BlockDescriptor,
    build: BuildFn,
}

impl BlockFactory for FnFactory {
    fn descriptor(&self) -> &BlockDescriptor {
        &self.descriptor
    }

    fn build(&self, params: &Params, ctx: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
        (self.build)(params, ctx)
    }
}

/// Registers `name` with its pinned descriptor from `planned`.
pub(crate) fn register_pinned(
    r: &mut Registry,
    planned: &[BlockDescriptor],
    name: &str,
    build: BuildFn,
) {
    let descriptor = planned
        .iter()
        .find(|d| d.name == name)
        .unwrap_or_else(|| panic!("{name} has no pinned descriptor"))
        .clone();
    r.register(Arc::new(FnFactory { descriptor, build }))
        .expect("block registered once");
}

/// A number.
pub(crate) fn get_f64(p: &Params, key: &str) -> Option<f64> {
    p.get(key).and_then(Value::as_f64)
}

/// A number, or `default`.
pub(crate) fn f64_or(p: &Params, key: &str, default: f64) -> f64 {
    get_f64(p, key).unwrap_or(default)
}

/// An integer, or `default`.
pub(crate) fn i64_or(p: &Params, key: &str, default: i64) -> i64 {
    p.get(key).and_then(Value::as_i64).unwrap_or(default)
}

/// A string, or `default`.
pub(crate) fn str_or<'a>(p: &'a Params, key: &str, default: &'a str) -> &'a str {
    p.get(key).and_then(Value::as_str).unwrap_or(default)
}

/// A boolean, or `default`.
pub(crate) fn bool_or(p: &Params, key: &str, default: bool) -> bool {
    p.get(key).and_then(Value::as_bool).unwrap_or(default)
}

/// A `"0x…"` hex value (or a plain integer).
pub(crate) fn get_hex(p: &Params, key: &str) -> Option<u64> {
    match p.get(key)? {
        Value::String(s) => parse_hex(s),
        v => v.as_u64(),
    }
}

/// A required number (schema validation normally guarantees it).
pub(crate) fn require_f64(p: &Params, key: &str) -> Result<f64, BlockError> {
    get_f64(p, key).ok_or_else(|| BlockError::Params(format!("{key} is required")))
}

/// The single input of a one-input block, checked against `allowed`.
pub(crate) fn single_input(
    inputs: &[PortInfo],
    block: &str,
    allowed: &[PortType],
) -> Result<PortInfo, BlockError> {
    let [input] = inputs else {
        return Err(BlockError::Ports(format!("{block} has exactly one input")));
    };
    if !allowed.contains(&input.ty) {
        return Err(BlockError::PortType {
            port: 0,
            expected: allowed[0],
            got: input.ty,
        });
    }
    if !(input.rate_hz.is_finite() && input.rate_hz > 0.0) {
        return Err(BlockError::Unrealisable(format!(
            "{block} needs a positive input rate"
        )));
    }
    Ok(*input)
}

/// Flags a block copies from its input to its outputs.
pub(crate) const PROPAGATED: ChunkFlags = ChunkFlags(
    ChunkFlags::DISCONTINUITY.0
        | ChunkFlags::RESET.0
        | ChunkFlags::CHANNEL_CHANGE.0
        | ChunkFlags::END.0,
);

/// Whether the chunk's flags require dropping history.
pub(crate) fn restarts(flags: ChunkFlags) -> bool {
    flags.0 & (ChunkFlags::DISCONTINUITY.0 | ChunkFlags::RESET.0) != 0
}

/// Source sample index of input item `item` (an absolute, possibly fractional, item index on
/// the input port), extrapolated from this chunk's time map.
pub(crate) fn source_at(meta: &ChunkMeta, item: f64) -> f64 {
    meta.source_index + (item - meta.index as f64) * meta.source_per_item
}

/// Sets an output's per-chunk metadata: time map, rate, channel and propagated flags.
pub(crate) fn set_meta(out: &mut Output, input: &ChunkMeta, source_index: f64, per_item: f64) {
    out.meta.source_index = source_index;
    out.meta.source_per_item = per_item;
    out.meta.channel = input.channel;
    out.meta.flags |= ChunkFlags(input.flags.0 & PROPAGATED.0);
}

/// Port-type mismatch between an input and an output buffer.
pub(crate) fn mismatch(port: usize, expected: PortType, got: PortType) -> BlockError {
    BlockError::PortType {
        port,
        expected,
        got,
    }
}

/// `iq` items of an input.
pub(crate) fn iq_in<'a>(input: &Input<'a>) -> Result<&'a [Complex32], BlockError> {
    match input.data {
        PortSlice::Iq(x) => Ok(x),
        other => Err(mismatch(0, PortType::Iq, other.port_type())),
    }
}

/// `real` items of an input.
pub(crate) fn real_in<'a>(input: &Input<'a>) -> Result<&'a [f32], BlockError> {
    match input.data {
        PortSlice::Real(x) => Ok(x),
        other => Err(mismatch(0, PortType::Real, other.port_type())),
    }
}

/// `soft` items of an input.
pub(crate) fn soft_in<'a>(input: &Input<'a>) -> Result<&'a [f32], BlockError> {
    match input.data {
        PortSlice::Soft(x) => Ok(x),
        other => Err(mismatch(0, PortType::Soft, other.port_type())),
    }
}

/// `bits` items of an input.
pub(crate) fn bits_in<'a>(input: &Input<'a>) -> Result<&'a [u8], BlockError> {
    match input.data {
        PortSlice::Bits(x) => Ok(x),
        other => Err(mismatch(0, PortType::Bits, other.port_type())),
    }
}

/// The `iq` buffer of an output.
pub(crate) fn iq_out(out: &mut Output) -> Result<&mut Vec<Complex32>, BlockError> {
    match &mut out.data {
        PortVec::Iq(v) => Ok(v),
        other => Err(mismatch(0, PortType::Iq, other.port_type())),
    }
}

/// The `real` buffer of an output.
pub(crate) fn real_out(out: &mut Output) -> Result<&mut Vec<f32>, BlockError> {
    match &mut out.data {
        PortVec::Real(v) => Ok(v),
        other => Err(mismatch(0, PortType::Real, other.port_type())),
    }
}

/// The `soft` buffer of an output.
pub(crate) fn soft_out(out: &mut Output) -> Result<&mut Vec<f32>, BlockError> {
    match &mut out.data {
        PortVec::Soft(v) => Ok(v),
        other => Err(mismatch(0, PortType::Soft, other.port_type())),
    }
}

/// The `bits` buffer of an output.
pub(crate) fn bits_out(out: &mut Output) -> Result<&mut Vec<u8>, BlockError> {
    match &mut out.data {
        PortVec::Bits(v) => Ok(v),
        other => Err(mismatch(0, PortType::Bits, other.port_type())),
    }
}

/// Exponential-average coefficient for time constant `tau_s` at `rate_hz` (1 = no memory).
pub(crate) fn ema_alpha(tau_s: f64, rate_hz: f64) -> f64 {
    if tau_s > 0.0 {
        (1.0 / (tau_s * rate_hz)).min(1.0)
    } else {
        1.0
    }
}

/// Real samples as complex (imaginary 0) into a reused scratch buffer.
pub(crate) fn real_to_complex(src: &[f32], dst: &mut Vec<Complex32>) {
    dst.clear();
    dst.extend(src.iter().map(|&x| Complex32::new(x, 0.0)));
}

/// Whether every key outside `hot` (the schema's hot keys) is unchanged between `old` and
/// `new`: then an update applies in place, otherwise the node is rebuilt.
pub(crate) fn cold_equal(hot: &[&str], old: &Params, new: &Params) -> bool {
    old.iter()
        .chain(new.iter())
        .filter(|(k, _)| !hot.contains(&k.as_str()))
        .all(|(k, _)| old.get(k) == new.get(k))
}
