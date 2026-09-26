//! T-971 (SIGNAL-062): **continuous RDS accumulation on the station's own row** — PS, RadioText
//! and PTY accumulate as the station keeps transmitting, instead of one ~4 s window committing a
//! PI and stopping.
//!
//! Measured on live air (explorer window 1, 2026-09-25, `journal-20260925.md`): the unprompted
//! `hk-rds` chain decoded one window, committed PI 1694 on 101.3 MHz and stopped — `ps: null,
//! ps_frames: []` after a three-minute dwell, PS unprompted on 0 of 19 stations, RadioText never.
//!
//! **Blind, through the mock SDR.** The explorer's 101.3 MHz capture (`fixtures/hackrf/
//! explorer-2026-09-25/fm-101p3-pi1694`, 2.4 Msps, 5 s) has its truth stripped before the device
//! sees it, and the device serves it in a live HackRF's 65 536-sample transfers, **looped** so the
//! station stays on air for several windows' worth (the mock marks each splice as a gap, as a
//! live stream's drop would be). Nothing is selected or prompted: the built-in registry, blind
//! detection and the `wfm-rds` chain decide everything. The hidden truth — PI, PS, PTY and
//! RadioText as the independent oracle (`py/fixtures/rds_ref.py`) read them from the same IQ — is
//! read only here, after the run.
//!
//! **What "on the station's own row" means.** The row `GET /api/inventory/{id}/decode` serves for
//! the station: its emitter's decoded identity's `hk-rds` / `rds-pi` decodes, latest first — so
//! the assertions read `Repository::decodes_for_identity` exactly as that route does.
//!
//! **Red on the old code** (a window, then stop): its only `rds-pi` row covers one window, has no
//! `follow` span, no RadioText, and at most the ~46 groups a 4 s window holds — each of which the
//! first test refuses.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::*;
use hk_core::{MockEnd, Pacing};
use hk_model::{
    AnnotationTarget, DecodedIdentity, IdentityScheme, InventoryIdentity, InventoryQuery,
};
use hk_pipeline::{
    Counters, Pipeline, PipelineConfig, TrackInventory, open_mock_replay_with_block_len,
    replay_plan,
};
use serde_json::Value;

const TAG: &str = "T-971/SIGNAL-062";
const SET_DIR: &str = "fixtures/hackrf/explorer-2026-09-25";
const NAME: &str = "fm-101p3-pi1694";
/// A live HackRF's transfer, samples (131 072 bytes of ci8).
const HACKRF_TRANSFER_SAMPLES: usize = 65_536;
/// Passes of the 5 s capture the station stays on air for.
const PASSES: u64 = 5;
/// Wall-clock guard for one run.
const LIMIT: Duration = Duration::from_secs(600);

/// An explorer capture whose LFS data is present, `None` (skip) otherwise.
fn explorer_fixture(name: &str) -> Option<PathBuf> {
    let rel = format!("{SET_DIR}/{name}");
    let mut dir = Some(hk_e2e::paths::repo_root());
    while let Some(d) = dir {
        let meta = d.join(format!("{rel}.sigmf-meta"));
        let data = d.join(format!("{rel}.sigmf-data"));
        if meta.is_file() && std::fs::metadata(&data).is_ok_and(|m| m.len() > 1 << 20) {
            return Some(meta);
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    if std::env::var("HK_REQUIRE_FIXTURES").is_ok_and(|v| v == "1") {
        panic!("{name}: fixture data not fetched (git lfs pull)");
    }
    eprintln!("SKIP {name}: fixture data is not fetched (git lfs pull)");
    None
}

/// The station's hidden truth (the oracle's reading of the capture).
#[derive(Debug)]
struct Truth {
    center_hz: f64,
    pi: String,
    ps: String,
    pty: u64,
    rt: String,
    rt_ab: bool,
    groups_decoded: u64,
}

fn truth(meta: &Path) -> Truth {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(meta).unwrap()).unwrap();
    let t = v["annotations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| &a["hackriff:truth"])
        .find(|t| t["kind"] == "wfm-broadcast+rds")
        .expect("an RDS station in the truth");
    let rds = &t["rds"];
    let last_rt = rds["rt_messages"]
        .as_array()
        .and_then(|m| m.last())
        .expect("the oracle read a RadioText message on this capture (py/fixtures/rds_ref.py)");
    Truth {
        center_hz: t["center_hz"].as_f64().unwrap(),
        pi: rds["pi_hex"].as_str().unwrap().to_ascii_uppercase(),
        ps: rds["ps"].as_str().unwrap().to_owned(),
        pty: rds["pty"].as_u64().unwrap(),
        rt: last_rt["text"].as_str().unwrap().to_owned(),
        rt_ab: last_rt["ab"].as_bool().unwrap(),
        groups_decoded: rds["groups_decoded"].as_u64().unwrap(),
    }
}

/// A truth-stripped copy of `meta` in `dir` whose data is the capture's own samples followed by
/// `tail` (ci8 bytes): what the device serves, and all it serves.
fn blind_recording(meta: &Path, dir: &Path, tail: &[u8]) -> PathBuf {
    let mut v: Value = serde_json::from_str(&std::fs::read_to_string(meta).unwrap()).unwrap();
    v["annotations"] = Value::Array(Vec::new());
    if let Some(g) = v["global"].as_object_mut() {
        g.remove("core:description");
    }
    let out = dir.join("blind.sigmf-meta");
    std::fs::write(&out, serde_json::to_vec_pretty(&v).unwrap()).unwrap();
    let mut data = std::fs::read(meta.with_extension("sigmf-data")).unwrap();
    data.extend_from_slice(tail);
    std::fs::write(dir.join("blind.sigmf-data"), data).unwrap();
    out
}

/// Runs `blind` behind the mock SDR (HackRF-sized transfers, lossless) until the device has
/// served `samples`, or the stream ends; returns the run's counters and data directory.
fn run(blind: &Path, end: MockEnd, samples: u64, tag: &str) -> (std::sync::Arc<Counters>, TempDir) {
    let dir = TempDir::new(tag);
    let replay =
        open_mock_replay_with_block_len(blind, Pacing::Unpaced, end, Some(HACKRF_TRANSFER_SAMPLES))
            .unwrap();
    let info = replay.info;
    let mut cfg = PipelineConfig::new(
        &dir.0,
        replay_plan(info.center_hz, info.sample_rate_hz, info.start_time),
    )
    .unwrap();
    cfg.source_class = replay.class;
    cfg.lossless = true;
    let handle = Pipeline::start(
        cfg,
        Box::new(replay.source),
        info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let counters = handle.counters();
    let stopper = handle.stopper();
    let c = std::sync::Arc::clone(&counters);
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let finished = std::sync::Arc::clone(&done);
    let started = Instant::now();
    let watch = std::thread::spawn(move || {
        while c.source.samples.load(Ordering::Relaxed) < samples
            && !finished.load(Ordering::Relaxed)
        {
            if started.elapsed() > LIMIT {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        stopper.stop();
    });
    let (summary, fired) = wait_guarded(handle, LIMIT + Duration::from_secs(60));
    done.store(true, Ordering::Relaxed);
    watch.join().unwrap();
    assert!(!fired, "[{TAG}] the run did not finish");
    assert!(
        started.elapsed() < LIMIT,
        "[{TAG}] the device did not serve {samples} samples within {LIMIT:?}"
    );
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    (counters, dir)
}

fn get(a: &std::sync::atomic::AtomicU64) -> u64 {
    a.load(Ordering::Relaxed)
}

/// Prints what the follows cost: time per followed sample, and the cores one station takes at
/// the capture's rate (a measurement, not a bound: bounds on time live in the timing tier).
fn report_cost(c: &Counters, fs: f64) {
    let (n, ns) = (
        get(&c.chains.rds_follow_samples),
        get(&c.chains.rds_follow_ns),
    );
    if n > 0 {
        let per = ns as f64 / n as f64;
        eprintln!(
            "[{TAG}] follow cost: {n} samples in {:.2} s = {per:.1} ns/sample = {:.3} cores per \
             station at {:.1} Msps",
            ns as f64 / 1e9,
            per * fs / 1e9,
            fs / 1e6
        );
    }
}

/// The capture's sample count and rate.
fn capture_len(meta: &Path) -> (u64, f64) {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(meta).unwrap()).unwrap();
    let fs = v["global"]["core:sample_rate"].as_f64().unwrap();
    let bytes = std::fs::metadata(meta.with_extension("sigmf-data"))
        .unwrap()
        .len();
    (bytes / 2, fs)
}

#[test]
fn t971_a_followed_station_accumulates_ps_rt_pty_on_its_own_row() {
    let Some(meta) = explorer_fixture(NAME) else {
        return;
    };
    let truth = truth(&meta);
    let (n, fs) = capture_len(&meta);
    let input = TempDir::new("t971-input");
    let blind = blind_recording(&meta, &input.0, &[]);
    let (c, dir) = run(&blind, MockEnd::Loop, PASSES * n, "t971-accumulate");
    report_cost(&c, fs);
    let chains = &c.chains;
    eprintln!(
        "[{TAG}] follows {} (refused {}), rows {}, ended silent/budget/stream {}/{}/{}",
        get(&chains.rds_follows),
        get(&chains.rds_follow_refused),
        get(&chains.rds_follow_rows),
        get(&chains.rds_follow_ended_silent),
        get(&chains.rds_follow_ended_budget),
        get(&chains.rds_follow_ended_stream),
    );

    // The station's own row: its inventory entry, identified by the PI it decoded.
    let repo = repo(&dir.0);
    let identity = DecodedIdentity {
        scheme: IdentityScheme::RdsPi,
        value: truth.pi.clone(),
    };
    let entries = inventory(&repo, InventoryQuery::default());
    let station = entries
        .iter()
        .find(|e| {
            (e.emitter.f_center_hz - truth.center_hz).abs() < 100e3
                && matches!(&e.identity, InventoryIdentity::Clear { identity: i, .. } if *i == identity)
        })
        .unwrap_or_else(|| {
            panic!(
                "[{TAG}] no inventory entry at {:.3} MHz carries PI {}: {entries:#?}",
                truth.center_hz / 1e6,
                truth.pi
            )
        });
    // As `GET /api/inventory/{id}/decode` reads it: the identity's decodes, latest per
    // (decoder, frame_model).
    let rows: Vec<_> = repo
        .decodes_for_identity(&identity)
        .unwrap()
        .into_iter()
        .filter(|d| d.decoder_id == "hk-rds" && d.frame_model == "rds-pi")
        .collect();
    let row = rows.last().expect("an rds-pi row");
    let m = &row.metadata;
    eprintln!(
        "[{TAG}] {} rds-pi rows; the station's row now: {m:#}",
        rows.len()
    );

    // Accumulated, not a snapshot: the row covers far more than one window of the station.
    let follow = &m["follow"];
    let span =
        follow["span_end"].as_f64().unwrap_or(0.0) - follow["span_start"].as_f64().unwrap_or(0.0);
    assert!(
        span >= 12.0,
        "[{TAG}] the station's row covers {span:.1} s of it ({follow}): one window (~4 s) was \
         decoded and the chain stopped, as on live air on 2026-09-25"
    );
    let groups_ok = m["groups_ok"].as_u64().unwrap();
    assert!(
        groups_ok > truth.groups_decoded,
        "[{TAG}] {groups_ok} CRC-valid groups on the row: no more than one 5 s pass of the \
         station holds ({}), so nothing accumulated past the window",
        truth.groups_decoded
    );

    // The fields, against the oracle's hidden reading of the same IQ.
    assert_eq!(m["pi"], truth.pi.as_str(), "[{TAG}] PI");
    assert_eq!(m["identity_provisional"], false, "[{TAG}] {m}");
    assert_eq!(m["pty"].as_u64(), Some(truth.pty), "[{TAG}] PTY");
    let ps_frames: Vec<(String, u64)> = serde_json::from_value(m["ps_frames"].clone()).unwrap();
    let truth_ps = ps_frames.iter().find(|(t, _)| *t == truth.ps);
    assert!(
        truth_ps.is_some_and(|(_, n)| *n > 2),
        "[{TAG}] PS {:?} must accumulate beyond the capture's own 2 frames: {ps_frames:?}",
        truth.ps
    );
    assert!(
        m["ps_sequence"]
            .as_array()
            .is_some_and(|s| s.iter().any(|p| p == truth.ps.as_str())),
        "[{TAG}] the dynamic PS's sequence: {}",
        m["ps_sequence"]
    );
    assert_eq!(
        m["rt"].as_str(),
        Some(truth.rt.as_str()),
        "[{TAG}] RadioText: {}",
        m["rt_messages"]
    );
    assert_eq!(
        m["rt_ab"].as_bool(),
        Some(truth.rt_ab),
        "[{TAG}] RT A/B flag"
    );

    // One row per change, not a packet stream: rows are few beside the groups they summarise.
    let followed = rows
        .iter()
        .filter(|d| d.metadata.get("follow").is_some())
        .count();
    assert!(
        followed >= 1 && (followed as u64) < groups_ok / 10,
        "[{TAG}] {followed} follow rows for {groups_ok} groups"
    );
    assert!(get(&chains.rds_follows) >= 1);
    assert!(
        get(&chains.rds_follows_active) == 0,
        "[{TAG}] a follow slot leaked"
    );

    // The label the station's row names is its PS.
    let labels = repo
        .annotations_for(&AnnotationTarget::Emitter(station.emitter.id))
        .unwrap();
    assert!(
        labels.iter().any(|a| a.value == truth.ps.trim()),
        "[{TAG}] station label: {:?}",
        labels.iter().map(|a| &a.value).collect::<Vec<_>>()
    );
}

/// T-971: a follow **stops when the emitter ends**. The station transmits for the capture's 5 s
/// and then the band goes quiet while the device keeps streaming: the follow must end on its RDS
/// falling silent, not at its budget, and not only because the stream stopped.
#[test]
fn t971_a_follow_ends_when_the_station_goes_silent() {
    let Some(meta) = explorer_fixture(NAME) else {
        return;
    };
    let (n, fs) = capture_len(&meta);
    // 15 s of near-silence (±1 LSB dither) after the station.
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let quiet: Vec<u8> = (0..(15.0 * fs) as usize * 2)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            if state >> 63 == 1 { 1u8 } else { 0xFF }
        })
        .collect();
    let input = TempDir::new("t971-silent-input");
    let blind = blind_recording(&meta, &input.0, &quiet);
    let (c, _dir) = run(&blind, MockEnd::Stop, u64::MAX, "t971-silent");
    let chains = &c.chains;
    let total = n + quiet.len() as u64 / 2;
    assert_eq!(
        get(&c.source.samples),
        total,
        "[{TAG}] the device served the whole recording"
    );
    assert!(
        get(&chains.rds_follows) >= 1,
        "[{TAG}] the station was followed"
    );
    assert_eq!(
        get(&chains.rds_follow_ended_silent),
        get(&chains.rds_follows),
        "[{TAG}] every follow ended on its station's RDS going silent (budget {}, stream {})",
        get(&chains.rds_follow_ended_budget),
        get(&chains.rds_follow_ended_stream)
    );
    assert_eq!(
        get(&chains.rds_follows_active),
        0,
        "[{TAG}] a follow slot leaked"
    );
}
