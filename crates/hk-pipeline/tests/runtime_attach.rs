//! ADR-0001 S1 outcome through the composed pipeline (T-027): runtime chains attached and detached
//! while a paced (real-time, lossy like a live source) replay runs cause **zero** ring overruns on
//! the always-on readers, and every always-on reader reads every sample the capture thread wrote.
//!
//! **Deterministic under load (T-037a item 6).** The run no longer relies on the default 4 s ring
//! outlasting reader lag on a loaded CI machine: the ring is sized to hold the whole recording
//! (`ring_s` 10 s for a 3 s scene), so the capture thread can never lap a reader however late it
//! is scheduled, while the run stays lossy (no flow gate). Attach/detach effects on the always-on
//! readers are still measured as counted overruns.

mod common;

use std::time::Duration;

use common::*;
use hk_core::Pacing;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_pipeline::{Candidate, builtin_chains};
use serde_json::json;

#[test]
fn chains_attached_and_detached_mid_run_drop_no_always_on_samples() {
    let out = synth_or_skip!(
        SynthRequest::new("occupancy_multi_hour")
            .seed(5)
            .param("windows", 1)
            .param("window_duration_s", 3.0)
    );
    let fx = out.fixture(0).unwrap();
    let dir = TempDir::new("attach");
    const RING_S: f64 = 10.0;
    let (cfg, replay) = replay_config(
        &dir.0,
        &fx.meta_path,
        json!({ "pipeline": { "ring_s": RING_S } }),
        Pacing::RealTime { speed: 1.0 },
    );
    assert!(!cfg.lossless, "paced replay runs without backpressure");
    let ring_capacity = (replay.info.sample_rate_hz * RING_S) as u64;
    let center = replay.info.center_hz;
    let handle = start(cfg, replay);
    let spec = builtin_chains()
        .into_iter()
        .find(|s| s.id == "fsk-bursts")
        .unwrap();
    let cycles = 20;
    for _ in 0..cycles {
        std::thread::sleep(Duration::from_millis(50));
        let at = handle.ring_position().saturating_sub(4096);
        handle.attach_chain(
            spec.clone(),
            Candidate {
                track: None,
                detection: None,
                f_lo_hz: center - 5e3,
                f_hi_hz: center + 5e3,
                first_sample: at,
                trigger_sample: at,
                bursty: Some(true),
            },
        );
        std::thread::sleep(Duration::from_millis(50));
        handle.detach_manual_chains();
    }
    let s = handle.wait().unwrap();
    eprintln!("{}", s.to_text());
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    let attached = s.counter("/chains/attached");
    assert!(attached >= cycles - 2, "attached {attached}");
    assert_eq!(s.counter("/chains/detached"), attached);
    assert!(s.counter("/chains/samples") > 0, "chains read the ring");
    let written = s.counter("/source/samples");
    assert!(
        written <= ring_capacity,
        "the ring holds the whole run ({written} <= {ring_capacity}), so no reader can be lapped"
    );
    for r in ["detect", "history", "spectrum"] {
        assert_eq!(s.counter(&format!("/readers/{r}/lost_samples")), 0, "{r}");
        assert_eq!(s.counter(&format!("/readers/{r}/overruns")), 0, "{r}");
        assert_eq!(
            s.counter(&format!("/readers/{r}/samples")),
            written,
            "{r} read every sample"
        );
    }
    assert_eq!(s.always_on_lost_samples, 0);
}
