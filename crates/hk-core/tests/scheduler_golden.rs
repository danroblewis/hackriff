//! T-009 golden schedules: fixed ScanPlans on a synthetic clock must emit exactly the checked-in
//! tune sequence (`tests/golden/*.json`). Regenerate only deliberately: `HK_UPDATE_GOLDEN=1
//! cargo test -p hk-core --test scheduler_golden`, then review the diff.

mod sched_common;

use hk_core::scheduler::{PlanWarning, Poi, Purpose};
use hk_model::ScanPolicy;
use sched_common::*;
use serde_json::{Value, json};

/// SPACE-050 (natural radio noise-floor survey): HF, the FM band, and two S-band spans that
/// straddle the HackRF RF-path switches (2170 / 2740 MHz); sweep-only with per-band gains.
#[test]
fn space_050_sweep_only_noise_floor_survey_matches_golden() {
    let p = plan(
        "SPACE-050 noise-floor survey",
        1,
        vec![
            region(3.0, 30.0, 2.0, Some(10.0)),
            region(88.0, 108.0, 1.0, Some(10.0)),
            region(2155.0, 2185.0, 1.0, Some(10.0)),
            region(2725.0, 2760.0, 1.0, Some(10.0)),
        ],
        ScanPolicy::SweepOnly,
        vec![
            gain(1.0, 30.0, 16.0, 30.0, false, None),
            gain(2000.0, 3000.0, 32.0, 30.0, true, None),
        ],
        Value::Null,
    );
    let mut s = hackrf(&p);
    let hops = s.plan().hops.len();
    let steps = run(&mut s, 2 * hops + 3);
    assert!(steps.iter().all(|st| st.purpose.is_discovery()));
    // The S-band spans are cut at both path switches: paths 0, 1 and 2 all appear.
    for path in 0..=2u8 {
        assert!(steps.iter().any(|st| st.rf_path == path), "rf path {path}");
    }
    check_golden("space_050_sweep_only", &s, &steps);
}

/// AWARE-042: ISM-band discovery with POI dwells at 3:1, a verified POI (gain step A/B × 3,
/// ±1 MHz retune, rate change) and a bursty POI sized to its burst interval.
#[test]
fn aware_042_sweep_dwell_verification_matches_golden() {
    let p = plan(
        "AWARE-042 ISM activity",
        1,
        vec![
            region(902.0, 928.0, 2.0, None),
            region(430.0, 440.0, 1.0, None),
        ],
        ScanPolicy::SweepThenDwell,
        vec![],
        json!({ "scheduler": { "sweeps_per_cycle": 3, "dwells_per_cycle": 1, "rate_change": true } }),
    );
    let mut s = hackrf(&p);
    s.offer_poi(Poi {
        key: 1,
        center_hz: 915.0e6,
        bandwidth_hz: 500e3,
        interestingness: 0.9,
        burst_interval_ns: None,
        verify: true,
    })
    .unwrap();
    s.offer_poi(Poi {
        key: 2,
        center_hz: 433.92e6,
        bandwidth_hz: 200e3,
        interestingness: 0.5,
        burst_interval_ns: Some(S),
        verify: false,
    })
    .unwrap();
    let steps = run(&mut s, 40);
    assert!(steps.iter().any(|st| st.purpose.is_trust_test()));
    assert!(
        steps
            .iter()
            .any(|st| st.purpose == Purpose::Dwell { poi: 2 } && st.duration_ns == 3 * S)
    );
    check_golden("aware_042_sweep_dwell", &s, &steps);
}

/// Overlapping regions merge, a dwell-only region contributes long windows, and an accessory
/// (filter-port) band is flagged pending T-028.
#[test]
fn overlap_dwell_only_and_accessory_band_matches_golden() {
    let p = plan(
        "overlap + dwell-only + accessory",
        1,
        vec![
            region(144.0, 148.0, 1.0, None),
            region(146.0, 160.0, 3.0, Some(5.0)),
            region(420.0, 450.0, 1.0, None),
        ],
        ScanPolicy::SweepThenDwell,
        vec![gain(144.0, 150.0, 24.0, 20.0, false, Some("filter-2m"))],
        json!({ "scheduler": { "region_policy": [null, null, "dwell-only"] } }),
    );
    let mut s = hackrf(&p);
    let warnings = &s.plan().warnings;
    assert!(
        warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::AccessoryTrustPending { entry: 0, .. }))
    );
    assert!(
        warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::RevisitUnachievable { region: 1, .. }))
    );
    let steps = run(&mut s, 12);
    assert!(
        steps
            .iter()
            .any(|st| matches!(st.purpose, Purpose::RegionDwell { .. }))
    );
    assert!(steps.iter().any(|st| st.accessory));
    check_golden("overlap_dwell_only_accessory", &s, &steps);
}
