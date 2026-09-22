//! HIL smoke test (T5, T-514): the live RTL-SDR source against a real NooElec NESDR Nano 3.
//! **Receive only.** Ignored, so CI never needs a device; run manually after checking the device
//! is free:
//!
//! ```text
//! rtl_test -t
//! cargo test -p hk-core --features rtlsdr --test rtlsdr_hil -- --ignored --nocapture
//! ```
//!
//! The dongle is addressed **by serial** ([`SERIAL`], overridable with `HK_RTLSDR_SERIAL`), never
//! by USB index: an index changes when anything else is plugged in, so it could open a different
//! radio and file the measurements under the wrong provenance. One process per device; this test
//! never opens the HackRF.
//!
//! **The R820T needs ~10 s to lock on a cold open on this Mac** (librtlsdr prints
//! `[R82XX] PLL not locked!`). Both tests below allow for that: a slow first block is the
//! hardware warming up, not a failure.
#![cfg(feature = "rtlsdr")]

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use hk_core::source::conformance::{self, ConformanceSpec};
use hk_core::{
    Discontinuity, NamedGain, OpenRequest, RtlSdrConfig, RtlSdrDriver, RtlSdrSource, Source,
};

/// The NooElec NESDR Nano 3 this ticket was written against.
const SERIAL: &str = "7673444264";
/// 100.3 MHz: a broadcast-FM station verified present at this location.
const CENTER_HZ: f64 = 100.3e6;
const FS: f64 = 2.048e6;
/// A table gain with headroom for a strong local FM carrier.
const GAIN_DB: f64 = 28.0;

fn serial() -> String {
    std::env::var("HK_RTLSDR_SERIAL").unwrap_or_else(|_| SERIAL.to_owned())
}

fn config() -> RtlSdrConfig {
    RtlSdrConfig {
        serial: Some(serial()),
        center_hz: CENTER_HZ,
        sample_rate_hz: FS,
        tuner_gain_db: GAIN_DB,
        ..RtlSdrConfig::default()
    }
}

#[test]
#[ignore = "needs an RTL-SDR (run with --ignored)"]
fn fm_band_samples_flow_retune_applies_and_the_device_closes() {
    let mut src = RtlSdrSource::open_with(&config()).expect("open the RTL-SDR (is it free?)");
    let info = src.device_info().clone();
    eprintln!("device: {}", info.hw_description());
    assert_eq!(info.serial, serial(), "opened the dongle we asked for");
    assert_eq!(
        info.gains_db.len(),
        29,
        "the R820T reports 29 gain steps: {:?}",
        info.gains_db
    );
    assert!(!src.pausable());
    assert_eq!(
        src.bias_tee(),
        hk_model::BiasTee::Unknown,
        "this driver never drives the bias-tee pin, so it cannot claim `off`"
    );
    let stats = src.stats();
    let control = src.control();
    let mut buf = Vec::new();
    let start = Instant::now();
    // The PLL lock budget, then 3 s of streaming.
    let watchdog = Duration::from_secs(40);
    let (mut samples, mut blocks, mut power, mut retuned_at) = (0u64, 0u64, 0f64, None);
    let mut first_at = None;
    let mut retuned = false;
    let mut next_index = 0u64;
    let mut gaps = 0u64;
    let mut streaming_since = None;
    loop {
        assert!(start.elapsed() < watchdog, "watchdog");
        let h = src
            .read_block_ci8(&mut buf)
            .expect("read")
            .expect("stream open");
        let p = h.provenance.get();
        assert_eq!(p.device_id, format!("rtlsdr:{}", serial()));
        assert_eq!(p.antenna_port.as_deref(), Some("unknown"));
        assert_eq!(p.bias_tee, hk_model::BiasTee::Unknown);
        assert_eq!(
            (p.tune.lna_db, p.tune.vga_db, p.tune.amp_on),
            (GAIN_DB, 0.0, false)
        );
        assert_eq!(p.tune.sample_rate_hz, FS, "the rate is exactly realisable");
        if blocks == 0 {
            assert!(h.discontinuity.contains(Discontinuity::STREAM_START));
            first_at = Some(start.elapsed());
            streaming_since = Some(Instant::now());
        } else {
            assert!(
                h.first_sample() >= next_index,
                "the counter never goes back"
            );
            gaps += h.dropped_before;
        }
        next_index = h.first_sample() + buf.len() as u64;
        samples += buf.len() as u64;
        blocks += 1;
        power += buf
            .iter()
            .map(|s| f64::from(s.re).powi(2) + f64::from(s.im).powi(2))
            .sum::<f64>();
        let streamed = streaming_since.expect("set on the first block").elapsed();
        if !retuned && streamed > Duration::from_millis(1500) {
            control.tune(CENTER_HZ + 700e3).expect("retune");
            retuned = true;
        }
        if h.discontinuity.contains(Discontinuity::RETUNE) {
            assert_eq!(p.tune.center_hz, CENTER_HZ + 700e3);
            retuned_at = Some(streamed);
        } else if retuned_at.is_none() {
            assert_eq!(p.tune.center_hz, CENTER_HZ);
        } else {
            assert_eq!(p.tune.center_hz, CENTER_HZ + 700e3);
        }
        if streamed > Duration::from_secs(3) && retuned_at.is_some() {
            break;
        }
    }
    let elapsed = streaming_since.unwrap().elapsed().as_secs_f64();
    let rms = (power / (2.0 * samples as f64)).sqrt();
    let s = stats.to_json();
    eprintln!(
        "first block after {first_at:?} (R820T lock); {blocks} blocks, {samples} samples in \
         {elapsed:.2} s ({:.2} Msps), rms {rms:.1} codes, gaps {gaps} samples, retune reached a \
         block at {retuned_at:?}\nstats {s}",
        samples as f64 / elapsed / 1e6
    );
    assert!(first_at.is_some());
    assert!(
        samples as f64 > 0.5 * FS * elapsed,
        "samples flow near the tuned rate"
    );
    assert!(rms > 0.5, "the ADC sees something (rms {rms})");
    assert!(retuned_at.is_some(), "the retune reached a block");
    assert_eq!(
        gaps,
        stats.settle_discarded_samples.load(Ordering::Relaxed)
            + stats.transfers.dropped_samples.load(Ordering::Relaxed),
        "gaps are exactly settle discards plus counted drops"
    );
    control.stop().unwrap();
    assert!(src.read_block_ci8(&mut buf).unwrap().is_none(), "stopped");
    drop(src);
    // A clean close releases the device: it opens again at once.
    let again = RtlSdrSource::open_with(&config()).expect("reopen after a clean close");
    assert_eq!(again.device_info().serial, info.serial);
    drop(again);
}

#[test]
#[ignore = "needs an RTL-SDR (run with --ignored)"]
fn the_nano_3_passes_the_device_conformance_suite() {
    let request = OpenRequest {
        device: Some(serial()),
        center_hz: CENTER_HZ,
        sample_rate_hz: FS,
        gains: vec![NamedGain::new("lna", GAIN_DB)],
        baseband_filter_hz: None,
        bias_tee: false,
    };
    let mut spec = ConformanceSpec::new(request, CENTER_HZ + 700e3);
    // Faster than the opening rate, so the re-anchored block time lands later, never earlier.
    spec.alt_rate_hz = Some(2.4e6);
    spec.expect_pausable = Some(false);
    spec.warmup_blocks = 4;
    // Block times are extrapolated from the sample counter, so they are exact by construction —
    // the suite's default 1 µs tolerance holds even across the rate change, where the stream
    // re-anchors: the check skips the RATE_CHANGE block itself and the blocks either side of it
    // are extrapolated from the same anchor.
    // The R820T's ~10 s lock is inside this budget.
    spec.deadline = Duration::from_secs(180);
    let report = conformance::run(&RtlSdrDriver, &spec);
    eprintln!("{report}");
    report.assert_passed();
}
