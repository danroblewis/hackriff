//! T-888: an unmeasured PSK family score is **absent** from `features@1`, not a default.
//!
//! T-311's rule — absent means not measured — already held for `blind_ook` and `blind_fsk`. The two
//! PSK scores were always set, and where C14 could not look past a lower-order line they carried a
//! value fixed by the veto constant: every integer-h FSK device row read `blind_qpsk` = 0.200
//! exactly (T-877), which a fitted density reads as evidence. These tests drive synthetic IQ
//! through C14 and the feature vector — the path the pipeline and `fit-densities` both take — and
//! assert on the vector itself.

use hk_classify::SymbolEstimator;
use hk_classify::features::{FeatureInput, Features, features};
use hk_classify::synth::{Class, DEV_SEEDS, SynthConfig, generate};

const SNR_DB: f64 = 25.0;

fn features_of(class: Class, seed: u64) -> Features {
    let s = generate(class, &SynthConfig::new(SNR_DB, seed));
    let symbols = SymbolEstimator::new().from_samples(
        &s.symbol_samples,
        s.symbol_sample_rate_hz,
        Some(s.obw_hz),
        Some(SNR_DB),
    );
    assert!(symbols.is_some(), "{class:?}: C14 did not run");
    features(&FeatureInput {
        samples: &s.samples,
        sample_rate_hz: s.sample_rate_hz,
        obw_hz: Some(s.obw_hz),
        snr_db: Some(SNR_DB),
        symbols: symbols.as_ref(),
    })
}

/// A PSK score is never the veto floor times a saturated ramp: where it is present it is a
/// measurement, and the exact floor values (0.3 for BPSK's ratio veto, 0.2 for QPSK's) — what a
/// record whose lower order decisively explains the higher one produced — no longer appear.
#[test]
fn no_psk_score_is_the_veto_floor_default() {
    for class in Class::TAXONOMY {
        for seed in DEV_SEEDS.start..DEV_SEEDS.start + 4 {
            let f = features_of(*class, seed);
            for (name, floor) in [("blind_bpsk", 0.3), ("blind_qpsk", 0.2)] {
                if let Some(v) = f.get(name) {
                    assert!(
                        (v - floor).abs() > 1e-9,
                        "{class:?} seed {seed}: {name} = {v}, the veto-floor default"
                    );
                }
            }
        }
    }
}

/// A carrier is an order-1 line, so its square and fourth power are lines whatever it carries:
/// neither PSK score can be measured, and both are absent.
#[test]
fn a_carrier_leaves_both_psk_scores_absent() {
    for seed in DEV_SEEDS.start..DEV_SEEDS.start + 4 {
        let f = features_of(Class::Cw, seed);
        assert_eq!(
            f.get("blind_bpsk"),
            None,
            "cw seed {seed}: blind_bpsk present"
        );
        assert_eq!(
            f.get("blind_qpsk"),
            None,
            "cw seed {seed}: blind_qpsk present"
        );
    }
}

/// BPSK's order-2 line implies an order-4 one, so the QPSK score is unmeasured on BPSK — while the
/// BPSK score is measured, and high. QPSK has no order-2 line, so both are measured: its
/// `blind_bpsk` is a real "looked, no order-2 line", whatever residual order-1 coherence sits
/// beside it, and so is every other class's zero where the higher order has no line at all.
#[test]
fn psk_scores_are_absent_exactly_where_the_lower_order_explains_them() {
    for seed in DEV_SEEDS.start..DEV_SEEDS.start + 4 {
        let bpsk = features_of(Class::Bpsk, seed);
        let b = bpsk.get("blind_bpsk").expect("bpsk: blind_bpsk measured");
        assert!(b > 0.5, "bpsk seed {seed}: blind_bpsk {b}");
        assert_eq!(
            bpsk.get("blind_qpsk"),
            None,
            "bpsk seed {seed}: blind_qpsk present"
        );

        let qpsk = features_of(Class::Qpsk, seed);
        let q = qpsk.get("blind_qpsk").expect("qpsk: blind_qpsk measured");
        assert!(q > 0.5, "qpsk seed {seed}: blind_qpsk {q}");
        assert_eq!(
            qpsk.get("blind_bpsk"),
            Some(0.0),
            "qpsk seed {seed}: blind_bpsk must be measured, and zero"
        );

        // No line at either order: measured, and zero — not absent.
        let ofdm = features_of(Class::Ofdm, seed);
        assert_eq!(ofdm.get("blind_bpsk"), Some(0.0), "ofdm seed {seed}");
        assert_eq!(ofdm.get("blind_qpsk"), Some(0.0), "ofdm seed {seed}");
    }
}
