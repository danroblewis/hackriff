//! T-036 legal-guardrail tests: gated decode getters, tags, audited identity reclassification.
//! Use cases: SIGNAL-001 (decoded identities), AWARE-036 (framed FSK identities).

use rusqlite::params;
use serde_json::json;

use super::{RepoError, Repository};
use crate::cluster::*;
use crate::*;

/// 2026-09 plus `sec` seconds.
fn t(sec: i64) -> Timestamp {
    Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + sec * 1_000_000_000)
}

fn repo() -> Repository {
    Repository::open_in_memory().unwrap()
}

fn identity(scheme: &str, value: &str) -> DecodedIdentity {
    DecodedIdentity {
        scheme: IdentityScheme::Other(scheme.into()),
        value: value.into(),
    }
}

/// A decode naming `value` in its identity, metadata and frame model.
fn decode(id: Option<&DecodedIdentity>, marker: &str, class: ContentClass, sec: i64) -> Decode {
    Decode {
        id: DecodeId::new(),
        demodulation_ref: None,
        recording_ref: None,
        decoder_id: "hk-test".into(),
        decoder_version: "0.1.0".into(),
        frame_model: format!("model-{marker}"),
        metadata: json!({"addr": marker, "function": 2}),
        content: class
            .permits_content()
            .then(|| json!({"text": format!("{marker}-TEXT")})),
        crc_status: CrcStatus::Valid,
        identity: id.cloned(),
        content_class: class,
        t: t(sec),
    }
}

/// Stores a decode and resolves its identity sighting.
fn store(r: &mut Repository, d: &Decode, f: f64) -> Option<EmitterId> {
    r.insert_decode(d).unwrap();
    Sighting::decode(d, f, 25e3, None).map(|s| r.record_sighting(&s, None).unwrap().emitter_id)
}

/// Every public decode getter, error and inventory output under `access`, as text.
fn scan(
    r: &Repository,
    decodes: &[DecodeId],
    identities: &[DecodedIdentity],
    emitters: &[EmitterId],
    access: IdentityAccess,
) -> String {
    let mut s = String::new();
    for &id in decodes {
        let d = r.decode(id).unwrap();
        s += &format!("{d:?}\n{}\n", serde_json::to_string(&d).unwrap());
        s += &format!("{:?}\n", r.decode_with_access(id, access).unwrap());
    }
    for i in identities {
        let rows = r.decodes_for_identity(i).unwrap();
        s += &format!("{rows:?}\n{}\n", serde_json::to_string(&rows).unwrap());
        s += &format!(
            "{:?}\n",
            r.decodes_for_identity_with_access(i, access).unwrap()
        );
    }
    for &e in emitters {
        s += &serde_json::to_string(&r.emitter(e).unwrap()).unwrap();
        s += &format!("{:?}\n", r.emitter_with_access(e, access).unwrap());
        s += &format!("{:?}\n", r.identity_reclassifications(e, access).unwrap());
    }
    s += &format!(
        "{:?}\n",
        r.query_inventory(&InventoryQuery {
            access,
            ..Default::default()
        })
        .unwrap()
    );
    let missing = r.decode_with_access(DecodeId::new(), access).unwrap_err();
    s += &format!("{missing} {missing:?}\n");
    s
}

/// T-036 item 1: restricted, metadata-only, mixed and own-key decode rows never show their
/// identity, metadata or identifier-bearing labels without the class permitting it, on any
/// getter, error or inventory output; a lookup by a withheld identity finds nothing.
#[test]
fn decode_getters_gate_identity_metadata_and_labels() {
    use ContentClass::*;
    const PAGER: &str = "SENTINEL-PAGER-7391";
    const CELL: &str = "SENTINEL-CELL-4402";
    const META: &str = "SENTINEL-META-2208";
    const NOID: &str = "SENTINEL-NOID-6630";
    const MIX: &str = "SENTINEL-MIX-8120";
    const OWN: &str = "SENTINEL-OWN-5510";
    const OPEN: &str = "SENTINEL-OPEN-1000";
    let mut r = repo();
    let mut errors = String::new();

    let pager = identity("pocsag-capcode", PAGER);
    let cell = identity("imsi", CELL);
    let meta = identity("hk-framing", META);
    let mix = identity("sensor-x", MIX);
    let own = identity("own-sensor", OWN);
    let open = identity("rds-like", OPEN);
    let rows = [
        (decode(Some(&pager), PAGER, RestrictedPaging, 1), 929.6e6),
        (decode(Some(&cell), CELL, RestrictedCellular, 2), 1900e6),
        (decode(Some(&meta), META, MetadataOnly, 3), 915e6),
        (decode(None, NOID, RestrictedPaging, 4), 931e6),
        // The same identity from an unrestricted and a restricted source: withheld everywhere.
        (decode(Some(&mix), MIX, Unrestricted, 5), 433.9e6),
        (decode(Some(&mix), MIX, RestrictedPaging, 6), 433.9e6),
        (decode(Some(&own), OWN, OwnKeyDecrypted, 7), 868.3e6),
        (decode(Some(&open), OPEN, Unrestricted, 8), 98.1e6),
    ];
    let mut ids = Vec::new();
    let mut emitters = Vec::new();
    for (d, f) in &rows {
        ids.push(d.id);
        emitters.extend(store(&mut r, d, *f));
    }
    // Errors near restricted rows: content refused, reclassification refused.
    let mut gated = rows[0].0.clone();
    gated.id = DecodeId::new();
    gated.content = Some(json!({"text": PAGER}));
    let err = r.insert_decode(&gated).unwrap_err();
    errors += &format!("{err} {err:?}\n");
    let err = r
        .reclassify_identity(
            emitters[0],
            Unrestricted,
            IdentityAccess::OwnTrafficAuthorised,
            "mine",
            "user",
        )
        .unwrap_err();
    errors += &format!("{err} {err:?}\n");
    let identities = [
        pager.clone(),
        cell.clone(),
        meta.clone(),
        mix.clone(),
        own.clone(),
        open.clone(),
    ];

    // Positive control: the crate-private read holds every sentinel.
    let raw: String = ids
        .iter()
        .map(|&id| format!("{:?}", r.decode_ungated(id).unwrap()))
        .collect();
    for s in [PAGER, CELL, META, NOID, MIX, OWN, OPEN] {
        assert!(raw.contains(s), "{s} stored");
    }

    let standard = scan(&r, &ids, &identities, &emitters, IdentityAccess::Standard) + &errors;
    for s in [PAGER, CELL, META, NOID, MIX, OWN] {
        assert!(!standard.contains(s), "{s} leaked without authorisation");
    }
    // Unrestricted: identity, metadata and label in clear.
    assert!(standard.contains(&format!("model-{OPEN}")));
    let v = r
        .decode_with_access(rows[7].0.id, IdentityAccess::Standard)
        .unwrap();
    assert_eq!(v.decode, rows[7].0);
    assert!(!v.metadata_withheld && !v.labels_withheld);
    assert_eq!(
        r.decodes_for_identity(&open).unwrap(),
        vec![rows[7].0.clone()]
    );

    // Withheld rows: identity scheme and class shown, value, metadata and label withheld.
    let v = r
        .decode_with_access(rows[0].0.id, IdentityAccess::Standard)
        .unwrap();
    assert!(matches!(
        &v.identity,
        InventoryIdentity::Withheld { scheme, class: Some(RestrictedPaging) }
            if *scheme == pager.scheme
    ));
    assert_eq!(
        (v.decode.identity.as_ref(), &v.decode.metadata),
        (None, &json!({}))
    );
    assert!(v.metadata_withheld && v.labels_withheld);
    assert_eq!(v.decode.frame_model, WITHHELD_LABEL);
    assert_eq!(
        (v.decode.decoder_id.as_str(), v.decode.crc_status),
        ("hk-test", CrcStatus::Valid)
    );
    // The unrestricted row of a mixed identity is withheld too.
    let v = r
        .decode_with_access(rows[4].0.id, IdentityAccess::OwnTrafficAuthorised)
        .unwrap();
    assert!(v.decode.identity.is_none() && v.metadata_withheld && v.content_withheld);
    assert_eq!(v.decode.content, None);
    // A label naming only a metadata value is withheld.
    let v = r
        .decode_with_access(rows[3].0.id, IdentityAccess::Standard)
        .unwrap();
    assert_eq!(v.decode.frame_model, WITHHELD_LABEL);
    assert_eq!(v.identity, InventoryIdentity::None);
    for i in [&pager, &cell, &meta, &mix, &own] {
        for access in [
            IdentityAccess::Standard,
            IdentityAccess::OwnTrafficAuthorised,
        ] {
            if access == IdentityAccess::OwnTrafficAuthorised && i == &own {
                continue;
            }
            assert!(
                r.decodes_for_identity_with_access(i, access)
                    .unwrap()
                    .is_empty(),
                "a lookup confirms a withheld identity: {:?}",
                i.scheme
            );
        }
    }

    // Own traffic: only with the explicit authorisation; restricted rows stay withheld.
    let authorised = scan(
        &r,
        &ids,
        &identities,
        &emitters,
        IdentityAccess::OwnTrafficAuthorised,
    );
    for s in [PAGER, CELL, META, NOID, MIX] {
        assert!(!authorised.contains(s), "{s} leaked with authorisation");
    }
    let own_rows = r
        .decodes_for_identity_with_access(&own, IdentityAccess::OwnTrafficAuthorised)
        .unwrap();
    assert_eq!(own_rows.len(), 1);
    assert_eq!(own_rows[0].decode, rows[6].0);
    assert!(authorised.contains(&format!("{OWN}-TEXT")));
}

/// T-036 item 2: identity-bearing tags are refused on restricted sightings and never shown or
/// matched on rows whose identity is withheld; labels still flow, unrestricted rows keep tags.
#[test]
fn identity_bearing_tags_never_leave_with_a_withheld_identity() {
    use ContentClass::*;
    for (tag, free) in [
        ("pager", true),
        ("suspect-artifact", true),
        ("out-of-allocation", true),
        ("watch list/mine", true),
        ("ism433", false),
        ("capcode-1234567", false),
        ("cafe", false),
        ("a1b2c3", false),
        ("", false),
        ("tag\u{e9}", false),
    ] {
        assert_eq!(tag_is_identity_free(tag), free, "{tag:?}");
    }

    let mut r = repo();
    const CAPCODE: &str = "1234567";
    let pager = identity("pocsag-capcode", CAPCODE);
    let d = decode(Some(&pager), "pocsag", RestrictedPaging, 1);
    r.insert_decode(&d).unwrap();
    let mut s = Sighting::decode(&d, 929.6e6, 25e3, None).unwrap();
    // Producer tags derived from the restricted decode are refused, whole sighting included.
    for bad in [format!("capcode-{CAPCODE}"), "tg4242".into()] {
        s.tags = vec![bad];
        let err = r.record_sighting(&s, None).unwrap_err();
        assert!(matches!(err, RepoError::Invalid(_)), "{err:?}");
        assert!(!format!("{err} {err:?}").contains(CAPCODE));
    }
    let alpha = identity("alias", "zulu");
    let mut sa = s.clone();
    sa.source = LinkTarget::Decode(DecodeId::new());
    sa.identity = Some(IdentityClaim {
        identity: alpha,
        content_class: MetadataOnly,
    });
    sa.tags = vec!["call-zulu".into()];
    assert!(
        r.record_sighting(&sa, None).is_err(),
        "names the claim value"
    );
    s.tags = vec!["pager".into()];
    let pager_emitter = r.record_sighting(&s, None).unwrap().emitter_id;
    // insert_emitter identities are unclassified (withheld): labels only.
    let mut legacy = Emitter {
        id: EmitterId::new(),
        f_center_hz: 851e6,
        bandwidth_hz: 12.5e3,
        first_seen: t(0),
        last_seen: t(1),
        count: 1,
        fingerprint: serde_json::Value::Null,
        identity: Identity::Decoded(identity("talkgroup-x", "sys9:4242")),
        known_status: KnownStatus::Unknown,
        classifications: Vec::new(),
        tags: ["tg-sys9:4242".to_owned()].into(),
    };
    assert!(matches!(
        r.insert_emitter(&legacy).unwrap_err(),
        RepoError::Invalid(_)
    ));
    legacy.tags = ["trunk".to_owned()].into();
    r.insert_emitter(&legacy).unwrap();

    // Unrestricted rows keep every tag, digits included.
    let rds = identity("rds-like", "C0DE");
    let open = decode(Some(&rds), "rds", Unrestricted, 2);
    r.insert_decode(&open).unwrap();
    let mut so = Sighting::decode(&open, 98.1e6, 200e3, None).unwrap();
    so.tags = vec!["fm-98.1".into(), "t-1".into()];
    let open_emitter = r.record_sighting(&so, None).unwrap().emitter_id;
    let mut so2 = so.clone();
    so2.source = LinkTarget::Track(TrackId::new());
    so2.identity = None;
    so2.f_center_hz = 146e6;
    so2.fingerprint = Some(Fingerprint::new(146e6, 12.5e3));
    let anon = r.record_sighting(&so2, None).unwrap().emitter_id;

    // T-038: a free-text user tag on the restricted row is refused whatever it says (the refusal
    // depends on the class only, so it is no oracle). Tags already stored there (written before
    // the identity was restricted, or merged in) are never shown or matched.
    for tag in [CAPCODE, "t-1", "cafe", "zulu"] {
        let err = r.add_emitter_tag(pager_emitter, tag).unwrap_err();
        assert!(matches!(err, RepoError::Invalid(_)), "{err:?}");
        assert!(!format!("{err} {err:?}").contains(CAPCODE));
    }
    for tag in [CAPCODE, "t-1", "cafe"] {
        r.conn
            .execute(
                "INSERT INTO emitter_tag (emitter_id, tag) VALUES (?1, ?2)",
                params![super::blob(pager_emitter), tag],
            )
            .unwrap();
    }

    let mut out = String::new();
    for access in [
        IdentityAccess::Standard,
        IdentityAccess::OwnTrafficAuthorised,
    ] {
        let e = r.emitter_with_access(pager_emitter, access).unwrap();
        assert!(e.tags_withheld);
        assert_eq!(
            e.emitter.tags.iter().collect::<Vec<_>>(),
            vec!["pager"],
            "{access:?}"
        );
        out += &format!("{e:?}\n{:?}\n", r.emitter(pager_emitter).unwrap());
        for tag in [CAPCODE, "t-1", "cafe"] {
            let page = r
                .query_inventory(&InventoryQuery {
                    tag: Some(tag.into()),
                    access,
                    ..Default::default()
                })
                .unwrap();
            out += &format!("{page:?}\n");
            assert!(
                page.entries.iter().all(|e| e.emitter.id != pager_emitter),
                "tag filter {tag} matched a withheld row"
            );
        }
        let pagers = r
            .query_inventory(&InventoryQuery {
                tag: Some("pager".into()),
                access,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(pagers.entries.len(), 1);
        assert_eq!(pagers.entries[0].emitter.id, pager_emitter);
        out += &format!(
            "{:?}\n",
            r.query_inventory(&InventoryQuery {
                access,
                ..Default::default()
            })
            .unwrap()
        );
    }
    assert!(!out.contains(CAPCODE) && !out.contains("cafe"), "{out}");
    let e = r.emitter(open_emitter).unwrap();
    assert!(e.tags.contains("fm-98.1") && e.tags.contains("t-1"));
    assert!(
        !r.emitter_with_access(open_emitter, IdentityAccess::Standard)
            .unwrap()
            .tags_withheld
    );

    // A gated tag filter pages over the rows it may match: t-1 holds the open and anonymous rows.
    let mut seen = Vec::new();
    let mut offset = 0;
    loop {
        let page = r
            .query_inventory(&InventoryQuery {
                tag: Some("t-1".into()),
                limit: 1,
                offset,
                ..Default::default()
            })
            .unwrap();
        seen.extend(page.entries.iter().map(|e| e.emitter.id));
        match page.next_offset {
            Some(o) => offset = o,
            None => break,
        }
    }
    seen.sort();
    let mut want = vec![open_emitter, anon];
    want.sort();
    assert_eq!(seen, want);
}

/// T-038 item 3: a letters-only identity (no digits, no hex run: it passes the T-036 shape rule)
/// written as a tag never leaves a restricted emitter. `add_emitter_tag` and a producer sighting
/// landing on the row refuse it; a copy stored while the emitter had no identity is never shown
/// or matched by `query_inventory` or the emitter getters (sentinel scan).
#[test]
fn letters_only_identity_tags_never_leave_a_restricted_emitter() {
    const ALIAS: &str = "quokka";
    assert!(tag_is_identity_free(ALIAS) && !tag_in_vocabulary(ALIAS));
    assert!(TAG_VOCABULARY.windows(2).all(|w| w[0] < w[1]), "sorted");
    assert!(TAG_VOCABULARY.iter().all(|t| tag_in_vocabulary(t)));
    let mut r = repo();
    // An anonymous track emitter, tagged with free text while it has no identity.
    let track = Sighting {
        source: LinkTarget::Track(TrackId::new()),
        seen: TimeRange::instant(t(0)),
        count: 1,
        f_center_hz: 929.6e6,
        bandwidth_hz: 25e3,
        fingerprint: Some(Fingerprint::new(929.6e6, 25e3)),
        identity: None,
        context: None,
        classification: None,
        tags: vec![ALIAS.into(), "pager".into()],
    };
    let e = r.record_sighting(&track, None).unwrap().emitter_id;
    r.add_emitter_tag(e, &format!("{ALIAS}-mine")).unwrap();
    // A restricted decode puts the alias identity on it (a non-channel-sharing scheme names its
    // context emitter).
    let alias = DecodedIdentity {
        scheme: IdentityScheme::RdsPi,
        value: ALIAS.into(),
    };
    let d = decode(Some(&alias), "alias", ContentClass::RestrictedPaging, 5);
    r.insert_decode(&d).unwrap();
    let s = Sighting::decode(&d, 929.6e6, 25e3, Some(e)).unwrap();
    assert_eq!(r.record_sighting(&s, None).unwrap().emitter_id, e);

    // Writes: every free-text tag is refused on the restricted row, whatever it says.
    for tag in [ALIAS, "Quokka", "zulu", "call-sign", "t-1"] {
        let err = r.add_emitter_tag(e, tag).unwrap_err();
        assert!(matches!(err, RepoError::Invalid(_)), "{tag}: {err:?}");
    }
    let mut onto = track.clone();
    onto.source = LinkTarget::Track(TrackId::new());
    onto.fingerprint = None;
    onto.context = Some(e);
    onto.tags = vec![ALIAS.into()];
    let err = r.record_sighting(&onto, None).unwrap_err();
    assert!(matches!(err, RepoError::Invalid(_)), "{err:?}");
    onto.tags = vec!["watch".into()];
    assert_eq!(r.record_sighting(&onto, None).unwrap().emitter_id, e);
    r.add_emitter_tag(e, "interesting").unwrap();

    // Reads: vocabulary labels only, at every access level; filters by the alias never match.
    let mut out = String::new();
    for access in [
        IdentityAccess::Standard,
        IdentityAccess::OwnTrafficAuthorised,
    ] {
        let entry = r.emitter_with_access(e, access).unwrap();
        // T-040: the alias tags stored before the restriction were purged, so none are hidden.
        assert!(!entry.tags_withheld);
        assert_eq!(
            entry
                .emitter
                .tags
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["interesting", "pager", "watch"],
            "{access:?}"
        );
        out += &format!("{entry:?}\n");
        for tag in [
            None,
            Some(ALIAS.to_owned()),
            Some(format!("{ALIAS}-mine")),
            Some("pager".to_owned()),
        ] {
            let page = r
                .query_inventory(&InventoryQuery {
                    tag: tag.clone(),
                    access,
                    ..Default::default()
                })
                .unwrap();
            let hit = page.entries.iter().any(|x| x.emitter.id == e);
            assert_eq!(
                hit,
                !tag.as_deref().is_some_and(|t| t.contains(ALIAS)),
                "{tag:?}"
            );
            out += &format!("{page:?}\n");
        }
    }
    out += &format!("{:?}\n", r.emitter(e).unwrap());
    assert!(!out.to_lowercase().contains(ALIAS), "{out}");
}

fn reclass_count(r: &Repository) -> i64 {
    r.conn
        .query_row(
            "SELECT count(*) FROM identity_reclassification",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

/// T-036 item 3: only an explicit own-traffic authorisation opens a withheld identity, only to
/// own-key-decrypted or unrestricted, with an append-only audit row and no rewritten decode;
/// restricted-cellular and restricted-paging identities can never be opened.
#[test]
fn reclassify_identity_is_authorised_audited_and_never_opens_restricted() {
    use ContentClass::*;
    let auth = IdentityAccess::OwnTrafficAuthorised;
    let refused = |e: RepoError| {
        assert!(
            matches!(e, RepoError::ReclassificationRefused { .. }),
            "{e:?}"
        )
    };
    let mut r = repo();
    // The user's own sensor, framed by a metadata-only decoder (AWARE-036).
    let mine = identity("hk-framing", "0badc0de");
    let first = decode(Some(&mine), "0badc0de", MetadataOnly, 1);
    let e = store(&mut r, &first, 915e6).unwrap();
    let body_before: String = r
        .conn
        .query_row(
            "SELECT body FROM decode WHERE decode_id = ?1",
            [super::blob(first.id)],
            |row| row.get(0),
        )
        .unwrap();
    let withheld = |r: &Repository| {
        matches!(
            r.emitter_with_access(e, auth).unwrap().identity,
            InventoryIdentity::Withheld { .. }
        )
    };
    assert!(withheld(&r));

    // Refusals: no authorisation, a class that does not open, empty reason.
    refused(
        r.reclassify_identity(e, OwnKeyDecrypted, IdentityAccess::Standard, "mine", "dan")
            .unwrap_err(),
    );
    for bad in [MetadataOnly, RestrictedPaging, RestrictedCellular] {
        refused(
            r.reclassify_identity(e, bad, auth, "mine", "dan")
                .unwrap_err(),
        );
    }
    refused(
        r.reclassify_identity(e, OwnKeyDecrypted, auth, " ", "dan")
            .unwrap_err(),
    );
    assert_eq!(reclass_count(&r), 0);
    assert!(withheld(&r));

    // Authorised: opens to own-key-decrypted, audited.
    let rec = r
        .reclassify_identity(e, OwnKeyDecrypted, auth, "my own 915 MHz sensor", "dan")
        .unwrap();
    assert_eq!(
        (rec.emitter_id, rec.old_class, rec.new_class),
        (e, MetadataOnly, OwnKeyDecrypted)
    );
    assert!(matches!(
        r.emitter_with_access(e, auth).unwrap().identity,
        InventoryIdentity::Clear {
            class: OwnKeyDecrypted,
            ..
        }
    ));
    assert_eq!(
        r.emitter(e).unwrap().identity,
        Identity::Unknown,
        "standard"
    );
    let audit = r.identity_reclassifications(e, auth).unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(
        (audit[0].author.as_str(), audit[0].reason.as_deref()),
        ("dan", Some("my own 915 MHz sensor"))
    );
    assert_eq!(audit[0].scheme, mine.scheme);
    assert_eq!(
        r.identity_reclassifications(e, IdentityAccess::Standard)
            .unwrap()[0]
            .reason,
        None
    );
    // Stored decode untouched; its detail stays gated by its own class.
    let body_after: String = r
        .conn
        .query_row(
            "SELECT body FROM decode WHERE decode_id = ?1",
            [super::blob(first.id)],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(body_before, body_after);
    assert_eq!(r.decode_ungated(first.id).unwrap(), first);
    let views = r.decodes_for_identity_with_access(&mine, auth).unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].decode.identity.as_ref(), Some(&mine));
    assert!(views[0].metadata_withheld);
    assert!(r.decodes_for_identity(&mine).unwrap().is_empty());

    // The audit is append-only.
    for sql in [
        "UPDATE identity_reclassification SET new_class = 'unrestricted'",
        "DELETE FROM identity_reclassification",
    ] {
        assert!(r.conn.execute(sql, params![]).is_err(), "{sql}");
    }

    // The same metadata-only framer seen again keeps it open (most restrictive no longer
    // re-closes an opened identity); re-opening to the same class is refused; opening further
    // to unrestricted chains.
    store(&mut r, &decode(Some(&mine), "x", MetadataOnly, 2), 915e6);
    assert!(!withheld(&r));
    refused(
        r.reclassify_identity(e, OwnKeyDecrypted, auth, "again", "dan")
            .unwrap_err(),
    );
    r.reclassify_identity(e, Unrestricted, auth, "publish my sensor", "dan")
        .unwrap();
    store(&mut r, &decode(Some(&mine), "y", MetadataOnly, 3), 915e6);
    assert_eq!(
        r.emitter(e).unwrap().identity,
        Identity::Decoded(mine.clone()),
        "chained reclassification keeps metadata-only sources opened"
    );
    assert_eq!(r.decodes_for_identity(&mine).unwrap().len(), 3);

    // A restricted-paging source closes it again, and it can never be reopened.
    store(
        &mut r,
        &decode(Some(&mine), "z", RestrictedPaging, 4),
        915e6,
    );
    assert!(withheld(&r));
    assert!(
        r.decodes_for_identity_with_access(&mine, auth)
            .unwrap()
            .is_empty()
    );
    refused(
        r.reclassify_identity(e, Unrestricted, auth, "still mine", "dan")
            .unwrap_err(),
    );
    assert_eq!(reclass_count(&r), 2);

    // Restricted-cellular and restricted-paging identities can never be opened.
    for (class, value) in [
        (RestrictedCellular, "310150123456789"),
        (RestrictedPaging, "7654321"),
    ] {
        let id = identity("restricted", value);
        let em = store(&mut r, &decode(Some(&id), value, class, 5), 930e6).unwrap();
        for new in [OwnKeyDecrypted, Unrestricted] {
            let err = r
                .reclassify_identity(em, new, auth, "mine", "dan")
                .unwrap_err();
            assert!(!format!("{err} {err:?}").contains(value));
            refused(err);
        }
        assert!(matches!(
            r.emitter_with_access(em, auth).unwrap().identity,
            InventoryIdentity::Withheld { .. }
        ));
    }
    // An unclassified (legacy) identity and an anonymous emitter cannot be opened.
    let legacy = r
        .upsert_emitter_observation(&EmitterObservation {
            emitter_id: EmitterId::new(),
            seen: TimeRange::new(t(0), t(1)),
            count: 1,
            f_center_hz: 851e6,
            bandwidth_hz: 12.5e3,
            identity: Some(identity("talkgroup-x", "sys9:4242")),
        })
        .unwrap()
        .emitter_id;
    refused(
        r.reclassify_identity(legacy, OwnKeyDecrypted, auth, "mine", "dan")
            .unwrap_err(),
    );
    let anon = r
        .record_sighting(
            &Sighting {
                source: LinkTarget::Track(TrackId::new()),
                seen: TimeRange::new(t(0), t(1)),
                count: 1,
                f_center_hz: 146e6,
                bandwidth_hz: 12.5e3,
                fingerprint: None,
                identity: None,
                context: None,
                classification: None,
                tags: Vec::new(),
            },
            None,
        )
        .unwrap()
        .emitter_id;
    refused(
        r.reclassify_identity(anon, OwnKeyDecrypted, auth, "mine", "dan")
            .unwrap_err(),
    );
    assert_eq!(reclass_count(&r), 2);
}

/// Every stored `emitter_tag` row, raw (`hex(emitter_id):tag`), newline-separated.
fn raw_tags(r: &Repository) -> String {
    let mut stmt = r
        .conn
        .prepare("SELECT hex(emitter_id) || ':' || tag FROM emitter_tag ORDER BY 1")
        .unwrap();
    let rows: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    rows.join("\n")
}

/// Stored tags of one emitter row, raw.
fn raw_tags_of(r: &Repository, e: EmitterId) -> Vec<String> {
    let mut stmt = r
        .conn
        .prepare("SELECT tag FROM emitter_tag WHERE emitter_id = ?1 ORDER BY tag")
        .unwrap();
    stmt.query_map([super::blob(e)], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// An anonymous track emitter at `f` carrying `tags`.
fn track_emitter(r: &mut Repository, f: f64, tags: &[&str]) -> EmitterId {
    let s = Sighting {
        source: LinkTarget::Track(TrackId::new()),
        seen: TimeRange::instant(t(0)),
        count: 1,
        f_center_hz: f,
        bandwidth_hz: 12.5e3,
        fingerprint: Some(Fingerprint::new(f, 12.5e3)),
        identity: None,
        context: None,
        classification: None,
        tags: tags.iter().map(|&t| t.to_owned()).collect(),
    };
    r.record_sighting(&s, None).unwrap().emitter_id
}

/// T-040 item 1: `remove_emitter_tag` on a withheld row answers the same whether or not the
/// guessed hidden tag is stored (class-only refusal); vocabulary labels still remove normally.
#[test]
fn remove_emitter_tag_on_a_withheld_row_is_no_oracle() {
    use ContentClass::*;
    const HIDDEN: &str = "sentinel-remove-7391";
    let mut r = repo();
    let with = store(
        &mut r,
        &decode(
            Some(&identity("pocsag-capcode", "1111111")),
            "a",
            RestrictedPaging,
            1,
        ),
        929.6e6,
    )
    .unwrap();
    let without = store(
        &mut r,
        &decode(
            Some(&identity("pocsag-capcode", "2222222")),
            "b",
            RestrictedPaging,
            2,
        ),
        931.9e6,
    )
    .unwrap();
    // A hidden tag stored before T-040 (raw insert: every product path now refuses or purges it).
    r.conn
        .execute(
            "INSERT INTO emitter_tag (emitter_id, tag) VALUES (?1, ?2)",
            params![super::blob(with), HIDDEN],
        )
        .unwrap();
    let a = r.remove_emitter_tag(with, HIDDEN);
    let b = r.remove_emitter_tag(without, HIDDEN);
    assert!(matches!(a, Err(RepoError::Invalid(_))), "{a:?}");
    assert_eq!(format!("{a:?}"), format!("{b:?}"));
    let (a, b) = (a.unwrap_err(), b.unwrap_err());
    assert_eq!(format!("{a}"), format!("{b}"));
    assert!(!format!("{a} {a:?}").contains(HIDDEN));
    // Vocabulary labels are shown on the row anyway, so their removal answers normally.
    r.add_emitter_tag(with, "pager").unwrap();
    assert!(r.remove_emitter_tag(with, "pager").unwrap());
    assert!(!r.remove_emitter_tag(without, "pager").unwrap());
    // A row without an identity keeps the plain answer for free text.
    let open = track_emitter(&mut r, 146e6, &[HIDDEN]);
    assert!(r.remove_emitter_tag(open, HIDDEN).unwrap());
    assert!(!r.remove_emitter_tag(open, HIDDEN).unwrap());
}

/// T-040 item 2: merging into a restricted emitter stores no free-text tag on the survivor or on
/// the absorbed row; vocabulary labels are carried over.
#[test]
fn merge_into_a_restricted_emitter_stores_no_free_text_tag() {
    use ContentClass::*;
    const INTO: &str = "sentinel-merge-into-4402";
    const MOVED: &str = "sentinel-merge-moved-2208";
    let mut r = repo();
    // Survivor already restricted; the absorbed anonymous row carries free text.
    let pager = store(
        &mut r,
        &decode(
            Some(&identity("pocsag-capcode", "3333333")),
            "p",
            RestrictedPaging,
            1,
        ),
        929.6e6,
    )
    .unwrap();
    let anon = track_emitter(&mut r, 146e6, &[INTO, "watch"]);
    assert!(raw_tags(&r).contains(INTO), "positive control");
    r.merge_emitters(anon, pager, t(10), "same transmitter")
        .unwrap();
    assert_eq!(raw_tags_of(&r, pager), vec!["watch"]);
    // The identity moves in: the anonymous survivor becomes restricted and loses its free text.
    let survivor = track_emitter(&mut r, 433.9e6, &[MOVED, "interesting"]);
    let cell = store(
        &mut r,
        &decode(
            Some(&identity("imsi", "310150123456789")),
            "c",
            RestrictedCellular,
            2,
        ),
        1900e6,
    )
    .unwrap();
    assert!(raw_tags(&r).contains(MOVED), "positive control");
    let m = r
        .merge_emitters(cell, survivor, t(11), "same transmitter")
        .unwrap();
    assert!(m.identity_moved);
    assert_eq!(raw_tags_of(&r, survivor), vec!["interesting"]);
    let raw = raw_tags(&r);
    assert!(!raw.contains(INTO) && !raw.contains(MOVED), "{raw}");
}

/// T-040 item 3: a tag added (or removed) through a merged-away id acts on the live survivor.
#[test]
fn a_tag_added_through_a_merged_away_id_lands_on_the_live_row() {
    const TAG: &str = "sentinel-live-5510";
    let mut r = repo();
    let gone = track_emitter(&mut r, 146e6, &[]);
    let live = track_emitter(&mut r, 433.9e6, &[]);
    r.merge_emitters(gone, live, t(5), "same").unwrap();
    r.add_emitter_tag(gone, TAG).unwrap();
    assert_eq!(raw_tags_of(&r, live), vec![TAG]);
    assert!(raw_tags_of(&r, gone).is_empty());
    assert!(r.emitter(live).unwrap().tags.contains(TAG));
    assert!(r.remove_emitter_tag(gone, TAG).unwrap());
    assert!(raw_tags_of(&r, live).is_empty());
    // A merged-away id of a restricted survivor is refused by the survivor's class.
    let anon = track_emitter(&mut r, 162e6, &[]);
    let pager = store(
        &mut r,
        &decode(
            Some(&identity("pocsag-capcode", "4444444")),
            "p",
            ContentClass::RestrictedPaging,
            1,
        ),
        929.6e6,
    )
    .unwrap();
    r.merge_emitters(anon, pager, t(6), "same").unwrap();
    assert!(matches!(
        r.add_emitter_tag(anon, TAG),
        Err(RepoError::Invalid(_))
    ));
    r.add_emitter_tag(anon, "pager").unwrap();
    assert_eq!(raw_tags_of(&r, pager), vec!["pager"]);
    assert!(!raw_tags(&r).contains(TAG));
}

/// T-040 item 4: free-text tags stored before a class tightens are deleted in the tightening
/// write, on every path (identity arriving by sighting, a more restrictive source, a restrictive
/// source after an audited reclassification, the legacy observation upsert, a linked restricted
/// decode); a raw scan finds no sentinel afterwards.
#[test]
fn tags_are_purged_when_the_class_tightens() {
    use ContentClass::*;
    const SIGHTED: &str = "sentinel-sighted-8120";
    const ADDED: &str = "sentinel-added-6630";
    const OPENED: &str = "sentinel-opened-1000";
    const RECLASSED: &str = "sentinel-reclassed-7391";
    const LEGACY: &str = "sentinel-legacy-4402";
    const LINKED: &str = "sentinel-linked-2208";
    let all = [SIGHTED, ADDED, OPENED, RECLASSED, LEGACY, LINKED];
    let auth = IdentityAccess::OwnTrafficAuthorised;
    let mut r = repo();

    // (a) An anonymous emitter gets a restricted identity through a context sighting.
    let a = track_emitter(&mut r, 929.6e6, &[SIGHTED, "pager"]);
    r.add_emitter_tag(a, ADDED).unwrap();
    let alias = DecodedIdentity {
        scheme: IdentityScheme::RdsPi,
        value: "quokka".into(),
    };
    let d = decode(Some(&alias), "alias", RestrictedPaging, 1);
    r.insert_decode(&d).unwrap();
    let s = Sighting::decode(&d, 929.6e6, 25e3, Some(a)).unwrap();

    // (b) An unrestricted identity with free text, later closed by a restricted-paging source.
    let rds = identity("rds-like", "C0DE");
    let b = store(&mut r, &decode(Some(&rds), "rds", Unrestricted, 2), 98.1e6).unwrap();
    r.add_emitter_tag(b, OPENED).unwrap();

    // (c) A metadata-only identity opened by audited reclassification, tagged, then closed.
    let mine = identity("hk-framing", "0badc0de");
    let c = store(&mut r, &decode(Some(&mine), "m", MetadataOnly, 3), 915e6).unwrap();
    r.reclassify_identity(c, OwnKeyDecrypted, auth, "my sensor", "dan")
        .unwrap();
    r.add_emitter_tag(c, RECLASSED).unwrap();

    // (d) A legacy anonymous row later given an (unclassified) identity by the observation upsert.
    let obs = |identity: Option<DecodedIdentity>, id: EmitterId, f: f64| EmitterObservation {
        emitter_id: id,
        seen: TimeRange::new(t(0), t(1)),
        count: 1,
        f_center_hz: f,
        bandwidth_hz: 12.5e3,
        identity,
    };
    let dd = r
        .upsert_emitter_observation(&obs(None, EmitterId::new(), 851e6))
        .unwrap()
        .emitter_id;
    r.add_emitter_tag(dd, LEGACY).unwrap();

    // (e) A legacy identity row classed only by linked decodes: unrestricted, then restricted.
    let trunk = identity("talkgroup-x", "sys9:4242");
    let e = r
        .upsert_emitter_observation(&obs(Some(trunk.clone()), EmitterId::new(), 852e6))
        .unwrap()
        .emitter_id;
    let link = |r: &mut Repository, d: &Decode| {
        r.insert_decode(d).unwrap();
        r.link_emitter(&EmitterLink {
            emitter_id: e,
            target: LinkTarget::Decode(d.id),
            linked_at: d.t,
        })
        .unwrap();
    };
    link(&mut r, &decode(Some(&trunk), "open", Unrestricted, 4));
    r.add_emitter_tag(e, LINKED).unwrap();

    let before = raw_tags(&r);
    for sentinel in all {
        assert!(before.contains(sentinel), "positive control {sentinel}");
    }

    // Tighten every row.
    assert_eq!(r.record_sighting(&s, None).unwrap().emitter_id, a);
    store(
        &mut r,
        &decode(Some(&rds), "rds2", RestrictedPaging, 5),
        98.1e6,
    );
    store(
        &mut r,
        &decode(Some(&mine), "m2", RestrictedPaging, 6),
        915e6,
    );
    r.upsert_emitter_observation(&obs(Some(identity("talkgroup-y", "sys9:1")), dd, 851e6))
        .unwrap();
    link(&mut r, &decode(Some(&trunk), "closed", RestrictedPaging, 7));

    let after = raw_tags(&r);
    for sentinel in all {
        assert!(!after.contains(sentinel), "{sentinel} survived: {after}");
    }
    assert_eq!(raw_tags_of(&r, a), vec!["pager"], "vocabulary kept");
    for id in [a, b, c, dd, e] {
        let entry = r.emitter_with_access(id, auth).unwrap();
        assert!(
            matches!(entry.identity, InventoryIdentity::Withheld { .. }),
            "{id}"
        );
        assert!(!entry.tags_withheld, "nothing left to hide on {id}");
    }
}
