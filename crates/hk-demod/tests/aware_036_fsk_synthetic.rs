//! AWARE-036 (unknown burst triage → bits → framing), synthetic: the T-023 `fsk_burst_train`
//! sensor (2-FSK 4800 Bd, ±9.6 kHz, preamble + sync 2DD4 + 48-bit payload + CRC-16/CCITT-FALSE,
//! bursts every 120 ms with jitter, CFO 5 kHz), our own test signal, so its payload may be
//! decoded and stored (classified `unrestricted` explicitly).
//!
//! - 20 dB: BER ≤ 1e-3 against the generator bits; framing finds the preamble, sync 2DD4, a
//!   48-bit payload and CRC-16/CCITT-FALSE with validate ratio ≥ 0.95; every CRC-valid payload
//!   equals the truth; Decodes carry the payload, a ground-truth Annotation is written and the
//!   Emitter's known status is appended as `known`.
//! - 12 dB: BER ≤ 1e-2; CRC still identified; CRC-valid payloads exact.
//! - GFSK BT 0.5 at 20 dB: same framing.
//! - Inverted polarity (conjugated IQ): polarity `inverted`, sync 2DD4, payloads exact.
//! - Weak bursts (4 dB): C14 does not trust them; prior-led trials (cluster prior from the
//!   20 dB run + sync prior from its model) find the sync, and the prior is reported.
//! - Negatives: noise boxes → no sync, no CRC; a wrong-rate prior → no CRC.

mod common;

use common::*;
use hk_demod::fsk::{
    ClusterPrior, DemodPriors, EmitterClassification, FramedRecordContext, FskBurst, FskReceiver,
    FskReceiverConfig, SeedSource, SyncPrior, framing_identity, write_framed_bursts,
};
use hk_e2e::{Fixture, SynthRequest, synth_or_skip};
use hk_estimate::SnippetRequest;
use hk_estimate::framing::bits::{BitOrder, unpack};
use hk_estimate::framing::{FramingConfig, FramingResult, Polarity, infer_framing};
use hk_model::{
    AnnotationKind, AnnotationTarget, ContentClass, CrcStatus, InventoryIdentity, InventoryQuery,
    KnownStatus, Repository, StatusAuthor,
};
use num_complex::Complex;

const AWARE_036: &str = "AWARE-036";

struct Scene {
    iq: Vec<Complex<i8>>,
    fx: Fixture,
    center: f64,
    fs: f64,
}

fn scene(snr_db: f64, bt: f64) -> Option<Scene> {
    let req = SynthRequest::new("fsk_burst_train")
        .seed(36)
        .param("snr_db", snr_db)
        .param("bt", bt)
        .param("cfo_hz", 5000.0)
        .param("duration_s", 2.4);
    let out = match req.generate() {
        Ok(o) => o,
        Err(e) if e.is_unavailable() && !hk_e2e::synth::require_synth() => {
            eprintln!("SKIP: synth unavailable: {e}");
            return None;
        }
        Err(e) => panic!("synth failed: {e}"),
    };
    let fx = out.fixture(0).unwrap();
    Some(Scene {
        iq: read_ci8(&fx.data_path()),
        center: fx.center_hz_at(0).unwrap(),
        fs: fx.sample_rate,
        fx,
    })
}

fn truth_bits(b: &hk_e2e::fixture::TruthItem) -> Vec<u8> {
    let hex = b.value["frame"]["bits_hex"].as_str().unwrap();
    let n = b.value["frame"]["n_bits"].as_u64().unwrap() as usize;
    let bytes: Vec<u8> = (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap())
        .collect();
    let mut bits = unpack(&bytes, BitOrder::MsbFirst);
    bits.truncate(n);
    bits
}

/// Bit errors of `truth` inside `bits` at the best alignment (missing bits count as errors).
fn errors(bits: &[u8], truth: &[u8]) -> usize {
    let mut best = truth.len();
    let n = truth.len() as isize;
    for off in -16..=(bits.len() as isize - n + 16).max(-16) {
        let mut e = 0;
        for (k, &t) in truth.iter().enumerate() {
            let j = off + k as isize;
            if j < 0 || j as usize >= bits.len() || bits[j as usize] != t {
                e += 1;
            }
        }
        best = best.min(e);
    }
    best
}

fn run_all(
    s: &Scene,
    conj: bool,
    cfg: FskReceiverConfig,
    priors: &DemodPriors,
    boxes_offset_hz: f64,
) -> Vec<FskBurst> {
    let iq: Vec<Complex<i8>> = if conj {
        s.iq.iter()
            .map(|z| Complex::new(z.re, z.im.saturating_neg()))
            .collect()
    } else {
        s.iq.clone()
    };
    let prov = provenance(s.center, s.fs);
    let mut rx = FskReceiver::new(cfg);
    s.fx.of_kind("fsk-burst")
        .iter()
        .map(|b| {
            let off = b.center_hz() - s.center + boxes_offset_hz;
            let req = SnippetRequest {
                start_index: b.sample_start,
                end_index: b.sample_start + b.sample_count,
                center_offset_hz: if conj { -off } else { off },
                bandwidth_hz: b.bandwidth_hz(),
            };
            rx.run(info(0, &prov), &iq, &req, priors).unwrap()
        })
        .collect()
}

fn bits_of(bursts: &[FskBurst]) -> Vec<Vec<u8>> {
    bursts.iter().map(|b| b.bits().to_vec()).collect()
}

fn describe(tag: &str, bursts: &[FskBurst], r: &FramingResult) {
    for (i, b) in bursts.iter().enumerate() {
        let s = b.symbols.as_ref();
        eprintln!(
            "[{AWARE_036}] {tag} burst {i:2}: seed {} rate {:.1} trusted {} (C14 {:?} best {:?} \
             /{:?}) bits {:4} lock {:.2} timing {:.3} UI eye {:.2} dev {:?} snr/sym {:?} sync {:?} \
             crc {:?}",
            b.seed.source.as_str(),
            s.map_or(f64::NAN, |s| s.lock.tracked_rate_bd),
            b.seed.c14_trusted,
            b.blind.symbol_rate_bd.value().map(|r| r.round()),
            b.blind.best_candidate_bd().map(|r| r.round()),
            b.seed.harmonic_divisor,
            b.bits().len(),
            s.map_or(0.0, |s| s.lock.lock_quality),
            s.map_or(f64::NAN, |s| s.lock.timing_rms_ui),
            s.map_or(0.0, |s| s.lock.eye_opening),
            s.and_then(|s| s.deviation_hz).map(|d| d.round()),
            s.and_then(|s| s.symbol_snr_db)
                .map(|d| (d * 10.0).round() / 10.0),
            r.frames[i].sync_bit,
            r.frames[i].crc_valid
        );
    }
    eprintln!(
        "[{AWARE_036}] {tag} model: {}",
        serde_json::to_string(&r.model).unwrap()
    );
}

/// Asserts the sensor framing; returns the CRC validate ratio.
fn assert_sensor_framing(tag: &str, s: &Scene, bursts: &[FskBurst], r: &FramingResult) -> f64 {
    let m = &r.model;
    let pre = m.preamble.as_ref().expect("preamble");
    assert!(
        (24..=33).contains(&pre.length_bits_median),
        "[{AWARE_036}] {tag}: preamble {pre:?}"
    );
    let sync = m.sync.as_ref().expect("sync");
    assert_eq!(sync.hex.as_deref(), Some("2DD4"), "[{AWARE_036}] {tag}");
    let crc = m
        .crc
        .as_ref()
        .unwrap_or_else(|| panic!("[{AWARE_036}] {tag}: no CRC"));
    assert_eq!(crc.algorithm, "CRC-16/CCITT-FALSE", "[{AWARE_036}] {tag}");
    assert_eq!(m.frame.payload_bits, Some(48), "[{AWARE_036}] {tag}");
    let truths = s.fx.of_kind("fsk-burst");
    let mut valid = 0;
    for (i, b) in bursts.iter().enumerate() {
        if r.frames[i].crc_valid != Some(true) {
            continue;
        }
        valid += 1;
        let p = r.payload(i, b.bits()).unwrap();
        let hex: String = p.bytes.iter().map(|x| format!("{x:02x}")).collect();
        assert_eq!(
            hex,
            truths[i].value["frame"]["payload_hex"].as_str().unwrap(),
            "[{AWARE_036}] {tag}: burst {i} CRC-valid payload differs from truth"
        );
    }
    assert_eq!(valid, crc.validated);
    crc.validate_ratio
}

#[test]
fn aware_036_fsk_synthetic_bits_framing_crc_and_records() {
    let _ = synth_or_skip!(
        SynthRequest::new("fsk_burst_train")
            .seed(36)
            .param("duration_s", 0.1)
    );
    let mut strong: Option<(Vec<FskBurst>, FramingResult)> = None;
    for (snr, ber_bound, min_ratio) in [(20.0, 1e-3, 0.95), (12.0, 1e-2, 0.8)] {
        let Some(s) = scene(snr, 0.0) else { return };
        // Blind: C14 where it trusts, else the standard-rate table (no emitter priors).
        let blind = DemodPriors {
            standard_rates: true,
            ..Default::default()
        };
        let bursts = run_all(&s, false, FskReceiverConfig::default(), &blind, 0.0);
        let truths = s.fx.of_kind("fsk-burst");
        assert!(truths.len() >= 15, "[{AWARE_036}] {} bursts", truths.len());
        let (mut errs, mut total) = (0usize, 0usize);
        for (b, t) in bursts.iter().zip(&truths) {
            let tb = truth_bits(t);
            errs += errors(b.bits(), &tb);
            total += tb.len();
        }
        let ber = errs as f64 / total as f64;
        let r = infer_framing(&bits_of(&bursts), &FramingConfig::default());
        let tag = format!("{snr} dB");
        describe(&tag, &bursts, &r);
        eprintln!("[{AWARE_036}] {tag}: BER {ber:.2e} ({errs}/{total})");
        assert!(
            ber <= ber_bound,
            "[{AWARE_036}] {tag}: BER {ber:.2e} > {ber_bound:.0e}"
        );
        let ratio = assert_sensor_framing(&tag, &s, &bursts, &r);
        assert!(
            ratio >= min_ratio,
            "[{AWARE_036}] {tag}: validate ratio {ratio}"
        );
        assert_eq!(r.model.polarity, Polarity::Normal);
        if snr == 20.0 {
            strong = Some((bursts, r));
        }
    }

    // Records: our own synthetic sensor, classified unrestricted by the caller.
    let (bursts, r) = strong.unwrap();
    let Some(s) = scene(20.0, 0.0) else { return };
    let mut repo = Repository::open_in_memory().unwrap();
    let ctx = FramedRecordContext {
        classification: Some(EmitterClassification {
            content_class: ContentClass::Unrestricted,
            by: "test: synthetic AWARE-036 sensor (own test signal)".into(),
        }),
        ..Default::default()
    };
    let w = write_framed_bursts(&mut repo, &bursts, &r, &ctx).unwrap();
    assert_eq!(w.content_class, ContentClass::Unrestricted);
    let truths = s.fx.of_kind("fsk-burst");
    let mut with_content = 0;
    for &(i, id) in &w.decode_ids {
        let d = repo.decode(id).unwrap();
        assert_eq!(d.decoder_id, "hk-infer");
        assert!(
            d.frame_model.contains("crc=CRC-16/CCITT-FALSE"),
            "{}",
            d.frame_model
        );
        if d.crc_status == CrcStatus::Valid {
            // `payload_hex` is lowercase, like the generator's truth (T-037b).
            let hex = d.content.as_ref().expect("content")["payload_hex"]
                .as_str()
                .unwrap();
            assert_eq!(
                hex,
                truths[i].value["frame"]["payload_hex"].as_str().unwrap()
            );
            with_content += 1;
        }
    }
    assert!(with_content as f64 >= 0.95 * truths.len() as f64);
    let gt = repo
        .annotations_for(&AnnotationTarget::Emitter(w.emitter_id))
        .unwrap();
    let gt: Vec<_> = gt
        .iter()
        .filter(|a| a.kind == AnnotationKind::GroundTruth)
        .collect();
    assert_eq!(gt.len(), 1, "[{AWARE_036}] ground-truth annotation");
    eprintln!(
        "[{AWARE_036}] ground truth: {} ({})",
        gt[0].value, gt[0].metadata["valid_frames"]
    );
    assert!(gt[0].value.contains("crc-16-ccitt-false"));
    assert!(w.known_status_appended);
    let hist = repo.known_status_history(w.emitter_id).unwrap();
    let last = hist.last().unwrap();
    assert_eq!(
        (last.status, last.author),
        (KnownStatus::Known, StatusAuthor::Decoder)
    );
    assert!(hist.len() >= 2, "{hist:?}");
    assert_eq!(
        repo.emitter(w.emitter_id).unwrap().known_status,
        KnownStatus::Known
    );
    let bs = w.bitstream.as_ref().unwrap();
    assert_eq!(bs.framing.sync_word_hex.as_deref(), Some("2DD4"));
    assert_eq!(
        repo.bitstream(bs.id).unwrap().content_class,
        ContentClass::Unrestricted
    );

    // T-034: the classified sensor's framing identity is visible in the inventory, and
    // re-writing the same bursts (a re-demodulation) does not grow the count.
    let framing = framing_identity(&r).expect("framing identity");
    let entry = |repo: &Repository, id| {
        repo.query_inventory(&InventoryQuery::default())
            .unwrap()
            .entries
            .into_iter()
            .find(|e| e.emitter.id == id)
            .expect("inventory entry")
    };
    let e = entry(&repo, w.emitter_id);
    assert!(
        matches!(
            &e.identity,
            InventoryIdentity::Clear { identity, class: ContentClass::Unrestricted } if *identity == framing
        ),
        "[{AWARE_036}] {:?}",
        e.identity
    );
    assert_eq!(e.emitter.count, bursts.len() as u64);
    let again = write_framed_bursts(&mut repo, &bursts, &r, &ctx).unwrap();
    assert_eq!(
        (again.emitter_id, again.emitter_created),
        (w.emitter_id, false)
    );
    assert_eq!(
        entry(&repo, w.emitter_id).emitter.count,
        bursts.len() as u64
    );
}

#[test]
fn aware_036_fsk_synthetic_gfsk_and_inverted_polarity() {
    let Some(s) = scene(20.0, 0.5) else { return };
    let bursts = run_all(
        &s,
        false,
        FskReceiverConfig::default(),
        &DemodPriors::default(),
        0.0,
    );
    let r = infer_framing(&bits_of(&bursts), &FramingConfig::default());
    describe("GFSK BT 0.5", &bursts, &r);
    assert!(assert_sensor_framing("GFSK BT 0.5", &s, &bursts, &r) >= 0.95);

    let Some(s) = scene(20.0, 0.0) else { return };
    let bursts = run_all(
        &s,
        true,
        FskReceiverConfig::default(),
        &DemodPriors::default(),
        0.0,
    );
    let r = infer_framing(&bits_of(&bursts), &FramingConfig::default());
    describe("inverted", &bursts, &r);
    assert_eq!(
        r.model.polarity,
        Polarity::Inverted,
        "[{AWARE_036}] inverted IQ"
    );
    assert!(assert_sensor_framing("inverted", &s, &bursts, &r) >= 0.95);
    // The demodulated bits really are complemented.
    let tb = truth_bits(s.fx.of_kind("fsk-burst")[0]);
    let inv: Vec<u8> = tb.iter().map(|b| 1 - b).collect();
    assert!(errors(bursts[0].bits(), &inv) <= 1);
}

#[test]
fn aware_036_fsk_synthetic_weak_bursts_prior_led_and_negatives() {
    let Some(strong) = scene(20.0, 0.0) else {
        return;
    };
    let sb = run_all(
        &strong,
        false,
        FskReceiverConfig::default(),
        &DemodPriors::default(),
        0.0,
    );
    let sr = infer_framing(&bits_of(&sb), &FramingConfig::default());
    let cluster = ClusterPrior::from_trusted(&sb, 0.02);
    eprintln!("[{AWARE_036}] cluster priors {cluster:?}");
    assert!(
        !cluster.is_empty(),
        "[{AWARE_036}] no trusted C14 rate at 20 dB"
    );
    assert!((cluster[0].rate_bd / 4800.0 - 1.0).abs() < 0.01);
    let priors = DemodPriors {
        cluster: cluster.clone(),
        standard_rates: true,
        sync: SyncPrior::from_model(&sr.model),
    };

    // Weak bursts.
    let Some(weak) = scene(4.0, 0.0) else { return };
    let wb = run_all(&weak, false, FskReceiverConfig::default(), &priors, 0.0);
    let untrusted: Vec<&FskBurst> = wb.iter().filter(|b| !b.seed.c14_trusted).collect();
    let found = untrusted
        .iter()
        .filter(|b| b.seed.sync_confirmed == Some(true))
        .count();
    for b in &wb {
        eprintln!(
            "[{AWARE_036}] 4 dB: trusted {} seed {:?} confirmed {:?} trials {:?}",
            b.seed.c14_trusted,
            b.seed.source,
            b.seed.sync_confirmed,
            b.seed
                .trials
                .iter()
                .map(|t| (t.rate_bd, t.sync_found, (t.lock_quality * 100.0).round()))
                .collect::<Vec<_>>()
        );
    }
    assert!(
        untrusted.len() as f64 >= 0.5 * wb.len() as f64,
        "[{AWARE_036}] 4 dB bursts should be below the C14 trust floor"
    );
    assert!(
        found as f64 >= 0.8 * untrusted.len() as f64,
        "[{AWARE_036}] prior-led sync found in {found}/{}",
        untrusted.len()
    );
    for b in untrusted
        .iter()
        .filter(|b| b.seed.sync_confirmed == Some(true))
    {
        assert!(
            matches!(b.seed.source, SeedSource::ClusterPrior { index: 0, .. }),
            "[{AWARE_036}] prior reported {:?}",
            b.seed.source
        );
        assert!((b.seed.rate_bd - cluster[0].rate_bd).abs() < 1.0);
    }
    let wr = infer_framing(
        &bits_of(&wb),
        &FramingConfig {
            sync_prior: SyncPrior::from_model(&sr.model).map(|p| p.bits),
            ..Default::default()
        },
    );
    eprintln!(
        "[{AWARE_036}] 4 dB framing: sync found {:?}, crc {:?}",
        wr.model.sync.as_ref().map(|s| s.found_in),
        wr.model.crc.as_ref().map(|c| (&c.algorithm, c.validated))
    );

    // Negative: boxes of pure noise (same times, 150 kHz away from the channel).
    let nb = run_all(
        &strong,
        false,
        FskReceiverConfig::default(),
        &DemodPriors {
            standard_rates: true,
            ..Default::default()
        },
        -150e3,
    );
    let nr = infer_framing(&bits_of(&nb), &FramingConfig::default());
    eprintln!("[{AWARE_036}] noise boxes: {:?}", nr.model.status);
    assert!(nr.model.crc.is_none(), "[{AWARE_036}] CRC claimed on noise");
    assert!(
        nr.model.sync.as_ref().is_none_or(|s| s.found_in == 0),
        "[{AWARE_036}] sync on noise {:?}",
        nr.model.sync
    );

    // Negative: a wrong-rate prior (6000 Bd), trusted C14 disabled.
    let wrong = DemodPriors {
        cluster: vec![ClusterPrior {
            rate_bd: 6000.0,
            source: "test: wrong rate".into(),
            ..cluster[0].clone()
        }],
        standard_rates: false,
        sync: None,
    };
    let cfg = FskReceiverConfig {
        use_trusted: false,
        ..Default::default()
    };
    let xb = run_all(&strong, false, cfg, &wrong, 0.0);
    let xr = infer_framing(&bits_of(&xb), &FramingConfig::default());
    eprintln!(
        "[{AWARE_036}] wrong-rate prior: {:?} candidate {:?}",
        xr.model.status, xr.model.crc_candidate
    );
    assert!(
        xr.model.crc.is_none(),
        "[{AWARE_036}] false CRC at a wrong rate"
    );
}
