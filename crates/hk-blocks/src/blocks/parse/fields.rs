//! `fields`: evaluates a recipe field map against every frame and attaches the layer tree
//! (`FrameInfo::layers`, shared as `Arc<LayerTree>`); bytes and every other frame property pass
//! through unchanged (ADR-0011 §3).
//!
//! **Status** is the fit rate since the map last changed: `quality` = share of frames that fit
//! fully, `error_rate` = 1 − quality, extras `frames_ok`, `frames_partial`, `frames_failed`.
//! **Hot edit:** `update_params` recompiles the named map from the new recipe's `field_maps`
//! and applies it in place at the chunk boundary (the fit counters restart; no frame state).

use std::sync::Arc;

use hk_recipe::fields::eval::Evaluator;
use hk_recipe::{BlockDescriptor, Params, PortType};
use hk_stream::inspector::FitStatus;
use serde_json::Value;

use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::buffer::{ChunkMeta, PortSlice, PortVec};
use crate::registry::{BlockFactory, BuildCtx};
use crate::status::Status;

/// Builds [`Fields`].
pub struct FieldsFactory {
    descriptor: BlockDescriptor,
}

impl FieldsFactory {
    /// The factory (descriptor pinned in [`super::planned`]).
    pub fn new() -> Self {
        Self {
            descriptor: super::pinned("fields"),
        }
    }
}

impl Default for FieldsFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockFactory for FieldsFactory {
    fn descriptor(&self) -> &BlockDescriptor {
        &self.descriptor
    }

    fn build(&self, params: &Params, ctx: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
        let (map_id, evaluator) = compile(params, ctx)?;
        Ok(Box::new(Fields {
            map_id,
            evaluator,
            counts: [0; 3],
            status: Status::default(),
        }))
    }
}

/// Resolves and compiles the `map` parameter's field map.
fn compile(params: &Params, ctx: &BuildCtx<'_>) -> Result<(String, Evaluator), BlockError> {
    if let Some(k) = params.keys().find(|k| k.as_str() != "map") {
        return Err(BlockError::Params(format!("fields has no parameter {k}")));
    }
    let id = params
        .get("map")
        .and_then(Value::as_str)
        .ok_or_else(|| BlockError::Params("map (a field-map id) is required".into()))?;
    let map = ctx
        .field_map(id)
        .ok_or_else(|| BlockError::Params(format!("the recipe has no field map {id}")))?;
    let evaluator = Evaluator::new(map).map_err(|errors| {
        let first = errors.first().map(ToString::to_string).unwrap_or_default();
        BlockError::Params(format!(
            "field map {id} is invalid ({} errors; first: {first})",
            errors.len()
        ))
    })?;
    Ok((id.to_owned(), evaluator))
}

/// The block.
pub struct Fields {
    map_id: String,
    evaluator: Evaluator,
    /// Frames ok, partial, failed since the map was (re)compiled.
    counts: [u64; 3],
    status: Status,
}

impl Fields {
    /// Field-map id in use.
    pub fn map_id(&self) -> &str {
        &self.map_id
    }

    fn refresh_status(&mut self) {
        let [ok, partial, failed] = self.counts;
        let total = ok + partial + failed;
        if total > 0 {
            let q = ok as f32 / total as f32;
            self.status.quality = Some(q);
            self.status.error_rate = Some(1.0 - q);
        } else {
            self.status.quality = None;
            self.status.error_rate = None;
        }
        self.status.extra.set("frames_ok", ok as f64);
        self.status.extra.set("frames_partial", partial as f64);
        self.status.extra.set("frames_failed", failed as f64);
    }
}

impl Block for Fields {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let [input] = inputs else {
            return Err(BlockError::Ports("fields has exactly one input".into()));
        };
        if input.ty != PortType::Frames {
            return Err(BlockError::PortType {
                port: 0,
                expected: PortType::Frames,
                got: input.ty,
            });
        }
        self.refresh_status();
        Ok(vec![*input])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let PortSlice::Frames(frames) = input.data else {
            return Err(BlockError::PortType {
                port: 0,
                expected: PortType::Frames,
                got: input.data.port_type(),
            });
        };
        let out = io.output(0)?;
        out.meta = ChunkMeta {
            index: out.meta.index,
            ..input.meta
        };
        let PortVec::Frames(buf) = &mut out.data else {
            return Err(BlockError::PortType {
                port: 0,
                expected: PortType::Frames,
                got: out.data.port_type(),
            });
        };
        for f in frames.iter() {
            let tree = self.evaluator.eval(f.bytes, f.info.bit_len);
            match tree.fit {
                FitStatus::Ok | FitStatus::None => self.counts[0] += 1,
                FitStatus::Partial => self.counts[1] += 1,
                FitStatus::Failed => self.counts[2] += 1,
            }
            let mut info = f.info.clone();
            info.layers = Some(Arc::new(tree));
            buf.push(f.bytes, info);
        }
        let n = frames.len() as u64;
        self.status.items_in += n;
        self.status.items_out += n;
        self.refresh_status();
        Ok(())
    }

    fn reset(&mut self) {}

    fn update_params(
        &mut self,
        params: &Params,
        ctx: &BuildCtx<'_>,
    ) -> Result<ParamUpdate, BlockError> {
        let (map_id, evaluator) = compile(params, ctx)?;
        self.map_id = map_id;
        self.evaluator = evaluator;
        self.counts = [0; 3];
        self.refresh_status();
        Ok(ParamUpdate::Applied)
    }

    fn status(&self) -> Status {
        self.status
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use hk_model::CrcStatus;
    use hk_recipe::FieldMap;
    use serde_json::json;

    use super::*;
    use crate::buffer::{ChunkFlags, FrameBuf, FrameInfo, Input, Output};

    fn maps(width: u32) -> BTreeMap<String, FieldMap> {
        let m: FieldMap = serde_json::from_value(json!({
            "unit": "bits",
            "fields": [{"name": "kind", "type": "uint", "length": width}]
        }))
        .unwrap();
        BTreeMap::from([("m".to_owned(), m)])
    }

    fn params() -> Params {
        json!({"map": "m"}).as_object().unwrap().clone()
    }

    fn frames_port() -> PortInfo {
        PortInfo {
            ty: PortType::Frames,
            rate_hz: 10.0,
            max_items: 8,
            hold_items: 0,
        }
    }

    fn run(block: &mut dyn Block, frames: &FrameBuf) -> Output {
        let mut outputs = vec![Output::for_port(&frames_port())];
        outputs[0].begin_chunk();
        let inputs = [Input {
            meta: ChunkMeta {
                flags: ChunkFlags::DISCONTINUITY,
                ..ChunkMeta::start(10.0)
            },
            data: PortSlice::Frames(frames),
        }];
        block.process(&mut Io::new(&inputs, &mut outputs)).unwrap();
        outputs.pop().unwrap()
    }

    #[test]
    fn attaches_layers_counts_fit_and_hot_swaps_the_map() {
        let m = maps(4);
        let ctx = BuildCtx {
            field_maps: &m,
            input_types: &[PortType::Frames],
        };
        let mut b = crate::Registry::builtin()
            .build("fields", &params(), &ctx)
            .unwrap();
        b.init(&[frames_port()]).unwrap();
        let mut frames = FrameBuf::with_capacity(2, 8);
        let mut info = FrameInfo::new(0, 100, 0);
        info.check = CrcStatus::Valid;
        frames.push_bits(&[1, 0, 1, 1, 0, 0, 0, 0], info);
        frames.push_bits(&[1, 1], FrameInfo::new(1, 200, 0));
        let out = run(b.as_mut(), &frames);
        let PortSlice::Frames(got) = out.data.as_slice() else {
            panic!()
        };
        let f0 = got.get(0).unwrap();
        assert_eq!(f0.bytes, frames.get(0).unwrap().bytes);
        assert_eq!(f0.info.check, CrcStatus::Valid);
        let t0 = f0.info.layers.as_ref().unwrap();
        assert_eq!(t0.node("kind").unwrap().value, Some(json!(11)));
        assert_eq!(
            got.get(1).unwrap().info.layers.as_ref().unwrap().fit,
            FitStatus::Failed
        );
        assert!(out.meta.flags.contains(ChunkFlags::DISCONTINUITY));
        let s = b.status();
        assert_eq!(s.quality, Some(0.5));
        assert_eq!((s.items_in, s.items_out), (2, 2));

        // Hot edit: a 2-bit map now fits both frames; applied in place.
        let m2 = maps(2);
        let ctx2 = BuildCtx {
            field_maps: &m2,
            input_types: &[PortType::Frames],
        };
        assert_eq!(
            b.update_params(&params(), &ctx2).unwrap(),
            ParamUpdate::Applied
        );
        let out = run(b.as_mut(), &frames);
        let PortSlice::Frames(got) = out.data.as_slice() else {
            panic!()
        };
        assert_eq!(
            got.get(1)
                .unwrap()
                .info
                .layers
                .as_ref()
                .unwrap()
                .node("kind")
                .unwrap()
                .value,
            Some(json!(3))
        );
        assert_eq!(b.status().quality, Some(1.0));

        // A map the recipe lacks is refused and the running map is kept.
        let empty = BTreeMap::new();
        let ctx3 = BuildCtx {
            field_maps: &empty,
            input_types: &[PortType::Frames],
        };
        assert!(b.update_params(&params(), &ctx3).is_err());
    }
}
