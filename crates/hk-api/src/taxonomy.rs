//! `GET /api/taxonomy` (T-218, ADR-0016 §1–§2, §9): the modulation taxonomy and the threshold set
//! the classifier decides with, as data.
//!
//! The UI is a thin client (CLAUDE.md): it must never carry its own copy of the family tree, the
//! label spellings or the SNR gates, because a stale copy would show a different story from the
//! one the backend decided. This route is that single source: the released taxonomies, which one
//! new rows are written under, and `thresholds@1`.
//!
//! It is **reference data, not measurement**: no emitter, no detection and no identity is
//! reachable here, so it needs no gating beyond the API token. It also never pre-populates
//! anything — a client that wants to know what was *found* asks `/api/inventory`.

use hk_model::classify::taxonomy::{CURRENT, TAXONOMIES, Taxonomy, UNKNOWN};
use hk_model::classify::thresholds::{THRESHOLDS, THRESHOLDS_VERSION};
use hk_model::classify::{LAMBDA0_MIN, MAX_CONFIDENCE, TaxonomyRef};
use serde_json::{Value, json};

fn taxonomy_value(t: &Taxonomy) -> Value {
    json!({
        "ref": TaxonomyRef::of(t).to_string(),
        "name": t.name,
        "version": t.version,
        "families": t.families.iter().map(|f| json!({
            "family": f.name,
            "coarse": f.coarse.as_str(),
            "classes": f.classes,
        })).collect::<Vec<_>>(),
        "legacy": t.legacy.iter().map(|(label, family)| json!({
            "label": label, "family": family,
        })).collect::<Vec<_>>(),
    })
}

/// The document `GET /api/taxonomy` answers.
pub fn taxonomy_json() -> Value {
    json!({
        "current": TaxonomyRef::current().to_string(),
        "unknown": UNKNOWN,
        "taxonomies": TAXONOMIES.iter().map(taxonomy_value).collect::<Vec<_>>(),
        "thresholds": {
            "version": THRESHOLDS_VERSION,
            "max_confidence": MAX_CONFIDENCE,
            "lambda0_min": LAMBDA0_MIN,
            "families": THRESHOLDS.iter().map(|t| json!({
                "family": t.family,
                "snr_gate_db": t.snr_gate_db,
                "class_gate_db": t.class_gate_db,
                "min_confidence": t.min_confidence,
                "open_set_max": t.open_set_max,
            })).collect::<Vec<_>>(),
        },
        "coarse": CURRENT.families.iter().map(|f| f.coarse.as_str())
            .chain(std::iter::once(UNKNOWN))
            .collect::<std::collections::BTreeSet<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_document_carries_every_family_its_classes_and_its_gates() {
        let v = taxonomy_json();
        assert_eq!(v["current"], "hk-mod@1");
        assert_eq!(v["unknown"], "unknown");
        let tax = &v["taxonomies"][0];
        assert_eq!(tax["ref"], "hk-mod@1");
        let families: Vec<&str> = tax["families"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["family"].as_str().unwrap())
            .collect();
        assert_eq!(families.len(), CURRENT.families.len());
        assert!(families.contains(&"fsk") && families.contains(&"noise-like"));
        let fsk = tax["families"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["family"] == "fsk")
            .unwrap();
        assert_eq!(fsk["coarse"], "digital");
        assert_eq!(fsk["classes"][0], "2fsk");
        // The legacy map is served too: a client reading an old row can map its label.
        assert!(
            tax["legacy"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l["label"] == "fsk2" && l["family"] == "fsk")
        );

        assert_eq!(v["thresholds"]["version"], "thresholds@1");
        let gated = v["thresholds"]["families"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["family"] == "fsk")
            .unwrap();
        assert_eq!(gated["snr_gate_db"], 20.0, "the S5 floor is served as-is");
        let ungated = v["thresholds"]["families"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["family"] == "noise-like")
            .unwrap();
        assert!(ungated["snr_gate_db"].is_null(), "noise-like has no gate");
        assert_eq!(v["thresholds"]["lambda0_min"], 0.1);
    }

    #[test]
    fn nothing_measured_is_reachable_through_it() {
        let v = taxonomy_json();
        let text = v.to_string();
        for forbidden in ["emitter", "identity", "detection", "f_center_hz"] {
            assert!(
                !text.contains(forbidden),
                "{forbidden} leaked into taxonomy"
            );
        }
    }
}
