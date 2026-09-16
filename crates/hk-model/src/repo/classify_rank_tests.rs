//! T-211: current-family arbitration rank (ADR-0016 §2), generalising T-183. Every pair of
//! writers (M3 rows at each rank, and the three pre-M3 writers: decoder evidence, chain label,
//! track shape) in both write orders and both merge directions; the inventory family filter
//! agrees with the entry's family; migration 0007 keeps pre-M3 rows readable; M3 rows round-trip.

use rusqlite::params;

use super::classify::EFFECTIVE_RANK_SQL;
use super::{RepoError, Repository};
use crate::classify::tests::{assert_close, sample};
use crate::classify::{ArbRank, Stage, TaxonomyRef};
use crate::cluster::*;
use crate::*;

fn t(sec: f64) -> Timestamp {
    Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + (sec * 1e9) as i64)
}

fn tr(a: f64, b: f64) -> TimeRange {
    TimeRange::new(t(a), t(b))
}

fn track(f: f64, seen: TimeRange) -> Sighting {
    let fp = Fingerprint::new(f, 200e3);
    Sighting {
        source: LinkTarget::Track(TrackId::new()),
        seen,
        count: 5,
        f_center_hz: fp.f_center_hz,
        bandwidth_hz: fp.bandwidth_hz,
        fingerprint: Some(fp),
        identity: None,
        context: None,
        classification: None,
        tags: Vec::new(),
    }
}

/// The RDS writer's sighting without its classification (rows are written separately).
fn rds(f: f64, seen: TimeRange) -> Sighting {
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
                value: "C0DE".into(),
            },
            content_class: ContentClass::Unrestricted,
        }),
        context: None,
        classification: None,
        tags: Vec::new(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Writer {
    M3(ArbRank),
    /// `family::record_decoder_evidence` (`decoder:<id>`, no input).
    LegacyDecoder,
    /// A demodulator chain's label (no input, e.g. analog without RDS).
    LegacyChain,
    /// A track sighting's occupancy family (input `track`).
    LegacyTrack,
}

const WRITERS: [Writer; 8] = [
    Writer::M3(ArbRank::User),
    Writer::M3(ArbRank::Decoder),
    Writer::M3(ArbRank::LockVerified),
    Writer::M3(ArbRank::Classifier),
    Writer::M3(ArbRank::TrackShape),
    Writer::LegacyDecoder,
    Writer::LegacyChain,
    Writer::LegacyTrack,
];

impl Writer {
    fn rank(self) -> ArbRank {
        match self {
            Writer::M3(r) => r,
            Writer::LegacyDecoder => ArbRank::Decoder,
            Writer::LegacyChain => ArbRank::Classifier,
            Writer::LegacyTrack => ArbRank::TrackShape,
        }
    }

    fn family(self) -> &'static str {
        match self {
            Writer::M3(ArbRank::User) => "analog",
            Writer::M3(ArbRank::Decoder) => "pulsed",
            Writer::M3(ArbRank::LockVerified) => "fsk",
            Writer::M3(ArbRank::Classifier) => "psk-qam",
            Writer::M3(ArbRank::TrackShape) => "noise-like",
            Writer::LegacyDecoder => "readsb",
            Writer::LegacyChain => "wfm",
            Writer::LegacyTrack => "fm-broadcast",
        }
    }

    fn write(self, r: &mut Repository, id: EmitterId, at: f64) {
        let legacy = |family: &str, model_version: &str| Classification {
            t: t(at),
            family: family.into(),
            confidence: 0.9,
            open_set_score: 0.1,
            model_version: model_version.into(),
        };
        match self {
            Writer::M3(rank) => {
                let stage = match rank {
                    ArbRank::User => Stage::User,
                    ArbRank::Decoder => Stage::Decoder,
                    ArbRank::LockVerified => Stage::Verifier,
                    ArbRank::Classifier => Stage::FeatureTree,
                    ArbRank::TrackShape => Stage::TrackShape,
                };
                let mut c = sample(self.family(), stage);
                c.t = t(at);
                r.record_classification(id, &c, rank).unwrap();
            }
            Writer::LegacyDecoder => r
                .append_classification(id, &legacy("readsb", "decoder:readsb"))
                .unwrap(),
            Writer::LegacyChain => r
                .append_classification(id, &legacy("wfm", "hk-demod/mode@1"))
                .unwrap(),
            Writer::LegacyTrack => {
                r.conn
                    .execute(
                        "INSERT INTO emitter_classification (emitter_id, t, family, confidence, \
                         open_set_score, model_version, input_kind, input_id) \
                         VALUES (?1, ?2, 'fm-broadcast', 0.8, 0.2, 'hk-pipeline/family-map@1', \
                         'track', ?3)",
                        params![
                            id.as_uuid().into_bytes(),
                            t(at).as_unix_nanos(),
                            TrackId::new().as_uuid().into_bytes()
                        ],
                    )
                    .unwrap();
            }
        }
    }
}

/// The entry's family, checking the inventory `family` filter agrees for each writer's family.
fn family(r: &Repository, id: EmitterId, writers: [Writer; 2]) -> Option<String> {
    let family = r
        .query_inventory(&InventoryQuery::default())
        .unwrap()
        .entries
        .into_iter()
        .find(|e| e.emitter.id == id)
        .and_then(|e| e.family);
    for w in writers {
        let q = InventoryQuery {
            family: Some(w.family().into()),
            ..InventoryQuery::default()
        };
        let listed = r
            .query_inventory(&q)
            .unwrap()
            .entries
            .iter()
            .any(|e| e.emitter.id == id);
        assert_eq!(
            listed,
            family.as_deref() == Some(w.family()),
            "family filter {} agrees with the entry family {family:?}",
            w.family()
        );
    }
    family
}

fn winner(a: Writer, b: Writer, later: Writer) -> Writer {
    match a.rank().cmp(&b.rank()) {
        std::cmp::Ordering::Less => a,
        std::cmp::Ordering::Greater => b,
        std::cmp::Ordering::Equal => later,
    }
}

#[test]
fn t211_every_rank_pair_wins_in_both_write_orders_latest_among_equals() {
    for a in WRITERS {
        for b in WRITERS.into_iter().filter(|b| *b != a) {
            let mut r = Repository::open_in_memory().unwrap();
            let id = r
                .record_sighting(&track(101.3e6, tr(0.0, 5.0)), None)
                .unwrap()
                .emitter_id;
            a.write(&mut r, id, 1.0);
            b.write(&mut r, id, 2.0);
            let want = winner(a, b, b);
            assert_eq!(
                family(&r, id, [a, b]).as_deref(),
                Some(want.family()),
                "{a:?} then {b:?}"
            );
            let cur = r.current_classification(id).unwrap().unwrap();
            assert_eq!(cur.arb_rank, want.rank(), "{a:?} then {b:?}");
            assert_eq!(cur.detail.is_some(), matches!(want, Writer::M3(_)));
            let latest = r.latest_classification(id).unwrap().unwrap();
            assert_eq!(latest.classification.family, b.family());
        }
    }
}

#[test]
fn t211_every_rank_pair_wins_in_both_merge_directions() {
    for a in WRITERS {
        for b in WRITERS.into_iter().filter(|b| *b != a) {
            for track_survives in [true, false] {
                let mut r = Repository::open_in_memory().unwrap();
                let e = r
                    .record_sighting(&track(101.3e6, tr(0.0, 5.0)), None)
                    .unwrap()
                    .emitter_id;
                let d = r
                    .record_sighting(&rds(101.3022e6, tr(1.0, 4.0)), None)
                    .unwrap()
                    .emitter_id;
                assert_ne!(e, d);
                let (from, into) = if track_survives { (d, e) } else { (e, d) };
                a.write(&mut r, into, 1.0);
                b.write(&mut r, from, 2.0);
                let m = r
                    .merge_same_emission(from, into, t(5.0), "same emission", &tol())
                    .unwrap()
                    .expect("merged");
                // The absorbed entry's history is appended after the survivor's.
                let absorbed = if m.into == into { b } else { a };
                let want = winner(a, b, absorbed);
                let ctx = format!("{a:?} on {into}, {b:?} on {from}, survivor {}", m.into);
                assert_eq!(
                    family(&r, m.into, [a, b]).as_deref(),
                    Some(want.family()),
                    "{ctx}"
                );
                let cur = r.current_classification(m.into).unwrap().unwrap();
                assert_eq!(cur.arb_rank, want.rank(), "{ctx}: rank columns carried");
                assert_eq!(cur.detail.is_some(), matches!(want, Writer::M3(_)), "{ctx}");
            }
        }
    }
}

/// T-218 (from the T-211 review): a writer that knows *who* it is but has no distribution to
/// offer — a user reclassifying by hand — writes its rank explicitly with
/// `append_classification_ranked`, instead of letting the columns derive rank 3. Without that, a
/// decoder row or a later chain label would take the family back off the user.
#[test]
fn t218_a_ranked_legacy_row_carries_its_stage_and_outranks_later_writers() {
    let mut r = Repository::open_in_memory().unwrap();
    let id = r
        .record_sighting(&track(101.3e6, tr(0.0, 5.0)), None)
        .unwrap()
        .emitter_id;
    let user = Classification {
        t: t(1.0),
        family: "wfm".into(),
        confidence: 0.95,
        open_set_score: 0.05,
        model_version: "hk-ui/reclassify@1".into(),
    };
    r.append_classification_ranked(id, &user, Stage::User, ArbRank::User)
        .unwrap();

    let cur = r.current_classification(id).unwrap().unwrap();
    assert_eq!((cur.stage, cur.arb_rank), (Stage::User, ArbRank::User));
    assert_eq!(cur.classification.family, "wfm");
    // The user's own vocabulary is kept: a reclassification is an assertion, not a measured
    // distribution, so there is no M3 detail and no taxonomy on the row.
    assert!(cur.detail.is_none() && cur.taxonomy.is_none());

    // Every other writer, at every rank, written afterwards: the user still decides.
    for w in WRITERS.into_iter().filter(|w| w.rank() != ArbRank::User) {
        w.write(&mut r, id, 2.0);
        let cur = r.current_classification(id).unwrap().unwrap();
        assert_eq!(
            (cur.classification.family.as_str(), cur.arb_rank),
            ("wfm", ArbRank::User),
            "{w:?} must not overrule the user"
        );
    }
    let listed = r
        .query_inventory(&InventoryQuery {
            family: Some("wfm".into()),
            ..InventoryQuery::default()
        })
        .unwrap()
        .entries
        .iter()
        .any(|e| e.emitter.id == id);
    assert!(listed, "the family filter agrees with the user's label");

    // The (stage, rank) pair is checked, and an unknown emitter is refused.
    assert!(
        r.append_classification_ranked(id, &user, Stage::User, ArbRank::Decoder)
            .is_err(),
        "a user row may not claim the decoder rank"
    );
    assert!(
        r.append_classification_ranked(id, &user, Stage::TrackShape, ArbRank::User)
            .is_err()
    );
    assert!(
        r.append_classification_ranked(EmitterId::new(), &user, Stage::User, ArbRank::User)
            .is_err(),
        "an unknown emitter is refused"
    );
    // A chain writer that knows it is locked is the other user of this seam (ADR-0016 §2).
    r.append_classification_ranked(id, &user, Stage::Chain, ArbRank::LockVerified)
        .unwrap();
    assert_eq!(
        r.current_classification(id).unwrap().unwrap().arb_rank,
        ArbRank::User,
        "still below the user"
    );
}

fn tol() -> Tolerances {
    Tolerances::default()
}

#[test]
fn t211_sql_rank_derivation_matches_the_rust_legacy_rule() {
    let mut r = Repository::open_in_memory().unwrap();
    let id = r
        .record_sighting(&track(101.3e6, tr(0.0, 5.0)), None)
        .unwrap()
        .emitter_id;
    for (mv, kind) in [
        ("decoder:readsb", None),
        ("decoder:readsb", Some("track")),
        ("hk-pipeline/family-map@1", Some("track")),
        ("hk-demod/mode@1", Some("demodulation")),
        ("hk-demod/c20-fsk@0.1.0", Some("decode")),
        ("hk-demod/mode@1", None),
        ("Decoder:x", None),
        ("decoder", None),
        ("", Some("detection")),
    ] {
        let input = kind.map(|_| TrackId::new().as_uuid().into_bytes());
        r.conn
            .execute(
                "INSERT INTO emitter_classification (emitter_id, t, family, confidence, \
                 open_set_score, model_version, input_kind, input_id) \
                 VALUES (?1, 0, 'x', 0.5, 0.5, ?2, ?3, ?4)",
                params![id.as_uuid().into_bytes(), mv, kind, input],
            )
            .unwrap();
        let sql: i64 = r
            .conn
            .query_row(
                &format!(
                    "SELECT {EFFECTIVE_RANK_SQL} FROM emitter_classification c \
                     ORDER BY c.classification_id DESC LIMIT 1"
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        let (stage, rank) = ArbRank::legacy(mv, kind);
        assert_eq!(sql, i64::from(rank.value()), "{mv:?} {kind:?}");
        let read = r.latest_classification(id).unwrap().unwrap();
        assert_eq!((read.stage, read.arb_rank), (stage, rank));
        assert_eq!((read.taxonomy, read.detail), (None, None));
    }
}

#[test]
fn t211_a_later_classifier_unknown_never_hides_a_lock_verified_label() {
    let mut r = Repository::open_in_memory().unwrap();
    let id = r
        .record_sighting(&track(101.3e6, tr(0.0, 5.0)), None)
        .unwrap()
        .emitter_id;
    let locked = sample("analog", Stage::Chain);
    r.record_classification(id, &locked, ArbRank::LockVerified)
        .unwrap();
    let mut unknown = sample(crate::classify::UNKNOWN, Stage::FeatureTree);
    unknown.t = t(9.0);
    r.record_classification(id, &unknown, ArbRank::Classifier)
        .unwrap();
    let cur = r.current_classification(id).unwrap().unwrap();
    assert_eq!(cur.classification.family, "analog");
    assert_eq!(cur.stage, Stage::Chain);
    assert_close(
        r.latest_classification(id)
            .unwrap()
            .unwrap()
            .detail
            .as_ref()
            .unwrap(),
        &unknown,
    );
    assert_eq!(r.classification_history(id).unwrap().len(), 2);
}

#[test]
fn t211_record_classification_refuses_bad_rows() {
    let mut r = Repository::open_in_memory().unwrap();
    let id = r
        .record_sighting(&track(101.3e6, tr(0.0, 5.0)), None)
        .unwrap()
        .emitter_id;
    let tree = sample("fsk", Stage::FeatureTree);
    for rank in [
        ArbRank::User,
        ArbRank::Decoder,
        ArbRank::LockVerified,
        ArbRank::TrackShape,
    ] {
        assert!(matches!(
            r.record_classification(id, &tree, rank),
            Err(RepoError::Invalid(_))
        ));
    }
    let mut broken = tree.clone();
    broken.confidence = 0.5;
    assert!(matches!(
        r.record_classification(id, &broken, ArbRank::Classifier),
        Err(RepoError::Invalid(_))
    ));
    assert!(matches!(
        r.record_classification(EmitterId::new(), &tree, ArbRank::Classifier),
        Err(RepoError::NotFound { .. })
    ));
    assert!(r.classification_history(id).unwrap().is_empty());
}

#[test]
fn t211_migration_0007_keeps_pre_m3_rows_readable_and_m3_rows_round_trip() {
    let path = std::env::temp_dir().join(format!("hk-t211-{}.sqlite", uuid::Uuid::now_v7()));
    let id = {
        let mut r = Repository::open(&path).unwrap();
        let id = r
            .record_sighting(&track(101.3e6, tr(0.0, 5.0)), None)
            .unwrap()
            .emitter_id;
        Writer::LegacyChain.write(&mut r, id, 1.0);
        Writer::LegacyDecoder.write(&mut r, id, 2.0);
        Writer::LegacyTrack.write(&mut r, id, 3.0);
        id
    };
    // Roll the file back to the schema before 0007: pre-M3 rows without the new columns.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        for col in ["detail", "arb_rank", "stage", "taxonomy"] {
            conn.execute_batch(&format!(
                "ALTER TABLE emitter_classification DROP COLUMN {col}"
            ))
            .unwrap();
        }
        // T-219: an older file has no 0008 relation table either, so drop it with the 0007
        // columns. The version is pinned to the one before 0007 by number, not to
        // `SCHEMA_VERSION - 1`, so a later migration does not silently change what is rolled back.
        // T-218: nor the 0009 signature tables. Every later migration's objects have to go, or
        // replaying them onto this file fails on the first `CREATE TABLE`.
        // T-201: nor the 0010 measured-features table.
        // T-202: nor the 0011 cluster tables (children first: they reference each other).
        conn.execute_batch(
            "DROP TABLE IF EXISTS emitter_relation; \
             DROP TABLE IF EXISTS signature_match; \
             DROP TABLE IF EXISTS signature; \
             DROP TABLE IF EXISTS emission_features; \
             DROP TABLE IF EXISTS cluster_event; \
             DROP TABLE IF EXISTS emitter_cluster; \
             DROP TABLE IF EXISTS signature_cluster",
        )
        .unwrap();
        const BEFORE_0007: i64 = 6;
        conn.pragma_update(None, "user_version", BEFORE_0007)
            .unwrap();
    }
    let mut r = Repository::open(&path).unwrap();
    let history = r.classification_history(id).unwrap();
    let derived: Vec<_> = history
        .iter()
        .map(|h| (h.classification.family.as_str(), h.stage, h.arb_rank))
        .collect();
    assert_eq!(
        derived,
        [
            ("wfm", Stage::Chain, ArbRank::Classifier),
            ("readsb", Stage::Decoder, ArbRank::Decoder),
            ("fm-broadcast", Stage::TrackShape, ArbRank::TrackShape),
        ]
    );
    assert!(
        history
            .iter()
            .all(|h| h.taxonomy.is_none() && h.detail.is_none())
    );
    assert_eq!(
        family(&r, id, [Writer::LegacyDecoder, Writer::LegacyChain]).as_deref(),
        Some("readsb")
    );

    // A new row on the migrated file: an explicit rank beats the legacy track derivation.
    let mut c = sample("fsk", Stage::Verifier);
    c.input = Some(LinkTarget::Track(TrackId::new()));
    c.reasons = vec!["too_short".into()];
    r.record_classification(id, &c, ArbRank::LockVerified)
        .unwrap();
    let rec = r.latest_classification(id).unwrap().unwrap();
    assert_close(rec.detail.as_ref().unwrap(), &c);
    assert_eq!(rec.taxonomy, Some(TaxonomyRef::current()));
    assert_eq!(
        (rec.stage, rec.arb_rank),
        (Stage::Verifier, ArbRank::LockVerified)
    );
    assert_eq!(rec.input, c.input);
    assert_eq!(rec.classification.family, "fsk");
    assert_eq!(rec.classification.model_version, "hk-classify/tree@1");
    assert_eq!(
        r.current_classification(id)
            .unwrap()
            .unwrap()
            .classification
            .family,
        "readsb",
        "a decoder (rank 1) still outranks lock-verified (rank 2)"
    );
    let emitter = r.emitter(id).unwrap();
    assert_eq!(emitter.classifications.len(), 4);
    assert_eq!(emitter.classifications[3], rec.classification);
    drop(r);
    // Reopening keeps everything.
    let r = Repository::open(&path).unwrap();
    assert_close(
        r.latest_classification(id)
            .unwrap()
            .unwrap()
            .detail
            .as_ref()
            .unwrap(),
        &c,
    );
    drop(r);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}
