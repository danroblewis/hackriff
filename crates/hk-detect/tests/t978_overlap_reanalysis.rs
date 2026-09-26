//! **T-978: overlapping boxes over one emission are re-analysed against the spectrum.**
//!
//! The explorer's live HackRF window of 2026-09-25 (05:52–06:23, San Francisco) served two
//! candidates for one P25 emission — 852.8586 MHz at 9.3 kHz and 852.8591 MHz at 26.2 kHz — and, at
//! 861.4346 MHz, one 557 kHz box beside the 20–80 kHz fragments it covered. CLAUDE.md is explicit
//! that this cannot stand: *"overlapping Confirmed/Candidate boxes are proof the analysis is wrong
//! … the system detects the overlap and automatically re-analyzes that region to resolve it to the
//! real signal(s)"*.
//!
//! T-369 detects it. What it could not do is resolve it, because everything it reasons with is the
//! rows: `hk_model::relate::modes` merges the bands *of those rows*, which for a region whose boxes
//! overlap by construction is one mode, and the verdict then falls to `distinguishing_evidence`,
//! whose bandwidth-ratio guard reads 26.2/9.3 = 2.8 as two emissions. `red_proof_*` below pins both
//! halves of that on the explorer's own numbers.
//!
//! This asserts the measuring half: `hk_detect::overlap::measure_region` over the integrated
//! spectrum, and `hk_model::region_verdicts` over what it measured.

use hk_detect::overlap::{OverlapConfig, measure_region};
use hk_detect::rules::Geometry;
use hk_detect::{EdgeRule, IntegratedSnapshot};
use hk_model::relate::{
    REGION_MERGE_UNCOVERED, REGION_OFF_CENTRE, REGION_TOO_COARSE, RegionVerdict, bands_compete,
    distinguishing_evidence, region_verdicts,
};
use hk_model::{EmitterId, FreqRange, RegionMeasurement, RowEvidence, Tolerances};

/// A spectrum of `bins` cells of `bin_hz` about `center_hz`, all at the floor, with the named
/// emissions added as raised plateaus of `snr_db` over it.
fn spectrum(
    center_hz: f64,
    bin_hz: f64,
    bins: usize,
    emissions: &[(f64, f64, f64)],
) -> IntegratedSnapshot {
    let fs = bin_hz * bins as f64;
    let geometry = Geometry::new(center_hz, fs, bins, 0.0, &EdgeRule::default());
    let floor = 1e-9_f64;
    let mut mean_psd = vec![floor; bins];
    for &(f_center, width, snr_db) in emissions {
        let lo = geometry.bin_at_or_above(f_center - width / 2.0);
        let hi = geometry.bin_at_or_above(f_center + width / 2.0).max(lo + 1);
        for p in mean_psd.iter_mut().take(hi.min(bins)).skip(lo) {
            *p = floor * 10f64.powf(snr_db / 10.0);
        }
    }
    IntegratedSnapshot {
        geometry,
        span_s: 1.0,
        mean_psd: mean_psd.clone(),
        mean_floor: vec![floor; bins],
        block_psd: vec![mean_psd],
    }
}

/// A live inventory row at `f_center` of `width`, seen over the whole run.
fn row(f_center_hz: f64, width_hz: f64, snr_db: f64) -> RowEvidence {
    RowEvidence {
        emitter_id: EmitterId::new(),
        f_center_hz,
        bandwidth_hz: width_hz,
        xdb_bandwidth_hz: None,
        confirmed: false,
        snr_db: Some(snr_db),
        peak_dbfs: None,
        duty_cycle: None,
        suspect_fraction: 0.0,
        image_flagged: false,
        imd_flagged: false,
        spur_flagged: false,
        identity: None,
        fingerprint: None,
        tuned_lo: Vec::new(),
        spans: vec![(0, 10_000_000_000)],
        count: 8,
        first_seen_ns: 0,
    }
}

fn verdicts(rows: &[&RowEvidence], m: &RegionMeasurement) -> Vec<(EmitterId, RegionVerdict)> {
    region_verdicts(rows, m, &Tolerances::default())
}

/// **The defect, on the explorer's own numbers.** Neither rule the inventory had could resolve the
/// P25 pair: they never compete (the wider band overlaps by 35 %, under the 60 % both-bands gate
/// `bands_compete` needs), and the guard that decides stage 4 calls them two emissions because
/// their recorded widths differ by 2.8x. Both boxes are therefore served, for ever.
#[test]
fn red_proof_the_p25_pair_is_unresolvable_from_the_rows() {
    let narrow = row(852_858_600.0, 9_300.0, 18.0);
    let wide = row(852_859_100.0, 26_200.0, 20.0);
    assert!(
        !bands_compete(narrow.freq(), wide.freq()),
        "the 9.3 kHz box inside the 26.2 kHz one never competes, so stages 1-3 never see it"
    );
    assert_eq!(
        distinguishing_evidence(&wide, &narrow, &Tolerances::default()),
        Some("bandwidth ratio beyond tolerance"),
        "and the guard stage 4 defers to calls two cuts of one emission two emissions"
    );
}

/// The same pair, resolved by the spectrum: the region measures **one** emission, so the box that
/// reads it is shown and the other defers.
#[test]
fn narrow_and_wide_over_one_emission_resolve_to_one() {
    // 2.4 Msps / 2048 bins = 1.17 kHz cells: 8 cells across the 9.3 kHz box.
    let snap = spectrum(
        852_860_000.0,
        1_171.875,
        2048,
        &[(852_859_100.0, 26_200.0, 20.0)],
    );
    let region = FreqRange::new(852_846_000.0, 852_872_200.0);
    let m = measure_region(region, &snap, &OverlapConfig::default())
        .expect("the region is inside the tuned span");
    assert_eq!(
        m.emissions.len(),
        1,
        "the spectrum shows one emission, however many boxes were cut out of it: {:?}",
        m.emissions
    );
    let e = m.emissions[0];
    assert!(
        (e.center_hz - 852_859_100.0).abs() < 2.0 * m.resolution_hz,
        "re-estimated centre {:.1} Hz",
        e.center_hz
    );
    assert!(
        (e.obw_hz - 26_200.0).abs() < 4.0 * m.resolution_hz,
        "re-estimated OBW {:.1} Hz",
        e.obw_hz
    );

    let narrow = row(852_858_600.0, 9_300.0, 18.0);
    let wide = row(852_859_100.0, 26_200.0, 20.0);
    let v = verdicts(&[&narrow, &wide], &m);
    let of = |id| {
        v.iter()
            .find(|(k, _)| *k == id)
            .map(|(_, x)| x.clone())
            .unwrap()
    };
    assert_eq!(
        of(wide.emitter_id),
        RegionVerdict::Emission { emission: 0 },
        "the box matching the measurement is the one shown"
    );
    assert_eq!(
        of(narrow.emitter_id),
        RegionVerdict::Reading {
            emission: 0,
            of: wide.emitter_id
        },
        "and the other is another reading of it, not a second emission"
    );
}

/// A box drawn over two emissions the spectrum separates is a **merge**, and defers — the two real
/// emissions are what stays. The explorer's 861.43 MHz case: one 557 kHz box over fragments.
#[test]
fn a_merged_box_over_two_emitters_splits() {
    let snap = spectrum(
        861_430_000.0,
        1_171.875,
        2048,
        &[
            (861_335_000.0, 30_000.0, 22.0),
            (861_450_000.0, 40_000.0, 25.0),
        ],
    );
    let region = FreqRange::new(861_156_000.0, 861_713_000.0);
    let m = measure_region(region, &snap, &OverlapConfig::default())
        .expect("the region is inside the tuned span");
    assert_eq!(
        m.emissions.len(),
        2,
        "two separated emitters measure as two emissions: {:?}",
        m.emissions
    );

    let merged = row(861_434_600.0, 557_000.0, 24.0);
    let a = row(861_335_000.0, 30_000.0, 22.0);
    let b = row(861_450_000.0, 40_000.0, 25.0);
    let v = verdicts(&[&merged, &a, &b], &m);
    let of = |id| {
        v.iter()
            .find(|(k, _)| *k == id)
            .map(|(_, x)| x.clone())
            .unwrap()
    };
    assert_eq!(of(a.emitter_id), RegionVerdict::Emission { emission: 0 });
    assert_eq!(of(b.emitter_id), RegionVerdict::Emission { emission: 1 });
    match of(merged.emitter_id) {
        RegionVerdict::Merged { emissions, of } => {
            assert_eq!(emissions, vec![0, 1]);
            assert_eq!(of, b.emitter_id, "it defers to the strongest it merged");
        }
        other => panic!("the 557 kHz box merges two measured emissions, got {other:?}"),
    }
}

/// The same merged box with **no** row for one of the emissions it covers is kept, not retired:
/// retiring it would lose an emission, and losing a signal is never the resolution of an overlap.
#[test]
fn a_merge_whose_emissions_have_no_rows_is_kept() {
    let snap = spectrum(
        861_430_000.0,
        1_171.875,
        2048,
        &[
            (861_335_000.0, 30_000.0, 22.0),
            (861_450_000.0, 40_000.0, 25.0),
        ],
    );
    let region = FreqRange::new(861_156_000.0, 861_713_000.0);
    let m = measure_region(region, &snap, &OverlapConfig::default()).unwrap();
    let merged = row(861_434_600.0, 557_000.0, 24.0);
    let a = row(861_335_000.0, 30_000.0, 22.0);
    let v = verdicts(&[&merged, &a], &m);
    assert_eq!(
        v.iter()
            .find(|(k, _)| *k == merged.emitter_id)
            .map(|(_, x)| x.clone()),
        Some(RegionVerdict::Kept {
            why: REGION_MERGE_UNCOVERED
        })
    );
}

/// **The guard the measurement must not break** (`hk_model::relate::distinguishing_evidence`'s own
/// example): a narrow emission sitting *off centre* inside a wide one is a subcarrier, not another
/// reading of its host, and it stays listed. A subcarrier is offset from its host by construction —
/// a concentric one would be the carrier — which is what makes concentricity the discriminator.
#[test]
fn an_offset_narrow_box_inside_a_wide_one_is_kept() {
    let snap = spectrum(
        100_000_000.0,
        1_171.875,
        2048,
        &[(100_000_000.0, 180_000.0, 30.0)],
    );
    let region = FreqRange::new(99_910_000.0, 100_090_000.0);
    let m = measure_region(region, &snap, &OverlapConfig::default()).unwrap();
    assert_eq!(m.emissions.len(), 1);
    let host = row(100_000_000.0, 180_000.0, 30.0);
    let sub = row(100_057_000.0, 12_500.0, 14.0);
    let v = verdicts(&[&host, &sub], &m);
    assert_eq!(
        v.iter()
            .find(|(k, _)| *k == sub.emitter_id)
            .map(|(_, x)| x.clone()),
        Some(RegionVerdict::Kept {
            why: REGION_OFF_CENTRE
        }),
        "the subcarrier is 57 kHz off the measured centre of a 180 kHz emission"
    );
}

/// The measurement says nothing it cannot see. At 4.9 kHz cells a 9.3 kHz box is under two cells
/// wide, so the region is left exactly as it was found and the refusal is recorded.
#[test]
fn a_region_measured_coarser_than_its_narrowest_box_is_refused() {
    let snap = spectrum(
        852_860_000.0,
        4_882.812_5,
        4096,
        &[(852_859_100.0, 26_200.0, 20.0)],
    );
    let region = FreqRange::new(852_846_000.0, 852_872_200.0);
    let m = measure_region(region, &snap, &OverlapConfig::default()).unwrap();
    let narrow = row(852_858_600.0, 9_300.0, 18.0);
    let wide = row(852_859_100.0, 26_200.0, 20.0);
    for (_, v) in verdicts(&[&narrow, &wide], &m) {
        assert_eq!(
            v,
            RegionVerdict::Kept {
                why: REGION_TOO_COARSE
            }
        );
    }
}

/// A region outside the tuned span has no measurement at all — never an empty one, which would read
/// as "nothing is on air there".
#[test]
fn an_untuned_region_measures_nothing_rather_than_silence() {
    let snap = spectrum(
        100_000_000.0,
        1_171.875,
        2048,
        &[(100_000_000.0, 180_000.0, 30.0)],
    );
    assert!(
        measure_region(
            FreqRange::new(452_000_000.0, 452_100_000.0),
            &snap,
            &OverlapConfig::default()
        )
        .is_none()
    );
}

/// **What the re-analysis costs, measured** (T-453). It runs on the detect *writer*, off the
/// capture thread, and only for a region an overlap was found in — but the figure is reported
/// rather than assumed. Printed, not asserted: a latency bound belongs in the `timing` tier
/// (docs/10 §3.6), and this is here so the number in the commit message can be re-measured.
#[test]
fn the_cost_of_one_region_is_measured() {
    let snap = spectrum(
        861_430_000.0,
        1_171.875,
        2048,
        &[
            (861_335_000.0, 30_000.0, 22.0),
            (861_450_000.0, 40_000.0, 25.0),
        ],
    );
    // The widest region the explorer reported: 557 kHz, 476 cells at 1.17 kHz.
    let region = FreqRange::new(861_156_000.0, 861_713_000.0);
    let cfg = OverlapConfig::default();
    let merged = row(861_434_600.0, 557_000.0, 24.0);
    let a = row(861_335_000.0, 30_000.0, 22.0);
    let b = row(861_450_000.0, 40_000.0, 25.0);
    let rows: Vec<&RowEvidence> = vec![&merged, &a, &b];

    let n = 2_000;
    let started = std::time::Instant::now();
    let mut resolved = 0usize;
    for _ in 0..n {
        let m = measure_region(region, &snap, &cfg).unwrap();
        resolved += region_verdicts(&rows, &m, &Tolerances::default()).len();
    }
    let per = started.elapsed().as_secs_f64() / n as f64;
    eprintln!(
        "T-978: measure_region + region_verdicts over a {:.0} kHz region ({} cells, 3 boxes): \
         {:.1} us per region ({resolved} verdicts over {n} passes)",
        region.width_hz() / 1e3,
        (region.width_hz() / 1_171.875).round(),
        per * 1e6,
    );
    assert_eq!(resolved, 3 * n, "every box gets a verdict every pass");
}
