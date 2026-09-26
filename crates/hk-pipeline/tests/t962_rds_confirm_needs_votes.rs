//! T-962 (SIGNAL-062): the confirm gate's route A cannot fire on a provisional RDS PI.
//!
//! The companion to `hk-demod`'s `t962_rds_identity_needs_votes`, one layer up: that test asserts
//! what is *written* for a three-group PI, this one asserts what the **confirm gate** then sees
//! and decides. `ConfirmPolicy::decide_route` reads `Repository::identity_decode_evidence`, which
//! keys off the Emitter's own identity columns — so an identity that was never claimed is an
//! identity the gate cannot confirm on, and route A stays shut with everything else about the two
//! runs identical.
//!
//! **Which bound.** N agreeing CRC-valid groups (`GroupConfig::pi_commit_votes` = 10), not
//! ADR-0022 §6's `analytic_holdout_bits`: that budget governs `ConfirmPolicy.synthesized`, the
//! route for a synthesized pipeline with a searched check stage whose look-elsewhere the engine
//! prices (ADR-0022 §5.1–§5.2). RDS is a shipped, template-fixed decoder on route A and computes
//! no such quantity. ADR-0022 §1.3 is why a bound is needed at all: a confirm is a lifecycle
//! change no rule demotes, so the confirm gate binds, always — and may only ever be made
//! stricter.

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_demod::rds::{GroupConfig, Offset, RDS_BITRATE_BD, RdsDecoder, RdsReport, encode_block};
use hk_demod::{AnalogMode, AnalogReceiver, AnalogSession, RecordContext, write_session};
use hk_dsp::InputInfo;
use hk_dsp::synth::Rng;
use hk_estimate::SnippetRequest;
use hk_model::{
    EmitterId, Fingerprint, LifecycleState, LinkTarget, MeasurementKey, Provenance, Repository,
    SampleTime, Sighting, TimeRange, Timestamp,
};
use hk_pipeline::inventory::{ConfirmEvidence, ConfirmPolicy, ConfirmRoute};
use num_complex::Complex32;

const T962: &str = "T-962";
const FS: f64 = 500e3;
const CENTER: f64 = 100e6;

fn provenance() -> ProvenanceHandle {
    let p: Provenance = serde_json::from_value(serde_json::json!({
        "device_id": "synthetic:t962",
        "tune": {
            "center_hz": CENTER, "sample_rate_hz": FS, "lna_db": 16.0, "vga_db": 20.0,
            "amp_on": false, "bandwidth_hz": FS * 0.75,
        },
        "overload": false, "quantisation_limited": false, "clock_source": "internal",
        "clock_locked": true, "timestamp_method": "synthetic", "timestamp_error_budget_ns": 0,
    }))
    .unwrap();
    ProvenanceHandle::new(p)
}

/// A broadcast-width FM scene: one 4 kHz programme tone at 75 kHz peak deviation.
fn fm_scene() -> Vec<Complex32> {
    let n = FS as usize;
    let mut rng = Rng::new(962);
    let mut phase = 0.0f64;
    (0..n)
        .map(|i| {
            let t = i as f64 / FS;
            let inst = 40e3 + 75e3 * (std::f64::consts::TAU * 4e3 * t).sin();
            phase += std::f64::consts::TAU * inst / FS;
            let noise = 0.002 * (rng.next_u64() as f64 / u64::MAX as f64 - 0.5);
            Complex32::new(
                (0.1 * phase.cos() + noise) as f32,
                (0.1 * phase.sin() + noise) as f32,
            )
        })
        .collect()
}

fn group_0a_bits(pi: u16, ps: &[u8; 8], seg: usize) -> Vec<u8> {
    let b2 = (1u16 << 10) | (10u16 << 5) | seg as u16;
    let b4 = (u16::from(ps[2 * seg]) << 8) | u16::from(ps[2 * seg + 1]);
    [
        encode_block(pi, Offset::A),
        encode_block(b2, Offset::B),
        encode_block(0xE0CD, Offset::C),
        encode_block(b4, Offset::D),
    ]
    .iter()
    .flat_map(|b| (0..26).map(move |i| ((b >> (25 - i)) & 1) as u8))
    .collect()
}

fn report_over(groups: usize) -> RdsReport {
    let mut dec = RdsDecoder::new(GroupConfig::default(), RDS_BITRATE_BD);
    let mut bits = Vec::new();
    for g in 0..groups {
        bits.extend(group_0a_bits(0xC0DE, b"HACKRIFF", g % 4));
    }
    for (i, &b) in bits.iter().enumerate() {
        dec.push_bit(b, i as f64);
    }
    dec.report()
}

fn session_with(rds: RdsReport) -> AnalogSession {
    let iq = fm_scene();
    let prov = provenance();
    let info = InputInfo {
        time: SampleTime {
            sample_index: 0,
            host_time: Timestamp::from_unix_nanos(1_789_000_000_000_000_000),
        },
        discontinuity: Discontinuity::STREAM_START,
        dropped_before: 0,
        provenance: &prov,
    };
    let req = SnippetRequest {
        start_index: 0,
        end_index: iq.len() as u64,
        center_offset_hz: 40e3,
        bandwidth_hz: 200e3,
    };
    let mut s = AnalogReceiver::default().run(info, &iq, &req).unwrap();
    assert_eq!(s.mode.mode, AnalogMode::Wfm, "[{T962}] {:?}", s.mode);
    s.wfm.as_mut().expect("WFM chain").rds = Some(rds);
    s
}

/// An inventory entry placed on occupancy alone, with no identity: the candidate a chain hands to
/// the RDS write as its `emitter_hint`.
fn seed_candidate(repo: &mut Repository, t: TimeRange) -> EmitterId {
    let sighting = Sighting {
        source: LinkTarget::Detection(hk_model::DetectionId::new()),
        seen: t,
        count: 1,
        f_center_hz: CENTER + 40e3,
        bandwidth_hz: 180e3,
        fingerprint: Some(Fingerprint::new(CENTER + 40e3, 180e3)),
        identity: None,
        context: None,
        classification: None,
        tags: Vec::new(),
    };
    repo.record_sighting_measured(&sighting, &MeasurementKey::new("t962-track"), None)
        .unwrap()
        .emitter_id
}

/// Identity evidence for route A: the scheme and the count of CRC-valid decodes carrying it.
type IdentityEvidence = Option<(hk_model::IdentityScheme, u64)>;

/// The gate's own composition: the evidence `ConfirmInventory::review` builds for route A, with
/// the other two routes deliberately empty so the identity is the only thing under test.
fn identity_only_decision(
    repo: &Repository,
    id: EmitterId,
) -> (IdentityEvidence, Option<(ConfirmRoute, String)>) {
    let identity = repo.identity_decode_evidence(id).unwrap();
    let ev = ConfirmEvidence {
        identity: identity.clone(),
        track: None,
        verified: None,
    };
    (identity, ConfirmPolicy::default().decide_route(&ev))
}

fn run(groups: usize) -> (Repository, EmitterId, usize) {
    let rds = report_over(groups);
    let votes = rds.pi.expect("[T-962] PI reported").votes as usize;
    let s = session_with(rds);
    let mut repo = Repository::open_in_memory().unwrap();
    let hint = seed_candidate(&mut repo, s.time_range());
    let ctx = RecordContext {
        emitter_hint: Some(hint),
        ..RecordContext::default()
    };
    let w = write_session(&mut repo, &s, &ctx).unwrap();
    let id = w.emitter_id.unwrap_or(hint);
    (repo, id, votes)
}

#[test]
fn t962_three_groups_do_not_confirm_and_a_real_station_does() {
    // The defect, reproduced as evidence: 98.085 MHz committed PI 1704 on about three CRC-valid
    // groups in 45 s while an independent oracle found no RDS on the clip at all.
    let (weak_repo, weak, weak_votes) = run(5);
    assert_eq!(weak_votes, 3, "[{T962}] the scene is three agreeing groups");
    let (weak_evidence, weak_decision) = identity_only_decision(&weak_repo, weak);
    assert_eq!(
        weak_evidence, None,
        "[{T962}] three agreeing CRC-valid groups became identity evidence for the confirm gate. \
         Route A is a one-way door (ADR-0022 §1.3): below pi_commit_votes the PI is a reading, \
         and a reading is not an identity."
    );
    assert_eq!(
        weak_decision, None,
        "[{T962}] the gate confirmed on a provisional PI: {weak_decision:?}"
    );
    assert_eq!(
        weak_repo.emitter_lifecycle_state(weak).unwrap(),
        LifecycleState::Candidate,
        "[{T962}] and the entry stays a Candidate"
    );

    // A second of the same station, which is what the bound actually costs.
    let (strong_repo, strong, strong_votes) = run(12);
    assert!(
        strong_votes >= GroupConfig::default().pi_commit_votes as usize,
        "[{T962}] {strong_votes} votes"
    );
    let (strong_evidence, strong_decision) = identity_only_decision(&strong_repo, strong);
    let (scheme, n) = strong_evidence.expect("[T-962] a committed PI is identity evidence");
    assert_eq!(scheme.as_string(), "rds-pi", "[{T962}]");
    assert!(n >= 1, "[{T962}] {n} CRC-valid decodes carry it");
    let (route, reason) = strong_decision.expect("[T-962] and it confirms, exactly as before");
    assert_eq!(route, ConfirmRoute::Identity, "[{T962}] {reason}");
    eprintln!("[{T962}] committed: {reason}");
}
