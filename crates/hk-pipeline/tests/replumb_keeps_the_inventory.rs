//! **T-941 — a class-boundary retune never leaves the run without an inventory.**
//!
//! The live report (2026-09-25, HackRF in SF): a retune from 100.9 MHz to 162.2 MHz — unrestricted
//! to metadata-only, so a re-plumb — logged
//!
//! ```text
//! hk-detect did not stop within 8s of its segment ending …
//! hk-control did not stop within 8s …
//! segment state still held 5 s after its threads ended (26 holders); salvaging it
//! the old segment's inventory is locked by its straggler; continuing without inventory
//! ```
//!
//! and from then on `detect.detections_written` kept climbing (12 487 → 15 619 in 20 s) while
//! `tracks_opened` never moved again and `/api/inventory` answered `total 0`. Detection was
//! running and writing rows; tracks, candidates and the inventory were gone, every surface said
//! "Nothing on the air", and only a restart brought them back.
//!
//! The mechanism was one line of ownership: the inventory belonged to the **segment**, so a
//! re-plumb had to *move* it out of the old segment's state — and when a straggler held that state
//! and the inventory's mutex (a thread still inside an `Inventory` call is exactly that),
//! `take_parts` took the inventory from under it and left the run a `NullInventory`, for ever. It
//! is the run's now (`run::Common::inventory`), so there is nothing to move and nothing to lose.
//!
//! What is asserted here, through the mock SDR (CLAUDE.md: e2e goes through the device contract):
//!
//! 1. **The re-plumb still completes** on a salvaged segment and capture comes back — T-508's
//!    guarantee, unchanged.
//! 2. **The run's own inventory keeps receiving the run's events afterwards.** The inventory the
//!    run was started with counts what it is given; after the class-boundary re-plumb it must see
//!    the *new* segment's events. This is the assertion that goes red on the old code: the new
//!    segment got a `NullInventory` and the run's inventory was never called again.
//! 3. **Tracks keep opening**, so the other half of the reported symptom (`tracks_opened` frozen
//!    while detections were still being written) cannot come back unnoticed.
//!
//! The straggler is reproduced **deterministically** by `PipelineHandle::hold_inventory` — a
//! thread that holds the running segment's state *and* its inventory lock, and returns only once
//! it really holds them. Nothing here reads a wall clock to choose what to assert.
//!
//! Measured on the pre-fix code (the inventory moved through `Parts`, `take_parts` substituting a
//! `NullInventory` after its 1 s `try_lock`): assertion 2 fails with the run's inventory stuck at
//! the first segment's single event, while 1 and 3 pass — the shape of the report exactly.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::{TempDir, tone_recording};
use hk_core::{MockEnd, MockOptions, MockSdrDriver, Pacing};
use hk_detect::TrackEvent;
use hk_detect::track::TrackSummary;
use hk_model::{
    ContentClass, DemodulationId, EmitterId, RepoError, Repository, Timestamp, TrackId,
};
use hk_pipeline::class::window_class;
use hk_pipeline::replay_plan;
use hk_pipeline::stats::Counters;
use hk_pipeline::{
    Inventory, Pipeline, PipelineConfig, PipelineController, PipelineHandle, SourceInfo,
    TrackInventory,
};
use serde_json::json;

const LIMIT: Duration = Duration::from_secs(60);

/// The recording's window, wholly inside FM broadcast (87.5–108 MHz): the run's class is
/// `unrestricted`.
const FROM: (f64, f64) = (107.4e6, 1.0e6);
/// A window that reaches **past 108 MHz**, so it is in no positively-chosen band and its class is
/// the fail-closed `metadata-only`. Same rate, so the **class boundary alone** forces the
/// re-plumb, as it did on the report's 100.9 → 162.2 MHz move.
///
/// It is 200 kHz away rather than 60 MHz for one reason: the mock SDR is an honest radio, and a
/// window with nothing recorded in it is served as **calibrated noise** (`mock::dsp::Coverage::
/// Noise`). Tuned 60 MHz off the recording there is no signal to detect, so "tracks keep opening"
/// would be measuring the fixture, not the pipeline. Here the recorded tone (107.45 MHz) is inside
/// both windows, so detection has the same thing to find before and after.
const TO: (f64, f64) = (107.6e6, 1.0e6);

/// What the run's inventory has been given. Counting is all this needs to do: the question is
/// whether the live run is still talking to *this* inventory after the re-plumb.
#[derive(Default)]
struct Calls {
    capture_names: AtomicU64,
    events: AtomicU64,
}

impl Calls {
    fn total(&self) -> u64 {
        self.capture_names.load(Ordering::SeqCst) + self.events.load(Ordering::SeqCst)
    }
}

/// The run's real inventory, counting what reaches it. Every call is delegated, so the run behaves
/// exactly as it does in production (the entries, the track→emitter bindings, the live offers).
struct Counting {
    inner: TrackInventory,
    calls: Arc<Calls>,
}

impl Inventory for Counting {
    fn capture_name(&mut self, name: &str) {
        self.calls.capture_names.fetch_add(1, Ordering::SeqCst);
        self.inner.capture_name(name);
    }

    fn track_event(&mut self, repo: &mut Repository, event: &TrackEvent) -> Result<(), RepoError> {
        self.calls.events.fetch_add(1, Ordering::SeqCst);
        self.inner.track_event(repo, event)
    }

    fn chain_emitter(
        &mut self,
        repo: &mut Repository,
        track: Option<TrackId>,
        emitter: EmitterId,
    ) -> Result<(), RepoError> {
        self.calls.events.fetch_add(1, Ordering::SeqCst);
        self.inner.chain_emitter(repo, track, emitter)
    }

    fn chain_measurement(
        &mut self,
        repo: &mut Repository,
        track: Option<TrackId>,
        demod: DemodulationId,
        at: Timestamp,
    ) -> Result<(), RepoError> {
        self.calls.events.fetch_add(1, Ordering::SeqCst);
        self.inner.chain_measurement(repo, track, demod, at)
    }

    fn live_track(
        &mut self,
        repo: &mut Repository,
        summary: &TrackSummary,
    ) -> Result<(), RepoError> {
        self.calls.events.fetch_add(1, Ordering::SeqCst);
        self.inner.live_track(repo, summary)
    }

    fn live_trust(
        &mut self,
        repo: &mut Repository,
        summary: &TrackSummary,
        measuring: bool,
    ) -> Result<(), RepoError> {
        self.calls.events.fetch_add(1, Ordering::SeqCst);
        self.inner.live_trust(repo, summary, measuring)
    }

    fn emitter_of_track(&self, track: TrackId) -> Option<EmitterId> {
        self.inner.emitter_of_track(track)
    }

    fn recorded_emitter_of_track(&self, track: TrackId) -> Option<EmitterId> {
        self.inner.recorded_emitter_of_track(track)
    }
}

fn captured(c: &Counters) -> u64 {
    c.source.samples.load(Ordering::Relaxed)
}

fn stat(controller: &PipelineController, key: &str) -> u64 {
    controller.status().stats[key].as_u64().unwrap_or(0)
}

fn wait_captured(handle: &PipelineHandle, from: u64, n: u64, why: &str) {
    let counters = handle.counters();
    let deadline = Instant::now() + LIMIT;
    while captured(&counters) < from + n {
        assert!(
            Instant::now() < deadline,
            "{why}: {} samples in {LIMIT:?}; run state {:?}",
            captured(&counters) - from,
            handle.controller().status()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Waits until the run's inventory has been told at least `n` capture names.
///
/// **One per segment**, and the one thing a *straggler* of the old segment cannot produce a second
/// of: its own name was consumed when it filed it (`CaptureNamer::take`). So "the run's inventory
/// has been told two capture names" means the run's inventory was reached by the **new segment**,
/// which is exactly the question — the old code left the new segment a `NullInventory` while the
/// straggler went on writing into the real one, so counting *any* activity would not tell them
/// apart.
fn wait_capture_names(calls: &Calls, n: u64, why: &str) {
    let deadline = Instant::now() + LIMIT;
    while calls.capture_names.load(Ordering::SeqCst) < n {
        assert!(
            Instant::now() < deadline,
            "{why}: the run's inventory was told {} capture name(s) in {LIMIT:?}, wanted {n} \
             (it was given {} thing(s) in all)",
            calls.capture_names.load(Ordering::SeqCst),
            calls.total()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Waits until the detector has opened more than `from` tracks.
fn wait_tracks(handle: &PipelineHandle, from: u64, why: &str) {
    let counters = handle.counters();
    let deadline = Instant::now() + LIMIT;
    while counters.detect.tracks_opened.load(Ordering::Relaxed) <= from {
        assert!(
            Instant::now() < deadline,
            "{why}: tracks_opened stuck at {} for {LIMIT:?} (detections_written {}); run state {:?}",
            counters.detect.tracks_opened.load(Ordering::Relaxed),
            counters.detect.detections_written.load(Ordering::Relaxed),
            handle.controller().status()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A real-time run of the mock SDR over a looping tone recording at [`FROM`], with `calls`'
/// inventory as the run's.
fn mock_run(dir: &TempDir, calls: Arc<Calls>) -> PipelineHandle {
    let meta = tone_recording(&dir.0.join("rec"), "tone", FROM.1, 2.0, FROM.0, None);
    let driver = MockSdrDriver::new(
        &meta,
        MockOptions {
            end: MockEnd::Loop,
            block_len: 16_384,
            pacing: Pacing::RealTime { speed: 1.0 },
            ..MockOptions::default()
        },
    )
    .unwrap();
    let source = driver.open_mock(&driver.default_request()).unwrap();
    let info = SourceInfo {
        sample_rate_hz: FROM.1,
        center_hz: FROM.0,
        start_time: source.start_time(),
    };
    let mut plan = replay_plan(FROM.0, FROM.1, info.start_time);
    plan.extra = json!({ "pipeline": { "ring_s": 0.5 } });
    let mut cfg = PipelineConfig::new(&dir.0, plan).unwrap();
    cfg.source_class = window_class(FROM.0, FROM.1);
    cfg.live_window_class = true;
    cfg.settings.chains = Some(Vec::new());
    let inventory = Counting {
        inner: TrackInventory::default(),
        calls,
    };
    Pipeline::start(cfg, Box::new(source), info, None, Box::new(inventory)).unwrap()
}

#[test]
fn a_class_boundary_retune_hands_the_inventory_to_the_new_segment() {
    assert_eq!(
        (window_class(FROM.0, FROM.1), window_class(TO.0, TO.1),),
        (ContentClass::Unrestricted, ContentClass::MetadataOnly),
        "this test is about a class boundary; these two windows must be either side of one"
    );
    let dir = TempDir::new("t941-inventory-handover");
    let calls = Arc::new(Calls::default());
    let handle = mock_run(&dir, Arc::clone(&calls));
    let controller = handle.controller();
    wait_captured(&handle, 0, 200_000, "the run never warmed up");
    // The first segment is talking to the inventory the run was started with.
    wait_capture_names(
        &calls,
        1,
        "the run's inventory was never told the first segment's capture name",
    );
    wait_tracks(&handle, 0, "the tone never produced a track");
    let tracks_before = handle
        .counters()
        .detect
        .tracks_opened
        .load(Ordering::Relaxed);

    // The straggler of the report: a thread of the old segment holding its state *and* the
    // inventory's lock, as one caught inside an `Inventory` call does.
    let hold = handle
        .hold_inventory()
        .expect("the run's inventory could be held");

    let at_retune = captured(&handle.counters());
    let out = controller
        .retune(TO.0, TO.1)
        .expect("a class-boundary retune with the old segment's state held");
    assert!(
        out.replumbed && out.content_class == ContentClass::MetadataOnly,
        "the class boundary was crossed by a re-plumb: {out:?}"
    );
    assert_eq!(
        stat(&controller, "segments_salvaged"),
        1,
        "the salvage path is what this test is about; if this is 0 the straggler was not held"
    );

    // The straggler finishes its call and lets go, as a real one does. Nothing is asserted about
    // how long it took: what matters is that the run picks its inventory back up afterwards
    // instead of having been detached from it for ever.
    drop(hold);

    wait_capture_names(
        &calls,
        2,
        "THIS IS T-941: after a class-boundary re-plumb whose old segment was held by a straggler, \
         the new segment never reached the live run's inventory — it was handed a NullInventory, \
         so detection keeps writing rows while the inventory, candidates and confirmed lists stay \
         empty for the rest of the run",
    );
    wait_captured(
        &handle,
        at_retune,
        200_000,
        "capture never resumed after the re-plumb",
    );
    wait_tracks(
        &handle,
        tracks_before,
        "no track opened after the re-plumb (the reported symptom: detections_written rising, \
         tracks_opened frozen)",
    );

    let st = controller.status();
    assert!(!st.finished, "{st:?}");
    assert_eq!((st.center_hz, st.sample_rate_hz), TO, "{st:?}");
    assert_eq!(
        st.content_class,
        ContentClass::MetadataOnly,
        "the new segment runs under the window's own class: {st:?}"
    );
    // T-941: the abandonment is a **served** fact, not only a line on stderr — and this run
    // produces one, which is the report's coupling reproduced. `hk-detect` cannot finish while the
    // inventory is held: its segment's end closes the open track, and filing that close needs the
    // lock. So it is still running when the re-plumb's bound expires and is left behind, exactly as
    // the log says ("hk-detect did not stop within 8s of its segment ending"). What has changed is
    // the consequence: the run kept its inventory and picked up again by itself.
    assert!(
        stat(&controller, "workers_abandoned") >= 1,
        "the straggler should have held hk-detect past the re-plumb's bound: {:?}",
        controller.status()
    );

    handle.stop();
    handle.wait().unwrap();
}
