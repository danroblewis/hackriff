//! T-093 (SIGNAL-062): one recipe pipeline spanning several channels (`follow_hops`), end to end
//! **through the mock SDR device interface** (a synthetic multi-channel burst net behind
//! `MockSdrDriver`, looping, lossless), never feeding files into the pipeline.
//!
//! The scene is a net of four 12.5 kHz channels carrying tone bursts; a burst's length is its
//! message (`code × 8 ms`). The truth list stays in the test: the pipeline sees only IQ. The
//! same message is sent on two adjacent channels 3 ms apart, strong on one and weak on the other.
//! A test-only `test_burst` block (energy detector: frame = `[code]`, one corrected bit when weak)
//! runs once per channel; `follow_hops` merges.
//!
//! Asserted per complete recording loop: every message decoded exactly once, tagged with the
//! channel it was sent on (index and `channel_hz`), frames in end order across channels (a
//! message is complete only at its end, T-107; including a short burst that ends before an
//! earlier, longer one on another channel), none counted `late`, the duplicate removed with the
//! strong copy kept and counted, and a channel added mid-run followed from then on without a gap
//! on the others.
//!
//! T-107 also covers the other channel sources end to end: the blind detections in a band (from a
//! survey run's own detector and tracker) and a hop set the tracker found from a hopping net's IQ;
//! the test inserts neither.

mod common;

use std::io::{Cursor, Write};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{TempDir, wait_guarded};
use hk_api::StreamRegistry;
use hk_blocks::{
    Block, BlockError, BlockFactory, BuildCtx, ChunkFlags, ChunkMeta, FrameInfo, Io, ParamUpdate,
    PortInfo, PortSlice, PortVec, Registry, Status,
};
use hk_core::{MockEnd, MockOptions, MockSdrDriver, Source};
use hk_detect::track::HopSetSummary;
use hk_detect::track::inventory::hop_set_sighting;
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use hk_model::{CrcStatus, EmitterId, FreqRange, InventoryQuery, TimeRange, Timestamp, TrackId};
use hk_pipeline::class::band_class;
use hk_pipeline::recipes::runtime::{RecipeRuntime, Target, parse_recipe};
use hk_pipeline::{
    Pipeline, PipelineConfig, PipelineHandle, SourceInfo, TrackInventory, replay_plan,
};
use hk_recipe::{BlockDescriptor, Params, PortSpec, PortType};
use hk_stream::{Declared, Record, StreamReader};
use serde_json::{Value, json};

const FS: f64 = 500_000.0;
const CENTER_HZ: f64 = 100.0e6;
const SECS: f64 = 2.0;
const LOOP: u64 = (FS * SECS) as u64;
const LIMIT: Duration = Duration::from_secs(120);
/// Channel centres: A, B, C adjacent at 25 kHz spacing; D added mid-run.
const CHANNELS: [f64; 4] = [
    CENTER_HZ - 75e3,
    CENTER_HZ - 50e3,
    CENTER_HZ - 25e3,
    CENTER_HZ + 50e3,
];
const DUP_CODE: u8 = 7;

/// Hidden truth: `(channel, start s, code, strong)`; length = code × 8 ms.
const TRUTH: &[(usize, f64, u8, bool)] = &[
    (0, 0.20, 3, true),
    (0, 0.60, 4, true),
    (0, 1.20, 12, true),
    (1, 0.25, 6, true),
    (1, 1.00, DUP_CODE, true),
    (1, 1.60, 8, true),
    (2, 0.30, 9, true),
    (2, 1.003, DUP_CODE, false),
    // Starts after A's 96 ms burst at 1.20 s and ends before it: complete first, leaves first.
    (2, 1.22, 5, true),
    (3, 0.40, 11, true),
    (3, 0.90, 10, true),
    (3, 1.70, 13, true),
];

fn wait(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + LIMIT;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

// --- The scene ---------------------------------------------------------------------------------

fn scene(dir: &std::path::Path) -> std::path::PathBuf {
    let bursts: Vec<(f64, f64, f64, f64)> = TRUTH
        .iter()
        .map(|&(ch, start, code, strong)| {
            let amp = if strong { 60.0 } else { 20.0 };
            (CHANNELS[ch], start, f64::from(code) * 0.008, amp)
        })
        .collect();
    write_scene(dir, &bursts)
}

/// Length of one hop of the hopping net (T-107): the burst decoder reads it as message 4.
const HOP_DWELL_S: f64 = 0.034;

/// A net hopping over the four channels (T-107): contiguous 34 ms dwells in a scrambled order
/// (never the same channel twice in a row) from 0.1 s to 1.29 s of each loop, then silence.
fn hopper_scene(dir: &std::path::Path) -> std::path::PathBuf {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut prev = 0usize;
    let bursts: Vec<(f64, f64, f64, f64)> = (0..35)
        .map(|k| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let ch = (prev + 1 + (state % 3) as usize) % 4;
            prev = ch;
            (
                CHANNELS[ch],
                0.1 + f64::from(k) * HOP_DWELL_S,
                HOP_DWELL_S,
                60.0,
            )
        })
        .collect();
    write_scene(dir, &bursts)
}

/// Writes one loop of tone bursts `(channel Hz, start s, length s, amplitude)` (each tone 1 kHz
/// above its channel centre, 2 ms raised-cosine edges) plus noise as a ci8 SigMF recording.
fn write_scene(dir: &std::path::Path, bursts: &[(f64, f64, f64, f64)]) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let n = LOOP as usize;
    let mut re = vec![0f64; n];
    let mut im = vec![0f64; n];
    let ramp = (0.002 * FS) as usize;
    for &(ch_hz, start, len_s, amp) in bursts {
        let f = ch_hz - CENTER_HZ + 1_000.0;
        let (s0, len) = ((start * FS).round() as usize, (len_s * FS).round() as usize);
        for i in 0..len {
            let edge = i.min(len - 1 - i);
            let w = if edge < ramp {
                0.5 - 0.5 * (std::f64::consts::PI * edge as f64 / ramp as f64).cos()
            } else {
                1.0
            };
            let ph = 2.0 * std::f64::consts::PI * f * (s0 + i) as f64 / FS;
            re[s0 + i] += amp * w * ph.cos();
            im[s0 + i] += amp * w * ph.sin();
        }
    }
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut noise = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        ((state >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 12.0
    };
    let mut data = Vec::with_capacity(2 * n);
    for i in 0..n {
        data.push((re[i] + noise()).round().clamp(-128.0, 127.0) as i8 as u8);
        data.push((im[i] + noise()).round().clamp(-128.0, 127.0) as i8 as u8);
    }
    std::fs::write(dir.join("net.sigmf-data"), data).unwrap();
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(FS);
    meta.captures.push(Capture {
        sample_start: 0,
        frequency: Some(CENTER_HZ),
        datetime: Some("2026-09-15T12:00:00Z".into()),
        provenance: None,
        clip_count: None,
        extra: serde_json::Map::new(),
    });
    let path = dir.join("net.sigmf-meta");
    meta.write(&path).unwrap();
    path
}

// --- A test-only per-channel decoder: tone-burst energy detector --------------------------------

struct BurstFactory(BlockDescriptor);

impl BlockFactory for BurstFactory {
    fn descriptor(&self) -> &BlockDescriptor {
        &self.0
    }

    fn build(&self, _p: &Params, _ctx: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
        Ok(Box::new(Burst::default()))
    }
}

/// Emits `[round(length / 8 ms)]` per burst at the burst start; `corrected_bits` 1 when weak.
#[derive(Default)]
struct Burst {
    rate: f64,
    avg: f64,
    floor: f64,
    seen: u64,
    on: bool,
    /// Re-armed once the level fell back near the floor after a burst.
    armed: bool,
    start: u64,
    len: u64,
    peak: f64,
    frames: u64,
    status: Status,
}

impl Block for Burst {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let [i] = inputs else {
            return Err(BlockError::Ports("one input".into()));
        };
        self.rate = i.rate_hz;
        Ok(vec![PortInfo {
            ty: PortType::Frames,
            rate_hz: 20.0,
            max_items: 16,
            hold_items: 0,
        }])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        if input.meta.flags.contains(ChunkFlags::DISCONTINUITY)
            || input.meta.flags.contains(ChunkFlags::RESET)
        {
            self.seen = 0;
            self.on = false;
        }
        let PortSlice::Iq(x) = input.data else {
            return Err(BlockError::Ports("iq input".into()));
        };
        let out = io.output(0)?;
        out.meta = ChunkMeta {
            index: out.meta.index,
            ..input.meta
        };
        let PortVec::Frames(buf) = &mut out.data else {
            return Err(BlockError::Ports("frames output".into()));
        };
        let fast = 1.0 / (0.001 * self.rate);
        let slow = 1.0 / (0.2 * self.rate);
        let warm = (0.05 * self.rate) as u64;
        for (k, z) in x.iter().enumerate() {
            let p = f64::from(z.norm_sqr());
            self.avg += fast * (p - self.avg);
            self.seen += 1;
            if self.seen <= warm {
                self.floor = if self.seen == 1 {
                    self.avg
                } else {
                    self.floor + (self.avg - self.floor) / self.seen as f64
                };
                continue;
            }
            if !self.on {
                if self.avg < 8.0 * self.floor {
                    self.armed = true;
                }
                if self.armed && self.avg > 30.0 * self.floor {
                    self.armed = false;
                    self.on = true;
                    self.start = input.meta.source_index_of(k).round() as u64;
                    self.len = 0;
                    self.peak = self.avg;
                } else {
                    self.floor += slow * (self.avg - self.floor);
                }
                continue;
            }
            self.len += 1;
            self.peak = self.peak.max(self.avg);
            // Off at a quarter of the peak: the detector's rise and fall lags add about 2 ms.
            if self.avg < 0.25 * self.peak {
                self.on = false;
                let ms = self.len as f64 / self.rate * 1000.0;
                let code = ((ms - 2.0) / 8.0).round() as u8;
                let mut info = FrameInfo::new(self.frames, self.start, 0);
                info.bit_len = 8;
                info.check = CrcStatus::Valid;
                info.corrected_bits = u32::from(self.peak < 1000.0 * self.floor);
                buf.push(&[code], info);
                self.frames += 1;
            }
        }
        self.status.items_in += x.len() as u64;
        self.status.items_out = self.frames;
        Ok(())
    }

    fn reset(&mut self) {
        self.seen = 0;
        self.on = false;
    }

    fn update_params(&mut self, _: &Params, _: &BuildCtx<'_>) -> Result<ParamUpdate, BlockError> {
        Ok(ParamUpdate::Applied)
    }

    fn status(&self) -> Status {
        self.status
    }
}

fn registry() -> Registry {
    let mut r = Registry::builtin();
    r.register(Arc::new(BurstFactory(hk_blocks::schema::descriptor(
        "test_burst",
        "test",
        "T-093 test block: one [code] frame per tone burst",
        vec![PortSpec::new("in", PortType::Iq)],
        vec![PortSpec::new("out", PortType::Frames)],
        vec![],
        true,
    ))))
    .unwrap();
    r
}

fn recipe(list_hz: &[f64]) -> Value {
    json!({
        "schema": "hackriff.recipe", "schema_version": 2, "id": "burst-net", "version": 1,
        "name": "T-093 burst net",
        "input": {"port": "iq", "sample_rate_hz": 25000.0,
                  "channels": {"mode": "follow-hops", "channel_bandwidth_hz": 12500.0,
                               "max_channels": 4, "list_hz": list_hz}},
        "nodes": [
            {"id": "burst", "block": "test_burst"},
            {"id": "hops", "block": "follow_hops", "params": {"dedupe_s": 0.5, "order_window_s": 0.25}},
            {"id": "post", "block": "identity"}
        ],
        "outputs": [{"id": "frames", "kind": "inspector", "from": "post"}],
        "output_policy": {"content_class": "unrestricted"}
    })
}

fn band() -> Target {
    Target::Band {
        f_lo: CHANNELS[0] - 10e3,
        f_hi: CHANNELS[3] + 10e3,
    }
}

// --- A run through the mock SDR device --------------------------------------------------------

struct Run {
    handle: Option<PipelineHandle>,
    _driver: MockSdrDriver,
    streams: StreamRegistry,
    rt: Arc<RecipeRuntime>,
    dir: TempDir,
}

impl Run {
    fn start(tag: &str) -> Self {
        Self::start_in(TempDir::new(tag), scene)
    }

    /// A run over the recording `make` writes, with `dir` as its data directory (an earlier run's
    /// directory keeps that run's inventory).
    fn start_in(dir: TempDir, make: fn(&std::path::Path) -> std::path::PathBuf) -> Self {
        let meta = make(&dir.0.join("rec"));
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
        let streams = StreamRegistry::new();
        let reg = streams.clone();
        cfg.stream_sink = Some(Arc::new(move |h, p| reg.register(h, p)));
        let reg = streams.clone();
        cfg.stream_unsink = Some(Arc::new(move |id| {
            reg.unregister(id);
        }));
        let handle = Pipeline::start(
            cfg,
            Box::new(source),
            info,
            None,
            Box::new(TrackInventory::default()),
        )
        .unwrap();
        let counters = handle.counters();
        wait("the first samples", || {
            counters.source.samples.load(Ordering::Relaxed) > 0
        });
        let rt = handle.recipe_runtime();
        rt.set_registry(registry());
        Self {
            handle: Some(handle),
            _driver: driver,
            streams,
            rt,
            dir,
        }
    }

    fn frames(&self, id: &str) -> u64 {
        self.rt.stats_json(id).unwrap()["frames"].as_u64().unwrap()
    }

    /// The pipeline's status as published after this call: status is published every
    /// `STATUS_INTERVAL`, so a snapshot taken right after a wait may predate what was waited for
    /// (T-175: an optimised build gets here before the first tick). Two ticks guarantee one began
    /// after the call.
    fn fresh_status(&self, id: &str) -> Value {
        let ticks = || {
            self.rt.stats_json(id).unwrap()["status_ticks"]
                .as_u64()
                .unwrap()
        };
        let t0 = ticks();
        wait("status ticks", || ticks() >= t0 + 2);
        self.rt.pipeline_json(id).unwrap()["status"].clone()
    }

    /// Stops the run; returns its data directory.
    fn finish(mut self) -> TempDir {
        self.rt.stop_all();
        let handle = self.handle.take().unwrap();
        handle.stop();
        let (s, fired) = wait_guarded(handle, Duration::from_secs(120));
        assert!(!fired, "the run stopped within the limit");
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        self.dir
    }
}

#[derive(Clone, Default)]
struct Collected(Arc<Mutex<Vec<u8>>>);

impl Write for Collected {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Subscribes an in-process consumer to `stream_id`.
fn subscribe(run: &Run, stream_id: &str) -> Collected {
    let c = Collected::default();
    run.streams
        .handle(stream_id)
        .expect("the inspector stream is offered")
        .subscribe("t093", Declared::local(c.clone()), Box::new(|_| {}))
        .unwrap();
    c
}

/// Frame records collected so far: `(sample_index, channel, channel_hz, code)`.
fn frame_records(c: &Collected) -> Vec<(u64, u16, f64, u8)> {
    let bytes = c.0.lock().unwrap().clone();
    let mut r = StreamReader::new(Cursor::new(bytes));
    let mut out = Vec::new();
    if r.read_header().is_err() {
        return out; // nothing delivered yet
    }
    loop {
        let v: Value = match r.next_record() {
            Ok(Some(Record::Message(m))) => m.value,
            Ok(Some(Record::Unknown(b))) => serde_json::from_slice(&b).unwrap(),
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => break,
        };
        if v["type"] != "frame" {
            continue;
        }
        let md = &v["metadata"];
        let hex = v["content"]["hex"].as_str().unwrap();
        out.push((
            md["sample_index"].as_u64().unwrap(),
            md["channel"].as_u64().unwrap() as u16,
            md["channel_hz"].as_f64().unwrap(),
            u8::from_str_radix(&hex[..2], 16).unwrap(),
        ));
    }
    out
}

fn truth_of(code: u8) -> Vec<&'static (usize, f64, u8, bool)> {
    TRUTH.iter().filter(|t| t.2 == code).collect()
}

#[test]
fn one_recipe_follows_a_channel_net_ordered_tagged_deduplicated_and_gains_a_channel_mid_run() {
    let run = Run::start("t093-net");
    let id = run
        .rt
        .start(parse_recipe(recipe(&CHANNELS[..3])).unwrap(), band())
        .unwrap();
    let p = run.rt.pipeline_json(&id).unwrap();
    let hops = &p["follow_hops"];
    assert_eq!(hops["channel_source"], "list");
    let listed: Vec<f64> = hops["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["center_hz"].as_f64().unwrap())
        .collect();
    assert_eq!(listed, CHANNELS[..3]);
    let sink = subscribe(&run, &format!("inspector/{id}/frames"));

    // Three loops on A, B, C (8 messages per loop once the duplicate is removed), then D joins.
    let per_loop_abc = TRUTH.iter().filter(|t| t.0 < 3).count() as u64 - 1;
    let per_loop_all = TRUTH.len() as u64 - 1;
    let n0 = run.frames(&id);
    wait("three loops on three channels", || {
        run.frames(&id) >= n0 + 3 * per_loop_abc
    });
    let added = run.rt.set_channels(&id, &CHANNELS).unwrap();
    assert_eq!(added["added"][0]["index"], 3);
    assert_eq!(added["removed"].as_array().unwrap().len(), 0);
    let at = added["applied_at_sample"].as_u64().unwrap();
    let n1 = run.frames(&id);
    wait("three loops on four channels", || {
        run.frames(&id) >= n1 + 3 * per_loop_all
    });
    let status = run.fresh_status(&id);
    run.rt.stop_json(&id).unwrap();
    let frames = frame_records(&sink);
    run.finish();

    // The test's own burst detector needs a settling window after its lane starts before its
    // length measurement means anything: `warm` (50 ms) and then the floor's 0.2 s time constant.
    // A lane that starts within that window of one of its bursts folds the burst into its floor
    // estimate, fires late and reads the burst short (T-228: channel D's 104 ms burst read as
    // 96 ms right after the mid-run add). Measure the window on the sample clock from when each
    // lane started - the run for A, B and C, `applied_at_sample` for D - and assert on what
    // follows it. Which bursts land in it depends on where the pipeline happened to start, not on
    // anything the merge does.
    let settle = (0.3 * FS) as u64;
    let si0 = frames.iter().map(|f| f.0).min().unwrap_or(0);
    let lane_settled = |f: &&(u64, u16, f64, u8)| {
        f.0 >= if usize::from(f.1) == 3 {
            at + settle
        } else {
            si0 + settle
        }
    };
    let frames: Vec<(u64, u16, f64, u8)> = frames.iter().filter(lane_settled).copied().collect();

    assert!(frames.len() as u64 >= 5 * per_loop_abc, "{frames:?}");
    // Tagged with the right channel; the duplicate keeps its strong copy (channel B).
    for &(si, ch, ch_hz, code) in &frames {
        let t = truth_of(code);
        assert!(!t.is_empty(), "frame {code} at {si} is not a sent message");
        let sent = t.iter().find(|t| t.3).unwrap();
        assert_eq!(
            usize::from(ch),
            sent.0,
            "message {code} tagged with its channel: {frames:?}"
        );
        assert!(
            (ch_hz - CHANNELS[sent.0]).abs() < 1.0,
            "channel_hz of {code}"
        );
    }
    // Time-ordered across channels by when each message ended (T-107), to the resolution of one
    // ring read (at most 65 536 samples); nothing counted late.
    let end = |f: &(u64, u16, f64, u8)| f.0 + (f64::from(f.3) * 0.008 * FS) as u64;
    for w in frames.windows(2) {
        assert!(
            end(&w[0]) <= end(&w[1]) + (1 << 16),
            "frames out of end order: {w:?}"
        );
    }
    // Loop bookkeeping from the first frame of message 3 (A at 0.20 s).
    let first = frames.iter().find(|f| f.3 == 3).expect("message 3");
    let base = first.0 as i64 - (0.20 * FS) as i64;
    let loop_of = |si: u64, code: u8| {
        let start = (truth_of(code)[0].1 * FS) as i64;
        let rel = si as i64 - base - start;
        let k = (rel as f64 / LOOP as f64).round() as i64;
        assert!(
            (rel - k * LOOP as i64).abs() < (0.02 * FS) as i64,
            "message {code} at {si} is not at its sent time (channel added at {at}): \
             {frames:?}, status {status}"
        );
        k
    };
    let loops: Vec<i64> = frames.iter().map(|f| loop_of(f.0, f.3)).collect();
    let (kmin, kmax) = (*loops.iter().min().unwrap(), *loops.iter().max().unwrap());
    let mut before_add = 0;
    let mut after_add = 0;
    let mut spanning = 0;
    for k in kmin + 1..kmax {
        let loop_start = base + k * LOOP as i64;
        let loop_end = loop_start + LOOP as i64;
        let codes: Vec<u8> = frames
            .iter()
            .zip(&loops)
            .filter(|(_, l)| **l == k)
            .map(|(f, _)| f.3)
            .collect();
        let mut expected: Vec<u8> = TRUTH
            .iter()
            .filter(|t| t.0 < 3 && (t.3 || t.2 != DUP_CODE))
            .map(|t| t.2)
            .collect();
        if loop_start >= (at + settle) as i64 {
            expected.extend(TRUTH.iter().filter(|t| t.0 == 3).map(|t| t.2));
            after_add += 1;
        } else if loop_end <= at as i64 {
            before_add += 1;
        } else {
            // The loop D joined in: D's messages at most once, every other channel's exactly once.
            spanning += 1;
            for t in TRUTH.iter().filter(|t| t.0 == 3) {
                assert!(codes.iter().filter(|c| **c == t.2).count() <= 1);
            }
            expected.extend(
                codes
                    .iter()
                    .copied()
                    .filter(|c| TRUTH.iter().any(|t| t.0 == 3 && t.2 == *c)),
            );
        }
        let mut got = codes.clone();
        got.sort_unstable();
        expected.sort_unstable();
        assert_eq!(got, expected, "loop {k}: every message exactly once");
    }
    assert!(
        before_add >= 1,
        "a complete loop before the channel was added"
    );
    assert!(
        after_add >= 1,
        "a complete loop after the channel was added"
    );
    assert!(before_add + after_add + spanning >= 3);
    let dups = status["hops.dups"].as_f64().unwrap_or(0.0);
    assert!(dups >= 2.0, "duplicates counted in status: {status}");
    assert_eq!(status["hops.late"].as_f64(), Some(0.0), "{status}");
}

/// Records a hop set as the tracker's hop-set linking does (T-059/T-084): a blind measurement of
/// which channels one emitter hops over, with no band-plan input.
fn record_hop_set(run: &Run, channels_hz: &[f64], at_s: i64) -> EmitterId {
    let mut repo = common::repo(&run.dir.0);
    let t0 = Timestamp::from_unix_nanos((1_789_000_000 + at_s) * 1_000_000_000);
    let h = HopSetSummary {
        id: TrackId::new(),
        channels_hz: channels_hz.to_vec(),
        members: Vec::new(),
        raster_hz: Some(25e3),
        hop_rate_hz: Some(5.0),
        dwell_s: Some(0.06),
        hops: 40,
        time: TimeRange {
            start: t0,
            end: t0.saturating_add_nanos(10_000_000_000),
        },
    };
    repo.record_sighting(&hop_set_sighting(&h), None)
        .unwrap()
        .emitter_id
}

#[test]
fn a_blind_hop_set_supplies_the_channels_and_a_channel_it_gains_is_followed() {
    let run = Run::start("t093-hopset");
    let eid = record_hop_set(&run, &CHANNELS[..2], 0);
    let id = run
        .rt
        .start(parse_recipe(recipe(&[])).unwrap(), Target::Emitter(eid))
        .unwrap();
    let channels = |run: &Run| -> Vec<f64> {
        run.rt.pipeline_json(&id).unwrap()["follow_hops"]["channels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["center_hz"].as_f64().unwrap())
            .collect()
    };
    let p = run.rt.pipeline_json(&id).unwrap();
    assert_eq!(p["follow_hops"]["channel_source"], "hop-set");
    assert_eq!(channels(&run), CHANNELS[..2]);
    let sink = subscribe(&run, &format!("inspector/{id}/frames"));

    // The hop set is measured again with a third channel.
    let again = record_hop_set(&run, &CHANNELS[..3], 20);
    assert_eq!(again, eid, "the same hop set");
    let r = run.rt.refresh_channels(&id).unwrap();
    assert_eq!(r["added"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(channels(&run), CHANNELS[..3]);
    // Every channel, the added one and the two it already followed, delivers a frame. Waiting only
    // for the added channel and then checking the others raced under load: the added channel's
    // frame could arrive first (full check after the T-181 merge, 2026-09-15).
    wait(
        "a frame from every channel, including the added one",
        || {
            let frames = frame_records(&sink);
            (0..3).all(|c| frames.iter().any(|f| f.1 == c))
        },
    );

    // A hot edit across the per-channel instances: a node inserted upstream (every lane rebuilds
    // its decoder) and the merge's dedupe window changed in place. Capture never stops and every
    // channel keeps decoding.
    let mut draft = recipe(&[]);
    draft["nodes"] = json!([
        {"id": "pre", "block": "identity"},
        {"id": "burst", "block": "test_burst"},
        {"id": "hops", "block": "follow_hops", "params": {"dedupe_s": 0.4, "order_window_s": 0.25}},
        {"id": "post", "block": "identity"}
    ]);
    let e = run.rt.edit(&id, parse_recipe(draft).unwrap()).unwrap();
    assert_eq!(e["edit_rev"], 1, "{e}");
    assert_eq!(
        e["swap"]["updated"], 1,
        "the merge node updated in place: {e}"
    );
    let at = e["applied_at_sample"].as_u64().unwrap();
    wait("frames on every channel after the edit", || {
        let f = frame_records(&sink);
        (0..3).all(|c| f.iter().any(|x| x.1 == c && x.0 > at))
    });
    let status = run.fresh_status(&id);
    let p = run.rt.pipeline_json(&id).unwrap();
    assert_eq!(p["state"], "running");
    assert!(
        status["ch2.pre.items_in"].as_f64().unwrap_or(0.0) > 0.0,
        "{status}"
    );
    run.finish();
}

// --- T-107: the blind channel sources, end to end ----------------------------------------------

/// Inventory rows around the net: `(centre Hz, bandwidth Hz, hop_set_hz, id)`.
fn inventory(dir: &std::path::Path) -> Vec<(f64, f64, Vec<f64>, EmitterId)> {
    let repo = common::repo(dir);
    let q = InventoryQuery {
        freq: Some(FreqRange {
            lo_hz: CHANNELS[0] - 20e3,
            hi_hz: CHANNELS[3] + 20e3,
        }),
        limit: 256,
        ..InventoryQuery::default()
    };
    repo.query_inventory(&q)
        .unwrap()
        .entries
        .iter()
        .map(|x| {
            let e = &x.emitter;
            let hop: Vec<f64> = e
                .fingerprint
                .get("hop_set_hz")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_f64).collect())
                .unwrap_or_default();
            (e.f_center_hz, e.bandwidth_hz, hop, e.id)
        })
        .collect()
}

/// The sent channel a measured centre belongs to (the tones sit 1 kHz above the centres).
fn sent_channel(hz: f64) -> Option<usize> {
    CHANNELS
        .iter()
        .position(|c| (hz - (c + 1_000.0)).abs() < 3_000.0)
}

fn channel_centers(p: &Value) -> Vec<f64> {
    p["follow_hops"]["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["center_hz"].as_f64().unwrap())
        .collect()
}

/// T-107 (SIGNAL-062): with no `list_hz` and no hop-set target, a follow-hops pipeline follows the
/// blind detections in its band. A survey run replays the net through the mock SDR and its own
/// detector and tracker write the inventory from IQ (the test inserts nothing); a second run on
/// the same data directory follows the channels those detections found, and every channel
/// decodes its own messages.
#[test]
fn the_blind_detections_in_a_band_supply_the_channels() {
    let survey = Run::start("t107-detections");
    let counters = survey.handle.as_ref().unwrap().counters();
    wait("three survey loops", || {
        counters.source.samples.load(Ordering::Relaxed) >= 3 * LOOP
    });
    let dir = survey.finish();
    let rows = inventory(&dir.0);
    for k in 0..CHANNELS.len() {
        assert!(
            rows.iter()
                .any(|r| r.2.is_empty() && sent_channel(r.0) == Some(k)),
            "channel {k} detected: {rows:?}"
        );
    }

    let run = Run::start_in(dir, scene);
    let id = run
        .rt
        .start(parse_recipe(recipe(&[])).unwrap(), band())
        .unwrap();
    let p = run.rt.pipeline_json(&id).unwrap();
    assert_eq!(p["follow_hops"]["channel_source"], "detections", "{p}");
    let chans = channel_centers(&p);
    assert_eq!(chans.len(), CHANNELS.len(), "{chans:?} from {rows:?}");
    for (k, f) in chans.iter().enumerate() {
        assert_eq!(sent_channel(*f), Some(k), "{chans:?}");
    }
    let sink = subscribe(&run, &format!("inspector/{id}/frames"));
    wait("a message decoded on every channel", || {
        let f = frame_records(&sink);
        (0..CHANNELS.len() as u16).all(|c| f.iter().any(|x| x.1 == c))
    });
    for &(si, ch, _, code) in &frame_records(&sink) {
        let t = truth_of(code);
        assert!(!t.is_empty(), "frame {code} at {si} is not a sent message");
        let sent = t.iter().find(|t| t.3).unwrap();
        assert_eq!(usize::from(ch), sent.0, "message {code} on its channel");
    }
    run.finish();
}

/// T-107 (SIGNAL-062): a net hops over four channels; the pipeline's tracker links the dwells
/// into a hop set from IQ through the mock SDR (the test inserts nothing), and a follow-hops
/// pipeline targeting that emitter follows the measured channels and decodes hops on each.
#[test]
fn a_hop_set_the_tracker_found_from_iq_supplies_the_channels() {
    let run = Run::start_in(TempDir::new("t107-hopper"), hopper_scene);
    let deadline = Instant::now() + LIMIT;
    let (eid, hop) = loop {
        let rows = inventory(&run.dir.0);
        if let Some(r) = rows.iter().find(|r| r.2.len() >= 3) {
            break (r.3, r.2.clone());
        }
        assert!(
            Instant::now() < deadline,
            "the tracker formed no hop set: {rows:?}"
        );
        std::thread::sleep(Duration::from_millis(250));
    };
    for f in &hop {
        assert!(
            sent_channel(*f).is_some(),
            "measured hop channel {f} is a channel the net used: {hop:?}"
        );
    }
    let mut doc = recipe(&[]);
    // Every hop carries the same message: copies on different channels are separate hops.
    doc["nodes"][1]["params"]["dedupe_s"] = json!(0.005);
    let id = run
        .rt
        .start(parse_recipe(doc).unwrap(), Target::Emitter(eid))
        .unwrap();
    let p = run.rt.pipeline_json(&id).unwrap();
    assert_eq!(p["follow_hops"]["channel_source"], "hop-set", "{p}");
    let chans = channel_centers(&p);
    assert_eq!(chans.len(), hop.len().min(4), "{chans:?} from {hop:?}");
    let sink = subscribe(&run, &format!("inspector/{id}/frames"));
    wait("a hop decoded on every followed channel", || {
        let f = frame_records(&sink);
        (0..chans.len() as u16).all(|c| f.iter().any(|x| x.1 == c))
    });
    for &(si, ch, ch_hz, code) in &frame_records(&sink) {
        assert_eq!(code, 4, "a {HOP_DWELL_S} s hop at {si}");
        assert!((ch_hz - chans[usize::from(ch)]).abs() < 1.0);
    }
    run.finish();
}
