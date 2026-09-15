//! Parse blocks (T-089): the declarative parser (`fields`, evaluating an
//! [`hk_recipe::FieldMap`] into a layer tree), `text` assembly and `consensus` (T-210: field
//! values pass only once enough agreeing frames back them).

use std::sync::Arc;

use hk_recipe::PortType::Frames;
use hk_recipe::{BlockDescriptor, ParamType, PortSpec};

use crate::Registry;
use crate::schema::{ParamExt, boolean, descriptor, hex, int, list, object, one_of, param, string};

pub mod consensus;
pub mod fields;
pub mod text;

/// The pinned descriptor named `name`.
fn pinned(name: &str) -> BlockDescriptor {
    planned()
        .into_iter()
        .find(|d| d.name == name)
        .expect("parse block pinned in planned()")
}

/// Pinned descriptors of this group.
pub fn planned() -> Vec<BlockDescriptor> {
    let frames = || {
        (
            vec![PortSpec::new("in", Frames)],
            vec![PortSpec::new("out", Frames)],
        )
    };
    let (f_in, f_out) = frames();
    let (t_in, t_out) = frames();
    let (c_in, c_out) = frames();
    vec![
        descriptor(
            "fields",
            "parse",
            "Evaluates a field map against each frame and attaches the layer tree (bytes unchanged).",
            f_in,
            f_out,
            vec![
                param("map", ParamType::FieldMap, "Field map id in the recipe.")
                    .required()
                    .hot(),
                param(
                    "skip_invalid",
                    boolean(),
                    "Frames whose check is invalid pass through without a layer tree (no fields from corrupt data); false parses them too (a protocol whose check fails by design, e.g. Mode S address/parity overlay).",
                )
                .default_value(false)
                .hot(),
            ],
            true,
        ),
        descriptor(
            "text",
            "parse",
            "Assembles segmented text carried across frames (RDS PS/RadioText) and emits one frame per assembled string; its layer tree is {<name>.key, <name>.text}.",
            t_in,
            t_out,
            vec![
                param(
                    "name",
                    string(64),
                    "Top-level layer name of the output tree.",
                )
                .required(),
                param(
                    "key",
                    ParamType::FieldPath,
                    "Integer field identifying the source (e.g. PI); a change restarts assembly.",
                ),
                param(
                    "address",
                    list(ParamType::FieldPath, 1),
                    "Segment address field(s); the first present wins.",
                )
                .required(),
                param(
                    "chars",
                    list(ParamType::FieldPath, 1),
                    "Character field(s) holding one segment; the first present wins.",
                )
                .required(),
                param(
                    "segments",
                    int(1, 256),
                    "Number of segments in a complete string.",
                )
                .required(),
                param(
                    "chars_per_segment",
                    int(1, 64),
                    "Characters per segment; absent: the chars field's length.",
                ),
                param(
                    "reset_on",
                    ParamType::FieldPath,
                    "Field whose change clears the string (RadioText A/B flag).",
                ),
                param(
                    "terminator",
                    hex(8),
                    "Character that ends the string early (RadioText 0x0D).",
                ),
                param(
                    "emit",
                    one_of(&["on-complete", "on-change"]),
                    "Emit when every segment is in, or on every change.",
                )
                .default_value("on-complete")
                .hot(),
                param(
                    "charset",
                    one_of(&["ascii", "latin1", "rds"]),
                    "Character set.",
                )
                .default_value("ascii"),
            ],
            true,
        ),
        descriptor(
            "consensus",
            "parse",
            "Commits field values (an identity key, and per-address segments) only once agreeing frames reach a weight (clean CRC-valid frames weigh more than corrected ones) and withholds every other value (node value cleared, marked error), so a false correction never surfaces (T-210).",
            c_in,
            c_out,
            vec![
                param(
                    "key",
                    ParamType::FieldPath,
                    "Identity field (RDS PI): the other fields count and pass only while a frame's key equals the committed key; a newly committed key clears their state.",
                ),
                param(
                    "fields",
                    list(
                        object(vec![
                            param("field", ParamType::FieldPath, "Field whose value needs consensus.")
                                .required(),
                            param(
                                "address",
                                ParamType::FieldPath,
                                "Segment address: one consensus slot per address value (RDS PS segment).",
                            ),
                        ]),
                        0,
                    ),
                    "Fields committed by consensus.",
                ),
                param(
                    "clean_weight",
                    int(1, 16),
                    "Weight of an observation from a CRC-valid frame.",
                )
                .default_value(2),
                param(
                    "corrected_weight",
                    int(0, 16),
                    "Weight of an observation from any other non-invalid frame (corrected, no-crc, unknown); below commit_weight.",
                )
                .default_value(1),
                param(
                    "commit_weight",
                    int(1, 64),
                    "Agreeing weight within the window that commits a value (defaults: two observations with one clean, or three corrected).",
                )
                .default_value(3),
                param(
                    "window",
                    int(1, 32),
                    "Observations per slot counted (the most recent).",
                )
                .default_value(8),
            ],
            true,
        ),
    ]
}

/// Registers this group's implemented blocks.
pub fn register(r: &mut Registry) {
    r.register(Arc::new(fields::FieldsFactory::new()))
        .expect("fields registered once");
    r.register(Arc::new(text::TextFactory::new()))
        .expect("text registered once");
    r.register(Arc::new(consensus::ConsensusFactory::new()))
        .expect("consensus registered once");
}
