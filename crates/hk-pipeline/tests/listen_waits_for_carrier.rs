//! T-987 (SIGNAL-062): **Listen on a bursty channel opens squelched and waits for the carrier.**
//!
//! A GMRS-like channel keyed one second in six: NBFM, 1 kHz tone at 3 kHz deviation, 62.5 kHz
//! above the tuned centre, through the scripted radio behind the device interface. Listen is
//! opened **in the silence** — the radio is held there, so the probe reads nothing but noise, as
//! the explorer's did on 461.125 / 462.225 / 464.700 MHz — and the inventory holds that emitter's
//! history: three past bursts, each classified `nbfm` (what the per-burst classifier writes).
//!
//! - **Before T-987** every one of these opens was refused `422 no-analog-mode`.
//! - **Now** the emitter (and a range covering it) opens: the header says `mode: nbfm` and carries
//!   the `wait` block (`waiting for carrier (last seen …, mode nbfm from 3 of 3 bursts)`), the
//!   squelch is armed from the silence, **no audio flows while the channel is silent**, and when
//!   the radio plays on into the next burst the audio arrives and is the burst's 1 kHz tone.
//! - **A selection with no history is still refused** `422 no-analog-mode`, saying there is
//!   nothing to wait for.

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use common::*;
use hk_model::{Classification, Emitter, EmitterId, Identity, KnownStatus, Repository, Timestamp};
use hk_pipeline::class::window_class;
use hk_pipeline::inventory::NullInventory;
use hk_pipeline::{Pipeline, PipelineConfig, PipelineHandle, SourceInfo, replay_plan};
use hk_stream::record::parse_status_record;
use hk_stream::{Declared, OpenRequest, OpenedStream, Record, StreamOpener, StreamReader};
use num_complex::Complex;
use serde_json::Value;

const CF: f64 = 462.6e6;
const FS: f64 = 500e3;
/// The channel: 462.6625 MHz.
const OFFSET_HZ: f64 = 62.5e3;
const DEV_HZ: f64 = 3_000.0;
const TONE_HZ: f64 = 1_000.0;
/// Keyed for `BURST_S` at the start of every `PERIOD_S`.
const PERIOD_S: f64 = 6.0;
const BURST_S: f64 = 1.0;
const LIMIT: Duration = Duration::from_secs(120);

fn at(s: f64) -> u64 {
    (s * FS) as u64
}

fn wait(what: &str, limit: Duration, f: impl Fn() -> bool) {
    let deadline = Instant::now() + limit;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The burst train: noise always, the NBFM carrier only while keyed. Phase is a function of
/// the absolute sample index, so it is continuous across blocks.
fn burst_train() -> radio::Generator {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let (period, burst) = (at(PERIOD_S), at(BURST_S));
    Box::new(move |_center, rate, index, n, out| {
        for i in 0..n as u64 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let nr = ((state >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 8.0;
            let ni = ((state & 0xff_ffff) as f64 / (1u64 << 24) as f64 - 0.5) * 8.0;
            let k = index + i;
            let (mut re, mut im) = (nr, ni);
            if k % period < burst {
                let t = k as f64 / rate;
                let ph = std::f64::consts::TAU * OFFSET_HZ * t
                    + DEV_HZ / TONE_HZ * (std::f64::consts::TAU * TONE_HZ * t).sin();
                re += 40.0 * ph.cos();
                im += 40.0 * ph.sin();
            }
            out.push(Complex::new(
                re.round().clamp(-128.0, 127.0) as i8,
                im.round().clamp(-128.0, 127.0) as i8,
            ));
        }
    })
}

struct Run {
    handle: PipelineHandle,
    ctl: Arc<radio::RadioControl>,
    dir: TempDir,
}

impl Run {
    /// Starts the run held at `hold` samples (in the silence after the first burst).
    fn start(tag: &str, hold: u64) -> Self {
        let dir = TempDir::new(tag);
        let (radio, ctl) = radio::Radio::new(CF, FS, 8_192, burst_train());
        ctl.hold_at(hold);
        let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
        let mut cfg = PipelineConfig::new(&dir.0, replay_plan(CF, FS, t0)).unwrap();
        cfg.source_class = window_class(CF, FS);
        cfg.live_window_class = true;
        cfg.lossless = true;
        cfg.settings.chains = Some(Vec::new());
        let handle = Pipeline::start(
            cfg,
            Box::new(radio),
            SourceInfo {
                sample_rate_hz: FS,
                center_hz: CF,
                start_time: t0,
            },
            None,
            // The history is the test's to write: nothing from this run's detections merges
            // into (or races) the emitter it seeds.
            Box::new(NullInventory),
        )
        .unwrap();
        let counters = handle.counters();
        wait("the run to reach the silence", LIMIT, || {
            counters.source.samples.load(Ordering::Relaxed) >= hold
        });
        Self { handle, ctl, dir }
    }

    fn finish(self) {
        let Self { handle, ctl, dir } = self;
        ctl.run_free();
        ctl.finish();
        let (s, fired) = wait_guarded(handle, LIMIT);
        assert!(!fired, "the run finished on its own");
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        drop(dir);
    }

    /// Opens a listen stream while the radio plays on to `until` (still silent), so the probe
    /// gets its samples and reads only the silence.
    fn open(&self, params: &[(&str, String)], until: u64) -> Result<OpenedStream, Value> {
        let listen = self.handle.listen_service();
        let req = OpenRequest {
            params: params
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
            peer: "t987".into(),
        };
        let counters = self.handle.counters();
        let probes = counters.listen.probes.load(Ordering::SeqCst);
        let opener = std::thread::spawn(move || listen.open(&req).map_err(|e| e.to_json()));
        // The probe starts at the live edge, where the radio is held: let it take its place
        // there before the radio plays on, so it reads the silence and only the silence.
        let deadline = Instant::now() + LIMIT;
        while counters.listen.probes.load(Ordering::SeqCst) == probes && !opener.is_finished() {
            assert!(Instant::now() < deadline, "the probe to start");
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(100));
        self.ctl.hold_at(until);
        opener.join().unwrap()
    }
}

/// The emitter's history: three past bursts on the channel, each classified `nbfm`.
fn seed_history(dir: &std::path::Path) -> EmitterId {
    let t = |s: f64| Timestamp::from_unix_nanos(radio::T0_NS + (s * 1e9) as i64);
    // Three earlier bursts, a period apart, ending just before this run's first.
    let bursts = [-3.0 * PERIOD_S, -2.0 * PERIOD_S, -PERIOD_S].map(|s| s + BURST_S);
    let e = Emitter {
        id: EmitterId::new(),
        f_center_hz: CF + OFFSET_HZ,
        bandwidth_hz: 9e3,
        first_seen: t(bursts[0] - BURST_S),
        last_seen: t(bursts[2]),
        count: 3,
        fingerprint: Value::Null,
        identity: Identity::Unknown,
        known_status: KnownStatus::Unknown,
        classifications: bursts
            .iter()
            .map(|&s| Classification {
                t: t(s),
                family: "nbfm".into(),
                confidence: 0.8,
                open_set_score: 0.2,
                model_version: "t987-per-burst".into(),
            })
            .collect(),
        tags: Default::default(),
    };
    Repository::open(dir.join("hackriff.db"))
        .unwrap()
        .insert_emitter(&e)
        .unwrap();
    e.id
}

/// A `Write` end whose bytes arrive at [`Pipe`].
struct Tx(Sender<Vec<u8>>);

impl Write for Tx {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0
            .send(b.to_vec())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The `Read` end: blocks up to [`LIMIT`] for the next bytes.
struct Pipe {
    rx: Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for Pipe {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.buf.len() {
            match self.rx.recv_timeout(LIMIT) {
                Ok(b) => {
                    self.buf = b;
                    self.pos = 0;
                }
                Err(RecvTimeoutError::Disconnected) => return Ok(0),
                Err(RecvTimeoutError::Timeout) => {
                    return Err(std::io::Error::from(std::io::ErrorKind::TimedOut));
                }
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// A local consumer on `o`, read on a thread: `(audio samples, status records)` as they come.
fn consume(o: &OpenedStream) -> Receiver<Result<Vec<i16>, Value>> {
    let (tx, rx) = mpsc::channel();
    o.handle
        .subscribe("t987", Declared::local(Tx(tx)), Box::new(|_| {}))
        .unwrap();
    let (out, records) = mpsc::channel();
    std::thread::spawn(move || {
        let mut r = StreamReader::new(Pipe {
            rx,
            buf: Vec::new(),
            pos: 0,
        });
        while let Ok(Some(rec)) = r.next_record() {
            let item = match rec {
                Record::Binary(b) => Ok(b
                    .payload
                    .chunks_exact(2)
                    .map(|x| i16::from_le_bytes([x[0], x[1]]))
                    .collect()),
                Record::Unknown(f) => match parse_status_record(&f) {
                    Some((_, v)) => Err(v),
                    None => continue,
                },
                _ => continue,
            };
            if out.send(item).is_err() {
                return;
            }
        }
    });
    records
}

/// Tone power at `f` relative to the total power of `x`, dB.
fn tone_db(x: &[f32], fs: f64, f: f64) -> f64 {
    let (mut re, mut im, mut total) = (0.0f64, 0.0f64, 0.0f64);
    for (n, &v) in x.iter().enumerate() {
        let ph = std::f64::consts::TAU * f * n as f64 / fs;
        re += f64::from(v) * ph.cos();
        im += f64::from(v) * ph.sin();
        total += f64::from(v).powi(2);
    }
    let tone = 2.0 * (re * re + im * im) / x.len() as f64;
    10.0 * (tone / total.max(1e-30)).log10()
}

fn range(center: f64, half: f64) -> Vec<(&'static str, String)> {
    vec![
        ("f_lo", format!("{}", center - half)),
        ("f_hi", format!("{}", center + half)),
    ]
}

#[test]
fn listen_opened_between_bursts_waits_squelched_then_plays_the_next_burst() {
    // Held at 1.2 s: the first burst (0–1 s) is over and the channel is silent until 6 s.
    let run = Run::start("t987-listen-waits", at(BURST_S + 0.2));
    let channel = CF + OFFSET_HZ;

    // 1. No history yet: the silence is refused honestly, as before.
    let Err(refused) = run.open(&range(channel, 6.25e3), at(2.4)) else {
        panic!("a silent channel with no history is refused");
    };
    assert_eq!(refused["code"], "no-analog-mode", "{refused}");
    let reason = refused["reason"].as_str().unwrap();
    assert!(reason.contains("nothing to wait for"), "{refused}");

    // 2. The inventory holds the channel's past bursts.
    let emitter = seed_history(&run.dir.0);

    // 3. A range over the channel now opens, waiting.
    let by_range = run
        .open(&range(channel, 6.25e3), at(3.6))
        .unwrap_or_else(|r| panic!("a range over a bursty channel with history opens: {r}"));
    let h = serde_json::to_value(&by_range.header).unwrap();
    assert_eq!(h["audio"]["mode"], "nbfm", "{h}");
    assert_eq!(h["audio"]["wait"]["emitter_id"], emitter.to_string(), "{h}");
    drop(by_range);

    // 4. The emitter itself: opened in the silence, it waits.
    let opened = run
        .open(&[("emitter", emitter.to_string())], at(4.8))
        .unwrap_or_else(|r| panic!("listen on a bursty emitter opens squelched: {r}"));
    let h = serde_json::to_value(&opened.header).unwrap();
    let audio = &h["audio"];
    assert_eq!(audio["mode"], "nbfm", "the mode from the bursts: {h}");
    let w = &audio["wait"];
    assert_eq!(w["mode_bursts"], 3, "{h}");
    assert_eq!(w["analog_bursts"], 3, "{h}");
    assert_eq!(w["emitter_id"], emitter.to_string(), "{h}");
    assert!(w["last_seen_ns"].is_i64(), "{h}");
    let statement = w["statement"].as_str().unwrap();
    assert!(
        statement.starts_with("waiting for carrier (last seen ")
            && statement.ends_with("mode nbfm from 3 of 3 bursts)"),
        "{statement}"
    );
    assert!(
        w["probe"]
            .as_str()
            .is_some_and(|p| p.contains("no analog modulation recognised")),
        "{h}"
    );
    assert_eq!(audio["mode_confidence"], 1.0, "{h}");
    assert!(
        audio["mode_rules"]
            .as_str()
            .is_some_and(|r| r.ends_with("+history")),
        "{h}"
    );
    assert!(
        audio["squelch"]["noise_dbfs"].is_f64(),
        "the squelch is armed from the silence: {h}"
    );
    let center = h["center_hz"].as_f64().unwrap();
    assert!((center - channel).abs() < 1.0, "{h}");

    // 5. While the channel stays silent, status flows and audio does not.
    let records = consume(&opened);
    run.ctl.hold_at(at(5.9));
    let mut status_seen = 0;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match records.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(samples)) => panic!(
                "audio while the channel is silent ({} samples)",
                samples.len()
            ),
            Ok(Err(status)) => {
                assert_eq!(status["squelch_open"], false, "{status}");
                status_seen += 1;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("the stream ended while waiting"),
        }
    }
    assert!(status_seen > 0, "status records while waiting");

    // 6. The carrier returns (the burst at 6–7 s): the audio flows, and it is the burst's tone.
    run.ctl.hold_at(at(PERIOD_S + BURST_S + 0.5));
    let mut pcm: Vec<f32> = Vec::new();
    let deadline = Instant::now() + LIMIT;
    while pcm.len() < (0.6 * 48_000.0) as usize {
        assert!(Instant::now() < deadline, "audio when the carrier returned");
        match records.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(samples)) => pcm.extend(samples.iter().map(|&v| f32::from(v) / 32767.0)),
            Ok(Err(_status)) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("the stream ended"),
        }
    }
    let tail = &pcm[pcm.len() / 4..];
    let db = tone_db(tail, 48_000.0, TONE_HZ);
    assert!(
        db > -3.0,
        "the burst's 1 kHz tone dominates the audio ({db:.1} dB)"
    );

    drop(records);
    drop(opened);
    run.finish();
}
