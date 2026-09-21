//! **T-541 — a degraded server is still alive to a supervisor.**
//!
//! T-530 established the rule for the gap a **re-plumb** leaves in `spectrum/live`: a segment's
//! end is not the stream's end, so the handshake answers `101` or `503 replumbing`, never `410
//! Gone`. `ops/stage.sh`'s `healthy()` is `/` plus exactly that handshake, and a long enough 410
//! window is "hk serve crashed" as the user experiences it — the demo gets restarted underneath
//! them for a fault the backend was already handling.
//!
//! **The same gap is left by a device failure, and T-530 did not cover it.** `Shared::continues`
//! — the run's record of "a successor is coming under this id" — was set only by
//! `PipelineController::retune`. A capture read that failed tore the segment down with `continues`
//! false, so every publisher called `finish()` and the handshake answered `410` for the *whole*
//! recovery: `recovery_backoff` alone is up to 7.75 s across `MAX_RECOVERY_ATTEMPTS`, against a
//! health check that probes twice, three seconds apart. A front end hiccup therefore restarted
//! the server.
//!
//! So the capture thread now sets `continues` itself when the run is one `supervise` will recover
//! (`capture::run`), with `RECOVERY_SUCCESSOR_GRACE` rather than the re-plumb's grace, because the
//! successor arrives on a backoff schedule rather than a handover the producer controls.
//!
//! Driven through the generic device contract with the scripted radio, and through the real
//! WebSocket front end — the same request `stage.sh` makes with curl, byte for byte.
//!
//! **Non-vacuity.** `a_handshake_during_a_capture_recovery_is_not_410` is RED without the
//! `capture.rs` change: it prints `410 / stream finished` where it expects `101`/`503`.
//! `a_run_that_really_ended_still_says_so` is the guard that the fix did not turn a real end into
//! a permanent "try again".

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use hk_api::{ApiState, Server, ServerConfig, StreamRegistry, Token};
use hk_model::Timestamp;
use hk_pipeline::class::window_class;
use hk_pipeline::{
    CaptureState, Pipeline, PipelineConfig, PipelineHandle, SourceInfo, TrackInventory, replay_plan,
};

const CENTER: f64 = 433.92e6;
const FS: f64 = 500e3;
const STREAM_ID: &str = "spectrum/live";
const TOKEN: &str = "t541-recovery-spectrum-handshake-token-0123456789";
const WARMUP_LIMIT: Duration = Duration::from_secs(60);

/// One `/ws/spectrum/live` handshake, byte for byte the request `ops/stage.sh`'s `healthy()`
/// makes. Returns `(status, the response text)`.
fn handshake(addr: SocketAddr) -> (u16, String) {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    write!(
        s,
        "GET /ws/{STREAM_ID}?token={TOKEN} HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    )
    .unwrap();
    read_status(s)
}

/// The other half of `healthy()`: a plain `GET /`. A recovering server must still serve it.
fn root(addr: SocketAddr) -> (u16, String) {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    write!(s, "GET / HTTP/1.1\r\nHost: {addr}\r\n\r\n").unwrap();
    read_status(s)
}

fn read_status(mut s: TcpStream) -> (u16, String) {
    let (mut got, mut buf) = (Vec::new(), [0u8; 4096]);
    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(e) => panic!("reading the response: {e}"),
        }
    }
    let text = String::from_utf8_lossy(&got).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {text:?} ({} bytes)", got.len()));
    (status, text)
}

/// The answer must never be "this stream is gone" while the run is still trying to capture.
fn assert_not_an_end(status: u16, text: &str, when: &str) {
    assert_ne!(
        status, 410,
        "THIS IS T-541: {when}, /ws/{STREAM_ID} answered 410 Gone — the resource is permanently \
         gone — while the run was recovering capture and had not finished. `ops/stage.sh` reads \
         that as a crashed server and restarts the demo under the user, for a device hiccup the \
         backend was already handling. Response:\n{text}"
    );
    assert!(
        status == 101 || status == 503,
        "{when}: expected the upgrade (101) or a retryable refusal (503), got {status}:\n{text}"
    );
}

struct Run {
    handle: PipelineHandle,
    ctl: Arc<radio::RadioControl>,
    streams: StreamRegistry,
    server: Server,
    _dir: TempDir,
}

impl Run {
    fn start(tag: &str) -> Self {
        let dir = TempDir::new(tag);
        let (rx, ctl) = radio::Radio::new(CENTER, FS, 16_384, radio::tone(|_| 80e3));
        let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
        let mut cfg = PipelineConfig::new(&dir.0, replay_plan(CENTER, FS, t0)).unwrap();
        cfg.source_class = window_class(CENTER, FS);
        // What `hk serve --device …` runs: a live, window-classed run, so a read error recovers.
        cfg.live_window_class = true;
        cfg.settings.chains = Some(Vec::new());
        let streams = StreamRegistry::new();
        let reg = streams.clone();
        cfg.stream_sink = Some(Arc::new(move |h, p| reg.register(h, p)));
        let handle = Pipeline::start(
            cfg,
            Box::new(rx),
            SourceInfo {
                sample_rate_hz: FS,
                center_hz: CENTER,
                start_time: t0,
            },
            None,
            Box::new(TrackInventory::default()),
        )
        .unwrap();
        let mut config = ServerConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            Token::from_config(TOKEN).unwrap(),
        );
        config.max_connections = 512;
        let server = Server::start(
            config,
            ApiState {
                streams: streams.clone(),
                ..ApiState::default()
            },
        )
        .unwrap();
        let run = Self {
            handle,
            ctl,
            streams,
            server,
            _dir: dir,
        };
        run.warm_up();
        run
    }

    fn warm_up(&self) {
        let deadline = Instant::now() + WARMUP_LIMIT;
        while self.streams.offer(STREAM_ID).is_none() {
            assert!(
                Instant::now() < deadline,
                "the spectrum stream was never offered"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let (status, text) = handshake(self.server.local_addr());
        assert_eq!(status, 101, "the stream does not serve at all:\n{text}");
    }

    fn generation(&self) -> u64 {
        self.streams
            .offer(STREAM_ID)
            .expect("the spectrum stream is offered")
            .generation
    }

    fn finish(self) {
        self.handle.stop();
        self.ctl.fail_reads(false);
        self.ctl.finish();
        self.ctl.run_free();
        let _ = self.handle.wait();
    }
}

/// How many reads fail. Enough that the recovery takes several attempts and its backoffs
/// (`recovery_backoff`: 0.25 s, 0.5 s, 1 s) hold the gap open for longer than the health check's
/// 3 s spacing — which is the whole reason the wrong answer mattered — without exhausting
/// `MAX_RECOVERY_ATTEMPTS`.
const FAILING_READS: u64 = 3;

#[test]
fn a_handshake_during_a_capture_recovery_is_not_410() {
    let run = Run::start("t541-recovery-gap");
    let addr = run.server.local_addr();
    let controller = run.handle.controller();
    let before = run.generation();

    // The device stalls. Nothing is retuned, nothing is asked of the control plane: this is the
    // run failing on its own, which is the case T-530 left uncovered.
    run.ctl.fail_reads_for(FAILING_READS);

    // Probe the way `healthy()` does, for as long as the recovery lasts, and afterwards.
    let mut seen: Vec<(u16, String)> = Vec::new();
    let mut in_gap = 0usize;
    let mut recovered_seen = false;
    let deadline = Instant::now() + WARMUP_LIMIT;
    while Instant::now() < deadline {
        let st = controller.status();
        let gap = run.generation() == before || st.capture == CaptureState::Recovering;
        let (status, text) = handshake(addr);
        if status == 503 && text.contains("consumer limit") {
            // This test's own probes piling up behind the consumer cap is not its subject.
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        if gap {
            in_gap += 1;
        }
        seen.push((status, text));
        // The root — the first half of `healthy()` — must be served throughout, degraded or not.
        let (rs, rt) = root(addr);
        assert_eq!(
            rs, 200,
            "a recovering server stopped serving `/`, which `ops/stage.sh` reads as dead:\n{rt}"
        );
        if st.capture == CaptureState::Running && run.generation() != before {
            recovered_seen = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    assert!(
        recovered_seen,
        "capture never came back from {FAILING_READS} failed reads: {:?}",
        controller.status()
    );
    assert!(
        in_gap > 0,
        "the recovery never stayed open long enough to probe, so this test proved nothing"
    );
    for (status, text) in &seen {
        assert_not_an_end(*status, text, "during a capture recovery");
    }
    let upgraded = seen.iter().filter(|(s, _)| *s == 101).count();
    eprintln!(
        "t541: {} handshakes across a capture recovery ({in_gap} of them in the gap), {upgraded} \
         upgraded, {} retryable",
        seen.len(),
        seen.len() - upgraded
    );
    assert!(
        controller.status().stats["capture_failures"]
            .as_u64()
            .unwrap_or(0)
            >= 1,
        "the failure must be reported, not merely survived: {:?}",
        controller.status()
    );
    run.finish();
}

/// The guard on the other side: a run that has **really ended** must not keep telling clients to
/// try again for ever. The between-windows claim is a bounded promise that expires by itself
/// (`hk_stream::Publisher::finish_between_windows_for`); what is asserted here is that a stopped
/// run ends the stream rather than leaving it in a permanent gap.
#[test]
fn a_run_that_really_ended_still_says_so() {
    let run = Run::start("t541-real-end");
    let addr = run.server.local_addr();
    run.handle.stop();
    run.ctl.finish();
    run.ctl.run_free();
    let _ = run.handle.wait();

    let deadline = Instant::now() + WARMUP_LIMIT;
    loop {
        let (status, text) = handshake(addr);
        if status == 410 {
            assert!(text.contains("finished"), "{text}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "a stopped run never told a new consumer the stream was over (last: {status}):\n{text}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
