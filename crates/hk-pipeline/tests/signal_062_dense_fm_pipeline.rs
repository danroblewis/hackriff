//! T-099 (SIGNAL-062) through the composed pipeline and a scripted radio: a dense FM scene, the
//! target station with equal-power neighbours 400 kHz either side (each the `fm_broadcast_rds`
//! synthesiser with its own PI), summed and quantised as the capture thread does. Before T-099
//! the neighbours made mode selection abstain on bandwidth and the `wfm-rds` chain stopped at the
//! probe (`mode_rejected`); now the chain attaches, selects WFM and decodes the target's PI.
//!
//! T-402 widened `fm_broadcast_rds` from an unregulated ~100 kHz OBW99 to a realistic ~190-220 kHz
//! (a regulated ~75 kHz peak deviation, matching what a station's limiter actually holds), which
//! made the original 200 kHz spacing narrower than the stations themselves: `t129` measured one
//! merged occupancy channel across all three instead of three, and the target's own PI stopped
//! decoding cleanly between its now-much-closer neighbours (`crates/hk-demod/tests/
//! signal_062_dense_fm.rs` hit the same overlap and needed the same kind of widening). Unlike that
//! test, this one runs blind detection/tracking/confirmation over time rather than a hand-specified
//! box, and needed more margin before decoding cleanly again; 400 kHz spacing plus `FS` doubled to
//! 2.4 Msps (so `t129`'s `0.8 * FS` observed band still covers all three stations, including the
//! upper neighbour at +500 kHz) is the smallest combination found that passes both tests.
//!
//! Blind: the pipeline sees only the radio's IQ; the PIs are the scene's private truth, compared
//! against the stored decodes after the run.

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::*;
use hk_core::{Pacing, ReplayOptions, SigmfReplaySource, Source};
use hk_e2e::SynthRequest;
use hk_model::attention::occupancy::OccupancySubject;
use hk_model::{ContentClass, FreqRange, TimeRange, Timestamp};
use hk_pipeline::class::window_class;
use hk_pipeline::{
    NodeSpec, Pipeline, PipelineConfig, SourceInfo, TrackInventory, builtin_chains, replay_plan,
};
use num_complex::Complex;

const SIGNAL_062: &str = "SIGNAL-062";
const FS: f64 = 2.4e6;
/// Capture centre; the stations sit at −300, +100 and +500 kHz from it.
const CENTER_HZ: f64 = 99.4e6;
const SPACING_HZ: f64 = 400e3;

/// Station truth `(center_hz, bandwidth_hz)`.
type Truth = Vec<(f64, f64)>;

/// The dense scene quantised as the capture thread does, and its private station truth
/// `(center_hz, bandwidth_hz)` from the synthesiser's annotations.
fn scene_iq(scene: &[(f64, &str, u64)], duration_s: f64) -> Option<(Vec<Complex<i8>>, Truth)> {
    let mut sum: Vec<Complex<f32>> = Vec::new();
    let mut truth = Vec::new();
    for &(offset, pi, seed) in scene {
        let (iq, t) = station(offset, pi, seed, duration_s)?;
        truth.extend(t);
        if sum.is_empty() {
            sum = iq;
        } else {
            sum.iter_mut().zip(&iq).for_each(|(a, b)| *a += *b);
        }
    }
    let q = |x: f32| (x * 128.0).round().clamp(-128.0, 127.0) as i8;
    Some((
        sum.iter().map(|z| Complex::new(q(z.re), q(z.im))).collect(),
        truth,
    ))
}

/// One station's IQ (the synthesiser's float output, quantised later with the others) and its
/// emission truth.
fn station(
    offset_hz: f64,
    pi: &str,
    seed: u64,
    duration_s: f64,
) -> Option<(Vec<Complex<f32>>, Truth)> {
    let req = SynthRequest::new("fm_broadcast_rds")
        .seed(seed)
        .param("sample_rate", FS)
        .param("center_hz", CENTER_HZ)
        .param("offset_hz", offset_hz)
        .param("duration_s", duration_s)
        .param("pi_hex", pi)
        .param("noise_dbfs", -60.0);
    let out = match req.generate() {
        Ok(out) => out,
        Err(e) if e.is_unavailable() && !hk_e2e::synth::require_synth() => {
            eprintln!("SKIP {}: {e}", module_path!());
            return None;
        }
        Err(e) => panic!("synthetic scenario generation failed: {e}"),
    };
    let fx = out.fixture(0).unwrap();
    let mut src = SigmfReplaySource::open(
        &fx.meta_path,
        ReplayOptions {
            block_len: 65_536,
            pacing: Pacing::Unpaced,
        },
    )
    .unwrap();
    let mut iq = Vec::new();
    while let Some(b) = src.next_block().unwrap() {
        iq.extend_from_slice(&b.samples);
    }
    Some((iq, emission_truth(&fx.meta_path)))
}

/// Scene time before the `wfm-rds` window can open: detection, confirmation and the chain's
/// `pre_s`/probe. Measured, the window opened well inside the first 2 s.
const SCENE_LEAD_S: f64 = 2.0;

/// The shipped `wfm-rds` chain's `pre_s + window_s`: how much contiguous scene one window reads.
fn wfm_rds_window_s() -> f64 {
    builtin_chains()
        .into_iter()
        .filter(|c| c.id == "wfm-rds")
        .flat_map(|c| c.nodes)
        .find_map(|n| match n {
            NodeSpec::AnalogAuto {
                pre_s, window_s, ..
            } => Some(pre_s + window_s),
            _ => None,
        })
        .expect("the shipped wfm-rds chain has an analog-auto node")
}

fn wait(what: &str, limit: Duration, f: impl Fn() -> bool) {
    let deadline = Instant::now() + limit;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn signal_062_dense_fm_wfm_chain_attaches_and_decodes_the_target_pi() {
    // (offset from the capture centre, PI): the target, then its neighbours.
    let scene = [
        (100e3, "C0DE", 9_901),
        (100e3 - SPACING_HZ, "1A2B", 9_902),
        (100e3 + SPACING_HZ, "3C4D", 9_903),
    ];
    // The scene is read ONCE, never looped: the radio's loop point splices the end of the scene
    // onto its start, a phase and RDS-bitstream discontinuity no real station has. Before T-926
    // the chain's window was cut to the probe (~0.5 s) and never reached it; a full `window_s`
    // (4 s) window over the old 2 s scene looped three times always did, and its RDS decoder
    // counted the splice as errored blocks — 2 of 29 for every station, identically, 0.069 —
    // while the same scene 8 s long and read once decoded 43 groups with none. The block-error
    // assertion below is about the pipeline, so the scene must be clean for the whole window.
    let scene_s = wfm_rds_window_s() + SCENE_LEAD_S;
    let Some((iq, _)) = scene_iq(&scene, scene_s) else {
        return;
    };
    let total = iq.len() as u64;
    assert_eq!(window_class(CENTER_HZ, FS), ContentClass::Unrestricted);

    let dir = TempDir::new("t099-dense-fm");
    let (radio, ctl) = radio::Radio::new(CENTER_HZ, FS, 16_384, radio::looped(iq));
    let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
    let mut cfg = PipelineConfig::new(&dir.0, replay_plan(CENTER_HZ, FS, t0)).unwrap();
    cfg.source_class = window_class(CENTER_HZ, FS);
    cfg.lossless = true;
    cfg.settings.chains = Some(
        builtin_chains()
            .into_iter()
            .filter(|c| c.id == "wfm-rds")
            .collect(),
    );
    ctl.hold_at(total);
    let handle = Pipeline::start(
        cfg,
        Box::new(radio),
        SourceInfo {
            sample_rate_hz: FS,
            center_hz: CENTER_HZ,
            start_time: t0,
        },
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let counters = handle.counters();
    assert!(ctl.wait_emitted(total, Duration::from_secs(600)));
    wait("the scene to be read", Duration::from_secs(600), || {
        counters.detect_reader.samples.load(Ordering::Relaxed) >= total
    });
    ctl.finish();
    let (s, fired) = wait_guarded(handle, Duration::from_secs(600));
    eprintln!("{}", s.to_text());
    assert!(!fired, "the run finished on its own");
    assert!(s.errors.is_empty(), "{:?}", s.errors);

    // T-962: what this test is about is that the target's **PI is decoded** between equal-power
    // neighbours. It used to look that up through `decodes_for_identity`, which asks a different
    // question — whether the PI was committed as a transmitter *identity* — and those two came
    // apart when the commit bound landed. The claim is read off the decode row itself now, so it
    // keeps saying what T-099 meant by it whatever the identity gate decides.
    let rows = rds_pi_rows(&dir.0);
    let decoded = |pi: &str| rows.iter().filter(|r| r.0 == pi).count();
    let found: Vec<(&str, usize)> = scene.iter().map(|&(_, pi, _)| (pi, decoded(pi))).collect();
    eprintln!(
        "[{SIGNAL_062}] chains attached {}, mode rejected {}, demodulations {}, PI decodes {found:?}, rows {rows:?}",
        s.counter("/chains/attached"),
        s.counter("/chains/mode_rejected"),
        s.counter("/chains/demodulations"),
    );
    assert!(
        s.counter("/chains/demodulations") >= 1,
        "[{SIGNAL_062}] no WFM demodulation in the dense scene"
    );
    assert!(
        found[0].1 > 0,
        "[{SIGNAL_062}] target PI not decoded between equal-power neighbours: {found:?}"
    );

    // T-962, pinned here because this is where it was measured: this scene's RDS evidence window.
    //
    // Measured when the bound landed, every station in this clean synthetic scene decoded its PI
    // from **three** CRC-valid groups — a short, contended chain window (three stations 400 kHz
    // apart), the same vote count the false 98.085 MHz commit reached, while one real station read
    // for 4.8 s gives 52 (`hk-demod::signal_062_real`). The window length is not this test's to
    // pin: T-926 (full 4 s WFM windows) and T-971 (accumulation across sessions) lengthen it by
    // design, and scheduling under load changes it. What must hold whatever the window is the
    // rule itself, row by row: **provisional exactly when the vote is below the bar, committed
    // exactly at or above it, and a DecodedIdentity only on a committed row.**
    let bar = u64::from(hk_model::RDS_PI_COMMIT_VOTES);
    eprintln!(
        "[{SIGNAL_062}] T-962 RDS evidence window (votes, provisional, block error, identity): \
         {rows:?}"
    );
    for (pi, votes, provisional, error, has_identity) in &rows {
        assert_eq!(
            *provisional,
            *votes < bar,
            "[{SIGNAL_062}] T-962: PI {pi} with {votes} agreeing groups against a {bar}-vote bar"
        );
        assert_eq!(
            *has_identity, !*provisional,
            "[{SIGNAL_062}] T-962: PI {pi} ({votes} votes): an identity is written exactly when \
             the PI is committed"
        );
        assert!(
            *error < 1e-9,
            "[{SIGNAL_062}] T-962: a clean scene decodes without a damaged block; PI {pi} block \
             error rate {error}"
        );
    }
}

/// Every `rds-pi` decode row the run stored: (PI, vote count, provisional, block error rate,
/// carries a DecodedIdentity).
///
/// Read straight off the table, because the point is to see the rows the *identity* index cannot
/// see: a provisional PI writes no identity, so `decodes_for_identity` returns none of them.
fn rds_pi_rows(dir: &Path) -> Vec<(String, u64, bool, f64, bool)> {
    let conn = rusqlite::Connection::open(dir.join("hackriff.db")).unwrap();
    let mut stmt = conn.prepare("SELECT body FROM decode ORDER BY t").unwrap();
    let bodies: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    bodies
        .iter()
        .map(|b| serde_json::from_str::<serde_json::Value>(b).unwrap())
        .filter(|d| d["frame_model"] == "rds-pi")
        .map(|d| {
            let m = &d["metadata"];
            (
                m["pi"].as_str().unwrap_or_default().to_owned(),
                m["pi_votes"].as_u64().unwrap_or_default(),
                m["pi_provisional"].as_bool().unwrap_or_default(),
                m["block_error_rate"].as_f64().unwrap_or(1.0),
                !d["identity"].is_null(),
            )
        })
        .collect()
}

/// T-129 (AWARE-042, SIGNAL-062): occupancy channels learned blind on the dense scene (no chains)
/// match the hidden station truth: one channel per station covering its occupied band, each
/// occupied; noise-only stretches of the band learn no channel and read idle.
#[test]
fn t129_dense_fm_learns_one_occupancy_channel_per_station() {
    let scene = [
        (100e3, "C0DE", 9_901),
        (100e3 - SPACING_HZ, "1A2B", 9_902),
        (100e3 + SPACING_HZ, "3C4D", 9_903),
    ];
    let Some((iq, stations)) = scene_iq(&scene, 2.0) else {
        return;
    };
    assert_eq!(stations.len(), 3, "{stations:?}");
    let total = 3 * iq.len() as u64;
    let dir = TempDir::new("t129-dense-fm");
    let (radio, ctl) = radio::Radio::new(CENTER_HZ, FS, 16_384, radio::looped(iq));
    let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
    let mut cfg = PipelineConfig::new(&dir.0, replay_plan(CENTER_HZ, FS, t0)).unwrap();
    cfg.source_class = window_class(CENTER_HZ, FS);
    cfg.lossless = true;
    cfg.settings.chains = Some(Vec::new());
    ctl.hold_at(total);
    let handle = Pipeline::start(
        cfg,
        Box::new(radio),
        SourceInfo {
            sample_rate_hz: FS,
            center_hz: CENTER_HZ,
            start_time: t0,
        },
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let counters = handle.counters();
    let occ = handle.occupancy();
    let product = handle.floor_product();
    assert!(ctl.wait_emitted(total, Duration::from_secs(600)));
    wait("the scene to be read", Duration::from_secs(600), || {
        counters.detect_reader.samples.load(Ordering::Relaxed) >= total
    });
    ctl.finish();
    let (s, fired) = wait_guarded(handle, Duration::from_secs(600));
    assert!(!fired && s.errors.is_empty(), "{:?}", s.errors);

    let end = product
        .lock()
        .unwrap()
        .uncalibrated_pyramid()
        .latest_frame_end()
        .expect("history holds frames");
    let span = TimeRange::new(t0.saturating_add_nanos(-1_000_000_000), end);
    let band = FreqRange::centered(CENTER_HZ, 0.8 * FS);
    let (_, f_cell) = occ.plan_info();
    let rows = occ.span_stats(band, span).expect("span stats");
    let mut channels = Vec::new();
    for r in &rows {
        match r.subject {
            OccupancySubject::Channel { key } => {
                let f = key.freq(f_cell);
                eprintln!(
                    "[T-129 dense] ch {:.3}-{:.3} ({:.0} kHz) fco {:?}",
                    f.lo_hz / 1e6,
                    f.hi_hz / 1e6,
                    f.width_hz() / 1e3,
                    r.fco
                );
                channels.push((f, r.fco));
            }
            OccupancySubject::Band { .. } => eprintln!(
                "[T-129 dense] band fco {:?} all-visits {:?} fbo {:?}",
                r.fco, r.fco_all_visits, r.fbo
            ),
        }
    }
    eprintln!("[T-129 dense] truth {stations:?}");
    assert_eq!(channels.len(), stations.len(), "one channel per station");
    assert_station_channels("T-129 dense", &channels, &stations, 1.5);
    // The noise-only stretch below the lowest station reads idle.
    let lowest = stations
        .iter()
        .map(|s| s.0 - 0.5 * s.1)
        .fold(f64::INFINITY, f64::min);
    let gap = FreqRange::new(band.lo_hz + 20e3, lowest - 60e3);
    let gap_rows = occ.span_stats(gap, span).expect("gap stats");
    let gap_band = gap_rows
        .iter()
        .find(|r| matches!(r.subject, OccupancySubject::Band { .. }))
        .expect("a gap band row");
    assert!(
        gap_band.fco.is_some_and(|x| x <= 0.05),
        "gap {:.3}-{:.3} MHz reads {:?}",
        gap.lo_hz / 1e6,
        gap.hi_hz / 1e6,
        gap_band.fco
    );
}
