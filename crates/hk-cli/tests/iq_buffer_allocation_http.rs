//! T-217 item 1: HTTP-level proof that `hk serve` answers while the IQ capture ring is still
//! allocating a large quota in the background. T-178 proved the background-allocation behaviour
//! only at the pipeline level (`crates/hk-pipeline/tests/iq_capture_ring_allocation.rs`); this
//! drives it through the real HTTP API `hk serve` exposes, over the mock SDR device, as
//! `docs/api.md` "IQ capture buffer" documents.
//!
//! **Disk discipline.** The staging quota this exercises (`--iq-retention 1h` at 20 Msps, 144 GB,
//! docs/api.md) is never actually reserved or written: `GatedAllocation` is a mocked
//! [`hk_store::iqbuffer::IqBufferHooks`] plumbed in through the CLI's test-only
//! [`hk_cli::pipeline::IqBufferHooksOverride`] seam ([`ServeOptions::iq_buffer_hooks`]). It makes a
//! sparse file (`file.set_len`, never writing data) and, after its first ~1 GiB step, blocks until
//! the test releases it, mirroring the pipeline-level T-178 test's `GatedAllocation`.

use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use hk_cli::pipeline::{
    IqBufferArgs, IqBufferHooksOverride, LiveArgs, TempDataDirGuard, temp_data_dir,
};
use hk_cli::serve::{ServeOptions, ServeSource, Serving, start};
use hk_store::iqbuffer::{FsSpace, IqBufferHooks};
use serde_json::Value;

const TOKEN: &str = "t217-alloc-http-token-0123456789abcdef";

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/hackrf/2026-09-13/fm_100p8M_2p4M_l32g30a1_t1p5_5s.sigmf-meta")
}

/// A huge filesystem whose ring allocation makes a sparse file and, after its first step, blocks
/// until the test releases it (same shape as
/// `crates/hk-pipeline/tests/iq_capture_ring_allocation.rs`'s `GatedAllocation`, one level up
/// through the CLI's `hk serve` composition instead of a raw `PipelineConfig`).
#[derive(Default)]
struct GatedAllocation {
    released: Mutex<bool>,
    cv: Condvar,
    steps: Mutex<u64>,
}

impl GatedAllocation {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.cv.notify_all();
    }
}

impl IqBufferHooks for GatedAllocation {
    fn fs_space(&self, _: &Path) -> io::Result<FsSpace> {
        Ok(FsSpace {
            free: 2 << 40,
            total: 4 << 40,
        })
    }

    fn preallocate(&self, file: &File, len: u64) -> io::Result<bool> {
        let step = {
            let mut s = self.steps.lock().unwrap();
            *s += 1;
            *s
        };
        if step > 1 {
            let mut r = self.released.lock().unwrap();
            while !*r {
                r = self.cv.wait(r).unwrap();
            }
        }
        // A sparse `set_len`, never writing 144 GB of real data or reserving real blocks.
        file.set_len(len)?;
        Ok(false)
    }
}

/// `METHOD path` with a bearer token and an optional JSON body; returns the status and the parsed
/// body (`Value::Null` if the body is empty or not JSON).
fn call(addr: SocketAddr, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let body = body.unwrap_or("");
    let ct = if body.is_empty() {
        String::new()
    } else {
        format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        )
    };
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {TOKEN}\r\n{ct}Connection: close\r\n\r\n{body}"
    )
    .unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let status = raw
        .get(9..12)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("malformed response head: {raw:?}"));
    let body = raw.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

fn get(addr: SocketAddr, path: &str) -> (u16, Value) {
    call(addr, "GET", path, None)
}

fn post(addr: SocketAddr, path: &str, body: &str) -> (u16, Value) {
    call(addr, "POST", path, Some(body))
}

fn stop_server(serving: Serving) {
    serving.handle.stop();
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = serving.handle;
    let waiter = std::thread::spawn(move || {
        let _ = tx.send(handle.wait());
        // T-236: `handle` drops after the send, so the pipeline's stores tear down (and write) on
        // this thread. Join it before the guard removes the data directory.
    });
    let _ = rx.recv_timeout(Duration::from_secs(30));
    let _ = waiter.join();
    drop(serving.server);
}

#[test]
fn hk_serve_answers_status_and_capture_routes_while_a_144gb_ring_allocates() {
    let gate = Arc::new(GatedAllocation::default());
    let dir = temp_data_dir();
    // T-232: this test previously cleaned up with a bare `remove_dir_all` at the end, which a
    // panic (or an early `assert!` failure) would skip entirely — this file's guard was the one
    // call site of eight that lacked it. `TempDataDirGuard` also keeps the directory on failure
    // for inspection, matching every other `temp_data_dir()` call site in this crate.
    let _guard = TempDataDirGuard::new(dir.clone());
    let start_t = Instant::now();
    let serving = start(&ServeOptions {
        source: ServeSource::HackRf {
            spec: format!("mock:{}", fixture_path().display()),
            live: LiveArgs::default(),
            extra: Vec::new(),
        },
        data_dir: Some(dir.clone()),
        // An ephemeral loopback port: never one of the fixed ports a demo or another test binds.
        bind: "127.0.0.1:0".parse().unwrap(),
        ui_dist: None,
        fft_len: 1024,
        rows_per_s: 25.0,
        calibration: None,
        token: Some(TOKEN.into()),
        listen: Default::default(),
        compute: Default::default(),
        // A 1 h retention at the mock's highest configurable rate (20 Msps, from the HackRF One
        // capability descriptor mock devices share) is the documented 144 GB staging quota
        // (docs/api.md "IQ capture buffer"). The gate above stands in for the real allocator, so
        // opening this never reserves or writes that much real disk.
        iq_buffer: IqBufferArgs {
            retention_s: Some(3600.0),
            max_bytes: None,
        },
        iq_buffer_hooks: Some(IqBufferHooksOverride(gate.clone())),
    })
    .unwrap();
    assert!(
        start_t.elapsed() < Duration::from_secs(10),
        "hk serve start blocked on allocation: {:?}",
        start_t.elapsed()
    );
    let addr = serving.server.local_addr();

    // /api/status and the capture routes answer within about 1 s while allocation is held open by
    // the gate (never releasing until the assertions below are done).
    for path in ["/api/status", "/api/iqbuffer"] {
        let t = Instant::now();
        let (st, v) = get(addr, path);
        assert_eq!(st, 200, "{path}: {v}");
        assert!(
            t.elapsed() < Duration::from_secs(1),
            "{path} took {:?} while allocating",
            t.elapsed()
        );
    }

    let (st, v) = get(addr, "/api/iqbuffer");
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["enabled"], false, "{v}");
    assert_eq!(v["allocation"], "allocating", "{v}");
    assert!(v["allocation_progress"].as_f64().is_some(), "{v}");
    assert_eq!(v["quota_bytes"].as_u64(), Some(144_000_000_000), "{v}");

    // A clip request is unavailable (503, the documented code) while the ring has no writer yet.
    let t = Instant::now();
    let (st, v) = post(
        addr,
        "/api/iqbuffer/clip",
        r#"{"global_index": 0, "samples": 1000}"#,
    );
    assert_eq!(st, 503, "{v}");
    assert_eq!(v["code"], "unavailable", "{v}");
    assert!(
        t.elapsed() < Duration::from_secs(1),
        "the clip request took {:?} while allocating",
        t.elapsed()
    );

    // T-217 item 2: capture is not buffered while allocating, and the samples the mock produced
    // meanwhile are counted, not silently missing, so a user can see why a large ring's first
    // minutes hold no IQ. The feeder waits up to `ALLOCATION_WAIT` (2 s) before it starts reading
    // at all, so poll rather than assume a fixed delay is enough.
    let deadline = Instant::now() + Duration::from_secs(15);
    let skipped = loop {
        let (st, v) = get(addr, "/api/iqbuffer");
        assert_eq!(st, 200, "{v}");
        assert_eq!(
            v["allocation"], "allocating",
            "{v} (gate released too early)"
        );
        let skipped = v["allocation_skipped_samples"]
            .as_u64()
            .unwrap_or_else(|| panic!("allocation_skipped_samples missing: {v}"));
        if skipped > 0 {
            break skipped;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for allocation_skipped_samples to grow: {v}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(skipped > 0, "expected samples skipped while allocating");

    gate.release();
    stop_server(serving);
}

/// **T-920: while the ring allocates, `/api/coverage` already answers — and says the ring is not
/// the one answering.**
///
/// The failure this pins, observed on a loaded Linux host and never on a quiet Mac: the IQ ring
/// opens on a background thread (T-178/T-217), while the **open dwell** (T-596) rasterises the
/// tuned band from the first poll. So there is a real window — microseconds on an idle box, whole
/// seconds under load or with a large quota — in which `/api/coverage` reports
/// `any.observed_cells > 0` with `sources[iq-ring].available: false`. A test that waits on the
/// observed count and then asserts something about the ring walks straight into it; that is
/// exactly what `api_contract::coverage_greys_only_what_was_never_observed_and_names_the_device_that_looked`
/// did, 4/4 red on node2 and green on the Mac, for a reason that has nothing to do with Linux.
///
/// Two things are asserted, and the first is the one that makes the second matter:
///
///  1. the window is **real** — coverage genuinely answers `observed` while the ring is held in
///     `allocating`, from the open dwell alone;
///  2. the ring's row **says so**: `available: false` with `state: "allocating"` and the ring's
///     own reason — never a bare `false` a client has to guess at. That is T-920's product
///     change, proved through the real HTTP API rather than at the seam.
///
/// RED before the change: (2) finds no `state` or `reason` key at all on the row.
#[test]
fn coverage_answers_while_the_ring_allocates_and_the_ring_row_says_it_is_not_answering() {
    let gate = Arc::new(GatedAllocation::default());
    let dir = temp_data_dir();
    let _guard = TempDataDirGuard::new(dir.clone());
    let serving = start(&ServeOptions {
        source: ServeSource::HackRf {
            spec: format!("mock:{}", fixture_path().display()),
            live: LiveArgs::default(),
            extra: Vec::new(),
        },
        data_dir: Some(dir.clone()),
        bind: "127.0.0.1:0".parse().unwrap(),
        ui_dist: None,
        fft_len: 1024,
        rows_per_s: 25.0,
        calibration: None,
        token: Some(TOKEN.into()),
        listen: Default::default(),
        compute: Default::default(),
        // The same 144 GB staging quota the test above uses, so the gate holds allocation open
        // for as long as the assertions need without reserving or writing any of it.
        iq_buffer: IqBufferArgs {
            retention_s: Some(3600.0),
            max_bytes: None,
        },
        iq_buffer_hooks: Some(IqBufferHooksOverride(gate.clone())),
    })
    .unwrap();
    let addr = serving.server.local_addr();

    // The fixture's own band (100.8 MHz ± 1.2 MHz): what the mock front end is tuned to, and what
    // the dwell in flight is therefore a record of.
    let (lo, hi) = (100.8e6 - 1.2e6, 100.8e6 + 1.2e6);
    let query = format!("/api/coverage?f_lo={lo}&f_hi={hi}&cells=8");
    let deadline = Instant::now() + Duration::from_secs(30);
    let coverage = loop {
        let (st, v) = get(addr, &query);
        // The gate must still be holding allocation open, or this proves nothing about the
        // window — it would just be a normal, fully-opened ring.
        let (_, b) = get(addr, "/api/iqbuffer");
        assert_eq!(
            b["allocation"], "allocating",
            "the ring finished opening before the window could be observed (gate released \
             early?): {b}"
        );
        if st == 200 && v["any"]["observed_cells"].as_u64().unwrap_or(0) > 0 {
            break v;
        }
        assert!(
            Instant::now() < deadline,
            "coverage never reported an observed cell while the ring allocated: {v}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };

    let src = |kind: &str| -> Value {
        coverage["sources"]
            .as_array()
            .expect("sources")
            .iter()
            .find(|s| s["kind"] == kind)
            .unwrap_or_else(|| panic!("the {kind} source row: {coverage}"))
            .clone()
    };

    // 1. The window is real: the band reads observed, and it is NOT the ring that said so.
    let open = src("open-dwell");
    assert!(
        open["spans"].as_u64().unwrap_or(0) > 0,
        "the dwell in flight is what carried the tuned band here: {coverage}"
    );
    assert_eq!(src("iq-ring")["spans"], 0, "{coverage}");

    // 2. And the ring's row states which negative it is, in the ring's own words.
    let ring = src("iq-ring");
    assert_eq!(ring["available"], false, "{coverage}");
    assert_eq!(
        ring["state"], "allocating",
        "a ring that is still being laid down must say so, not serve a bare `available: false` \
         a client cannot tell from a refusal: {coverage}"
    );
    let (_, iq) = get(addr, "/api/iqbuffer");
    assert_eq!(
        ring["reason"], iq["reason"],
        "the coverage row quotes the ring's own reason, the one /api/iqbuffer is serving: \
         {coverage}"
    );
    assert!(
        ring["reason"]
            .as_str()
            .is_some_and(|r| r.contains("allocating")),
        "{coverage}"
    );

    // 3. Non-vacuity: once the ring is open the same row flips, so none of the above passes by
    //    the server simply never calling a ring available.
    gate.release();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (st, v) = get(addr, &query);
        if st == 200 {
            let ring = v["sources"]
                .as_array()
                .expect("sources")
                .iter()
                .find(|s| s["kind"] == "iq-ring")
                .cloned()
                .expect("the iq-ring row");
            if ring["available"] == true {
                assert_eq!(ring["state"], "open", "{v}");
                assert_eq!(ring["reason"], Value::Null, "{v}");
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the ring never became available after the gate was released: {v}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    stop_server(serving);
}
