//! T-287 (C23): the control-channel hunt during a **normal** pipeline run, through the mock SDR.
//!
//! T-267 proved the capability by driving the device interface directly from a test. That left the
//! hunt with no caller inside the pipeline: a normal run wrote no control-channel row, and the
//! capability's tests passed either way — the fourth capability-with-no-caller found in one night
//! (T-242 signature matching, T-247 the C15 classifier, T-206's own gate dimensions, T-267 here).
//! **A capability that nothing calls is indistinguishable from an absent one.**
//!
//! So this test closes the vacuum from the other side, and deliberately does the opposite of
//! [`super::t267_trunk_cc`]: it never mentions `CcConfirmer`, `CcCandidate` or a raster. It starts
//! a run the way `hk serve` does, on a truth-stripped recording served by the mock SDR, with the
//! **built-in** chain registry (no plan override, nothing configured by the test), and then asks
//! the repository whether a control channel was confirmed. If the wiring were removed, this fails;
//! if only the confirmer were removed, T-267 fails too. Neither can pass vacuously.
//!
//! Blind: the only frequency anything is given is where the device says it is tuned. Truth is
//! opened at the end, and only to check the answer.

use hk_core::Pacing;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::{ContentClass, TrunkProtocol};
use serde_json::json;

use crate::blind::{replay_config, start};
use crate::common::*;

const T287: &str = "T-287";

#[test]
fn t287_a_normal_run_writes_a_confirmed_control_channel_row() {
    let out = synth_or_skip!(
        SynthRequest::new("trunk_control_channel")
            .seed(287)
            .param("duration_s", 2.0)
    );
    let fx = out.fixture(0).unwrap();

    // Content gating ON for this run, so the class gate is genuinely closed rather than dormant.
    // 851 MHz LMR lands in no positive content prior, so `class::band_class` falls through to the
    // fail-closed `metadata-only`: the class that refuses every chain with `requires_content` and
    // every recording. The hunt must still run under it, because what it writes is metadata —
    // that a control channel exists at a frequency — and metadata is never gated.
    let gating = hk_model::content_gating_enabled();
    hk_model::set_content_gating(true);

    let dir = TempDir::new("t287");
    let (cfg, dev) = replay_config(&dir.0, &fx.meta_path, json!({}), Pacing::Unpaced);
    assert_eq!(
        cfg.source_class,
        ContentClass::MetadataOnly,
        "[{T287}] the class gate must actually be closed at 851 MHz, or this proves nothing"
    );
    assert!(
        !cfg.source_class.permits_content(),
        "[{T287}] gating is on and this class forbids content"
    );
    let summary = finish(start(cfg, dev));
    hk_model::set_content_gating(gating);

    let count = |p: &str| summary.counter(p);
    let (passes, channels) = (count("/chains/cc_passes"), count("/chains/cc_channels"));
    let (candidates, demods) = (count("/chains/cc_candidates"), count("/chains/cc_demods"));
    let (confirmed, systems) = (count("/chains/cc_confirmed"), count("/chains/cc_systems"));
    eprintln!(
        "[{T287}] hunt during the run: {passes} pass(es), {channels} raster channels measured, \
         {candidates} candidates, {demods} demodulated ({} refused by admission), \
         {confirmed} confirmed, {systems} row(s)",
        count("/chains/cc_admission_refused")
    );

    // ---- The wiring ran at all. This is the assertion T-267 could not make.
    assert!(
        passes >= 1,
        "[{T287}] the control-channel hunt never ran during a normal run: the chain was never \
         attached, so the capability has no caller"
    );
    assert!(channels > 0, "[{T287}] the raster was never swept");
    // ---- C23's pitfall is live in this run: the decoy reaches candidacy exactly as the control
    // channel does, so confirmation is doing the work, not occupancy.
    assert!(
        candidates >= 2,
        "[{T287}] the control channel and the continuous decoy must BOTH reach candidacy, or the \
         run is not testing the pitfall: {candidates}"
    );
    // ---- Admission control bounded the expensive half.
    assert!(
        demods <= 8,
        "[{T287}] the per-pass admission cap (max_demods 8) did not bound demodulations: {demods}"
    );
    assert!(
        demods >= candidates.min(8),
        "[{T287}] candidates went unexamined"
    );
    // ---- Metadata only: under this class the run recorded no content at all, and still found
    // the control channel.
    assert_eq!(
        count("/chains/recordings"),
        0,
        "[{T287}] M4 is metadata-only and the class forbids content: nothing may be recorded"
    );

    // ---- Truth, opened only now, and only to check the answer.
    let scenario = fx.scenario().unwrap();
    let truth = &scenario.value["trunking"];
    let cc_truth_hz = truth["control_channel"]["rf_center_hz"].as_f64().unwrap();
    let decoy_truth_hz = truth["continuous_decoy"]["rf_center_hz"].as_f64().unwrap();
    let raster_hz = truth["raster_hz"].as_f64().unwrap();

    let repo = repo(&dir.0);
    let rows = repo.trunk_systems().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "[{T287}] exactly one control channel should be on file, got {:?}",
        rows.iter().map(|s| s.cc_freq_hz).collect::<Vec<_>>()
    );
    let system = &rows[0];
    let found_hz = system
        .cc_freq_hz
        .expect("a confirmed control channel always has a frequency");
    assert!(
        (found_hz - cc_truth_hz).abs() <= raster_hz / 2.0,
        "[{T287}] the row says {:.4} MHz but the control channel is at {:.4} MHz",
        found_hz / 1e6,
        cc_truth_hz / 1e6
    );
    assert!(
        (found_hz - decoy_truth_hz).abs() > raster_hz / 2.0,
        "[{T287}] the run confirmed the continuous decoy at {:.4} MHz",
        decoy_truth_hz / 1e6
    );
    assert_eq!(confirmed, 1, "[{T287}] exactly one confirmation");
    assert_eq!(systems, 1, "[{T287}] exactly one row written");

    // ---- Scope: confirming a control channel is not naming it. T-268 decodes the identifiers,
    // and a row that has not read them says so rather than guessing.
    assert_eq!(
        system.protocol,
        TrunkProtocol::Unknown,
        "[{T287}] confirming a CC is not naming its protocol"
    );
    assert!(
        system.system_id.is_none() && system.site_id.is_none(),
        "[{T287}] nothing was decoded, so nothing is claimed: {system:?}"
    );
    assert!(
        system.talkgroups.is_empty() && system.channel_plan.is_empty(),
        "[{T287}] the hunt reads no control-channel messages"
    );
    eprintln!(
        "[{T287}] confirmed {:.4} MHz through a normal run (truth {:.4} MHz, decoy {:.4} MHz \
         rejected), protocol {:?}",
        found_hz / 1e6,
        cc_truth_hz / 1e6,
        decoy_truth_hz / 1e6,
        system.protocol
    );
}
