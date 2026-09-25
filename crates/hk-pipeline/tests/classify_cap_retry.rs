//! **T-886: a track refused a classifying chain because the cap was full is classified when a
//! slot frees — it is not first come, first served for the life of the track.**
//!
//! The `classify` node's `max_chains` bounds how many classifying chains run **at once**
//! (T-878). Above it the attach is refused and counted (`classify_admission_refused`). Until
//! T-886 that refusal was final for any track a **decode chain had matched**: `attach_measuring`
//! ran only from `try_attach`, and `try_attach` only for a track still in `pending` — which a
//! track leaves the moment its decode chain attaches. So on a busy band the first few confirmed
//! tracks took the slots and every later decoded track was never classified at all, however long
//! it stayed on air and however idle the classifier became — exactly the wrong bias, since the
//! cap exists to bound concurrent cost, not to pick winners.
//!
//! **The scene, through the mock SDR.** `fsk_burst_train` is a train of FSK bursts that the
//! built-in `fsk-bursts` chain matches, so every confirmed track leaves `pending` as soon as its
//! decode chain attaches. The run below keeps the built-in registry but makes the squeeze
//! explicit: **one** classify slot (`max_chains: 1`) and a 20 ms analysed extent over a
//! five-second scene, so the chain holding the slot finishes while the other tracks are still on
//! air, and `fsk-bursts` attaching on its first detection, so a refused track is one a decode
//! chain has already claimed.
//!
//! What is asserted is **coverage**, not accuracy: how many tracks the classifier was given at
//! all. Whether a 20 ms extent is enough for the cascade to name a family is
//! `device_fsk_classify`'s question, and a row that abstains is a legitimate answer to it.
//!
//! **Blind**: the recording is blinded like every scene test here; nothing reads the scene's
//! truth. What is asserted is only that the cap was actually hit, and that more than the one
//! track holding the slot at that moment ends up with a classification row.

mod common;

use common::*;
use hk_core::Pacing;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_pipeline::{NodeSpec, builtin_chains};
use serde_json::json;

/// The built-in registry with one classify slot, and the fsk chain attaching on first sight.
fn squeezed_registry() -> serde_json::Value {
    let mut chains = builtin_chains();
    let mut found = false;
    for c in &mut chains {
        if c.id == "fsk-bursts" {
            c.min_detections = 1;
        }
        for n in &mut c.nodes {
            if let NodeSpec::Classify {
                max_chains,
                window_s,
                ..
            } = n
            {
                *max_chains = 1;
                // A 20 ms analysed extent, so the chain holding the one slot reaches its window
                // and finishes **inside the scene**: what is under test is the track waiting for
                // that slot, which has to still be on air when it frees. (With the built-in
                // 0.25 s the single chain holds the slot for the whole five seconds, which is
                // the cap doing its job, not the bug.)
                *window_s = 0.02;
                found = true;
            }
        }
    }
    assert!(found, "the built-in registry has a classify node");
    serde_json::to_value(&chains).expect("the registry serialises")
}

/// Classifying chains attached, refusals, confirmed tracks, and the run summary.
fn run_squeezed(seed: u64) -> (u64, u64, u64, String) {
    let out = SynthRequest::new("fsk_burst_train")
        .seed(seed)
        .param("snr_db", 30.0)
        .param("duration_s", 5.0)
        .generate()
        .expect("scene synthesises");
    let meta = out.fixture(0).unwrap().meta_path;
    let dir = TempDir::new("t886-classify-cap");
    let extra = json!({ "pipeline": { "chains": squeezed_registry() } });
    let (cfg, replay, _input) = blind_replay_config(&dir.0, &meta, extra, Pacing::Unpaced);
    let s = start(cfg, replay).wait().unwrap();
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    (
        s.counter("/chains/classify_attached"),
        s.counter("/chains/classify_admission_refused"),
        s.counter("/detect/tracks_confirmed"),
        s.to_text(),
    )
}

#[test]
fn a_track_refused_a_classifier_at_the_cap_is_classified_when_a_slot_frees() {
    let _probe = synth_or_skip!(SynthRequest::new("fsk_burst_train").seed(886));
    let (attached, refused, confirmed, summary) = run_squeezed(886);
    eprintln!(
        "[T-886] one classify slot: {attached} chain(s) attached, {refused} refused, \
         {confirmed} confirmed track(s)"
    );
    assert!(
        refused > 0,
        "[T-886] the one-slot cap was never hit, so this proves nothing:\n{summary}"
    );
    // T-878's guarantee, kept under a full cap: **every confirmed track** is classified, one at
    // a time. A refusal costs a track its place in the queue, never its classification.
    assert_eq!(
        attached, confirmed,
        "[T-886] {attached} classifying chain(s) for {confirmed} confirmed track(s) after \
         {refused} refusal(s): a refused track never got a second look once its decode chain had \
         claimed it.\n{summary}"
    );
}
