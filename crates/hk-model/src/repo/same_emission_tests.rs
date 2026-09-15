//! T-082: one inventory entry per physical emitter. The same-emission rule links a decoder entry
//! to the track entry of the same emission; merges keep evidence and lifecycle; distinct emitters
//! stay apart; deleted entries are never resurrected.

use std::collections::BTreeMap;

use super::Repository;
use crate::cluster::*;
use crate::*;

fn t(sec: f64) -> Timestamp {
    Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + (sec * 1e9) as i64)
}

fn tr(a: f64, b: f64) -> TimeRange {
    TimeRange::new(t(a), t(b))
}

fn repo() -> Repository {
    Repository::open_in_memory().unwrap()
}

fn tol() -> Tolerances {
    Tolerances::default()
}

fn track(f: f64, bw: f64, seen: TimeRange, count: u64) -> Sighting {
    track_fp(Fingerprint::new(f, bw), seen, count)
}

fn track_fp(fp: Fingerprint, seen: TimeRange, count: u64) -> Sighting {
    Sighting {
        source: LinkTarget::Track(TrackId::new()),
        seen,
        count,
        f_center_hz: fp.f_center_hz,
        bandwidth_hz: fp.bandwidth_hz,
        fingerprint: Some(fp),
        identity: None,
        context: None,
        classification: None,
        tags: Vec::new(),
    }
}

/// The RDS writer's sighting (hk-demod `write_session`): a demodulation with a decoded PI.
fn rds(f: f64, seen: TimeRange, pi: &str) -> Sighting {
    Sighting {
        source: LinkTarget::Demodulation(DemodulationId::new()),
        seen,
        count: 1,
        f_center_hz: f,
        bandwidth_hz: 180e3,
        fingerprint: Some(Fingerprint {
            family: Some("wfm".into()),
            ..Fingerprint::new(f, 180e3)
        }),
        identity: Some(IdentityClaim {
            identity: DecodedIdentity {
                scheme: IdentityScheme::RdsPi,
                value: pi.into(),
            },
            content_class: ContentClass::Unrestricted,
        }),
        context: None,
        classification: Some(Classification {
            t: seen.end,
            family: "wfm".into(),
            confidence: 0.9,
            open_set_score: 0.1,
            model_version: "hk-demod/mode@1".into(),
        }),
        tags: Vec::new(),
    }
}

/// The blind framer's sighting (hk-demod `write_framed_bursts`): structural identity.
fn framed(f: f64, bw: f64, seen: TimeRange, count: u64) -> Sighting {
    Sighting {
        source: LinkTarget::Demodulation(DemodulationId::new()),
        seen,
        count,
        f_center_hz: f,
        bandwidth_hz: bw,
        fingerprint: Some(Fingerprint {
            family: Some("fsk".into()),
            symbol_rate_hz: Some(4800.0),
            ..Fingerprint::new(f, bw)
        }),
        identity: Some(IdentityClaim {
            identity: DecodedIdentity {
                scheme: IdentityScheme::Other("hk-framing".into()),
                value: "sync-2dd4-crc16".into(),
            },
            content_class: ContentClass::MetadataOnly,
        }),
        context: None,
        classification: None,
        tags: Vec::new(),
    }
}

fn confirm(r: &mut Repository, id: EmitterId, author: LifecycleAuthor, actor: &str, why: &str) {
    r.change_emitter_lifecycle(id, LifecycleState::Confirmed, author, actor, why, t(5.0))
        .unwrap()
        .expect("confirmed");
}

fn listed(r: &Repository) -> Vec<EmitterId> {
    r.query_inventory(&InventoryQuery::default())
        .unwrap()
        .entries
        .into_iter()
        .map(|e| e.emitter.id)
        .collect()
}

#[test]
fn t082_decoder_entry_of_a_tracked_emission_merges_into_one_entry_with_all_evidence() {
    let mut r = repo();
    let e = r
        .record_sighting(&track(101.303e6, 200e3, tr(0.0, 5.0), 5), None)
        .unwrap()
        .emitter_id;
    confirm(&mut r, e, LifecycleAuthor::Auto, "rule@1", "continuous");
    // The cause: an identity sighting with no holder and no context is created on its own.
    let res = r
        .record_sighting(&rds(101.3022e6, tr(1.0, 4.0), "C0DE"), None)
        .unwrap();
    assert!(res.created);
    let d = res.emitter_id;
    assert_eq!(listed(&r).len(), 2);

    assert!(r.same_emission(e, d, &tol()).unwrap().is_some());
    let partners: Vec<EmitterId> = r
        .same_emission_partners(d, &tol())
        .unwrap()
        .into_iter()
        .map(|p| p.0)
        .collect();
    assert_eq!(partners, vec![e]);

    let m = r
        .merge_same_emission(d, e, t(5.0), "same emission", &tol())
        .unwrap()
        .expect("merged");
    assert_eq!((m.from, m.into), (d, e), "the confirmed, first-seen entry survives");
    assert!(m.identity_moved);
    assert_eq!(listed(&r), vec![e]);
    let em = r.emitter(e).unwrap();
    assert_eq!(em.count, 5, "the decode overlaps the track: its burst is not counted twice");
    assert_eq!(
        em.identity,
        Identity::Decoded(DecodedIdentity {
            scheme: IdentityScheme::RdsPi,
            value: "C0DE".into()
        })
    );
    assert!(
        em.classifications.iter().any(|c| c.family == "wfm"),
        "classification history carried"
    );
    let links = r.emitter_links(e).unwrap();
    assert!(links.iter().any(|l| matches!(l.target, LinkTarget::Track(_))));
    assert!(
        links
            .iter()
            .any(|l| matches!(l.target, LinkTarget::Demodulation(_)))
    );
    assert_eq!(
        r.emitter_lifecycle_state(d).unwrap(),
        LifecycleState::Confirmed,
        "a merged id reads its survivor"
    );
    assert_eq!(r.emitter_lifecycle_history(e).unwrap().len(), 1);
    assert_eq!(r.emitter_recurrence(e, 8).unwrap().appearances, 1);
    assert_eq!(r.emitter_merges(e).unwrap().len(), 1);
    // Idempotent, and a later decode of the identity reaches the one entry.
    assert!(
        r.merge_same_emission(d, e, t(6.0), "again", &tol())
            .unwrap()
            .is_none()
    );
    assert_eq!(
        r.record_sighting(&rds(101.302e6, tr(4.0, 5.0), "C0DE"), None)
            .unwrap()
            .emitter_id,
        e
    );
}

#[test]
fn t082_framer_entry_joins_the_sensor_track_and_confirmed_wins() {
    let mut r = repo();
    let e = r
        .record_sighting(&track(433.972e6, 36e3, tr(0.0, 2.3), 20), None)
        .unwrap()
        .emitter_id;
    let d = r
        .record_sighting(&framed(433.973e6, 30e3, tr(0.03, 2.3), 19), None)
        .unwrap();
    assert!(d.created, "a channel-sharing identity never joins by fingerprint");
    let d = d.emitter_id;
    // The user promoted the framer's entry first: it survives although the track was first seen.
    confirm(&mut r, d, LifecycleAuthor::User, "tok-1", "my sensor");
    let m = r
        .merge_same_emission(e, d, t(3.0), "same emission", &tol())
        .unwrap()
        .expect("merged");
    assert_eq!((m.from, m.into), (e, d));
    assert_eq!(listed(&r), vec![d]);
    assert_eq!(r.emitter(d).unwrap().count, 20);
    assert_eq!(r.emitter_lifecycle_history(d).unwrap().len(), 1);

    // A confirmed row merged into a candidate confirms it; the history says so.
    let x = r
        .record_sighting(&track(915e6, 40e3, tr(0.0, 1.0), 3), None)
        .unwrap()
        .emitter_id;
    let y = r
        .record_sighting(&track(920e6, 40e3, tr(0.0, 1.0), 3), None)
        .unwrap()
        .emitter_id;
    confirm(&mut r, y, LifecycleAuthor::User, "tok-1", "keep");
    r.merge_emitters(y, x, t(9.0), "user merge").unwrap();
    assert_eq!(
        r.emitter_lifecycle_state(x).unwrap(),
        LifecycleState::Confirmed
    );
    let h = r.emitter_lifecycle_history(x).unwrap();
    assert_eq!(h.len(), 1);
    assert_eq!(
        (h[0].previous, h[0].author, h[0].actor.as_str()),
        (LifecycleState::Candidate, LifecycleAuthor::User, "tok-1")
    );
    assert!(
        h[0].reason.contains("keep") && h[0].reason.contains("merged from emitter"),
        "{}",
        h[0].reason
    );
    assert_eq!(listed(&r).len(), 2);
}

#[test]
fn t082_distinct_emitters_stay_apart() {
    let mut r = repo();
    // Two stations 200 kHz apart, on air together, and a decode of the upper one.
    let a = r
        .record_sighting(&track(101.3e6, 200e3, tr(0.0, 5.0), 5), None)
        .unwrap()
        .emitter_id;
    let b = r
        .record_sighting(&track(101.5e6, 200e3, tr(0.0, 5.0), 5), None)
        .unwrap()
        .emitter_id;
    assert_ne!(a, b);
    let d = r
        .record_sighting(&rds(101.501e6, tr(1.0, 4.0), "B0B0"), None)
        .unwrap()
        .emitter_id;
    let partners: Vec<EmitterId> = r
        .same_emission_partners(d, &tol())
        .unwrap()
        .into_iter()
        .map(|p| p.0)
        .collect();
    assert_eq!(partners, vec![b]);
    assert!(r.same_emission(a, b, &tol()).unwrap().is_none());
    assert!(r.same_emission(a, d, &tol()).unwrap().is_none());
    r.merge_same_emission(d, b, t(5.0), "same emission", &tol())
        .unwrap()
        .expect("merged");

    // Another identity on the same channel: two identities never merge.
    let other = r
        .record_sighting(&rds(101.5e6, tr(2.0, 3.0), "B0B1"), None)
        .unwrap()
        .emitter_id;
    assert!(r.same_emission(b, other, &tol()).unwrap().is_none());

    // The same frequency a day later is not this emission.
    let late = r
        .record_sighting(&rds(101.3e6, tr(86_400.0, 86_401.0), "C0DE"), None)
        .unwrap()
        .emitter_id;
    assert!(r.same_emission(a, late, &tol()).unwrap().is_none());

    // A hop set and a channel decode inside it stay apart.
    let hop = r
        .record_sighting(
            &track_fp(
                Fingerprint {
                    hop_raster_hz: Some(200e3),
                    hop_set_hz: vec![868.1e6, 868.3e6, 868.5e6],
                    ..Fingerprint::new(868.3e6, 600e3)
                },
                tr(0.0, 5.0),
                30,
            ),
            None,
        )
        .unwrap()
        .emitter_id;
    let channel = r
        .record_sighting(&framed(868.3e6, 50e3, tr(1.0, 2.0), 4), None)
        .unwrap()
        .emitter_id;
    assert!(r.same_emission(hop, channel, &tol()).unwrap().is_none());

    // Two sensors sharing a channel at once, told apart by period, stay apart.
    let fp = |period| Fingerprint {
        period_s: Some(period),
        ..Fingerprint::new(433.92e6, 36e3)
    };
    let s1 = r
        .record_sighting(&track_fp(fp(1.0), tr(0.0, 10.0), 10), None)
        .unwrap()
        .emitter_id;
    let s2 = r
        .record_sighting(&track_fp(fp(0.12), tr(0.0, 10.0), 80), None)
        .unwrap()
        .emitter_id;
    assert_ne!(s1, s2);
    assert!(r.same_emission(s1, s2, &tol()).unwrap().is_none());
}

#[test]
fn t082_after_deleting_a_merged_entry_redetection_creates_one_new_candidate() {
    let mut r = repo();
    let e = r
        .record_sighting(&track(101.303e6, 200e3, tr(0.0, 5.0), 5), None)
        .unwrap()
        .emitter_id;
    let d = r
        .record_sighting(&rds(101.302e6, tr(1.0, 4.0), "C0DE"), None)
        .unwrap()
        .emitter_id;
    let survivor = r
        .merge_same_emission(e, d, t(5.0), "same emission", &tol())
        .unwrap()
        .expect("merged")
        .into;
    r.change_emitter_lifecycle(
        survivor,
        LifecycleState::Deleted,
        LifecycleAuthor::User,
        "tok-1",
        "delete",
        t(6.0),
    )
    .unwrap();
    assert!(listed(&r).is_empty());

    // Detected again: a track entry and a decoder entry, linked into one new candidate.
    let c = r
        .record_sighting(&track(101.303e6, 200e3, tr(100.0, 105.0), 5), None)
        .unwrap();
    assert!(c.created);
    let d2 = r
        .record_sighting(&rds(101.302e6, tr(101.0, 104.0), "C0DE"), None)
        .unwrap();
    assert!(d2.created);
    assert!(r.same_emission(survivor, c.emitter_id, &tol()).unwrap().is_none());
    let partners: Vec<EmitterId> = r
        .same_emission_partners(d2.emitter_id, &tol())
        .unwrap()
        .into_iter()
        .map(|p| p.0)
        .collect();
    assert_eq!(partners, vec![c.emitter_id], "the deleted entry is no partner");
    let m = r
        .merge_same_emission(d2.emitter_id, c.emitter_id, t(105.0), "same emission", &tol())
        .unwrap()
        .expect("merged");
    assert_eq!(listed(&r), vec![m.into]);
    assert_eq!(
        r.emitter_lifecycle_state(m.into).unwrap(),
        LifecycleState::Candidate
    );
    assert_eq!(
        r.emitter_lifecycle_state(survivor).unwrap(),
        LifecycleState::Deleted
    );
    assert_eq!(r.emitter(survivor).unwrap().count, 5);
    assert!(
        r.merge_same_emission(survivor, m.into, t(106.0), "x", &tol())
            .unwrap()
            .is_none()
    );
}

#[test]
fn t082_refined_centre_links_an_offset_decode() {
    let mut r = repo();
    let e = r
        .record_sighting(&track(101.0e6, 60e3, tr(0.0, 5.0), 5), None)
        .unwrap()
        .emitter_id;
    let d = r
        .record_sighting(&framed(101.05e6, 40e3, tr(1.0, 4.0), 3), None)
        .unwrap()
        .emitter_id;
    assert!(r.same_emission(e, d, &tol()).unwrap().is_none());
    r.insert_refined_tuning(&RefinedTuning {
        emitter_id: e,
        provenance: REFINED_BY_OUTPUT_ANALYSIS.into(),
        objective: "hk-demod/wfm-output@1".into(),
        mode: "wfm".into(),
        source: "test".into(),
        center_hz: 101.05e6,
        bandwidth_hz: 180e3,
        start_center_hz: 101.0e6,
        start_bandwidth_hz: 60e3,
        detected_center_hz: 0.0,
        detected_bandwidth_hz: 0.0,
        objective_value: 62.5,
        locked: true,
        converged: true,
        iterations: 7,
        evaluations: 20,
        elapsed_s: 0.8,
        mode_params: BTreeMap::new(),
        t: t(3.0),
    })
    .unwrap();
    assert!(r.same_emission(e, d, &tol()).unwrap().is_some());
    assert_eq!(
        r.same_emission_partners(e, &tol())
            .unwrap()
            .into_iter()
            .map(|p| p.0)
            .collect::<Vec<_>>(),
        vec![d]
    );
}
