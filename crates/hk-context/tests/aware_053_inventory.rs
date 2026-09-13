//! AWARE-053 (T-018 × T-019): when entity resolution creates an emitter or its family changes,
//! the band-plan prior (`match_known_status`) appends the status: an FM-broadcast-shaped emitter
//! at 120 MHz (aeronautical allocation) becomes `unexpected-here`; the same family inside the FM
//! broadcast band becomes `known`; repeat sightings with an unchanged family append nothing.

use hk_context::{BandTable, Region as BandRegion, match_known_status};
use hk_model::{
    Classification, Fingerprint, InventoryQuery, KnownStatus, LinkTarget, PriorVerdict, Repository,
    Sighting, StatusAuthor, TimeRange, Timestamp, TrackId,
};

fn t(sec: i64) -> Timestamp {
    Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + sec * 1_000_000_000)
}

fn classified_track(f: f64, bw: f64, family: &str, at: i64) -> Sighting {
    let seen = TimeRange::new(t(at), t(at + 60));
    Sighting {
        source: LinkTarget::Track(TrackId::new()),
        seen,
        count: 5,
        f_center_hz: f,
        bandwidth_hz: bw,
        fingerprint: Some(Fingerprint::new(f, bw)),
        identity: None,
        context: None,
        classification: Some(Classification {
            t: seen.end,
            family: family.into(),
            confidence: 0.9,
            open_set_score: 0.1,
            model_version: "am-fm-rules@1".into(),
        }),
        tags: Vec::new(),
    }
}

#[test]
fn aware_053_unexpected_family_off_allocation_gets_status_via_priors() {
    let table = BandTable::bundled(BandRegion::Us).unwrap();
    let prior = |family: &str, f: f64, bw: f64| {
        let m = match_known_status(&table, family, f, bw);
        PriorVerdict {
            status: m.status,
            prior_ref: m.prior_ref,
            reason: m.reason,
        }
    };
    let mut repo = Repository::open_in_memory().unwrap();

    let off = repo
        .record_sighting(
            &classified_track(120e6, 150e3, "fm-broadcast", 0),
            Some(&prior),
        )
        .unwrap();
    assert!(off.created);
    assert_eq!(off.status_appended, Some(KnownStatus::UnexpectedHere));
    let history = repo.known_status_history(off.emitter_id).unwrap();
    assert_eq!(history.len(), 2, "{history:?}");
    assert_eq!(
        (history[0].status, history[0].author),
        (KnownStatus::Unknown, StatusAuthor::Clusterer)
    );
    assert_eq!(
        (history[1].status, history[1].author),
        (KnownStatus::UnexpectedHere, StatusAuthor::Prior)
    );
    assert!(history[1].prior_ref.is_some());

    // Same emitter, same family: no new status entry.
    let again = repo
        .record_sighting(
            &classified_track(120.001e6, 150e3, "fm-broadcast", 3600),
            Some(&prior),
        )
        .unwrap();
    assert_eq!(
        (again.emitter_id, again.status_appended),
        (off.emitter_id, None)
    );
    assert_eq!(repo.known_status_history(off.emitter_id).unwrap().len(), 2);

    let on = repo
        .record_sighting(
            &classified_track(98.1e6, 150e3, "fm-broadcast", 0),
            Some(&prior),
        )
        .unwrap();
    assert_eq!(on.status_appended, Some(KnownStatus::Known));

    let unexpected = repo
        .query_inventory(&InventoryQuery {
            status: vec![KnownStatus::UnexpectedHere],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(unexpected.entries.len(), 1);
    assert_eq!(unexpected.entries[0].emitter.id, off.emitter_id);
}
