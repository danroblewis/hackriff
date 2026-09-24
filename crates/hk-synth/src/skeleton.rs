//! Skeletons: the structure space (ADR-0015 §1.2). A skeleton offers alternative node sub-chains
//! per stage slot — `slots: {S3: [{id: "none", nodes: []}, {id: "nrzi", nodes: [...]}]}` — and a
//! candidate records which alternative it fixed in `choices`.
//!
//! A skeleton is always expressed in **ADR-0011's own blocks**, never transcribed from another
//! decoder's graph (§15.3). The built-in generic skeletons (`generic-fsk-framed`,
//! `generic-ook-pwm`, `generic-ook-manchester`, `generic-msk`, `generic-ppm`) ship as templates
//! with a skeleton body and are M-5's.

use std::collections::BTreeMap;

use hk_recipe::NodeSpec;
use serde::{Deserialize, Serialize};

use crate::stage::Stage;

/// One alternative for a stage slot: an id and the nodes it contributes (possibly none).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotAlternative {
    /// Alternative id, recorded in a candidate's `choices` (`"none"`, `"nrzi"`, `"fsk"`).
    pub id: String,
    /// The nodes this alternative inserts, in order.
    #[serde(default)]
    pub nodes: Vec<NodeSpec>,
    /// The `hk-mod@1` family this alternative commits to, when it commits to one (an S1 demod
    /// choice). Carried so the trace can filter by family ("why not PSK", ADR-0021 §2.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
}

/// The slot table of a skeleton, as a template's `skeleton` body carries it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkeletonSlots {
    /// Alternatives per stage. A stage absent here has no slot in this skeleton.
    pub slots: BTreeMap<Stage, Vec<SlotAlternative>>,
}

/// A named, versioned skeleton.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Skeleton {
    /// Stable id.
    pub id: String,
    /// Version; immutable once saved (ADR-0011 §2.4's rule).
    pub version: u32,
    /// Alternatives per stage (the same table as [`SkeletonSlots::slots`]).
    pub slots: BTreeMap<Stage, Vec<SlotAlternative>>,
}

impl Skeleton {
    /// A skeleton from a template's `id`, `version` and skeleton body.
    pub fn from_body(id: impl Into<String>, version: u32, body: SkeletonSlots) -> Self {
        Self {
            id: id.into(),
            version,
            slots: body.slots,
        }
    }

    /// `id@version`, the form a candidate's `skeleton` field and a trace node carry.
    pub fn key(&self) -> String {
        format!("{}@{}", self.id, self.version)
    }

    /// The alternatives offered at `stage`, empty when the skeleton has no slot there.
    pub fn alternatives(&self, stage: Stage) -> &[SlotAlternative] {
        self.slots.get(&stage).map_or(&[], Vec::as_slice)
    }

    /// The stages this skeleton has slots for, shallowest first.
    pub fn stages(&self) -> impl Iterator<Item = Stage> + '_ {
        self.slots.keys().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_parse_in_the_adr_shape() {
        let s: Skeleton = serde_json::from_str(
            r#"{ "id": "generic-fsk-framed", "version": 1,
                 "slots": {
                   "S1": [ { "id": "fsk", "family": "fsk",
                             "nodes": [ { "id": "fsk", "block": "fsk_demod" } ] } ],
                   "S3": [ { "id": "none", "nodes": [] },
                           { "id": "nrzi", "nodes": [ { "id": "nrzi", "block": "nrzi" } ] } ] } }"#,
        )
        .unwrap();
        assert_eq!(s.key(), "generic-fsk-framed@1");
        assert_eq!(s.stages().collect::<Vec<_>>(), [Stage::S1, Stage::S3]);
        assert_eq!(s.alternatives(Stage::S3).len(), 2);
        assert!(s.alternatives(Stage::S2).is_empty());
        assert_eq!(s.alternatives(Stage::S1)[0].family.as_deref(), Some("fsk"));
    }
}
