//! SIGNAL-062 (T3): FM broadcast with RDS, from the real HackRF LFS fixture
//! `fm_100p8M_2p4M_l32g30a1_t1p5_5s` (2.4 Msps at 100.8 MHz, served by the mock SDR). Nothing about
//! the mode is configured: the detection → track → registry path picks the analog-auto chain,
//! whose classifier must select WFM, find the 19 kHz pilot, decode the RDS PI and write it as the
//! Emitter's identity and label. The run is shared ([`fm_run`]).
//!
//! **Blind (T-047).** The test never names a frequency or an identity to look up. It matches the
//! run's detections and whole inventory against the fixture's private truth list (the station's
//! extent), and only then compares what the system decoded on the matched emitter (PI, pilot) with
//! the truth values.

use std::sync::OnceLock;

use hk_e2e::blind::matching;
use hk_e2e::{Fixture, TruthItem};
use hk_model::{
    AnnotationKind, AnnotationTarget, ContentClass, IdentityScheme, InventoryIdentity,
    InventoryQuery, LinkTarget,
};

use crate::blind::{BlindRun, BlindSource, assert_truth_found, blind_replay, center_tol_hz};
use crate::common::*;

const SIGNAL_062: &str = "SIGNAL-062";
pub const FM_FIXTURE: &str = "fm_100p8M_2p4M_l32g30a1_t1p5_5s";

/// One blind run of the FM fixture, shared by the tests that need it.
pub struct FmRun {
    pub dir: TempDir,
    pub summary: hk_pipeline::RunSummary,
    /// The private truth (read only by the asserting test).
    pub fx: Fixture,
}

/// The shared FM run; `None` when the fixture is not fetched (skip).
pub fn fm_run() -> Option<&'static FmRun> {
    static RUN: OnceLock<Option<FmRun>> = OnceLock::new();
    RUN.get_or_init(|| {
        let (meta, fx) = crate::blind::private_truth(FM_FIXTURE)?;
        // The fixture's truth is stripped and sealed; the built-in registry runs with no plan
        // extra, chain or mode chosen by the test.
        let BlindRun { dir, summary, .. } = blind_replay(&meta, "fm", BlindSource::default());
        assert_eq!(
            summary.source_class, "unrestricted",
            "[{SIGNAL_062}] FM band prior makes the source unrestricted"
        );
        Some(FmRun { dir, summary, fx })
    })
    .as_ref()
}

/// The broadcast station of the private truth list.
fn station(fx: &Fixture) -> TruthItem {
    fx.of_kind("wfm-broadcast")
        .first()
        .map(|t| (*t).clone())
        .unwrap_or_else(|| panic!("[{SIGNAL_062}] fixture truth has no wfm-broadcast station"))
}

#[test]
fn signal_062_fm_rds_auto_wfm_pilot_pi_label() {
    let Some(run) = fm_run() else { return };
    let s = &run.summary;
    assert_eq!(s.always_on_lost_samples, 0);
    assert!(
        s.counter("/chains/attached") >= 1,
        "[{SIGNAL_062}] no chain"
    );
    // Every truth emission detected, FM broadcast among the top-k explanations.
    assert_truth_found(SIGNAL_062, &run.dir.0, &run.fx, 0.0, true);

    let truth = station(&run.fx);
    let tol = center_tol_hz(&truth);
    let repo = repo(&run.dir.0);
    let all = inventory(&repo, InventoryQuery::default());
    let at_station = matching(
        &truth,
        0.0,
        &all,
        |e| (e.emitter.f_center_hz, e.emitter.bandwidth_hz),
        tol,
    );
    eprintln!(
        "[{SIGNAL_062}] {} of {} inventory emitters match the private truth station",
        at_station.len(),
        all.len()
    );

    // The matched emitter decoded an RDS PI itself, visible in clear, equal to the truth's PI.
    let (entry, pi) = at_station
        .iter()
        .find_map(|e| match &e.identity {
            InventoryIdentity::Clear { identity, class }
                if identity.scheme == IdentityScheme::RdsPi =>
            {
                assert_eq!(*class, ContentClass::Unrestricted, "[{SIGNAL_062}]");
                Some((*e, identity.clone()))
            }
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!(
                "[{SIGNAL_062}] no emitter at the truth station decoded an RDS PI: {at_station:?}"
            )
        });
    let truth_pi = truth.str("/rds/pi_hex").expect("truth RDS PI");
    assert!(
        pi.value.eq_ignore_ascii_case(truth_pi),
        "[{SIGNAL_062}] decoded PI {} vs truth {truth_pi}",
        pi.value
    );
    let decodes = repo.decodes_for_identity(&pi).unwrap();
    assert!(
        !decodes.is_empty(),
        "[{SIGNAL_062}] no RDS decodes for PI {}",
        pi.value
    );
    assert!(
        decodes
            .iter()
            .all(|d| d.content_class == ContentClass::Unrestricted)
    );

    // WFM demodulation chosen automatically on the matched emitter.
    let eid = entry.emitter.id;
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
    // 19 kHz pilot, found blind (T-037b): every WFM Demodulation in the whole inventory with a
    // stored pilot frequency, then matched against the truth station and its truth pilot.
    let truth_pilot = truth
        .f64("/pilot/frequency_hz")
        .expect("truth pilot frequency");
    let mut piloted = Vec::new();
    for e in &all {
        for l in repo.emitter_links(e.emitter.id).unwrap() {
            if let LinkTarget::Demodulation(id) = l.target {
                let d = repo.demodulation(id).unwrap();
                if let (true, Some(p)) = (d.mode == "wfm", d.params.pilot_hz) {
                    piloted.push((e.emitter.f_center_hz, e.emitter.bandwidth_hz, p));
                }
            }
        }
    }
    eprintln!(
        "[{SIGNAL_062}] WFM demodulations with a pilot (emitter Hz, bw Hz, pilot Hz): {piloted:?}"
    );
    assert!(
        !piloted.is_empty(),
        "[{SIGNAL_062}] no WFM demodulation stored its pilot frequency"
    );
    for (f, _, p) in &piloted {
        assert!(
            (p - truth_pilot).abs() <= 10.0,
            "[{SIGNAL_062}] pilot at {p} Hz on the WFM emitter at {f} Hz (truth {truth_pilot})"
        );
    }
    assert!(
        piloted
            .iter()
            .any(|(f, bw, _)| hk_e2e::blind::matches_truth(&truth, 0.0, *f, *bw, tol)),
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
    // The RDS decoder's label (other labels, such as the family map's explanations, may sit on the
    // same emitter in any order once its track and decoder entries are one, T-082).
    let labels: Vec<_> = repo
        .annotations_for(&AnnotationTarget::Emitter(eid))
        .unwrap()
        .into_iter()
        .filter(|a| a.kind == AnnotationKind::Label && a.metadata.get("pi").is_some())
        .collect();
    assert!(!labels.is_empty(), "[{SIGNAL_062}] no Emitter label");
    assert_eq!(
        labels[0].metadata["pi"].as_str(),
        Some(pi.value.as_str()),
        "[{SIGNAL_062}] label is keyed by the decoded PI"
    );
    eprintln!(
        "[{SIGNAL_062}] emitter {eid}: PI {} in clear, {} RDS decodes, pilot lock {pilot_lock:.3}, \
         label {:?}",
        pi.value,
        decodes.len(),
        labels[0].value,
    );
}
