//! T-891 acceptance: SPACE-001, SPACE-041 and PROP-019 end to end **through the device
//! interface** — a synthetic VLF recording behind the mock accessory source
//! (`hk_core::source::AccessoryMockDriver`), read block by block by the VLF service
//! (`hk_pipeline::vlf`), asserted on what it reports.
//!
//! Blind: the truth (carrier frequencies, the SID step, the phase advance, the sferic times) is
//! only in this file's generator and its assertions; the service gets default settings plus, for
//! PROP-019, the one piece of external geometry the use case needs (the transmitter's path length),
//! never a transmitter list or a frequency to tune to.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hk_core::source::{AccessoryMockDriver, AccessoryMockOptions, write_real_sigmf};
use hk_core::{
    BlockHeader, Discontinuity, ProvenanceHandle, Source, SourceCapabilities, SourceControl,
    SourceDriver, SourceError,
};
use hk_dsp::vlf::reflection_height_change_km;
use hk_model::{Provenance, SampleTime, Timestamp};
use hk_pipeline::vlf::{VlfConfig, VlfPath, VlfReport, VlfService, VlfState, analyze_source};
use num_complex::Complex32;

const FS: f64 = 48_000.0;
const DURATION_S: f64 = 40.0;
/// 2026-09-25T12:00:00Z, the recording's `core:datetime`.
const T0_NS: i64 = 1_790_337_600_000_000_000;

// ---- hidden truth -------------------------------------------------------------------------
/// SPACE-001: a transmitter whose amplitude jumps +50 % at `SID_STEP_S` (a flare's D-region drop).
const SID_HZ: f64 = 21_437.62;
const SID_AMP: f32 = 0.010;
const SID_STEP_S: f64 = 22.0;
/// PROP-019: a transmitter whose phase advances by `PHASE_STEP_RAD` at `PHASE_STEP_S`.
const PHASE_HZ: f64 = 16_413.2;
const PHASE_AMP: f32 = 0.008;
const PHASE_STEP_S: f64 = 27.0;
const PHASE_STEP_RAD: f64 = 0.9;
/// SPACE-041: sferic onsets.
const SFERICS_S: [f64; 4] = [3.2, 11.7, 18.05, 33.3];
// -------------------------------------------------------------------------------------------

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hk-vlf-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn synth() -> Vec<f32> {
    let n = (FS * DURATION_S) as usize;
    let mut seed = 0x5eed_u32;
    let mut noise = || {
        // Sum of 4 uniforms ≈ Gaussian, σ ≈ 0.002.
        let mut s = 0.0f32;
        for _ in 0..4 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            s += (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
        }
        s * 0.0035
    };
    let tau = 2.0 * std::f64::consts::PI;
    let mut x: Vec<f32> = (0..n)
        .map(|i| {
            let t = i as f64 / FS;
            let a = if t < SID_STEP_S {
                SID_AMP
            } else {
                SID_AMP * 1.5
            };
            let ph = if t < PHASE_STEP_S {
                0.0
            } else {
                PHASE_STEP_RAD
            };
            a * (tau * SID_HZ * t + 0.3).cos() as f32
                + PHASE_AMP * (tau * PHASE_HZ * t + ph).cos() as f32
                + noise()
        })
        .collect();
    for s in SFERICS_S {
        // A damped 5 kHz ring, 0.2 ms decay: broadband and impulsive, like a lightning sferic.
        let i0 = (s * FS) as usize;
        for k in 0..200 {
            let t = k as f64 / FS;
            x[i0 + k] += (0.9 * (-t / 2e-4).exp() * (tau * 5_000.0 * t).cos()) as f32;
        }
    }
    x
}

/// Writes the synthetic scene; `gpsdo` marks the capture as GPS-disciplined in its provenance.
fn fixture(tag: &str, gpsdo: bool) -> PathBuf {
    let meta = temp_dir(tag).join("vlf_scene.sigmf-meta");
    let prov: Option<Provenance> = gpsdo.then(|| {
        serde_json::from_value(serde_json::json!({
            "device_id": "vlf-receiver:bench",
            "tune": {"center_hz": 0.0, "sample_rate_hz": FS, "lna_db": 0.0, "vga_db": 0.0,
                     "amp_on": false, "bandwidth_hz": FS / 2.0},
            "overload": false, "quantisation_limited": false,
            "clock_source": "gpsdo", "clock_locked": true,
            "timestamp_method": "external-reference"
        }))
        .unwrap()
    });
    write_real_sigmf(
        &meta,
        &synth(),
        FS,
        "2026-09-25T12:00:00Z",
        "synthetic VLF scene",
        prov,
    )
    .unwrap();
    meta
}

fn open(meta: &PathBuf) -> (AccessoryMockDriver, Box<dyn Source>) {
    let d = AccessoryMockDriver::new(meta, AccessoryMockOptions::default()).unwrap();
    let s = d.open(&d.default_request()).unwrap();
    (d, s)
}

fn t_s(t: Timestamp) -> f64 {
    (t.as_unix_nanos() - T0_NS) as f64 / 1e9
}

fn path_cfg() -> VlfConfig {
    VlfConfig {
        // PROP-019's geometry: the user knows this transmitter is 2000 km away.
        paths: vec![VlfPath {
            carrier_hz: PHASE_HZ,
            tolerance_hz: 1.0,
            path_km: 2000.0,
            base_height_km: 70.0,
        }],
        ..VlfConfig::default()
    }
}

fn carrier(r: &VlfReport, hz: f64) -> &hk_pipeline::vlf::VlfCarrierReport {
    r.carriers
        .iter()
        .find(|c| (c.carrier_hz_refined - hz).abs() < 0.5)
        .unwrap_or_else(|| panic!("no carrier near {hz} Hz: {:#?}", r.carriers))
}

fn assert_scene(r: &VlfReport) {
    assert_eq!(r.state, VlfState::Finished, "{:?}", r.error);
    assert_eq!(r.accessory, Some("vlf-receiver"));
    assert!(
        r.device_id
            .as_deref()
            .is_some_and(|d| d.starts_with("vlf-receiver:"))
    );
    assert_eq!(
        r.provenance
            .as_ref()
            .and_then(|p| p.antenna_port.as_deref()),
        Some("accessory:vlf-receiver")
    );
    assert_eq!(
        r.carriers.len(),
        2,
        "exactly the two transmitters: {:#?}",
        r.carriers
    );

    // SPACE-001: the SID step, on the right carrier, at the right time, of the right size.
    let sid = carrier(r, SID_HZ);
    assert!((sid.carrier_hz_refined - SID_HZ).abs() < 0.05, "{sid:?}");
    assert_eq!(sid.amplitude_steps.len(), 1, "{:?}", sid.amplitude_steps);
    let st = sid.amplitude_steps[0];
    assert!((t_s(st.t_ns) - SID_STEP_S).abs() < 0.3, "{st:?}");
    assert!((st.relative_change - 0.5).abs() < 0.1, "{st:?}");
    assert!(sid.phase_steps.is_empty(), "{:?}", sid.phase_steps);
    assert!(
        sid.phase_steps
            .iter()
            .all(|p| p.reflection_height_change_km.is_none())
    );

    // PROP-019: the phase advance → a lower reflection height.
    let ph = carrier(r, PHASE_HZ);
    assert!(ph.amplitude_steps.is_empty(), "{:?}", ph.amplitude_steps);
    assert_eq!(ph.phase_steps.len(), 1, "{:?}", ph.phase_steps);
    let ps = ph.phase_steps[0];
    assert!((t_s(ps.t_ns) - PHASE_STEP_S).abs() < 0.3, "{ps:?}");
    assert!((ps.dphi_rad - PHASE_STEP_RAD).abs() < 0.05, "{ps:?}");

    // SPACE-041: every sferic, as a Detection through the accessory, at its time.
    assert_eq!(r.sferic_total, SFERICS_S.len() as u64, "{:#?}", r.sferics);
    for (d, truth) in r.sferics.iter().zip(SFERICS_S) {
        assert!(
            (t_s(d.time.start) - truth).abs() < 1e-3,
            "{truth}: {:?}",
            d.time
        );
        assert!(d.time.end > d.time.start && t_s(d.time.end) - truth < 0.006);
        assert!(d.flags.impulsive);
        assert_eq!(Some(d.provenance_ref), r.provenance_ref);
        assert_eq!(d.freq().lo_hz, 0.0);
        assert_eq!(d.freq().hi_hz, FS / 2.0);
        assert!(d.snr_peak_db > 20.0, "{}", d.snr_peak_db);
    }
    assert!((t_s(r.window.unwrap().end) - DURATION_S).abs() < 1e-6);
}

#[test]
fn space_001_space_041_prop_019_blind_through_the_mock_accessory() {
    let meta = fixture("scene", true);
    let (_d, src) = open(&meta);
    let r = analyze_source(src, path_cfg());
    assert_scene(&r);
    assert!(r.phase_disciplined, "a GPSDO-locked capture");
    let ps = carrier(&r, PHASE_HZ).phase_steps[0];
    let want = reflection_height_change_km(PHASE_STEP_RAD, PHASE_HZ, 2000.0, 70.0);
    let got = ps
        .reflection_height_change_km
        .expect("the path geometry matched");
    assert!(want < 0.0 && (got - want).abs() < 0.5, "{got} vs {want}");
    assert_eq!(ps.path_km, Some(2000.0));
}

#[test]
fn prop_019_free_running_clock_is_not_claimed_disciplined() {
    let meta = fixture("free", false);
    let (_d, src) = open(&meta);
    let r = analyze_source(src, VlfConfig::default());
    assert_scene(&r);
    assert!(!r.phase_disciplined);
    // No geometry given: the step is measured, never converted on a guess.
    assert!(
        carrier(&r, PHASE_HZ).phase_steps[0]
            .reflection_height_change_km
            .is_none()
    );
}

/// A soundcard overrun mid-stream: counted, and the analyses carry on across it.
struct OverrunAfter {
    inner: Box<dyn Source>,
    at_block: u64,
    frames: u64,
    handle: Arc<AtomicU64>,
    n: u64,
}

impl Source for OverrunAfter {
    fn capabilities(&self) -> &SourceCapabilities {
        self.inner.capabilities()
    }
    fn control(&self) -> Arc<dyn SourceControl> {
        self.inner.control()
    }
    fn read_block(&mut self, s: &mut Vec<Complex32>) -> Result<Option<BlockHeader>, SourceError> {
        self.n += 1;
        if self.n == self.at_block {
            self.handle.fetch_add(self.frames, Ordering::SeqCst);
        }
        self.inner.read_block(s)
    }
}

#[test]
fn space_041_overrun_is_counted_and_analysis_continues() {
    let meta = fixture("overrun", true);
    let (d, src) = open(&meta);
    // 0.5 s dropped at ~15 s: between sferics and before both steps.
    let src = OverrunAfter {
        inner: src,
        at_block: 150,
        frames: 24_000,
        handle: d.overrun_handle(),
        n: 0,
    };
    let r = analyze_source(Box::new(src), path_cfg());
    assert_eq!(r.gaps, 1);
    assert_eq!(r.dropped_samples, 24_000);
    assert_eq!(r.samples, (FS * DURATION_S) as u64 - 24_000);
    assert_scene(&r);
}

#[test]
fn space_001_live_service_reports_while_running_and_stops() {
    let meta = fixture("service", true);
    let (_d, src) = open(&meta);
    let svc = VlfService::start(src, path_cfg()).unwrap();
    assert!(
        svc.wait_done(Duration::from_secs(60)),
        "{:?}",
        svc.report(false).state
    );
    let r = svc.report(true);
    assert_scene(&r);
    let pts = carrier(&r, SID_HZ).points.as_ref().unwrap();
    assert_eq!(pts.len(), carrier(&r, SID_HZ).point_count);
    assert!(
        (t_s(pts[0].t_ns)).abs() < 1e-9,
        "tracked from the stream's first sample"
    );
    svc.stop();
}

/// A stream that is not accessory-fed: a radio's own provenance.
struct Radio {
    caps: SourceCapabilities,
    prov: ProvenanceHandle,
    n: u64,
}

impl Source for Radio {
    fn capabilities(&self) -> &SourceCapabilities {
        &self.caps
    }
    fn control(&self) -> Arc<dyn SourceControl> {
        unimplemented!("not used")
    }
    fn read_block(&mut self, s: &mut Vec<Complex32>) -> Result<Option<BlockHeader>, SourceError> {
        s.clear();
        s.resize(4800, Complex32::new(0.01, 0.0));
        let h = BlockHeader {
            time: SampleTime {
                sample_index: self.n * 4800,
                host_time: Timestamp::from_unix_nanos(T0_NS),
            },
            provenance: self.prov.clone(),
            discontinuity: if self.n == 0 {
                Discontinuity::STREAM_START
            } else {
                Discontinuity::NONE
            },
            dropped_before: 0,
        };
        self.n += 1;
        Ok((self.n < 100).then_some(h))
    }
}

#[test]
fn the_base_device_alone_claims_none_of_them() {
    let meta = fixture("refuse", true);
    let (_d, src) = open(&meta);
    let mut accessory = {
        let mut b = Vec::new();
        let mut s = src;
        s.read_block(&mut b)
            .unwrap()
            .unwrap()
            .provenance
            .get()
            .clone()
    };
    // A HackRF's stream, and one that forges only the port: both refused.
    accessory.device_id = "hackrf:0000000000000000".into();
    for port in [None, Some("accessory:vlf-receiver".to_string())] {
        let mut p = accessory.clone();
        p.antenna_port = port;
        let r = analyze_source(
            Box::new(Radio {
                caps: SourceCapabilities::hackrf_one(),
                prov: ProvenanceHandle::new(p),
                n: 0,
            }),
            VlfConfig::default(),
        );
        assert_eq!(r.state, VlfState::Failed);
        assert!(
            r.error
                .as_deref()
                .unwrap()
                .contains("not an accessory-fed source"),
            "{:?}",
            r.error
        );
        assert!(r.carriers.is_empty() && r.sferics.is_empty());
    }
    assert!(!SourceCapabilities::hackrf_one().supports_frequency(SID_HZ));
}
