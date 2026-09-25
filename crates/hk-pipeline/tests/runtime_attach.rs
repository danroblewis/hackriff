//! ADR-0001 S1 outcome through the composed pipeline (T-027): runtime chains attached and detached
//! while a paced (real-time, lossy like a live source) replay runs cause **zero** ring overruns on
//! the always-on readers, and every always-on reader reads every sample the capture thread wrote.
//!
//! **Deterministic under load (T-037a item 6).** The run no longer relies on the default 4 s ring
//! outlasting reader lag on a loaded CI machine: the ring is sized to hold the whole recording
//! (`ring_s` 10 s for a 3 s scene), so the capture thread can never lap a reader however late it
//! is scheduled, while the run stays lossy (no flow gate). Attach/detach effects on the always-on
//! readers are still measured as counted overruns.
//!
//! **No wall clock decides what is asserted (T-917, docs/10 §3.6).** The cycles used to be two
//! 50 ms sleeps each inside a 3 s real-time scene, so how many attaches landed before the replay
//! ended depended on scheduling ("attached 10" of 20 at load 30). Now each attach and each detach
//! is confirmed by its counter before the next step, and the replay's end is **held** until the
//! last cycle is confirmed ([`HoldEnd`]), so every one of the cycles lands on a live segment and
//! the count is exact whatever the load. Nothing here bounds a latency; a slow box only waits
//! longer.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::*;
use hk_core::Pacing;
use hk_core::{BlockHeader, Source, SourceCapabilities, SourceControl, SourceError};
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_pipeline::{Candidate, Pipeline, TrackInventory, builtin_chains};
use num_complex::{Complex, Complex32};
use serde_json::json;

/// A source that reports end-of-stream only once `release` is set: until then it answers "no
/// block yet", as a live radio between blocks does. Every block the inner source has is delivered
/// unchanged and at its own pace.
struct HoldEnd {
    inner: Box<dyn Source>,
    release: Arc<AtomicBool>,
}

impl HoldEnd {
    fn held<T>(&self, h: Option<T>) -> Option<Option<T>> {
        match h {
            Some(h) => Some(Some(h)),
            None if self.release.load(Ordering::SeqCst) => Some(None),
            None => None,
        }
    }
}

impl Source for HoldEnd {
    fn capabilities(&self) -> &SourceCapabilities {
        self.inner.capabilities()
    }
    fn control(&self) -> Arc<dyn SourceControl> {
        self.inner.control()
    }
    fn pausable(&self) -> bool {
        self.inner.pausable()
    }
    fn read_block(
        &mut self,
        samples: &mut Vec<Complex32>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        loop {
            let h = self.inner.read_block(samples)?;
            if let Some(h) = self.held(h) {
                return Ok(h);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn read_block_ci8(
        &mut self,
        samples: &mut Vec<Complex<i8>>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        loop {
            let h = self.inner.read_block_ci8(samples)?;
            if let Some(h) = self.held(h) {
                return Ok(h);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Waits for `c` to reach `n`. The limit only turns a wedge into a failure; it never decides a
/// count.
fn reach(what: &str, c: &AtomicU64, n: u64) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while c.load(Ordering::SeqCst) < n {
        assert!(
            Instant::now() < deadline,
            "{what}: {} of {n}",
            c.load(Ordering::SeqCst)
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

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
    let release = Arc::new(AtomicBool::new(false));
    let handle = Pipeline::start(
        cfg,
        Box::new(HoldEnd {
            inner: Box::new(replay.source),
            release: Arc::clone(&release),
        }),
        replay.info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let counters = handle.counters();
    let chains = &counters.chains;
    let spec = builtin_chains()
        .into_iter()
        .find(|s| s.id == "fsk-bursts")
        .unwrap();
    let cycles = 20;
    for n in 1..=cycles {
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
        reach("attached", &chains.attached, n);
        handle.detach_manual_chains();
        reach("detached", &chains.detached, n);
    }
    release.store(true, Ordering::SeqCst);
    let s = handle.wait().unwrap();
    eprintln!("{}", s.to_text());
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    let attached = s.counter("/chains/attached");
    assert_eq!(attached, cycles, "every cycle attached on a live segment");
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
