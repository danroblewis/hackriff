//! Legal guardrail (T-027 review fix): an **untagged** recording (no `hackriff:content_class`)
//! in a paging or cellular allocation gets its restricted class from frequency (47 CFR 22.531 /
//! 90.494 / 24.129 paging, 22.905 cellular; `hk_pipeline::class::restricted_bands`), and a user
//! classification rule that tries to open the FSK emitter's content is clamped. The sensor is
//! still detected, tracked, demodulated and framed (metadata flows, CRCs check), but no Decode
//! keeps content, nothing is recorded, and none of the payload sentinels appear in the data
//! directory (database, WAL, tiles, recordings) or in any stream a consumer received. The
//! positive control (the same scene classified under a fail-closed band stores its content) is
//! `aware_036_pipeline.rs`; broadcast FM staying unrestricted is `class.rs`'s unit test and
//! SIGNAL-062.

mod common;

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use hk_core::Pacing;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::sigmf::SigmfMeta;
use hk_stream::Declared;
use serde_json::json;

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn assert_no_content(meta: &Path, payload_hex: &[String], center_hz: f64, class: &str) {
    let m = SigmfMeta::read(meta).unwrap();
    assert!(
        !m.global.extra.contains_key("hackriff:content_class"),
        "the recording must be untagged"
    );
    let sent = sentinels(payload_hex);
    let dir = TempDir::new(class);
    let (mut cfg, replay) = replay_config(
        &dir.0,
        meta,
        json!({ "pipeline": { "classify": [{
            "freq_hz": [center_hz - 1e6, center_hz + 1e6],
            "content_class": "unrestricted",
            "by": "test: a rule that tries to open a restricted band"
        }] } }),
        Pacing::Unpaced,
    );
    let streams = Buf::default();
    let sink = streams.clone();
    cfg.stream_sink = Some(Arc::new(move |h, handle| {
        let r = handle.subscribe(
            "sentinel-scan",
            Declared::local(sink.clone()),
            Box::new(|_| {}),
        );
        eprintln!("subscribed to {}: {:?}", h.stream_id, r.is_ok());
    }));
    let (s, fired) = wait_guarded(start(cfg, replay), Duration::from_secs(600));
    eprintln!("{}", s.to_text());
    assert!(!fired, "the run did not finish; the watchdog stopped it");
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    assert_eq!(s.source_class, class, "derived from frequency");
    assert!(s.counter("/detect/detections") > 0, "metadata still flows");
    assert!(
        s.counter("/chains/attached") >= 1,
        "the FSK chain still runs"
    );
    let decodes = s.counter("/chains/decodes");
    assert!(
        decodes > 0,
        "bursts were framed (there was content to withhold)"
    );
    assert!(s.counter("/chains/crc_valid") > 0);
    assert_eq!(
        s.counter("/chains/content_withheld"),
        decodes,
        "no Decode keeps content"
    );
    assert_eq!(s.counter("/chains/recordings"), 0, "no recording");
    let mut hay = all_bytes(&dir.0);
    let stream_bytes = streams.0.lock().unwrap().clone();
    assert!(
        !stream_bytes.is_empty(),
        "the spectrum stream reached the consumer"
    );
    hay.extend_from_slice(&stream_bytes);
    assert_eq!(
        count_found(&hay, &sent),
        0,
        "payload sentinels found in the data directory or streams"
    );
}

fn payloads(truths: &[&hk_e2e::TruthItem]) -> Vec<String> {
    truths
        .iter()
        .map(|t| t.value["frame"]["payload_hex"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn untagged_recording_in_the_930_mhz_paging_band_yields_no_content_despite_a_rule() {
    let out = synth_or_skip!(
        SynthRequest::new("fsk_burst_train")
            .seed(36)
            .param("snr_db", 20.0)
            .param("duration_s", 1.2)
            .param("center_hz", 930.0e6)
    );
    let fx = out.fixture(0).unwrap();
    let p = payloads(&fx.of_kind("fsk-burst"));
    assert!(!p.is_empty());
    assert_no_content(&fx.meta_path, &p, 930.0e6, "restricted-paging");
}

#[test]
fn untagged_recording_in_the_880_mhz_cellular_band_yields_no_content_despite_a_rule() {
    let out = synth_or_skip!(
        SynthRequest::new("fsk_burst_train")
            .seed(36)
            .param("snr_db", 20.0)
            .param("duration_s", 1.2)
            .param("center_hz", 880.0e6)
    );
    let fx = out.fixture(0).unwrap();
    let p = payloads(&fx.of_kind("fsk-burst"));
    assert!(!p.is_empty());
    assert_no_content(&fx.meta_path, &p, 880.0e6, "restricted-cellular");
}
