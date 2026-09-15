//! `identity`: copies its input to its output. The reference implementation of the block
//! contract: any port type, no parameters, flags and time map propagated, no allocation on
//! sample-rate ports once the output is sized.

use hk_recipe::{BlockDescriptor, Params, PortSpec, PortType};

use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::buffer::{ChunkMeta, PortSlice, PortVec};
use crate::registry::{BlockFactory, BuildCtx};
use crate::schema::descriptor as make;
use crate::status::Status;

/// The descriptor.
pub fn descriptor() -> BlockDescriptor {
    make(
        "identity",
        "util",
        "Copies its input to its output unchanged (any port type).",
        vec![PortSpec::any_of("in", &PortType::ALL)],
        vec![PortSpec::any_of("out", &PortType::ALL)],
        vec![],
        true,
    )
}

/// Builds [`Identity`].
pub struct IdentityFactory {
    descriptor: BlockDescriptor,
}

impl IdentityFactory {
    /// The factory.
    pub fn new() -> Self {
        Self {
            descriptor: descriptor(),
        }
    }
}

impl Default for IdentityFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockFactory for IdentityFactory {
    fn descriptor(&self) -> &BlockDescriptor {
        &self.descriptor
    }

    fn build(&self, _params: &Params, _ctx: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
        Ok(Box::new(Identity::default()))
    }
}

/// The block.
#[derive(Debug, Default)]
pub struct Identity {
    ty: Option<PortType>,
    status: Status,
}

impl Block for Identity {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let [input] = inputs else {
            return Err(BlockError::Ports("identity has exactly one input".into()));
        };
        self.ty = Some(input.ty);
        Ok(vec![*input])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let out = io.output(0)?;
        out.meta = ChunkMeta {
            index: out.meta.index,
            ..input.meta
        };
        match (input.data, &mut out.data) {
            (PortSlice::Iq(x), PortVec::Iq(y)) => y.extend_from_slice(x),
            (PortSlice::Real(x), PortVec::Real(y)) | (PortSlice::Soft(x), PortVec::Soft(y)) => {
                y.extend_from_slice(x)
            }
            (PortSlice::Bits(x), PortVec::Bits(y)) => y.extend_from_slice(x),
            (PortSlice::Frames(x), PortVec::Frames(y)) => {
                for f in x.iter() {
                    y.push(f.bytes, f.info.clone());
                }
            }
            (got, expected) => {
                return Err(BlockError::PortType {
                    port: 0,
                    expected: expected.port_type(),
                    got: got.port_type(),
                });
            }
        }
        let n = input.data.len() as u64;
        self.status.items_in += n;
        self.status.items_out += n;
        Ok(())
    }

    fn reset(&mut self) {}

    fn update_params(
        &mut self,
        params: &Params,
        _ctx: &BuildCtx<'_>,
    ) -> Result<ParamUpdate, BlockError> {
        if params.is_empty() {
            Ok(ParamUpdate::Applied)
        } else {
            Err(BlockError::Params("identity has no parameters".into()))
        }
    }

    fn status(&self) -> Status {
        self.status
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{ChunkFlags, FrameBuf, FrameInfo, Input, Output};

    fn run(block: &mut dyn Block, info: PortInfo, data: PortSlice<'_>) -> Output {
        let out = block.init(&[info]).unwrap();
        let mut outputs = vec![Output::for_port(&out[0])];
        outputs[0].begin_chunk();
        let meta = ChunkMeta {
            source_index: 480.0,
            source_per_item: 2.0,
            flags: ChunkFlags::DISCONTINUITY,
            ..ChunkMeta::start(info.rate_hz)
        };
        let inputs = [Input { meta, data }];
        block.process(&mut Io::new(&inputs, &mut outputs)).unwrap();
        outputs.pop().unwrap()
    }

    #[test]
    fn copies_frames_and_propagates_meta() {
        let mut frames = FrameBuf::with_capacity(2, 16);
        frames.push_bits(&[1, 1, 0, 1], FrameInfo::new(0, 480, 3));
        let info = PortInfo {
            ty: PortType::Frames,
            rate_hz: 11.4,
            max_items: 4,
            hold_items: 0,
        };
        let mut b = Identity::default();
        let out = run(&mut b, info, PortSlice::Frames(&frames));
        assert_eq!(out.data.as_slice(), PortSlice::Frames(&frames));
        assert_eq!(out.meta.source_index, 480.0);
        assert!(out.meta.flags.contains(ChunkFlags::DISCONTINUITY));
    }

    #[test]
    fn refuses_a_mismatched_output_and_parameters() {
        let mut b = Identity::default();
        let info = PortInfo {
            ty: PortType::Bits,
            rate_hz: 1.0,
            max_items: 4,
            hold_items: 0,
        };
        b.init(&[info]).unwrap();
        let mut outputs = vec![Output::for_port(&PortInfo {
            ty: PortType::Soft,
            ..info
        })];
        let bits = [1u8];
        let inputs = [Input {
            meta: ChunkMeta::start(1.0),
            data: PortSlice::Bits(&bits),
        }];
        let err = b.process(&mut Io::new(&inputs, &mut outputs)).unwrap_err();
        assert!(matches!(err, BlockError::PortType { .. }));
        let mut p = Params::new();
        p.insert("x".into(), 1.into());
        let maps = std::collections::BTreeMap::new();
        let ctx = BuildCtx {
            field_maps: &maps,
            input_types: &[PortType::Bits],
        };
        assert!(b.update_params(&p, &ctx).is_err());
    }
}
