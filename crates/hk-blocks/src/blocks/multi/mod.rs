//! Multi-channel blocks (T-093). `follow_hops` is the merge point of a follow-hops recipe:
//! the runtime instantiates everything upstream of it once per channel and feeds each
//! instance's frames (tagged with `FrameInfo::channel`) into this one node (ADR-0011 §2.5).

use hk_recipe::PortType::Frames;
use hk_recipe::{BlockDescriptor, PortSpec};

use crate::Registry;
use crate::schema::{ParamExt, descriptor, float, param};

/// Pinned descriptors of this group.
pub fn planned() -> Vec<BlockDescriptor> {
    vec![descriptor(
        "follow_hops",
        "multi",
        "Merges frames from every followed channel in source-time order and drops duplicates (the same bytes on another channel within dedupe_s).",
        vec![PortSpec::new("in", Frames)],
        vec![PortSpec::new("out", Frames)],
        vec![
            param("dedupe_s", float(0.0, 60.0, "s"), "Duplicate window.")
                .default_value(0.5)
                .hot(),
            param(
                "order_window_s",
                float(0.0, 5.0, "s"),
                "Reordering delay (bounded latency).",
            )
            .default_value(0.2),
        ],
        true,
    )]
}

/// Registers this group's implemented blocks (none yet).
pub fn register(_r: &mut Registry) {}
