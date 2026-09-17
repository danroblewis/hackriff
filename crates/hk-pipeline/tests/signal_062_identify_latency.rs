//! T-186 (SIGNAL-062): detection → identification latency of a strong continuous station, through
//! the mock SDR over the real HackRF FM capture (`fm_100p8M_2p4M_l32g30a1_t1p5_5s`, 101.3 MHz),
//! replayed blind.
//!
//! **What is measured, on the sample clock.** A recording inventory wraps the default
//! [`TrackInventory`] and, every time a chain hands an emitter to the inventory (the call that
//! ranks its explanations and is what `/api/inventory` then serves), snapshots the emitter's top
//! explanation and the sample time of the newest evidence it holds (its classifications and last
//! sighting). The latency of the first status-backed `fm-broadcast` explanation is that evidence
//! time minus the start of the station's first detection. Both come from the recording's clock, so
//! the bound does not depend on how fast the machine runs.
//!
//! **A-priori bound: [`IDENTIFY_BOUND_S`] = 3 s.** Before T-186 the only write was the analog
//! chain's full 4 s window (and with no RDS PI decoded, none at all).
//!
//! **Evidence floor.** A brief copy of the same station (0.9 s, shorter than the probe plus the
//! leading refine window) must not be identified early: no early identification, and no
//! status-backed explanation or WFM mode classification without a CRC-valid decoded identity.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use hk_core::{MockEnd, Pacing};
use hk_detect::TrackEvent;
use hk_detect::TrackSummary;
use hk_model::{
    EmitterId, FreqRange, IdentityScheme, InventoryIdentity, InventoryQuery, Region, RepoError,
    Repository, TimeRange, Timestamp, TrackId,
};
use hk_pipeline::family::MIN_CONFIDENCE;
use hk_pipeline::{
    Inventory, Pipeline, PipelineConfig, RunSummary, TrackInventory, open_mock_replay, replay_plan,
};
use serde_json::json;

const SIGNAL_062: &str = "SIGNAL-062";
const NAME: &str = "fm_100p8M_2p4M_l32g30a1_t1p5_5s";
/// A-priori bound, sample-clock seconds from the station's first detection to a status-backed
/// family + top explanation.
const IDENTIFY_BOUND_S: f64 = 3.0;
/// The brief copy's length, s: shorter than the analog probe (0.5 s) plus the leading refine
/// window it must complete (1 s) before any early identification.
const BRIEF_S: f64 = 0.9;

#[derive(Clone, Debug)]
struct Snap {
    emitter: EmitterId,
    evidence_ns: i64,
    top: Option<String>,
    status_backed: bool,
    wfm_classified: bool,
    pi: bool,
}

struct Recording {
    inner: TrackInventory,
    snaps: Arc<Mutex<Vec<Snap>>>,
}

impl Recording {
    fn snap(&self, repo: &mut Repository, emitter: EmitterId) -> Result<(), RepoError> {
        let id = repo.live_emitter_id(emitter)?;
        let e = repo.emitter(id)?;
        let x = hk_pipeline::explanations(repo, id)?;
        let evidence_ns = e
            .classifications
            .iter()
            .map(|c| c.t.as_unix_nanos())
            .chain(std::iter::once(e.last_seen.as_unix_nanos()))
            .max()
            .unwrap_or(i64::MIN);
        self.snaps.lock().unwrap().push(Snap {
            emitter: id,
            evidence_ns,
            top: x.first().map(|x| x.service.clone()),
            status_backed: x
                .first()
                .is_some_and(|x| x.status_evidence_confidence >= MIN_CONFIDENCE),
            wfm_classified: e.classifications.iter().any(|c| c.family == "wfm"),
            pi: matches!(&e.identity, hk_model::Identity::Decoded(d) if d.scheme == IdentityScheme::RdsPi),
        });
        Ok(())
    }
}

impl Inventory for Recording {
    fn capture_name(&mut self, name: &str) {
        self.inner.capture_name(name);
    }
    fn track_event(&mut self, repo: &mut Repository, event: &TrackEvent) -> Result<(), RepoError> {
        self.inner.track_event(repo, event)
    }
    fn chain_emitter(
        &mut self,
        repo: &mut Repository,
        track: Option<TrackId>,
        emitter: EmitterId,
    ) -> Result<(), RepoError> {
        self.inner.chain_emitter(repo, track, emitter)?;
        self.snap(repo, emitter)
    }
    fn live_track(
        &mut self,
        repo: &mut Repository,
        summary: &TrackSummary,
    ) -> Result<(), RepoError> {
        self.inner.live_track(repo, summary)
    }

    fn emitter_of_track(&self, track: hk_model::TrackId) -> Option<EmitterId> {
        self.inner.emitter_of_track(track)
    }
}

/// Replays `meta` blind through the mock SDR (unpaced, lossless) with the recording inventory.
fn run_mock(dir: &std::path::Path, meta: &std::path::Path) -> (RunSummary, Vec<Snap>) {
    let input = TempDir::new("t186-blind");
    let blind = blind_meta(meta, &input.0);
    let dev = open_mock_replay(&blind, Pacing::Unpaced, MockEnd::Stop).unwrap();
    let info = dev.info;
    let mut plan = replay_plan(info.center_hz, info.sample_rate_hz, info.start_time);
    plan.extra = json!({});
    let mut cfg = PipelineConfig::new(dir, plan).unwrap();
    cfg.source_class = dev.class;
    cfg.lossless = true;
    let snaps = Arc::new(Mutex::new(Vec::new()));
    let inventory = Recording {
        inner: TrackInventory::default(),
        snaps: Arc::clone(&snaps),
    };
    let handle =
        Pipeline::start(cfg, Box::new(dev.source), info, None, Box::new(inventory)).unwrap();
    let s = handle.wait().unwrap();
    eprintln!("{}", s.to_text());
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    let snaps = snaps.lock().unwrap().clone();
    (s, snaps)
}

fn station() -> FreqRange {
    FreqRange::centered(101.3e6, 150e3)
}

fn first_detection_ns(repo: &Repository) -> i64 {
    let ever = TimeRange::new(
        Timestamp::from_unix_nanos(0),
        Timestamp::from_unix_nanos(i64::MAX / 2),
    );
    repo.detections_in_region(&Region::new(station(), ever))
        .unwrap()
        .iter()
        .map(|d| d.time.start.as_unix_nanos())
        .min()
        .unwrap_or_else(|| panic!("[{SIGNAL_062}] 101.3 MHz not detected"))
}

#[test]
fn signal_062_strong_continuous_station_identified_within_3s_of_first_detection() {
    let Some(meta) = real_fixture(NAME) else {
        return;
    };
    let dir = TempDir::new("t186-strong");
    let (s, snaps) = run_mock(&dir.0, &meta);
    let repo = repo(&dir.0);
    let t_det = first_detection_ns(&repo);
    for x in &snaps {
        eprintln!(
            "[{SIGNAL_062}] inventory hand-off: evidence +{:.3} s, top {:?}, status-backed {}, wfm {}, pi {}",
            (x.evidence_ns - t_det) as f64 / 1e9,
            x.top,
            x.status_backed,
            x.wfm_classified,
            x.pi
        );
    }
    let first = snaps
        .iter()
        .find(|x| x.top.as_deref() == Some("fm-broadcast") && x.status_backed && x.wfm_classified)
        .unwrap_or_else(|| {
            panic!("[{SIGNAL_062}] never identified as fm-broadcast from WFM evidence: {snaps:?}")
        });
    let latency_s = (first.evidence_ns - t_det) as f64 / 1e9;
    eprintln!("[{SIGNAL_062}] identified {latency_s:.3} s (sample clock) after first detection");
    assert!(
        latency_s <= IDENTIFY_BOUND_S,
        "[{SIGNAL_062}] family + top explanation {latency_s:.3} s after first detection \
         (bound {IDENTIFY_BOUND_S} s)"
    );
    assert!(s.counter("/chains/identifications") >= 1);

    // One inventory entry: the early emitter is the one that later carries the RDS PI.
    let entries = inventory(
        &repo,
        InventoryQuery {
            identity_scheme: Some(IdentityScheme::RdsPi),
            ..InventoryQuery::default()
        },
    );
    let pi = entries
        .iter()
        .find(|e| {
            matches!(&e.identity, InventoryIdentity::Clear { identity, .. } if identity.value == "1694")
        })
        .unwrap_or_else(|| panic!("[{SIGNAL_062}] no PI 1694 entry: {entries:?}"));
    assert_eq!(
        repo.live_emitter_id(first.emitter).unwrap(),
        pi.emitter.id,
        "[{SIGNAL_062}] the early identification and the RDS write are one entry"
    );
}

#[test]
fn signal_062_brief_station_is_not_identified_before_its_evidence_window() {
    let Some(meta) = real_fixture(NAME) else {
        return;
    };
    // A 0.9 s copy of the capture (metadata unchanged but the checksum).
    let src = TempDir::new("t186-brief-src");
    let mut m: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&meta).unwrap()).unwrap();
    if let Some(g) = m["global"].as_object_mut() {
        g.remove("core:sha512");
    }
    let fs = m["global"]["core:sample_rate"].as_f64().unwrap();
    let brief_meta = src.0.join("brief.sigmf-meta");
    std::fs::write(&brief_meta, serde_json::to_string_pretty(&m).unwrap()).unwrap();
    let data = std::fs::read(meta.with_extension("sigmf-data")).unwrap();
    let n = 2 * (BRIEF_S * fs) as usize;
    std::fs::write(src.0.join("brief.sigmf-data"), &data[..n.min(data.len())]).unwrap();

    let dir = TempDir::new("t186-brief");
    let (s, snaps) = run_mock(&dir.0, &brief_meta);
    assert_eq!(
        s.counter("/chains/identifications"),
        0,
        "[{SIGNAL_062}] identified from less than the leading window"
    );
    for x in &snaps {
        assert!(
            x.pi || !(x.status_backed || x.wfm_classified),
            "[{SIGNAL_062}] brief station identified without a decoded identity: {x:?}"
        );
    }
    let repo = repo(&dir.0);
    for e in inventory(&repo, InventoryQuery::default()) {
        let decoded = matches!(e.identity, InventoryIdentity::Clear { .. });
        let x = hk_pipeline::explanations(&repo, e.emitter.id).unwrap();
        let status_backed = x
            .first()
            .is_some_and(|x| x.status_evidence_confidence >= MIN_CONFIDENCE);
        let wfm = e.emitter.classifications.iter().any(|c| c.family == "wfm");
        assert!(
            decoded || !(status_backed || wfm),
            "[{SIGNAL_062}] brief entry {} identified without a decoded identity: {x:?}",
            e.emitter.id
        );
    }
}
