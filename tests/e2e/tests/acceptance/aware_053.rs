//! AWARE-053 (T4): known-signal priors separate known from unexpected.
//!
//! - **Known allocation.** The FM fixture's pipeline run (shared with SIGNAL-062): a detection of
//!   the 101.3 MHz station → the bundled 47 CFR 2.106 band table returns the FM broadcast row and
//!   `match_known_status` says `known` with that row's `prior_ref`.
//! - **Off allocation.** The same IQ relabelled 19.2 MHz up (stations at 118.8–121.2 MHz, inside
//!   the aeronautical allocation) through the pipeline: detections → tracks → `TrackInventory`
//!   emitters. A broadcast-FM classification of such an emitter appends `unexpected-here` with a
//!   `prior_ref`, visible through `query_inventory`'s status filter.
//!
//! **Classifier stand-in.** The composed pipeline has no C15 family classifier for tracks yet
//! (closed-track sightings carry no classification, so the prior can only say `unknown`; the WFM
//! chain's family `wfm` has no service mapping and its writer passes no prior). The test records
//! the classification a C15 stage would emit, with the exact prior closure `TrackInventory` uses,
//! onto the pipeline-created emitter. The pipeline's own statuses are printed for the report.

use hk_context::{BandTable, Region as BandRegion, match_known_status};
use hk_model::sigmf::SigmfMeta;
use hk_model::{
    Classification, EmitterId, Fingerprint, FreqRange, InventoryQuery, KnownStatus, LinkTarget,
    PriorVerdict, Region, Repository, Sighting, StatusAuthor, TimeRange, TrackId,
};
use serde_json::json;

use crate::common::*;
use crate::signal_062::{FM_FIXTURE, STATION_HZ, fm_run};

const AWARE_053: &str = "AWARE-053";
const SHIFT_HZ: f64 = 19.2e6;

fn table() -> BandTable {
    BandTable::bundled(BandRegion::Us).unwrap()
}

/// A C15-style classified sighting of an emitter at `f`/`bw`, through the band-plan prior exactly
/// as `hk_pipeline::TrackInventory` wires it.
fn classify(
    repo: &mut Repository,
    f: f64,
    bw: f64,
    family: &str,
    seen: TimeRange,
) -> hk_model::Resolution {
    let table = table();
    let prior = |family: &str, f: f64, bw: f64| {
        let m = match_known_status(&table, family, f, bw);
        PriorVerdict {
            status: m.status,
            prior_ref: m.prior_ref,
            reason: m.reason,
        }
    };
    let sighting = Sighting {
        source: LinkTarget::Track(TrackId::new()),
        seen,
        count: 1,
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
            model_version: "acceptance-c15-stand-in@1".into(),
        }),
        tags: Vec::new(),
    };
    repo.record_sighting(&sighting, Some(&prior)).unwrap()
}

fn print_statuses(
    repo: &Repository,
    band: FreqRange,
    tag: &str,
) -> Vec<(EmitterId, f64, f64, TimeRange)> {
    let mut out = Vec::new();
    for e in inventory(
        repo,
        InventoryQuery {
            freq: Some(band),
            ..InventoryQuery::default()
        },
    ) {
        let h = repo.known_status_history(e.emitter.id).unwrap();
        eprintln!(
            "[{AWARE_053}] {tag}: pipeline emitter {:.4} MHz bw {:.0} Hz family {:?} statuses {:?}",
            e.emitter.f_center_hz / 1e6,
            e.emitter.bandwidth_hz,
            e.family,
            h.iter()
                .map(|c| (c.status, c.author, c.prior_ref.clone(), c.reason.clone()))
                .collect::<Vec<_>>()
        );
        out.push((
            e.emitter.id,
            e.emitter.f_center_hz,
            e.emitter.bandwidth_hz,
            TimeRange::new(e.emitter.first_seen, e.emitter.last_seen),
        ));
    }
    out
}

fn copy_db(from: &std::path::Path, to: &std::path::Path) {
    for name in ["hackriff.db", "hackriff.db-wal", "hackriff.db-shm"] {
        let src = from.join(name);
        if src.is_file() {
            std::fs::copy(&src, to.join(name)).unwrap();
        }
    }
}

#[test]
fn aware_053_detection_at_known_frequency_matches_the_fm_broadcast_allocation() {
    let Some(run) = fm_run() else { return };
    let table = table();
    let dets = repo(&run.dir.0)
        .detections_in_region(&Region::new(FreqRange::centered(STATION_HZ, 150e3), ever()))
        .unwrap();
    let d = dets
        .iter()
        .max_by(|a, b| a.snr_peak_db.total_cmp(&b.snr_peak_db))
        .unwrap_or_else(|| panic!("[{AWARE_053}] no detection at 101.3 MHz"));
    let rows = table.overlapping_center(d.f_center_hz, d.obw_hz);
    let fm = rows
        .iter()
        .find(|r| r.has_tag("fm-broadcast"))
        .unwrap_or_else(|| panic!("[{AWARE_053}] no FM broadcast row at {} Hz", d.f_center_hz));
    let m = match_known_status(&table, "fm-broadcast", d.f_center_hz, d.obw_hz);
    eprintln!(
        "[{AWARE_053}] detection {:.4} MHz obw {:.0} Hz → row {} ({:?}): {:?}",
        d.f_center_hz / 1e6,
        d.obw_hz,
        fm.id,
        fm.primary_services,
        m
    );
    assert_eq!(m.status, KnownStatus::Known, "[{AWARE_053}]");
    assert_eq!(m.prior_ref, Some(fm.prior_ref()), "[{AWARE_053}]");

    // On a copy of the run's database (the SIGNAL-062 test reads the original).
    let scratch = TempDir::new("a053k");
    copy_db(&run.dir.0, &scratch.0);
    let mut repo = repo(&scratch.0);
    let pipeline = print_statuses(&repo, FreqRange::centered(STATION_HZ, 200e3), "FM band");
    assert!(
        !pipeline.is_empty(),
        "[{AWARE_053}] no pipeline emitter at 101.3 MHz"
    );
    let (_, f, bw, seen) = pipeline[0];
    let r = classify(&mut repo, f, bw, "fm-broadcast", seen);
    assert_eq!(
        r.status_appended,
        Some(KnownStatus::Known),
        "[{AWARE_053}] {r:?}"
    );
    let last = repo
        .known_status_history(r.emitter_id)
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(last.author, StatusAuthor::Prior);
    assert_eq!(last.prior_ref, Some(fm.prior_ref()));
}

#[test]
fn aware_053_off_allocation_emitter_gets_unexpected_here_with_prior_ref() {
    let Some(meta_path) = real_fixture(FM_FIXTURE) else {
        return;
    };
    // The FM IQ, relabelled 19.2 MHz up: identical samples, aeronautical-band frequencies.
    let src = TempDir::new("a053src");
    let mut meta = SigmfMeta::read(&meta_path).unwrap();
    for cap in &mut meta.captures {
        cap.frequency = cap.frequency.map(|f| f + SHIFT_HZ);
        if let Some(p) = &mut cap.provenance {
            p.tune.center_hz += SHIFT_HZ;
        }
    }
    if let Some(p) = &mut meta.global.provenance {
        p.tune.center_hz += SHIFT_HZ;
    }
    meta.annotations.clear();
    let retuned = src.0.join("retuned.sigmf-meta");
    meta.write(&retuned).unwrap();
    std::os::unix::fs::symlink(
        meta_path.with_extension("sigmf-data"),
        src.0.join("retuned.sigmf-data"),
    )
    .unwrap();

    let dir = TempDir::new("a053u");
    let (cfg, replay) = replay_config(&dir.0, &retuned, json!({}), hk_core::Pacing::Unpaced);
    let s = finish(start(cfg, replay));
    assert_eq!(
        s.source_class, "metadata-only",
        "[{AWARE_053}] 120 MHz is no unrestricted band prior"
    );
    let station = STATION_HZ + SHIFT_HZ;
    let mut repo = repo(&dir.0);
    let band = FreqRange::centered(station, 200e3);
    let dets = repo
        .detections_in_region(&Region::new(band, ever()))
        .unwrap();
    assert!(
        !dets.is_empty(),
        "[{AWARE_053}] no detection at {station} Hz"
    );
    let table = table();
    assert!(
        table
            .overlapping_center(station, 150e3)
            .iter()
            .all(|r| !r.has_tag("fm-broadcast")),
        "[{AWARE_053}] 121.1 MHz must not be an FM broadcast allocation"
    );
    let pipeline = print_statuses(&repo, band, "aeronautical band");
    assert!(
        !pipeline.is_empty(),
        "[{AWARE_053}] the pipeline created no emitter at {station} Hz"
    );
    let (eid, f, bw, seen) = pipeline[0];
    let r = classify(&mut repo, f, bw, "fm-broadcast", seen);
    eprintln!("[{AWARE_053}] classified sighting → {r:?}");
    assert!(
        pipeline.iter().any(|p| p.0 == r.emitter_id) && !r.created,
        "[{AWARE_053}] the classification lands on the pipeline-created emitter {eid}"
    );
    assert_eq!(
        r.status_appended,
        Some(KnownStatus::UnexpectedHere),
        "[{AWARE_053}] off-allocation broadcast FM"
    );
    let last = repo
        .known_status_history(r.emitter_id)
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(last.author, StatusAuthor::Prior);
    let prior_ref = last
        .prior_ref
        .clone()
        .unwrap_or_else(|| panic!("[{AWARE_053}] unexpected-here without a prior_ref"));
    eprintln!(
        "[{AWARE_053}] unexpected-here: {prior_ref} ({})",
        last.reason
    );
    let listed = inventory(
        &repo,
        InventoryQuery {
            status: vec![KnownStatus::UnexpectedHere],
            ..InventoryQuery::default()
        },
    );
    assert!(
        listed.iter().any(|e| e.emitter.id == r.emitter_id),
        "[{AWARE_053}] query_inventory status filter"
    );
}
