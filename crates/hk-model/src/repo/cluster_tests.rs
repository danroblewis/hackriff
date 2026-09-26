//! Entity resolution and inventory query tests (T-018). Use-case ids are in the test names.

use rusqlite::params;

use super::{RepoError, Repository, blob};
use crate::cluster::*;
use crate::*;

const DAY: i64 = 86_400;

/// 2026-09 plus `sec` seconds.
fn t(sec: i64) -> Timestamp {
    Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + sec * 1_000_000_000)
}

fn tr(a: i64, b: i64) -> TimeRange {
    TimeRange::new(t(a), t(b))
}

fn repo() -> Repository {
    Repository::open_in_memory().unwrap()
}

/// The `fsk_burst_train` sensor's features (2-FSK 4800 Bd ±9.6 kHz, 120 ms period).
fn fsk_fp(f: f64) -> Fingerprint {
    Fingerprint {
        symbol_rate_hz: Some(4800.0),
        deviation_hz: Some(9600.0),
        period_s: Some(0.12),
        duty_cycle: Some(0.15),
        burst_length_s: Some(0.018),
        ..Fingerprint::new(f, 36e3)
    }
}

fn track_sighting(fp: Fingerprint, seen: TimeRange, count: u64) -> Sighting {
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

fn claim(scheme: IdentityScheme, value: &str, class: ContentClass) -> IdentityClaim {
    IdentityClaim {
        identity: DecodedIdentity {
            scheme,
            value: value.into(),
        },
        content_class: class,
    }
}

fn decode_sighting(
    identity: IdentityClaim,
    at: i64,
    f: f64,
    bw: f64,
    context: Option<EmitterId>,
) -> Sighting {
    Sighting {
        source: LinkTarget::Decode(DecodeId::new()),
        seen: TimeRange::instant(t(at)),
        count: 1,
        f_center_hz: f,
        bandwidth_hz: bw,
        fingerprint: None,
        identity: Some(identity),
        context,
        classification: None,
        tags: Vec::new(),
    }
}

fn wfm(f: f64) -> Fingerprint {
    Fingerprint {
        family: Some("wfm".into()),
        ..Fingerprint::new(f, 180e3)
    }
}

/// AWARE-036 / docs/07 §2.11 acceptance: two sessions of one emitter → one Emitter, count summed,
/// first/last seen spanning both, `unknown` status from the clusterer, `known` after the T-013
/// CRC ground truth (which a later prior does not override); replays never double-count.
#[test]
fn aware_036_two_sessions_one_emitter_count_summed_unknown_then_known() {
    let mut r = repo();
    let s1 = track_sighting(fsk_fp(915.005e6), tr(0, 3), 24);
    let a = r.record_sighting(&s1, None).unwrap();
    assert!(a.created);
    assert_eq!(a.assignment, Assignment::Created);

    // Next day: CFO drifted 3 kHz, period and rate estimates jittered.
    let mut fp2 = fsk_fp(915.008e6);
    fp2.period_s = Some(0.121);
    fp2.symbol_rate_hz = Some(4810.0);
    let s2 = track_sighting(fp2, tr(DAY, DAY + 3), 25);
    let b = r.record_sighting(&s2, None).unwrap();
    assert_eq!(b.emitter_id, a.emitter_id);
    assert!(
        matches!(b.assignment, Assignment::Fingerprint { .. }),
        "{b:?}"
    );

    let e = r.emitter(a.emitter_id).unwrap();
    assert_eq!((e.count, e.seen()), (49, tr(0, DAY + 3)));
    assert_eq!(e.identity, Identity::Unknown);
    assert_eq!(e.known_status, KnownStatus::Unknown);
    let history = r.known_status_history(a.emitter_id).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].author, StatusAuthor::Clusterer);
    let fp = r.emitter_fingerprint(a.emitter_id).unwrap().unwrap();
    assert_eq!(fp.observations, 2);
    assert!(fp.f_center_hz > 915.005e6 && fp.f_center_hz < 915.008e6);

    // Identical replays of both sessions add nothing.
    for s in [&s1, &s2] {
        let x = r.record_sighting(s, None).unwrap();
        assert_eq!(
            (x.emitter_id, x.assignment, x.count_added, x.created),
            (a.emitter_id, Assignment::Replay, 0, false)
        );
    }
    // A track re-offered after it grew adds only its growth.
    let mut grown = s2.clone();
    grown.count = 30;
    grown.seen = tr(DAY, DAY + 4);
    assert_eq!(r.record_sighting(&grown, None).unwrap().count_added, 5);
    let e = r.emitter(a.emitter_id).unwrap();
    assert_eq!((e.count, e.seen()), (54, tr(0, DAY + 4)));
    assert_eq!(r.emitter_links(a.emitter_id).unwrap().len(), 2);

    // T-013: CRC-valid framing ground truth appends `known` (author decoder).
    r.append_known_status(&KnownStatusChange {
        emitter_id: a.emitter_id,
        status: KnownStatus::Known,
        prior_ref: None,
        reason: "CRC-valid inferred framing".into(),
        t: t(DAY + 5),
        author: StatusAuthor::Decoder,
    })
    .unwrap();
    let prior = |_: &str, _: f64, _: f64| PriorVerdict {
        status: KnownStatus::UnexpectedHere,
        prior_ref: Some("bandplan:test".into()),
        reason: "test prior".into(),
    };
    let mut s3 = track_sighting(fsk_fp(915.006e6), tr(2 * DAY, 2 * DAY + 1), 3);
    s3.classification = Some(Classification {
        t: t(2 * DAY + 1),
        family: "2fsk".into(),
        confidence: 0.9,
        open_set_score: 0.1,
        model_version: "fsk-rules@1".into(),
    });
    let c = r.record_sighting(&s3, Some(&prior)).unwrap();
    assert_eq!((c.emitter_id, c.status_appended), (a.emitter_id, None));
    assert_eq!(
        r.emitter(a.emitter_id).unwrap().known_status,
        KnownStatus::Known
    );

    let known = r
        .query_inventory(&InventoryQuery {
            status: vec![KnownStatus::Known],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(known.entries.len(), 1);
    assert_eq!(known.entries[0].family.as_deref(), Some("2fsk"));

    // The classification records its input and feature-set version, once.
    r.record_sighting(&s3, Some(&prior)).unwrap();
    let classes = r.classification_history(a.emitter_id).unwrap();
    assert_eq!(classes.len(), 1);
    assert_eq!(classes[0].input, Some(s3.source));
    assert_eq!(classes[0].feature_set_version, Some(FEATURE_SET_VERSION));
}

/// T-336, the cross-producer half of the sighting rule. `count` totals *occurrences*, so one
/// emission over one stretch of air is one occurrence whoever watched it — including when two
/// producers' observations reach one entry directly (by fingerprint here), the route T-329
/// measured counting the same five seconds twice. Both halves are asserted, because "always
/// discount" satisfies the first alone and destroys the count: disjoint spans are two
/// occurrences, and partial overlap counts the shared stretch once, as the larger of the two.
#[test]
fn one_span_two_producers_is_one_occurrence_and_disjoint_spans_are_two() {
    let mut r = repo();
    let rows = |r: &Repository, id: EmitterId| -> i64 {
        r.conn
            .query_row(
                "SELECT count(*) FROM emitter_observation WHERE emitter_id = ?1",
                [blob(id)],
                |x| x.get(0),
            )
            .unwrap()
    };

    // THE PROPERTY. The tracker watches 3 s of a sensor and counts 4 bursts.
    let tracker = track_sighting(fsk_fp(915.005e6), tr(0, 3), 4);
    let a = r.record_sighting(&tracker, None).unwrap();
    assert!(a.created);
    let id = a.emitter_id;
    // A second producer — its own source row, its own centre estimate — watches the same 3 s and
    // counts the same 4 bursts. Same entry by fingerprint, and no new air time.
    let chain = track_sighting(fsk_fp(915.0051e6), tr(0, 3), 4);
    let b = r.record_sighting(&chain, None).unwrap();
    assert_eq!(b.emitter_id, id);
    assert!(
        matches!(b.assignment, Assignment::Fingerprint { .. }),
        "{b:?}"
    );
    assert_eq!(b.count_added, 0);
    assert_eq!(r.emitter(id).unwrap().count, 4);
    // The ledger still records who measured what: two rows, both reading 4. A row's count is
    // what its producer measured, never what it added.
    assert_eq!(rows(&r, id), 2);

    // THE CONTROL. A later session over a disjoint span is a second occurrence and adds in full;
    // without this, discounting everything would pass the property and destroy the count.
    let c = r
        .record_sighting(&track_sighting(fsk_fp(915.005e6), tr(10, 13), 4), None)
        .unwrap();
    assert_eq!((c.emitter_id, c.count_added), (id, 4));
    assert_eq!(r.emitter(id).unwrap().count, 8);

    // PARTIAL OVERLAP, decided by rule 1's threshold and not prorated. Sharing the majority of
    // the shorter span (8 s of 10) is the same air: the two count once, as the larger of them
    // (6, not 4 + 6), so only the excess adds.
    let d = r
        .record_sighting(&track_sighting(fsk_fp(915.005e6), tr(22, 32), 6), None)
        .unwrap();
    assert_eq!((d.emitter_id, d.count_added), (id, 6)); // new air: adds in full
    let e = r
        .record_sighting(&track_sighting(fsk_fp(915.005e6), tr(24, 34), 8), None)
        .unwrap();
    assert_eq!((e.emitter_id, e.count_added), (id, 2));
    assert_eq!(r.emitter(id).unwrap().count, 16);

    // Sharing less than that (2 s of 10) is new air, and counts in full — the same near-miss
    // rule 1 makes for a touching window or a few ms of jitter on an edge.
    let f = r
        .record_sighting(&track_sighting(fsk_fp(915.005e6), tr(50, 60), 5), None)
        .unwrap();
    assert_eq!((f.emitter_id, f.count_added), (id, 5));
    let g = r
        .record_sighting(&track_sighting(fsk_fp(915.005e6), tr(58, 68), 6), None)
        .unwrap();
    assert_eq!((g.emitter_id, g.count_added), (id, 6));
    assert_eq!(r.emitter(id).unwrap().count, 27);
    assert_eq!(rows(&r, id), 7);
}

#[test]
fn two_close_emitters_with_different_fingerprints_stay_apart() {
    let mut r = repo();
    let sensor = |f: f64| Fingerprint {
        symbol_rate_hz: Some(4800.0),
        period_s: Some(30.0),
        burst_length_s: Some(0.02),
        ..Fingerprint::new(f, 30e3)
    };
    let beacon = |f: f64| Fingerprint {
        symbol_rate_hz: Some(2400.0),
        period_s: Some(10.0),
        burst_length_s: Some(0.2),
        ..Fingerprint::new(f, 30e3)
    };
    let a = r
        .record_sighting(&track_sighting(sensor(433.920e6), tr(0, 60), 2), None)
        .unwrap();
    let b = r
        .record_sighting(&track_sighting(beacon(433.922e6), tr(0, 60), 6), None)
        .unwrap();
    assert!(a.created && b.created);
    assert_ne!(a.emitter_id, b.emitter_id);
    for day in 1..3 {
        let s = day * DAY;
        let a2 = r
            .record_sighting(&track_sighting(sensor(433.9215e6), tr(s, s + 60), 2), None)
            .unwrap();
        let b2 = r
            .record_sighting(&track_sighting(beacon(433.9205e6), tr(s, s + 60), 6), None)
            .unwrap();
        assert_eq!((a2.emitter_id, b2.emitter_id), (a.emitter_id, b.emitter_id));
    }
    let band = r
        .query_inventory(&InventoryQuery {
            freq: Some(FreqRange::new(433.9e6, 433.94e6)),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(band.entries.len(), 2);
    assert_eq!(r.emitter(a.emitter_id).unwrap().count, 6);
    assert_eq!(r.emitter(b.emitter_id).unwrap().count, 18);
}

/// SIGNAL-001: ADS-B identity sightings from two replays → one Emitter per ICAO; replaying the
/// same decodes does not double the count; the shared 1090 MHz channel context neither takes an
/// aircraft identity nor absorbs anonymous tracks into an aircraft.
#[test]
fn signal_001_adsb_one_emitter_per_icao_not_doubled_on_replay() {
    let mut r = repo();
    let channel = r
        .record_sighting(
            &track_sighting(Fingerprint::new(1090e6, 2e6), tr(0, 600), 500),
            None,
        )
        .unwrap()
        .emitter_id;
    let icao = |v: &str| claim(IdentityScheme::AdsbIcao, v, ContentClass::Unrestricted);
    let replay1: Vec<Sighting> = [
        ("a1b2c3", 10),
        ("a1b2c3", 20),
        ("abcdef", 15),
        ("abcdef", 25),
    ]
    .into_iter()
    .map(|(v, at)| decode_sighting(icao(v), at, 1090e6, 2e6, Some(channel)))
    .collect();
    for _ in 0..2 {
        for s in &replay1 {
            let res = r.record_sighting(s, None).unwrap();
            assert!(res.conflict.is_none() && res.merge.is_none(), "{res:?}");
        }
    }
    for (v, at) in [("a1b2c3", DAY + 10), ("abcdef", DAY + 15)] {
        r.record_sighting(
            &decode_sighting(icao(v), at, 1090e6, 2e6, Some(channel)),
            None,
        )
        .unwrap();
    }
    let a = r
        .emitter_by_identity(&icao("a1b2c3").identity)
        .unwrap()
        .unwrap();
    let b = r
        .emitter_by_identity(&icao("abcdef").identity)
        .unwrap()
        .unwrap();
    assert_ne!(a.id, b.id);
    assert_eq!((a.count, a.seen()), (3, tr(10, DAY + 10)));
    assert_eq!((b.count, b.seen()), (3, tr(15, DAY + 15)));

    let ch = r.emitter(channel).unwrap();
    assert_eq!((ch.identity, ch.count), (Identity::Unknown, 500));
    let anon = r
        .record_sighting(
            &track_sighting(Fingerprint::new(1090e6, 2e6), tr(DAY, DAY + 600), 400),
            None,
        )
        .unwrap();
    assert_eq!(anon.emitter_id, channel);

    let aircraft = r
        .query_inventory(&InventoryQuery {
            identity_scheme: Some(IdentityScheme::AdsbIcao),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(aircraft.entries.len(), 2);
    assert!(
        aircraft
            .entries
            .iter()
            .all(|e| matches!(e.identity, InventoryIdentity::Clear { .. }))
    );
}

/// SIGNAL-062: the RDS PI decoded on a station's detection-track emitter names that emitter
/// (no separate identity emitter); the next session's track finds the station by fingerprint.
#[test]
fn signal_062_rds_pi_names_the_station_track_emitter() {
    let mut r = repo();
    let track = r
        .record_sighting(&track_sighting(wfm(98.1e6), tr(0, 60), 1), None)
        .unwrap();
    let pi = claim(IdentityScheme::RdsPi, "1694", ContentClass::Unrestricted);
    let d = r
        .record_sighting(
            &decode_sighting(pi.clone(), 30, 98.1e6, 200e3, Some(track.emitter_id)),
            None,
        )
        .unwrap();
    assert_eq!(
        (d.emitter_id, d.assignment, d.created),
        (track.emitter_id, Assignment::Context, false)
    );
    let next = r
        .record_sighting(&track_sighting(wfm(98.1004e6), tr(DAY, DAY + 60), 1), None)
        .unwrap();
    assert_eq!(next.emitter_id, track.emitter_id);
    let e = r.emitter(track.emitter_id).unwrap();
    assert_eq!(e.identity, Identity::Decoded(pi.identity));
    // Two occurrences, not three (T-336): the PI was decoded at t=30, inside the minute the
    // track already counted, so the decode names the station without counting its air again.
    // The next day's track is a second occurrence.
    assert_eq!((e.count, e.seen()), (2, tr(0, DAY + 60)));
    assert_eq!(
        r.query_inventory(&InventoryQuery::default())
            .unwrap()
            .entries
            .len(),
        1
    );
}

/// SIGNAL-062 merge path: the station already exists from an identity-only session; a new
/// session's track emitter gets the PI → the track emitter merges into the station. Links are
/// re-pointed (old rows superseded, kept), stale ids follow the merge, and queries return only
/// the survivor.
#[test]
fn merge_repoints_links_and_queries_return_only_the_survivor() {
    let mut r = repo();
    let pi = claim(IdentityScheme::RdsPi, "C0DE", ContentClass::Unrestricted);
    let track_s = track_sighting(wfm(88.5e6), tr(DAY, DAY + 60), 1);
    let tid = r.record_sighting(&track_s, None).unwrap().emitter_id;
    let recording = LinkTarget::Recording(RecordingId::new());
    r.link_emitter(&EmitterLink {
        emitter_id: tid,
        target: recording,
        linked_at: t(DAY + 61),
    })
    .unwrap();
    r.add_emitter_tag(tid, "survey-2").unwrap();
    // An identity-only writer (e.g. RDS without an emitter hint) created the station earlier.
    let sid = r
        .upsert_emitter_observation(&EmitterObservation {
            emitter_id: EmitterId::new(),
            seen: tr(0, 120),
            count: 4,
            f_center_hz: 88.5e6,
            bandwidth_hz: 200e3,
            identity: Some(pi.identity.clone()),
        })
        .unwrap()
        .emitter_id;
    assert_eq!(
        r.query_inventory(&InventoryQuery::default())
            .unwrap()
            .entries
            .len(),
        2
    );

    let decode = decode_sighting(pi.clone(), DAY + 30, 88.5e6, 200e3, Some(tid));
    let d = r.record_sighting(&decode, None).unwrap();
    assert_eq!((d.emitter_id, d.assignment), (sid, Assignment::Identity));
    let m = d.merge.clone().expect("merged");
    assert_eq!(
        (m.from, m.into, m.from_count, m.identity_moved),
        (tid, sid, 1, false)
    );
    assert_eq!(r.emitter_merges(sid).unwrap(), vec![m]);

    let e = r.emitter(sid).unwrap();
    // 4 occurrences from the identity-only writer + the track's 1. The decode that triggered the
    // merge sits inside the track's minute, which the merge has just brought onto this entry, so
    // it adds nothing (T-336); before that check it made a sixth occurrence out of air already
    // counted.
    assert_eq!((e.count, e.seen()), (5, tr(0, DAY + 60)));
    assert!(e.tags.contains("survey-2"));
    assert_eq!(r.live_emitter_id(tid).unwrap(), sid);

    assert!(r.emitter_links(tid).unwrap().is_empty());
    let history = r.emitter_link_history(tid).unwrap();
    assert_eq!(history.len(), 2);
    assert!(
        history
            .iter()
            .all(|l| l.superseded_by == Some(sid) && l.superseded_at == Some(t(DAY + 30)))
    );
    let live: Vec<LinkTarget> = r
        .emitter_links(sid)
        .unwrap()
        .into_iter()
        .map(|l| l.target)
        .collect();
    for target in [track_s.source, recording, decode.source] {
        assert!(live.contains(&target), "{target:?} in {live:?}");
    }

    let page = r.query_inventory(&InventoryQuery::default()).unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].emitter.id, sid);
    let region = Region::new(FreqRange::new(88e6, 89e6), tr(0, 2 * DAY));
    let in_region: Vec<EmitterId> = r
        .emitters_in_region(&region)
        .unwrap()
        .iter()
        .map(|e| e.id)
        .collect();
    assert_eq!(in_region, vec![sid]);

    // Replaying the merged emitter's track resolves to the survivor without counting.
    let rep = r.record_sighting(&track_s, None).unwrap();
    assert_eq!(
        (rep.emitter_id, rep.assignment, rep.count_added),
        (sid, Assignment::Replay, 0)
    );
    // Stale ids held by producers follow the merge.
    let up = r
        .upsert_emitter_observation(&EmitterObservation {
            emitter_id: tid,
            seen: tr(DAY + 100, DAY + 101),
            count: 1,
            f_center_hz: 88.5e6,
            bandwidth_hz: 200e3,
            identity: None,
        })
        .unwrap();
    assert_eq!(up.emitter_id, sid);
    // A disjoint minute: a sixth occurrence, added in full.
    assert_eq!(r.emitter(sid).unwrap().count, 6);

    // Two identified emitters are never merged; merged emitters cannot merge again.
    let other = r
        .record_sighting(
            &decode_sighting(
                claim(IdentityScheme::RdsPi, "BEEF", ContentClass::Unrestricted),
                0,
                90e6,
                200e3,
                None,
            ),
            None,
        )
        .unwrap()
        .emitter_id;
    assert!(matches!(
        r.merge_emitters(other, sid, t(3 * DAY), "user"),
        Err(RepoError::IdentityConflict { .. })
    ));
    assert!(matches!(
        r.merge_emitters(tid, other, t(3 * DAY), "user"),
        Err(RepoError::Invalid(_))
    ));

    // A user merge of an identified emitter into an anonymous one moves the identity.
    let anon = r
        .record_sighting(&track_sighting(wfm(107.7e6), tr(0, 10), 2), None)
        .unwrap()
        .emitter_id;
    let moved = r.merge_emitters(other, anon, t(3 * DAY), "user").unwrap();
    assert!(moved.identity_moved);
    let beef = DecodedIdentity {
        scheme: IdentityScheme::RdsPi,
        value: "BEEF".into(),
    };
    assert_eq!(r.emitter_by_identity(&beef).unwrap().unwrap().id, anon);
    assert_eq!(r.emitter(other).unwrap().identity, Identity::Unknown);
}

#[test]
fn identity_conflicts_are_reported_not_merged() {
    let mut r = repo();
    let a = r
        .record_sighting(&track_sighting(wfm(100.1e6), tr(0, 10), 1), None)
        .unwrap()
        .emitter_id;
    let pi = |v: &str| claim(IdentityScheme::RdsPi, v, ContentClass::Unrestricted);
    r.record_sighting(
        &decode_sighting(pi("AAAA"), 5, 100.1e6, 200e3, Some(a)),
        None,
    )
    .unwrap();
    // A different PI on the same context emitter: reported; a new emitter holds it.
    let c = r
        .record_sighting(
            &decode_sighting(pi("BBBB"), 6, 100.1e6, 200e3, Some(a)),
            None,
        )
        .unwrap();
    assert!(c.created && c.merge.is_none());
    assert_ne!(c.emitter_id, a);
    assert_eq!(
        c.conflict,
        Some(IdentityConflictReport {
            emitters: vec![a],
            reason: ConflictReason::ContextHoldsOtherIdentity
        })
    );
    // Two identities now share one fingerprint cluster: an anonymous track takes neither.
    let anon = r
        .record_sighting(&track_sighting(wfm(100.1e6), tr(20, 30), 1), None)
        .unwrap();
    assert!(anon.created);
    let report = anon.conflict.expect("reported");
    assert_eq!(
        report.reason,
        ConflictReason::FingerprintMatchesSeveralIdentities
    );
    assert_eq!(report.emitters.len(), 2);
    // Later anonymous tracks join that unidentified cluster instead of minting more.
    let again = r
        .record_sighting(&track_sighting(wfm(100.1e6), tr(40, 50), 1), None)
        .unwrap();
    assert_eq!(again.emitter_id, anon.emitter_id);
    assert_eq!(
        r.emitter(a).unwrap().identity,
        Identity::Decoded(pi("AAAA").identity)
    );
}

#[test]
fn inventory_queries_filter_by_region_time_status_tag_scheme_family_and_paginate() {
    let mut r = repo();
    let mut mk = |f: f64, seen: TimeRange, family: &str| -> EmitterId {
        let mut s = track_sighting(Fingerprint::new(f, 10e3), seen, 1);
        s.classification = Some(Classification {
            t: seen.end,
            family: family.into(),
            confidence: 0.8,
            open_set_score: 0.2,
            model_version: "test@1".into(),
        });
        r.record_sighting(&s, None).unwrap().emitter_id
    };
    let ism1 = mk(433.92e6, tr(0, 100), "ook");
    let ism2 = mk(434.50e6, tr(1000, 1100), "2fsk");
    let air = mk(1090e6, tr(500, 600), "ppm");
    let old = mk(162.0e6, tr(-5000, -4000), "gmsk");
    r.add_emitter_tag(ism2, "watch").unwrap();
    r.add_emitter_tag(air, "watch").unwrap();
    r.append_known_status(&KnownStatusChange {
        emitter_id: air,
        status: KnownStatus::Known,
        prior_ref: None,
        reason: "user checked".into(),
        t: t(700),
        author: StatusAuthor::User,
    })
    .unwrap();

    let ids = |q: InventoryQuery| -> Vec<EmitterId> {
        r.query_inventory(&q)
            .unwrap()
            .entries
            .iter()
            .map(|e| e.emitter.id)
            .collect()
    };
    let q = InventoryQuery::default;
    assert_eq!(ids(q()), vec![ism2, air, ism1, old]);
    assert_eq!(
        ids(InventoryQuery {
            freq: Some(FreqRange::new(433e6, 435e6)),
            ..q()
        }),
        vec![ism2, ism1]
    );
    assert_eq!(
        ids(InventoryQuery {
            time: Some(tr(50, 550)),
            ..q()
        }),
        vec![air, ism1]
    );
    assert_eq!(
        ids(InventoryQuery {
            status: vec![KnownStatus::Known],
            ..q()
        }),
        vec![air]
    );
    assert_eq!(
        ids(InventoryQuery {
            status: vec![KnownStatus::Unknown, KnownStatus::UnexpectedHere],
            ..q()
        }),
        vec![ism2, ism1, old]
    );
    assert_eq!(
        ids(InventoryQuery {
            tag: Some("watch".into()),
            ..q()
        }),
        vec![ism2, air]
    );
    assert_eq!(
        ids(InventoryQuery {
            family: Some("2fsk".into()),
            ..q()
        }),
        vec![ism2]
    );
    assert_eq!(
        ids(InventoryQuery {
            freq: Some(FreqRange::new(433e6, 435e6)),
            tag: Some("watch".into()),
            time: Some(tr(0, 2000)),
            ..q()
        }),
        vec![ism2]
    );
    assert!(
        ids(InventoryQuery {
            identity_scheme: Some(IdentityScheme::AdsbIcao),
            ..q()
        })
        .is_empty()
    );

    let first = r
        .query_inventory(&InventoryQuery { limit: 3, ..q() })
        .unwrap();
    assert_eq!(first.entries.len(), 3);
    assert_eq!(first.next_offset, Some(3));
    let second = r
        .query_inventory(&InventoryQuery {
            limit: 3,
            offset: 3,
            ..q()
        })
        .unwrap();
    assert_eq!(
        second
            .entries
            .iter()
            .map(|e| e.emitter.id)
            .collect::<Vec<_>>(),
        vec![old]
    );
    assert_eq!(second.next_offset, None);
}

/// T-171: `count_inventory` matches the same filters as `query_inventory` but ignores
/// `limit`/`offset`, including the default exclusion of deleted rows.
#[test]
fn count_inventory_matches_filters_and_ignores_pagination() {
    let mut r = repo();
    let mk = |r: &mut Repository, f: f64, seen: TimeRange| -> EmitterId {
        let s = track_sighting(Fingerprint::new(f, 10e3), seen, 1);
        r.record_sighting(&s, None).unwrap().emitter_id
    };
    let ism1 = mk(&mut r, 433.92e6, tr(0, 100));
    mk(&mut r, 434.50e6, tr(1000, 1100));
    mk(&mut r, 1090e6, tr(500, 600));
    mk(&mut r, 162.0e6, tr(-5000, -4000));

    let q = InventoryQuery::default();
    assert_eq!(r.count_inventory(&q).unwrap(), 4);
    assert_eq!(
        r.query_inventory(&InventoryQuery {
            limit: 2,
            ..q.clone()
        })
        .unwrap()
        .entries
        .len(),
        2,
        "count is unaffected by a page smaller than the total"
    );
    assert_eq!(
        r.count_inventory(&InventoryQuery {
            freq: Some(FreqRange::new(433e6, 435e6)),
            ..q.clone()
        })
        .unwrap(),
        2
    );

    r.change_emitter_lifecycle(
        ism1,
        LifecycleState::Deleted,
        LifecycleAuthor::User,
        "user",
        "gone",
        tr(0, 100).end,
    )
    .unwrap();
    assert_eq!(
        r.count_inventory(&q).unwrap(),
        3,
        "deleted rows are excluded by default, as in query_inventory"
    );
    assert_eq!(
        r.count_inventory(&InventoryQuery {
            states: vec![LifecycleState::Deleted],
            ..q
        })
        .unwrap(),
        1
    );
}

/// T-158: an emitter's latest measured `snr_peak_db`/`peak_level_dbfs` comes from the newest
/// (highest `t_start`) detection linked to it, directly or through a currently-linked track;
/// `None` until a detection is linked. T-350: and it is dated with **that detection's**
/// `TimeRange`, which moves as the winning detection does.
#[test]
fn emitter_latest_measurement_reads_the_newest_linked_detection() {
    let mut r = repo();
    let plan = ScanPlan {
        id: ScanPlanId::new(),
        version: 1,
        name: "ism".into(),
        created_at: t(0),
        regions: vec![PlanRegion {
            freq: FreqRange::new(433e6, 435e6),
            priority: 1.0,
            revisit_ns: None,
        }],
        policy: ScanPolicy::SweepThenDwell,
        gain_table: vec![],
        schedule: Schedule::Cron {
            expr: "* * * * *".into(),
        },
        extra: serde_json::Value::Null,
    };
    r.insert_scan_plan(&plan).unwrap();
    let survey = Survey {
        id: SurveyId::new(),
        plan_id: plan.id,
        plan_version: plan.version,
        device_id: "test".into(),
        state: SurveyState::Open,
        t_start: t(0),
        t_end: None,
        summary: None,
    };
    r.insert_survey(&survey).unwrap();
    let prov_id = r
        .intern_provenance(&Provenance {
            device_id: "test".into(),
            tune: Tune {
                center_hz: 433.92e6,
                sample_rate_hz: 8e6,
                lna_db: 24.0,
                vga_db: 20.0,
                amp_on: false,
                bandwidth_hz: 200e3,
            },
            overload: false,
            quantisation_limited: false,
            noise_sigma_lsb: None,
            temperature_c: None,
            antenna_port: None,
            bias_tee: crate::BiasTee::Unknown,
            clock_source: ClockSource::Internal,
            clock_locked: true,
            calibration_state_ref: None,
            spur_mask_ref: None,
            timestamp_method: TimestampMethod::Synthetic,
            timestamp_error_budget_ns: None,
            capture_artefacts: Vec::new(),
        })
        .unwrap();
    let det = |snr: f64, peak: f32, seen: TimeRange| Detection {
        id: DetectionId::new(),
        survey_id: survey.id,
        time: seen,
        f_center_hz: 433.92e6,
        obw_hz: 12e3,
        xdb_bandwidth_hz: None,
        xdb_level_db: None,
        snr_peak_db: snr,
        snr_mean_db: snr - 3.0,
        peak_level_dbfs: peak,
        peak_level_dbm: None,
        sk: None,
        clip_count: 0,
        detector_version: "test@1".into(),
        provenance_ref: prov_id,
        flags: DetectionFlags::default(),
    };

    // A candidate from a track sighting with nothing linked yet: no measurement.
    let s = track_sighting(Fingerprint::new(433.92e6, 12e3), tr(0, 1), 1);
    let track_id = match s.source {
        LinkTarget::Track(id) => id,
        _ => unreachable!(),
    };
    let id = r.record_sighting(&s, None).unwrap().emitter_id;
    assert_eq!(r.emitter_latest_measurement(id).unwrap(), None);

    // Two detections linked through the track: the newer one (by t_start) is read.
    let d_old = det(10.0, -50.0, tr(0, 1));
    let d_new = det(22.5, -18.25, tr(10, 11));
    r.insert_detections(&[d_old.clone(), d_new.clone()])
        .unwrap();
    r.upsert_track(&Track {
        id: track_id,
        state: TrackState::Open,
        split_from: None,
        time: tr(0, 11),
        f_center_hz: 433.92e6,
        bandwidth_hz: 12e3,
        detection_count: 2,
        timing: TimingFeatures::default(),
        updated_at: t(11),
    })
    .unwrap();
    r.link_detections_to_track(track_id, &[d_old.id, d_new.id], t(11))
        .unwrap();
    // T-350: the measurement carries the detection's own time extent, so a reader can never
    // hold the level without knowing when it was taken. `tr(10, 11)` is `d_new`'s span, not
    // `d_old`'s and not the track's hull `tr(0, 11)` - the emitter's own extent would be the
    // wrong answer here and is exactly what a hull would have given.
    assert_eq!(
        r.emitter_latest_measurement(id).unwrap(),
        Some(LatestMeasurement {
            snr_peak_db: 22.5,
            peak_level_dbfs: -18.25,
            time: tr(10, 11),
        })
    );

    // A detection linked directly to the emitter wins when it is newer still.
    let d_direct = det(30.0, -5.5, tr(20, 21));
    r.insert_detections(std::slice::from_ref(&d_direct))
        .unwrap();
    r.link_emitter(&EmitterLink {
        emitter_id: id,
        target: LinkTarget::Detection(d_direct.id),
        linked_at: t(21),
    })
    .unwrap();
    assert_eq!(
        r.emitter_latest_measurement(id).unwrap(),
        Some(LatestMeasurement {
            snr_peak_db: 30.0,
            peak_level_dbfs: -5.5,
            // T-350: the directly-linked detection's own span, moving with the measurement.
            time: tr(20, 21),
        })
    );
}

/// T-034: re-demodulating the same IQ mints new source ids but is the same measurement, so it
/// adds nothing; a later capture, a touching span, another producer or channel, an unkeyed
/// sighting and a different identity all still count.
#[test]
fn remeasurement_of_the_same_iq_is_not_counted_again() {
    let mut r = repo();
    let key = MeasurementKey::new("hk-rds");
    let pi = claim(IdentityScheme::RdsPi, "1694", ContentClass::Unrestricted);
    let session = |seen: TimeRange, f: f64, identity: &IdentityClaim| Sighting {
        source: LinkTarget::Demodulation(DemodulationId::new()),
        seen,
        count: 1,
        f_center_hz: f,
        bandwidth_hz: 180e3,
        fingerprint: Some(wfm(f)),
        identity: Some(identity.clone()),
        context: None,
        classification: None,
        tags: Vec::new(),
    };
    let first = r
        .record_sighting_measured(&session(tr(0, 5), 101.3e6, &pi), &key, None)
        .unwrap();
    assert!(first.created);
    let id = first.emitter_id;
    let count = |r: &Repository| r.emitter(id).unwrap().count;

    // Re-demodulation: new demodulation id, edges a few ms off, centre 40 Hz off.
    let jittered = TimeRange::new(
        Timestamp::from_unix_nanos(t(0).as_unix_nanos() + 2_000_000),
        Timestamp::from_unix_nanos(t(5).as_unix_nanos() + 3_000_000),
    );
    for _ in 0..2 {
        let again = r
            .record_sighting_measured(&session(jittered, 101.30004e6, &pi), &key, None)
            .unwrap();
        assert_eq!(
            (again.emitter_id, again.assignment, again.count_added),
            (id, Assignment::Replay, 0)
        );
    }
    assert_eq!(count(&r), 1);

    // A second capture an hour later is a new observation, and its own re-run is not.
    let later = r
        .record_sighting_measured(&session(tr(3600, 3605), 101.3e6, &pi), &key, None)
        .unwrap();
    assert_eq!((later.emitter_id, later.count_added), (id, 1));
    r.record_sighting_measured(&session(tr(3600, 3605), 101.3e6, &pi), &key, None)
        .unwrap();
    assert_eq!(count(&r), 2);
    // Back-to-back windows only touch: counted.
    r.record_sighting_measured(&session(tr(5, 10), 101.3e6, &pi), &key, None)
        .unwrap();
    assert_eq!(count(&r), 3);
    // Another producer on the same IQ is a separate *measurement* — its own ledger row, not a
    // replay — but not a separate occurrence: it watched air the first producer already counted,
    // so it adds nothing (T-336, the cross-producer half of the rule).
    let other = r
        .record_sighting_measured(
            &session(tr(0, 5), 101.3e6, &pi),
            &MeasurementKey::new("hk-other"),
            None,
        )
        .unwrap();
    assert_ne!(other.assignment, Assignment::Replay);
    assert_eq!(other.count_added, 0);
    assert_eq!(count(&r), 3);
    // Same for an unkeyed sighting of that air: a new ledger row, no new occurrence.
    r.record_sighting(&session(tr(0, 5), 101.3e6, &pi), None)
        .unwrap();
    assert_eq!(count(&r), 3);
    // Another channel, or another station's identity on this one, is not this measurement.
    let beef = claim(IdentityScheme::RdsPi, "BEEF", ContentClass::Unrestricted);
    let other_channel = r
        .record_sighting_measured(&session(tr(0, 5), 101.7e6, &beef), &key, None)
        .unwrap();
    assert!(other_channel.created);
    let clash = r
        .record_sighting_measured(&session(tr(0, 5), 101.3e6, &beef), &key, None)
        .unwrap();
    assert_eq!(clash.emitter_id, other_channel.emitter_id);
    assert_ne!(clash.assignment, Assignment::Replay);
    assert_eq!(count(&r), 3);
}

#[test]
fn emitter_links_only_allow_recording_supersession() {
    let mut r = repo();
    let a = r
        .record_sighting(
            &track_sighting(Fingerprint::new(1e8, 1e4), tr(0, 1), 1),
            None,
        )
        .unwrap()
        .emitter_id;
    let b = r
        .record_sighting(
            &track_sighting(Fingerprint::new(2e8, 1e4), tr(0, 1), 1),
            None,
        )
        .unwrap()
        .emitter_id;
    let conn = &r.conn;
    let err = conn
        .execute(
            "UPDATE emitter_link SET linked_at = 0 WHERE emitter_id = ?1",
            [blob(a)],
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("append-only"), "{err}");
    conn.execute(
        "UPDATE emitter_link SET superseded_by = ?2, superseded_at = 5 WHERE emitter_id = ?1",
        params![blob(a), blob(b)],
    )
    .unwrap();
    let err = conn
        .execute(
            "UPDATE emitter_link SET superseded_by = ?1, superseded_at = 6 WHERE emitter_id = ?1",
            [blob(a)],
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("append-only"), "{err}");
}

#[test]
fn invalid_sightings_are_refused() {
    let mut r = repo();
    let mut s = track_sighting(Fingerprint::new(1e8, 1e4), tr(5, 1), 1);
    assert!(matches!(
        r.record_sighting(&s, None),
        Err(RepoError::Invalid(_))
    ));
    s.seen = tr(0, 1);
    s.fingerprint.as_mut().unwrap().period_s = Some(f64::NAN);
    assert!(r.record_sighting(&s, None).is_err());
    assert!(
        r.query_inventory(&InventoryQuery::default())
            .unwrap()
            .entries
            .is_empty(),
        "rolled back"
    );
}

/// T-961, rule 2b: **a decode attaches to the entry whose (t, f) region it came from.**
///
/// The 106.1 MHz case from the explorer's second FM pass (2026-09-25). Blind detection had a row
/// at 106.1127 MHz, 160 kHz wide, still `unknown`; a recipe run on a *band* — so no emitter
/// context to name — decoded its RDS PI on a 200 kHz channel at 106.1126 MHz. The identity was
/// held by nobody, so rule 5 minted a second entry 100 Hz away: two overlapping boxes for one
/// station, and a CRC-valid decode that named neither the detection nor its family.
///
/// The station is one emitter, and the PI belongs to it.
#[test]
fn signal_062_a_decoded_identity_with_no_context_lands_on_the_detection_it_came_from() {
    let mut r = repo();
    let detection = track_sighting(wfm(106.1127e6), tr(0, 12), 3);
    let station = r.record_sighting(&detection, None).unwrap();
    assert_eq!(station.assignment, Assignment::Created);

    let pi = claim(IdentityScheme::RdsPi, "1323", ContentClass::Unrestricted);
    let decode = decode_sighting(pi.clone(), 9, 106.1126e6, 200e3, None);
    let d = r.record_sighting(&decode, None).unwrap();

    assert_eq!(
        d.emitter_id, station.emitter_id,
        "the PI lands on the detection it was decoded from, not a second row"
    );
    assert!(matches!(d.assignment, Assignment::Region { .. }), "{d:?}");
    assert!(!d.created);
    assert_eq!(d.conflict, None);
    assert_eq!(
        r.emitter(station.emitter_id).unwrap().identity,
        Identity::Decoded(pi.identity.clone()),
        "the entry now carries the PI"
    );
    let region = Region::new(FreqRange::new(106.0e6, 106.2e6), tr(0, 60));
    let rows: Vec<EmitterId> = r
        .emitters_in_region(&region)
        .unwrap()
        .iter()
        .map(|e| e.id)
        .collect();
    assert_eq!(rows, vec![station.emitter_id], "one box, not two");

    // The PI's own entry now exists, so further decodes resolve to it by rule 2 as before.
    let again = r
        .record_sighting(&decode_sighting(pi, 11, 106.1126e6, 200e3, None), None)
        .unwrap();
    assert_eq!(
        (again.emitter_id, again.assignment),
        (station.emitter_id, Assignment::Identity)
    );
}

/// T-961: rule 2b reaches only where "the region it came from" names one transmitter.
///
/// Four refusals, each of which would be a wrong attachment: a **channel-sharing** scheme (every
/// aircraft in a 2 MHz ADS-B window is not the one emitter there); an entry that **already holds
/// another identity**; an entry **not on air** when the decode was made; and a decode that names
/// **no channel** at all, so it cannot say what region it came from. Each falls back to rule 5
/// exactly as before.
#[test]
fn signal_062_region_attachment_refuses_a_shared_channel_another_identity_or_another_time() {
    let mut r = repo();
    let window = r
        .record_sighting(
            &track_sighting(Fingerprint::new(1090e6, 2e6), tr(0, 12), 3),
            None,
        )
        .unwrap()
        .emitter_id;
    let icao = claim(
        IdentityScheme::AdsbIcao,
        "a1b2c3",
        ContentClass::Unrestricted,
    );
    let d = r
        .record_sighting(&decode_sighting(icao, 6, 1090e6, 2e6, None), None)
        .unwrap();
    assert!(d.created, "a shared channel names no single transmitter");
    assert_ne!(d.emitter_id, window);

    // Another station's entry, already identified: its identity is not up for replacement.
    let held = claim(IdentityScheme::RdsPi, "3AAB", ContentClass::Unrestricted);
    let other = r
        .record_sighting(&decode_sighting(held, 6, 88.5e6, 200e3, None), None)
        .unwrap()
        .emitter_id;
    let pi = claim(IdentityScheme::RdsPi, "1323", ContentClass::Unrestricted);
    let clash = r
        .record_sighting(&decode_sighting(pi.clone(), 7, 88.5e6, 200e3, None), None)
        .unwrap();
    assert!(clash.created, "{clash:?}");
    assert_ne!(clash.emitter_id, other);

    // On air yesterday, silent now: this decode was not made from it.
    let past = r
        .record_sighting(&track_sighting(wfm(99.7e6), tr(0, 12), 3), None)
        .unwrap()
        .emitter_id;
    let later = r
        .record_sighting(
            &decode_sighting(
                claim(IdentityScheme::RdsPi, "4C5D", ContentClass::Unrestricted),
                DAY,
                99.7e6,
                200e3,
                None,
            ),
            None,
        )
        .unwrap();
    assert!(later.created, "{later:?}");
    assert_ne!(later.emitter_id, past);

    // A producer that recorded no channel (folded to 0 Hz) names no region.
    let unplaced = r
        .record_sighting(
            &decode_sighting(
                claim(IdentityScheme::RdsPi, "6E7F", ContentClass::Unrestricted),
                6,
                0.0,
                0.0,
                None,
            ),
            None,
        )
        .unwrap();
    assert!(unplaced.created, "{unplaced:?}");
}
