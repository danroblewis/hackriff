//! SIGNAL-062 Listen (T-043): click a signal → auto analog demodulation → audio stream, through
//! the running pipeline and the authenticated `/ws/open/listen` WebSocket, blind.
//!
//! - **Positive:** the FM fixture replays in a loop (the `hk serve --replay --loop` shape, via the
//!   blind harness); the station is found by matching `/api/inventory` rows against the private
//!   truth (never a frequency lookup); a Listen request on that emitter streams `ri16_le` audio
//!   whose mode was chosen automatically (WFM), whose content is demodulated FM audio (audio-band
//!   energy, 19 kHz pilot removed although the probe found it in the signal), with bounded
//!   latency; a third concurrent listener is refused; frames stop on detach.

use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hk_api::{ApiState, Server, ServerConfig, Token};
use hk_e2e::TruthItem;
use hk_e2e::blind::matching;
use hk_stream::record::parse_status_record;
use hk_stream::{BinaryRecordHeader, OpenerRegistry, StreamHeader, StreamKind};
use serde_json::Value;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::blind::{BlindLive, BlindSource, blind_live, private_truth};
use crate::common::*;
use crate::signal_062::FM_FIXTURE;

const TAG: &str = "SIGNAL-062/listen";
const CENTER_TOL_HZ: f64 = 100e3;
type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

fn station(fx: &hk_e2e::Fixture) -> TruthItem {
    fx.of_kind("wfm-broadcast")
        .first()
        .map(|t| (*t).clone())
        .expect("fixture truth has a wfm-broadcast station")
}

/// `hk serve`'s API wiring over a running blind run: inventory, status and the listen opener.
fn serve_live(live: &BlindLive) -> Server {
    let counters = live.handle.counters();
    // The default cap is 8 (T-066); pin 2 here so the third listener exercises the refusal.
    live.handle
        .set_listen_settings(hk_pipeline::ListenSettings {
            max_listeners: 2,
            ..Default::default()
        });
    let state = ApiState {
        inventory: Some(Arc::new(Mutex::new(repo(&live.dir.0)))),
        status: Some(Arc::new(move || counters.to_json())),
        on_demand: OpenerRegistry::new().with("listen", live.handle.listen_service()),
        ..ApiState::default()
    };
    let config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        Token::from_config(API_TOKEN).unwrap(),
    );
    Server::start(config, state).unwrap()
}

fn status(addr: SocketAddr) -> Value {
    let (code, body) = api_get(addr, "/api/status");
    assert_eq!(code, 200);
    serde_json::from_slice(&body).unwrap()
}

fn listen_counter(addr: SocketAddr, name: &str) -> u64 {
    status(addr)["listen"][name].as_u64().unwrap_or(0)
}

/// Polls `/api/inventory` until an emitter matches the private truth (moved by `shift_hz`);
/// returns `(id, f_center_hz, bandwidth_hz)` of the strongest-evidence match.
pub fn found_blind(addr: SocketAddr, truth: &TruthItem, shift_hz: f64) -> (String, f64, f64) {
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        let (_, rows) = api_inventory(addr);
        let hits = matching(
            truth,
            shift_hz,
            &rows,
            |r| {
                (
                    r["f_center_hz"].as_f64().unwrap_or(f64::NAN),
                    r["bandwidth_hz"].as_f64().unwrap_or(0.0),
                )
            },
            CENTER_TOL_HZ,
        );
        if let Some(best) = hits.iter().max_by_key(|r| {
            (
                r["bandwidth_hz"].as_f64().unwrap_or(0.0) as u64,
                r["count"].as_u64(),
            )
        }) {
            eprintln!(
                "[{TAG}] {} of {} inventory rows match the private truth; listening to {} at \
                 {:.4} MHz ({:.0} kHz)",
                hits.len(),
                rows.len(),
                best["id"],
                best["f_center_hz"].as_f64().unwrap() / 1e6,
                best["bandwidth_hz"].as_f64().unwrap() / 1e3
            );
            return (
                best["id"].as_str().unwrap().to_owned(),
                best["f_center_hz"].as_f64().unwrap(),
                best["bandwidth_hz"].as_f64().unwrap(),
            );
        }
        assert!(
            Instant::now() < deadline,
            "[{TAG}] the station was not found blind; inventory (MHz, kHz): {:?}",
            rows.iter()
                .map(|r| (
                    r["f_center_hz"].as_f64().unwrap_or(0.0) / 1e6,
                    r["bandwidth_hz"].as_f64().unwrap_or(0.0) / 1e3
                ))
                .collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

pub fn open(addr: SocketAddr, query: &str) -> Ws {
    let (mut ws, _) = tungstenite::connect(format!(
        "ws://{addr}/ws/open/listen?token={API_TOKEN}&{query}"
    ))
    .expect("upgrade");
    if let MaybeTlsStream::Plain(s) = ws.get_mut() {
        s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    }
    ws
}

/// The first message: the stream header, or the refusal JSON.
pub fn first(ws: &mut Ws) -> Result<StreamHeader, Value> {
    loop {
        if let Message::Text(t) = ws.read().expect("first message") {
            return match StreamHeader::from_json_bytes(t.as_bytes()) {
                Ok(h) => Ok(h),
                Err(_) => Err(serde_json::from_str(&t).unwrap()),
            };
        }
    }
}

/// Reads to the close; returns (binary messages, close code).
fn drain(ws: &mut Ws) -> (usize, Option<u16>) {
    let (mut bins, mut code) = (0, None);
    loop {
        match ws.read() {
            Ok(Message::Binary(_)) => bins += 1,
            Ok(Message::Close(f)) => code = f.map(|f| u16::from(f.code)),
            Ok(_) => {}
            Err(_) => return (bins, code),
        }
    }
}

pub fn close(mut ws: Ws) {
    let _ = ws.close(None);
    let _ = drain(&mut ws);
}

/// Mean power of `x` at `f` over Hann-windowed segments (Hz resolution `fs / seg`).
pub fn band_power(x: &[f32], fs: f64, f_lo: f64, f_hi: f64) -> f64 {
    const SEG: usize = 4800;
    let win: Vec<f64> = (0..SEG)
        .map(|n| 0.5 - 0.5 * (std::f64::consts::TAU * n as f64 / SEG as f64).cos())
        .collect();
    let df = fs / SEG as f64;
    let (k0, k1) = ((f_lo / df).round() as usize, (f_hi / df).round() as usize);
    let (mut acc, mut n) = (0.0, 0usize);
    for seg in x.chunks_exact(SEG) {
        for k in k0..=k1 {
            let (mut re, mut im) = (0.0f64, 0.0f64);
            let w = std::f64::consts::TAU * k as f64 / SEG as f64;
            for (i, (&v, &h)) in seg.iter().zip(&win).enumerate() {
                let a = f64::from(v) * h;
                re += a * (w * i as f64).cos();
                im += a * (w * i as f64).sin();
            }
            acc += re * re + im * im;
            n += 1;
        }
    }
    acc / n.max(1) as f64
}

pub fn db(x: f64) -> f64 {
    10.0 * x.max(1e-30).log10()
}

#[test]
fn signal_062_listen_streams_auto_demodulated_fm_audio_and_detaches() {
    let Some((meta, fx)) = private_truth(FM_FIXTURE) else {
        return;
    };
    let truth = station(&fx);
    let live = blind_live(&meta, "lsn", BlindSource::default());
    let server = serve_live(&live);
    let addr = server.local_addr();
    let (emitter, f_center, bw) = found_blind(addr, &truth, 0.0);

    let started = Instant::now();
    let mut ws = open(addr, &format!("emitter={emitter}"));
    let header = first(&mut ws).unwrap_or_else(|r| panic!("[{TAG}] refused: {r}"));
    eprintln!(
        "[{TAG}] header after {:.2} s: {}",
        started.elapsed().as_secs_f64(),
        serde_json::to_string(&header).unwrap()
    );
    assert_eq!(header.kind, StreamKind::Audio);
    assert_eq!(header.datatype.as_deref(), Some("ri16_le"));
    assert_eq!(header.sample_rate_hz, Some(48_000.0));
    assert_eq!(header.content_class, hk_model::ContentClass::Unrestricted);
    let audio = header.audio.clone().expect("audio profile");
    assert_eq!(audio.mode, "wfm", "[{TAG}] auto mode must select WFM");
    let pilot = audio.params.pilot_hz.expect("probe found the 19 kHz pilot");
    assert!((pilot - 19_000.0).abs() < 20.0, "[{TAG}] pilot {pilot}");
    let (tlo, thi) = (truth.f_lo_hz, truth.f_hi_hz);
    let channel = header.center_hz.unwrap();
    assert!(
        (channel - 0.5 * (tlo + thi)).abs() < 50e3,
        "[{TAG}] demodulated channel {channel} Hz is the truth station"
    );

    // A second listener (by selection) is admitted; a third is refused at the cap.
    let mut second = open(
        addr,
        &format!("f_lo={}&f_hi={}", f_center - bw / 2.0, f_center + bw / 2.0),
    );
    assert!(first(&mut second).is_ok(), "[{TAG}] second listener");
    let mut third = open(addr, &format!("emitter={emitter}"));
    let refused = first(&mut third).expect_err("third listener refused");
    assert_eq!(refused["status"], 503, "{refused}");
    assert_eq!(drain(&mut third), (0, Some(4503)));
    close(second);

    // Collect ≥ 3 s of audio.
    const WANT: usize = 3 * 48_000;
    let mut pcm: Vec<f32> = Vec::with_capacity(WANT);
    let (mut last_seq, mut seq_gaps, mut dropped, mut statuses) = (None::<u64>, 0u64, 0u64, 0);
    let mut max_latency_ms: f64 = 0.0;
    let deadline = Instant::now() + Duration::from_secs(300);
    while pcm.len() < WANT {
        assert!(
            Instant::now() < deadline,
            "[{TAG}] only {} audio samples",
            pcm.len()
        );
        let Message::Binary(b) = ws.read().expect("audio record") else {
            continue;
        };
        let h = BinaryRecordHeader::decode(&b).expect("record header");
        if let Some(prev) = last_seq {
            seq_gaps += h.seq.saturating_sub(prev + 1);
        }
        last_seq = Some(h.seq);
        match h.record_type {
            1 => pcm.extend(hk_stream::audio::decode_pcm(&b[32..])),
            2 => dropped += u64::from_le_bytes(b[32..40].try_into().unwrap()),
            _ => {
                let (_, v) = parse_status_record(&b).expect("status record");
                statuses += 1;
                max_latency_ms = max_latency_ms.max(v["latency_ms"].as_f64().unwrap_or(0.0));
            }
        }
    }
    let audio_s = pcm.len() as f64 / 48_000.0;
    eprintln!(
        "[{TAG}] {audio_s:.2} s of audio in {:.1} s wall, {statuses} status records, {seq_gaps} seq \
         gaps, {dropped} dropped, max latency {max_latency_ms:.1} ms",
        started.elapsed().as_secs_f64()
    );

    // Demodulated FM audio: loud enough, energy in the audio band, pilot removed, filtered top.
    let x = &pcm[12_000..];
    let rms = (x.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / x.len() as f64).sqrt();
    let audio_band = band_power(x, 48_000.0, 200.0, 5_000.0);
    let pilot_band = band_power(x, 48_000.0, 18_950.0, 19_050.0);
    let above = band_power(x, 48_000.0, 20_000.0, 23_000.0);
    eprintln!(
        "[{TAG}] rms {rms:.4}; mean bin power: audio {:.1} dB, 19 kHz {:.1} dB, 20–23 kHz {:.1} dB",
        db(audio_band),
        db(pilot_band),
        db(above)
    );
    assert!(rms > 0.01, "[{TAG}] audio is silent (rms {rms})");
    assert!(
        db(audio_band) - db(pilot_band) > 30.0,
        "[{TAG}] 19 kHz pilot not removed from the audio"
    );
    assert!(
        db(audio_band) - db(above) > 30.0,
        "[{TAG}] audio is not low-passed programme audio"
    );
    let latency_us = listen_counter(addr, "latency_us_max");
    assert!(
        latency_us < 500_000,
        "[{TAG}] processing latency {latency_us} µs"
    );

    // Detach: close the socket; the chain stops and no further frames are produced.
    close(ws);
    let t0 = Instant::now();
    while listen_counter(addr, "running") > 0 {
        assert!(
            t0.elapsed() < Duration::from_secs(30),
            "[{TAG}] chain still attached"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let frames = listen_counter(addr, "frames");
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        listen_counter(addr, "frames"),
        frames,
        "[{TAG}] frames stop on detach"
    );
    let s = status(addr)["listen"].clone();
    eprintln!("[{TAG}] listen counters: {s}");
    assert_eq!(s["attached"], s["detached"]);
    assert_eq!(s["refused_busy"], 1);
    assert_eq!(s["refused_class"], 0);

    live.handle.stop();
    finish(live.handle);
}
