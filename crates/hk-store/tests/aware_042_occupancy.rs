//! AWARE-042 (duty-cycle and occupancy statistics) through the spectrum-history pyramid.
//!
//! Truth: the `occupancy_multi_hour` synthetic scenario's seeded burst schedule (`schedule.json`,
//! 3 h, 8 NBFM channels at 12.5 kHz spacing, exact per-channel occupancy, burst durations and
//! UTC hour-of-day occupancy).
//!
//! **Frame source (stated per the task):** frames are synthesised **directly from the schedule**,
//! not rendered through IQ, because 3 h of IQ is ~4 GB. Frame model, chosen to match what the STFT
//! (hk-dsp) would emit for the scenario: 10 frames/s, each 100 ms long, 64 bins of 3125 Hz over the
//! scenario's 200 kHz span; per-bin PSD = noise density (−40 dBFS over 200 kHz) × a mean-1
//! gamma(16) variate (a 16-average periodogram) + each burst's density (its power spread over the
//! 7 kHz NBFM bandwidth, overlap-weighted per bin) × the fraction of the frame the burst is on.
//! The default floor (no caller floor) sets the occupancy threshold.

use std::collections::BTreeMap;
use std::path::PathBuf;

use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::{FreqRange, PowerUnit, TimeRange, Timestamp};
use hk_store::history::burst_histogram;
use hk_store::{FrameInput, Pyramid, PyramidConfig, RegionQuery, Resolution};
use serde_json::Value;

const AWARE_042: &str = "AWARE-042";
const S: i64 = 1_000_000_000;
const FPS: i64 = 10;
const NBINS: usize = 64;
const SPAN_HZ: f64 = 200e3;
const NOISE_DBFS: f64 = -40.0;
const NBFM_BW_HZ: f64 = 7000.0;

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Rng(u64);

impl Rng {
    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
    fn gamma(&mut self, k: u32) -> f64 {
        (0..k).map(|_| -self.unit().max(1e-12).ln()).sum::<f64>() / f64::from(k)
    }
}

/// Seconds since the epoch of `YYYY-MM-DDTHH:MM:SSZ`.
fn parse_utc(s: &str) -> i64 {
    let n = |r: std::ops::Range<usize>| s[r].parse::<i64>().unwrap();
    let (y, m, d) = (n(0..4), n(5..7), n(8..10));
    let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86400 + n(11..13) * 3600 + n(14..16) * 60 + n(17..19)
}

fn f(v: &Value, k: &str) -> f64 {
    v[k].as_f64()
        .unwrap_or_else(|| panic!("{AWARE_042}: missing {k}"))
}

fn within(what: &str, got: f64, want: f64, abs: f64, rel: f64, worst: &mut BTreeMap<String, f64>) {
    let err = (got - want).abs();
    let tol = abs.max(rel * want.abs());
    let e = worst.entry(what.to_owned()).or_insert(0.0);
    *e = e.max(err);
    assert!(
        err <= tol,
        "{AWARE_042}: {what}: got {got:.5}, truth {want:.5}, |err| {err:.5} > tol {tol:.5}"
    );
}

fn quantile(v: &mut [f64], q: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    let pos = q * (v.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    v[lo] + (v[hi] - v[lo]) * (pos - lo as f64)
}

#[test]
fn aware_042_occupancy_duty_cycle_and_hour_profile_from_history() {
    let out = synth_or_skip!(
        SynthRequest::new("occupancy_multi_hour")
            .seed(11)
            .param("windows", 1)
            .param("window_duration_s", 0.02)
    );
    let schedule = out.file_json("schedule.json").unwrap();
    let span_s = f(&schedule, "span_s");
    let start_s = parse_utc(schedule["start_utc"].as_str().unwrap());
    let channels = schedule["channels"].as_array().unwrap();
    let n_ch = channels.len();
    let center = 446.05e6;
    let f_lo = center - SPAN_HZ / 2.0;
    let bw = SPAN_HZ / NBINS as f64;
    let n0 = 10f64.powf(NOISE_DBFS / 10.0) / SPAN_HZ;

    // Bursts per channel, time-ordered.
    let mut bursts: Vec<Vec<(f64, f64)>> = vec![Vec::new(); n_ch];
    for b in schedule["bursts"].as_array().unwrap() {
        let c = b["channel"].as_u64().unwrap() as usize;
        let s0 = f(b, "start_s");
        bursts[c].push((s0, s0 + f(b, "duration_s")));
    }
    for v in &mut bursts {
        v.sort_by(|a, b| a.0.total_cmp(&b.0));
    }
    // Per-channel bin weights: density contribution of a fully-on burst to each bin.
    let weights: Vec<Vec<(usize, f64)>> = channels
        .iter()
        .map(|ch| {
            let fc = f(ch, "center_hz");
            let density = 10f64.powf(f(ch, "power_dbfs") / 10.0) / NBFM_BW_HZ;
            let (a, b) = (fc - NBFM_BW_HZ / 2.0, fc + NBFM_BW_HZ / 2.0);
            (0..NBINS)
                .filter_map(|i| {
                    let lo = f_lo + i as f64 * bw;
                    let ov = (b.min(lo + bw) - a.max(lo)).max(0.0);
                    (ov > 0.0).then_some((i, density * ov / bw))
                })
                .collect()
        })
        .collect();

    let dir =
        TempDir(std::env::temp_dir().join(format!("hk-store-aware042-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&dir.0);
    let mut p = Pyramid::open(&dir.0, PyramidConfig::default()).unwrap();
    let mut rng = Rng(0x0042_0042);
    let mut ptr = vec![0usize; n_ch];
    let mut psd = vec![0f32; NBINS];
    let n_frames = (span_s as i64) * FPS;
    for k in 0..n_frames {
        let a = k as f64 / FPS as f64;
        let b = a + 1.0 / FPS as f64;
        let mut row: Vec<f64> = (0..NBINS).map(|_| n0 * rng.gamma(16)).collect();
        for c in 0..n_ch {
            while ptr[c] < bursts[c].len() && bursts[c][ptr[c]].1 <= a {
                ptr[c] += 1;
            }
            let mut on = 0.0;
            for &(s0, s1) in bursts[c][ptr[c]..].iter().take_while(|x| x.0 < b) {
                on += (s1.min(b) - s0.max(a)).max(0.0);
            }
            if on > 0.0 {
                let frac = on * FPS as f64;
                for &(i, w) in &weights[c] {
                    row[i] += w * frac;
                }
            }
        }
        for (d, v) in psd.iter_mut().zip(&row) {
            *d = *v as f32;
        }
        let t = start_s * S + k * S / FPS;
        p.ingest(&FrameInput::new(
            Timestamp::from_unix_nanos(t),
            S / FPS,
            f_lo,
            bw,
            PowerUnit::Dbfs,
            &psd,
        ))
        .unwrap();
    }
    let end = start_s * S + (span_s as i64) * S;
    p.seal_through(Timestamp::from_unix_nanos(end)).unwrap();

    let region = |resolution| RegionQuery {
        freq: FreqRange::new(446.0e6, 446.1e6),
        time: TimeRange::new(
            Timestamp::from_unix_nanos(start_s * S),
            Timestamp::from_unix_nanos(end),
        ),
        resolution,
    };
    let chans: Vec<FreqRange> = channels
        .iter()
        .map(|ch| FreqRange::centered(f(ch, "center_hz"), 12_500.0))
        .collect();
    let h0 = p.query(&region(Resolution::Level(0))).unwrap();
    assert_eq!((h0.level, h0.nt, h0.nf), (0, span_s as usize, 16));
    assert!(h0.cells.iter().all(|c| c.observed() && c.level == 0));
    let s0 = h0.channel_summaries(&chans);
    // Coarse level: 12.5 kHz × 1 min cells, one per channel.
    let q1 = region(Resolution::Cell {
        t_ns: 60 * S,
        f_hz: 12_500.0,
    });
    assert_eq!(p.choose_level(&q1), 1);
    let h1 = p.query(&q1).unwrap();
    let s1 = h1.channel_summaries(&chans);

    let stats = &schedule["stats"]["per_channel"];
    let mut worst = BTreeMap::new();
    let mut all_est = Vec::new();
    let mut all_truth = Vec::new();
    for (c, (est, coarse)) in s0.iter().zip(&s1).enumerate() {
        let truth = &stats[c];
        assert_eq!(est.columns.len(), 2, "two 6.25 kHz cells per channel");
        assert_eq!(coarse.columns.len(), 1);
        let occ = f(truth, "occupancy_fraction");
        within(
            "occupancy L0",
            est.occupancy_fraction.unwrap(),
            occ,
            0.004,
            0.08,
            &mut worst,
        );
        within(
            "occupancy L1",
            coarse.occupancy_fraction.unwrap(),
            occ,
            0.004,
            0.08,
            &mut worst,
        );
        assert_eq!(
            coarse.peak_occupancy, est.peak_occupancy,
            "peak occupancy survives rollup"
        );
        for (hod, v) in truth["utc_hour_of_day"].as_object().unwrap() {
            let h: usize = hod.parse().unwrap();
            let want = f(v, "occupancy");
            within(
                "hour-of-day L0",
                est.hour_of_day[h].unwrap(),
                want,
                0.005,
                0.10,
                &mut worst,
            );
            within(
                "hour-of-day L1",
                coarse.hour_of_day[h].unwrap(),
                want,
                0.005,
                0.10,
                &mut worst,
            );
        }
        let n = f(truth, "n_bursts");
        within(
            "burst count",
            est.bursts_s.len() as f64,
            n,
            3.0,
            0.10,
            &mut worst,
        );
        let mut d = est.bursts_s.clone();
        for q in ["p10", "p50", "p90"] {
            let want = f(&truth["duration_quantiles_s"], q);
            let got = quantile(&mut d, q[1..].parse::<f64>().unwrap() / 100.0);
            within(
                &format!("duration {q} (s)"),
                got,
                want,
                0.25,
                0.15,
                &mut worst,
            );
        }
        all_est.extend(est.bursts_s.iter().copied());
        all_truth.extend(bursts[c].iter().map(|(a, b)| b - a));
    }
    // Pooled burst-duration histogram against the schedule's bins.
    let edges: Vec<f64> = schedule["stats"]["duration_histogram"]["edges_s"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e.as_f64().unwrap_or(f64::INFINITY))
        .collect();
    let truth_counts: Vec<f64> = schedule["stats"]["duration_histogram"]["counts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_f64().unwrap())
        .collect();
    assert_eq!(
        burst_histogram(&all_truth, &edges)
            .iter()
            .map(|&c| c as f64)
            .collect::<Vec<_>>(),
        truth_counts
    );
    let est_counts = burst_histogram(&all_est, &edges);
    let (nt, ne) = (all_truth.len() as f64, all_est.len() as f64);
    for (i, (&e, &t)) in est_counts.iter().zip(&truth_counts).enumerate() {
        within(
            &format!("duration histogram fraction bin {i}"),
            e as f64 / ne,
            t / nt,
            0.05,
            0.0,
            &mut worst,
        );
    }
    eprintln!(
        "{AWARE_042} worst absolute errors over {n_ch} channels ({} bursts):",
        nt
    );
    for (k, v) in &worst {
        eprintln!("  {k}: {v:.4}");
    }
}
