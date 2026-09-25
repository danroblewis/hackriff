//! T-878: **through the mock SDR, the C15 classifier writes a row for every scene, whether or not a
//! decode chain matched, attached or succeeded.**
//!
//! Until T-878 the classifier's only call site sat inside the fsk chain, after it had written
//! framed bursts. T-852 probed five scenes through the device and each got **no classification row
//! at all**, for one of four reasons that had nothing to do with the signal:
//!
//! | scene | why it got no row before T-878 |
//! |---|---|
//! | `pocsag_pagers` | no decode chain matched its tracks (`unmatched`) |
//! | `acars_message` | no decode chain matched its track |
//! | `retune_diversity` | only the FM-broadcast chain attached, and it never classifies |
//! | `lora_ism_burst` | the fsk chain attached but got < 4 bursts, and stopped before classifying |
//! | `multipath_echo` | classified three times, but `should_record` dropped every row: the fsk chain's unlocked label tied with the classifier at rank 3 |
//!
//! Now the classifier runs on its own measuring chain for every confirmed track
//! (`hk_pipeline::chains::classify`, ADR-0016 §4 "Placement"), and the rank-3 tie records the row
//! and restates the chain's label (`hk_pipeline::classify::record`).
//!
//! **Blind.** Each run gets the blinded recording (`blind_replay_config`): the scene's truth is
//! sealed away and nothing here reads it. What is asserted is only that the classifier *spoke*
//! about the emissions the device served — a row with a posterior, naming a family or `unknown` —
//! and that speaking never took an emitter's family off a demodulator chain that labelled it. How
//! *accurate* the calls are is ADR-0016 §7's gate, measured elsewhere; a row that says `unknown`
//! is a legitimate answer here.

mod common;

use common::*;
use hk_core::Pacing;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::InventoryQuery;
use hk_model::classify::{ArbRank, Stage, TaxonomyRef, family_of};
use serde_json::json;

/// What one blind run left in the inventory.
struct Run {
    /// `(emitter, family, top-3 posterior)` of every feature-tree row.
    rows: Vec<(String, String, String)>,
    /// Emitters whose family a rank-3 modulation label from a demodulator chain should hold, and
    /// what the current classification actually is.
    chain_owned: Vec<(String, String, Stage)>,
    summary: String,
}

fn run(scene: SynthRequest) -> Run {
    let out = scene.generate().expect("scene synthesises");
    let meta = out.fixture(0).unwrap().meta_path;
    let dir = TempDir::new("t878-classify");
    let (cfg, replay, _input) = blind_replay_config(&dir.0, &meta, json!({}), Pacing::Unpaced);
    let s = start(cfg, replay).wait().unwrap();
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    let repo = repo(&dir.0);
    let mut rows = Vec::new();
    let mut chain_owned = Vec::new();
    for e in inventory(&repo, InventoryQuery::default()) {
        let id = e.emitter.id;
        let history = repo.classification_history(id).unwrap();
        for r in &history {
            if r.stage != Stage::FeatureTree {
                continue;
            }
            let d = r
                .detail
                .as_ref()
                .expect("a feature-tree row carries its detail");
            assert!(
                d.posterior.iter().any(|l| l.label == d.family),
                "{id}: the row's family {} is not in its own posterior {:?}",
                d.family,
                d.posterior
            );
            rows.push((id.to_string(), d.family.clone(), format!("{:?}", d.top(3))));
        }
        // An unlocked chain label at rank 3 that names a modulation: the classifier must have left
        // the family with it (the T-878 tie), unless something better-ranked took it since.
        let chain_label = history.iter().any(|r| {
            r.stage == Stage::Chain
                && r.arb_rank == ArbRank::Classifier
                && family_of(&r.classification.family, &TaxonomyRef::current()).is_some()
        });
        if chain_label {
            let current = repo.current_classification(id).unwrap().unwrap();
            chain_owned.push((
                id.to_string(),
                current.classification.family.clone(),
                current.stage,
            ));
        }
    }
    Run {
        rows,
        chain_owned,
        summary: s.to_text(),
    }
}

fn assert_classified(name: &str, scene: SynthRequest) -> Run {
    let r = run(scene);
    for (id, family, top) in &r.rows {
        eprintln!("[T-878] {name}: {id} {family} {top}");
    }
    for (id, family, stage) in &r.chain_owned {
        eprintln!("[T-878] {name}: {id} labelled by a chain, now {family} ({stage:?})");
    }
    for l in r
        .summary
        .lines()
        .filter(|l| l.starts_with("classify:") || l.starts_with("chains:") || l.contains("elapsed"))
    {
        eprintln!("[T-878] {name}: {l}");
    }
    assert!(
        !r.rows.is_empty(),
        "[T-878] {name}: the classifier wrote no row for any emission the device served.\n{}",
        r.summary
    );
    for (id, family, stage) in &r.chain_owned {
        assert_ne!(
            *stage,
            Stage::FeatureTree,
            "[T-878] {name}: the classifier took {id}'s family off its demodulator chain \
             (now {family})"
        );
    }
    r
}

/// No decode chain matches a POCSAG channel's tracks.
#[test]
fn pocsag_tracks_no_decoder_matched_are_classified() {
    let _probe = synth_or_skip!(SynthRequest::new("pocsag_pagers").seed(878));
    assert_classified(
        "pocsag_pagers",
        SynthRequest::new("pocsag_pagers").seed(878),
    );
}

/// No decode chain matches an ACARS burst's track.
#[test]
fn acars_tracks_no_decoder_matched_are_classified() {
    let _probe = synth_or_skip!(SynthRequest::new("acars_message").seed(878));
    assert_classified(
        "acars_message",
        SynthRequest::new("acars_message").seed(878),
    );
}

/// Only the FM-broadcast chain attaches in the retune scene's first capture, and it never
/// classifies.
#[test]
fn retune_scene_tracks_only_a_non_classifying_chain_took_are_classified() {
    let _probe = synth_or_skip!(SynthRequest::new("retune_diversity").seed(878));
    assert_classified(
        "retune_diversity",
        SynthRequest::new("retune_diversity").seed(878),
    );
}

/// The fsk chain attaches to a LoRa burst but demodulates too few FSK bursts to go on.
#[test]
fn lora_burst_the_fsk_chain_gave_up_on_is_classified() {
    let _probe = synth_or_skip!(SynthRequest::new("lora_ism_burst").seed(878));
    assert_classified(
        "lora_ism_burst",
        SynthRequest::new("lora_ism_burst").seed(878),
    );
}

/// The fsk chain labels the multipath scene's emitters without a CRC lock, so its label ties the
/// classifier at rank 3: the row is recorded and the chain keeps the family.
#[test]
fn multipath_rows_an_unlocked_chain_label_ties_with_are_recorded() {
    let _probe = synth_or_skip!(SynthRequest::new("multipath_echo").seed(878));
    // Two seconds of the eight-second default: still dozens of bursts per channel, so the fsk
    // chain attaches and labels without a lock, at a quarter of the run time.
    let r = assert_classified(
        "multipath_echo",
        SynthRequest::new("multipath_echo")
            .seed(878)
            .param("duration_s", 2.0),
    );
    // The tie must actually have been exercised, or the family check above passes vacuously.
    assert!(
        !r.chain_owned.is_empty(),
        "[T-878] multipath_echo: no emitter carried an unlocked chain label, so the rank-3 tie was \
         never tested\n{}",
        r.summary
    );
}
