//! T-891: the accessory-fed source (a VLF/LF receiver into a soundcard) behind the generic
//! device interface, and its SigMF mock. SPACE-001 / SPACE-041 / PROP-019 are `needs-accessory`:
//! the base HackRF cannot reach 16–30 kHz, and every block through the accessory says so in its
//! provenance.

use std::path::PathBuf;

use hk_core::source::conformance::{ConformanceSpec, run};
use hk_core::source::{AccessoryKind, AccessoryMockDriver, AccessoryMockOptions, write_real_sigmf};
use hk_core::{Discontinuity, SourceCapabilities, SourceDriver, SourceError};
use hk_model::{ClockSource, Timestamp, TimestampMethod};
use num_complex::Complex32;

const FS: f64 = 48_000.0;
/// 2026-09-25T12:00:00Z.
const T0_NS: i64 = 1_790_337_600_000_000_000;

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hk-accessory-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ramp(n: usize) -> Vec<f32> {
    (0..n).map(|i| ((i % 1000) as f32 / 1000.0) - 0.5).collect()
}

fn fixture(tag: &str, n: usize) -> (PathBuf, Vec<f32>) {
    let dir = temp_dir(tag);
    let meta = dir.join("vlf.sigmf-meta");
    let x = ramp(n);
    write_real_sigmf(&meta, &x, FS, "2026-09-25T12:00:00Z", "synthetic VLF", None).unwrap();
    (meta, x)
}

fn driver(meta: &PathBuf) -> AccessoryMockDriver {
    AccessoryMockDriver::new(
        meta,
        AccessoryMockOptions {
            block_len: 1000,
            ..AccessoryMockOptions::default()
        },
    )
    .unwrap()
}

#[test]
fn space_001_accessory_stream_says_it_came_through_the_accessory() {
    let (meta, x) = fixture("prov", 10_000);
    let d = driver(&meta);
    let mut src = d.open(&d.default_request()).unwrap();
    let ctl = src.control();
    let mut buf: Vec<Complex32> = Vec::new();
    let h = src.read_block(&mut buf).unwrap().unwrap();
    assert!(h.discontinuity.contains(Discontinuity::STREAM_START));
    assert_eq!(
        AccessoryKind::from_provenance(&h.provenance),
        Some(AccessoryKind::VlfReceiver)
    );
    assert!(
        h.provenance.device_id.starts_with("vlf-receiver:"),
        "{}",
        h.provenance.device_id
    );
    assert_eq!(
        h.provenance.antenna_port.as_deref(),
        Some("accessory:vlf-receiver")
    );
    assert_eq!(h.provenance.tune.center_hz, 0.0);
    assert_eq!(h.provenance.tune.sample_rate_hz, FS);
    assert_eq!(h.provenance.timestamp_method, TimestampMethod::Synthetic);
    let info = ctl.device_info().unwrap();
    assert_eq!(info.device_id, h.provenance.device_id);
    // Real samples: the recording, bit for bit, with im = 0.
    assert_eq!(buf.len(), 1000);
    assert!(buf.iter().zip(&x).all(|(c, r)| c.re == *r && c.im == 0.0));
    assert_eq!(h.time.host_time, Timestamp::from_unix_nanos(T0_NS));
    // Timestamps follow the counter.
    let h2 = src.read_block(&mut buf).unwrap().unwrap();
    assert_eq!(h2.first_sample(), 1000);
    assert_eq!(
        h2.time.host_time.as_unix_nanos() - T0_NS,
        1000 * 1_000_000_000 / 48_000
    );
    assert!(!h2.discontinuity.contains(Discontinuity::STREAM_START));
}

#[test]
fn space_041_overrun_is_an_exact_gap_counted_in_stats() {
    let (meta, x) = fixture("overrun", 10_000);
    let d = driver(&meta);
    let mut src = d.open(&d.default_request()).unwrap();
    let ctl = src.control();
    let mut buf = Vec::new();
    src.read_block(&mut buf).unwrap().unwrap();
    d.inject_overrun(2500);
    let h = src.read_block(&mut buf).unwrap().unwrap();
    assert!(h.discontinuity.contains(Discontinuity::GAP));
    assert_eq!(h.dropped_before, 2500);
    assert_eq!(h.first_sample(), 3500);
    assert_eq!(buf[0].re, x[3500]);
    assert_eq!(
        h.time.host_time.as_unix_nanos() - T0_NS,
        (3500.0 * 1e9 / FS).round() as i64
    );
    let st = ctl.stats().unwrap();
    assert_eq!(
        (st.overruns, st.dropped_samples, st.blocks, st.samples),
        (1, 2500, 2, 2000)
    );
}

#[test]
fn prop_019_controls_follow_the_contract_and_the_stream_ends() {
    let (meta, _) = fixture("controls", 3_000);
    let d = driver(&meta);
    let mut src = d.open(&d.default_request()).unwrap();
    let ctl = src.control();
    // Fixed at baseband: tune is the documented Unsupported, never a silent no-op.
    assert!(matches!(
        ctl.tune(20e3),
        Err(SourceError::Unsupported { .. })
    ));
    assert!(matches!(
        ctl.set_sample_rate(96_000.0),
        Err(SourceError::OutOfRange { .. })
    ));
    ctl.set_sample_rate(FS).unwrap();
    assert!(matches!(
        ctl.set_bias_tee(true),
        Err(SourceError::Unsupported { .. })
    ));
    let mut buf = Vec::new();
    let mut n = 0;
    while let Some(h) = src.read_block(&mut buf).unwrap() {
        assert!(!h.discontinuity.contains(Discontinuity::RATE_CHANGE));
        n += buf.len();
    }
    assert_eq!(n, 3_000, "a finite recording ends with Ok(None)");
    // Stop.
    let mut src = d.open(&d.default_request()).unwrap();
    src.control().stop().unwrap();
    assert!(src.read_block(&mut buf).unwrap().is_none());
    // A request off baseband or off the recording's rate is refused at open.
    let mut r = d.default_request();
    r.center_hz = 1e6;
    assert!(d.open(&r).is_err());
}

#[test]
fn the_base_hackrf_reaches_none_of_it_and_the_accessory_does() {
    let hackrf = SourceCapabilities::hackrf_one();
    let (meta, _) = fixture("caps", 2_000);
    let acc = driver(&meta).capabilities();
    for hz in [16e3, 20e3, 23.4e3] {
        assert!(
            !hackrf.supports_frequency(hz),
            "the HackRF must not claim {hz} Hz"
        );
        assert!(acc.supports_frequency(hz));
    }
    assert!(!acc.tx_capable && acc.gain_stages.is_empty() && !acc.bias_tee);
}

#[test]
fn a_gps_disciplined_recording_reports_its_clock() {
    let dir = temp_dir("gpsdo");
    let meta = dir.join("vlf.sigmf-meta");
    let p: hk_model::Provenance = serde_json::from_value(serde_json::json!({
        "device_id": "awesome-receiver",
        "tune": {"center_hz": 0.0, "sample_rate_hz": FS, "lna_db": 0.0, "vga_db": 0.0,
                 "amp_on": false, "bandwidth_hz": FS / 2.0},
        "overload": false, "quantisation_limited": false,
        "clock_source": "gpsdo", "clock_locked": true,
        "timestamp_method": "external-reference"
    }))
    .unwrap();
    write_real_sigmf(
        &meta,
        &ramp(2000),
        FS,
        "2026-09-25T12:00:00Z",
        "gpsdo",
        Some(p),
    )
    .unwrap();
    let d = driver(&meta);
    let mut src = d.open(&d.default_request()).unwrap();
    let mut buf = Vec::new();
    let h = src.read_block(&mut buf).unwrap().unwrap();
    assert_eq!(h.provenance.clock_source, ClockSource::Gpsdo);
    assert!(h.provenance.clock_locked);
    assert_eq!(
        h.provenance.timestamp_method,
        TimestampMethod::ExternalReference
    );
    // Still the accessory's identity, not the recording's.
    assert_eq!(
        AccessoryKind::from_provenance(&h.provenance),
        Some(AccessoryKind::VlfReceiver)
    );
}

/// The shared device conformance suite, minus the checks that need a local oscillator: a
/// fixed-baseband receiver answers `tune` with `Unsupported`, so the retune checks cannot apply.
#[test]
fn accessory_passes_the_device_conformance_checks_that_apply() {
    let (meta, _) = fixture("conformance", 48_000 * 4);
    let d = driver(&meta);
    let overrun = d.overrun_handle();
    let mut spec = ConformanceSpec::new(d.default_request(), 0.0);
    spec.expect_settle_discard = false;
    spec.exact_loss_accounting = true;
    spec.expect_pausable = Some(false);
    spec.inject_overrun = Some(Box::new(move || {
        overrun.fetch_add(500, std::sync::atomic::Ordering::SeqCst);
        500
    }));
    let report = run(&d as &dyn SourceDriver, &spec);
    let not_applicable = ["tune-out-of-range", "tune-in-range", "settle-discard"];
    let failed: Vec<_> = report
        .failures()
        .into_iter()
        .filter(|c| !not_applicable.contains(&c.name))
        .collect();
    assert!(failed.is_empty(), "{report}");
    for must in [
        "open",
        "first-block",
        "device-info",
        "timestamps",
        "counter",
        "overrun",
        "stats",
        "stop",
    ] {
        assert!(report.passed(must), "{must}: {report}");
    }
}
