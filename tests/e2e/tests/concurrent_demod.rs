//! T-071 (SIGNAL-062, AWARE-036): concurrent multi-signal demodulation, blind, through the **mock
//! SDR device** (truth stripped; only the test holds it).
//!
//! - **Three FM stations at once.** `fm_100p8M_2p4M_l32g30a1_t1p5_5s` holds one strong station
//!   (101.3 MHz, PI 1694); two synthetic RDS stations with their own PI and programme are added to
//!   the recording at 100.3 and 100.7 MHz (their truth joins the private truth). Looping behind the
//!   mock, stations are found by blind detection (`/api/inventory` rows wide enough for broadcast
//!   FM; no frequency is looked up) and three Listen chains run concurrently, each on its own
//!   refined channel with its own stream. Each stream carries its own station: its RDS PI is the
//!   private truth PI of the station at its channel, the three PIs differ, pairwise audio
//!   cross-correlation stays low, and a fourth Listen on the first emitter (the positive control)
//!   correlates highly only with the stream on the station its probe chose. Per-chain CPU, latency
//!   and drop counters are reported in `/api/status` `chain_stats` and the unified `budget`.
//! - **Closing one stream** leaves the others running: frames keep coming, and what each consumer
//!   read tiles one unbroken seq range — every record's seq or a "dropped N" marker declaring
//!   exactly the missing one. The chains themselves lose no input samples (`lost_samples == 0`,
//!   a real invariant here: the unpaced mock is pausable, so the run is lossless and the source
//!   back-pressures). Whether a *consumer queue* fills is not asserted at this tier — see T-433
//!   and the performance test at the bottom of the file.
//! - **Admission beyond the budget** (a runtime budget of 3 chains) refuses a Listen and a bits tap
//!   with 503 `busy` and the reason, and the running streams are unaffected.
//! - **Two FSK emitters.** Two synthetic sensors (different ids, ±120 kHz) are mixed into one
//!   recording; two bits taps on selections around the two blindly detected emitters stream
//!   concurrently over TCP, and every CRC-valid payload of a stream belongs to one emitter's truth,
//!   a different emitter per stream.
//! - **Dedupe.** The FM recording IQ-shifted +150 kHz puts the station between two raster channels:
//!   exactly one analog chain writes it (T-070 lets either neighbour take it).
//! - **Performance** (`#[ignore]`, run in release; the T5/bench tier of docs/10 §2): 3 WFM
//!   listeners and 2 FSK taps at 2.4 Msps in **real time** on the three-station scene (a cf32
//!   recording, quantised to ci8 by the capture thread); prints per-chain CPU and latency and the
//!   load average, and asserts the throughput claim itself — no chain loses input samples, no
//!   listener skips to stay live, no consumer queue overflows, and the listeners produce a full
//!   second of audio per second. That claim is only meaningful against a clock, so it lives here
//!   and not in the unpaced CI-tier tests above.

#[path = "acceptance/common.rs"]
mod common;

#[path = "acceptance/blind.rs"]
#[allow(dead_code)]
mod blind;

use std::collections::BTreeSet;
use std::io::{Cursor, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use blind::{BlindSource, blind_config, blind_live, private_truth, start};
use common::*;
use hk_api::{StreamRegistry, StreamServer, StreamServerConfig, Token};
use hk_core::{MockEnd, Pacing};
use hk_e2e::blind::strip_truth;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::InventoryQuery;
use hk_model::sigmf::Datatype;
use hk_pipeline::{
    Counters, ListenSettings, Pipeline, PipelineConfig, TrackInventory, open_mock_replay,
    replay_plan,
};
use hk_stream::record::parse_status_record;
use hk_stream::{
    Declared, OpenRefusal, OpenRequest, OpenedStream, OpenerRegistry, Record, StreamHeader,
    StreamOpener, StreamReader,
};
use serde_json::{Value, json};

const FM_FIXTURE: &str = "fm_100p8M_2p4M_l32g30a1_t1p5_5s";
const TAG: &str = "T-071";
const LIMIT: Duration = Duration::from_secs(900);
/// Longest wait to find stations or emitters blind.
const FIND_LIMIT: Duration = Duration::from_secs(300);
/// Distinct stations are at least this far apart (a broadcast FM channel).
const STATION_SEP_HZ: f64 = 150e3;

fn wait_for(what: &str, limit: Duration, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < limit, "[{TAG}] timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn request(params: &[(&str, String)]) -> OpenRequest {
    let q: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect();
    OpenRequest::from_query(&q, "t071-test")
}

/// Opens a stream, retrying only while the run is starting (409) or re-plumbing (503).
fn open(opener: &dyn StreamOpener, params: &[(&str, String)]) -> Result<OpenedStream, OpenRefusal> {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match opener.open(&request(params)) {
            Err(e) if (e.status == 409 || e.code == "replumbing") && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            r => return r,
        }
    }
}

/// A stream with an in-process consumer that keeps the wire bytes.
struct Sub {
    header: StreamHeader,
    buf: Buf,
    opened: Option<OpenedStream>,
}

impl Sub {
    fn new(o: OpenedStream) -> Self {
        let buf = Buf::default();
        o.handle
            .subscribe("t071", Declared::local(buf.clone()), Box::new(|_| {}))
            .unwrap();
        Self {
            header: o.header.clone(),
            buf,
            opened: Some(o),
        }
    }

    fn parse(&self) -> Parsed {
        let bytes = self.buf.0.lock().unwrap().clone();
        parse(&bytes)
    }
}

/// What a consumer read.
#[derive(Default)]
struct Parsed {
    /// `(t ns, payload)` of each data record.
    data: Vec<(i64, Vec<u8>)>,
    /// Sequence numbers of data and status records.
    seqs: Vec<u64>,
    dropped: u64,
    status: Vec<Value>,
    /// The stream as seq ranges, **in wire order**: `(first seq, count)` — one seq per record
    /// read, and a "dropped N" marker's whole range where the publisher says it dropped N. The
    /// contract is that these tile `first..last` with no hole and no overlap, so a gap that no
    /// marker accounts for is loss the stream failed to declare (T-433).
    runs: Vec<(u64, u64)>,
}

fn parse(bytes: &[u8]) -> Parsed {
    let mut r = StreamReader::new(Cursor::new(bytes));
    let mut p = Parsed::default();
    if r.read_header().is_err() {
        return p;
    }
    // A record still being written at the end fails to parse: stop there.
    while let Ok(Some(rec)) = r.next_record() {
        match rec {
            Record::Binary(b) => {
                p.seqs.push(b.header.seq);
                p.runs.push((b.header.seq, 1));
                p.data.push((b.header.t.as_unix_nanos(), b.payload));
            }
            Record::Unknown(frame) => {
                if let Some((h, v)) = parse_status_record(&frame) {
                    p.seqs.push(h.seq);
                    p.runs.push((h.seq, 1));
                    p.status.push(v);
                }
            }
            Record::Dropped(m) => {
                p.dropped += m.count;
                p.runs.push((m.first_seq, m.count));
            }
            _ => {}
        }
    }
    p
}

/// The stream contract's loss rule (`hk_stream::publisher`): a consumer that cannot keep up loses
/// records, but **never silently** — every gap in `seq` is preceded by a "dropped N" marker naming
/// exactly the missing range. So what a consumer read must tile one unbroken seq range: each
/// record's seq, and each marker's `first_seq..first_seq + count`, start where the last ended.
///
/// T-433: this replaced `contiguous(&p.seqs)` + `dropped == 0`, which together asserted that no
/// consumer queue ever filled. Whether a queue fills is a property of the machine, not of the
/// code — and doubly so here, where the mock is **unpaced**, so the chain publishes at whatever
/// multiple of real time the box happens to manage (measured: ~4.5x, 1048 records = 21 s of audio
/// in 4.6 s). "The sink drained 4.5x real time" is not a claim about the product. That the stream
/// declares every record it loses is, and it holds however slow the reader is.
fn accounted(p: &Parsed) -> Result<(), String> {
    let mut next: Option<u64> = None;
    for &(first, count) in &p.runs {
        if let Some(expect) = next
            && first != expect
        {
            return Err(format!(
                "seq {first} follows {}: {} unaccounted, {} records, {} dropped and declared",
                expect - 1,
                first as i64 - expect as i64,
                p.seqs.len(),
                p.dropped
            ));
        }
        next = Some(first + count);
    }
    Ok(())
}

/// The control for [`accounted`] (T-315): it must reject a seq gap no marker declared and accept
/// the same gap once the stream declares it. Without this, "no unaccounted loss" would be
/// satisfiable by a stream that published nothing at all.
#[test]
fn a_seq_gap_is_loss_unless_the_stream_declared_it() {
    let check = |runs: Vec<(u64, u64)>| {
        accounted(&Parsed {
            runs,
            ..Parsed::default()
        })
    };
    // Three records in a row.
    check(vec![(7, 1), (8, 1), (9, 1)]).expect("no gap");
    // Seq 8 never arrived and nothing said so.
    let e = check(vec![(7, 1), (9, 1)]).expect_err("an undeclared gap is loss");
    assert!(e.contains("seq 9 follows 7"), "{e}");
    // The same hole, declared: a marker covering 8 and 9, then the record at 10.
    check(vec![(7, 1), (8, 2), (10, 1)]).expect("a declared gap is accounted");
    // A marker that under-counts the hole leaves the rest unaccounted.
    let e = check(vec![(7, 1), (8, 1), (10, 1)]).expect_err("an under-counted gap is loss");
    assert!(e.contains("seq 10 follows 8"), "{e}");
}

/// Blindly detected emitters wide enough to be broadcast FM, widest first: `(id, centre, width)`.
fn wide_emitters(addr: SocketAddr) -> Vec<(String, f64, f64)> {
    let (_, rows) = api_inventory(addr);
    let mut v: Vec<(String, f64, f64)> = rows
        .iter()
        .filter_map(|r| {
            Some((
                r["id"].as_str()?.to_owned(),
                r["f_center_hz"].as_f64()?,
                r["bandwidth_hz"].as_f64()?,
            ))
        })
        .filter(|r| r.2 >= 80e3)
        .collect();
    v.sort_by(|a, b| b.2.total_cmp(&a.2));
    v
}

/// Opens Listen on `n` distinct stations found blindly: a candidate whose refined channel lies
/// within [`STATION_SEP_HZ`] of an open stream is the same station and is closed again.
fn listen_to_stations(listen: &dyn StreamOpener, addr: SocketAddr, n: usize) -> Vec<(String, Sub)> {
    let t0 = Instant::now();
    let mut tried = BTreeSet::new();
    let mut open_subs: Vec<(String, Sub)> = Vec::new();
    while open_subs.len() < n {
        assert!(
            t0.elapsed() < FIND_LIMIT,
            "[{TAG}] found {} of {n} stations blind",
            open_subs.len()
        );
        for (id, f, bw) in wide_emitters(addr) {
            if open_subs.len() >= n || !tried.insert(id.clone()) {
                continue;
            }
            match open(listen, &[("emitter", id.clone())]) {
                Ok(o) => {
                    let c = o.header.center_hz.unwrap();
                    if open_subs
                        .iter()
                        .any(|(_, s)| (s.header.center_hz.unwrap() - c).abs() < STATION_SEP_HZ)
                    {
                        eprintln!("[{TAG}] {id} at {:.4} MHz: a station already open", c / 1e6);
                        continue;
                    }
                    eprintln!(
                        "[{TAG}] listening to emitter {id} ({:.4} MHz, {:.0} kHz wide) on {:.4} MHz, \
                         mode {:?}",
                        f / 1e6,
                        bw / 1e3,
                        c / 1e6,
                        o.header.audio.as_ref().map(|a| a.mode.clone())
                    );
                    open_subs.push((id, Sub::new(o)));
                }
                Err(e) => eprintln!("[{TAG}] emitter {id} ({:.4} MHz) refused: {e}", f / 1e6),
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    open_subs
}

fn frames(s: &Sub) -> usize {
    s.parse().data.len()
}

/// Audio of `p` decimated ×10 to 4.8 kS/s on `[start_ns, start_ns + len/4800 s)` (zeros where no
/// frame was published).
fn audio_window(p: &Parsed, start_ns: i64, len: usize) -> Vec<f64> {
    let mut x = vec![0.0; len];
    for (t, pcm) in &p.data {
        for (j, c) in pcm.chunks_exact(2).enumerate() {
            let at = (t - start_ns) as f64 * 48e3 / 1e9 + j as f64;
            if at < 0.0 {
                continue;
            }
            let k = (at / 10.0) as usize;
            if k < len {
                x[k] += f64::from(i16::from_le_bytes([c[0], c[1]])) / 32767.0 / 10.0;
            }
        }
    }
    x
}

/// Largest normalised cross-correlation within ±50 ms.
fn xcorr(a: &[f64], b: &[f64]) -> f64 {
    let e = |v: &[f64]| v.iter().map(|x| x * x).sum::<f64>();
    let norm = (e(a) * e(b)).sqrt();
    if norm <= 0.0 {
        return 0.0;
    }
    (-240i64..=240)
        .map(|lag| {
            let s: f64 = (0..a.len())
                .filter_map(|i| {
                    let j = i as i64 + lag;
                    (j >= 0 && (j as usize) < b.len()).then(|| a[i] * b[j as usize])
                })
                .sum();
            (s / norm).abs()
        })
        .fold(0.0, f64::max)
}

fn rms(p: &Parsed) -> f64 {
    let (mut e, mut n) = (0.0, 0usize);
    for (_, pcm) in &p.data {
        for c in pcm.chunks_exact(2) {
            let v = f64::from(i16::from_le_bytes([c[0], c[1]])) / 32767.0;
            e += v * v;
            n += 1;
        }
    }
    (e / n.max(1) as f64).sqrt()
}

fn listen_stats(counters: &Counters) -> Vec<Value> {
    counters.to_json()["chain_stats"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["kind"] == "listen")
        .cloned()
        .collect()
}

/// A synthetic RDS station inside the FM recording's window (2.4 Msps at 100.8 MHz), with its own
/// PI and programme (stereo tones), about 3 dB below the recording's station.
fn fm_station(seed: u64, offset_hz: f64, pi: &str, left_hz: f64, right_hz: f64) -> SynthRequest {
    SynthRequest::new("fm_broadcast_rds")
        .seed(seed)
        .datatype(Datatype::Cf32Le)
        .param("sample_rate", 2.4e6)
        .param("center_hz", 100.8e6)
        .param("offset_hz", offset_hz)
        .param("duration_s", 5.0)
        .param("power_dbfs", -16.0)
        .param("noise_dbfs", -120.0)
        .param("pi_hex", pi)
        .param("ps", "T071")
        .param("left_tone_hz", left_hz)
        .param("right_tone_hz", right_hz)
}

#[test]
fn three_fm_stations_demodulate_concurrently_each_stream_its_own_station() {
    let Some((real, _)) = private_truth(FM_FIXTURE) else {
        return;
    };
    if hardware_skip(TAG) {
        return;
    }
    let s1 = synth_or_skip!(fm_station(7111, -500e3, "7A11", 700.0, 300.0));
    let s2 = synth_or_skip!(fm_station(7112, -100e3, "7A12", 1700.0, 1100.0));
    let work = TempDir::new("t071fm-scene");
    let meta = add_stations(&real, &[&s1, &s2], &work.0);
    // The private truth: (centre, PI) of every station.
    let truth: Vec<(f64, String)> = hk_e2e::Fixture::load(&meta)
        .unwrap()
        .of_kind("wfm-broadcast")
        .iter()
        .map(|t| {
            let v = &t.value;
            (
                v["center_hz"]
                    .as_f64()
                    .or_else(|| v["nominal_center_hz"].as_f64())
                    .expect("truth centre"),
                v.pointer("/rds/pi_hex")
                    .or_else(|| v.pointer("/pi_hex"))
                    .and_then(Value::as_str)
                    .expect("truth PI")
                    .to_uppercase(),
            )
        })
        .collect();
    assert_eq!(truth.len(), 3, "{truth:?}");

    let live = blind_live(&meta, "t071fm", BlindSource::default());
    let counters = live.handle.counters();
    let server = serve_api(&live.dir.0, Arc::clone(&counters));
    let addr = server.local_addr();
    let listen = live.handle.listen_service();

    let mut subs = listen_to_stations(&*listen, addr, 3);
    // The positive control: a second Listen on the first station.
    let dup = Sub::new(open(&*listen, &[("emitter", subs[0].0.clone())]).expect("control"));
    subs.push(("control".into(), dup));
    let ids: BTreeSet<&str> = subs
        .iter()
        .map(|(_, s)| s.header.stream_id.as_str())
        .collect();
    assert_eq!(ids.len(), 4, "[{TAG}] one stream per chain");

    wait_for("300 audio frames on every stream", LIMIT, || {
        subs.iter().all(|(_, s)| frames(s) >= 300)
    });

    // Budget and per-chain counters.
    let status = counters.to_json();
    eprintln!("[{TAG}] budget {}", status["budget"]);
    assert_eq!(status["budget"]["listeners"], 4, "{}", status["budget"]);
    assert_eq!(status["budget"]["chains"], 4);
    assert_eq!(status["budget"]["max_chains"], 16);
    let stats = listen_stats(&counters);
    assert_eq!(stats.len(), 4, "[{TAG}] {stats:?}");
    for s in &stats {
        eprintln!("[{TAG}] chain {s}");
        assert!(ids.contains(s["stream_id"].as_str().unwrap()), "{s}");
        assert!(s["records"].as_u64().unwrap() >= 300, "{s}");
        assert!(s["cpu_s"].as_f64().unwrap() > 0.0, "{s}");
        assert!(s["samples"].as_u64().unwrap() > 0, "{s}");
        // The chain's own losslessness, which IS a code property here: the unpaced mock is
        // pausable, so `blind_live` runs with `lossless = true` and the source back-pressures
        // instead of the ring overwriting. `dropped` next to it is the test's own in-process sink
        // failing to drain an unpaced firehose — reported, not asserted (T-433; the real-time
        // form of that claim is asserted in the performance test below).
        assert_eq!(s["lost_samples"], 0, "{s}");
        assert_eq!(s["consumers"], 1, "{s}");
    }

    // Each stream carries its own station.
    let parsed: Vec<Parsed> = subs.iter().map(|(_, s)| s.parse()).collect();
    let start_ns = parsed.iter().map(|p| p.data[0].0).max().unwrap() + 1_000_000_000;
    let end_ns = parsed
        .iter()
        .map(|p| p.data.last().unwrap().0)
        .min()
        .unwrap();
    let len = (((end_ns - start_ns) as f64 / 1e9).min(3.0) * 4800.0) as usize;
    assert!(len >= 4800, "[{TAG}] {len} common samples");
    let x: Vec<Vec<f64>> = parsed
        .iter()
        .map(|p| audio_window(p, start_ns, len))
        .collect();
    let (mut decoded, mut covered) = (Vec::new(), BTreeSet::new());
    for (i, p) in parsed.iter().enumerate() {
        let (_, s) = &subs[i];
        let pi = s
            .header
            .audio
            .as_ref()
            .and_then(|a| a.refinement.as_ref())
            .and_then(|r| r.labels.get("rds_pi").cloned());
        let c = s.header.center_hz.unwrap();
        eprintln!(
            "[{TAG}] stream {} on {:.4} MHz: {} frames, rms {:.3}, PI {pi:?}, dropped {}",
            s.header.stream_id,
            c / 1e6,
            p.data.len(),
            rms(p),
            p.dropped
        );
        assert!(rms(p) > 0.005, "[{TAG}] stream {i} has audio");
        accounted(p).unwrap_or_else(|e| panic!("[{TAG}] stream {i} lost records silently: {e}"));
        // The station at the stream's channel (private truth); its PI and no other.
        let (tc, tpi) = truth
            .iter()
            .min_by(|a, b| (a.0 - c).abs().total_cmp(&(b.0 - c).abs()))
            .unwrap();
        assert!(
            (tc - c).abs() < 20e3,
            "[{TAG}] stream {i} on {c} Hz is on no station"
        );
        if let Some(pi) = &pi {
            assert_eq!(
                &pi.to_uppercase(),
                tpi,
                "[{TAG}] stream {i} carries another station's PI"
            );
            decoded.push((i, pi.clone()));
        }
        if i < 3 {
            covered.insert(tpi.clone());
        }
    }
    assert_eq!(covered.len(), 3, "[{TAG}] three streams, three stations");
    assert!(
        decoded.iter().filter(|(i, _)| *i < 3).count() >= 2,
        "[{TAG}] RDS PI decoded on at least two streams: {decoded:?}"
    );
    // The control's probe chose a station on its own: its twin is the stream on that channel.
    let centre = |k: usize| subs[k].1.header.center_hz.unwrap();
    let twin = (0..3)
        .find(|&k| (centre(k) - centre(3)).abs() < 20e3)
        .expect("the control is on one of the three stations");
    let mut pairs = Vec::new();
    for i in 0..4 {
        for j in i + 1..4 {
            pairs.push((i, j, xcorr(&x[i], &x[j])));
        }
    }
    let report: Vec<String> = pairs
        .iter()
        .map(|(i, j, r)| format!("{i}-{j}: {r:.3}"))
        .collect();
    eprintln!(
        "[{TAG}] audio cross-correlation (control 3 is stream {twin}'s station) {}",
        report.join(", ")
    );
    for (i, j, r) in pairs {
        if j == 3 && i == twin {
            assert!(
                r > 0.6,
                "[{TAG}] the control does not match its station: {r}"
            );
        } else {
            assert!(r < 0.3, "[{TAG}] streams {i} and {j} cross-talk: {r}");
        }
    }

    // Closing one stream (the control) leaves the others running.
    let (_, control) = subs.pop().unwrap();
    drop(control);
    wait_for("the closed chain to end", Duration::from_secs(60), || {
        counters.listen.active.load(Ordering::Relaxed) == 3 && listen_stats(&counters).len() == 3
    });
    let before: Vec<usize> = subs.iter().map(|(_, s)| frames(s)).collect();
    wait_for("the other streams to continue", LIMIT, || {
        subs.iter()
            .zip(&before)
            .all(|((_, s), b)| frames(s) >= b + 100)
    });

    // Admission beyond the budget: refused with the reason; the running chains are unaffected.
    live.handle.set_listen_settings(ListenSettings {
        max_chains: 3,
        max_listeners: 3,
        max_taps: 3,
        ..ListenSettings::default()
    });
    let e = open(&*listen, &[("emitter", subs[1].0.clone())])
        .err()
        .expect("a fourth listener is refused");
    eprintln!("[{TAG}] listen refusal: {e}");
    assert_eq!((e.status, e.code.as_str()), (503, "busy"));
    assert!(e.reason.contains("listener limit: 3 of 3"), "{e}");
    let bits = live.handle.bits_service();
    let e = open(&*bits, &[])
        .err()
        .expect("a tap beyond the budget is refused");
    eprintln!("[{TAG}] tap refusal: {e}");
    assert_eq!((e.status, e.code.as_str()), (503, "busy"));
    assert!(e.reason.contains("chain budget: 3 of 3 chains"), "{e}");
    assert!(counters.budget.refused_busy.load(Ordering::Relaxed) >= 2);
    let before: Vec<usize> = subs.iter().map(|(_, s)| frames(s)).collect();
    wait_for("the running streams after the refusals", LIMIT, || {
        subs.iter()
            .zip(&before)
            .all(|((_, s), b)| frames(s) >= b + 100)
    });
    for (_, s) in &subs {
        let p = s.parse();
        accounted(&p).unwrap_or_else(|e| {
            panic!("[{TAG}] {} lost records silently: {e}", s.header.stream_id)
        });
    }
    assert_eq!(listen_stats(&counters).len(), 3);
    live.handle.set_listen_settings(ListenSettings::default());

    for (_, mut s) in subs {
        s.opened.take();
    }
    wait_for("every listener to end", Duration::from_secs(60), || {
        counters.budget.chains.load(Ordering::Relaxed) == 0
            && counters.listen.running.load(Ordering::Relaxed) == 0
    });
    live.handle.stop();
    finish(live.handle);
}

/// Appends `other`'s truth annotations (not its capture annotation) to `meta`.
fn merge_truth(meta: &mut Value, other: &Value) {
    let extra: Vec<Value> = other["annotations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|x| !x.to_string().contains("\"kind\":\"capture\""))
        .cloned()
        .collect();
    meta["annotations"].as_array_mut().unwrap().extend(extra);
}

fn read_json(p: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
}

fn write_cf32(dir: &Path, name: &str, mut meta: Value, iq: &[f32]) -> PathBuf {
    meta["global"]["core:datatype"] = json!("cf32_le");
    if let Some(g) = meta["global"].as_object_mut() {
        g.remove("core:sha512");
    }
    let mut out = Vec::with_capacity(4 * iq.len());
    for v in iq {
        out.extend_from_slice(&v.to_le_bytes());
    }
    let path = dir.join(format!("{name}.sigmf-meta"));
    std::fs::write(path.with_extension("sigmf-data"), out).unwrap();
    std::fs::write(&path, serde_json::to_vec_pretty(&meta).unwrap()).unwrap();
    path
}

fn cf32(bytes: &[u8]) -> impl Iterator<Item = f32> + '_ {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
}

/// The real ci8 recording (scaled as the pipeline quantises, x/128) with synthetic cf32 stations
/// of the same capture added, keeping every truth.
fn add_stations(real: &Path, synth: &[&hk_e2e::SynthOutput], dir: &Path) -> PathBuf {
    let mut meta = read_json(real);
    let raw = std::fs::read(real.with_extension("sigmf-data")).unwrap();
    let mut iq: Vec<f32> = raw[..raw.len() / 2 * 2]
        .iter()
        .map(|&b| f32::from(b as i8) / 128.0)
        .collect();
    for s in synth {
        let m = s.fixture(0).unwrap().meta_path;
        merge_truth(&mut meta, &read_json(&m));
        let d = std::fs::read(m.with_extension("sigmf-data")).unwrap();
        for (x, v) in iq.iter_mut().zip(cf32(&d)) {
            *x += v;
        }
    }
    write_cf32(dir, "three_stations", meta, &iq)
}

/// Sums two cf32 recordings of the same capture into one, keeping both truths.
fn mix(a: &Path, b: &Path, dir: &Path) -> PathBuf {
    let mut meta = read_json(a);
    merge_truth(&mut meta, &read_json(b));
    let da = std::fs::read(a.with_extension("sigmf-data")).unwrap();
    let db = std::fs::read(b.with_extension("sigmf-data")).unwrap();
    let iq: Vec<f32> = cf32(&da).zip(cf32(&db)).map(|(x, y)| x + y).collect();
    write_cf32(dir, "two_sensors", meta, &iq)
}

fn truth_payloads(out: &hk_e2e::SynthOutput) -> BTreeSet<String> {
    out.fixture(0)
        .unwrap()
        .of_kind("fsk-burst")
        .iter()
        .map(|t| t.value["frame"]["payload_hex"].as_str().unwrap().to_owned())
        .collect()
}

/// The payload bytes of a burst's bits, as the status record locates them.
fn payload_hex(bits: &[u8], status: &Value) -> Option<String> {
    let at = status["payload_bit"].as_u64()? as usize;
    let n = status["payload_bits"].as_u64()? as usize;
    let lsb = status["bit_order"] == "lsb-first";
    let slice = bits.get(at..at + n)?;
    Some(
        slice
            .chunks_exact(8)
            .map(|c| {
                let byte = c.iter().enumerate().fold(0u8, |a, (i, &b)| {
                    a | ((b & 1) << if lsb { i } else { 7 - i })
                });
                format!("{byte:02x}")
            })
            .collect(),
    )
}

/// Reads a bits stream over TCP until `want` CRC-valid payloads: `(header, payloads, bursts)`.
fn read_valid_payloads(
    addr: SocketAddr,
    line: String,
    want: usize,
) -> (StreamHeader, Vec<String>, usize) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(LIMIT)).unwrap();
    writeln!(s, "{line}").unwrap();
    let mut r = StreamReader::new(s);
    let header = r.read_header().expect("a bits stream header").clone();
    let (mut pending, mut valid, mut bursts) = (None, Vec::new(), 0usize);
    while valid.len() < want && bursts < 40 * want {
        match r.next_record().expect("record") {
            Some(Record::Unknown(frame)) => pending = parse_status_record(&frame).map(|(_, v)| v),
            Some(Record::Binary(b)) => {
                bursts += 1;
                let status = pending.take().expect("a status record precedes each burst");
                if status["crc"] == "valid"
                    && let Some(hex) = payload_hex(&b.payload, &status)
                {
                    valid.push(hex);
                }
            }
            Some(Record::Dropped(m)) => panic!("[{TAG}] a reading client lost {} records", m.count),
            Some(_) => {}
            None => panic!("[{TAG}] the bits stream ended early"),
        }
    }
    (header, valid, bursts)
}

#[test]
fn two_fsk_emitters_stream_concurrent_bits_each_matching_only_its_own_truth() {
    let sensor = |seed: u64, offset_hz: f64, id: u32, first_s: f64| {
        SynthRequest::new("fsk_burst_train")
            .seed(seed)
            .datatype(Datatype::Cf32Le)
            .param("channel_offset_hz", offset_hz)
            .param("sensor_id", id)
            .param("first_burst_s", first_s)
            .param("snr_db", 20.0)
            .param("duration_s", 2.4)
    };
    let a = synth_or_skip!(sensor(7101, -120e3, 0x1A71, 0.03));
    let b = synth_or_skip!(sensor(7102, 120e3, 0x2B72, 0.09));
    let (truth_a, truth_b) = (truth_payloads(&a), truth_payloads(&b));
    assert!(truth_a.len() >= 15 && truth_b.len() >= 15);
    assert!(truth_a.is_disjoint(&truth_b), "distinct sensors");
    let work = TempDir::new("t071mix");
    let meta = mix(
        &a.fixture(0).unwrap().meta_path,
        &b.fixture(0).unwrap().meta_path,
        &work.0,
    );

    let live = blind_live(
        &meta,
        "t071fsk",
        BlindSource {
            vouched_class: Some("unrestricted"),
            ..BlindSource::default()
        },
    );
    let counters = live.handle.counters();
    let tcp = StreamServer::start(
        StreamServerConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            Token::from_config(API_TOKEN).unwrap(),
        ),
        StreamRegistry::new(),
        OpenerRegistry::new().with("bits", live.handle.bits_service()),
    )
    .unwrap();
    let addr = tcp.local_addr();

    // Two emitters found blindly: the centres the system estimated for the bursts it demodulates
    // (an all-bursts tap), at least 100 kHz apart.
    let bits = live.handle.bits_service();
    let discovery = Sub::new(open(&*bits, &[]).expect("a discovery tap"));
    let mut centres: Vec<f64> = Vec::new();
    wait_for("bursts of two emitters", FIND_LIMIT, || {
        centres.clear();
        for st in discovery.parse().status {
            if let Some(f) = st["f_center_hz"].as_f64()
                && centres.iter().all(|c| (c - f).abs() >= 100e3)
            {
                centres.push(f);
            }
        }
        centres.len() >= 2
    });
    drop(discovery);
    eprintln!(
        "[{TAG}] FSK emitters at {:?} MHz",
        centres.iter().map(|c| c / 1e6).collect::<Vec<_>>()
    );

    let readers: Vec<_> = centres[..2]
        .iter()
        .map(|&c| {
            let line = format!(
                "open/bits?token={API_TOKEN}&f_lo={}&f_hi={}",
                c - 40e3,
                c + 40e3
            );
            std::thread::spawn(move || read_valid_payloads(addr, line, 8))
        })
        .collect();
    wait_for("both taps open", LIMIT, || {
        counters.budget.taps.load(Ordering::Relaxed) == 2
            || readers.iter().any(std::thread::JoinHandle::is_finished)
    });
    let taps = counters.to_json()["chain_stats"].clone();
    let results: Vec<_> = readers.into_iter().map(|r| r.join().unwrap()).collect();
    let mut owners = BTreeSet::new();
    for (i, (header, valid, bursts)) in results.iter().enumerate() {
        let in_a = valid.iter().filter(|h| truth_a.contains(*h)).count();
        let in_b = valid.iter().filter(|h| truth_b.contains(*h)).count();
        eprintln!(
            "[{TAG}] bits stream {} ({:?} Hz): {bursts} bursts, {} CRC-valid, {in_a} of sensor A, \
             {in_b} of sensor B",
            header.stream_id,
            header.center_hz,
            valid.len()
        );
        assert!(
            valid.len() >= 8,
            "[{TAG}] stream {i}: {} valid payloads",
            valid.len()
        );
        assert!(
            (in_a == valid.len() && in_b == 0) || (in_b == valid.len() && in_a == 0),
            "[{TAG}] stream {i} mixes emitters or carries non-truth payloads ({in_a} A, {in_b} B \
             of {})",
            valid.len()
        );
        owners.insert(in_a > 0);
    }
    assert_eq!(
        owners.len(),
        2,
        "[{TAG}] the two streams carry different emitters"
    );
    let tap_stats: Vec<&Value> = taps
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["kind"] == "bits-tap")
        .collect();
    eprintln!("[{TAG}] tap chain stats {tap_stats:?}");
    assert_eq!(tap_stats.len(), 2, "[{TAG}] per-tap counters while open");
    wait_for("the taps to close", Duration::from_secs(30), || {
        counters.budget.taps.load(Ordering::Relaxed) == 0 && tcp.stats().active == 0
    });
    live.handle.stop();
    finish(live.handle);
}

#[test]
fn an_off_raster_station_between_two_raster_chains_is_written_by_exactly_one() {
    const SHIFT_HZ: f64 = 150e3;
    let Some((meta, fx)) = private_truth(FM_FIXTURE) else {
        return;
    };
    if hardware_skip(TAG) {
        return;
    }
    let shifted = fx.of_kind("wfm-broadcast")[0].expect_f64("/center_hz") + SHIFT_HZ;
    let run = blind_config(
        &meta,
        "t071dedupe",
        BlindSource {
            iq_shift_hz: SHIFT_HZ,
            ..BlindSource::default()
        },
        json!({}),
    );
    let dir = run.dir;
    let handle = start(run.cfg, run.replay);
    let counters = handle.counters();
    finish(handle);

    let repo = repo(&dir.0);
    let mut seen = BTreeSet::new();
    let mut writes = Vec::new();
    for e in inventory(&repo, InventoryQuery::default()) {
        for r in repo.refined_tuning_history(e.emitter.id).unwrap() {
            if r.source == hk_pipeline::refine::SOURCE_ANALOG_CHAIN
                && (r.center_hz - shifted).abs() < 5e3
                && seen.insert((r.start_center_hz.to_bits(), r.elapsed_s.to_bits()))
            {
                writes.push(r);
            }
        }
    }
    let dup = counters.chains.duplicate_emission.load(Ordering::Relaxed);
    for r in &writes {
        eprintln!(
            "[{TAG}] analog chain from {:.4} MHz wrote {:.4} MHz (duplicate_emission {dup})",
            r.start_center_hz / 1e6,
            r.center_hz / 1e6
        );
    }
    assert_eq!(
        writes.len(),
        1,
        "[{TAG}] the off-raster station must be owned by exactly one chain"
    );
}

/// The load average (1, 5, 15 min).
fn load_average() -> String {
    std::process::Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .or_else(|_| std::fs::read_to_string("/proc/loadavg"))
        .unwrap_or_default()
}

#[test]
#[ignore = "performance: cargo test --release -p hk-e2e --test concurrent_demod -- --ignored"]
fn three_wfm_listeners_and_two_fsk_taps_stay_real_time_at_2p4_msps() {
    const MEASURE: Duration = Duration::from_secs(30);
    let Some(real) = real_fixture(FM_FIXTURE) else {
        return;
    };
    // The three-station scene of the FM test (the recording holds one strong station).
    let s1 = synth_or_skip!(fm_station(7111, -500e3, "7A11", 700.0, 300.0));
    let s2 = synth_or_skip!(fm_station(7112, -100e3, "7A12", 1700.0, 1100.0));
    let work = TempDir::new("t071perf-scene");
    let meta = add_stations(&real, &[&s1, &s2], &work.0);
    let src = TempDir::new("t071perf-src");
    let blind = strip_truth(&meta, &src.0, "blind", 0.0).unwrap();
    let replay = open_mock_replay(&blind, Pacing::RealTime { speed: 1.0 }, MockEnd::Loop).unwrap();
    let info = replay.info;
    let dir = TempDir::new("t071perf");
    let mut cfg = PipelineConfig::new(
        &dir.0,
        replay_plan(info.center_hz, info.sample_rate_hz, info.start_time),
    )
    .unwrap();
    cfg.source_class = replay.class;
    cfg.device_id = replay.device.device_id.clone();
    cfg.device_hw = Some(replay.device.hw.clone());
    cfg.lossless = false;
    let handle = Pipeline::start(
        cfg,
        Box::new(replay.source),
        info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let counters = handle.counters();
    let server = serve_api(&dir.0, Arc::clone(&counters));
    let listen = handle.listen_service();
    let subs = listen_to_stations(&*listen, server.local_addr(), 3);
    let bits = handle.bits_service();
    let taps: Vec<Sub> = (0..2)
        .map(|_| Sub::new(open(&*bits, &[]).expect("tap")))
        .collect();

    let lc = &counters.listen;
    let get = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
    let (f0, q0, k0) = (
        get(&lc.frames),
        get(&lc.squelched_frames),
        get(&lc.skipped_samples),
    );
    let (l0, a0) = (
        get(&counters.chains.lost_samples),
        counters.always_on_lost(),
    );
    let src0 = get(&counters.source.samples);
    let t0 = Instant::now();
    std::thread::sleep(MEASURE);
    let secs = t0.elapsed().as_secs_f64();
    let audio = (get(&lc.frames) - f0 + get(&lc.squelched_frames) - q0) as f64 * 0.02;
    let status = counters.to_json();
    for s in status["chain_stats"].as_array().unwrap() {
        eprintln!(
            "[{TAG}] perf {} {}: cpu_load {} ({} s), latency last/max {}/{} ms, backlog {} ms, \
             lost {}, records {}, dropped {}",
            s["kind"],
            s["stream_id"],
            s["cpu_load"],
            s["cpu_s"],
            s["latency_ms_last"],
            s["latency_ms_max"],
            s["backlog_ms"],
            s["lost_samples"],
            s["records"],
            s["dropped"]
        );
    }
    // T-433: **this** is where `dropped == 0` belongs, and here it is asserted rather than printed.
    // The claim "the always-on capture path does not lose samples" is a throughput/timing claim,
    // which docs/10 §2 puts at T5/bench: it is only meaningful against a clock. This test alone
    // paces the mock at `RealTime { speed: 1.0 }`, runs in release, and is `#[ignore]`d out of the
    // CI gate precisely so it is measured on a box that is not carrying four concurrent builds.
    // The CI-tier test above replays UNPACED at ~4.5x real time, where a full consumer queue
    // measures the box's spare capacity against an arbitrary replay speed and nothing else.
    for s in status["chain_stats"].as_array().unwrap() {
        assert_eq!(
            s["dropped"], 0,
            "[{TAG}] a consumer could not keep up at real time: {s}"
        );
        assert_eq!(
            s["lost_samples"], 0,
            "[{TAG}] a chain lost input samples at real time: {s}"
        );
    }
    let skipped = get(&lc.skipped_samples) - k0;
    let source_msps = (get(&counters.source.samples) - src0) as f64 / secs / 1e6;
    eprintln!(
        "[{TAG}] perf: {secs:.1} s, source {source_msps:.3} Msps, audio {:.2} s per listener-second \
         (3 listeners), listen skipped {skipped} samples, chain lost {}, always-on lost {}, budget \
         {}, load average {}",
        audio / (3.0 * secs),
        get(&counters.chains.lost_samples) - l0,
        counters.always_on_lost() - a0,
        status["budget"],
        load_average()
    );
    assert_eq!(taps.len(), 2);
    assert!(
        audio / (3.0 * secs) >= 0.95,
        "[{TAG}] listeners fell behind real time"
    );
    assert_eq!(
        skipped, 0,
        "[{TAG}] a listener skipped samples to stay live"
    );
    drop(taps);
    drop(subs);
    handle.stop();
    finish(handle);
}
