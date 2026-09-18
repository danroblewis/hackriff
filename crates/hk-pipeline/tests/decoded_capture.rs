//! T-092 (SIGNAL-062): always-on decoded-stream capture end to end **through the mock SDR device
//! interface** (a tone recording behind `MockSdrDriver`, looping), never feeding files into the
//! pipeline.
//!
//! - **Recorded automatically:** a recipe pipeline's inspector output appears in the run's
//!   capture store with no request; its stored frames match the frames a live consumer saw
//!   (same seq, t, bytes); frame and time scrubs seek through the index; a replay through the
//!   `inspector?capture=` opener re-parses with the recipe's field map.
//! - **Quota:** segments roll at the per-capture size and the oldest are evicted within the total.
//! - **Never blocks:** a stalled disk drops and counts records while the pipeline keeps producing
//!   frames at full rate.
//!
//! The recipe uses the contract's `identity` block plus a test-only `test_framer` block (as in
//! `recipe_runtime.rs`).

mod common;

use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{TempDir, tone_recording, wait_guarded};
use hk_blocks::{
    Block, BlockError, BlockFactory, BuildCtx, FrameInfo, Io, ParamUpdate, PortInfo, PortSlice,
    PortVec, Registry, Status,
};
use hk_core::{MockEnd, MockOptions, MockSdrDriver, Source};
use hk_pipeline::class::band_class;
use hk_pipeline::recipes::capture::MAX_CAPTURE_REPLAYS;
use hk_pipeline::recipes::runtime::{RecipeRuntime, Target, parse_recipe};
use hk_pipeline::{
    Pipeline, PipelineConfig, PipelineHandle, SourceInfo, TrackInventory, replay_plan,
};
use hk_recipe::{BlockDescriptor, ParamSchema, ParamType, Params, PortSpec, PortType};
use hk_store::decoded::{CaptureQuota, DecodedCaptures};
use hk_stream::inspector::{CaptureInfo, CaptureSource, FitStatus, FrameRecord, RecordedFrames};
use hk_stream::{Declared, OpenRequest};
use serde_json::{Value, json};

const FS: f64 = 1.0e6;
const CENTER_HZ: f64 = 100.0e6;
const TONE_HZ: f64 = CENTER_HZ + 50e3;
const LIMIT: Duration = Duration::from_secs(90);

fn wait(what: &str, limit: Duration, f: impl Fn() -> bool) {
    let deadline = Instant::now() + limit;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

// --- test_framer: a [tag, counter] frame every `every` input samples -----------------------------

struct FramerFactory(BlockDescriptor);

impl BlockFactory for FramerFactory {
    fn descriptor(&self) -> &BlockDescriptor {
        &self.0
    }

    fn build(&self, p: &Params, _ctx: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
        let get = |k: &str, d: u64| p.get(k).and_then(Value::as_u64).unwrap_or(d);
        Ok(Box::new(Framer {
            every: get("every", 1000).max(1),
            tag: get("tag", 1) as u8,
            phase: 0,
            frames: 0,
            status: Status::default(),
        }))
    }
}

struct Framer {
    every: u64,
    tag: u8,
    phase: u64,
    frames: u64,
    status: Status,
}

impl Block for Framer {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let [i] = inputs else {
            return Err(BlockError::Ports("one input".into()));
        };
        Ok(vec![PortInfo {
            ty: PortType::Frames,
            rate_hz: i.rate_hz / self.every as f64,
            max_items: i.max_items / self.every as usize + 2,
            hold_items: self.every as usize,
        }])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let PortSlice::Iq(x) = input.data else {
            return Err(BlockError::Ports("iq input".into()));
        };
        let meta = input.meta;
        let out = io.output(0)?;
        out.meta = hk_blocks::ChunkMeta {
            index: out.meta.index,
            source_per_item: meta.source_per_item * self.every as f64,
            rate_hz: meta.rate_hz / self.every as f64,
            ..meta
        };
        let PortVec::Frames(buf) = &mut out.data else {
            return Err(BlockError::Ports("frames output".into()));
        };
        for k in 0..x.len() {
            self.phase += 1;
            if self.phase >= self.every {
                self.phase = 0;
                let mut info =
                    FrameInfo::new(self.frames, meta.source_index_of(k).round() as u64, 0);
                info.bit_len = 16;
                buf.push(&[self.tag, (self.frames & 0xff) as u8], info);
                self.frames += 1;
            }
        }
        self.status.items_in += x.len() as u64;
        self.status.items_out = self.frames;
        Ok(())
    }

    fn reset(&mut self) {
        self.phase = 0;
    }

    fn update_params(
        &mut self,
        _p: &Params,
        _ctx: &BuildCtx<'_>,
    ) -> Result<ParamUpdate, BlockError> {
        Ok(ParamUpdate::Rebuild)
    }

    fn status(&self) -> Status {
        self.status
    }
}

fn registry() -> Registry {
    let int = |name: &str, default: i64| ParamSchema {
        name: name.into(),
        ty: ParamType::Int {
            min: Some(0),
            max: Some(1 << 20),
        },
        required: false,
        default: Some(json!(default)),
        hot: false,
        doc: String::new(),
    };
    let mut r = Registry::builtin();
    r.register(Arc::new(FramerFactory(hk_blocks::schema::descriptor(
        "test_framer",
        "test",
        "T-092 test block: a [tag, counter] frame every `every` samples",
        vec![PortSpec::new("in", PortType::Iq)],
        vec![PortSpec::new("out", PortType::Frames)],
        vec![
            int("every", 1000),
            int("tag", 1),
            ParamSchema {
                name: "map".into(),
                ty: ParamType::FieldMap,
                required: false,
                default: None,
                hot: true,
                doc: String::new(),
            },
        ],
        true,
    ))))
    .unwrap();
    r
}

fn recipe(id: &str, every: u64) -> Value {
    json!({
        "schema": "hackriff.recipe", "schema_version": 2, "id": id, "version": 1,
        "name": "T-092 tone framer",
        "input": {"port": "iq", "sample_rate_hz": 100000.0, "bandwidth_hz": 20000.0},
        "nodes": [
            {"id": "framer", "block": "test_framer", "params": {"every": every, "tag": 7, "map": "m"}}
        ],
        "field_maps": {"m": {"unit": "bits", "fields": [
            {"name": "tag", "type": "uint", "length": 8},
            {"name": "counter", "type": "uint", "length": 8}
        ]}},
        "outputs": [{"id": "frames", "kind": "inspector", "from": "framer"}],
        "output_policy": {"content_class": "unrestricted"}
    })
}

fn band() -> Target {
    Target::Band {
        f_lo: TONE_HZ - 10e3,
        f_hi: TONE_HZ + 10e3,
    }
}

// --- A run through the mock SDR device ---------------------------------------------------------

struct Run {
    handle: Option<PipelineHandle>,
    _driver: MockSdrDriver,
    rt: Arc<RecipeRuntime>,
    store: DecodedCaptures,
    _dir: TempDir,
}

impl Run {
    fn start(tag: &str) -> Self {
        let dir = TempDir::new(tag);
        let meta = tone_recording(&dir.0.join("rec"), "tone", FS, 1.0, CENTER_HZ, None);
        let driver = MockSdrDriver::new(
            &meta,
            MockOptions {
                end: MockEnd::Loop,
                block_len: 16_384,
                ..MockOptions::default()
            },
        )
        .unwrap();
        let source = driver.open_mock(&driver.default_request()).unwrap();
        let info = SourceInfo {
            sample_rate_hz: source.recording().sample_rate_hz,
            center_hz: source.recording().center_hz,
            start_time: source.start_time(),
        };
        let mut cfg = PipelineConfig::new(
            &dir.0,
            replay_plan(info.center_hz, info.sample_rate_hz, info.start_time),
        )
        .unwrap();
        cfg.source_class = band_class(&[info.center_hz], info.sample_rate_hz);
        cfg.lossless = source.pausable();
        cfg.settings.chains = Some(Vec::new());
        let handle = Pipeline::start(
            cfg,
            Box::new(source),
            info,
            None,
            Box::new(TrackInventory::default()),
        )
        .unwrap();
        let counters = handle.counters();
        wait("the first samples", LIMIT, || {
            counters
                .source
                .samples
                .load(std::sync::atomic::Ordering::Relaxed)
                > 0
        });
        let rt = handle.recipe_runtime();
        rt.set_registry(registry());
        let store = handle
            .decoded_captures()
            .expect("every run has a decoded-capture store");
        assert!(
            store.dir().starts_with(&dir.0),
            "the store lives in the run's data dir"
        );
        Self {
            handle: Some(handle),
            _driver: driver,
            rt,
            store,
            _dir: dir,
        }
    }

    fn frames(&self, id: &str) -> u64 {
        self.rt.stats_json(id).unwrap()["frames"].as_u64().unwrap()
    }

    fn captures_of(&self, pipeline: &str) -> Vec<CaptureInfo> {
        self.store
            .list()
            .unwrap()
            .into_iter()
            .filter(|c| c.pipeline_id == pipeline)
            .collect()
    }

    fn wait_captures_ended(&self, pipeline: &str) {
        wait("the captures to finish", LIMIT, || {
            let c = self.captures_of(pipeline);
            !c.is_empty() && c.iter().all(|c| !c.recording)
        });
    }

    fn finish(mut self) {
        self.rt.stop_all();
        let handle = self.handle.take().unwrap();
        handle.stop();
        let (s, fired) = wait_guarded(handle, Duration::from_secs(120));
        assert!(!fired, "the run stopped within the limit");
        assert!(s.errors.is_empty(), "{:?}", s.errors);
    }
}

/// An in-memory consumer writer.
#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Buf {
    fn frames(&self) -> Vec<FrameRecord> {
        let bytes = self.0.lock().unwrap().clone();
        let Ok(mut r) = RecordedFrames::open(io::Cursor::new(bytes)) else {
            return Vec::new(); // no header yet
        };
        let mut out = Vec::new();
        // A live buffer may end inside a record: keep what decoded.
        while let Ok(Some(f)) = r.next_frame() {
            out.push(f);
        }
        out
    }
}

fn open(rt: &Arc<RecipeRuntime>, params: &[(&str, &str)]) -> hk_stream::OpenedStream {
    let req = OpenRequest {
        params: params
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        peer: "test".into(),
    };
    rt.inspector_service()
        .open(&req)
        .unwrap_or_else(|e| panic!("open {params:?}: {e:?}"))
}

fn read_all(store: &DecodedCaptures, id: &str, from: u64) -> Vec<FrameRecord> {
    let c = store.open_at(id, from).unwrap().unwrap();
    RecordedFrames::open(c.reader)
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn a_pipeline_is_recorded_automatically_and_scrubs_reparses_and_replays_like_live() {
    let run = Run::start("t092-live");
    let doc = recipe("tone", 1000);
    run.rt.save_json(doc.clone()).unwrap();
    let id = run
        .rt
        .start(parse_recipe(doc).unwrap(), band())
        .unwrap_or_else(|e| panic!("start: {e:?}"));

    // A live consumer, attached after the pipeline started.
    let live = Buf::default();
    let opened = open(&run.rt, &[("pipeline", &id)]);
    opened
        .handle
        .subscribe("live", Declared::local(live.clone()), Box::new(|_| {}))
        .unwrap();
    wait("live frames", LIMIT, || live.frames().len() >= 40);

    // Recorded with no request, while recording. The recorder is a separate consumer on its own
    // writer thread: it opens the capture when it decodes the stream header, which is not ordered
    // against what the live consumer above has read. Wait for the capture to exist, then assert
    // there is exactly one (a second would mean the segment rolled).
    wait("the recorder's capture", LIMIT, || {
        !run.captures_of(&id).is_empty()
    });
    let caps = run.captures_of(&id);
    assert_eq!(caps.len(), 1, "{caps:?}");
    let cap = caps[0].clone();
    assert!(cap.recording);
    assert_eq!(
        (
            cap.recipe_id.as_str(),
            cap.recipe_version,
            cap.output_id.as_str()
        ),
        ("tone", 1, "frames")
    );
    assert_eq!(cap.stream_id, format!("inspector/{id}/frames"));

    let produced = stop(&run, &id);
    run.wait_captures_ended(&id);
    let cap = run.store.info(&cap.id).unwrap().unwrap();
    assert_eq!(cap.end_reason.as_deref(), Some("finished"));
    assert_eq!(cap.dropped_records, 0, "{cap:?}");
    assert_eq!(
        cap.frames, produced,
        "every frame the pipeline produced is stored: {cap:?}"
    );

    // Stored frames match what the live consumer saw (the live stream is a suffix).
    let stored = read_all(&run.store, &cap.id, 0);
    assert_eq!(stored.len() as u64, cap.frames);
    assert_eq!(
        stored[0].metadata.frame,
        Some(0),
        "recorded from the first frame"
    );
    let live = live.frames();
    assert!(live.len() >= 40);
    for f in &live {
        let n = f.metadata.frame.unwrap() as usize;
        let s = &stored[n];
        assert_eq!((s.seq, s.t), (f.seq, f.t), "frame {n}");
        assert_eq!(
            s.content.as_ref().unwrap().hex,
            f.content.as_ref().unwrap().hex
        );
        assert!(
            s.content.as_ref().unwrap().layers.is_none(),
            "layers are not stored"
        );
    }

    // Scrub by frame (index seek) and by time (index binary search).
    for k in [1u64, 17, cap.frames - 1] {
        let page = read_all(&run.store, &cap.id, k);
        assert_eq!(page[0].metadata.frame, Some(k));
        assert_eq!(page.len() as u64, cap.frames - k);
        assert_eq!(
            run.store
                .frame_at_time(&cap.id, stored[k as usize].t)
                .unwrap(),
            Some(k)
        );
    }

    // Replay through the opener, re-parsed with the saved recipe's field map, from frame 5.
    let replay = open(
        &run.rt,
        &[
            ("capture", &cap.id),
            ("from_frame", "5"),
            ("field_map", "tone@1:m"),
        ],
    );
    assert_eq!(replay.header.stream_id, format!("capture/{}", cap.id));
    let got = Buf::default();
    let consumer = replay
        .handle
        .subscribe("replay", Declared::local(got.clone()), Box::new(|_| {}))
        .unwrap();
    wait("the replay to finish", LIMIT, || {
        replay
            .handle
            .stats(consumer)
            .is_some_and(|s| matches!(s.state, hk_stream::ConsumerState::Closed(_)))
    });
    let replayed = got.frames();
    assert_eq!(replayed.len() as u64, cap.frames - 5);
    for (r, s) in replayed.iter().zip(&stored[5..]) {
        // `seq` is the replay stream's own; `t` and metadata are the recording's.
        assert_eq!((r.t, r.metadata.frame), (s.t, s.metadata.frame));
        assert_eq!(r.metadata.fit, Some(FitStatus::Ok));
        let layers = r.content.as_ref().unwrap().layers.as_ref().unwrap();
        assert_eq!(layers.node("tag").unwrap().value, Some(json!(7)));
        assert_eq!(
            layers.node("counter").unwrap().value,
            Some(json!(r.metadata.frame.unwrap() & 0xff))
        );
    }
    // Replays are capped: unsubscribed replays hold their slots; beyond the cap, 503 busy until
    // one ends.
    let cap_req = OpenRequest {
        params: vec![("capture".into(), cap.id.clone())],
        peer: "t".into(),
    };
    let mut held = Vec::new();
    let refusal = loop {
        match run.rt.inspector_service().open(&cap_req) {
            Ok(s) => held.push(s),
            Err(e) => break e,
        }
        assert!(held.len() <= MAX_CAPTURE_REPLAYS, "no replay cap");
    };
    assert_eq!((refusal.status, refusal.code.as_str()), (503, "busy"));
    assert!(!held.is_empty());
    drop(held);
    wait("a replay slot to free", LIMIT, || {
        run.rt.inspector_service().open(&cap_req).is_ok()
    });

    // Unknown capture: 404.
    let req = OpenRequest {
        params: vec![("capture".into(), "nope".into())],
        peer: "t".into(),
    };
    assert_eq!(
        run.rt.inspector_service().open(&req).err().unwrap().status,
        404
    );
    run.finish();
}

#[test]
fn segments_roll_and_the_oldest_are_evicted_within_the_quota() {
    let run = Run::start("t092-quota");
    let quota = CaptureQuota {
        total_bytes: 96 * 1024,
        capture_bytes: 24 * 1024,
        queue_bytes: 0,
    };
    run.store.set_quota(quota);
    let id = run
        .rt
        .start(parse_recipe(recipe("tone", 200)).unwrap(), band())
        .unwrap();
    // Enough frames for many segments (each ~24 KiB of ~250-byte records).
    wait("many segments", LIMIT, || {
        assert!(
            run.store.used_bytes() <= quota.total_bytes + quota.capture_bytes,
            "{}",
            run.store.used_bytes()
        );
        run.captures_of(&id).iter().any(|c| c.segment >= 8)
    });
    let produced = stop(&run, &id);
    run.wait_captures_ended(&id);
    let caps = run.captures_of(&id);
    assert!(run.store.used_bytes() <= quota.total_bytes, "{caps:?}");
    let segs: Vec<u32> = caps.iter().map(|c| c.segment).collect();
    assert!(
        !segs.contains(&0),
        "the oldest segment was evicted: {segs:?}"
    );
    // The survivors are the newest segments and contiguous.
    let (lo, hi) = (*segs.iter().min().unwrap(), *segs.iter().max().unwrap());
    assert_eq!((hi - lo + 1) as usize, segs.len(), "{segs:?}");
    // The window ends at the live edge: every survivor but the highest-numbered ended by ROLLING
    // into its successor, and the highest-numbered is the one the stream itself ended on — the
    // segment that was live when recording stopped. So eviction took the oldest, never the newest
    // and never a hole out of the middle. `end_reason` says that directly and, unlike any count
    // of frames, it does not race the recorder.
    let newest = caps.iter().find(|c| c.segment == hi).unwrap();
    for c in caps.iter().filter(|c| c.segment != hi) {
        assert_eq!(c.end_reason.as_deref(), Some("rolled"), "{c:?}");
    }
    assert!(
        matches!(
            newest.end_reason.as_deref(),
            Some("finished" | "drain-timeout")
        ),
        "the newest survivor is the segment the stream ended on: {caps:?}"
    );
    // ...and nothing is lost from its tail silently. "The newest segment ends with the last frame
    // the pipeline produced" is NOT a property of this store, and asserting it was T-451's flake:
    // `produced` counts frames offered to the publisher, and the recorder sits behind that on its
    // own thread as an egress consumer that must never block the pipeline, so it can end short two
    // ways. Its bounded ring (`CaptureQuota::queue_bytes`, floored at `MIN_QUEUE_BYTES` = 2 MiB +
    // 64 KiB ~= 5 800 of these ~374-byte records) DROPS AND COUNTS a frame offered while it is
    // full; and when the stream finishes, a recorder still draining after the publisher's 5 s
    // drain grace is closed, its queue discarded and the capture flagged `drain-timeout`. What the
    // store does guarantee is that neither loss is silent: the gap is counted, or the capture says
    // it was cut short. On the clean path (`finished`) the inequality below is the old equality,
    // exactly, because a tail drop's marker lands in this same newest segment — drops are marked
    // on the next enqueue and, at the end, on drain while the ring is empty, and the writer only
    // ever opens a segment to store a frame, so no segment can open after the last stored frame.
    //
    // MEASURED (T-451). 6 `cargo nextest run -p hk-pipeline` runs at 8 threads: produced 769-991
    // frames, all stored, gap 0, dropped 0 — 6x inside the ring and draining well inside the
    // grace. Injecting latency into the recorder's own file writes walks it through all three
    // regimes, and `produced` with it, because nothing the test controls bounds `produced`: it is
    // (wall time for the recorder to write 8 segments) x (unpaced producer rate), so it stretches
    // with disk latency while the producer, which never waits for the recorder, does not.
    //   3-5 ms  produced  3 939- 6 003  gap 0                dropped 0            finished
    //   7-10 ms produced  7 969-11 393  gap   333-   751     dropped 1 146-1 763  finished
    //   12+ ms  produced 12 745-42 019  gap 6 656-39 775     dropped 0            drain-timeout
    // The run T-448 caught, at 9 788 produced, sits squarely in the middle row — the ring-overrun
    // regime this assertion covers with its counted drops. The old `== produced - 1` fails in both
    // of the lower two rows, reproducibly: 10 ms gave `left: Some(10203) right: Some(10778)`,
    // the same shape as the reported `right: Some(9787)`.
    let last = read_all(&run.store, &newest.id, newest.frames - 1);
    let last_frame = last[0].metadata.frame.expect("a frame record");
    assert!(
        last_frame < produced,
        "stored frame {last_frame} of {produced} produced: {caps:?}"
    );
    let gap = produced - 1 - last_frame;
    assert!(
        newest.end_reason.as_deref() != Some("finished") || gap <= newest.dropped_records,
        "{gap} frames produced after the last stored one (frame {last_frame} of {produced}), the \
         capture says it ended cleanly, and the newest segment counts only {} dropped: {caps:?}",
        newest.dropped_records
    );
    run.finish();
}

/// A stream file whose writes block while `gate` is set.
struct GatedFile {
    file: std::fs::File,
    gate: Arc<AtomicBool>,
}

impl Write for GatedFile {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        while self.gate.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
        self.file.write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[test]
fn a_stalled_disk_drops_and_counts_frames_without_stalling_the_pipeline() {
    let run = Run::start("t092-slow");
    let gate = Arc::new(AtomicBool::new(true));
    let g = Arc::clone(&gate);
    run.store.set_data_writer(Arc::new(move |p: &Path| {
        Ok(Box::new(GatedFile {
            file: std::fs::File::create(p)?,
            gate: Arc::clone(&g),
        }) as Box<dyn Write + Send>)
    }));
    let id = run
        .rt
        .start(parse_recipe(recipe("tone", 20)).unwrap(), band())
        .unwrap();
    // The disk is stalled from the first byte; the recorder's queue (~2 MiB, ~250-byte records)
    // fills after ~8000 frames, and the pipeline keeps producing well past that.
    let started = Instant::now();
    wait("frames past the recorder's queue", LIMIT, || {
        run.frames(&id) >= 14_000
    });
    let stalled_for = started.elapsed();
    gate.store(false, Ordering::SeqCst);
    let stats = run.rt.stop_json(&id).unwrap()["stopped"]["stats"].clone();
    let produced = stats["frames"].as_u64().unwrap();
    let status_ticks = stats["status_ticks"].as_u64().unwrap();
    run.wait_captures_ended(&id);
    let caps = run.captures_of(&id);
    let stored: u64 = caps.iter().map(|c| c.frames).sum();
    let dropped: u64 = caps.iter().map(|c| c.dropped_records).sum();
    assert!(dropped > 0, "{caps:?}");
    assert!(stored > 0, "{caps:?}");
    // Every frame is stored or counted dropped; drop markers also count the status records
    // dropped during the stall.
    assert!(
        (produced..=produced + status_ticks).contains(&(stored + dropped)),
        "stored {stored} + dropped {dropped} vs {produced} frames, {status_ticks} ticks: \
         {caps:?} (stalled {stalled_for:?})"
    );
    run.finish();
}

/// Stops pipeline `id`; its final frame count (a stopped pipeline is no longer listed).
fn stop(run: &Run, id: &str) -> u64 {
    let v = run.rt.stop_json(id).unwrap();
    v["stopped"]["stats"]["frames"].as_u64().unwrap()
}
