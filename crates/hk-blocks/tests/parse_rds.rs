//! T-089: the RDS worked recipe's parse tail (`group` fields → `ps`/`rt` text, ADR-0011 §5)
//! over synthetic RDS groups, built from the recipe document itself through the block registry.
//! PI, PTY, PS and RadioText are asserted against hidden truth and cross-checked group by group
//! against the existing `hk_demod::rds` decoder (the tutorial's oracle) fed the same groups as
//! checkworded 26-bit blocks.

use std::collections::BTreeMap;

use hk_blocks::{
    Block, BuildCtx, ChunkFlags, ChunkMeta, FrameBuf, FrameInfo, Input, Io, Output, PortInfo,
    PortSlice, Registry,
};
use hk_demod::rds::{GroupConfig, Offset, RdsDecoder, encode_block};
use hk_model::CrcStatus;
use hk_recipe::{PortType, Recipe};
use hk_stream::inspector::FitStatus;
use serde_json::{Value, json};

const PI: u16 = 0x54a8;
const PTY: u16 = 10;
const PS: &[u8; 8] = b"HACKRIFF";
const RT: &str = "Blind first, then explain";

fn recipe() -> Recipe {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../recipes/rds.recipe.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Group 0A carrying PS segment `seg`, as four 16-bit information words.
fn group_0a(seg: usize) -> [u16; 4] {
    let b2 = (PTY << 5) | (1 << 10) | (0b010 << 2) | seg as u16;
    let chars = (u16::from(PS[2 * seg]) << 8) | u16::from(PS[2 * seg + 1]);
    [PI, b2, 0xe0cd, chars]
}

/// Group 2A carrying RadioText segment `seg` (A/B flag 0); the text ends with CR, then spaces.
fn group_2a(seg: usize) -> [u16; 4] {
    let mut text: Vec<u8> = RT.bytes().collect();
    text.push(0x0d);
    text.resize(64, b' ');
    let c = &text[4 * seg..4 * seg + 4];
    let b2 = (2 << 12) | (1 << 10) | (PTY << 5) | seg as u16;
    [
        PI,
        b2,
        (u16::from(c[0]) << 8) | u16::from(c[1]),
        (u16::from(c[2]) << 8) | u16::from(c[3]),
    ]
}

fn frames_port() -> PortInfo {
    PortInfo {
        ty: PortType::Frames,
        rate_hz: 11.4,
        max_items: 64,
        hold_items: 0,
    }
}

fn build(registry: &Registry, recipe: &Recipe, node: &str) -> Box<dyn Block> {
    let spec = recipe.nodes.iter().find(|n| n.id == node).unwrap();
    let ctx = BuildCtx {
        field_maps: &recipe.field_maps,
        input_types: &[PortType::Frames],
    };
    let mut b = registry.build(&spec.block, &spec.params, &ctx).unwrap();
    b.init(&[frames_port()]).unwrap();
    b
}

fn step(block: &mut dyn Block, input: &FrameBuf, flags: ChunkFlags) -> FrameBuf {
    let mut outputs = vec![Output::for_port(&frames_port())];
    outputs[0].begin_chunk();
    let inputs = [Input {
        meta: ChunkMeta {
            flags,
            ..ChunkMeta::start(11.4)
        },
        data: PortSlice::Frames(input),
    }];
    block.process(&mut Io::new(&inputs, &mut outputs)).unwrap();
    match outputs.pop().unwrap().data {
        hk_blocks::PortVec::Frames(f) => f,
        _ => unreachable!(),
    }
}

fn node_value(f: &hk_blocks::Frame<'_>, path: &str) -> Option<Value> {
    f.info.layers.as_ref()?.node(path)?.value.clone()
}

#[test]
fn rds_recipe_parse_tail_recovers_pi_pty_ps_and_radiotext() {
    let recipe = recipe();
    let registry = Registry::builtin();
    let mut group = build(&registry, &recipe, "group");
    let mut ps = build(&registry, &recipe, "ps");
    let mut rt = build(&registry, &recipe, "rt");

    // Three cycles: PS segments 0–3 then RadioText segments 0–6, one group per chunk-frame.
    let mut groups = Vec::new();
    for _ in 0..3 {
        groups.extend((0..4).map(group_0a));
        groups.extend((0..7).map(group_2a));
    }

    // Oracle: the existing decoder over the same groups as checkworded blocks.
    let mut oracle = RdsDecoder::new(GroupConfig::default(), 1187.5);
    let mut bit = 0.0;
    for g in &groups {
        let offsets = [Offset::A, Offset::B, Offset::C, Offset::D];
        for (info, off) in g.iter().zip(offsets) {
            let block = encode_block(*info, off);
            for k in (0..26).rev() {
                oracle.push_bit(((block >> k) & 1) as u8, bit);
                bit += 1.0;
            }
        }
    }
    let oracle_groups = oracle.take_groups();
    assert!(
        oracle_groups.len() >= groups.len() - 2,
        "oracle synced late"
    );

    let mut input = FrameBuf::with_capacity(groups.len(), groups.len() * 8);
    for (i, g) in groups.iter().enumerate() {
        let bytes: Vec<u8> = g.iter().flat_map(|w| w.to_be_bytes()).collect();
        let mut info = FrameInfo::new(i as u64, i as u64 * 20_880, 0);
        info.bit_len = 64;
        info.check = CrcStatus::Valid;
        input.push(&bytes, info);
    }
    let parsed = step(group.as_mut(), &input, ChunkFlags::DISCONTINUITY);
    assert_eq!(parsed.len(), groups.len());

    for f in parsed.iter() {
        let tree = f.info.layers.as_ref().unwrap();
        assert_eq!(tree.fit, FitStatus::Ok, "{:?}", tree.errors);
        assert_eq!(node_value(&f, "pi"), Some(json!(PI)));
        assert_eq!(tree.node("pi").unwrap().text.as_deref(), Some("0x54A8"));
        assert_eq!(node_value(&f, "pty"), Some(json!(PTY)));
        assert_eq!(tree.node("version").unwrap().text.as_deref(), Some("A"));
        assert_eq!(tree.node("tp").unwrap().text.as_deref(), Some("true"));
    }
    // Cross-check every group the oracle decoded, matched by its bit position.
    for og in &oracle_groups {
        let f = parsed.get((og.position / 104.0).round() as usize).unwrap();
        assert_eq!(og.pi.map(|p| json!(p)), node_value(&f, "pi"));
        assert_eq!(og.pty.map(|p| json!(p)), node_value(&f, "pty"));
        let (gt, b) = og.group_type.unwrap();
        assert_eq!(Some(json!(gt)), node_value(&f, "group_type"));
        assert_eq!(Some(json!(u8::from(b))), node_value(&f, "version"));
        assert_eq!(og.tp.map(|x| json!(u8::from(x))), node_value(&f, "tp"));
        if let Some((seg, chars)) = og.ps_segment {
            assert_eq!(Some(json!(seg)), node_value(&f, "ps.segment"));
            let text = String::from_utf8(chars.to_vec()).unwrap();
            assert_eq!(Some(json!(text)), node_value(&f, "ps.chars"));
        } else {
            assert!(node_value(&f, "ps.segment").is_none());
        }
    }
    assert_eq!(oracle.report().ps(), Some("HACKRIFF"));

    // PS: one assembled string per completed cycle, keyed by PI.
    let names = step(ps.as_mut(), &parsed, ChunkFlags::NONE);
    assert_eq!(names.len(), 3);
    for f in names.iter() {
        assert_eq!(f.bytes, PS);
        assert_eq!(node_value(&f, "ps.text"), Some(json!("HACKRIFF")));
        assert_eq!(node_value(&f, "ps.key"), Some(json!(PI)));
        let tree = f.info.layers.as_ref().unwrap();
        let text = tree.node("ps.text").unwrap();
        assert_eq!(tree.fields_at_byte(7), &[text.id]);
    }
    assert_eq!(
        ps.status()
            .extra
            .iter()
            .find(|(k, _)| *k == "strings")
            .unwrap()
            .1,
        3.0
    );

    // RadioText: ends at the CR; segments after it are not waited for.
    let texts = step(rt.as_mut(), &parsed, ChunkFlags::NONE);
    assert_eq!(texts.len(), 3);
    assert_eq!(
        node_value(&texts.get(0).unwrap(), "radiotext.text"),
        Some(json!(RT))
    );

    // A discontinuity drops partial state: half a PS cycle emits nothing.
    let mut half = FrameBuf::with_capacity(2, 16);
    for f in parsed.iter().take(2) {
        half.push(f.bytes, f.info.clone());
    }
    assert!(step(ps.as_mut(), &half, ChunkFlags::DISCONTINUITY).is_empty());

    // Frames that failed their check are ignored by text assembly.
    let mut bad = FrameBuf::with_capacity(4, 32);
    for f in parsed.iter().take(4) {
        let mut info = f.info.clone();
        info.check = CrcStatus::Invalid;
        bad.push(f.bytes, info);
    }
    assert!(step(ps.as_mut(), &bad, ChunkFlags::DISCONTINUITY).is_empty());
}

#[test]
fn text_emit_is_hot_and_other_params_rebuild() {
    let recipe = recipe();
    let registry = Registry::builtin();
    let mut ps = build(&registry, &recipe, "ps");
    let spec = recipe.nodes.iter().find(|n| n.id == "ps").unwrap();
    let empty = BTreeMap::new();
    let ctx = BuildCtx {
        field_maps: &empty,
        input_types: &[PortType::Frames],
    };
    let mut hot = spec.params.clone();
    hot.insert("emit".into(), json!("on-change"));
    assert_eq!(
        ps.update_params(&hot, &ctx).unwrap(),
        hk_blocks::ParamUpdate::Applied
    );
    let mut cold = hot.clone();
    cold.insert("segments".into(), json!(8));
    assert_eq!(
        ps.update_params(&cold, &ctx).unwrap(),
        hk_blocks::ParamUpdate::Rebuild
    );
}
