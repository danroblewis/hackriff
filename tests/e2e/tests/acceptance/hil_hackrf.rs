//! T-053 (T5 HIL): the device-interface acceptance checks against the **real HackRF One** over live
//! air in the FM broadcast band. **Receive only.** Ignored; never gates CI. Run on the bench rig
//! after checking the device is free (`hackrf_info`), one HackRF user at a time:
//!
//! ```text
//! HK_DEVICE=hackrf cargo test --release -p hk-e2e --features hk-core/hackrf,hk-cli/hackrf \
//!     --test acceptance_m0 -- --ignored --nocapture hil_
//! ```
//!
//! Truth comes from a **blind live survey**, not fixture metadata: the device is opened directly
//! (8 Msps windows across 86–109 MHz, LNA 32 / VGA 30 / amp on), strong channels are picked from
//! Welch channel power over a lower-quartile floor, and the device is closed. The survey result is
//! held only by the test. `hk serve --hackrf` then runs the whole pipeline on the densest 2.4 Msps
//! window, which knows nothing of the survey, and the test matches what it produced:
//!
//! - **SIGNAL-062:** every strong surveyed station detected (inventory centre within 100 kHz) with
//!   `fm-broadcast` in the top-k; RDS PI decoded blind on at least one; Listen streams WFM audio on
//!   the strongest one.
//! - **SPACE-050-style floor:** `/api/floor` steps over the run are finite and stable (uncalibrated,
//!   there is no calibration for this serial).
//! - **AWARE-042-style occupancy:** `/api/history` occupancy at the station columns is well above
//!   the columns the survey found empty.
//! - **Retune within the band:** live control moves to a second surveyed window and its stations
//!   are detected there.
//!
//! Every check is recorded; the table (`HIL-RESULT` lines) is printed before the final assert.

use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hk_cli::pipeline::{LiveArgs, LiveSource, open_live, temp_data_dir};
use hk_cli::serve::{ServeOptions, ServeSource, Serving, start};
use hk_dsp::welch::{WelchConfig, welch};
use hk_stream::record::parse_status_record;
use hk_stream::{BinaryRecordHeader, StreamKind};
use num_complex::Complex32;
use serde_json::Value;
use tungstenite::Message;

use crate::blind::{TOP_K, reasonable_services};
use crate::common::*;
use crate::listen::{band_power, close, db, first, open};

const TAG: &str = "T-053/hil";
const LNA_DB: f64 = 32.0;
const VGA_DB: f64 = 30.0;
const AMP: bool = true;

const SURVEY_RATE_HZ: f64 = 8e6;
/// Overlapping 8 Msps windows: every frequency in 86.2–109.3 MHz lies inside the baseband filter
/// (±3 MHz) of a window whose centre (DC, LO leakage) is at least 300 kHz away.
const SURVEY_CENTRES_HZ: [f64; 8] = [89.0e6, 91.5e6, 94.0e6, 96.5e6, 99.0e6, 101.5e6, 104.0e6, 106.5e6];
const SURVEY_EDGE_HZ: f64 = 2.8e6;
const SURVEY_DC_HZ: f64 = 300e3;
const SURVEY_S: f64 = 0.5;
const CHANNEL_HALF_HZ: f64 = 75e3;
/// Channel power over the survey floor for a station.
const STATION_SNR_DB: f64 = 15.0;
/// ... and for a station the pipeline must find.
const STRONG_SNR_DB: f64 = 20.0;

const RUN_RATE_HZ: f64 = 2.4e6;
/// Inside the 1.8 MHz baseband filter (0.75 x 2.4 Msps) with room for a 200 kHz channel.
const RUN_EDGE_HZ: f64 = 0.7e6;
const RUN_DC_HZ: f64 = 150e3;
const MATCH_TOL_HZ: f64 = 100e3;

fn live_args(center_hz: f64, sample_rate_hz: f64) -> LiveArgs {
    LiveArgs {
        center_hz,
        sample_rate_hz,
        lna_db: LNA_DB,
        vga_db: VGA_DB,
        amp: AMP,
        baseband_filter_hz: None,
        gains: Vec::new(),
    }
}

#[derive(Clone, Debug)]
struct Station {
    f_hz: f64,
    snr_db: f64,
}

struct Results(Vec<(String, &'static str, String)>);

impl Results {
    fn add(&mut self, test: impl Into<String>, pass: bool, detail: impl Into<String>) {
        self.push(test, if pass { "PASS" } else { "FAIL" }, detail);
    }
    fn push(&mut self, test: impl Into<String>, outcome: &'static str, detail: impl Into<String>) {
        let (test, detail) = (test.into(), detail.into());
        eprintln!("[{TAG}] {outcome} {test}: {detail}");
        self.0.push((test, outcome, detail));
    }
}

fn unix_s() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn mhz(f: f64) -> String {
    format!("{:.3}", f / 1e6)
}

/// `n` samples at `center_hz`, after the retune reached the stream and 0.25 s settled.
fn capture(src: &mut LiveSource, center_hz: f64, n: usize) -> Vec<Complex32> {
    let mut buf = Vec::new();
    let mut skip = (0.25 * SURVEY_RATE_HZ) as usize;
    let mut tuned = false;
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let h = src
            .source
            .read_block_ci8(&mut buf)
            .expect("read the HackRF")
            .expect("the live stream does not end");
        if !tuned {
            if (h.center_hz() - center_hz).abs() > 1.0 {
                continue;
            }
            tuned = true;
        }
        if skip > 0 {
            skip = skip.saturating_sub(buf.len());
            continue;
        }
        out.extend(
            buf.iter()
                .map(|s| Complex32::new(f32::from(s.re) / 128.0, f32::from(s.im) / 128.0)),
        );
    }
    out.truncate(n);
    out
}

/// Blind station picking on one survey window: 150 kHz channel power at every bin, local maxima
/// over ±150 kHz standing `STATION_SNR_DB` above the lower-quartile channel power; centre is the
/// PSD centroid over ±100 kHz.
fn stations_in(psd: &[f32], fc: f64, fs: f64) -> (Vec<Station>, f64) {
    let n = psd.len();
    let df = fs / n as f64;
    let half = (CHANNEL_HALF_HZ / df).round() as usize;
    let freq = |k: usize| fc + (k as f64 - (n / 2) as f64) * df;
    let mut cum = vec![0f64; n + 1];
    for k in 0..n {
        cum[k + 1] = cum[k] + f64::from(psd[k]);
    }
    let ch = |k: usize| cum[k + half + 1] - cum[k - half];
    let usable: Vec<usize> = (half..n - half)
        .filter(|&k| {
            let o = (freq(k) - fc).abs();
            (SURVEY_DC_HZ..=SURVEY_EDGE_HZ).contains(&o)
        })
        .collect();
    let mut powers: Vec<f64> = usable.iter().map(|&k| ch(k)).collect();
    powers.sort_by(f64::total_cmp);
    // 10th percentile: most FM channels are occupied, so a quartile sits on weak stations.
    let floor = powers[powers.len() / 10];
    let cent = (100e3 / df).round() as usize;
    let mut out = Vec::new();
    for &k in &usable {
        let p = ch(k);
        let snr_db = 10.0 * (p / floor).log10();
        if snr_db < STATION_SNR_DB {
            continue;
        }
        let (lo, hi) = (k.saturating_sub(2 * half).max(half), (k + 2 * half).min(n - half - 1));
        if (lo..=hi).any(|j| ch(j) > p || (ch(j) == p && j < k)) {
            continue;
        }
        let (a, b) = (k.saturating_sub(cent), (k + cent).min(n - 1));
        let (mut w, mut wf) = (0f64, 0f64);
        for j in a..=b {
            w += f64::from(psd[j]);
            wf += f64::from(psd[j]) * freq(j);
        }
        out.push(Station {
            f_hz: (wf / w / 1e3).round() * 1e3,
            snr_db,
        });
    }
    (out, floor)
}

/// Merges stations seen in overlapping windows (within 100 kHz), keeping the higher SNR.
fn merge(mut all: Vec<Station>) -> Vec<Station> {
    all.sort_by(|a, b| b.snr_db.total_cmp(&a.snr_db));
    let mut out: Vec<Station> = Vec::new();
    for s in all {
        if out.iter().all(|o| (o.f_hz - s.f_hz).abs() > MATCH_TOL_HZ) {
            out.push(s);
        }
    }
    out.sort_by(|a, b| a.f_hz.total_cmp(&b.f_hz));
    out
}

/// Strong stations a 2.4 Msps run at `c` should see (inside the filter with a channel's margin,
/// clear of DC).
fn in_window(stations: &[Station], c: f64, min_snr: f64) -> Vec<Station> {
    stations
        .iter()
        .filter(|s| {
            let o = (s.f_hz - c).abs();
            s.snr_db >= min_snr && (RUN_DC_HZ..=RUN_EDGE_HZ).contains(&o)
        })
        .cloned()
        .collect()
}

/// The 100 kHz-grid centre with the most strong stations (then the most SNR), excluding centres
/// within `avoid.1` of `avoid.0`.
fn best_window(stations: &[Station], avoid: Option<(f64, f64)>) -> Option<f64> {
    let score = |c: f64| {
        let w = in_window(stations, c, STRONG_SNR_DB);
        (w.len(), w.iter().map(|s| s.snr_db).sum::<f64>())
    };
    (885..=1075)
        .map(|k| f64::from(k) * 1e5)
        .filter(|c| avoid.is_none_or(|(a, d)| (c - a).abs() >= d))
        .map(|c| (c, score(c)))
        .filter(|(_, (n, _))| *n > 0)
        .max_by(|a, b| a.1.0.cmp(&b.1.0).then(a.1.1.total_cmp(&b.1.1)))
        .map(|(c, _)| c)
}

fn matched<'a>(rows: &'a [Value], s: &Station) -> Vec<&'a Value> {
    rows.iter()
        .filter(|r| (r["f_center_hz"].as_f64().unwrap_or(f64::NAN) - s.f_hz).abs() <= MATCH_TOL_HZ)
        .collect()
}

fn fm_top_k(r: &Value) -> bool {
    let want = reasonable_services("wfm-broadcast");
    r["explanations"].as_array().is_some_and(|x| {
        x.iter()
            .take(TOP_K)
            .any(|e| e["service"].as_str().is_some_and(|s| want.contains(&s)))
    })
}

fn pi(r: &Value) -> Option<String> {
    (r["identity_scheme"] == "rds-pi")
        .then(|| r["identity_value"].as_str().map(str::to_owned))
        .flatten()
}

fn found(rows: &[Value], s: &Station) -> bool {
    matched(rows, s).iter().any(|r| fm_top_k(r))
}

fn get_json(addr: SocketAddr, path: &str) -> Value {
    let (code, body) = api_get(addr, path);
    assert_eq!(code, 200, "{path}: {}", String::from_utf8_lossy(&body));
    serde_json::from_slice(&body).unwrap()
}

fn percentile(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * p).round() as usize]
}

/// Waits until `pred(rows)` or `timeout`; returns the last rows.
fn poll_inventory(addr: SocketAddr, timeout: Duration, pred: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let t = Instant::now();
    loop {
        let (_, rows) = api_inventory(addr);
        if pred(&rows) || t.elapsed() >= timeout {
            return rows;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

#[test]
#[ignore = "HIL (T-053): HK_DEVICE=hackrf, --features hk-core/hackrf, a HackRF One, receive-only"]
fn hil_blind_fm_survey_on_the_hackrf() {
    if !hk_e2e::synth::hardware_device_selected() {
        eprintln!(
            "SKIP hil_blind_fm_survey_on_the_hackrf: set HK_DEVICE=hackrf to select the real HackRF"
        );
        return;
    }
    let mut res = Results(Vec::new());

    // 1. Blind live survey (the test's private truth).
    let t_survey = Instant::now();
    let mut src = open_live("hackrf", &live_args(SURVEY_CENTRES_HZ[0], SURVEY_RATE_HZ))
        .expect("open the HackRF (is it free? built with --features hk-core/hackrf?)");
    let hw = src.device.hw.clone();
    let control = src.control.clone();
    let mut all = Vec::new();
    for (i, &c) in SURVEY_CENTRES_HZ.iter().enumerate() {
        if i > 0 {
            control.tune(c).expect("survey retune");
        }
        let iq = capture(&mut src, c, (SURVEY_S * SURVEY_RATE_HZ) as usize);
        let cfg = WelchConfig {
            holds: false,
            spectral_kurtosis: false,
            ..WelchConfig::new(8192)
        };
        let psd = welch(&iq, SURVEY_RATE_HZ, c, &cfg).unwrap().psd;
        let (st, floor) = stations_in(&psd, c, SURVEY_RATE_HZ);
        eprintln!(
            "[{TAG}] survey {} MHz: floor {:.1} dBFS/150 kHz, {} stations",
            mhz(c),
            db(floor),
            st.len()
        );
        all.extend(st);
    }
    let survey_stats = control.stats();
    drop(control);
    drop(src);
    let stations = merge(all);
    eprintln!("[{TAG}] device: {hw}");
    eprintln!(
        "[{TAG}] survey in {:.1} s, source stats {survey_stats:?}; stations (MHz, dB): {:?}",
        t_survey.elapsed().as_secs_f64(),
        stations
            .iter()
            .map(|s| (mhz(s.f_hz), (s.snr_db * 10.0).round() / 10.0))
            .collect::<Vec<_>>()
    );
    let strong = stations.iter().filter(|s| s.snr_db >= STRONG_SNR_DB).count();
    res.add(
        "survey",
        strong > 0,
        format!("{} stations >= {STATION_SNR_DB} dB, {strong} >= {STRONG_SNR_DB} dB", stations.len()),
    );
    let ca = best_window(&stations, None).expect("the survey found a strong FM station");
    let truth_a = in_window(&stations, ca, STRONG_SNR_DB);
    let cb = best_window(&stations, Some((ca, 2.4e6)));
    eprintln!(
        "[{TAG}] run window {} MHz: truth {:?}; retune window {:?}",
        mhz(ca),
        truth_a.iter().map(|s| mhz(s.f_hz)).collect::<Vec<_>>(),
        cb.map(mhz)
    );
    std::thread::sleep(Duration::from_millis(500));

    // 2. The whole pipeline over the device, blind.
    let dir = temp_data_dir();
    let Serving {
        server,
        handle,
        live_control,
        source_control,
        ..
    } = start(&ServeOptions {
        source: ServeSource::HackRf {
            spec: "hackrf".into(),
            live: live_args(ca, RUN_RATE_HZ),
        },
        data_dir: Some(dir.clone()),
        bind: "127.0.0.1:0".parse().unwrap(),
        ui_dist: None,
        fft_len: 1024,
        rows_per_s: 25.0,
        calibration: None,
        token: Some(API_TOKEN.into()),
        listen: Default::default(),
        compute: Default::default(),
    })
    .expect("start hk serve over the HackRF");
    let addr = server.local_addr();
    let (run_start, t0) = (Instant::now(), unix_s());

    // SIGNAL-062: detection + FM broadcast top-k.
    let rows = poll_inventory(addr, Duration::from_secs(90), |rows| {
        truth_a.iter().all(|s| found(rows, s))
    });
    eprintln!(
        "[{TAG}] inventory after {:.0} s: {} rows (MHz, kHz, top-{TOP_K}): {:?}",
        run_start.elapsed().as_secs_f64(),
        rows.len(),
        rows.iter()
            .map(|r| (
                mhz(r["f_center_hz"].as_f64().unwrap_or(0.0)),
                (r["bandwidth_hz"].as_f64().unwrap_or(0.0) / 1e3).round(),
                r["explanations"]
                    .as_array()
                    .map(|x| x.iter().take(TOP_K).map(|e| e["service"].clone()).collect::<Vec<_>>())
            ))
            .collect::<Vec<_>>()
    );
    for s in &truth_a {
        let m = matched(&rows, s);
        res.add(
            format!("SIGNAL-062 detect {} MHz ({:.0} dB)", mhz(s.f_hz), s.snr_db),
            !m.is_empty(),
            format!(
                "{} inventory rows within 100 kHz: {:?}",
                m.len(),
                m.iter().map(|r| mhz(r["f_center_hz"].as_f64().unwrap_or(0.0))).collect::<Vec<_>>()
            ),
        );
        res.add(
            format!("SIGNAL-062 FM broadcast top-{TOP_K} {} MHz", mhz(s.f_hz)),
            m.iter().any(|r| fm_top_k(r)),
            format!(
                "top-{TOP_K}: {:?}",
                m.iter()
                    .map(|r| r["explanations"]
                        .as_array()
                        .map(|x| x.iter().take(TOP_K).map(|e| e["service"].clone()).collect::<Vec<_>>()))
                    .collect::<Vec<_>>()
            ),
        );
    }

    // Listen on the strongest station found.
    let target = truth_a
        .iter()
        .filter(|s| found(&rows, s))
        .max_by(|a, b| a.snr_db.total_cmp(&b.snr_db))
        .and_then(|s| {
            matched(&rows, s)
                .into_iter()
                .filter(|r| fm_top_k(r))
                .max_by_key(|r| {
                    (
                        r["bandwidth_hz"].as_f64().unwrap_or(0.0) as u64,
                        r["count"].as_u64(),
                    )
                })
                .map(|r| (s.clone(), r["id"].as_str().unwrap().to_owned()))
        });
    match target {
        None => res.add("SIGNAL-062 Listen", false, "no station found to listen to"),
        Some((s, id)) => {
            let started = Instant::now();
            let mut ws = open(addr, &format!("emitter={id}"));
            match first(&mut ws) {
                Err(refused) => res.add("SIGNAL-062 Listen", false, format!("refused: {refused}")),
                Ok(h) => {
                    let mode = h.audio.as_ref().map(|a| a.mode.clone()).unwrap_or_default();
                    let pilot = h.audio.as_ref().and_then(|a| a.params.pilot_hz);
                    const WANT: usize = 3 * 48_000;
                    let mut pcm: Vec<f32> = Vec::with_capacity(WANT);
                    let (mut last_seq, mut seq_gaps, mut dropped) = (None::<u64>, 0u64, 0u64);
                    let mut max_latency_ms: f64 = 0.0;
                    let deadline = Instant::now() + Duration::from_secs(60);
                    while pcm.len() < WANT && Instant::now() < deadline {
                        let Ok(msg) = ws.read() else { break };
                        let Message::Binary(b) = msg else { continue };
                        let rh = BinaryRecordHeader::decode(&b).expect("record header");
                        if let Some(prev) = last_seq {
                            seq_gaps += rh.seq.saturating_sub(prev + 1);
                        }
                        last_seq = Some(rh.seq);
                        match rh.record_type {
                            1 => pcm.extend(hk_stream::audio::decode_pcm(&b[32..])),
                            2 => dropped += u64::from_le_bytes(b[32..40].try_into().unwrap()),
                            _ => {
                                if let Some((_, v)) = parse_status_record(&b) {
                                    max_latency_ms =
                                        max_latency_ms.max(v["latency_ms"].as_f64().unwrap_or(0.0));
                                }
                            }
                        }
                    }
                    close(ws);
                    let x = &pcm[pcm.len().min(12_000)..];
                    let rms = (x.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>()
                        / x.len().max(1) as f64)
                        .sqrt();
                    let (audio, pilot_band) = (
                        band_power(x, 48_000.0, 200.0, 5_000.0),
                        band_power(x, 48_000.0, 18_950.0, 19_050.0),
                    );
                    let channel = h.center_hz.unwrap_or(f64::NAN);
                    let pass = h.kind == StreamKind::Audio
                        && mode == "wfm"
                        && pcm.len() >= WANT
                        && rms > 0.01
                        && db(audio) - db(pilot_band) > 30.0
                        && (channel - s.f_hz).abs() < MATCH_TOL_HZ;
                    res.add(
                        format!("SIGNAL-062 Listen {} MHz", mhz(s.f_hz)),
                        pass,
                        format!(
                            "mode {mode}, pilot {pilot:?} Hz, channel {} MHz, {:.2} s audio in {:.1} s, \
                             rms {rms:.3}, audio-19 kHz {:.1} dB, {seq_gaps} seq gaps, {dropped} dropped, \
                             max latency {max_latency_ms:.0} ms",
                            mhz(channel),
                            pcm.len() as f64 / 48_000.0,
                            started.elapsed().as_secs_f64(),
                            db(audio) - db(pilot_band)
                        ),
                    );
                }
            }
        }
    }

    // RDS PI, blind, on the stations (given up to 150 s of run).
    let rows = poll_inventory(
        addr,
        Duration::from_secs(150).saturating_sub(run_start.elapsed()),
        |rows| truth_a.iter().all(|s| matched(rows, s).iter().any(|r| pi(r).is_some())),
    );
    let pis: Vec<(String, Option<String>)> = truth_a
        .iter()
        .map(|s| (mhz(s.f_hz), matched(&rows, s).iter().find_map(|r| pi(r))))
        .collect();
    let any_pi = pis.iter().any(|(_, p)| p.is_some());
    res.add(
        "SIGNAL-062 RDS PI (where present)",
        any_pi,
        format!("after {:.0} s: {pis:?}", run_start.elapsed().as_secs_f64()),
    );

    // SPACE-050-style floor over the run so far.
    let t1 = unix_s();
    let region = format!(
        "f_lo={}&f_hi={}&t0={}&t1={}",
        ca - RUN_RATE_HZ / 2.0,
        ca + RUN_RATE_HZ / 2.0,
        t0.floor() - 2.0,
        t1.ceil() + 2.0
    );
    let floor = get_json(addr, &format!("/api/floor?{region}&max_steps=1000"));
    let steps = floor["steps"].as_array().cloned().unwrap_or_default();
    let mut vals: Vec<f64> = steps
        .iter()
        .filter_map(|s| s["value_db_per_hz"].as_f64())
        .filter(|v| v.is_finite())
        .collect();
    let unit = steps
        .first()
        .and_then(|s| s["unit"].as_str())
        .unwrap_or("-")
        .to_owned();
    if vals.len() >= 10 {
        let (p10, p50, p90) = (
            percentile(&mut vals, 0.1),
            percentile(&mut vals, 0.5),
            percentile(&mut vals, 0.9),
        );
        res.add(
            "SPACE-050 floor vs time (uncalibrated)",
            p90 - p10 < 3.0,
            format!(
                "{} of {} steps finite, {unit}, median {p50:.1}, p10-p90 spread {:.2} dB; \
                 calibrated floor skipped (no calibration for this serial)",
                vals.len(),
                steps.len(),
                p90 - p10
            ),
        );
    } else {
        res.add(
            "SPACE-050 floor vs time (uncalibrated)",
            false,
            format!("{} finite of {} steps", vals.len(), steps.len()),
        );
    }

    // AWARE-042-style occupancy: station columns vs columns the survey found empty.
    let hist = get_json(addr, &format!("/api/history?{region}&max_cells=40000"));
    let (nt, nf) = (
        hist["nt"].as_u64().unwrap_or(0) as usize,
        hist["nf"].as_u64().unwrap_or(0) as usize,
    );
    let (f_lo, f_cell) = (
        hist["f_lo_hz"].as_f64().unwrap_or(0.0),
        hist["f_cell_hz"].as_f64().unwrap_or(1.0),
    );
    let occ = hist["occupancy"].as_array().cloned().unwrap_or_default();
    let col_occ = |j: usize| {
        let v: Vec<f64> = (0..nt).filter_map(|i| occ.get(i * nf + j)?.as_f64()).collect();
        (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
    };
    let col_f = |j: usize| f_lo + (j as f64 + 0.5) * f_cell;
    let station_occ: Vec<(String, f64)> = truth_a
        .iter()
        .filter_map(|s| {
            let j = ((s.f_hz - f_lo) / f_cell).floor();
            (j >= 0.0 && (j as usize) < nf)
                .then(|| col_occ(j as usize).map(|o| (mhz(s.f_hz), o)))
                .flatten()
        })
        .collect();
    let mut quiet: Vec<f64> = (0..nf)
        .filter(|&j| {
            let f = col_f(j);
            let o = (f - ca).abs();
            (RUN_DC_HZ..=RUN_EDGE_HZ).contains(&o)
                && stations.iter().all(|s| (s.f_hz - f).abs() >= 250e3)
        })
        .filter_map(col_occ)
        .collect();
    if nt > 0 && !station_occ.is_empty() && !quiet.is_empty() {
        let quiet_med = percentile(&mut quiet, 0.5);
        let min_station = station_occ.iter().map(|(_, o)| *o).fold(f64::INFINITY, f64::min);
        res.add(
            "AWARE-042 occupancy (stations vs survey-empty)",
            min_station - quiet_med >= 0.25,
            format!(
                "{nt}x{nf} cells ({:.0} Hz, {} s); station occupancy {station_occ:?}; \
                 survey-empty median {quiet_med:.3} over {} columns",
                f_cell,
                hist["t_cell_s"],
                quiet.len()
            ),
        );
    } else {
        res.add(
            "AWARE-042 occupancy (stations vs survey-empty)",
            false,
            format!(
                "grid {nt}x{nf}, {} station columns, {} empty columns",
                station_occ.len(),
                quiet.len()
            ),
        );
    }

    // Retune within the band.
    let lc = live_control.expect("live control for the live radio");
    match cb {
        None => res.push("retune within FM band", "SKIP", "the survey found one window only"),
        Some(cb) => {
            let truth_b = in_window(&stations, cb, STRONG_SNR_DB);
            let t_retune = Instant::now();
            let tuned = lc.set_center(cb);
            let ok = tuned.as_ref().is_ok_and(|t| t.center_hz == cb);
            let rows = poll_inventory(addr, Duration::from_secs(60), |rows| {
                truth_b.iter().all(|s| found(rows, s))
            });
            let hits: Vec<(String, bool)> =
                truth_b.iter().map(|s| (mhz(s.f_hz), found(&rows, s))).collect();
            res.add(
                format!("retune {} -> {} MHz", mhz(ca), mhz(cb)),
                ok && hits.iter().all(|(_, h)| *h),
                format!(
                    "set_center {:?}; stations detected + FM top-k after {:.0} s: {hits:?}",
                    tuned.map(|t| t.center_hz).map_err(|e| e.to_string()),
                    t_retune.elapsed().as_secs_f64()
                ),
            );
        }
    }
    let status = get_json(addr, "/api/status");

    // Stop and account for drops.
    let wall = run_start.elapsed().as_secs_f64();
    handle.stop();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(handle.wait().map_err(|e| format!("{e:#}")));
    });
    let summary = rx
        .recv_timeout(Duration::from_secs(60))
        .expect("the live run stops")
        .expect("the live run ends without error");
    eprintln!("{}", summary.to_text());
    let stats = source_control.and_then(|c| c.stats());
    drop(server);
    eprintln!("[{TAG}] source {stats:?}; status source {}", status["source"]);
    let samples = summary.counter("/source/samples");
    let (dropped, overruns) = stats.map_or((0, 0), |s| (s.dropped_samples, s.overruns));
    res.add(
        "live run: samples, drops, errors",
        summary.errors.is_empty() && samples as f64 > 0.9 * wall * RUN_RATE_HZ,
        format!(
            "{samples} samples in {wall:.1} s ({:.3} of real time at 2.4 Msps), {overruns} overruns, \
             {dropped} samples dropped by the device, {} lost by always-on readers, errors {:?}",
            samples as f64 / (wall * RUN_RATE_HZ),
            summary.always_on_lost_samples,
            summary.errors
        ),
    );
    let _ = std::fs::remove_dir_all(&dir);

    eprintln!("[{TAG}] device {hw}; LNA {LNA_DB} / VGA {VGA_DB} / amp {AMP}");
    for (test, outcome, detail) in &res.0 {
        eprintln!("HIL-RESULT | {test} | {outcome} | {detail}");
    }
    let failed: Vec<_> = res.0.iter().filter(|r| r.1 == "FAIL").map(|r| &r.0).collect();
    assert!(failed.is_empty(), "[{TAG}] failed on the HackRF: {failed:?}");
}
