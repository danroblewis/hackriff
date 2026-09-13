//! Spike S3 frame server (throwaway).
//!
//! Serves the built client from `--dist` and a WebSocket at `/ws`.
//! Per connection: one JSON text header, then binary SpectrumFrames.
//!
//! Binary frame layout (little-endian, 24-byte header, payload aligned to 4):
//!   0  u32  seq           (increments per generated frame, including dropped ones)
//!   4  u8   dtype         (0 = f32 dBFS, 1 = u8 quantised over [min_db, max_db])
//!   5  u8   version       (1)
//!   6  u16  reserved
//!   8  u32  bins
//!   12 u32  reserved
//!   16 f64  t_unix_ms     (generation time, wall clock)
//!   24 ...  payload       (bins * 4 bytes f32, or bins bytes u8)
//!
//! Query params on /ws: bins (default 4096), fps (30), dtype (u8|f32).
//! Backpressure (ADR-0004 style): the generator never blocks. Frames go into a
//! bounded queue; when the socket can't keep up the frame is dropped and the
//! client sees a seq gap.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use serde::Deserialize;
use std::{
    net::SocketAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;
use tower_http::services::ServeDir;

const MIN_DB: f32 = -130.0;
const MAX_DB: f32 = -10.0;
const HDR: usize = 24;

#[derive(Deserialize, Clone)]
struct Params {
    bins: Option<usize>,
    fps: Option<f64>,
    dtype: Option<String>,
}

#[tokio::main]
async fn main() {
    let mut bind = "127.0.0.1:8080".to_string();
    let mut dist = "../client/dist".to_string();
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--bind" => {
                bind = args[i + 1].clone();
                i += 1
            }
            "--dist" => {
                dist = args[i + 1].clone();
                i += 1
            }
            _ => {}
        }
        i += 1;
    }
    let app = Router::new()
        .route("/ws", get(ws_handler))
        .fallback_service(ServeDir::new(&dist));
    let addr: SocketAddr = bind.parse().expect("bad --bind");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    eprintln!("s3-frame-server listening on http://{addr}/  (dist={dist}, pid={})", std::process::id());
    axum::serve(listener, app).await.unwrap();
}

async fn ws_handler(ws: WebSocketUpgrade, Query(p): Query<Params>) -> impl IntoResponse {
    ws.on_upgrade(move |sock| session(sock, p))
}

fn now_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64() * 1000.0
}

async fn session(mut sock: WebSocket, p: Params) {
    let bins = p.bins.unwrap_or(4096).clamp(64, 1 << 20);
    let fps = p.fps.unwrap_or(30.0).clamp(1.0, 240.0);
    let use_u8 = p.dtype.as_deref() != Some("f32");
    let header = serde_json::json!({
        "type": "hk.spectrum.header",
        "schema": "s3-spike/1",
        "bins": bins,
        "center_hz": 433.92e6,
        "span_hz": 20.0e6,
        "fps": fps,
        "dtype": if use_u8 { "u8" } else { "f32" },
        "units": "dBFS",
        "u8_min_db": MIN_DB,
        "u8_max_db": MAX_DB,
        "frame_header_bytes": HDR,
        "layout": "seq:u32 dtype:u8 ver:u8 rsv:u16 bins:u32 rsv:u32 t_unix_ms:f64 | payload",
    });
    if sock.send(Message::Text(header.to_string().into())).await.is_err() {
        return;
    }

    // Bounded queue between generator and socket writer: drop on full.
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(4);
    let gen = tokio::spawn(async move {
        let mut g = Gen::new(bins);
        let mut tick = tokio::time::interval(Duration::from_secs_f64(1.0 / fps));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut spec = vec![0f32; bins];
        let mut seq: u32 = 0;
        let mut dropped: u64 = 0;
        loop {
            tick.tick().await;
            g.step(&mut spec, 1.0 / fps as f32);
            let mut buf = Vec::with_capacity(HDR + bins * if use_u8 { 1 } else { 4 });
            buf.extend_from_slice(&seq.to_le_bytes());
            buf.push(if use_u8 { 1 } else { 0 });
            buf.push(1);
            buf.extend_from_slice(&0u16.to_le_bytes());
            buf.extend_from_slice(&(bins as u32).to_le_bytes());
            buf.extend_from_slice(&0u32.to_le_bytes());
            buf.extend_from_slice(&now_ms().to_le_bytes());
            if use_u8 {
                let k = 255.0 / (MAX_DB - MIN_DB);
                buf.extend(spec.iter().map(|&d| ((d - MIN_DB) * k).clamp(0.0, 255.0) as u8));
            } else {
                for d in &spec {
                    buf.extend_from_slice(&d.to_le_bytes());
                }
            }
            seq = seq.wrapping_add(1);
            match tx.try_send(buf) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => dropped += 1,
                Err(mpsc::error::TrySendError::Closed(_)) => break,
            }
        }
        eprintln!("session end: bins={bins} fps={fps} generated={seq} server_dropped={dropped}");
    });
    while let Some(buf) = rx.recv().await {
        if sock.send(Message::Binary(buf.into())).await.is_err() {
            break;
        }
    }
    drop(rx);
    let _ = gen.await;
}

/// Synthetic spectrum: noise floor with exponential (chi-square 2 dof) power
/// fluctuation, drifting tones, an FM-like wide hump, a slow chirp, and random
/// short bursts. Output in dBFS.
struct Gen {
    bins: usize,
    t: f32,
    rng: u64,
    bursts: Vec<(usize, usize, f32, f32)>, // (start, width, level_db, remaining_s)
    tilt: Vec<f32>,
}

impl Gen {
    fn new(bins: usize) -> Self {
        let tilt = (0..bins)
            .map(|i| {
                let x = i as f32 / bins as f32 * 2.0 - 1.0;
                -3.0 * x * x // gentle filter roll-off at the band edges
            })
            .collect();
        Gen { bins, t: 0.0, rng: 0x9E3779B97F4A7C15, bursts: Vec::new(), tilt }
    }
    fn rnd(&mut self) -> f32 {
        // xorshift64*
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        ((self.rng.wrapping_mul(0x2545F4914F6CDD1D) >> 40) as f32) / (1u64 << 24) as f32
    }
    fn add_db(spec: &mut [f32], i: usize, level_db: f32) {
        // power-add a component onto the bin
        let a = 10f32.powf(spec[i] / 10.0) + 10f32.powf(level_db / 10.0);
        spec[i] = 10.0 * a.log10();
    }
    fn step(&mut self, spec: &mut [f32], dt: f32) {
        self.t += dt;
        let n = self.bins;
        let nf = n as f32;
        for i in 0..n {
            let u = self.rnd().max(1e-7);
            // exponential power around the floor -> dB
            spec[i] = -105.0 + self.tilt[i] + 10.0 * (-u.ln()).log10();
        }
        let t = self.t;
        // drifting CW tones with narrow Gaussian skirts
        let tones = [
            (0.12 + 0.02 * (t * 0.3).sin(), -40.0),
            (0.37 + 0.005 * (t * 1.1).sin(), -55.0),
            (0.61, -70.0),
            (0.83 + 0.03 * (t * 0.07).cos(), -48.0),
        ];
        for (f, lvl) in tones {
            let c = (f * nf) as isize;
            let w = (nf / 4096.0 * 2.0).max(1.0) as isize;
            for d in -4 * w..=4 * w {
                let j = c + d;
                if j >= 0 && (j as usize) < n {
                    let g = -(d as f32 / w as f32).powi(2) * 3.0;
                    Self::add_db(spec, j as usize, lvl + g);
                }
            }
        }
        // FM-like hump (~200 kHz of 20 MHz) with modulation wobble
        let c = 0.5 + 0.004 * (t * 5.0).sin();
        let hw = (nf * 0.005) as isize;
        let cc = (c * nf) as isize;
        for d in -hw..=hw {
            let j = cc + d;
            if j >= 0 && (j as usize) < n {
                let x = d as f32 / hw as f32;
                Self::add_db(spec, j as usize, -60.0 - 12.0 * x * x + 3.0 * self.rnd());
            }
        }
        // slow chirp sweeping the span every 8 s
        let cf = ((t / 8.0).fract() * nf) as usize;
        for d in 0..(nf / 2048.0).max(2.0) as usize {
            if cf + d < n {
                Self::add_db(spec, cf + d, -75.0);
            }
        }
        // random bursts (pager/ISM-like), 20-300 ms, various widths
        if self.rnd() < 0.15 {
            let w = ((nf * (0.0005 + 0.01 * self.rnd())) as usize).max(1);
            let s = (self.rnd() * (nf - w as f32)) as usize;
            let lvl = -85.0 + 45.0 * self.rnd();
            let dur = 0.02 + 0.28 * self.rnd();
            self.bursts.push((s, w, lvl, dur));
        }
        let mut bursts = std::mem::take(&mut self.bursts);
        for b in bursts.iter_mut() {
            for j in b.0..(b.0 + b.1).min(n) {
                let r = self.rnd();
                Self::add_db(spec, j, b.2 + 2.0 * r);
            }
            b.3 -= dt;
        }
        bursts.retain(|b| b.3 > 0.0);
        self.bursts = bursts;
    }
}
