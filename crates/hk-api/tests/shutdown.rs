//! T-236: `Server::shutdown` waits for in-flight connection threads, bounded.
//!
//! Before T-236 every accepted connection ran on a detached thread `shutdown` never joined, so a
//! caller that stopped its server and then deleted the run's data directory raced a handler still
//! writing to a file under it (T-232 measured that race leaking 26 of 26 server-starting `hk-cli`
//! tests under load, and worked around it with a retry backoff in the test guard). These tests
//! assert the three properties that close it at the root: the wait actually happens, a stalled
//! peer can't extend it, and a handler stuck in compute is abandoned at the deadline and counted.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use hk_api::{ApiState, Server, ServerConfig, Token};
use serde_json::json;

const TOKEN: &str = "t236-shutdown-token-0123456789abcd";

/// A private scratch directory for one iteration (never the shared `hk-replay-*` convention:
/// nothing here is a pipeline data dir).
fn scratch(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "hk-api-t236-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn serve(state: ApiState, request_timeout: Duration) -> Server {
    let mut config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        Token::from_config(TOKEN).unwrap(),
    );
    config.request_timeout = request_timeout;
    Server::start(config, state).unwrap()
}

/// Sends a request and drops the connection without reading the response, so the handler thread is
/// still running (still touching whatever the handler touches) when the server is shut down.
fn fire_and_forget(addr: SocketAddr, path: &str) {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let _ = s.flush();
}

/// T-236: after `shutdown` returns, no connection thread is still writing under the directory the
/// handlers use — removing it succeeds on the *first* attempt, with no retry. Repeated, with
/// several unread requests in flight each time, so it is meaningful on a loaded machine.
#[test]
fn shutdown_leaves_no_thread_writing_under_the_handlers_directory() {
    for i in 0..10 {
        let dir = scratch("dir-free");
        let writes = Arc::new(AtomicU64::new(0));
        let mut state = ApiState::default();
        let (d, w) = (dir.clone(), Arc::clone(&writes));
        // A status handler that behaves like the real ones the leak came from (the audit log,
        // SQLite's WAL, the observation log): it writes files under the run's directory, and takes
        // long enough that it is still doing so when the server is told to stop.
        state.status = Some(Arc::new(move || {
            let n = w.fetch_add(1, Ordering::SeqCst);
            for k in 0..8 {
                let _ = std::fs::write(d.join(format!("work-{n}-{k}")), b"x");
                std::thread::sleep(Duration::from_millis(2));
            }
            json!({"ok": true})
        }));
        let mut server = serve(state, Duration::from_secs(10));
        let addr = server.local_addr();
        for _ in 0..8 {
            fire_and_forget(addr, "/api/status");
        }
        // Give the handlers time to start writing, then stop while they are mid-flight.
        std::thread::sleep(Duration::from_millis(5));
        server.shutdown();

        assert_eq!(
            server.abandoned_connections(),
            0,
            "iteration {i}: shutdown gave up on a connection that was only writing files"
        );
        std::fs::remove_dir_all(&dir).unwrap_or_else(|e| {
            let left: Vec<_> = std::fs::read_dir(&dir)
                .map(|rd| rd.flatten().map(|e| e.file_name()).collect())
                .unwrap_or_default();
            panic!(
                "iteration {i}: the directory was still busy on the first removal attempt after \
                 shutdown returned: {e} (entries: {left:?})"
            )
        });
        assert!(!dir.exists());
        assert!(writes.load(Ordering::SeqCst) > 0, "no handler ever ran");
    }
}

/// T-236: a client that opens a connection, sends half a request head and then stalls must not
/// hold shutdown for the request timeout. Its socket is closed at the deadline-free first step of
/// the drain, so the handler's read fails at once and the connection is never counted as
/// abandoned.
#[test]
fn a_stalled_client_does_not_hold_shutdown() {
    let state = ApiState {
        status: Some(Arc::new(|| json!({"ok": true}))),
        ..Default::default()
    };
    // A request timeout far longer than the drain budget: if shutdown waited for the peer rather
    // than closing its socket, this test would take a minute.
    let mut server = serve(state, Duration::from_secs(60));
    let addr = server.local_addr();

    let mut stalled = TcpStream::connect(addr).unwrap();
    write!(stalled, "GET /api/status HTTP/1.1\r\nHost: t\r\n").unwrap();
    stalled.flush().unwrap();
    // The half-sent head is parked in the server's read; give the handler time to block on it.
    std::thread::sleep(Duration::from_millis(50));

    let started = Instant::now();
    server.shutdown();
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(5),
        "shutdown waited {took:?} on a stalled client"
    );
    assert_eq!(server.abandoned_connections(), 0);
    // The peer sees the close rather than an open connection to a stopped server.
    let mut rest = Vec::new();
    stalled
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let _ = stalled.read_to_end(&mut rest);
}

/// T-236: the wait is bounded even when closing the socket cannot help — a handler stuck in
/// compute. Shutdown returns at its deadline, the thread is counted, and the count is visible.
#[test]
fn a_handler_stuck_in_compute_is_abandoned_at_the_deadline_and_counted() {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
    let state = ApiState {
        status: Some(Arc::new(move || {
            let _ = entered_tx.send(());
            // Not blocked on the socket: closing the connection cannot end this.
            let rx = release_rx.lock().unwrap();
            let _ = rx.recv_timeout(Duration::from_secs(60));
            json!({"ok": true})
        })),
        ..Default::default()
    };
    let mut server = serve(state, Duration::from_secs(10));
    let addr = server.local_addr();
    fire_and_forget(addr, "/api/status");
    entered_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the handler ran");

    let started = Instant::now();
    server.shutdown();
    let took = started.elapsed();
    assert!(
        (Duration::from_secs(1)..Duration::from_secs(10)).contains(&took),
        "shutdown returned after {took:?}: it should give up at its bounded deadline, \
         neither immediately nor never"
    );
    assert_eq!(
        server.abandoned_connections(),
        1,
        "the stuck connection should be counted"
    );
    // Let the stuck handler finish rather than leaving it parked for a minute.
    let _ = release_tx.send(());
}
