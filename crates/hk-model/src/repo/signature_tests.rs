//! T-218: migration 0009 (C18 signature storage, ADR-0016 §5/§9). The engine-level guarantees the
//! T-201 matcher and the T-202 clusterer will build on, checked here so they are not re-discovered
//! later: a signature version is immutable but retirable, a match is append-only, the outcome and
//! candidate columns agree, and both tables round-trip the `hk_model::signature` JSON bodies.

use rusqlite::params;

use super::{Repository, blob};
use crate::cluster::{Fingerprint, Sighting};
use crate::emitter::LinkTarget;
use crate::ids::{EmitterId, TrackId};
use crate::region::TimeRange;
use crate::signature::{
    MatchOutcome, SIGNATURE_SCHEMA, Signature, SignatureCandidate, SignatureKind, SignatureMatch,
    SignatureProvenance,
};
use crate::time::Timestamp;

fn t(sec: i64) -> Timestamp {
    Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + sec * 1_000_000_000)
}

/// One emitter to hang matches on (the foreign key is real: a match without an emitter is not a
/// row this store accepts).
fn an_emitter(r: &mut Repository) -> EmitterId {
    r.record_sighting(
        &Sighting {
            source: LinkTarget::Track(TrackId::new()),
            seen: TimeRange::new(t(0), t(1)),
            count: 3,
            f_center_hz: 148.5e6,
            bandwidth_hz: 12.5e3,
            fingerprint: Some(Fingerprint::new(148.5e6, 12.5e3)),
            identity: None,
            context: None,
            classification: None,
            tags: Vec::new(),
        },
        None,
    )
    .unwrap()
    .emitter_id
}

fn signature(version: u32) -> Signature {
    use crate::classify::TaxonomyRef;
    use crate::signature::{FieldExpect, FieldSpec};
    use std::collections::BTreeMap;

    let mut fields = BTreeMap::new();
    for (name, value) in [("symbol_rate_hz", 1200.0), ("deviation_hz", 4500.0)] {
        fields.insert(
            name.to_owned(),
            FieldSpec::required(FieldExpect::Value { value }),
        );
    }
    fields.insert(
        "sync_word".to_owned(),
        FieldSpec::required(FieldExpect::Bits {
            bits: "01111100".into(),
            max_errors: 1,
        }),
    );
    Signature {
        schema: SIGNATURE_SCHEMA,
        id: "pocsag-1200".into(),
        version,
        name: "POCSAG 1200".into(),
        kind: SignatureKind::Protocol,
        taxonomy: Some(TaxonomyRef::current()),
        family: Some("fsk".into()),
        class: None,
        fields,
        min_discriminating: 3,
        recipe: None,
        provenance: SignatureProvenance::Builtin,
        author: "hackriff".into(),
        created_at: t(0),
        supersedes: (version > 1).then(|| version - 1),
        bands_hz: Vec::new(),
        notes: None,
    }
}

fn insert(r: &Repository, s: &Signature) -> rusqlite::Result<usize> {
    r.conn.execute(
        "INSERT INTO signature (signature_id, version, name, kind, taxonomy, family, provenance, \
         author, created_at, supersedes, body) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            s.id,
            s.version,
            s.name,
            s.kind.as_str(),
            s.taxonomy.as_ref().map(ToString::to_string),
            s.family,
            s.provenance.as_str(),
            s.author,
            s.created_at.as_unix_nanos(),
            s.supersedes,
            serde_json::to_string(s).unwrap(),
        ],
    )
}

#[test]
fn a_signature_version_is_immutable_retirable_and_round_trips() {
    let r = Repository::open_in_memory().unwrap();
    let s = signature(1);
    s.validate().unwrap();
    insert(&r, &s).unwrap();

    // Same id and version twice: the primary key refuses it. A new version is fine.
    assert!(insert(&r, &signature(1)).is_err());
    insert(&r, &signature(2)).unwrap();

    let body: String = r
        .conn
        .query_row(
            "SELECT body FROM signature WHERE signature_id = ?1 AND version = 2",
            params![s.id],
            |row| row.get(0),
        )
        .unwrap();
    let back: Signature = serde_json::from_str(&body).unwrap();
    assert_eq!(back, signature(2));
    assert_eq!(back.supersedes, Some(1));

    // Content is immutable; retiring is not content.
    let edit = r.conn.execute(
        "UPDATE signature SET name = 'other' WHERE signature_id = ?1 AND version = 1",
        params![s.id],
    );
    assert!(edit.is_err(), "a signature version must be immutable");
    r.conn
        .execute(
            "UPDATE signature SET retired_at = ?2 WHERE signature_id = ?1 AND version = 1",
            params![s.id, t(10).as_unix_nanos()],
        )
        .unwrap();
    let retired: Option<i64> = r
        .conn
        .query_row(
            "SELECT retired_at FROM signature WHERE signature_id = ?1 AND version = 1",
            params![s.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retired, Some(t(10).as_unix_nanos()));

    // Retired, not deleted: every version stays readable.
    assert!(
        r.conn
            .execute(
                "DELETE FROM signature WHERE signature_id = ?1",
                params![s.id]
            )
            .is_err(),
        "signatures are retired, never deleted"
    );
    let versions: i64 = r
        .conn
        .query_row("SELECT count(*) FROM signature", [], |row| row.get(0))
        .unwrap();
    assert_eq!(versions, 2);
}

#[test]
fn a_signature_match_is_append_only_and_its_outcome_agrees_with_its_candidate() {
    let mut r = Repository::open_in_memory().unwrap();
    let emitter = an_emitter(&mut r);

    let mut m = SignatureMatch {
        schema: SIGNATURE_SCHEMA,
        emitter_id: emitter,
        t: t(1),
        outcome: MatchOutcome::Partial,
        features_ref: Some("features:1".into()),
        signatures_rev: 3,
        candidates: vec![SignatureCandidate {
            signature: signature(1).reference(),
            name: "POCSAG 1200".into(),
            score: 0.55,
            agreement: Vec::new(),
            missing: vec!["sync_word".into()],
            conflicting: Vec::new(),
            recipe: None,
        }],
        reasons: vec!["sync_not_measured".into()],
    };
    m.validate().unwrap();

    let insert_match = |r: &Repository, m: &SignatureMatch| {
        let top = m.top();
        r.conn.execute(
            "INSERT INTO signature_match (emitter_id, t, outcome, signature_id, version, score, \
             features_ref, signatures_rev, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                blob(m.emitter_id),
                m.t.as_unix_nanos(),
                m.outcome.as_str(),
                top.map(|c| c.signature.id.clone()),
                top.map(|c| c.signature.version),
                top.map(|c| c.score),
                m.features_ref,
                m.signatures_rev as i64,
                serde_json::to_string(m).unwrap(),
            ],
        )
    };
    insert_match(&r, &m).unwrap();

    // A later, better match is appended; the earlier one stays.
    m.t = t(2);
    m.outcome = MatchOutcome::Full;
    m.candidates[0].score = 0.93;
    m.candidates[0].missing.clear();
    m.reasons.clear();
    m.validate().unwrap();
    insert_match(&r, &m).unwrap();

    let rows: Vec<(String, f64)> = r
        .conn
        .prepare("SELECT outcome, score FROM signature_match ORDER BY match_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows.len(), 2, "history is kept");
    assert_eq!(rows[1].0, "full");
    assert!((rows[1].1 - 0.93).abs() < 1e-12);

    assert!(
        r.conn
            .execute("UPDATE signature_match SET score = 1.0", [])
            .is_err(),
        "matches are append-only"
    );
    assert!(
        r.conn.execute("DELETE FROM signature_match", []).is_err(),
        "matches are append-only"
    );

    // The CHECK constraints: 'none' carries no candidate, and any other outcome does.
    let bad = r.conn.execute(
        "INSERT INTO signature_match (emitter_id, t, outcome, signature_id, version, score, \
         signatures_rev, body) VALUES (?1, ?2, 'none', 'pocsag-1200', 1, 0.9, 1, '{}')",
        params![blob(emitter), t(3).as_unix_nanos()],
    );
    assert!(bad.is_err(), "a none outcome names no signature");
    let bad = r.conn.execute(
        "INSERT INTO signature_match (emitter_id, t, outcome, signatures_rev, body) \
         VALUES (?1, ?2, 'full', 1, '{}')",
        params![blob(emitter), t(4).as_unix_nanos()],
    );
    assert!(bad.is_err(), "a full outcome names its signature");
}
