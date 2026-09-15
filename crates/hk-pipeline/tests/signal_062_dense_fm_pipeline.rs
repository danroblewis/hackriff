//! T-099 (SIGNAL-062) through the composed pipeline and a scripted radio: a dense FM scene, the
//! target station with equal-power neighbours 200 kHz either side (each the `fm_broadcast_rds`
//! synthesiser with its own PI), summed and quantised as the capture thread does. Before T-099
//! the neighbours made mode selection abstain on bandwidth and the `wfm-rds` chain stopped at the
//! probe (`mode_rejected`); now the chain attaches, selects WFM and decodes the target's PI.
//!
//! Blind: the pipeline sees only the radio's IQ; the PIs are the scene's private truth, compared
//! against the stored decodes after the run.

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::*;
use hk_core::{Pacing, ReplayOptions, SigmfReplaySource, Source};
use hk_e2e::SynthRequest;
use hk_model::{ContentClass, DecodedIdentity, IdentityScheme, Timestamp};
use hk_pipeline::class::window_class;
use hk_pipeline::{
    Pipeline, PipelineConfig, SourceInfo, TrackInventory, builtin_chains, replay_plan,
};
use num_complex::Complex;

const SIGNAL_062: &str = "SIGNAL-062";
const FS: f64 = 1.2e6;
/// Capture centre; the stations sit at −100, +100 and +300 kHz from it.
const CENTER_HZ: f64 = 99.4e6;
const SPACING_HZ: f64 = 200e3;

/// One station's IQ (the synthesiser's float output, quantised later with the others).
fn station(offset_hz: f64, pi: &str, seed: u64) -> Option<Vec<Complex<f32>>> {
    let req = SynthRequest::new("fm_broadcast_rds")
        .seed(seed)
        .param("sample_rate", FS)
        .param("center_hz", CENTER_HZ)
        .param("offset_hz", offset_hz)
        .param("duration_s", 2.0)
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
    Some(iq)
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
    let mut sum: Vec<Complex<f32>> = Vec::new();
    for &(offset, pi, seed) in &scene {
        let Some(iq) = station(offset, pi, seed) else {
            return;
        };
        if sum.is_empty() {
            sum = iq;
        } else {
            sum.iter_mut().zip(&iq).for_each(|(a, b)| *a += *b);
        }
    }
    let q = |x: f32| (x * 128.0).round().clamp(-128.0, 127.0) as i8;
    let iq: Vec<Complex<i8>> = sum.iter().map(|z| Complex::new(q(z.re), q(z.im))).collect();
    let total = 3 * iq.len() as u64;
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

    let repo = repo(&dir.0);
    let decoded = |pi: &str| {
        repo.decodes_for_identity(&DecodedIdentity {
            scheme: IdentityScheme::RdsPi,
            value: pi.into(),
        })
        .unwrap()
        .len()
    };
    let found: Vec<(&str, usize)> = scene.iter().map(|&(_, pi, _)| (pi, decoded(pi))).collect();
    eprintln!(
        "[{SIGNAL_062}] chains attached {}, mode rejected {}, demodulations {}, PI decodes {found:?}",
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
}
