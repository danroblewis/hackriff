//! T-970: every FM broadcast station in the real captures is classified **wfm**, blind.
//!
//! The explorer's 2026-09-25 window measured the defect this asserts against: stations with a
//! locked 19 kHz stereo pilot and a CRC-valid RDS decode came back `unknown` at confidence 0.999
//! ("100 % unk" in the UI), so the always-on RDS chain — which attaches only to rows the app
//! classes WFM — never ran on them. On the code before this test, all six truth stations below
//! classify `unknown` at 0.999.
//!
//! **Blind.** The stations are found from the measured spectrum alone (local maxima over the
//! capture's own floor, each grown to the valley between it and its neighbours, with the
//! receiver's DC artefact excluded); the fixture's `hackriff:truth` is opened only in the
//! assertions, to check that every annotated station was both found and classified.

mod fmsupport;

use fmsupport::*;

/// A truth station the fixture's own analysis measured **below** this in-band SNR was attenuated
/// by the receiver's analogue passband rather than by the air — the 2026-09-15 capture's README
/// records exactly that for its 99.700 MHz station, which sits outside the 1.75 MHz baseband
/// filter at a 100.8 MHz centre. Abstaining there is never counted as wrong (the rule
/// `ota_fixtures.rs` already applies); being given a *different* family still is.
const MIN_ASSERTED_SNR_DB: f64 = 8.0;

#[test]
fn every_truth_station_in_every_fm_fixture_classifies_as_wfm() {
    let fixtures = fm_fixtures();
    if fixtures.is_empty() {
        return;
    }
    let mut asserted = 0;
    let mut exempt = 0;
    let mut failures: Vec<String> = Vec::new();
    for fx in &fixtures {
        let found = stations(fx);
        for st in &found {
            eprintln!(
                "[T-970] {}: {:+.1} kHz, {:.0} kHz wide -> {} ({:.3}) class {:?} open-set {:.3} snr {:?} reasons {:?}",
                fx.name,
                st.offset_hz / 1e3,
                st.bandwidth_hz / 1e3,
                st.class.family,
                st.class.confidence,
                st.class.class.as_ref().map(|c| c.label.clone()),
                st.class.open_set_score,
                st.class
                    .provenance
                    .snr_db
                    .map(|s| (s * 10.0).round() / 10.0),
                st.class.reasons,
            );
        }
        for truth in truth_stations(fx) {
            let required = truth.snr_db.is_none_or(|s| s >= MIN_ASSERTED_SNR_DB);
            let station = station_on(fx, &found, truth.center_hz);
            fn label(s: &Station) -> (&str, Option<&str>) {
                (
                    s.class.family.as_str(),
                    s.class.class.as_ref().map(|c| c.label.as_str()),
                )
            }
            match (required, station) {
                (true, None) => failures.push(format!(
                    "{}: no blind detection on the {:.3} MHz station",
                    fx.name,
                    truth.center_hz / 1e6
                )),
                (true, Some(st)) => {
                    asserted += 1;
                    if label(st) != ("analog", Some("wfm")) {
                        failures.push(format!(
                            "{} {:.3} MHz: {} ({:.3}) class {:?}, want analog/wfm; reasons {:?}",
                            fx.name,
                            truth.center_hz / 1e6,
                            st.class.family,
                            st.class.confidence,
                            st.class.class.as_ref().map(|c| c.label.clone()),
                            st.class.reasons,
                        ));
                    }
                    // Whatever a station is called, a call is never certain and an abstention is
                    // never near-certain (T-970's second half).
                    if st.class.family == hk_model::classify::UNKNOWN {
                        assert!(
                            st.class.confidence <= hk_model::classify::MAX_UNKNOWN_CONFIDENCE,
                            "{} {:.3} MHz: unknown at {:.3}",
                            fx.name,
                            truth.center_hz / 1e6,
                            st.class.confidence
                        );
                    }
                }
                // Below the floor: abstention allowed, another family never.
                (false, st) => {
                    exempt += 1;
                    if let Some(st) = st {
                        let (family, _) = label(st);
                        assert!(
                            family == "analog" || family == hk_model::classify::UNKNOWN,
                            "{} {:.3} MHz at {:?} dB: {family}",
                            fx.name,
                            truth.center_hz / 1e6,
                            truth.snr_db
                        );
                    }
                }
            }
        }
    }
    eprintln!("[T-970] {asserted} truth stations asserted, {exempt} below the passband floor");
    assert!(
        asserted >= 5,
        "only {asserted} truth stations asserted: the property would be weak"
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The negative control the rule has to survive: a **station-shaped window** centred on an
/// emission the fixture's own analysis measured to have *no* 19 kHz line.
///
/// The 2026-09-15 capture's 100.4653 MHz emission is harmonic 43 of a free-running oscillator
/// (T-317); its truth records "no 19 kHz line, so this is not a stereo FM broadcast". Handing the
/// rule a 150 kHz box there — the same shape a station's box has — is the sharpest test that the
/// rule reads the multiplex and not the width. The receiver's own DC offset and its 100 MHz spur
/// are put through the same window for the same reason.
#[test]
fn a_station_shaped_window_with_no_pilot_is_never_called_wfm() {
    let fixtures = fm_fixtures();
    if fixtures.is_empty() {
        return;
    }
    let mut checked = 0;
    for fx in &fixtures {
        for t in non_station_truth(fx) {
            let Some(c) = classify_window(fx, t.center_hz, 150e3) else {
                continue;
            };
            checked += 1;
            eprintln!(
                "[T-970] {} negative control {} at {:.4} MHz -> {} ({:.3}) class {:?} reasons {:?}",
                fx.name,
                t.kind,
                t.center_hz / 1e6,
                c.family,
                c.confidence,
                c.class.as_ref().map(|k| k.label.clone()),
                c.reasons,
            );
            assert!(
                c.class.as_ref().is_none_or(|k| k.label != "wfm"),
                "{} {} at {:.4} MHz was called wfm",
                fx.name,
                t.kind,
                t.center_hz / 1e6
            );
            assert!(
                c.reasons.iter().all(|r| !r.starts_with("wfm_pilot")),
                "{} {} at {:.4} MHz fired the pilot rule: {:?}",
                fx.name,
                t.kind,
                t.center_hz / 1e6,
                c.reasons
            );
        }
    }
    assert!(checked >= 3, "only {checked} negative controls ran");
}
