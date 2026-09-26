//! T-991: a window's content class is computed per sub-band, at the resolution of its
//! allocations — never taken whole-window from one sub-band.
//!
//! The explorer (journal 2026-09-25, window 3) tuned 157.4 MHz at 10 Msps — a 152.4–162.4 MHz
//! window spanning maritime VHF, AIS and NOAA Weather Radio — and read the whole window as
//! `restricted-paging`, because four 20 kHz Part 22 paging channels lie inside it. The paging
//! class belongs to those channels only.

use hk_model::ContentClass;
use hk_pipeline::class::{class_map, classify_emitter, window_class, window_class_map};

const CENTER: f64 = 157.0e6;
const FS: f64 = 10.0e6; // 152.0–162.0 MHz

fn class_at(map: &[hk_pipeline::class::SubBandClass], f: f64) -> ContentClass {
    map.iter()
        .find(|b| f >= b.lo_hz && f < b.hi_hz)
        .unwrap_or_else(|| panic!("{f} Hz not covered by {map:?}"))
        .content_class
}

#[test]
fn a_152_to_162_mhz_window_restricts_only_its_paging_sub_bands() {
    let map = window_class_map(CENTER, FS);
    // The map tiles the window exactly, in order, with no gap or overlap.
    assert_eq!(map.first().unwrap().lo_hz, 152.0e6);
    assert_eq!(map.last().unwrap().hi_hz, 162.0e6);
    for w in map.windows(2) {
        assert_eq!(w[0].hi_hz, w[1].lo_hz, "{map:?}");
    }

    // Marine VHF (ch 16 at 156.8 MHz, the coast side), AIS 1/2: unrestricted.
    for f in [156.8e6, 156.05e6, 161.975e6] {
        assert_eq!(
            class_at(&map, f),
            ContentClass::Unrestricted,
            "{f} Hz: {map:?}"
        );
    }
    // The four 47 CFR 22.531 high-VHF paging channels: restricted-paging, and only there.
    for f in [152.24e6, 152.84e6, 158.10e6, 158.70e6] {
        assert_eq!(
            class_at(&map, f),
            ContentClass::RestrictedPaging,
            "{f} Hz: {map:?}"
        );
    }
    let paging_hz: f64 = map
        .iter()
        .filter(|b| b.content_class == ContentClass::RestrictedPaging)
        .map(|b| b.hi_hz - b.lo_hz)
        .sum();
    assert!(
        (paging_hz - 4.0 * 20e3).abs() < 1.0,
        "paging covers only its 4 × 20 kHz channels, got {paging_hz} Hz: {map:?}"
    );
    // Land mobile between the two marine halves has no prior: fail closed, not unrestricted.
    assert_eq!(class_at(&map, 159.5e6), ContentClass::MetadataOnly);

    // The whole-window summary is not the paging class.
    assert_ne!(window_class(CENTER, FS), ContentClass::RestrictedPaging);
    assert_eq!(window_class(CENTER, FS), ContentClass::MetadataOnly);
}

#[test]
fn noaa_weather_radio_is_its_own_unrestricted_sub_band() {
    let map = class_map(162.0e6, 163.0e6);
    assert_eq!(
        class_at(&map, 162.55e6),
        ContentClass::Unrestricted,
        "{map:?}"
    );
    assert_eq!(
        class_at(&map, 162.7e6),
        ContentClass::MetadataOnly,
        "{map:?}"
    );
    // A window wholly inside the NOAA channels is unrestricted as a whole.
    assert_eq!(window_class(162.475e6, 0.15e6), ContentClass::Unrestricted);
}

#[test]
fn an_emitter_is_classed_by_its_own_sub_band_not_the_window() {
    let source = window_class(CENTER, FS);
    // AIS 2 (162.025 MHz ± 12.5 kHz) inside the mixed window: unrestricted by its prior.
    let ais = classify_emitter(&[], source, 162.0125e6, 162.0375e6);
    assert_eq!(ais.map(|(c, _)| c), Some(ContentClass::Unrestricted));
    // A paging channel inside it: restricted-paging.
    let pager = classify_emitter(&[], source, 152.235e6, 152.245e6);
    assert_eq!(pager.map(|(c, _)| c), Some(ContentClass::RestrictedPaging));
    // Land mobile, unvouched: still fail closed.
    assert_eq!(classify_emitter(&[], source, 159.49e6, 159.51e6), None);
}

#[test]
fn a_window_wholly_in_restricted_bands_keeps_its_restricted_class() {
    // 929.3–931.7 MHz: Part 90 / Part 24 / Part 22 paging end to end.
    assert_eq!(window_class(930.5e6, 2.4e6), ContentClass::RestrictedPaging);
    // 870–890 MHz: cellular block A/B downlink.
    assert_eq!(
        window_class(880.0e6, 20e6),
        ContentClass::RestrictedCellular
    );
    // FM broadcast stays unrestricted.
    assert_eq!(window_class(100.8e6, 2.4e6), ContentClass::Unrestricted);
}
