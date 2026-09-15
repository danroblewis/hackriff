//! T-049: the device conformance suite (`hk_core::source::conformance`) against the mock SDR
//! (synthetic ci8 and cf32 recordings, the real FM fixture when its LFS data is present) and, as a
//! manual HIL case, the HackRF One. Then the mock's realistic behaviour: retune inside and outside
//! coverage, rate changes, gain clipping with overload provenance, injected and real-time
//! overruns, end of stream and looping. Every read loop is bounded.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hk_core::source::conformance::{self, ConformanceSpec};
use hk_core::{
    BlockHeader, Coverage, Discontinuity, MockEnd, MockOptions, MockSdrDriver, MockSdrSource,
    Pacing, Source, SourceDriver,
};
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use hk_model::{ClockSource, Provenance, TimestampMethod, Tune};
use num_complex::{Complex, Complex32};

/// A scratch directory removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "hk-mock-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A synthetic recording: tones `(offset Hz, amplitude)` in complex white noise of `noise_var`
/// (full scale²), LNA 16 / VGA 20 / amp off.
struct Synth {
    datatype: Datatype,
    fs: f64,
    center: f64,
    secs: f64,
    tones: Vec<(f64, f64)>,
    noise_var: f64,
}

impl Synth {
    fn new(datatype: Datatype) -> Self {
        Self {
            datatype,
            fs: 1e6,
            center: 100e6,
            secs: 2.0,
            tones: vec![(200e3, 0.25)],
            noise_var: 0.002,
        }
    }

    fn write(&self, dir: &Path, name: &str) -> PathBuf {
        let n = (self.secs * self.fs) as usize;
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut uniform = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            ((state >> 11) as f64 + 0.5) / (1u64 << 53) as f64
        };
        let mut bytes = Vec::with_capacity(n * self.datatype.bytes_per_sample());
        for i in 0..n {
            let t = i as f64 / self.fs;
            let mut z = num_complex::Complex64::new(0.0, 0.0);
            for &(f, a) in &self.tones {
                z += num_complex::Complex64::from_polar(a, std::f64::consts::TAU * f * t);
            }
            let r = (-self.noise_var * uniform().ln()).sqrt();
            z += num_complex::Complex64::from_polar(r, std::f64::consts::TAU * uniform());
            match self.datatype {
                Datatype::Ci8 => {
                    let q = |v: f64| (v * 128.0).round().clamp(-128.0, 127.0) as i8 as u8;
                    bytes.push(q(z.re));
                    bytes.push(q(z.im));
                }
                _ => {
                    bytes.extend_from_slice(&(z.re as f32).to_le_bytes());
                    bytes.extend_from_slice(&(z.im as f32).to_le_bytes());
                }
            }
        }
        std::fs::write(dir.join(format!("{name}.sigmf-data")), bytes).unwrap();
        let mut meta = SigmfMeta::new(self.datatype);
        meta.global.sample_rate = Some(self.fs);
        meta.global.provenance = Some(Provenance {
            device_id: "synthetic:t-049".into(),
            tune: Tune {
                center_hz: self.center,
                sample_rate_hz: self.fs,
                lna_db: 16.0,
                vga_db: 20.0,
                amp_on: false,
                bandwidth_hz: self.fs,
            },
            overload: false,
            quantisation_limited: false,
            temperature_c: None,
            antenna_port: None,
            clock_source: ClockSource::Internal,
            clock_locked: true,
            calibration_state_ref: None,
            spur_mask_ref: None,
            timestamp_method: TimestampMethod::Synthetic,
            timestamp_error_budget_ns: Some(0),
        });
        meta.captures.push(Capture {
            sample_start: 0,
            frequency: Some(self.center),
            datetime: Some("2026-09-13T12:00:00Z".into()),
            provenance: None,
            clip_count: None,
            extra: serde_json::Map::new(),
        });
        let path = dir.join(format!("{name}.sigmf-meta"));
        meta.write(&path).unwrap();
        path
    }
}

fn opts(block_len: usize) -> MockOptions {
    MockOptions {
        block_len,
        ..MockOptions::default()
    }
}

/// The real FM fixture with its LFS data, searched up through ancestor checkouts; `None` skips
/// (`HK_REQUIRE_FIXTURES=1` fails instead).
fn fm_fixture() -> Option<PathBuf> {
    let rel = Path::new("fixtures/hackrf/2026-09-13/fm_100p8M_2p4M_l32g30a1_t1p5_5s.sigmf-meta");
    for dir in Path::new(env!("CARGO_MANIFEST_DIR")).ancestors() {
        let meta = dir.join(rel);
        let fetched = std::fs::File::open(meta.with_extension("sigmf-data"))
            .and_then(|f| f.metadata())
            .is_ok_and(|m| m.len() > 1 << 20);
        if fetched {
            return Some(meta);
        }
    }
    if std::env::var("HK_REQUIRE_FIXTURES").is_ok_and(|v| v == "1") {
        panic!("fm fixture data not fetched (git lfs pull)");
    }
    eprintln!("SKIP: fm fixture data is not fetched (git lfs pull)");
    None
}

fn f32s(block: &[Complex<i8>]) -> Vec<Complex32> {
    block
        .iter()
        .map(|s| Complex32::new(f32::from(s.re) / 128.0, f32::from(s.im) / 128.0))
        .collect()
}

/// Hann-windowed tone power at `f`: a tone of amplitude A reads A².
fn tone_power(x: &[Complex32], f: f64, fs: f64) -> f64 {
    let n = x.len();
    let (mut acc, mut wsum) = (num_complex::Complex64::new(0.0, 0.0), 0.0);
    for (i, z) in x.iter().enumerate() {
        let w = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n as f64).cos();
        let z = num_complex::Complex64::new(f64::from(z.re), f64::from(z.im));
        acc += z
            * w
            * num_complex::Complex64::from_polar(1.0, -std::f64::consts::TAU * f * i as f64 / fs);
        wsum += w;
    }
    (acc.norm() / wsum).powi(2)
}

fn mean_power(x: &[Complex32]) -> f64 {
    x.iter().map(|z| f64::from(z.norm_sqr())).sum::<f64>() / x.len() as f64
}

fn db(x: f64) -> f64 {
    10.0 * x.log10()
}

/// Reads until `pred` holds (bounded), returning that block.
fn read_until(
    src: &mut MockSdrSource,
    buf: &mut Vec<Complex<i8>>,
    pred: impl Fn(&BlockHeader) -> bool,
) -> BlockHeader {
    let start = Instant::now();
    for _ in 0..200 {
        assert!(start.elapsed() < Duration::from_secs(120), "watchdog");
        let h = src.read_block_ci8(buf).unwrap().expect("stream open");
        if pred(&h) {
            return h;
        }
    }
    panic!("condition not reached within 200 blocks");
}

fn mock_spec(driver: &Arc<MockSdrDriver>, retune_hz: f64, alt_rate: f64) -> ConformanceSpec {
    let mut spec = ConformanceSpec::new(driver.default_request(), retune_hz);
    spec.alt_rate_hz = Some(alt_rate);
    spec.expect_pausable = Some(matches!(driver.options().pacing, Pacing::Unpaced));
    spec.exact_loss_accounting = true;
    spec.max_blocks_per_wait = 16;
    let d = Arc::clone(driver);
    spec.inject_overrun = Some(Box::new(move || {
        d.last_control().expect("opened").inject_overrun(12_345);
        12_345
    }));
    spec
}

#[test]
fn mock_passes_device_conformance_on_a_synthetic_ci8_recording() {
    let dir = Scratch::new("conf8");
    let meta = Synth::new(Datatype::Ci8).write(&dir.0, "tone");
    let driver = Arc::new(
        MockSdrDriver::new(
            &meta,
            MockOptions {
                end: MockEnd::Loop,
                ..opts(4096)
            },
        )
        .unwrap(),
    );
    let report = conformance::run(&*driver, &mock_spec(&driver, 100.05e6, 2e6));
    eprintln!("{report}");
    report.assert_passed();
    assert_eq!(report.checks.len(), conformance::CHECKS.len());
}

#[test]
fn mock_passes_device_conformance_on_a_real_time_cf32_recording() {
    let dir = Scratch::new("conf32");
    let meta = Synth {
        fs: 250e3,
        tones: vec![(50e3, 0.25)],
        ..Synth::new(Datatype::Cf32Le)
    }
    .write(&dir.0, "tone");
    let driver = Arc::new(
        MockSdrDriver::new(
            &meta,
            MockOptions {
                pacing: Pacing::RealTime { speed: 4.0 },
                end: MockEnd::Loop,
                ..opts(2048)
            },
        )
        .unwrap(),
    );
    let mut spec = mock_spec(&driver, 100.02e6, 500e3);
    spec.pause = Duration::ZERO;
    let report = conformance::run(&*driver, &spec);
    eprintln!("{report}");
    report.assert_passed();
}

#[test]
fn mock_passes_device_conformance_on_the_fm_fixture() {
    let Some(meta) = fm_fixture() else { return };
    let driver = Arc::new(MockSdrDriver::new(&meta, opts(65_536)).unwrap());
    let report = conformance::run(&*driver, &mock_spec(&driver, 101.0e6, 2e6));
    eprintln!("{report}");
    report.assert_passed();
}

/// Manual HIL (T5): the same suite against the HackRF One. Receive only; check the device is free
/// (`hackrf_info`) and run `cargo test -p hk-core --features hackrf --test device_conformance --
/// --ignored --nocapture`.
#[cfg(feature = "hackrf")]
#[test]
#[ignore = "needs a HackRF One (run with --ignored)"]
fn hackrf_one_passes_device_conformance() {
    use hk_core::{HackRfDriver, NamedGain, OpenRequest};
    let request = OpenRequest {
        device: None,
        center_hz: 100.75e6,
        sample_rate_hz: 2e6,
        gains: vec![
            NamedGain::new("lna", 32.0),
            NamedGain::new("vga", 30.0),
            NamedGain::new("amp", 11.0),
        ],
        baseband_filter_hz: None,
        bias_tee: false,
    };
    let mut spec = ConformanceSpec::new(request, 101.0e6);
    spec.expect_pausable = Some(false);
    spec.time_tolerance_ns = 1_000;
    let report = conformance::run(&HackRfDriver, &spec);
    eprintln!("{report}");
    report.assert_passed();
}

#[test]
fn tuned_to_the_recording_the_mock_is_bit_exact() {
    let dir = Scratch::new("exact");
    let meta = Synth::new(Datatype::Ci8).write(&dir.0, "tone");
    let bytes = std::fs::read(meta.with_extension("sigmf-data")).unwrap();
    let driver = MockSdrDriver::new(&meta, opts(4096)).unwrap();
    let mut src = driver.open_mock(&driver.default_request()).unwrap();
    assert!(src.pausable(), "unpaced is accelerated and pausable");
    let mut buf = Vec::new();
    let mut at = 0;
    let mut total = 0u64;
    while let Some(h) = src.read_block_ci8(&mut buf).unwrap() {
        assert_eq!(h.first_sample(), total);
        assert_eq!(
            Coverage::from_provenance(&h.provenance),
            Some(Coverage::Recorded)
        );
        for s in &buf {
            assert_eq!((s.re as u8, s.im as u8), (bytes[at], bytes[at + 1]));
            at += 2;
        }
        total += buf.len() as u64;
    }
    assert_eq!(at, bytes.len(), "Stop ends with the recording");
    assert!(src.read_block_ci8(&mut buf).unwrap().is_none());
    let h0 = driver.recording();
    assert!(
        (db(h0.floor_power) - db(0.002)).abs() < 1.0,
        "floor {}",
        h0.floor_power
    );
}

#[test]
fn a_tone_keeps_its_absolute_frequency_after_a_retune_inside_coverage() {
    let dir = Scratch::new("retune");
    // 4 Msps at 100 MHz (98..102 MHz recorded), a tone at 100.8 MHz.
    let meta = Synth {
        fs: 4e6,
        secs: 0.25,
        tones: vec![(800e3, 0.25)],
        ..Synth::new(Datatype::Ci8)
    }
    .write(&dir.0, "tone");
    let driver = MockSdrDriver::new(&meta, opts(16_384)).unwrap();
    let mut src = driver.open_mock(&driver.default_request()).unwrap();
    let control = src.control();
    let mut buf = Vec::new();
    src.read_block_ci8(&mut buf).unwrap().unwrap();
    assert!(
        tone_power(&f32s(&buf), 800e3, 4e6) > 0.05,
        "the tone at +800 kHz"
    );

    // 100.6 MHz at 2 Msps: 99.6..101.6 MHz lies inside the recording; the tone is now at +200 kHz.
    control.tune(100.6e6).unwrap();
    control.set_sample_rate(2e6).unwrap();
    let h = read_until(&mut src, &mut buf, |h| {
        h.discontinuity.contains(Discontinuity::RETUNE)
    });
    assert!(
        h.discontinuity
            .contains(Discontinuity::RATE_CHANGE | Discontinuity::GAP)
    );
    assert_eq!(h.dropped_before, 16_384, "one block settles");
    assert_eq!(
        Coverage::from_provenance(&h.provenance),
        Some(Coverage::Recorded)
    );
    src.read_block_ci8(&mut buf).unwrap().unwrap();
    let x = f32s(&buf);
    let at = tone_power(&x, 200e3, 2e6);
    assert!((db(at) - db(0.0625)).abs() < 1.0, "tone at +200 kHz: {at}");
    for elsewhere in [-200e3, 800e3, -800e3, 600e3] {
        assert!(
            tone_power(&x, elsewhere, 2e6) < 1e-4,
            "no image at {elsewhere}"
        );
    }

    // 100.2 MHz at the recording rate: 98.2..102.2 MHz is partly outside; tone at +600 kHz.
    control.set_sample_rate(4e6).unwrap();
    control.tune(100.2e6).unwrap();
    let h = read_until(&mut src, &mut buf, |h| {
        h.discontinuity.contains(Discontinuity::RATE_CHANGE)
    });
    assert_eq!(h.center_hz(), 100.2e6);
    assert_eq!(
        Coverage::from_provenance(&h.provenance),
        Some(Coverage::Partial)
    );
    src.read_block_ci8(&mut buf).unwrap().unwrap();
    let at = tone_power(&f32s(&buf), 600e3, 4e6);
    assert!((db(at) - db(0.0625)).abs() < 1.0, "tone at +600 kHz: {at}");
    assert!(
        driver
            .last_control()
            .unwrap()
            .mock_stats()
            .uncovered_samples
            > 0
    );
}

#[test]
fn a_retune_outside_coverage_serves_calibrated_noise_and_is_flagged() {
    let dir = Scratch::new("noise");
    let meta = Synth::new(Datatype::Ci8).write(&dir.0, "tone");
    let driver = MockSdrDriver::new(&meta, opts(16_384)).unwrap();
    let mut src = driver.open_mock(&driver.default_request()).unwrap();
    let mut buf = Vec::new();
    src.read_block_ci8(&mut buf).unwrap().unwrap();
    src.control().tune(433.92e6).unwrap();
    let h = read_until(&mut src, &mut buf, |h| {
        h.discontinuity.contains(Discontinuity::RETUNE)
    });
    assert!(h.discontinuity.contains(Discontinuity::PROVENANCE_CHANGE));
    assert_eq!(h.provenance.antenna_port.as_deref(), Some("mock:noise"));
    assert_eq!(
        Coverage::from_provenance(&h.provenance),
        Some(Coverage::Noise)
    );
    let x = f32s(&buf);
    let p = mean_power(&x);
    let floor = driver.recording().floor_power;
    assert!(
        (db(p) - db(floor)).abs() < 1.0,
        "noise {p} vs recorded floor {floor}"
    );
    assert!(
        tone_power(&x, 200e3, 1e6) < 1e-4,
        "no recorded tone out of coverage"
    );
    let stats = driver.last_control().unwrap().mock_stats();
    assert_eq!(stats.uncovered_samples, buf.len() as u64);
    assert!(!h.provenance.overload);
}

#[test]
fn a_rate_change_resamples_with_the_right_spectrum_width() {
    let dir = Scratch::new("rate");
    // 4 Msps (98..102 MHz), tones at +400 kHz and +1.2 MHz.
    let meta = Synth {
        fs: 4e6,
        secs: 0.25,
        tones: vec![(400e3, 0.2), (1.2e6, 0.2)],
        ..Synth::new(Datatype::Cf32Le)
    }
    .write(&dir.0, "tones");
    let driver = MockSdrDriver::new(&meta, opts(16_384)).unwrap();
    let mut src = driver.open_mock(&driver.default_request()).unwrap();
    let control = src.control();
    let mut buf = Vec::new();
    src.read_block_ci8(&mut buf).unwrap().unwrap();

    // Narrower: 2 Msps spans ±1 MHz. +400 kHz stays, +1.2 MHz is filtered out, not aliased.
    control.set_sample_rate(2e6).unwrap();
    let h = read_until(&mut src, &mut buf, |h| {
        h.discontinuity.contains(Discontinuity::RATE_CHANGE)
    });
    assert_eq!(h.sample_rate_hz(), 2e6);
    let x = f32s(&buf);
    assert!((db(tone_power(&x, 400e3, 2e6)) - db(0.04)).abs() < 1.0);
    assert!(
        tone_power(&x, -800e3, 2e6) < 1e-4,
        "+1.2 MHz must not alias to -800 kHz"
    );
    // Time follows the counter at the new rate.
    let next = src.read_block_ci8(&mut buf).unwrap().unwrap();
    let dt = next.time.host_time.as_unix_nanos() - h.time.host_time.as_unix_nanos();
    assert!((dt as f64 - 16_384.0 / 2e6 * 1e9).abs() < 2.0);

    // Wider than the recording: 8 Msps. Both tones in place; beyond ±2 MHz is the floor.
    control.set_sample_rate(8e6).unwrap();
    let h = read_until(&mut src, &mut buf, |h| {
        h.discontinuity.contains(Discontinuity::RATE_CHANGE)
    });
    assert_eq!(
        Coverage::from_provenance(&h.provenance),
        Some(Coverage::Partial)
    );
    src.read_block_ci8(&mut buf).unwrap().unwrap();
    let x = f32s(&buf);
    for f in [400e3, 1.2e6] {
        assert!(
            (db(tone_power(&x, f, 8e6)) - db(0.04)).abs() < 1.0,
            "tone at {f}"
        );
    }
    // Noise fill at the recorded floor PSD between 2.4 and 3.6 MHz.
    let floor = driver.recording().floor_power;
    let probes: Vec<f64> = (0..30)
        .map(|k| tone_power(&x, 2.4e6 + 40e3 * k as f64, 8e6))
        .collect();
    let mean_probe = probes.iter().sum::<f64>() / probes.len() as f64;
    // A Hann-windowed probe of white noise of PSD N0 reads N0 · fs · 1.5 / N.
    let expected = floor / 4e6 * 8e6 * 1.5 / x.len() as f64;
    assert!(
        (db(mean_probe) - db(expected)).abs() < 2.0,
        "noise fill {mean_probe} vs {expected}"
    );
}

#[test]
fn a_gain_increase_clips_at_8_bits_with_overload_provenance() {
    let dir = Scratch::new("gain");
    let meta = Synth::new(Datatype::Cf32Le).write(&dir.0, "tone");
    let driver = MockSdrDriver::new(&meta, opts(8192)).unwrap();
    let mut src = driver.open_mock(&driver.default_request()).unwrap();
    let control = src.control();
    let mut buf = Vec::new();
    let h = src.read_block_ci8(&mut buf).unwrap().unwrap();
    assert!(!h.provenance.overload);
    let rms0 = mean_power(&f32s(&buf));

    // −20 dB: signal and floor both drop by 20 dB.
    control.set_gain("vga", 0.0).unwrap();
    let h = read_until(&mut src, &mut buf, |h| {
        h.discontinuity.contains(Discontinuity::GAIN_CHANGE)
    });
    assert_eq!(h.provenance.tune.vga_db, 0.0);
    assert!((db(mean_power(&f32s(&buf))) - db(rms0) + 20.0).abs() < 1.0);

    // +24 dB (LNA 40): the 0.25 tone reaches ~4 × full scale and saturates.
    control.set_gain("vga", 20.0).unwrap();
    control.set_gain("lna", 40.0).unwrap();
    let h = read_until(&mut src, &mut buf, |h| h.provenance.overload);
    assert!(buf.iter().any(|s| s.re == 127 || s.re == -128));
    assert!(buf.iter().all(|s| (-128..=127).contains(&s.re)));
    assert!(h.discontinuity.contains(Discontinuity::PROVENANCE_CHANGE));
    let stats = driver.last_control().unwrap().mock_stats();
    assert!(stats.clipped_components > 0 && stats.overload_blocks > 0);
    // Sticky until the next gain change.
    assert!(
        src.read_block_ci8(&mut buf)
            .unwrap()
            .unwrap()
            .provenance
            .overload
    );
    control.set_gain("lna", 16.0).unwrap();
    let h = read_until(&mut src, &mut buf, |h| {
        h.discontinuity.contains(Discontinuity::GAIN_CHANGE)
    });
    assert!(!h.provenance.overload, "a new gain state starts clean");
}

/// T-130: a recording without `hackriff:provenance` (unknown capture gains) is not boosted by the
/// working gain: at the scheduler's default (LNA 24 / VGA 20) nothing clips; +12 dB saturates.
#[test]
fn an_unrecorded_gain_recording_clips_only_when_really_overdriven() {
    let dir = Scratch::new("unrecorded-gain");
    let meta = Synth::new(Datatype::Ci8).write(&dir.0, "tone");
    let mut m = SigmfMeta::read(&meta).unwrap();
    m.global.provenance = None;
    m.write(&meta).unwrap();
    let driver = MockSdrDriver::new(&meta, opts(8192)).unwrap();
    let mut src = driver.open_mock(&driver.default_request()).unwrap();
    let control = src.control();
    assert!(!src.recording().gains_recorded);
    let mut buf = Vec::new();
    control
        .set_gains(&hk_core::Gains {
            lna_db: 24.0,
            vga_db: 20.0,
            amp_on: false,
        })
        .unwrap();
    for _ in 0..20 {
        let h = src.read_block_ci8(&mut buf).unwrap().unwrap();
        assert!(!h.provenance.overload, "the working gain does not overload");
    }
    let clipped = |b: &[Complex<i8>]| {
        b.iter()
            .filter(|s| s.re == 127 || s.re == -128 || s.im == 127 || s.im == -128)
            .count()
    };
    assert_eq!(clipped(&buf), 0);
    assert_eq!(
        driver
            .last_control()
            .unwrap()
            .mock_stats()
            .clipped_components,
        0
    );
    // +24 dB: the 0.25 tone reaches ≈4 × full scale.
    control
        .set_gains(&hk_core::Gains {
            lna_db: 40.0,
            vga_db: 28.0,
            amp_on: false,
        })
        .unwrap();
    let h = read_until(&mut src, &mut buf, |h| h.provenance.overload);
    assert_eq!(
        (h.provenance.tune.lna_db, h.provenance.tune.vga_db),
        (40.0, 28.0)
    );
    assert!(
        clipped(&buf) > 0,
        "≈4× overdrive saturates the 8-bit output"
    );
}

#[test]
fn a_gain_increase_clips_the_strong_fm_station() {
    let Some(meta) = fm_fixture() else { return };
    let driver = MockSdrDriver::new(&meta, opts(65_536)).unwrap();
    let mut src = driver.open_mock(&driver.default_request()).unwrap();
    let mut buf = Vec::new();
    let h = src.read_block_ci8(&mut buf).unwrap().unwrap();
    assert!(!h.provenance.overload, "recorded without overload");
    assert_eq!(
        (
            h.provenance.tune.lna_db,
            h.provenance.tune.vga_db,
            h.provenance.tune.amp_on
        ),
        (32.0, 30.0, true),
        "the recording's own gains"
    );
    src.control().set_gain("vga", 62.0).unwrap();
    let h = read_until(&mut src, &mut buf, |h| {
        h.discontinuity.contains(Discontinuity::GAIN_CHANGE)
    });
    let h = if h.provenance.overload {
        h
    } else {
        read_until(&mut src, &mut buf, |h| h.provenance.overload)
    };
    assert_eq!(h.provenance.tune.vga_db, 62.0);
    let clipped = buf
        .iter()
        .filter(|s| s.re == 127 || s.re == -128 || s.im == 127 || s.im == -128)
        .count();
    assert!(
        clipped * 1000 > buf.len(),
        "+32 dB clips the station: {clipped}"
    );
    assert!(driver.last_control().unwrap().mock_stats().overload_blocks > 0);
}

#[test]
fn an_injected_overrun_is_an_exact_gap_in_the_stats() {
    let dir = Scratch::new("overrun");
    let meta = Synth::new(Datatype::Ci8).write(&dir.0, "tone");
    let driver = MockSdrDriver::new(&meta, opts(4096)).unwrap();
    let mut src = driver.open_mock(&driver.default_request()).unwrap();
    let control = driver.last_control().unwrap();
    let mut buf = Vec::new();
    let first = src.read_block_ci8(&mut buf).unwrap().unwrap();
    assert!(first.discontinuity.contains(Discontinuity::STREAM_START));
    control.inject_overrun(12_345);
    let h = src.read_block_ci8(&mut buf).unwrap().unwrap();
    assert!(h.discontinuity.contains(Discontinuity::GAP));
    assert_eq!(h.dropped_before, 12_345);
    assert_eq!(h.first_sample(), 4096 + 12_345);
    let dt = h.time.host_time.as_unix_nanos() - first.time.host_time.as_unix_nanos();
    assert_eq!(dt, ((4096 + 12_345) as f64 * 1e9 / 1e6).round() as i64);
    let s = control.mock_stats().source;
    assert_eq!(
        (s.overruns, s.dropped_samples, s.discarded_samples),
        (1, 12_345, 0)
    );
    assert_eq!(src.control().stats(), Some(s));
}

#[test]
fn real_time_pacing_is_not_pausable_and_a_late_reader_loses_whole_blocks() {
    let dir = Scratch::new("late");
    let meta = Synth {
        fs: 250e3,
        ..Synth::new(Datatype::Ci8)
    }
    .write(&dir.0, "tone");
    let driver = MockSdrDriver::new(
        &meta,
        MockOptions {
            pacing: Pacing::RealTime { speed: 1.0 },
            queue_blocks: 2,
            ..opts(1024)
        },
    )
    .unwrap();
    let mut src = driver.open_mock(&driver.default_request()).unwrap();
    assert!(!src.pausable());
    let mut buf = Vec::new();
    src.read_block_ci8(&mut buf).unwrap().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let h = src.read_block_ci8(&mut buf).unwrap().unwrap();
    assert!(h.discontinuity.contains(Discontinuity::GAP));
    assert_eq!(h.dropped_before % 1024, 0, "whole blocks");
    let s = src.control().stats().unwrap();
    assert!(s.overruns >= 1 && s.dropped_samples == h.dropped_before);
}

#[test]
fn looping_splices_the_recording_with_a_gap_flag_and_a_continuing_counter() {
    let dir = Scratch::new("loop");
    let meta = Synth {
        secs: 0.02,
        ..Synth::new(Datatype::Ci8)
    }
    .write(&dir.0, "tone");
    let driver = MockSdrDriver::new(
        &meta,
        MockOptions {
            end: MockEnd::Loop,
            ..opts(4096)
        },
    )
    .unwrap();
    let mut src = driver.open_mock(&driver.default_request()).unwrap();
    let mut buf = Vec::new();
    let mut next = 0;
    let mut spliced = 0;
    for _ in 0..20 {
        let h = src.read_block_ci8(&mut buf).unwrap().unwrap();
        assert_eq!(h.first_sample(), next, "no samples lost at a splice");
        if h.discontinuity.contains(Discontinuity::GAP) {
            assert_eq!(h.dropped_before, 0);
            spliced += 1;
        }
        next += buf.len() as u64;
    }
    assert!(spliced >= 3, "20 000-sample recording, 80 000 samples read");
    assert_eq!(driver.last_control().unwrap().mock_stats().loops, spliced);
}

#[test]
fn open_requests_are_validated_against_the_capabilities() {
    let dir = Scratch::new("open");
    let meta = Synth::new(Datatype::Ci8).write(&dir.0, "tone");
    let driver = MockSdrDriver::new(&meta, opts(4096)).unwrap();
    assert_eq!(driver.name(), "mock");
    let caps = driver.capabilities();
    assert_eq!(caps.driver, "mock-sdr");
    assert!(!caps.tx_capable, "receive only");
    assert!(caps.sample_rates.supports(1e6) && caps.sample_rates.supports(20e6));
    let mut bad = driver.default_request();
    bad.center_hz = 7e9;
    assert!(driver.open(&bad).is_err());
    let mut bad = driver.default_request();
    bad.gains.push(hk_core::NamedGain::new("mixer", 3.0));
    assert!(driver.open(&bad).is_err());
    let mut ok = driver.default_request();
    ok.gains = vec![hk_core::NamedGain::new("lna", 30.0)];
    let src = driver.open_mock(&ok).unwrap();
    assert_eq!(
        src.provenance().tune.lna_db,
        24.0,
        "quantised down to the 8 dB step"
    );
    assert_eq!(src.provenance().device_id, "mock:synthetic:t-049");
    assert!(MockSdrDriver::new(dir.0.join("missing.sigmf-meta"), opts(4096)).is_err());
}

/// A two-window scene: window 1 is samples 0..1 s, window 2 (data samples 1 s..2 s) sits
/// `gap_s` later on the scene clock (`core:global_index`).
fn scene_recording(dir: &Path, gap_s: f64) -> (PathBuf, u64, u64) {
    let synth = Synth::new(Datatype::Ci8);
    let meta_path = synth.write(dir, "scene");
    let mut meta = SigmfMeta::read(&meta_path).unwrap();
    let one_s = synth.fs as u64;
    let gap = (gap_s * synth.fs) as u64;
    let mut second = meta.captures[0].clone();
    second.sample_start = one_s;
    second.datetime = None;
    second
        .extra
        .insert("core:global_index".into(), serde_json::json!(one_s + gap));
    meta.captures.push(second);
    meta.write(&meta_path).unwrap();
    (meta_path, one_s, gap)
}

/// T-125: a time-compressed scene. The recording gap between two windows is one `GAP` exactly at
/// the window boundary (no block straddles it, whatever the block length), stream time jumps by
/// the gap, and real-time pacing does not wait the hour out.
#[test]
fn scene_gaps_jump_stream_time_at_the_window_boundary() {
    let dir = Scratch::new("scene");
    let (meta, one_s, gap) = scene_recording(&dir.0, 3600.0);
    for pacing in [Pacing::Unpaced, Pacing::RealTime { speed: 4.0 }] {
        let driver = MockSdrDriver::new(
            &meta,
            MockOptions {
                block_len: 300_000,
                pacing,
                ..MockOptions::default()
            },
        )
        .unwrap();
        let mut src = driver.open_mock(&driver.default_request()).unwrap();
        let t0 = src.start_time().as_unix_nanos();
        let wall = Instant::now();
        let mut buf: Vec<Complex<i8>> = Vec::new();
        let mut end = 0u64;
        let mut gaps = Vec::new();
        for _ in 0..100 {
            let Some(h) = src.read_block_ci8(&mut buf).unwrap() else {
                break;
            };
            let ns = h.time.host_time.as_unix_nanos() - t0;
            assert_eq!(
                ns,
                (h.first_sample() as f64 * 1e9 / 1e6).round() as i64,
                "block time is the scene clock"
            );
            if h.discontinuity.contains(Discontinuity::GAP) {
                gaps.push((end, h.first_sample(), h.dropped_before));
            }
            end = h.first_sample() + buf.len() as u64;
        }
        assert_eq!(gaps, vec![(one_s, one_s + gap, gap)], "{pacing:?}");
        assert_eq!(
            end,
            2 * one_s + gap,
            "{pacing:?}: ends in the second window"
        );
        let stats = driver.last_control().unwrap().mock_stats();
        assert_eq!(stats.source.dropped_samples, gap);
        assert!(
            wall.elapsed() < Duration::from_secs(20),
            "{pacing:?}: the hour between windows is not waited out ({:?})",
            wall.elapsed()
        );
    }
}
