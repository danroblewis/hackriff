//! AWARE-036 (T4): an unknown ISM sensor. The `fsk_burst_train` synth (2-FSK 4800 Bd, ±9.6 kHz,
//! preamble + sync 2DD4 + 48-bit payload + CRC-16/CCITT-FALSE, a burst every 120 ms ± 10 ms,
//! 3 kHz CFO) replayed at 433.92 MHz, which is not an unrestricted band prior, so content fails
//! closed until a user classification rule vouches for the emitter.
//!
//! Unclassified run: every burst detected, one Track, one Emitter `known_status: unknown`, blind
//! symbol rate within ±1 % and deviation within ±5 %, CRC-valid Decodes whose content is withheld
//! (the gated getters also reduce their metadata to the fail-closed allowlist). Classified run:
//! framing recovered on every CRC-valid Decode (sync 2DD4, CRC-16/CCITT-FALSE) plus the decoder's
//! ground-truth label, the Decoder appends `known`, and the payloads are kept and match truth.

use hk_e2e::blind::matches_truth;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::{
    AnnotationKind, AnnotationTarget, CrcStatus, Decode, FreqRange, InventoryQuery, KnownStatus,
    LinkTarget, PageRequest, Region, Repository, StatusAuthor, TrackFilter,
};
use serde_json::json;

use crate::blind::{assert_truth_found, center_tol_hz, replay_config, start};
use crate::common::*;

const AWARE_036: &str = "AWARE-036";

fn decodes_of(repo: &Repository, eid: hk_model::EmitterId) -> Vec<Decode> {
    repo.emitter_links(eid)
        .unwrap()
        .into_iter()
        .filter_map(|l| match l.target {
            LinkTarget::Decode(id) => Some(repo.decode(id).unwrap()),
            _ => None,
        })
        .collect()
}

#[test]
fn aware_036_unknown_fsk_sensor_detected_tracked_estimated_framed_and_classified() {
    let out = synth_or_skip!(
        SynthRequest::new("fsk_burst_train")
            .seed(36)
            .param("snr_db", 20.0)
            .param("duration_s", 2.4)
    );
    let fx = out.fixture(0).unwrap();
    let truths = fx.of_kind("fsk-burst");
    let n = truths.len();
    assert!(n >= 15, "[{AWARE_036}] {n} truth bursts");
    let emitter_truth = &fx.scenario().unwrap().value["emitter"];
    let rate_bd = emitter_truth["symbol_rate_bd"].as_f64().unwrap();
    let dev_hz = emitter_truth["deviation_hz"].as_f64().unwrap();
    let payloads: Vec<String> = truths
        .iter()
        .map(|t| t.value["frame"]["payload_hex"].as_str().unwrap().to_owned())
        .collect();
    // Everything the run produced is matched against the private truth bursts' extents (T-047);
    // no region is queried.
    let at_sensor = |f: f64, bw: f64| {
        truths
            .iter()
            .any(|t| matches_truth(t, 0.0, f, bw, center_tol_hz(t)))
    };

    // --- Unclassified: metadata flows, content withheld.
    let dir = TempDir::new("a036");
    let (cfg, replay) = replay_config(&dir.0, &fx.meta_path, json!({}), hk_core::Pacing::Unpaced);
    assert_eq!(replay.class, hk_model::ContentClass::FAIL_CLOSED);
    let s = finish(start(cfg, replay));
    assert_eq!(s.always_on_lost_samples, 0);
    let repo = repo(&dir.0);

    // All bursts detected blind: every truth burst within frequency/extent/time tolerance.
    let found = assert_truth_found(AWARE_036, &dir.0, &fx, 0.0, true);
    assert_eq!(found.len(), n);

    // One Track: the sensor's bursts aggregate into one track (single-lobe fragments of a few
    // detections may add short tracks; those are below the fsk-bursts min_detections prior).
    let tracks: Vec<_> = repo
        .tracks_in_region(
            &Region::new(FreqRange::new(0.0, 7.0e9), ever()),
            &TrackFilter {
                min_detections: 4,
                ..TrackFilter::default()
            },
            PageRequest::default(),
        )
        .unwrap()
        .tracks
        .into_iter()
        .filter(|t| at_sensor(t.f_center_hz, 0.0))
        .collect();
    eprintln!(
        "[{AWARE_036}] tracks (≥4 detections): {:?}",
        tracks
            .iter()
            .map(|t| (t.f_center_hz, t.detection_count))
            .collect::<Vec<_>>()
    );
    assert_eq!(tracks.len(), 1, "[{AWARE_036}] one Track");

    // One Emitter, unknown, reached through query_inventory (identity withheld: unclassified).
    let emitters: Vec<_> = inventory(&repo, InventoryQuery::default())
        .into_iter()
        .filter(|e| {
            e.family.as_deref() == Some("2fsk")
                && at_sensor(e.emitter.f_center_hz, e.emitter.bandwidth_hz)
        })
        .collect();
    assert_eq!(
        emitters.len(),
        1,
        "[{AWARE_036}] one 2fsk Emitter: {emitters:?}"
    );
    let e = &emitters[0];
    assert_eq!(
        e.emitter.known_status,
        KnownStatus::Unknown,
        "[{AWARE_036}] unknown before classification"
    );
    assert!(
        repo.known_status_history(e.emitter.id)
            .unwrap()
            .iter()
            .all(|h| h.status == KnownStatus::Unknown)
    );

    // Blind estimates on the emitter's fingerprint.
    let fp = &e.emitter.fingerprint;
    let got_rate = fp["symbol_rate_hz"].as_f64().expect("symbol rate estimate");
    let got_dev = fp["deviation_hz"].as_f64().expect("deviation estimate");
    let rate_err = (got_rate - rate_bd) / rate_bd;
    let dev_err = (got_dev - dev_hz) / dev_hz;
    eprintln!(
        "[{AWARE_036}] symbol rate {got_rate:.1} Bd ({:+.3} %), deviation {got_dev:.0} Hz ({:+.2} %)",
        rate_err * 100.0,
        dev_err * 100.0
    );
    assert!(rate_err.abs() <= 0.01, "[{AWARE_036}] symbol rate ±1 %");
    assert!(dev_err.abs() <= 0.05, "[{AWARE_036}] deviation ±5 %");

    // Framing: CRC-valid Decodes carrying the recovered structure; content withheld.
    let decodes = decodes_of(&repo, e.emitter.id);
    let valid: Vec<&Decode> = decodes
        .iter()
        .filter(|d| d.crc_status == CrcStatus::Valid)
        .collect();
    eprintln!(
        "[{AWARE_036}] {} decodes, {} CRC-valid of {n} bursts",
        decodes.len(),
        valid.len()
    );
    assert!(
        valid.len() * 10 >= n * 8,
        "[{AWARE_036}] CRC-valid {} of {n}",
        valid.len()
    );
    // Unclassified, the gated getters withhold content and reduce metadata to the fail-closed
    // allowlist (the CRC status column still reads); framing structure is asserted on the
    // classified run below.
    for d in &valid {
        assert!(
            d.content.is_none(),
            "[{AWARE_036}] unclassified content kept"
        );
    }
    eprintln!(
        "[{AWARE_036}] unclassified: decode metadata {} ; ground-truth labels {:?}",
        valid[0].metadata,
        ground_truth_labels(&repo, e.emitter.id)
    );
    assert_eq!(
        count_found(&all_bytes(&dir.0), &sentinels(&payloads)),
        0,
        "[{AWARE_036}] payload stored without a classification"
    );

    // --- Classified by the user (own test sensor): CRC validates → known, payloads kept. The
    // classification rule's range is user configuration (the user vouches for their own sensor's
    // band), not a lookup: the emitter is still found by matching against the private truth.
    let dir2 = TempDir::new("a036c");
    let (cfg, replay) = replay_config(
        &dir2.0,
        &fx.meta_path,
        json!({ "pipeline": { "classify": [{
            "freq_hz": [433.8e6, 434.1e6],
            "content_class": "unrestricted",
            "by": "test: own synthetic AWARE-036 sensor"
        }] } }),
        hk_core::Pacing::Unpaced,
    );
    finish(start(cfg, replay));
    let repo2 = crate::common::repo(&dir2.0);
    let known: Vec<_> = inventory(&repo2, InventoryQuery::default())
        .into_iter()
        .filter(|e| {
            e.family.as_deref() == Some("2fsk")
                && at_sensor(e.emitter.f_center_hz, e.emitter.bandwidth_hz)
        })
        .collect();
    assert_eq!(known.len(), 1, "[{AWARE_036}] one classified emitter");
    let history = repo2.known_status_history(known[0].emitter.id).unwrap();
    assert_eq!(
        history.first().map(|h| h.status),
        Some(KnownStatus::Unknown)
    );
    let last = history.last().unwrap();
    assert_eq!(
        (last.status, last.author),
        (KnownStatus::Known, StatusAuthor::Decoder),
        "[{AWARE_036}] CRC-valid framing moves status toward known: {history:?}"
    );
    let classified = decodes_of(&repo2, known[0].emitter.id);
    // Framing recovered: sync 2DD4 and CRC-16/CCITT-FALSE on every CRC-valid Decode, and the
    // decoder's ground-truth label on the Emitter.
    let mut framed = 0;
    for d in classified
        .iter()
        .filter(|d| d.crc_status == CrcStatus::Valid)
    {
        let m = &d.metadata;
        assert_eq!(
            m["crc"]["algorithm"].as_str().map(str::to_ascii_uppercase),
            Some("CRC-16/CCITT-FALSE".into()),
            "[{AWARE_036}] CRC algorithm: {m}"
        );
        let sync = m["sync_word"].as_str().unwrap_or_default();
        assert_eq!(
            bits_to_hex(sync),
            "2dd4",
            "[{AWARE_036}] sync word bits {sync:?}"
        );
        framed += 1;
    }
    assert!(framed * 10 >= n * 8, "[{AWARE_036}] framed {framed} of {n}");
    let gt = ground_truth_labels(&repo2, known[0].emitter.id);
    eprintln!("[{AWARE_036}] classified: {framed} framed decodes, ground-truth labels {gt:?}");
    assert!(
        gt.iter()
            .any(|v| v.contains("sync-2dd4") && v.contains("crc-16-ccitt-false")),
        "[{AWARE_036}] framing label"
    );
    let kept: Vec<String> = classified
        .iter()
        .filter_map(|d| {
            d.content.as_ref()?["payload_hex"]
                .as_str()
                .map(str::to_owned)
        })
        .collect();
    // `payload_hex` is lowercase (`5a3c…`, T-037b), as the generator's truth.
    let matched = payloads.iter().filter(|t| kept.contains(t)).count();
    eprintln!(
        "[{AWARE_036}] classified: {} payloads kept (e.g. {:?} for truth {:?}), {matched} of {n} \
         truth payloads recovered; status {:?} ({})",
        kept.len(),
        kept.first(),
        payloads.first(),
        last.status,
        last.reason
    );
    assert!(
        matched * 10 >= n * 8,
        "[{AWARE_036}] payloads matching truth {matched} of {n}"
    );
}

/// Values of the Emitter's ground-truth annotations (gated getter).
fn ground_truth_labels(repo: &Repository, eid: hk_model::EmitterId) -> Vec<String> {
    repo.annotations_for(&AnnotationTarget::Emitter(eid))
        .unwrap()
        .into_iter()
        .filter(|a| a.kind == AnnotationKind::GroundTruth)
        .map(|a| a.value)
        .collect()
}

/// "0010110111010100" → "2dd4".
fn bits_to_hex(bits: &str) -> String {
    bits.as_bytes()
        .chunks(4)
        .filter(|c| c.len() == 4)
        .map(|c| {
            let v = c.iter().fold(0u8, |a, &b| (a << 1) | (b - b'0'));
            char::from_digit(u32::from(v), 16).unwrap()
        })
        .collect()
}
