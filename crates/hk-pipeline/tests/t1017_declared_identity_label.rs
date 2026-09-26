//! T-1017: a decoder's identity label is **declared**, and the declaration reaches the store.
//!
//! The list surfaces (`/api/inventory`, `/api/events`) serve a decoded identity's human-readable
//! name only where its decoder said which field that is (`hk_model::IdentityLabelDecl`). This test
//! covers the recipe end of that contract *through the real writer a running pipeline uses*:
//! spawning a `messages` writer records the recipe's declaration in the repository (so the label
//! survives the pipeline that decoded the rows), and a recipe that declares nothing records
//! nothing — no label is ever guessed from a field's name.
//!
//! **Why the declaring recipe here is constructed rather than a product recipe.** No landed recipe
//! decodes a name field yet: AIS types 1/2/3 (`recipes/ais.recipe.json`, T-963) carry MMSI and no
//! vessel name — the name lives in the type 5 static-data message, whose field map is a follow-up;
//! POCSAG has no capcode alias; the `rds` recipe's PS is *content*, and RDS's label comes from the
//! always-on `hk-rds` chain's declaration (`IdentityLabelRegistry::builtin`). So the real AIS
//! recipe is the "declares nothing" case, and the declaring case is the same recipe with the
//! declaration a decoder *with* a name field would carry.

mod common;

use std::sync::Arc;

use common::TempDir;
use hk_model::{ContentClass, LabelConfidence, LabelConfidenceSource, Repository};
use hk_pipeline::recipes::messages::MessagesSink;
use hk_pipeline::recipes::runtime::{PipelineStats, parse_recipe};
use serde_json::Value;

fn ais_doc() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../recipes/ais.recipe.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Spawns the `vessels` writer of `doc` over a fresh database and returns the declarations in force
/// afterwards. Dropping the sink joins its writer thread.
fn declarations_after_spawn(doc: Value) -> hk_model::IdentityLabelRegistry {
    let recipe = parse_recipe(doc).expect("the AIS recipe is valid");
    let dir = TempDir::new("t1017-declared-label");
    let db = dir.0.join("hk.sqlite");
    {
        let sink = MessagesSink::spawn_standalone(
            &db,
            &recipe,
            "vessels",
            ContentClass::Unrestricted,
            16,
            Arc::new(PipelineStats::default()),
            |_, _| {},
        )
        .expect("the vessels writer spawns");
        drop(sink);
    }
    Repository::open(&db)
        .unwrap()
        .identity_label_declarations()
        .unwrap()
}

#[test]
fn a_recipe_declares_its_identity_label_to_the_store_or_has_none() {
    // The real AIS recipe declares no label, so nothing is recorded for it and nothing is guessed
    // from its fields (`mmsi`, `message_type`, …). The built-in RDS declaration stands alone.
    let none = declarations_after_spawn(ais_doc());
    assert_eq!(
        none.get("recipe:ais", "ais-position-report"),
        None,
        "a recipe that declares no label must record none"
    );
    assert!(
        none.get("hk-rds", "rds-pi").is_some(),
        "the built-in chains' declarations are always in force"
    );
    assert_eq!(none.iter().count(), 1, "exactly the built-in declaration");

    // The same recipe as a decoder *with* a name field would write it: one declared label field and
    // one declared confidence, with the meaning stated.
    let mut doc = ais_doc();
    let out = doc["outputs"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|o| o["id"] == "vessels")
        .unwrap();
    let md = out["decode"]["metadata"].as_array_mut().unwrap();
    md.push(Value::String("vessel_name".into()));
    md.push(Value::String("name_votes".into()));
    out["decode"]["identity_label"] = serde_json::json!({
        "field": "vessel_name",
        "confidence": {"field": "name_votes", "from": "vote-counts", "meaning": "vote-share"},
    });
    let declared = declarations_after_spawn(doc);
    let decl = declared
        .get("recipe:ais", "ais-position-report")
        .expect("the declaration reached the store through the writer's spawn");
    assert_eq!(decl.label_field, "vessel_name");
    let c = decl.confidence.as_ref().expect("a declared confidence");
    assert_eq!(
        c.source,
        LabelConfidenceSource::VoteCounts {
            field: "name_votes".into()
        }
    );
    assert_eq!(
        c.meaning,
        LabelConfidence::VoteShare,
        "the meaning is declared, never assumed"
    );
}
