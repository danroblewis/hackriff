//! SIGNAL-062 (T3): FM broadcast with RDS, from the real HackRF LFS fixture
//! `fm_100p8M_2p4M_l32g30a1_t1p5_5s` (2.4 Msps at 100.8 MHz; the 101.3 MHz station carries RDS
//! PI 1694). Nothing about the mode is configured: the detection → track → registry path picks the
//! analog-auto chain, whose classifier must select WFM, find the 19 kHz pilot, decode the RDS PI
//! and write it as the Emitter's identity and label. The run is shared with AWARE-053's
//! known-allocation check ([`fm_run`]).

use std::sync::OnceLock;

use hk_model::{
    AnnotationKind, AnnotationTarget, ContentClass, DecodedIdentity, FreqRange, IdentityScheme,
    InventoryIdentity, InventoryQuery, LinkTarget, Region,
};
use serde_json::json;

use crate::common::*;

const SIGNAL_062: &str = "SIGNAL-062";
pub const FM_FIXTURE: &str = "fm_100p8M_2p4M_l32g30a1_t1p5_5s";
pub const STATION_HZ: f64 = 101.3e6;

/// One replay of the FM fixture, shared by the tests that need it.
pub struct FmRun {
    pub dir: TempDir,
    pub summary: hk_pipeline::RunSummary,
}

/// The shared FM run; `None` when the fixture is not fetched (skip).
pub fn fm_run() -> Option<&'static FmRun> {
    static RUN: OnceLock<Option<FmRun>> = OnceLock::new();
    RUN.get_or_init(|| {
        let meta = real_fixture(FM_FIXTURE)?;
        let dir = TempDir::new("fm");
        // Blind: the fixture's annotations (station, spur, DC labels) are stripped.
        let (cfg, replay, _input) =
            blind_replay_config(&dir.0, &meta, json!({}), hk_core::Pacing::Unpaced);
        assert_eq!(
            replay.class,
            ContentClass::Unrestricted,
            "[{SIGNAL_062}] FM band prior makes the source unrestricted"
        );
        assert!(
            cfg.settings.chains.is_none(),
            "[{SIGNAL_062}] built-in registry, no manual chain or mode"
        );
        let summary = finish(start(cfg, replay));
        Some(FmRun { dir, summary })
    })
    .as_ref()
}

#[test]
fn signal_062_fm_rds_auto_wfm_pilot_pi_label() {
    let Some(run) = fm_run() else { return };
    let s = &run.summary;
    assert_eq!(s.always_on_lost_samples, 0);
    let repo = repo(&run.dir.0);
    let station = FreqRange::centered(STATION_HZ, 150e3);
    let hits = repo
        .detections_in_region(&Region::new(station, ever()))
        .unwrap();
    assert!(!hits.is_empty(), "[{SIGNAL_062}] 101.3 MHz not detected");
    assert!(
        s.counter("/chains/attached") >= 1,
        "[{SIGNAL_062}] no chain"
    );

    // RDS PI decoded (unrestricted: the default getter shows it) and CRC/sync-checked groups.
    let pi = DecodedIdentity {
        scheme: IdentityScheme::RdsPi,
        value: "1694".into(),
    };
    let decodes = repo.decodes_for_identity(&pi).unwrap();
    assert!(
        !decodes.is_empty(),
        "[{SIGNAL_062}] RDS PI 1694 not decoded"
    );
    assert!(
        decodes
            .iter()
            .all(|d| d.content_class == ContentClass::Unrestricted)
    );

    // The emitter: visible identity via query_inventory, WFM demodulation chosen automatically,
    // pilot found, label annotation.
    let entries = inventory(
        &repo,
        InventoryQuery {
            identity_scheme: Some(IdentityScheme::RdsPi),
            ..InventoryQuery::default()
        },
    );
    let entry = entries
        .iter()
        .find(|e| {
            matches!(&e.identity, InventoryIdentity::Clear { identity, class }
                if *identity == pi && *class == ContentClass::Unrestricted)
        })
        .unwrap_or_else(|| panic!("[{SIGNAL_062}] no visible PI 1694 emitter: {entries:?}"));
    let eid = entry.emitter.id;
    assert!(
        (entry.emitter.f_center_hz - STATION_HZ).abs() < 50e3,
        "[{SIGNAL_062}] emitter at {} Hz",
        entry.emitter.f_center_hz
    );
    let demods: Vec<_> = repo
        .emitter_links(eid)
        .unwrap()
        .into_iter()
        .filter_map(|l| match l.target {
            LinkTarget::Demodulation(id) => Some(repo.demodulation(id).unwrap()),
            _ => None,
        })
        .collect();
    assert!(!demods.is_empty(), "[{SIGNAL_062}] no linked Demodulation");
    for d in &demods {
        eprintln!(
            "[{SIGNAL_062}] demodulation mode {} lock {:?} params {}",
            d.mode,
            d.lock_quality,
            serde_json::to_string(&d.params).unwrap()
        );
        assert_eq!(d.mode, "wfm", "[{SIGNAL_062}] auto mode must select WFM");
    }
    // 19 kHz pilot, found blind (T-037b): every WFM Demodulation in the whole inventory (no
    // frequency or identity filter) with a stored pilot frequency (`EstimatedParams::pilot_hz`).
    // Only then are they matched against the truth station the test holds.
    let mut piloted = Vec::new();
    for e in inventory(&repo, InventoryQuery::default()) {
        for l in repo.emitter_links(e.emitter.id).unwrap() {
            if let LinkTarget::Demodulation(id) = l.target {
                let d = repo.demodulation(id).unwrap();
                if let (true, Some(p)) = (d.mode == "wfm", d.params.pilot_hz) {
                    piloted.push((e.emitter.f_center_hz, p));
                }
            }
        }
    }
    eprintln!("[{SIGNAL_062}] WFM demodulations with a pilot (emitter Hz, pilot Hz): {piloted:?}");
    assert!(
        !piloted.is_empty(),
        "[{SIGNAL_062}] no WFM demodulation stored its pilot frequency"
    );
    for (f, p) in &piloted {
        assert!(
            (p - 19_000.0).abs() <= 10.0,
            "[{SIGNAL_062}] pilot at {p} Hz on the WFM emitter at {f} Hz"
        );
    }
    assert!(
        piloted.iter().any(|(f, _)| (f - STATION_HZ).abs() < 50e3),
        "[{SIGNAL_062}] the piloted WFM emitter found blind is the truth station"
    );
    // The pilot PLL's lock quality is stored too (`Demodulation.lock_quality`, `None` without a
    // pilot). RDS decoding also needs the 57 kHz subcarrier locked to 3 × pilot.
    let pilot_lock = demods
        .iter()
        .filter_map(|d| d.lock_quality)
        .fold(f64::NAN, f64::max);
    assert!(
        pilot_lock >= 0.5,
        "[{SIGNAL_062}] no 19 kHz pilot lock on the WFM demodulation (lock quality {pilot_lock})"
    );
    assert_eq!(
        entry.family.as_deref(),
        Some("wfm"),
        "[{SIGNAL_062}] family from the auto classifier"
    );
    let labels: Vec<_> = repo
        .annotations_for(&AnnotationTarget::Emitter(eid))
        .unwrap()
        .into_iter()
        .filter(|a| a.kind == AnnotationKind::Label)
        .collect();
    assert!(!labels.is_empty(), "[{SIGNAL_062}] no Emitter label");
    assert_eq!(
        labels[0].metadata["pi"],
        json!("1694"),
        "[{SIGNAL_062}] label is keyed by the decoded PI"
    );
    eprintln!(
        "[{SIGNAL_062}] emitter {eid}: PI 1694 in clear, {} RDS decodes, pilot lock {pilot_lock:.3}, \
         label {:?}, {} detections",
        decodes.len(),
        labels[0].value,
        hits.len()
    );
}
