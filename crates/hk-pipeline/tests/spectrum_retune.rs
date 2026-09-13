//! T-037a item 4: the spectrum stream header's centre follows a retune. A recording with two
//! captures at different centres replays through the pipeline; the stream sink sees one header
//! per tuned centre, in order.

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use hk_core::Pacing;
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use serde_json::json;

const FS: f64 = 250e3;
const CENTRES: [f64; 2] = [433.5e6, 434.5e6];

fn two_centre_recording(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let per = 60_000usize;
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut data = Vec::with_capacity(4 * per);
    for _ in 0..2 * per {
        for _ in 0..2 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            data.push((((state >> 56) as i64 - 128) / 10) as i8 as u8);
        }
    }
    std::fs::write(dir.join("retune.sigmf-data"), data).unwrap();
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(FS);
    for (k, fc) in CENTRES.iter().enumerate() {
        meta.captures.push(Capture {
            sample_start: (k * per) as u64,
            frequency: Some(*fc),
            datetime: Some(format!("2026-09-13T12:00:0{}Z", k)),
            provenance: None,
            clip_count: None,
            extra: serde_json::Map::new(),
        });
    }
    let path = dir.join("retune.sigmf-meta");
    meta.write(&path).unwrap();
    path
}

#[test]
fn a_retune_offers_the_spectrum_stream_again_with_the_new_centre() {
    let dir = TempDir::new("retune-header");
    let meta = two_centre_recording(&dir.0.join("src"));
    let (mut cfg, replay) = replay_config(&dir.0, &meta, json!({}), Pacing::Unpaced);
    let headers = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&headers);
    let stream_id = cfg.spectrum_stream_id.clone();
    cfg.stream_sink = Some(Arc::new(move |h, _publisher| {
        if h.stream_id == stream_id {
            seen.lock().unwrap().push(h.center_hz);
        }
    }));
    let (summary, fired) = wait_guarded(start(cfg, replay), Duration::from_secs(120));
    eprintln!("{}", summary.to_text());
    assert!(!fired, "the run finished on its own");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    assert_eq!(
        *headers.lock().unwrap(),
        vec![Some(CENTRES[0]), Some(CENTRES[1])],
        "one header per tuned centre"
    );
}
