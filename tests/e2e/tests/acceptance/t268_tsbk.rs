//! T-268 (C23): TSBK decode, IDEN_UP channel mapping, and the stale-table refusal — blind,
//! through the mock SDR, during a **normal** pipeline run.
//!
//! T-267 confirmed *that* a control channel is there and stopped: its row said
//! `TrunkProtocol::Unknown`, with an empty channel table. T-287 gave that hunt a caller. This test
//! asks the next question — what did the control channel *say*? — and it asks it the way T-287
//! does: no `CcConfirmer`, no raster, no decoder types anywhere in the test. It starts a run with
//! the **built-in** chain registry, nothing configured, and then reads the repository.
//!
//! # The claim, and the trap
//!
//! A P25 grant carries **no frequency**. It carries a 16-bit channel number — 4 bits of identifier,
//! 12 bits of channel — and only the band plan the control channel announced in its IDEN_UP
//! messages turns that into hertz. So the scene contains two grants:
//!
//! 1. one naming the identifier the control channel **does** announce, which must resolve to the
//!    voice frequency the fixture chose *before* it worked out a channel number for it;
//! 2. one naming an identifier the control channel **never** announces — and here is the trap:
//!    resolving it through the announced identifier would produce a completely plausible 800 MHz
//!    frequency. C23's pitfall is "stale IDEN tables mapping to wrong frequencies", and a wrong
//!    frequency that looks right is worse than no frequency at all. That grant must come back as
//!    `unmapped-channel`, and **the plausible wrong frequency must appear nowhere**.
//!
//! The age half of the same refusal (an identifier decoded, then trusted for too long) cannot be
//! staged in a two-second recording: it turns on a ten-minute threshold. It is proved instead
//! where it lives, over the real types and real timestamps, in `hk_detect::trunk::tsbk` and in
//! `hk_pipeline::chains::trunk` — both refusals funnel through one `ChannelMap::resolve`, and both
//! produce the same `unmapped-channel` row. This test proves the half a fixture can honestly hold.
//!
//! Blind: the only frequency anything is given is where the device says it is tuned. Truth is
//! opened at the end, and only to check the answer.

use hk_core::Pacing;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::{Encryption, GrantKind, Timestamp, TrunkProtocol};
use serde_json::json;

use crate::blind::{replay_config, start};
use crate::common::*;

const T268: &str = "T-268";

/// The mapping is integer arithmetic on decoded fields (`base + spacing × channel`), so the only
/// tolerance a correct decode needs is float representation. A hertz is already generous; anything
/// bigger would be hiding a wrong answer rather than allowing for measurement.
const MAP_TOLERANCE_HZ: f64 = 1.0;

#[test]
fn t268_a_normal_run_decodes_tsbks_maps_a_grant_and_refuses_an_unannounced_identifier() {
    let out = synth_or_skip!(
        SynthRequest::new("trunk_tsbk_control_channel")
            .seed(268)
            .param("duration_s", 2.0)
    );
    let fx = out.fixture(0).unwrap();

    let dir = TempDir::new("t268");
    let (cfg, dev) = replay_config(&dir.0, &fx.meta_path, json!({}), Pacing::Unpaced);
    let summary = finish(start(cfg, dev));

    let count = |p: &str| summary.counter(p);
    let (tsbks, iden_ups) = (count("/chains/cc_tsbks"), count("/chains/cc_iden_ups"));
    let admitted = count("/chains/cc_iden_admitted");
    let (mapped, unmapped) = (
        count("/chains/cc_grants_mapped"),
        count("/chains/cc_grants_unmapped"),
    );
    eprintln!(
        "[{T268}] decode during the run: {} confirmed CC, {tsbks} TSBK(s), {iden_ups} IDEN_UP \
         ({admitted} admitted to the plan), {mapped} grant(s) mapped, {unmapped} unmapped",
        count("/chains/cc_confirmed")
    );

    // ---- M4 is metadata-only: the run decoded a control channel and recorded nothing.
    assert_eq!(
        count("/chains/recordings"),
        0,
        "[{T268}] M4 is metadata-only: nothing may be recorded"
    );

    // ---- Truth, opened only now, and only to check the answer.
    let scenario = fx.scenario().unwrap();
    let truth = &scenario.value["trunking"];
    let cc_truth_hz = truth["control_channel"]["rf_center_hz"].as_f64().unwrap();
    let raster_hz = truth["raster_hz"].as_f64().unwrap();
    let t = &truth["tsbk"];
    let want_iden = t["iden"].as_u64().unwrap();
    let want_base = t["base_hz"].as_f64().unwrap();
    let want_spacing = t["spacing_hz"].as_f64().unwrap();
    let want_target = t["grant_target_hz"].as_f64().unwrap();
    let want_talkgroup = t["grant_talkgroup"].as_u64().unwrap().to_string();
    let unannounced_16 = t["unannounced_channel_16bit"].as_u64().unwrap().to_string();
    let trap_hz = t["wrong_frequency_if_misresolved_hz"].as_f64().unwrap();

    let repo = repo(&dir.0);
    let rows = repo.trunk_systems().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "[{T268}] exactly one control channel should be on file, got {:?}",
        rows.iter().map(|s| s.cc_freq_hz).collect::<Vec<_>>()
    );
    let system = &rows[0];
    let found_hz = system.cc_freq_hz.expect("a confirmed CC has a frequency");
    assert!(
        (found_hz - cc_truth_hz).abs() <= raster_hz / 2.0,
        "[{T268}] the row says {:.4} MHz but the control channel is at {:.4} MHz",
        found_hz / 1e6,
        cc_truth_hz / 1e6
    );

    // ---- Naming. T-287's row said `Unknown` because nothing had read a message; this one has.
    assert_eq!(
        system.protocol,
        TrunkProtocol::P25Phase1,
        "[{T268}] TSBKs decoded, so the protocol is named rather than left unknown"
    );

    // ---- IDEN_UP built the band plan, and each entry carries when it was decoded — the fact that
    // makes a stale table detectable at all.
    let plan = repo.channel_plan(system.id).unwrap();
    eprintln!(
        "[{T268}] channel plan: {:?}",
        plan.iter()
            .map(|e| (e.iden, e.base_hz, e.spacing_hz))
            .collect::<Vec<_>>()
    );
    assert!(
        admitted >= 1 && !plan.is_empty(),
        "[{T268}] no identifier was admitted to the channel table"
    );
    let entry = plan
        .iter()
        .find(|e| u64::from(e.iden) == want_iden)
        .unwrap_or_else(|| panic!("[{T268}] identifier {want_iden} is not in the plan: {plan:?}"));
    assert!(
        (entry.base_hz - want_base).abs() <= MAP_TOLERANCE_HZ,
        "[{T268}] base {} Hz, expected {want_base} Hz",
        entry.base_hz
    );
    assert!(
        (entry.spacing_hz - want_spacing).abs() <= MAP_TOLERANCE_HZ,
        "[{T268}] spacing {} Hz, expected {want_spacing} Hz",
        entry.spacing_hz
    );
    assert!(
        entry.t > Timestamp::UNIX_EPOCH,
        "[{T268}] the entry carries no decode time, so staleness could never be detected"
    );

    // ---- The grants the control channel issued.
    let grants = repo
        .grants_for_system(system.id, Timestamp::UNIX_EPOCH, 10_000)
        .unwrap();
    assert!(!grants.is_empty(), "[{T268}] no grant events were recorded");

    // Every event states nothing about encryption: T-270 owns the service-options bit, and
    // "nothing said" is never "clear" (T-266).
    assert!(
        grants.iter().all(|g| g.encryption == Encryption::Unknown),
        "[{T268}] a grant claimed an encryption state nothing had read"
    );

    // 1. The announced identifier resolves the grant to the frequency the fixture chose first.
    let resolved: Vec<&hk_model::GrantEvent> = grants
        .iter()
        .filter(|g| matches!(g.kind, GrantKind::Grant | GrantKind::GrantUpdate))
        .collect();
    assert!(
        !resolved.is_empty(),
        "[{T268}] no grant resolved to a frequency at all"
    );
    let on_target = resolved
        .iter()
        .filter(|g| {
            g.f_hz
                .is_some_and(|f| (f - want_target).abs() <= MAP_TOLERANCE_HZ)
        })
        .count();
    assert!(
        on_target >= 1,
        "[{T268}] no grant mapped to {:.6} MHz; got {:?}",
        want_target / 1e6,
        resolved.iter().map(|g| g.f_hz).collect::<Vec<_>>()
    );
    assert!(
        resolved
            .iter()
            .any(|g| g.talkgroup.as_deref() == Some(want_talkgroup.as_str())),
        "[{T268}] the grant's talkgroup was not decoded"
    );

    // 2. The identifier that was never announced is REPORTED, not resolved.
    let unmapped_rows: Vec<&hk_model::GrantEvent> = grants
        .iter()
        .filter(|g| g.kind == GrantKind::UnmappedChannel)
        .collect();
    assert!(
        unmapped >= 1 && !unmapped_rows.is_empty(),
        "[{T268}] the grant naming an unannounced identifier produced no unmapped-channel event"
    );
    let trap_row = unmapped_rows
        .iter()
        .find(|g| g.channel.as_deref() == Some(unannounced_16.as_str()))
        .unwrap_or_else(|| {
            panic!(
                "[{T268}] no unmapped-channel event for channel {unannounced_16}: {:?}",
                unmapped_rows.iter().map(|g| &g.channel).collect::<Vec<_>>()
            )
        });
    assert!(
        trap_row.f_hz.is_none(),
        "[{T268}] an unmapped channel carried a frequency: {:?}",
        trap_row.f_hz
    );
    assert_eq!(
        trap_row.detail["reason"].as_str(),
        Some("no-iden"),
        "[{T268}] the refusal does not say why: {}",
        trap_row.detail
    );

    // 3. THE ASSERTION THIS TEST EXISTS FOR. The plausible wrong frequency appears nowhere.
    let leaked: Vec<f64> = grants
        .iter()
        .filter_map(|g| g.f_hz)
        .filter(|f| (f - trap_hz).abs() <= MAP_TOLERANCE_HZ)
        .collect();
    assert!(
        leaked.is_empty(),
        "[{T268}] a grant naming an identifier that was never announced was resolved through \
         another identifier's band plan to {:.6} MHz — a plausible frequency, and the wrong one \
         (C23: stale IDEN tables mapping to wrong frequencies)",
        trap_hz / 1e6
    );

    eprintln!(
        "[{T268}] identifier {want_iden}: base {:.5} MHz, spacing {:.0} Hz -> grant on \
         {:.6} MHz (truth {:.6} MHz); identifier {} never announced -> unmapped-channel, and \
         {:.6} MHz never reported",
        entry.base_hz / 1e6,
        entry.spacing_hz,
        resolved[0].f_hz.unwrap_or_default() / 1e6,
        want_target / 1e6,
        t["unannounced_iden"],
        trap_hz / 1e6
    );
}
