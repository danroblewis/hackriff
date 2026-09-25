//! T-558: what a sustained survey costs in threads and resident memory.
//!
//! A device-wide sweep confirms a great many tracks, and every confirmation that matches a decode
//! spec spawns a chain — a thread plus that chain's retain buffer. The question this file answers
//! is whether the number alive at once is **bounded**, or a function of how long the survey ran.
//!
//! The survey is driven through the **SDR device interface** (the mock source behind a paced
//! replay), never by feeding a file to the pipeline; the attaches are the ones a confirmed track
//! makes, requested through the same public `attach_chain` path.
//!
//! Measured on this machine before the T-558 cap, at 2 Msps: threads rose **one per attach** —
//! 43 -> 219 over 175 attaches — and RSS 64 -> 1210 MiB and still climbing. After: threads flat
//! at 59 and RSS flat at its steady state, whether the survey attaches 25 chains or 400. T-542
//! measured the live process under a real 1 MHz-6 GHz sweep at ~450 threads and multi-GB RSS.

mod common;

use std::time::{Duration, Instant};

use common::*;
use hk_core::Pacing;
use hk_pipeline::chains::MAX_RUNTIME_CHAINS;
use hk_pipeline::{Candidate, ChainShape, builtin_chains};
use serde_json::json;

/// Threads in this process (macOS/Linux, via `ps`).
fn threads() -> usize {
    let pid = std::process::id();
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("ps")
            .args(["-M", "-p", &pid.to_string()])
            .output()
            .expect("ps");
        // A header line, then one line per thread.
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .count()
            .saturating_sub(1)
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::read_dir(format!("/proc/{pid}/task"))
            .map(|d| d.count())
            .unwrap_or(0)
    }
}

/// Resident set size, MiB.
fn rss_mib() -> f64 {
    let pid = std::process::id();
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .unwrap_or(0.0)
        / 1024.0
}

/// The residency of a survey that keeps confirming tracks: `attaches` chain attaches, none
/// detached, sampled as it goes. Returns (attaches, threads, RSS MiB, running chains) rows.
fn survey(attaches: usize, label: &str) -> Vec<(usize, usize, f64, u64)> {
    let dir = TempDir::new(label);
    let rec = tone_recording(&dir.0.join("src"), "tone", 2e6, 3.0, 433.92e6, None);
    let (cfg, replay) = replay_config(
        &dir.0,
        &rec,
        json!({ "pipeline": { "ring_s": 8.0 } }),
        Pacing::RealTime { speed: 1.0 },
    );
    let center = replay.info.center_hz;
    let handle = start(cfg, replay);
    let spec = builtin_chains()
        .into_iter()
        .find(|s| s.id == "fsk-bursts")
        .unwrap();
    let base = threads();
    let mut rows = vec![(0, base, rss_mib(), 0)];
    println!("[{label}] baseline threads {base} rss {:.0} MiB", rows[0].2);
    for i in 1..=attaches {
        let at = handle.ring_position().saturating_sub(4096);
        // Each confirmation is its own region, as a sweep's would be.
        let f = center - 500e3 + 1e3 * (i % 900) as f64;
        handle.attach_chain(
            spec.clone(),
            Candidate {
                track: None,
                detection: None,
                f_lo_hz: f - 5e3,
                f_hi_hz: f + 5e3,
                first_sample: at,
                trigger_sample: at,
                bursty: Some(true),
            },
        );
        if i % 25 == 0 {
            // Let the control thread drain the attaches it was sent.
            std::thread::sleep(Duration::from_millis(400));
            let t = threads();
            let r = rss_mib();
            let running = handle.counters().chain_stats.len() as u64;
            println!("[{label}] attaches {i:4}  threads {t:4}  rss {r:7.0} MiB  running {running}");
            rows.push((i, t, r, running));
        }
    }
    std::thread::sleep(Duration::from_millis(500));
    let t = threads();
    let r = rss_mib();
    println!("[{label}] FINAL attaches {attaches} threads {t} rss {r:.0} MiB");
    rows.push((attaches, t, r, 0));
    handle.stop();
    let _ = handle.wait();
    rows
}

#[test]
#[ignore = "measurement, not an assertion"]
fn measure_chain_residency_over_a_sustained_survey() {
    let t0 = Instant::now();
    let rows = survey(400, "residency");
    let base = rows[0];
    let last = rows[rows.len() - 1];
    println!(
        "threads {} -> {} (+{}), rss {:.0} -> {:.0} MiB (+{:.0}) in {:.1} s",
        base.1,
        last.1,
        last.1 as i64 - base.1 as i64,
        base.2,
        last.2,
        last.2 - base.2,
        t0.elapsed().as_secs_f64()
    );
}

/// The bound, stated as a bound: sixteen times the attaches is the same residency.
///
/// Sampled at two survey lengths on purpose. One length can be passed by a cap, by a slow climb,
/// or by luck; only the comparison shows the count is not a function of how long the survey ran.
/// Before the cap this read `threads 43 -> 68` at 25 attaches and `43 -> 219` at 400, and the
/// running-chain count was the attach count exactly.
/// The built-in classifier's own concurrency cap (T-878), from its spec rather than a literal.
fn classify_cap() -> usize {
    builtin_chains()
        .iter()
        .find_map(|s| match s.shape() {
            Ok(ChainShape::Classify { max_chains, .. }) => Some(max_chains),
            _ => None,
        })
        .expect("the classifier ships in the built-in registry")
}

/// `(classifying, everything else)` among the running chains in `chain_stats`. A chain's `kind`
/// is its spec id; anything that is not a built-in classify spec counts against the run-wide cap,
/// so no other kind can escape it unnoticed.
fn running(chain_stats: &serde_json::Value) -> (usize, usize) {
    let classify: Vec<String> = builtin_chains()
        .into_iter()
        .filter(|s| matches!(s.shape(), Ok(ChainShape::Classify { .. })))
        .map(|s| s.id)
        .collect();
    let list = chain_stats.as_array().expect("chain_stats is a list");
    let classifying = list
        .iter()
        .filter(|c| {
            c["kind"]
                .as_str()
                .is_some_and(|k| classify.iter().any(|id| id == k))
        })
        .count();
    (classifying, list.len() - classifying)
}

#[test]
fn a_sustained_survey_holds_a_bounded_number_of_chain_threads() {
    let dir = TempDir::new("t558-bound");
    let rec = tone_recording(&dir.0.join("src"), "tone", 2e6, 30.0, 433.92e6, None);
    let (cfg, replay) = replay_config(
        &dir.0,
        &rec,
        json!({ "pipeline": { "ring_s": 8.0 } }),
        Pacing::RealTime { speed: 1.0 },
    );
    let center = replay.info.center_hz;
    let handle = start(cfg, replay);
    let spec = builtin_chains()
        .into_iter()
        .find(|s| s.id == "fsk-bursts")
        .unwrap();
    let base = threads();

    let mut at_25 = (0usize, 0usize, 0usize);
    let mut at_400 = (0usize, 0usize, 0usize);
    for i in 1..=400usize {
        let at = handle.ring_position().saturating_sub(4096);
        let f = center - 500e3 + 1e3 * (i % 900) as f64;
        handle.attach_chain(
            spec.clone(),
            Candidate {
                track: None,
                detection: None,
                f_lo_hz: f - 5e3,
                f_hi_hz: f + 5e3,
                first_sample: at,
                trigger_sample: at,
                bursty: Some(true),
            },
        );
        if i == 25 || i == 400 {
            // Let the control thread drain the attaches it was sent.
            std::thread::sleep(Duration::from_millis(600));
            let (classifying, counted) = running(&handle.counters().chain_stats.to_json());
            let row = (threads(), counted, classifying);
            println!(
                "attaches {i}: threads {} running {} under the run-wide cap + {} classifying",
                row.0, row.1, row.2
            );
            if i == 25 { at_25 = row } else { at_400 = row }
        }
    }

    // Two bounds since T-878: every chain but the classifier's is under the run-wide cap, and the
    // classifier's chains (which a track attaches *before* its decode chain, so they must never
    // take a decode chain's slot) are under their own spec's `max_chains` instead.
    let cap = MAX_RUNTIME_CHAINS;
    let classify_cap = classify_cap();
    assert!(
        at_400.1 <= cap,
        "400 attaches left {} chains (classifying chains excluded) running, above the {cap} cap",
        at_400.1
    );
    assert!(
        at_400.2 <= classify_cap,
        "400 attaches left {} classifying chains running, above their {classify_cap} cap",
        at_400.2
    );
    let cap = cap + classify_cap;
    assert!(
        at_400.0 <= base + 2 * cap + 8,
        "400 attaches took the process from {base} threads to {}: the thread count is a \
         function of how long the survey ran, not of the cap",
        at_400.0
    );
    assert!(
        at_400.0 <= at_25.0 + 8,
        "sixteen times the attaches added {} threads ({} -> {}): the residency is not bounded",
        at_400.0 as i64 - at_25.0 as i64,
        at_25.0,
        at_400.0
    );
    handle.stop();
    let _ = handle.wait();
}
