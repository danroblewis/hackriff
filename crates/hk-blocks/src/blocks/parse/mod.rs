//! Parse blocks (T-089): the declarative parser (`fields`, evaluating an
//! [`hk_recipe::FieldMap`] into a layer tree) and `text` assembly.

use std::sync::Arc;

use hk_recipe::PortType::Frames;
use hk_recipe::{BlockDescriptor, ParamType, PortSpec};

use crate::Registry;
use crate::schema::{ParamExt, descriptor, hex, int, list, one_of, param, string};

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
    ]
}

/// Registers this group's implemented blocks.
pub fn register(r: &mut Registry) {
    r.register(Arc::new(fields::FieldsFactory::new()))
        .expect("fields registered once");
    r.register(Arc::new(text::TextFactory::new()))
        .expect("text registered once");
}
