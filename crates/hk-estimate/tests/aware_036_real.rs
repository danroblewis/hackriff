//! AWARE-036 (unknown burst triage), recorded: the 915 MHz FHSS 2-FSK fixture
//! (`ism_915M_10M_l24g30a1_t42p3_1p2s`, HackRF One, 10 Msps). Each annotated burst box goes
//! through snippet extraction and C13 with the fixture's measured clock (−6.8 ppm).
//!
//! Truth (`hackriff:truth`, S5): `rf_center_hz` (clock-corrected centre) and `bandwidth_hz`
//! (S5 noise-subtracted OBW99), `snr_db` (S5 box SNR). Edge-alias pairs (one transmission at
//! both ±fs/2) are counted once, using the stronger member; both members must agree.
//!
//! Asserted for sync-truth bursts with truth SNR ≥ 15 dB:
//! - RF centre (centroid, and FSK-hinted) within 3 kHz of the truth centre: 3 % of the
//!   100 kBd symbol rate, 1.5 % of the 200 kHz raster. The truth centre is itself an S5
//!   centroid; S5's per-burst raster offsets for this one transmitter spread over ±0.7 kHz.
//! - OBW99 within 0.9–1.35 × Carson (2·deviation + rate, from the independent sync and
//!   deviation truth), and no wider than S5's OBW99 + 5 %. S5 zeroed negative bins over a
//!   ≥ 1.25 Msps snippet, which adds noise to the tails, so its value is an upper bound.
//!
//! Negative controls: detection-only boxes (no sync truth) with S5 SNR < 0 dB must not report a
//! bandwidth or a centre (they abstain below the floor, fill the band or reach its edge).

mod common;

use common::*;
use hk_e2e::Fixture;
use hk_estimate::{Hints, SnippetRequest};

const AWARE_036: &str = "AWARE-036";
const NAME: &str = "ism_915M_10M_l24g30a1_t42p3_1p2s";

#[test]
fn aware_036_915mhz_bursts_centre_and_bandwidth() {
    let (meta, data) = fixture_or_skip!(NAME);
    let fx = Fixture::load(&meta).unwrap();
    let fs = fx.sample_rate;
    let prov = meta_provenance(&meta);
    let center = prov.tune.center_hz;
    let ppm = fx
        .scenario()
        .and_then(|s| s.f64("/clock/ppm_measured"))
        .expect("clock ppm in the scenario truth");
    let iq = read_ci8(&data);
    let mut ex = hk_estimate::SnippetExtractor::new(Default::default());
    let mut est = hk_estimate::ParamEstimator::new(Default::default());
    let hints = Hints {
        clock_ppm: Some(ppm),
        ..Default::default()
    };

    struct Row {
        index: i64,
        alias_of: Option<i64>,
        snr_truth: f64,
        rf_truth: Option<f64>,
        bw_truth: f64,
        carson: Option<f64>,
        sync: bool,
        ps: hk_estimate::ParameterSet,
        fsk: hk_estimate::ParameterSet,
    }
    let mut rows = Vec::new();
    let mut total_cost = 0u64;
    let mut extract_us = 0u128;
    for b in fx.emissions() {
        let req = SnippetRequest {
            start_index: b.sample_start,
            end_index: b.sample_start + b.sample_count,
            center_offset_hz: b.center_hz() - center,
            bandwidth_hz: b.bandwidth_hz(),
        };
        let t = std::time::Instant::now();
        let snip = ex
            .extract(common::info(0, &prov), &iq, &req)
            .expect("extract");
        extract_us += t.elapsed().as_micros();
        let ps = est.estimate(&snip, &hints);
        total_cost += ps.cost_us;
        let fsk = est.estimate(
            &snip,
            &Hints {
                family: hk_estimate::FamilyHint::Fsk { levels: 2 },
                ..hints
            },
        );
        if let (Some(rate), Some(dev)) = (b.f64("symbol_rate_bd"), b.f64("deviation_hz")) {
            let raster = b.f64("raster_channel_hz").unwrap();
            eprintln!(
                "[{AWARE_036}]    burst {:?}: Carson {:.0}, OBW/Carson {:?}, FSK rf-raster {:?}, \
                 centroid rf-raster {:?}, S5 rf-raster {:.0}, FSK σ {:?}",
                b.f64("burst_index"),
                2.0 * dev + rate,
                ps.obw99_hz.value().map(|o| o / (2.0 * dev + rate)),
                fsk.rf_center_corrected_hz
                    .value()
                    .map(|v| wrap(v - raster, fs)),
                ps.rf_center_corrected_hz
                    .value()
                    .map(|v| wrap(v - raster, fs)),
                b.f64("rf_center_hz")
                    .map_or(f64::NAN, |v| wrap(v - raster, fs)),
                fsk.cfo_hz.sigma()
            );
        }
        let row = Row {
            index: b.f64("burst_index").unwrap_or(-1.0) as i64,
            alias_of: b.f64("alias_of_burst_index").map(|v| v as i64),
            snr_truth: b.expect_f64("snr_db"),
            rf_truth: b.f64("rf_center_hz"),
            bw_truth: b.expect_f64("bandwidth_hz"),
            carson: b
                .f64("symbol_rate_bd")
                .zip(b.f64("deviation_hz"))
                .map(|(r, d)| 2.0 * d + r),
            sync: b.kind == "fsk-burst",
            ps,
            fsk,
        };
        let p = &row.ps;
        eprintln!(
            "[{AWARE_036}] burst {:2} ({}{}): truth snr {:5.1} bw {:7.0} | est snr box {:>6} ext \
             {:>6} obw {:>8} rf err {:>8} flags wrap={} edge={} pads_rej={} | {:?}",
            row.index,
            b.kind,
            row.alias_of
                .map_or(String::new(), |a| format!(", alias of {a}")),
            row.snr_truth,
            row.bw_truth,
            fmt(p.snr_box_db.value(), 1),
            fmt(p.snr_extent_db.value(), 1),
            fmt(p.obw99_hz.value(), 0),
            fmt(
                p.rf_center_corrected_hz
                    .value()
                    .zip(row.rf_truth)
                    .map(|(v, t)| wrap(v - t, fs)),
                0
            ),
            p.flags.nyquist_wrapped,
            p.flags.edge,
            p.flags.pads_rejected,
            (
                p.obw99_hz.reason().or(p.snr_box_db.reason()),
                p.obw99_hz.evidence().significance_db
            ),
        );
        rows.push(row);
    }
    eprintln!(
        "[{AWARE_036}] {} snippets: extraction {:.1} ms/snippet, estimation {:.2} ms/snippet",
        rows.len(),
        extract_us as f64 / 1e3 / rows.len() as f64,
        total_cost as f64 / 1e3 / rows.len() as f64
    );

    let mut checked = 0;
    for r in &rows {
        if !r.sync || r.snr_truth < 15.0 {
            continue;
        }
        if let Some(a) = r.alias_of {
            let partner = rows.iter().find(|o| o.index == a).expect("alias partner");
            // Count the pair once: the stronger member; the weaker must agree with it.
            if partner.snr_truth > r.snr_truth
                || (partner.snr_truth == r.snr_truth && partner.index < r.index)
            {
                continue;
            }
            let (x, y) = (
                r.ps.rf_center_corrected_hz.value(),
                partner.ps.rf_center_corrected_hz.value(),
            );
            if let (Some(x), Some(y)) = (x, y) {
                assert!(
                    wrap(x - y, fs).abs() <= 3_000.0,
                    "[{AWARE_036}] alias pair {}/{} disagree by {:.0} Hz",
                    r.index,
                    a,
                    wrap(x - y, fs)
                );
            }
            assert!(
                r.ps.flags.nyquist_wrapped,
                "[{AWARE_036}] edge burst {} not wrapped",
                r.index
            );
        }
        for (name, p) in [("centroid", &r.ps), ("fsk", &r.fsk)] {
            let rf = p.rf_center_corrected_hz.value().unwrap_or_else(|| {
                panic!(
                    "[{AWARE_036}] burst {} ({name}): no centre {:?}",
                    r.index, p.rf_center_hz
                )
            });
            let err = wrap(rf - r.rf_truth.unwrap(), fs);
            assert!(
                err.abs() <= 3_000.0,
                "[{AWARE_036}] burst {} ({name}): RF centre error {err:.0} Hz",
                r.index
            );
        }
        let obw = r.ps.obw99_hz.value().expect("obw");
        let carson = r.carson.expect("rate and deviation truth");
        assert!(
            (0.9..=1.35).contains(&(obw / carson)) && obw <= 1.05 * r.bw_truth,
            "[{AWARE_036}] burst {}: OBW {obw:.0} vs Carson {carson:.0} / S5 {:.0}",
            r.index,
            r.bw_truth
        );
        checked += 1;
    }
    assert_eq!(
        checked, 5,
        "[{AWARE_036}] bursts 3, 12, 14 and pairs 7/8, 9/10 counted once"
    );

    let negatives: Vec<&Row> = rows
        .iter()
        .filter(|r| !r.sync && r.snr_truth < 0.0)
        .collect();
    assert_eq!(
        negatives.len(),
        6,
        "[{AWARE_036}] detection-only slivers below 0 dB"
    );
    for r in negatives {
        assert!(
            !r.ps.obw99_hz.is_measured() && !r.ps.rf_center_hz.is_measured(),
            "[{AWARE_036}] sliver {} (S5 {:.1} dB) reported {:?} / {:?}",
            r.index,
            r.snr_truth,
            r.ps.obw99_hz,
            r.ps.rf_center_hz
        );
    }
}

fn wrap(d: f64, fs: f64) -> f64 {
    (d + fs / 2.0).rem_euclid(fs) - fs / 2.0
}

fn fmt(v: Option<f64>, digits: usize) -> String {
    v.map_or("-".into(), |x| format!("{x:.digits$}"))
}
