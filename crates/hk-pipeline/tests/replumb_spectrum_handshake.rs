//! **T-530 — a re-plumb is a gap in `spectrum/live`, not the end of it.**
//!
//! T-525 fixed the *length* of the gap a re-plumb leaves in the live spectrum stream (21.43 s of
//! unbroken non-101 handshakes on the live HackRF, 0.17 s after) and left the *answer given during
//! it* alone, deliberately: for that gap `/ws/spectrum/live` answered **410 Gone, "stream
//! finished"**, and the run had not finished — it was between segments.
//!
//! That is not cosmetic. `ops/stage.sh`'s `healthy()` is `/` **plus that handshake**, twice, three
//! seconds apart; a long enough 410 window is "hk serve crashed" as the user experiences it, and
//! the demo gets restarted under them. And `410` is the wrong word whatever is watching: it means
//! the resource is permanently gone, so a client that believes it is right to stop trying.
//!
//! **The contract these tests hold to.** A segment's end is not the stream's end. The producer
//! says which it is ([`hk_stream::Publisher::finish_between_windows`] vs `finish`), and the
//! handshake honours it: between windows it waits for the successor publisher and upgrades
//! normally, or — if the successor has still not arrived when its bound runs out — refuses with
//! `503 replumbing`, which means *not now*. Only a run that has really ended answers `410`. It is
//! the same rule [`hk_api::bridge`] already applied to a consumer that was **already attached**
//! (T-417/T-425): a registered id whose publisher finished is between windows. What was missing is
//! that the rule did not reach the consumer that arrives *during* the gap.
//!
//! Driven through the generic device contract with the scripted radio (CLAUDE.md), and through the
//! real WebSocket front end — the same request `stage.sh` makes with curl, byte for byte.
//!
//! **Non-vacuity.** Both tests are RED without the fix: `a_handshake_during_a_replumb_is_not_410`
//! prints `410 / stream finished` where it expects `101`/`503`, and `a_finished_run_still_410`
//! passes either way (it is the guard that the fix did not make a real end look like a gap).

mod common;
#[path = "support/http_response.rs"]
mod http_response;
#[path = "support/radio.rs"]
mod radio;

use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use hk_api::{ApiState, Server, ServerConfig, StreamRegistry, Token};
use hk_model::Timestamp;
use hk_pipeline::class::window_class;
use hk_pipeline::{
    Pipeline, PipelineConfig, PipelineHandle, SourceInfo, TrackInventory, replay_plan,
};

const CENTER: f64 = 433.92e6;
const FS: f64 = 500e3;
/// A different rate, so the retune re-plumbs (a same-class, same-rate tune is applied in place and
/// never ends a segment).
const REPLUMB_RATE: f64 = FS * 2.0;
const STREAM_ID: &str = "spectrum/live";
const TOKEN: &str = "t530-replumb-spectrum-handshake-token-0123456789";
const WARMUP_LIMIT: Duration = Duration::from_secs(60);

/// One `/ws/spectrum/live` handshake, byte for byte the request `ops/stage.sh`'s `healthy()`
/// makes. Returns `(status, the response text)`; the socket is dropped straight after, so a
/// successful upgrade ends as a hang-up, like a health check that got what it came for.
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
    // The head and the body: `stream finished` / `replumbing` are in the body, which the server
    // writes separately from the head (see `support/http_response.rs`).
    http_response::read_response(s)
}

/// The answer must never be "this stream is gone" while the run is still capturing.
fn assert_not_an_end(status: u16, text: &str, when: &str) {
    assert_ne!(
        status, 410,
        "THIS IS T-530: {when}, /ws/{STREAM_ID} answered 410 Gone — the resource is permanently \
         gone — while the run was between segments and still capturing. `ops/stage.sh` reads that \
         as a crashed server and restarts the demo under the user. Response:\n{text}"
    );
    assert!(
        status == 101 || status == 503,
        "{when}: expected the upgrade (101) or a retryable refusal (503 replumbing), got \
         {status}:\n{text}"
    );
    if status == 503 {
        assert!(
            text.contains("replumbing"),
            "{when}: a 503 during a re-plumb must say so, or a client cannot tell it from the \
             consumer cap:\n{text}"
        );
    }
}

/// A run of the scripted radio whose spectrum stream is offered through the real bridge registry
/// and served by the real HTTP/WebSocket front end.
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
        // What `hk serve` runs: a live, window-classed run, so a rate change re-plumbs.
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
        // Far above anything this test needs: a probe every 100 ms for five seconds is ~50
        // connections, and the server's default cap of 64 drops a connection past it **silently**
        // (`accept_loop`), which reads as an empty response and has nothing to do with the
        // contract under test.
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

    /// Waits until the live spectrum stream is offered and upgrades cleanly.
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
}

/// Reads that the radio makes without looking at its control mailbox, so the re-plumbed segment's
/// window guard has nothing to admit and its spectrum reader no first chunk to write a header
/// from. A control path slower than a block is the ordinary shape of a real front end (T-525); it
/// is used here only to hold the measured 0.17 s gap still long enough to aim at.
const HOLD_READS: u64 = 2000;
/// Attempts at catching the gap open. The radio is read on demand, so how long `HOLD_READS` lasts
/// is not a fact about the clock; a gap that closed before the probe is retried rather than
/// asserted away.
const GAP_ATTEMPTS: usize = 5;

#[test]
fn a_handshake_during_a_replumb_is_not_410() {
    let run = Run::start("t530-replumb-gap");
    let addr = run.server.local_addr();
    let controller = run.handle.controller();

    let mut in_gap = 0;
    for attempt in 0..GAP_ATTEMPTS {
        // Alternating rates, so every attempt is a real re-plumb rather than an in-place tune.
        let (center, rate) = if attempt % 2 == 0 {
            (CENTER + 600e3, REPLUMB_RATE)
        } else {
            (CENTER - 600e3, FS)
        };
        run.ctl.hold_changes(HOLD_READS);
        let before = run.generation();
        let out = controller
            .retune(center, rate)
            .expect("the re-plumb is answered");
        assert!(
            out.replumbed,
            "this retune must re-plumb, or no segment ended and there is no gap to test: {out:?}"
        );
        // `retune` returns only once the old segment's threads have let go of its state, so its
        // spectrum reader has finished its publisher. An unchanged generation means no successor
        // has been offered under the id yet: the gap is open, established without asking the code
        // under test anything.
        if run.generation() == before {
            let (status, text) = handshake(addr);
            assert_not_an_end(status, &text, "during a re-plumb");
            eprintln!("t530: in the gap, /ws/{STREAM_ID} answered {status}");
            in_gap += 1;
        }
        // Let the front end take its window: the successor is offered and the stream serves again.
        run.ctl.hold_changes(0);
        let deadline = Instant::now() + WARMUP_LIMIT;
        while run.generation() == before {
            assert!(
                Instant::now() < deadline,
                "the re-plumbed segment never offered its spectrum publisher"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let (status, text) = handshake(addr);
        assert_eq!(status, 101, "after the re-plumb settled:\n{text}");
    }
    assert!(
        in_gap > 0,
        "the gap never stayed open long enough to probe in {GAP_ATTEMPTS} re-plumbs, so this test \
         proved nothing"
    );

    // …and the ordinary case: a re-plumb nobody is holding up, probed continuously. This is the
    // shape `ops/stage.sh` actually meets, and every answer must be an upgrade or a retry.
    let probe = std::thread::spawn(move || {
        let mut seen = Vec::new();
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until {
            let (status, text) = handshake(addr);
            // This test's own probes piling up behind the consumer cap is not its subject.
            if status == 503 && text.contains("consumer limit") {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            seen.push((status, text));
            std::thread::sleep(Duration::from_millis(100));
        }
        seen
    });
    // The loop above left the front end on `REPLUMB_RATE` (its last attempt was an even one), so
    // going back to `FS` is a rate change and re-plumbs; the same centre and rate would be applied
    // in place and end no segment.
    let out = controller
        .retune(CENTER + 1.2e6, FS)
        .expect("the unheld re-plumb is answered");
    assert!(
        out.replumbed,
        "the last probe must cross a real re-plumb: {out:?}"
    );
    let seen = probe.join().unwrap();
    assert!(!seen.is_empty(), "the probe never ran");
    for (status, text) in &seen {
        assert_not_an_end(*status, text, "across an unheld re-plumb");
    }
    let upgraded = seen.iter().filter(|(s, _)| *s == 101).count();
    eprintln!(
        "t530: {} handshakes across an unheld re-plumb, {upgraded} upgraded, {} retryable",
        seen.len(),
        seen.len() - upgraded
    );
    assert!(upgraded > 0, "no handshake upgraded at all");

    run.handle.stop();
    run.ctl.finish();
    run.ctl.run_free();
    let _ = run.handle.wait();
}

#[test]
fn a_finished_run_still_answers_410() {
    let run = Run::start("t530-finished-run");
    let addr = run.server.local_addr();
    let controller = run.handle.controller();

    let Run {
        handle,
        ctl,
        streams,
        server,
        _dir,
        ..
    } = run;
    ctl.finish();
    ctl.run_free();
    let (summary, fired) = wait_guarded(handle, Duration::from_secs(120));
    assert!(!fired, "the run had to be stopped by the watchdog");
    assert!(
        controller.status().finished,
        "the run did not finish: {}",
        summary.to_text()
    );

    // The source ended: nothing will be offered under this id again, and the honest answer is the
    // one T-508 made visible on the canvas — this run is over.
    //
    // *Why* it is known at once and not waited out, stated as ordering rather than as elapsed
    // time (T-602's pattern, `hk-api/tests/bridge.rs`): the between-windows wait is only reachable
    // through a park in `StreamRegistry::wait_for_offer_after`, counted by `successor_waits_entered`
    // before the park. A 410 that had been mistaken for a gap first would have entered that wait
    // (and, on this run, waited the settle gap out to its bound); a real end never enters it. Timing
    // the response instead made the answer a function of the machine.
    let waits_before = streams.successor_waits_entered();
    let (status, text) = handshake(addr);
    assert_eq!(
        status, 410,
        "a run that has really ended must still say so; a gap and an end are different things:\n\
         {text}"
    );
    assert!(
        text.contains("stream finished"),
        "the 410 must still name the reason:\n{text}"
    );
    assert_eq!(
        streams.successor_waits_entered(),
        waits_before,
        "the end was treated as a gap and waited out for a successor that was never promised"
    );
    drop(server);
}
