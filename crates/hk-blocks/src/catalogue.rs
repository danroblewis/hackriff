//! The M1 block catalogue as pinned by ADR-0011 §1.5.
//!
//! [`planned`] lists every M1 block's descriptor: names, groups and port signatures are final
//! for all of them; parameter schemas are final (`params_pinned`) for the blocks the RDS
//! reference recipe uses and placeholders for the rest, which their implementing task pins.
//! Recipes validate against it before the blocks exist (`recipes/rds.recipe.json`), and an
//! implemented block must publish exactly its pinned descriptor (test below).

use hk_recipe::BlockDescriptor;

use crate::blocks;

/// Every M1 block's descriptor, implemented or not (implemented blocks' own descriptors win
/// for unpinned entries).
pub fn planned() -> Vec<BlockDescriptor> {
    let registry = crate::Registry::builtin();
    let mut all = Vec::new();
    all.extend(blocks::iq::planned());
    all.extend(blocks::symbol::planned());
    all.extend(blocks::framing::planned());
    all.extend(blocks::fec::planned());
    all.extend(blocks::parse::planned());
    all.extend(blocks::multi::planned());
    for d in &mut all {
        if !d.params_pinned
            && let Some(f) = registry.get(&d.name)
        {
            *d = f.descriptor().clone();
        }
    }
    for d in registry.descriptors() {
        if !all.iter().any(|p| p.name == d.name) {
            all.push(d.clone());
        }
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_recipe::Catalogue;
    use std::collections::BTreeSet;

    #[test]
    fn names_are_unique_and_ports_are_typed() {
        let all = planned();
        let names: BTreeSet<&str> = all.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names.len(), all.len());
        for d in &all {
            assert!(
                d.name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{}",
                d.name
            );
            for p in d.inputs.iter().chain(&d.outputs) {
                assert!(!p.types.is_empty(), "{}.{}", d.name, p.name);
            }
            for o in &d.outputs {
                // Polymorphic outputs need an input to follow.
                assert!(o.types.len() == 1 || !d.inputs.is_empty(), "{}", d.name);
            }
        }
        for name in [
            "mix",
            "lowpass",
            "resample",
            "fm_demod",
            "am_demod",
            "fsk_demod",
            "msk_demod",
            "ppm_demod",
            "subcarrier",
            "clock_recovery",
            "slicer",
            "diff_decode",
            "nrzi",
            "manchester",
            "sync_search",
            "deframe",
            "interleave",
            "deinterleave",
            "crc",
            "bch",
            "parity",
            "checksum",
            "fields",
            "text",
            "follow_hops",
            "identity",
        ] {
            assert!(all.descriptor(name).is_some(), "{name} missing");
        }
    }

    #[test]
    fn implemented_blocks_match_their_pinned_descriptors() {
        let pinned: Vec<BlockDescriptor> = [
            blocks::iq::planned(),
            blocks::symbol::planned(),
            blocks::framing::planned(),
            blocks::fec::planned(),
            blocks::parse::planned(),
            blocks::multi::planned(),
        ]
        .concat();
        for d in crate::Registry::builtin().descriptors() {
            if let Some(p) = pinned.iter().find(|p| p.name == d.name) {
                assert_eq!(
                    p.inputs, d.inputs,
                    "{} inputs drifted from ADR-0011",
                    d.name
                );
                assert_eq!(
                    p.outputs, d.outputs,
                    "{} outputs drifted from ADR-0011",
                    d.name
                );
                if p.params_pinned {
                    assert_eq!(
                        p.params, d.params,
                        "{} params drifted from ADR-0011",
                        d.name
                    );
                }
            }
        }
    }
}
