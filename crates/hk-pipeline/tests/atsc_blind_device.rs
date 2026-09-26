//! T-979: **through the mock SDR, a synthetic 8VSB channel arrives as one 6 MHz row explained as
//! ATSC, and its pilot offset is recorded as a receiver calibration.**
//!
//! The 2026-09-25 explorer window over UHF 470–608 MHz in San Francisco found 13 ATSC pilots
//! blind and explained none of them: the band plan had no data for 470–608 MHz, every row read
//! "no allocation data", one 6 MHz channel shattered into 15–60 narrow and medium candidates, and
//! the fragments inside channels 29/30 were suggested as `fm-broadcast` at 0.6 from shape alone.
//!
//! This test replays a synthetic channel with exactly that shape — a flat 5.381 MHz plateau with a
//! CW pilot on its lower edge, read 4 ppm low, in noise — **through the device interface**
//! (`open_replay` behind `Pipeline::start`, the same seam as the real HackRF), and asserts the
//! three things the ticket asks for:
//!
//! 1. one inventory row **6 MHz wide** at the channel the pilot implies (deliverable (b));
//! 2. its top explanation is **TV broadcast**, `known` against the new 470–608 MHz band-plan rows,
//!    citing the **measured pilot** rather than the emission's width (deliverables (a) and (b));
//! 3. a **C05 calibration** row measured from the pilot offset (deliverable (c)).
//!
//! **Blind.** The pipeline is given the recording and nothing else: the fixture carries no
//! annotations (`blind_replay_config` strips them) and no hint of what is in it. The truth below
//! is the test's own, checked against what the pipeline found — never looked up and tuned to.
//!
//! What this test does *not* assert is that the detector's own narrow fragments are gone. Merging
//! them into this row is the overlap-re-analysis work (CLAUDE.md: "overlap is an error signal that
//! triggers re-analysis"); what T-979 guarantees is that the 6 MHz row they must merge into exists
//! and is explained.

mod common;

use std::f64::consts::TAU;
use std::path::{Path, PathBuf};

use common::*;
use hk_core::Pacing;
use hk_dsp::fft::{CpuFft, FftBackend};
use hk_estimate::atsc::{CHANNEL_BANDWIDTH_HZ, FLAT_BANDWIDTH_HZ, PILOT_OFFSET_HZ};
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use hk_model::{CalibrationMethod, InventoryQuery, KnownStatus};
use hk_pipeline::atsc::ATSC_PILOT_VERSION;
use hk_pipeline::family::{self, ExplanationEvidence};
use num_complex::Complex32;
use serde_json::json;

// ---- the hidden truth of the fixture -----------------------------------------------------

/// US UHF channel 29: 560–566 MHz (47 CFR 73.603). The pipeline is never told this.
const TRUTH_CHANNEL: u16 = 29;
const TRUTH_CHANNEL_LO_HZ: f64 = 560e6;
/// The receiver reads 4 ppm low — the explorer's measured offset.
const TRUTH_PPM: f64 = -4.0;
/// Tuned centre of the capture: the channel centre.
const CENTER_HZ: f64 = 563e6;
const FS: f64 = 8e6;
const SECONDS: f64 = 0.3;

/// Deterministic xorshift, so the fixture is byte-identical on every machine.
struct Rng(u64);

impl Rng {
    fn next_f64(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }

    fn normal(&mut self) -> f64 {
        let u = self.next_f64().max(1e-12);
        let v = self.next_f64();
        (-2.0 * u.ln()).sqrt() * (TAU * v).cos()
    }
}

/// Writes the 8VSB-like recording as `<name>.sigmf-meta/-data` in `dir`.
///
/// The emission is synthesised from its **spectrum**: random-phase bins across exactly the
/// 5.381 MHz Nyquist band (what 8VSB looks like once the symbols are gone), inverse-transformed,
/// plus the CW pilot on the lower edge at the standard's 11.3 dB below average signal power, plus
/// white noise. Everything sits `TRUTH_PPM` off, as a receiver with that clock error would read it.
fn atsc_recording(dir: &Path, name: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let n = (SECONDS * FS) as usize;
    let block = 1usize << 16;
    let df = FS / block as f64;
    let read = |f: f64| f * (1.0 + TRUTH_PPM * 1e-6) - CENTER_HZ;
    let flat_lo = read(TRUTH_CHANNEL_LO_HZ + PILOT_OFFSET_HZ);
    let plateau_amp = 0.15;
    let pilot_amp = plateau_amp * 10f64.powf(-11.3 / 20.0);

    let mut rng = Rng(0x7979_a75c);
    let mut fft = CpuFft::new(block);
    let mut data: Vec<u8> = Vec::with_capacity(2 * n);
    let mut written = 0usize;
    let mut i0 = 0u64;
    while written < n {
        // One block of the plateau, built in the frequency domain and inverse-transformed by the
        // conjugate trick (hk-dsp plans forward transforms only).
        let k0 = (flat_lo / df).round() as i64;
        let k1 = ((flat_lo + FLAT_BANDWIDTH_HZ) / df).round() as i64;
        let lines = (k1 - k0).max(1) as f64;
        let a = (plateau_amp * block as f64 / lines.sqrt()) as f32;
        let mut spec = vec![Complex32::default(); block];
        for k in k0..k1 {
            let ph = rng.next_f64() * TAU;
            let bin = (k.rem_euclid(block as i64)) as usize;
            spec[bin] = Complex32::new(a * ph.cos() as f32, -a * ph.sin() as f32);
        }
        fft.forward(&mut spec);
        let scale = 1.0 / block as f32;
        for (j, s) in spec.iter().enumerate() {
            if written >= n {
                break;
            }
            let i = i0 + j as u64;
            // The pilot is phase-continuous across blocks: it is one CW line for the whole
            // recording, which is what makes it measurable to a fraction of a bin.
            let ph = TAU * flat_lo / FS * i as f64;
            let re = f64::from(s.re * scale) + pilot_amp * ph.cos() + 0.012 * rng.normal();
            let im = f64::from(-s.im * scale) + pilot_amp * ph.sin() + 0.012 * rng.normal();
            data.push(((re * 127.0).round().clamp(-128.0, 127.0) as i8) as u8);
            data.push(((im * 127.0).round().clamp(-128.0, 127.0) as i8) as u8);
            written += 1;
        }
        i0 += block as u64;
    }
    std::fs::write(dir.join(format!("{name}.sigmf-data")), data).unwrap();

    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(FS);
    meta.captures.push(Capture {
        sample_start: 0,
        frequency: Some(CENTER_HZ),
        datetime: Some("2026-09-25T06:00:00Z".into()),
        provenance: None,
        clip_count: None,
        extra: serde_json::Map::new(),
    });
    let path = dir.join(format!("{name}.sigmf-meta"));
    meta.write(&path).unwrap();
    path
}

#[test]
fn t979_a_blind_8vsb_channel_is_one_six_megahertz_row_explained_as_atsc() {
    let dir = TempDir::new("t979-atsc-blind");
    let fixture = TempDir::new("t979-atsc-fixture");
    let meta = atsc_recording(&fixture.0, "atsc-ch29");
    let (cfg, replay, _input) = blind_replay_config(&dir.0, &meta, json!({}), Pacing::Unpaced);
    let handle = start(cfg, replay);
    let survey = handle.atsc_survey();
    let summary = handle.wait().unwrap();
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    let (measured, empty, channels) = survey.counts();
    eprintln!("atsc survey: measured={measured} empty={empty} channels={channels}");

    let repo = repo(&dir.0);
    let rows = inventory(&repo, InventoryQuery::default());

    // 1. One row, 6 MHz wide, at the channel the measured pilot implies.
    let wide: Vec<_> = rows
        .iter()
        .filter(|r| {
            (r.emitter.bandwidth_hz - CHANNEL_BANDWIDTH_HZ).abs() < CHANNEL_BANDWIDTH_HZ * 0.05
        })
        .collect();
    assert_eq!(
        wide.len(),
        1,
        "one 8VSB channel is one 6 MHz row; inventory: {:?}",
        rows.iter()
            .map(|r| (r.emitter.f_center_hz, r.emitter.bandwidth_hz))
            .collect::<Vec<_>>()
    );
    let row = &wide[0].emitter;
    let truth_center = TRUTH_CHANNEL_LO_HZ + CHANNEL_BANDWIDTH_HZ / 2.0;
    assert!(
        (row.f_center_hz - truth_center).abs() < 20e3,
        "row centre {} Hz, channel {TRUTH_CHANNEL} is centred at {truth_center} Hz",
        row.f_center_hz
    );
    assert!(
        row.classifications
            .iter()
            .any(|c| c.family == "atsc-8vsb" && c.model_version == ATSC_PILOT_VERSION),
        "{:?}",
        row.classifications
    );

    // 2. Explained as TV broadcast, against the new 470-608 MHz band-plan rows, citing the pilot.
    let ex = family::explanations(&repo, row.id).unwrap();
    let top = ex.first().expect("an explanation");
    assert_eq!(top.service, "tv-broadcast", "{ex:#?}");
    assert_eq!(top.status, KnownStatus::Known, "{top:#?}");
    assert!(
        !top.has_flag("no-allocation-data"),
        "470-608 MHz now has allocation data: {top:#?}"
    );
    assert!(!top.has_flag("shape-only"), "{top:#?}");
    assert!(
        ex.iter()
            .all(|e| e.service != "fm-broadcast" || e.rank > top.rank),
        "nothing about a 6 MHz DTV channel is FM broadcast: {ex:#?}"
    );
    let (measured, nominal, offset, channel) = top
        .evidence
        .iter()
        .find_map(|e| match e {
            ExplanationEvidence::Pilot {
                measured_hz,
                nominal_hz,
                offset_hz,
                channel,
                ..
            } => Some((*measured_hz, *nominal_hz, *offset_hz, *channel)),
            _ => None,
        })
        .expect("the explanation cites the measured pilot");
    let truth_pilot = TRUTH_CHANNEL_LO_HZ + PILOT_OFFSET_HZ;
    assert_eq!(channel, Some(u32::from(TRUTH_CHANNEL)));
    assert_eq!(nominal, Some(truth_pilot));
    assert!(
        (measured - truth_pilot * (1.0 + TRUTH_PPM * 1e-6)).abs() < 200.0,
        "pilot measured {measured} Hz, injected {} Hz",
        truth_pilot * (1.0 + TRUTH_PPM * 1e-6)
    );
    let truth_offset = truth_pilot * TRUTH_PPM * 1e-6;
    assert!(
        (offset.unwrap() - truth_offset).abs() < 200.0,
        "pilot offset {} Hz, injected {truth_offset} Hz (the explorer's ~2.4 kHz low)",
        offset.unwrap()
    );

    // 3. The pilot offset was recorded as a receiver calibration: read 4 ppm low is an oscillator
    //    4 ppm fast.
    let cal = repo
        .latest_calibration_state_for_device("sigmf:unknown", &CalibrationMethod::AtscPilot)
        .unwrap()
        .expect("a C05 calibration measured from the pilot");
    assert!(
        (cal.ppm - -TRUTH_PPM).abs() < 0.5,
        "recorded {} ppm, injected an offset of {TRUTH_PPM} ppm",
        cal.ppm
    );
}

/// The fixture is recognisable: the recogniser reads the pilot and the 6 MHz channel straight off
/// the written ci8 file. Without this, a red test above cannot tell a broken fixture from a broken
/// pipeline.
#[test]
fn t979_the_fixture_carries_a_recognisable_8vsb_channel() {
    let fixture = TempDir::new("t979-atsc-selftest");
    let meta = atsc_recording(&fixture.0, "atsc-ch29");
    let raw = std::fs::read(meta.with_extension("sigmf-data")).unwrap();
    let x: Vec<Complex32> = raw
        .chunks_exact(2)
        .map(|p| Complex32::new(f32::from(p[0] as i8) / 128.0, f32::from(p[1] as i8) / 128.0))
        .take((0.05 * FS) as usize)
        .collect();
    let got = hk_estimate::atsc::find_channels(
        &x,
        FS,
        CENTER_HZ,
        &hk_estimate::atsc::AtscConfig::default(),
    );
    assert_eq!(got.len(), 1, "{got:#?}");
    assert_eq!(got[0].us_channel, Some(TRUTH_CHANNEL));
    let read = got[0].ppm.value().expect("a ppm reading");
    assert!((read - TRUTH_PPM).abs() < 1.0, "read {read} ppm");
}
