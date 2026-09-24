//! T-078 inventory lifecycle: candidate by default, confirm, user delete, re-detection after
//! delete, recurrence statistics.

use super::{RepoError, Repository};
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

fn fsk_fp(f: f64) -> Fingerprint {
    Fingerprint {
        symbol_rate_hz: Some(4800.0),
        deviation_hz: Some(9600.0),
        period_s: Some(0.12),
        duty_cycle: Some(0.15),
        ..Fingerprint::new(f, 36e3)
    }
}

/// A stored track with a measured duty cycle, and its sighting.
fn stored_track(repo: &mut Repository, f: f64, seen: TimeRange, duty: f64, count: u64) -> Sighting {
    let track = Track {
        id: TrackId::new(),
        state: TrackState::Closed,
        split_from: None,
        time: seen,
        f_center_hz: f,
        bandwidth_hz: 36e3,
        detection_count: count,
        timing: TimingFeatures {
            duty_cycle: Some(duty),
            ..TimingFeatures::default()
        },
        updated_at: seen.end,
    };
    repo.upsert_track(&track).unwrap();
    let mut s = Sighting::track(&track, fsk_fp(f));
    s.count = count;
    s
}

fn listed(repo: &Repository, states: Vec<LifecycleState>) -> Vec<EmitterId> {
    repo.query_inventory(&InventoryQuery {
        states,
        ..InventoryQuery::default()
    })
    .unwrap()
    .entries
    .into_iter()
    .map(|e| e.emitter.id)
    .collect()
}

#[test]
fn t078_candidate_by_default_then_confirmed_then_deleted_and_hidden() {
    let mut r = repo();
    let s = stored_track(&mut r, 433.92e6, tr(0.0, 2.4), 0.15, 20);
    let id = r.record_sighting(&s, None).unwrap().emitter_id;
    assert_eq!(
        r.emitter_lifecycle_state(id).unwrap(),
        LifecycleState::Candidate
    );
    assert!(r.emitter_lifecycle_history(id).unwrap().is_empty());
    assert_eq!(listed(&r, vec![]), vec![id]);
    assert_eq!(listed(&r, vec![LifecycleState::Candidate]), vec![id]);
    assert!(listed(&r, vec![LifecycleState::Confirmed]).is_empty());

    let c = r
        .change_emitter_lifecycle(
            id,
            LifecycleState::Confirmed,
            LifecycleAuthor::Auto,
            "rule@1",
            "evidence",
            t(2.4),
        )
        .unwrap()
        .expect("changed");
    assert_eq!(
        (c.previous, c.state),
        (LifecycleState::Candidate, LifecycleState::Confirmed)
    );
    // Confirming again changes nothing and appends nothing.
    assert!(
        r.change_emitter_lifecycle(
            id,
            LifecycleState::Confirmed,
            LifecycleAuthor::User,
            "tok-1",
            "again",
            t(3.0)
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(listed(&r, vec![LifecycleState::Confirmed]), vec![id]);
    let entry = r.emitter_with_access(id, IdentityAccess::Standard).unwrap();
    assert_eq!(entry.lifecycle, LifecycleState::Confirmed);

    r.change_emitter_lifecycle(
        id,
        LifecycleState::Deleted,
        LifecycleAuthor::User,
        "tok-1",
        "not interesting",
        t(4.0),
    )
    .unwrap()
    .expect("deleted");
    assert!(
        listed(&r, vec![]).is_empty(),
        "a deleted entry leaves the list"
    );
    assert!(
        listed(
            &r,
            vec![LifecycleState::Confirmed, LifecycleState::Candidate]
        )
        .is_empty()
    );
    assert_eq!(listed(&r, vec![LifecycleState::Deleted]), vec![id]);
    // History and links are kept.
    let h = r.emitter_lifecycle_history(id).unwrap();
    assert_eq!(h.len(), 2);
    assert_eq!(h[1].author, LifecycleAuthor::User);
    assert_eq!(h[1].reason, "not interesting");
    assert_eq!(r.emitter(id).unwrap().count, 20);
    assert!(!r.emitter_links(id).unwrap().is_empty());
}

#[test]
fn t078_transition_rules() {
    let mut r = repo();
    let s = stored_track(&mut r, 433.92e6, tr(0.0, 1.0), 0.15, 5);
    let id = r.record_sighting(&s, None).unwrap().emitter_id;
    let change = |r: &mut Repository, to, author, reason: &str| {
        r.change_emitter_lifecycle(id, to, author, "actor", reason, t(1.0))
    };
    use LifecycleAuthor::*;
    use LifecycleState::*;
    assert!(matches!(
        change(&mut r, Candidate, User, "x"),
        Err(RepoError::Invalid(_))
    ));
    assert!(matches!(
        change(&mut r, Deleted, Auto, "x"),
        Err(RepoError::Invalid(_))
    ));
    assert!(matches!(
        change(&mut r, Confirmed, User, " "),
        Err(RepoError::Invalid(_))
    ));
    assert!(matches!(
        r.change_emitter_lifecycle(EmitterId::new(), Confirmed, User, "a", "b", t(1.0)),
        Err(RepoError::NotFound { .. })
    ));
    change(&mut r, Deleted, User, "gone").unwrap();
    assert!(matches!(
        change(&mut r, Confirmed, User, "x"),
        Err(RepoError::NotFound { .. })
    ));
    assert!(matches!(
        change(&mut r, Deleted, User, "x"),
        Err(RepoError::NotFound { .. })
    ));
    // The history is append-only.
    assert!(
        r.conn
            .execute("UPDATE emitter_lifecycle SET reason = 'edited'", [])
            .is_err()
    );
}

#[test]
fn t078_redetection_after_delete_creates_a_new_candidate() {
    let mut r = repo();
    let first = stored_track(&mut r, 433.92e6, tr(0.0, 2.4), 0.15, 20);
    let old = r.record_sighting(&first, None).unwrap().emitter_id;
    // A second sighting of the same signal joins it while it is listed.
    let again = stored_track(&mut r, 433.921e6, tr(10.0, 12.4), 0.15, 20);
    assert_eq!(r.record_sighting(&again, None).unwrap().emitter_id, old);
    r.change_emitter_lifecycle(
        old,
        LifecycleState::Deleted,
        LifecycleAuthor::User,
        "tok-1",
        "delete",
        t(13.0),
    )
    .unwrap();

    // Detected again later: a new candidate, the deleted row untouched.
    let later = stored_track(&mut r, 433.92e6, tr(100.0, 102.4), 0.15, 20);
    let res = r.record_sighting(&later, None).unwrap();
    assert!(res.created, "{res:?}");
    assert_ne!(res.emitter_id, old);
    assert_eq!(
        r.emitter_lifecycle_state(res.emitter_id).unwrap(),
        LifecycleState::Candidate
    );
    assert_eq!(r.emitter(old).unwrap().count, 40);
    assert_eq!(listed(&r, vec![]), vec![res.emitter_id]);
    // Re-offering a source counted into the deleted row is a fresh sighting too.
    let re = r.record_sighting(&first, None).unwrap();
    assert_eq!(re.emitter_id, res.emitter_id);
    assert_eq!(r.emitter(old).unwrap().count, 40);
    // A deleted entry cannot be merged.
    assert!(matches!(
        r.merge_emitters(old, res.emitter_id, t(200.0), "test"),
        Err(RepoError::Invalid(_))
    ));
}

#[test]
fn t078_identity_held_by_a_deleted_entry_moves_to_the_new_candidate() {
    let mut r = repo();
    let identity = DecodedIdentity {
        scheme: IdentityScheme::RdsPi,
        value: "C0DE".into(),
    };
    let sighting = |start: f64| Sighting {
        source: LinkTarget::Demodulation(DemodulationId::new()),
        seen: tr(start, start + 1.0),
        count: 1,
        f_center_hz: 101.3e6,
        bandwidth_hz: 180e3,
        fingerprint: Some(Fingerprint::new(101.3e6, 180e3)),
        identity: Some(IdentityClaim {
            identity: identity.clone(),
            content_class: ContentClass::Unrestricted,
        }),
        context: None,
        classification: None,
        tags: Vec::new(),
    };
    let old = r.record_sighting(&sighting(0.0), None).unwrap().emitter_id;
    assert_eq!(
        r.identity_decode_evidence(old).unwrap(),
        Some((IdentityScheme::RdsPi, 0))
    );
    r.change_emitter_lifecycle(
        old,
        LifecycleState::Deleted,
        LifecycleAuthor::User,
        "tok-1",
        "delete",
        t(2.0),
    )
    .unwrap();
    let res = r.record_sighting(&sighting(50.0), None).unwrap();
    assert!(res.created);
    assert_ne!(res.emitter_id, old);
    assert_eq!(
        r.emitter_by_identity(&identity).unwrap().map(|e| e.id),
        Some(res.emitter_id)
    );
    assert_eq!(r.identity_decode_evidence(old).unwrap(), None);
}

#[test]
fn t078_recurrence_from_the_observation_ledger() {
    let mut r = repo();
    let a = stored_track(&mut r, 433.92e6, tr(0.0, 2.0), 0.25, 10);
    let id = r.record_sighting(&a, None).unwrap().emitter_id;
    let b = stored_track(&mut r, 433.92e6, tr(8.0, 10.0), 0.5, 30);
    assert_eq!(r.record_sighting(&b, None).unwrap().emitter_id, id);
    let rec = r.emitter_recurrence(id, 1).unwrap();
    assert_eq!(rec.occurrences, 40);
    assert_eq!(rec.appearances, 2);
    assert!((rec.span_s - 10.0).abs() < 1e-9, "{rec:?}");
    assert!((rec.on_air_s - 1.5).abs() < 1e-9, "{rec:?}");
    assert!((rec.duty_cycle.unwrap() - 0.15).abs() < 1e-9);
    assert_eq!(rec.recent.len(), 1);
    assert_eq!(rec.recent[0].time.start, t(8.0));
    assert_eq!(rec.recent[0].count, 30);
}

/// T-403: a confirmation's **reason** may strengthen; its **state** and its **time** may not.
///
/// An entry is confirmed by whichever rule is satisfied first, and the rules are not satisfied at
/// the same moment — occupancy evidence is complete seconds before a demodulator can report a
/// lock, and how much before depends on how loaded the host is. Without this the recorded
/// explanation is whichever route won a race, so the same capture explains itself differently on a
/// busy machine than on an idle one.
#[test]
fn t403_a_confirmations_reason_strengthens_without_inventing_a_state_change() {
    let mut r = repo();
    let s = stored_track(&mut r, 101.3e6, tr(0.0, 14.0), 1.0, 14);
    let id = r.record_sighting(&s, None).unwrap().emitter_id;
    r.change_emitter_lifecycle(
        id,
        LifecycleState::Confirmed,
        LifecycleAuthor::Auto,
        "hk-pipeline/confirm@1",
        "continuous and trusted",
        t(2.0),
    )
    .unwrap()
    .expect("the first sufficient evidence confirms it");

    // The stronger reason is recorded as a new row, and it is the one a reader of "why" sees.
    let up = r
        .restate_emitter_lifecycle(
            id,
            LifecycleAuthor::Auto,
            "hk-pipeline/confirm@1",
            "verified wfm emission",
            t(3.0),
        )
        .unwrap()
        .expect("a stronger reason is recorded");
    assert_eq!(up.state, LifecycleState::Confirmed);
    assert_eq!(
        up.previous,
        LifecycleState::Confirmed,
        "state == previous, so a reader counting transitions filters it out"
    );
    let h = r.emitter_lifecycle_history(id).unwrap();
    assert_eq!(h.len(), 2, "append-only: the first reason is still there");
    assert_eq!(h[0].reason, "continuous and trusted");
    assert_eq!(h[1].reason, "verified wfm emission");
    assert_eq!(
        h.iter()
            .find(|c| c.state == LifecycleState::Confirmed)
            .unwrap()
            .t,
        t(2.0),
        "time-to-Confirmed is the first transition and does not move"
    );
    assert_eq!(
        r.emitter_lifecycle_state(id).unwrap(),
        LifecycleState::Confirmed
    );

    // The same reason again is not a change, so nothing accumulates on a repeating review.
    assert!(
        r.restate_emitter_lifecycle(
            id,
            LifecycleAuthor::Auto,
            "hk-pipeline/confirm@1",
            "verified wfm emission",
            t(4.0),
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(r.emitter_lifecycle_history(id).unwrap().len(), 2);

    // A candidate has no confirmation to restate: this never confirms anything by itself.
    let s2 = stored_track(&mut r, 99.5e6, tr(0.0, 14.0), 1.0, 14);
    let other = r.record_sighting(&s2, None).unwrap().emitter_id;
    assert!(
        r.restate_emitter_lifecycle(
            other,
            LifecycleAuthor::Auto,
            "hk-pipeline/confirm@1",
            "verified wfm emission",
            t(4.0),
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(
        r.emitter_lifecycle_state(other).unwrap(),
        LifecycleState::Candidate
    );
    assert!(r.emitter_lifecycle_history(other).unwrap().is_empty());
}

/// ADR-0015 §5.5 (MAUTO M-9, T-860): a synthesized decode never counts toward the identity
/// confirm route, and an identity that rests only on synthesized decodes says so.
#[test]
fn t860_synthesized_decodes_never_count_toward_identity_and_are_marked() {
    let mut r = repo();
    let identity = DecodedIdentity {
        scheme: IdentityScheme::AdsbIcao,
        value: "4CA2B1".into(),
    };
    let e = r
        .record_sighting(
            &Sighting {
                source: LinkTarget::Demodulation(DemodulationId::new()),
                seen: tr(0.0, 1.0),
                count: 1,
                f_center_hz: 1090e6,
                bandwidth_hz: 2e6,
                fingerprint: None,
                identity: Some(IdentityClaim {
                    identity: identity.clone(),
                    content_class: ContentClass::Unrestricted,
                }),
                context: None,
                classification: None,
                tags: Vec::new(),
            },
            None,
        )
        .unwrap()
        .emitter_id;
    let decode = |decoder_id: &str, at: f64| Decode {
        id: DecodeId::new(),
        demodulation_ref: None,
        recording_ref: None,
        decoder_id: decoder_id.into(),
        decoder_version: "1".into(),
        frame_model: "adsb-df17".into(),
        metadata: serde_json::json!({}),
        content: None,
        crc_status: CrcStatus::Valid,
        identity: Some(identity.clone()),
        content_class: ContentClass::Unrestricted,
        t: t(at),
        provenance: None,
    };
    let no_identity = r
        .record_sighting(
            &Sighting {
                source: LinkTarget::Demodulation(DemodulationId::new()),
                seen: tr(0.0, 1.0),
                count: 1,
                f_center_hz: 433.92e6,
                bandwidth_hz: 50e3,
                fingerprint: Some(Fingerprint::new(433.92e6, 50e3)),
                identity: None,
                context: None,
                classification: None,
                tags: Vec::new(),
            },
            None,
        )
        .unwrap()
        .emitter_id;
    assert_eq!(r.identity_synthesized(no_identity).unwrap(), None);
    // An identity sighting with no decode behind it is not "synthesized".
    assert_eq!(r.identity_synthesized(e).unwrap(), Some(false));

    for i in 0..3 {
        r.insert_decode(&decode("synth:adsb", f64::from(i)))
            .unwrap();
    }
    assert_eq!(
        r.identity_decode_evidence(e).unwrap(),
        Some((IdentityScheme::AdsbIcao, 0)),
        "synthesized decodes never feed the identity route"
    );
    assert_eq!(r.identity_synthesized(e).unwrap(), Some(true));

    // One ordinary decoder's decode: the identity is no longer synthesized-only.
    r.insert_decode(&decode("dump1090", 5.0)).unwrap();
    assert_eq!(
        r.identity_decode_evidence(e).unwrap(),
        Some((IdentityScheme::AdsbIcao, 1))
    );
    assert_eq!(r.identity_synthesized(e).unwrap(), Some(false));
}
