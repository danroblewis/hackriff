//! T-088 (SIGNAL-062): decoder-workbench recipe pipelines end to end **through the mock SDR
//! device interface** (a tone recording behind `MockSdrDriver`, looping, lossless), never feeding
//! files into the pipeline.
//!
//! - **Run and serve:** a recipe runs as a chain on a mock channel; frame records arrive one per
//!   frame over TCP (`open/inspector`) and WebSocket (`/ws/inspector/<pipeline>/<output>`), with
//!   interleaved status records; an on-demand stage tap serves the DDC output.
//! - **Hot edit:** hot params, field-map content, a cold param and an added node apply at a chunk
//!   boundary: state kept where the plan says so, capture never stops, no ring sample is lost.
//! - **Store and re-run:** save (immutable increasing versions), list, re-run a saved version on a
//!   persisted selection, save a running revision.
//! - **Parallel:** two pipelines at once within the chain budget; the next is `503 busy`.
//!
//! The recipe uses the contract's `identity` block plus a test-only `test_framer` block
//! registered here (the real block library lands with T-086/T-087 through the same registry).

mod common;

use std::io::Write as _;
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::{TempDir, tone_recording, wait_guarded};
use hk_api::{
    ApiState, Server, ServerConfig, StreamRegistry, StreamServer, StreamServerConfig, Token,
};
use hk_blocks::{
    Block, BlockError, BlockFactory, BuildCtx, ChunkFlags, ChunkMeta, FrameInfo, Io, ParamUpdate,
    PortInfo, PortSlice, PortVec, Registry, Status,
};
use hk_core::{MockEnd, MockOptions, MockSdrDriver, Source};
use hk_model::{Repository, Selection};
use hk_pipeline::class::band_class;
use hk_pipeline::recipes::runtime::{RecipeRuntime, Target, parse_recipe};
use hk_pipeline::{
    Pipeline, PipelineConfig, PipelineHandle, SourceInfo, TrackInventory, replay_plan,
};
use hk_recipe::{BlockDescriptor, ParamSchema, ParamType, Params, PortSpec, PortType};
use hk_stream::{OpenerRegistry, Record, StreamKind, StreamReader};
use serde_json::{Value, json};
use tungstenite::Message;

const FS: f64 = 1.0e6;
const CENTER_HZ: f64 = 100.0e6;
/// `tone_recording` puts its tone 50 kHz above the centre.
const TONE_HZ: f64 = CENTER_HZ + 50e3;
const TOKEN: &str = "t088-recipe-runtime-token-0123456789abcdef";
const LIMIT: Duration = Duration::from_secs(90);

fn wait(what: &str, limit: Duration, f: impl Fn() -> bool) {
    let deadline = Instant::now() + limit;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

// --- A test-only block: emits a 2-byte frame [tag, counter] every `every` input samples ---------

fn int_param(name: &str, hot: bool, default: i64) -> ParamSchema {
    ParamSchema {
        name: name.into(),
        ty: ParamType::Int {
            min: Some(0),
            max: Some(1 << 20),
        },
        required: false,
        default: Some(json!(default)),
        hot,
        doc: String::new(),
    }
}

struct FramerFactory(BlockDescriptor);

impl BlockFactory for FramerFactory {
    fn descriptor(&self) -> &BlockDescriptor {
        &self.0
    }

    fn build(&self, p: &Params, _ctx: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
        Ok(Box::new(Framer {
            every: get(p, "every", 5000).max(1),
            tag: get(p, "tag", 1) as u8,
            ..Framer::default()
        }))
    }
}

fn get(p: &Params, k: &str, d: u64) -> u64 {
    p.get(k).and_then(Value::as_u64).unwrap_or(d)
}

#[derive(Default)]
struct Framer {
    every: u64,
    tag: u8,
    phase: u64,
    frames: u64,
    discs: u64,
    resets: u64,
    updates: u64,
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
        if input.meta.flags.contains(ChunkFlags::DISCONTINUITY) {
            self.discs += 1;
            self.phase = 0;
        }
        if input.meta.flags.contains(ChunkFlags::RESET) {
            self.resets += 1;
        }
        let PortSlice::Iq(x) = input.data else {
            return Err(BlockError::Ports("iq input".into()));
        };
        let out = io.output(0)?;
        out.meta = ChunkMeta {
            index: out.meta.index,
            source_per_item: input.meta.source_per_item * self.every as f64,
            rate_hz: input.meta.rate_hz / self.every as f64,
            ..input.meta
        };
        let PortVec::Frames(buf) = &mut out.data else {
            return Err(BlockError::Ports("frames output".into()));
        };
        for k in 0..x.len() {
            self.phase += 1;
            if self.phase >= self.every {
                self.phase = 0;
                let mut info =
                    FrameInfo::new(self.frames, input.meta.source_index_of(k).round() as u64, 0);
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
        p: &Params,
        _ctx: &BuildCtx<'_>,
    ) -> Result<ParamUpdate, BlockError> {
        if get(p, "every", 5000) != self.every {
            return Ok(ParamUpdate::Rebuild);
        }
        self.tag = get(p, "tag", 1) as u8;
        self.updates += 1;
        Ok(ParamUpdate::Applied)
    }

    fn status(&self) -> Status {
        let mut s = self.status;
        s.extra.set("discs", self.discs as f64);
        s.extra.set("resets", self.resets as f64);
        s.extra.set("updates", self.updates as f64);
        s
    }
}

fn registry() -> Registry {
    let mut r = Registry::builtin();
    r.register(Arc::new(FramerFactory(hk_blocks::schema::descriptor(
        "test_framer",
        "test",
        "T-088 test block: a [tag, counter] frame every `every` samples",
        vec![PortSpec::new("in", PortType::Iq)],
        vec![PortSpec::new("out", PortType::Frames)],
        vec![
            int_param("every", false, 5000),
            int_param("tag", true, 1),
            int_param("salt", false, 0),
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

/// identity (DDC output) → test_framer → identity; an inspector output and a declared stage.
fn recipe(id: &str, tag: u64, every: u64) -> Value {
    json!({
        "schema": "hackriff.recipe", "schema_version": 2, "id": id, "version": 1,
        "name": "T-088 tone framer",
        "input": {"port": "iq", "sample_rate_hz": 100000.0, "bandwidth_hz": 20000.0},
        "nodes": [
            {"id": "pre", "block": "identity"},
            {"id": "framer", "block": "test_framer",
             "params": {"every": every, "tag": tag, "map": "m"}},
            {"id": "post", "block": "identity"}
        ],
        "field_maps": {"m": {"unit": "bits", "fields": [
            {"name": "tag", "type": "uint", "length": 8}
        ]}},
        "outputs": [
            {"id": "frames", "kind": "inspector", "from": "post"},
            {"id": "baseband", "kind": "stage", "from": "pre"}
        ],
        "output_policy": {"content_class": "unrestricted"}
    })
}

fn band() -> Target {
    Target::Band {
        f_lo: TONE_HZ - 10e3,
        f_hi: TONE_HZ + 10e3,
    }
}

// --- A run through the mock SDR device --------------------------------------------------------

struct Run {
    handle: Option<PipelineHandle>,
    driver: MockSdrDriver,
    streams: StreamRegistry,
    rt: Arc<RecipeRuntime>,
    dir: TempDir,
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
        let streams = StreamRegistry::new();
        let reg = streams.clone();
        cfg.stream_sink = Some(Arc::new(move |h, p| reg.register(h, p)));
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
            counters.source.samples.load(Ordering::Relaxed) > 0
        });
        let rt = handle.recipe_runtime();
        assert!(
            Arc::ptr_eq(&rt, &handle.recipe_runtime()),
            "one runtime per run"
        );
        rt.set_registry(registry());
        Self {
            handle: Some(handle),
            driver,
            streams,
            rt,
            dir,
        }
    }

    fn handle(&self) -> &PipelineHandle {
        self.handle.as_ref().unwrap()
    }

    fn frames(&self, id: &str) -> u64 {
        self.rt.stats_json(id).unwrap()["frames"].as_u64().unwrap()
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

fn serve(rt: &Arc<RecipeRuntime>, streams: &StreamRegistry) -> (StreamServer, Server) {
    let openers = OpenerRegistry::new()
        .with("stage", rt.stage_service())
        .with("inspector", rt.inspector_service());
    let token = Token::from_config(TOKEN).unwrap();
    let tcp = StreamServer::start(
        StreamServerConfig::new("127.0.0.1:0".parse().unwrap(), token.clone()),
        streams.clone(),
        openers.clone(),
    )
    .unwrap();
    let http = Server::start(
        ServerConfig::new("127.0.0.1:0".parse().unwrap(), token),
        ApiState {
            streams: streams.clone(),
            on_demand: openers,
            ..ApiState::default()
        },
    )
    .unwrap();
    (tcp, http)
}

fn open_tcp(addr: SocketAddr, line: &str) -> StreamReader<TcpStream> {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.write_all(line.as_bytes()).unwrap();
    StreamReader::new(s)
}

/// The §14 record type: `type` once T-089 frames them natively, `metadata.record` until then.
fn record_type(v: &Value) -> &str {
    v["type"]
        .as_str()
        .filter(|t| *t != "message")
        .or_else(|| v["metadata"]["record"].as_str())
        .unwrap_or("")
}

fn next_message(r: &mut StreamReader<TcpStream>) -> Value {
    loop {
        match r.next_record().unwrap() {
            Some(Record::Message(m)) => return m.value,
            Some(_) => {}
            None => panic!("the stream ended"),
        }
    }
}

fn frame_bytes(v: &Value) -> Vec<u8> {
    hk_stream::inspector::from_hex(v["content"]["hex"].as_str().unwrap()).unwrap()
}

fn rev(v: &Value) -> u64 {
    v["metadata"]["edit_rev"].as_u64().unwrap()
}

#[test]
fn a_recipe_runs_on_a_mock_channel_and_frame_records_arrive_over_tcp_and_ws() {
    let run = Run::start("t088-serve");
    let id = run
        .rt
        .start(parse_recipe(recipe("tone", 1, 5000)).unwrap(), band())
        .unwrap();
    let (tcp, http) = serve(&run.rt, &run.streams);

    // TCP, through the live inspector opener.
    let mut r = open_tcp(
        tcp.local_addr(),
        &format!("open/inspector?token={TOKEN}&pipeline={id}\n"),
    );
    let h = r.read_header().unwrap().clone();
    assert_eq!(h.kind, StreamKind::Messages);
    assert_eq!(h.stream_id, format!("inspector/{id}/frames"));
    assert_eq!(h.message_schema.as_deref(), Some("hackriff.inspector/1"));
    assert_eq!(h.source, "hk-pipeline:recipe:tone@1");
    let (mut frames, mut status) = (Vec::new(), Vec::new());
    let deadline = Instant::now() + LIMIT;
    while frames.len() < 4 || status.is_empty() {
        assert!(Instant::now() < deadline, "frames and a status record");
        let v = next_message(&mut r);
        match record_type(&v) {
            "frame" => frames.push(v),
            "status" => status.push(v),
            other => panic!("unexpected record {other}: {v}"),
        }
    }
    let f = &frames[0];
    assert_eq!(f["decoder"], json!("recipe:tone@1"));
    assert_eq!(f["frame_model"], json!("tone"));
    assert_eq!(f["gated"], json!(false));
    assert_eq!(f["metadata"]["bit_len"], json!(16));
    assert_eq!(f["metadata"]["edit_rev"], json!(0));
    assert_eq!(f["metadata"]["recipe_version"], json!(1));
    assert_eq!(f["metadata"]["channel"], json!(0));
    assert_eq!(f["metadata"]["fit"], json!("none"));
    assert!(f["metadata"]["sample_index"].as_u64().is_some(), "{f}");
    assert_eq!(frame_bytes(f)[0], 1, "the tag parameter");
    // One record per frame, in order, 5000 channel samples (50 000 ring samples) apart (a pair
    // across the looping recording's splice may differ by the DDC's restart).
    let mut exact = 0;
    for w in frames.windows(2) {
        assert_eq!(
            w[1]["metadata"]["frame"].as_u64().unwrap(),
            w[0]["metadata"]["frame"].as_u64().unwrap() + 1
        );
        let d = w[1]["metadata"]["sample_index"].as_u64().unwrap()
            - w[0]["metadata"]["sample_index"].as_u64().unwrap();
        exact += usize::from((49_990..=50_010).contains(&d));
    }
    assert!(exact + 1 >= frames.len() - 1, "frame spacing: {frames:?}");
    // Status: every node batched in one record.
    let m = &status[0]["metadata"];
    for node in ["pre", "framer", "post"] {
        assert!(m[format!("{node}.items_in")].is_u64(), "{m}");
    }

    // WebSocket, the always-on stream by id.
    let (mut ws, _) = tungstenite::connect(format!(
        "ws://{}/ws/inspector/{id}/frames?token={TOKEN}",
        http.local_addr()
    ))
    .unwrap();
    let Message::Text(t) = ws.read().unwrap() else {
        panic!("the header comes first, as text")
    };
    let header: Value = serde_json::from_str(t.as_str()).unwrap();
    assert_eq!(header["stream_id"], json!(format!("inspector/{id}/frames")));
    let mut ws_frames = 0;
    while ws_frames < 2 {
        let body = match ws.read().unwrap() {
            Message::Text(t) => t.as_str().as_bytes().to_vec(),
            Message::Binary(b) => b.to_vec(),
            _ => continue,
        };
        if let Ok(v) = serde_json::from_slice::<Value>(&body)
            && record_type(&v) == "frame"
        {
            ws_frames += 1;
        }
    }

    // An on-demand stage tap on the DDC output, over TCP.
    let mut s = open_tcp(
        tcp.local_addr(),
        &format!("open/stage?token={TOKEN}&pipeline={id}&node=pre\n"),
    );
    let sh = s.read_header().unwrap().clone();
    assert_eq!(sh.kind, StreamKind::Iq);
    assert_eq!(sh.datatype.as_deref(), Some("cf32_le"));
    assert!((sh.sample_rate_hz.unwrap() - 100_000.0).abs() < 1.0);
    let mut records = 0;
    while records < 3 {
        if let Some(Record::Binary(b)) = s.next_record().unwrap() {
            assert!(!b.payload.is_empty() && b.payload.len() % 8 == 0);
            records += 1;
        }
    }
    assert!(
        run.streams
            .handle(&format!("stage/{id}/baseband"))
            .is_some(),
        "the declared stage output is offered"
    );
    drop((r, s));
    let _ = ws.close(None);

    let p = run.rt.pipeline_json(&id).unwrap();
    assert_eq!(p["state"], json!("running"), "{p}");
    assert_eq!(p["recipe_id"], json!("tone"));
    assert!(p["status"]["framer.items_in"].as_u64().unwrap() > 0, "{p}");
    let counters = run.handle().counters().to_json();
    assert_eq!(counters["budget"]["chains"], json!(1), "a recipe chain");
    drop((tcp, http));
    run.finish();
}

/// Reads frame records until `n` frames at `edit_rev` have arrived; returns the last frame before
/// the first one at `edit_rev`, the `edit` record announcing it, and the new frames.
fn across_edit(
    r: &mut StreamReader<TcpStream>,
    last: &mut Value,
    to: u64,
    n: usize,
) -> (Value, Value, Vec<Value>) {
    let (mut last_old, mut edit, mut new) = (last.clone(), Value::Null, Vec::new());
    let deadline = Instant::now() + LIMIT;
    while new.len() < n {
        assert!(Instant::now() < deadline, "frames of revision {to}");
        let v = next_message(r);
        match record_type(&v) {
            "frame" if rev(&v) == to => {
                assert!(
                    !edit.is_null(),
                    "the edit record precedes the new revision's frames"
                );
                new.push(v);
            }
            "frame" => last_old = v,
            "edit" if v["metadata"]["edit_rev"] == json!(to) => edit = v,
            _ => {}
        }
    }
    *last = new.last().cloned().unwrap_or(Value::Null);
    (last_old, edit, new)
}

#[test]
fn hot_edits_apply_at_a_chunk_boundary_without_stopping_capture_or_losing_samples() {
    let run = Run::start("t088-edit");
    let rt = &run.rt;
    let counters = run.handle().counters();
    let id = rt
        .start(parse_recipe(recipe("tone", 1, 5000)).unwrap(), band())
        .unwrap();
    let (tcp, _http) = serve(rt, &run.streams);
    let mut r = open_tcp(
        tcp.local_addr(),
        &format!("open/inspector?token={TOKEN}&pipeline={id}\n"),
    );
    r.read_header().unwrap();
    let (mut seen, mut last) = (0, Value::Null);
    while seen < 2 {
        let v = next_message(&mut r);
        if record_type(&v) == "frame" {
            seen += 1;
            last = v;
        }
    }
    let samples_before = counters.source.samples.load(Ordering::Relaxed);

    // 1. A hot parameter: applied in place, the framer keeps its state (its counter continues).
    let res = rt
        .edit(&id, parse_recipe(recipe("tone", 7, 5000)).unwrap())
        .unwrap();
    assert_eq!(res["edit_rev"], json!(1), "{res}");
    let framer = |res: &Value| {
        res["plan"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == "framer")
            .cloned()
            .unwrap()
    };
    assert_eq!(framer(&res)["change"], json!("params-hot"), "{res}");
    assert_eq!(framer(&res)["keys"], json!(["tag"]));
    assert_eq!(
        res["swap"],
        json!({"rebuilt": 0, "reset": 0, "updated": 1, "kept": 2})
    );
    let at = res["applied_at_sample"].as_u64().unwrap();
    let (old, edit, new) = across_edit(&mut r, &mut last, 1, 2);
    assert_eq!(edit["metadata"]["applied_at_sample"], json!(at));
    assert_eq!(frame_bytes(&new[0])[0], 7, "the new tag");
    assert_eq!(
        frame_bytes(&new[0])[1],
        frame_bytes(&old)[1].wrapping_add(1),
        "state kept: the counter continues across the boundary"
    );
    assert_eq!(
        new[0]["metadata"]["frame"].as_u64().unwrap(),
        old["metadata"]["frame"].as_u64().unwrap() + 1,
        "the stream kept its consumer and numbering"
    );
    assert!(new[0]["metadata"]["sample_index"].as_u64().unwrap() >= at);

    // 2. Field-map content: a hot change on the node naming the map.
    let mut doc = recipe("tone", 7, 5000);
    doc["field_maps"]["m"]["fields"][0]["label"] = json!("Tag");
    let res = rt.edit(&id, parse_recipe(doc).unwrap()).unwrap();
    assert_eq!(res["plan"]["field_maps_changed"], json!(["m"]), "{res}");
    assert_eq!(framer(&res)["keys"], json!(["map"]));
    assert_eq!(res["swap"]["updated"], json!(1));
    let (old, _, new) = across_edit(&mut r, &mut last, 2, 1);
    assert_eq!(
        frame_bytes(&new[0])[1],
        frame_bytes(&old)[1].wrapping_add(1)
    );

    // 3. A cold parameter: the framer is rebuilt (counter restarts), `post` downstream resets.
    let mut doc = recipe("tone", 7, 5000);
    doc["nodes"][1]["params"]["salt"] = json!(1);
    let res = rt.edit(&id, parse_recipe(doc.clone()).unwrap()).unwrap();
    assert_eq!(framer(&res)["change"], json!("params-cold"), "{res}");
    assert_eq!(res["plan"]["reset"], json!(["post"]));
    assert_eq!(
        res["swap"],
        json!({"rebuilt": 1, "reset": 1, "updated": 0, "kept": 1})
    );
    let (_, _, new) = across_edit(&mut r, &mut last, 3, 2);
    assert_eq!(frame_bytes(&new[0])[1], 0, "a fresh instance");

    // 4. A block added and the frame period changed: the framer renegotiates its output port,
    //    so `post` is rebuilt too (not reset); the output stream continues.
    doc["nodes"][1]["params"]["every"] = json!(2500);
    doc["nodes"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id": "extra", "block": "identity"}));
    let res = rt.edit(&id, parse_recipe(doc).unwrap()).unwrap();
    let extra = res["plan"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == "extra")
        .cloned()
        .unwrap();
    assert_eq!(extra["change"], json!("added"));
    assert_eq!(
        res["swap"],
        json!({"rebuilt": 3, "reset": 0, "updated": 0, "kept": 1}),
        "{res}"
    );
    let (_, _, new) = across_edit(&mut r, &mut last, 4, 4);
    let spacing: Vec<u64> = new
        .windows(2)
        .map(|w| {
            w[1]["metadata"]["sample_index"].as_u64().unwrap()
                - w[0]["metadata"]["sample_index"].as_u64().unwrap()
        })
        .collect();
    assert!(
        spacing
            .iter()
            .filter(|d| (24_990..=25_010).contains(*d))
            .count()
            >= 2,
        "the new period: {spacing:?}"
    );

    // An invalid draft is refused with paths; the running revision is untouched.
    let e = rt
        .edit(&id, parse_recipe(recipe("tone", 1 << 21, 2500)).unwrap())
        .unwrap_err();
    assert_eq!((e.status, e.code), (400, "invalid"));
    assert!(
        e.errors
            .iter()
            .any(|x| x.path.starts_with("nodes[1].params")),
        "{e}"
    );
    let p = rt.pipeline_json(&id).unwrap();
    assert_eq!(p["edit_rev"], json!(4));
    assert_eq!(p["nodes"].as_array().unwrap().len(), 4);

    // Capture never stopped and no ring sample was lost or skipped.
    wait("capture to continue", LIMIT, || {
        counters.source.samples.load(Ordering::Relaxed) > samples_before + 2_000_000
    });
    let stats = rt.stats_json(&id).unwrap();
    assert_eq!(stats["gaps"], json!(0), "{stats}");
    assert_eq!(stats["skipped_samples"], json!(0), "{stats}");
    let (disc, from_source) = (
        stats["discontinuities"].as_u64().unwrap(),
        stats["source_discontinuities"].as_u64().unwrap(),
    );
    assert_eq!(
        disc - from_source,
        1,
        "only the stream start (and the recording's loop splices): {stats}"
    );
    assert_eq!(stats["edits"], json!(4));
    assert_eq!(counters.chains.lost_samples.load(Ordering::Relaxed), 0);
    let mock = run.driver.last_control().unwrap().mock_stats();
    assert_eq!(
        (mock.source.overruns, mock.source.dropped_samples),
        (0, 0),
        "{mock:?}"
    );
    let status = &rt.pipeline_json(&id).unwrap()["status"];
    assert_eq!(
        status["pre.lock"],
        json!("none"),
        "status keeps flowing: {status}"
    );
    drop(r);
    drop(tcp);
    run.finish();
}

#[test]
fn recipes_are_saved_listed_and_rerun_on_a_selection() {
    let run = Run::start("t088-store");
    let rt = &run.rt;
    assert_eq!(
        rt.save_json(recipe("tone", 1, 5000)).unwrap()["version"],
        json!(1)
    );
    assert_eq!(
        rt.save_json(recipe("tone", 3, 5000)).unwrap()["version"],
        json!(2)
    );
    let e = rt.save_json(recipe("tone", 1 << 21, 5000)).unwrap_err();
    assert_eq!(e.status, 400);
    assert!(!e.errors.is_empty());
    let e = rt
        .save_json(json!({"schema": "hackriff.recipe"}))
        .unwrap_err();
    assert_eq!((e.status, e.code), (400, "invalid"));
    let list = rt.recipes_json();
    let tone = list["recipes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "tone")
        .cloned()
        .unwrap();
    assert_eq!(tone["versions"], json!([1, 2]), "{list}");
    assert_eq!(tone["version"], json!(2));
    assert_eq!(
        rt.recipe_json("tone", Some(1)).unwrap()["nodes"][1]["params"]["tag"],
        json!(1)
    );

    // Re-run the saved recipe on a persisted selection (by id), latest and a pinned version.
    let sel = Selection::new("tone".to_owned(), TONE_HZ - 10e3, TONE_HZ + 10e3);
    Repository::open(run.dir.0.join("hackriff.db"))
        .unwrap()
        .insert_selection(&sel)
        .unwrap();
    let target = json!({"selection_id": sel.id.to_string()});
    let p = rt
        .start_json(json!({"recipe_id": "tone", "target": target}))
        .unwrap();
    assert_eq!(p["recipe_version"], json!(2), "{p}");
    assert_eq!(p["target"], target);
    let id = p["id"].as_str().unwrap().to_owned();
    let p1 = rt
        .start_json(json!({"recipe_id": "tone", "version": 1, "target": target}))
        .unwrap();
    assert_eq!(p1["recipe_version"], json!(1));
    let id1 = p1["id"].as_str().unwrap().to_owned();
    wait("frames from the re-run", LIMIT, || run.frames(&id) > 2);

    // Save the running (edited) revision as the next version.
    rt.edit(&id, parse_recipe(recipe("tone", 9, 5000)).unwrap())
        .unwrap();
    let saved = rt.save_pipeline_json(&id).unwrap();
    assert_eq!(saved["version"], json!(3));
    assert_eq!(
        rt.recipe_json("tone", None).unwrap()["nodes"][1]["params"]["tag"],
        json!(9)
    );
    assert_eq!(rt.pipeline_json(&id).unwrap()["recipe_version"], json!(3));

    // Unknown targets and recipes.
    let unknown = Selection::new("x".to_owned(), 1.0, 2.0);
    let e = rt
        .start_json(
            json!({"recipe_id": "tone", "target": {"selection_id": unknown.id.to_string()}}),
        )
        .unwrap_err();
    assert_eq!(e.status, 404);
    let e = rt
        .start_json(json!({"recipe_id": "nope", "target": {"band": {"f_lo": TONE_HZ, "f_hi": TONE_HZ + 1e3}}}))
        .unwrap_err();
    assert_eq!(e.status, 404);
    let e = rt
        .start_json(json!({"recipe_id": "tone", "target": {"band": {"f_lo": CENTER_HZ + 6e5, "f_hi": CENTER_HZ + 6.1e5}}}))
        .unwrap_err();
    assert_eq!((e.status, e.code), (409, "outside_window"));

    let stopped = rt.stop_json(&id1).unwrap();
    assert_eq!(stopped["stopped"]["state"], json!("ended"));
    assert_eq!(stopped["stopped"]["end_reason"], json!("stopped"));
    rt.stop_json(&id).unwrap();
    assert_eq!(rt.pipeline_json(&id).unwrap_err().status, 404);
    assert_eq!(
        rt.delete_recipe_json("tone").unwrap()["deleted_versions"],
        json!([1, 2, 3])
    );
    run.finish();
}

#[test]
fn parallel_pipelines_run_at_once_within_the_chain_budget() {
    let run = Run::start("t088-parallel");
    let rt = &run.rt;
    let mut s = run.handle().listen_settings();
    s.max_chains = 2;
    s.max_listeners = 1;
    s.max_taps = 1;
    s.cpu_fraction = 64.0;
    run.handle().set_listen_settings(s);
    let a = rt
        .start(parse_recipe(recipe("a", 1, 5000)).unwrap(), band())
        .unwrap();
    let b = rt
        .start(parse_recipe(recipe("b", 2, 2500)).unwrap(), band())
        .unwrap();
    let e = rt
        .start(parse_recipe(recipe("c", 3, 5000)).unwrap(), band())
        .unwrap_err();
    assert_eq!((e.status, e.code), (503, "busy"), "{e}");
    wait("both pipelines produce frames", LIMIT, || {
        run.frames(&a) > 3 && run.frames(&b) > 6
    });
    let counters = run.handle().counters();
    assert_eq!(counters.to_json()["budget"]["chains"], json!(2));
    let all = rt.pipelines_json();
    assert_eq!(all["pipelines"].as_array().unwrap().len(), 2, "{all}");
    // Their streams are separate.
    assert!(
        run.streams
            .handle(&format!("inspector/{a}/frames"))
            .is_some()
    );
    assert!(
        run.streams
            .handle(&format!("inspector/{b}/frames"))
            .is_some()
    );
    rt.stop_json(&a).unwrap();
    wait("the slot to free", LIMIT, || {
        counters.to_json()["budget"]["chains"] == json!(1)
    });
    let c = rt
        .start(parse_recipe(recipe("c", 3, 5000)).unwrap(), band())
        .unwrap();
    wait("the third pipeline to produce frames", LIMIT, || {
        run.frames(&c) > 1
    });
    assert!(run.frames(&b) > 6, "b kept running");
    run.finish();
}
