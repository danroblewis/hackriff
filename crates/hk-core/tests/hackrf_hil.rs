//! HIL smoke test (T5, T-037a): the live HackRF One source against real hardware. **Receive
//! only.** Ignored, so CI never needs a device; run manually after checking the device is free:
//!
//! ```text
//! hackrf_info
//! cargo test -p hk-core --features hackrf --test hackrf_hil -- --ignored --nocapture
//! ```
//!
//! 100.0–101.5 MHz broadcast FM (centre 100.75 MHz, 2 Msps), LNA 32 / VGA 30 / amp on, 3 s:
//! samples flow at the tuned rate, a mid-run retune reaches a block flagged `RETUNE` with the new
//! centre, drops are counted, and the device closes cleanly (it reopens right after).
#![cfg(feature = "hackrf")]

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use hk_core::{Discontinuity, Gains, HackRfConfig, HackRfSource, Source};

const FS: f64 = 2e6;

fn config() -> HackRfConfig {
    HackRfConfig {
        center_hz: 100.75e6,
        sample_rate_hz: FS,
        gains: Gains {
            lna_db: 32.0,
            vga_db: 30.0,
            amp_on: true,
        },
        ..HackRfConfig::default()
    }
}

#[test]
#[ignore = "needs a HackRF One (run with --ignored)"]
fn fm_band_samples_flow_retune_applies_and_the_device_closes() {
    let mut src = HackRfSource::open_with(&config()).expect("open the HackRF (is it free?)");
    let info = src.device_info().clone();
    eprintln!("device: {}", info.hw_description());
    assert!(!src.pausable());
    assert!(!src.bias_tee(), "bias tee off");
    let stats = src.stats();
    let control = src.control();
    let mut buf = Vec::new();
    let start = Instant::now();
    let watchdog = Duration::from_secs(15);
    let (mut samples, mut blocks, mut power, mut retuned_at) = (0u64, 0u64, 0f64, None);
    let mut first = None;
    let mut retuned = false;
    let mut next_index = 0u64;
    let mut gaps = 0u64;
    while start.elapsed() < Duration::from_secs(3) {
        assert!(start.elapsed() < watchdog, "watchdog");
        let h = src
            .read_block_ci8(&mut buf)
            .expect("read")
            .expect("stream open");
        let p = h.provenance.get();
        assert_eq!(p.device_id, info.device_id());
        assert_eq!(p.antenna_port.as_deref(), Some("unknown"));
        assert_eq!(
            (p.tune.lna_db, p.tune.vga_db, p.tune.amp_on),
            (32.0, 30.0, true)
        );
        if blocks == 0 {
            assert!(h.discontinuity.contains(Discontinuity::STREAM_START));
            first = Some(h.time.host_time);
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
        if !retuned && start.elapsed() > Duration::from_millis(1500) {
            control.tune(101.0e6).expect("retune");
            retuned = true;
        }
        if h.discontinuity.contains(Discontinuity::RETUNE) {
            assert_eq!(p.tune.center_hz, 101.0e6);
            retuned_at = Some(start.elapsed());
        } else if retuned_at.is_none() {
            assert_eq!(p.tune.center_hz, 100.75e6);
        } else {
            assert_eq!(p.tune.center_hz, 101.0e6);
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let rms = (power / (2.0 * samples as f64)).sqrt();
    let s = stats.to_json();
    eprintln!(
        "{blocks} blocks, {samples} samples in {elapsed:.2} s ({:.2} Msps), rms {rms:.1} codes, \
         gaps {gaps} samples, retune reached a block at {retuned_at:?}\nstats {s}",
        samples as f64 / elapsed / 1e6
    );
    assert!(first.is_some());
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
    let again = HackRfSource::open_with(&config()).expect("reopen after a clean close");
    assert_eq!(again.device_info().serial, info.serial);
    drop(again);
}
