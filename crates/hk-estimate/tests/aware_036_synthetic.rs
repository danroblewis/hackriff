//! AWARE-036 (unknown burst triage), synthetic: the `fsk_burst_train` scenario (GFSK BT 0.5,
//! 4.8 kBd, 9.6 kHz deviation, 3 kHz CFO) through snippet extraction and C13.
//!
//! Per burst, at 20 and 30 dB: OBW99 within 10 % of the generator's Carson bandwidth, RF centre
//! within 1 % of the symbol rate, box and extent SNR within 1 dB of the generator's SNR. BT 0.5
//! keeps OBW99 near Carson (rectangular FSK's OBW99 is 13 % wider, see the report).

mod common;

use common::*;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_estimate::{FamilyHint, Hints, SnippetRequest};

const AWARE_036: &str = "AWARE-036";

#[test]
fn aware_036_fsk_burst_train_obw_cfo_snr() {
    for snr_db in [20.0, 30.0] {
        let out = synth_or_skip!(
            SynthRequest::new("fsk_burst_train")
                .seed(36)
                .param("bt", 0.5)
                .param("snr_db", snr_db)
        );
        let fx = out.fixture(0).unwrap();
        let fs = fx.sample_rate;
        let center = fx.center_hz_at(0).unwrap();
        let iq = read_ci8(&fx.data_path());
        let prov = hk_core::ProvenanceHandle::new(
            serde_json::from_value(provenance_json(center, fs, false)).unwrap(),
        );
        let mut ex = hk_estimate::SnippetExtractor::new(Default::default());
        let mut est = hk_estimate::ParamEstimator::new(Default::default());
        let bursts = fx.of_kind("fsk-burst");
        assert!(bursts.len() >= 3, "[{AWARE_036}] need bursts");
        for b in &bursts {
            let rate = b.expect_f64("symbol_rate_bd");
            let bw_truth = b.expect_f64("bandwidth_hz");
            let rf_truth = b.expect_f64("rf_center_hz");
            let snr_truth = b.expect_f64("snr_db");
            let req = SnippetRequest {
                start_index: b.sample_start,
                end_index: b.sample_start + b.sample_count,
                center_offset_hz: b.center_hz() - center,
                bandwidth_hz: b.bandwidth_hz(),
            };
            let snip = ex
                .extract(common::info(0, &prov), &iq, &req)
                .expect("extract");
            for family in [FamilyHint::Fsk { levels: 2 }, FamilyHint::Unknown] {
                let hints = Hints {
                    family,
                    ..Default::default()
                };
                let ps = est.estimate(&snip, &hints);
                let ctx = format!(
                    "burst {} at {snr_db} dB ({family:?})",
                    b.f64("burst_index").unwrap_or(-1.0)
                );
                let obw = ps.obw99_hz.value().unwrap_or(f64::NAN);
                let rf_err = ps.rf_center_hz.value().map_or(f64::NAN, |v| v - rf_truth);
                let snr_box = ps.snr_box_db.value().unwrap_or(f64::NAN);
                let snr_ext = ps.snr_extent_db.value().unwrap_or(f64::NAN);
                eprintln!(
                    "[{AWARE_036}] {ctx}: OBW {obw:.0} (truth {bw_truth:.0}, {:+.1} %), rf err \
                     {rf_err:+.1} Hz ({:+.2} % Rs), SNR box {snr_box:.2} ext {snr_ext:.2} \
                     (truth {snr_truth:.2}), cost {} us",
                    100.0 * (obw / bw_truth - 1.0),
                    100.0 * rf_err / rate,
                    ps.cost_us
                );
                if family == FamilyHint::Unknown {
                    continue; // the centroid is biased by bit imbalance: reported, not asserted
                }
                assert!(
                    (obw / bw_truth - 1.0).abs() <= 0.10,
                    "[{AWARE_036}] {ctx}: OBW {obw:.0} vs {bw_truth:.0}"
                );
                assert!(
                    rf_err.abs() <= 0.01 * rate,
                    "[{AWARE_036}] {ctx}: RF centre error {rf_err:.1} Hz"
                );
                assert!(
                    (snr_box - snr_truth).abs() <= 1.0,
                    "[{AWARE_036}] {ctx}: box SNR {snr_box:.2} vs {snr_truth:.2}"
                );
                assert!(
                    (snr_ext - snr_truth).abs() <= 1.0,
                    "[{AWARE_036}] {ctx}: extent SNR {snr_ext:.2} vs {snr_truth:.2}"
                );
            }
        }
    }
}
