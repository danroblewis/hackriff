//! Legal guardrail through the composed pipeline (T-027, ADR-0004): a source whose class is
//! `restricted-paging` yields no content in any output, even when a user rule tries to classify
//! the emitter as unrestricted. The FSK sensor is still detected, tracked, demodulated and framed
//! (metadata flows), but no Decode keeps content, no recording is written, and none of the
//! payload sentinels appear anywhere in the data directory. The positive control (the same
//! scene, classified, under a fail-closed band) is in `aware_036_pipeline.rs`.

mod common;

use common::*;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::sigmf::SigmfMeta;
use serde_json::json;

#[test]
fn restricted_source_class_yields_no_content_anywhere() {
    let out = synth_or_skip!(
        SynthRequest::new("fsk_burst_train")
            .seed(36)
            .param("snr_db", 20.0)
            .param("duration_s", 1.2)
    );
    let fx = out.fixture(0).unwrap();
    let payloads: Vec<String> = fx
        .of_kind("fsk-burst")
        .iter()
        .map(|t| t.value["frame"]["payload_hex"].as_str().unwrap().to_owned())
        .collect();
    let sent = sentinels(&payloads);

    // A copy of the scene tagged restricted-paging.
    let src = TempDir::new("legal-src");
    let meta_path = src.0.join("restricted.sigmf-meta");
    let mut meta = SigmfMeta::read(&fx.meta_path).unwrap();
    meta.global.extra.insert(
        "hackriff:content_class".into(),
        serde_json::Value::String("restricted-paging".into()),
    );
    meta.write(&meta_path).unwrap();
    std::fs::copy(fx.data_path(), src.0.join("restricted.sigmf-data")).unwrap();

    let dir = TempDir::new("legal");
    let s = run(
        &dir.0,
        &meta_path,
        json!({ "pipeline": { "classify": [{
            "freq_hz": [433.8e6, 434.1e6],
            "content_class": "unrestricted",
            "by": "test: an attempt to open a restricted source"
        }] } }),
    );
    assert_eq!(s.source_class, "restricted-paging");
    assert!(s.counter("/detect/detections") > 0, "metadata still flows");
    assert!(
        s.counter("/chains/attached") >= 1,
        "the FSK chain still runs"
    );
    let decodes = s.counter("/chains/decodes");
    assert_eq!(
        s.counter("/chains/content_withheld"),
        decodes,
        "no Decode keeps content"
    );
    assert_eq!(s.counter("/chains/recordings"), 0, "no recording");
    let found = count_found(&all_bytes(&dir.0), &sent);
    assert_eq!(found, 0, "payload sentinels found in the data directory");
}
