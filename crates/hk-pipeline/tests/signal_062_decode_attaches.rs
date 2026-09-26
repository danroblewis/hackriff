//! T-961 (SIGNAL-062): **a CRC-valid decode lands on the detection it was made from, names it and
//! confirms it** — it never draws a second box beside it.
//!
//! The 106.1 MHz case from the explorer's second FM pass (2026-09-25, live HackRF in SF). Blind
//! detection had the station at 106.1127 MHz, 160 kHz wide, `unknown` at confidence 0.999. The
//! RDS recipe, run on a *band* (so with no emitter to name as context), decoded PI 1323 on a
//! 200 kHz channel 100 Hz away. Two things went wrong, and both are here:
//!
//! 1. the identity sighting had no holder and no context, so entity resolution minted a **second**
//!    entry — two overlapping boxes for one station, which the overlap invariant (CLAUDE.md) says
//!    is proof the analysis is wrong;
//! 2. the decoder's family evidence was written at the **classifier's** arbitration rank, so it
//!    merely tied with the classifier's `unknown` and "latest among equals" gave the family back
//!    to whichever wrote last. A CRC-valid decode is rank 1 (ADR-0016 §2), above every automatic
//!    classifier.
//!
//! This drives the two seams the recipe writer drives — `Ingest::store_decode` for the row and its
//! identity, then the family step and `Inventory::chain_emitter` for the classification, the
//! confirmation review and overlap resolution (`chains::plugin::classify_decoder_emitters`).

mod common;

use common::TempDir;
use hk_model::cluster::Fingerprint;
use hk_model::{
    Classification, ContentClass, CrcStatus, Decode, DecodeId, DecodedIdentity, FreqRange,
    Identity, IdentityScheme, LifecycleState, LinkTarget, Region, Repository, Sighting, TimeRange,
    Timestamp, TrackId,
};
use hk_pipeline::family::decoder_service_evidence;
use hk_pipeline::{Inventory, TrackInventory};
use hk_plugins::Ingest;

const SIGNAL_062: &str = "SIGNAL-062";
/// The station's blind-detection row, as the explorer's journal recorded it.
const DETECTED_HZ: f64 = 106.1127e6;
const DETECTED_BW_HZ: f64 = 160e3;
/// The RDS recipe's channel and its decoded PI.
const CHANNEL_HZ: f64 = 106.1126e6;
const CHANNEL_BW_HZ: f64 = 200e3;
const PI: &str = "1323";

fn t(sec: f64) -> Timestamp {
    Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + (sec * 1e9) as i64)
}

/// The tracker's sighting of the station: a continuous 160 kHz emission over `seen`.
fn detection(seen: TimeRange) -> Sighting {
    let fp = Fingerprint {
        duty_cycle: Some(1.0),
        ..Fingerprint::new(DETECTED_HZ, DETECTED_BW_HZ)
    };
    Sighting {
        source: LinkTarget::Track(TrackId::new()),
        seen,
        count: 6,
        f_center_hz: fp.f_center_hz,
        bandwidth_hz: fp.bandwidth_hz,
        fingerprint: Some(fp),
        identity: None,
        context: None,
        classification: None,
        tags: Vec::new(),
    }
}

/// The classifier's verdict on the station: `unknown`, all but certain, as the UI listed it
/// ("unknown 100% unk").
fn unknown(at: f64) -> Classification {
    Classification {
        t: t(at),
        family: "unknown".into(),
        confidence: 0.999,
        open_set_score: 0.999,
        model_version: "hk-classify/tree@1".into(),
    }
}

/// One CRC-valid RDS group carrying the station's PI, as the recipe's `group-info` output maps it.
fn rds_group(at: f64) -> Decode {
    Decode {
        id: DecodeId::new(),
        demodulation_ref: None,
        recording_ref: None,
        decoder_id: "recipe:rds".into(),
        decoder_version: "1".into(),
        frame_model: "rds-group".into(),
        metadata: serde_json::json!({"group_type": 0, "version": "a"}),
        content: None,
        crc_status: CrcStatus::Valid,
        identity: Some(DecodedIdentity {
            scheme: IdentityScheme::RdsPi,
            value: PI.into(),
        }),
        content_class: ContentClass::Unrestricted,
        t: t(at),
        provenance: None,
    }
}

#[test]
fn signal_062_an_rds_decode_names_and_confirms_the_detection_it_came_from() {
    let dir = TempDir::new("t961-attach");
    let db = dir.0.join("hk.sqlite");
    let mut repo = Repository::open(&db).unwrap();
    let mut inv = TrackInventory::default();

    // Blind detection first, as it always is: a station-shaped emission the classifier cannot name.
    let station = repo
        .record_sighting(&detection(TimeRange::new(t(0.0), t(8.0))), None)
        .unwrap()
        .emitter_id;
    repo.append_classification(station, &unknown(8.0)).unwrap();
    assert_eq!(
        repo.current_classification(station)
            .unwrap()
            .map(|c| c.classification.family),
        Some("unknown".into()),
        "[{SIGNAL_062}] the classifier's verdict stands until something decodes"
    );

    // The recipe, run on a band: it has no emitter to name as context.
    let mut ingest = Ingest::new(Repository::open(&db).unwrap());
    for at in [5.0, 5.1, 5.2] {
        ingest
            .store_decode(
                rds_group(at),
                None,
                None,
                Some(CHANNEL_HZ),
                Some(CHANNEL_BW_HZ),
            )
            .unwrap();
    }
    let new = ingest.take_new_emitters();
    assert_eq!(
        new,
        vec![station],
        "[{SIGNAL_062}] the decode resolved to the detection it was made from, not a new entry"
    );

    // The family step of `chains::plugin::classify_decoder_emitters`, for the `rds` service.
    let evidence = decoder_service_evidence("rds", t(5.2)).expect("rds maps to a service family");
    repo.append_classification(station, &evidence).unwrap();
    inv.chain_emitter(&mut repo, None, station).unwrap();

    // One row on the canvas, carrying the PI.
    let region = Region::new(
        FreqRange::new(106.0e6, 106.2e6),
        TimeRange::new(t(0.0), t(60.0)),
    );
    let rows: Vec<_> = repo
        .emitters_in_region(&region)
        .unwrap()
        .into_iter()
        .map(|e| (e.id, e.identity))
        .collect();
    assert_eq!(
        rows,
        vec![(
            station,
            Identity::Decoded(DecodedIdentity {
                scheme: IdentityScheme::RdsPi,
                value: PI.into()
            })
        )],
        "[{SIGNAL_062}] one box for one station, and it is the one that carries the PI"
    );

    // Named, not `unknown` — and the classifier does not take it back. A CRC-valid decode is
    // ADR-0016 §2 rank 1; `unknown` at rank 3 written afterwards no longer wins on recency.
    let named = |repo: &Repository| {
        repo.current_classification(station)
            .unwrap()
            .map(|c| c.classification.family)
    };
    assert_eq!(named(&repo), Some("fm-broadcast".into()), "[{SIGNAL_062}]");
    repo.append_classification(station, &unknown(9.0)).unwrap();
    assert_eq!(
        named(&repo),
        Some("fm-broadcast".into()),
        "[{SIGNAL_062}] a decoded identity supersedes the classifier, it does not race it"
    );

    // Confirmed, by the decode.
    assert_eq!(
        repo.emitter_lifecycle_state(station).unwrap(),
        LifecycleState::Confirmed,
        "[{SIGNAL_062}] a CRC-valid decoded identity confirms the emitter"
    );
    let reason = repo
        .emitter_lifecycle_history(station)
        .unwrap()
        .pop()
        .expect("a confirmation was recorded")
        .reason;
    assert!(
        reason.starts_with("decoded identity"),
        "[{SIGNAL_062}] confirmed by the decode, not by occupancy: {reason}"
    );
}
