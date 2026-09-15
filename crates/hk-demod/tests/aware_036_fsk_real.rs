//! AWARE-036 (unknown burst triage → bits → framing), recorded: the 915 MHz FHSS 2-FSK fixture
//! (`ism_915M_10M_l24g30a1_t42p3_1p2s`, HackRF One, 10 Msps). Framing *structure*
//! from unidentified traffic.
//!
//! 1. Pass 1: every annotated box through [`FskReceiver`] without priors; trusted C14 bursts
//!    give emitter-cluster priors (rate/deviation) and a first framing model (sync).
//! 2. Pass 2: every box with prior-led trials (cluster priors, standard rates, sync prior).
//! 3. Framing inference over all pass-2 bursts: the learned sync is `0000110001011111`
//!    (S5 truth) and is located, at the truth rate, in ≥ 90 % of the sync-truth bursts;
//!    bursts below the C14 trust floor are among them, with the prior reported. Any CRC,
//!    length field or whitening is reported, not required (S5 found none).

mod common;

use common::*;
use hk_demod::fsk::{
    ClusterPrior, DemodPriors, FskBurst, FskReceiver, FskReceiverConfig, SeedSource, SyncPrior,
};
use hk_e2e::Fixture;
use hk_estimate::framing::{FramingConfig, infer_framing};
use hk_estimate::{Hints, SnippetConfig, SnippetRequest};

const AWARE_036: &str = "AWARE-036";
const NAME: &str = "ism_915M_10M_l24g30a1_t42p3_1p2s";
const SYNC: &str = "0000110001011111";

#[test]
fn aware_036_fsk_915mhz_sync_framing() {
    let (meta, data) = fixture_or_skip!(NAME);
    let fx = Fixture::load(&meta).unwrap();
    let prov = meta_provenance(&meta);
    let center = prov.tune.center_hz;
    let ppm = fx.scenario().and_then(|s| s.f64("/clock/ppm_measured"));
    let iq = read_ci8(&data);
    let cfg = FskReceiverConfig {
        snippet: SnippetConfig {
            min_rate_hz: 1.25e6,
            ..Default::default()
        },
        hints: Hints {
            clock_ppm: ppm,
            ..Default::default()
        },
        ..Default::default()
    };
    let emissions = fx.emissions();
    let requests: Vec<SnippetRequest> = emissions
        .iter()
        .map(|b| SnippetRequest {
            start_index: b.sample_start,
            end_index: b.sample_start + b.sample_count,
            center_offset_hz: b.center_hz() - center,
            bandwidth_hz: b.bandwidth_hz(),
        })
        .collect();
    let mut rx = FskReceiver::new(cfg);
    let run = |rx: &mut FskReceiver, priors: &DemodPriors| -> Vec<FskBurst> {
        requests
            .iter()
            .map(|r| rx.run(info(0, &prov), &iq, r, priors).unwrap())
            .collect()
    };

    // ---- pass 1: no priors
    let pass1 = run(&mut rx, &DemodPriors::default());
    let cluster = ClusterPrior::from_trusted(&pass1, 0.02);
    eprintln!("[{AWARE_036}] cluster priors: {cluster:?}");
    let trusted_bits: Vec<Vec<u8>> = pass1
        .iter()
        .filter(|b| b.seed.c14_trusted && b.symbols.is_some())
        .map(|b| b.bits().to_vec())
        .collect();
    let learn = infer_framing(&trusted_bits, &FramingConfig::default());
    eprintln!(
        "[{AWARE_036}] pass 1: {} trusted bursts; sync {:?}",
        trusted_bits.len(),
        learn.model.sync.as_ref().map(|s| (&s.bits, s.found_in))
    );
    assert!(!cluster.is_empty(), "[{AWARE_036}] no trusted C14 rate");

    // ---- pass 2: prior-led trials
    let priors = DemodPriors {
        cluster: cluster.clone(),
        standard_rates: true,
        sync: SyncPrior::from_model(&learn.model),
    };
    let pass2 = run(&mut rx, &priors);
    let bits2: Vec<Vec<u8>> = pass2.iter().map(|b| b.bits().to_vec()).collect();
    let result = infer_framing(&bits2, &FramingConfig::default());
    let m = &result.model;

    let mut sync_truth = 0usize;
    let mut found = 0usize;
    let mut weak_found = 0usize;
    for (i, (b, t)) in pass2.iter().zip(&emissions).enumerate() {
        let truth_rate = t.f64("symbol_rate_bd");
        let f = &result.frames[i];
        let rate = b.symbols.as_ref().map(|s| s.rate_bd);
        eprintln!(
            "[{AWARE_036}] burst {:2} ({:9}) snr {:5.1} truth {:?} | seed {:13} rate {:?} \
             trusted {} confirmed {:?} lock {:.2} | sync bit {:?} err {:?} preamble {:?} (S5 {:?}) \
             inv {} crc {:?}",
            t.f64("burst_index").unwrap_or(-1.0),
            t.kind,
            t.f64("snr_db").unwrap_or(f64::NAN),
            truth_rate,
            b.seed.source.as_str(),
            rate,
            b.seed.c14_trusted,
            b.seed.sync_confirmed,
            b.symbols.as_ref().map_or(0.0, |s| s.lock.lock_quality),
            f.sync_bit,
            f.sync_bit_errors,
            f.preamble_bits_before_sync,
            t.f64("preamble_bits"),
            f.inverted,
            f.crc_valid,
        );
        if t.kind != "fsk-burst" {
            continue;
        }
        sync_truth += 1;
        let rate_ok =
            matches!((rate, truth_rate), (Some(r), Some(tr)) if (r / tr - 1.0).abs() < 0.01);
        if f.sync_bit.is_some() && rate_ok {
            found += 1;
            if !b.seed.c14_trusted {
                weak_found += 1;
                assert_ne!(
                    b.seed.source,
                    SeedSource::TrustedC14,
                    "[{AWARE_036}] weak burst {i} must report its prior"
                );
            }
        }
    }
    let model_json = serde_json::to_string(m).unwrap();
    eprintln!("[{AWARE_036}] framing model: {model_json}");
    eprintln!(
        "[{AWARE_036}] sync-truth bursts {sync_truth}: sync found at the truth rate in {found} \
         ({weak_found} below the C14 trust floor, prior-led); CRC {:?}; candidate {:?}; length \
         field {:?}; whitening {:?}; payload {:?}",
        m.crc
            .as_ref()
            .map(|c| (&c.algorithm, c.validated, c.tested)),
        m.crc_candidate,
        m.length_field,
        m.whitening.as_ref().map(|w| &w.name),
        m.payload
            .as_ref()
            .map(|p| (p.class, p.constant_fraction, p.mean_entropy_bits)),
    );
    let s = m.sync.as_ref().expect("sync model");
    assert_eq!(s.bits, SYNC, "[{AWARE_036}] learned sync");
    assert_eq!(s.hex.as_deref(), Some("0C5F"));
    assert!(sync_truth >= 10);
    assert!(
        found as f64 >= 0.9 * sync_truth as f64,
        "[{AWARE_036}] sync found in {found}/{sync_truth} sync-truth bursts"
    );
    assert!(
        weak_found >= 1,
        "[{AWARE_036}] no weak burst decoded by priors"
    );
}
