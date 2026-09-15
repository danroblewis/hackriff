//! AWARE-036 through the composed pipeline (T-027): the `fsk_burst_train` synthetic sensor
//! (2-FSK 4800 Bd, preamble + sync 2DD4 + 48-bit payload + CRC-16, bursts every 120 ms) replayed
//! by `hk replay`'s path. Detections → one confirmed track → the `fsk-bursts` chain attached from
//! the registry by priors (bursty, 2–200 kHz) → per-burst blind estimate and demodulation →
//! framing with CRC → Decode rows, whose payloads are stored.

mod common;

use common::*;
use hk_e2e::{SynthRequest, synth_or_skip};
use serde_json::json;

const AWARE_036: &str = "AWARE-036";

#[test]
fn aware_036_fsk_bursts_detected_tracked_decoded_and_stored() {
    let out = synth_or_skip!(
        SynthRequest::new("fsk_burst_train")
            .seed(36)
            .param("snr_db", 20.0)
            .param("duration_s", 2.4)
    );
    let fx = out.fixture(0).unwrap();
    let truths = fx.of_kind("fsk-burst");
    assert!(truths.len() >= 15, "[{AWARE_036}] {} bursts", truths.len());
    let payloads: Vec<String> = truths
        .iter()
        .map(|t| t.value["frame"]["payload_hex"].as_str().unwrap().to_owned())
        .collect();
    let sent = sentinels(&payloads);

    let dir = TempDir::new("aware036");
    let s = run(&dir.0, &fx.meta_path, json!({}));
    let n = truths.len() as u64;
    assert_eq!(s.always_on_lost_samples, 0);
    assert!(
        s.counter("/detect/detections") > 0,
        "[{AWARE_036}] no detections"
    );
    assert!(s.detections_stored > 0);
    // The sensor's tone lobes aggregate into one track; single-lobe fragments may add short
    // extra tracks (T-031 follow-up), which the fsk-bursts spec's min_detections prior (4) keeps
    // from attaching chains.
    assert!(
        s.counter("/detect/tracks_confirmed") >= 1,
        "[{AWARE_036}] no confirmed track"
    );
    assert_eq!(
        s.counter("/chains/attached"),
        1,
        "[{AWARE_036}] exactly one fsk chain"
    );
    assert_eq!(s.counter("/chains/detached"), 1);
    let bursts = s.counter("/chains/fsk_bursts");
    let crc = s.counter("/chains/crc_valid");
    eprintln!("[{AWARE_036}] {bursts} bursts demodulated, {crc} CRC-valid of {n} truth bursts");
    assert!(bursts * 10 >= n * 8, "[{AWARE_036}] bursts {bursts} of {n}");
    assert!(crc * 10 >= n * 8, "[{AWARE_036}] CRC-valid {crc} of {n}");
    let decodes = s.counter("/chains/decodes");
    assert!(decodes >= crc, "[{AWARE_036}] decodes {decodes}");
    let found = count_found(&all_bytes(&dir.0), &sent);
    assert!(found > 0, "[{AWARE_036}] payloads are stored");

    // With a user classification rule (own test sensor) the same run stores the payloads too.
    let dir2 = TempDir::new("aware036c");
    let s2 = run(
        &dir2.0,
        &fx.meta_path,
        json!({ "pipeline": { "classify": [{
            "freq_hz": [433.8e6, 434.1e6],
            "content_class": "unrestricted",
            "by": "test: own synthetic AWARE-036 sensor"
        }] } }),
    );
    assert!(s2.counter("/chains/decodes") > 0);
    assert_eq!(s2.counter("/chains/content_withheld"), 0, "[{AWARE_036}]");
    let found = count_found(&all_bytes(&dir2.0), &sent);
    assert!(found > 0, "[{AWARE_036}] classified payloads are stored");
}
