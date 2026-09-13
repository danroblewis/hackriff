//! T-009: schedule steps applied through a SigMF replay's control (virtual tuning) land as
//! Retune / GainChange discontinuities exactly at block boundaries, with no torn blocks.

mod common;
mod sched_common;

use std::io::Cursor;

use common::{meta, provenance, ramp_ci8};
use hk_core::scheduler::{Poi, Purpose, Scheduler, SchedulerConfig, StepApplier};
use hk_core::{Discontinuity, Pacing, ReplayOptions, SigmfReplaySource, Source};
use hk_model::ScanPolicy;
use hk_model::sigmf::Datatype;
use num_complex::Complex;
use sched_common::*;
use serde_json::json;

const FS: f64 = 2e6;
const BLOCK: usize = 1024;
const BLOCKS_PER_STEP: usize = 3;
const STEPS: usize = 30;

#[test]
fn applied_steps_retune_a_virtual_replay_at_block_boundaries_without_torn_blocks() {
    let total = BLOCK * BLOCKS_PER_STEP * STEPS;
    let mut m = meta(Datatype::Ci8, FS);
    m.global.provenance = Some(provenance("synthetic:t-009-replay", 100e6, FS));
    let mut source = SigmfReplaySource::from_reader(
        m,
        Cursor::new(ramp_ci8(total)),
        ReplayOptions {
            block_len: BLOCK,
            pacing: Pacing::Unpaced,
        },
    )
    .unwrap()
    .with_virtual_tuning();
    let caps = source.capabilities().clone();

    // Replay capabilities: 99–101 MHz at 2 Msps only, no gain stages.
    let p = plan(
        "virtual replay",
        1,
        vec![region(99.1, 100.9, 1.0, None)],
        ScanPolicy::SweepThenDwell,
        vec![],
        json!({ "scheduler": { "sweep_rate_hz": FS, "dwell_min_rate_hz": FS, "sweeps_per_cycle": 2 } }),
    );
    let mut s =
        Scheduler::new(&p, SchedulerConfig::from_plan(&p).unwrap(), &caps, clock()).unwrap();
    s.offer_poi(Poi {
        key: 9,
        center_hz: 100.2e6,
        bandwidth_hz: 20e3,
        interestingness: 1.0,
        burst_interval_ns: None,
        verify: true,
    })
    .unwrap();

    let mut applier = StepApplier::new(source.control());
    let mut buf: Vec<Complex<i8>> = Vec::with_capacity(BLOCK);
    let mut expect_next = 0u64;
    let mut prev: Option<hk_model::Tune> = None;
    let (mut retunes, mut gain_changes) = (0, 0);
    let mut purposes = Vec::new();
    for _ in 0..STEPS {
        let step = s.next_step();
        s.clock().advance_ns(step.duration_ns);
        purposes.push(step.purpose);
        applier.apply(&step).unwrap();
        for b in 0..BLOCKS_PER_STEP {
            let h = source
                .read_block_ci8(&mut buf)
                .unwrap()
                .expect("data remains");
            assert_eq!(buf.len(), BLOCK, "no torn block");
            assert_eq!(h.first_sample(), expect_next, "contiguous sample counter");
            for (k, smp) in buf.iter().enumerate() {
                let v = ((expect_next as usize + k) % 256) as u8;
                assert_eq!((smp.re as u8, smp.im as u8), (v, v.wrapping_neg()));
            }
            expect_next += BLOCK as u64;

            let tune = h.provenance.tune.clone();
            assert_eq!(
                tune.center_hz, step.center_hz,
                "block carries the step's tune"
            );
            assert_eq!(
                (tune.lna_db, tune.vga_db, tune.amp_on),
                (step.gains.lna_db, step.gains.vga_db, step.gains.amp_on)
            );
            let flags = h.discontinuity;
            if b == 0 {
                match &prev {
                    None => assert!(flags.contains(Discontinuity::STREAM_START)),
                    Some(t) => {
                        let moved = t.center_hz != tune.center_hz;
                        let regained = (t.lna_db, t.vga_db, t.amp_on)
                            != (tune.lna_db, tune.vga_db, tune.amp_on);
                        assert_eq!(flags.contains(Discontinuity::RETUNE), moved, "{step:?}");
                        assert_eq!(flags.contains(Discontinuity::GAIN_CHANGE), regained);
                        assert!(!flags.contains(Discontinuity::RATE_CHANGE));
                        retunes += usize::from(moved);
                        gain_changes += usize::from(regained);
                    }
                }
            } else {
                assert_eq!(
                    flags,
                    Discontinuity::NONE,
                    "changes land on the first block only"
                );
            }
            prev = Some(tune);
        }
    }
    assert!(source.read_block_ci8(&mut buf).unwrap().is_none());
    assert!(purposes.iter().any(|p| matches!(p, Purpose::Retune { .. })));
    assert!(
        purposes
            .iter()
            .any(|p| matches!(p, Purpose::GainStep { .. }))
    );
    assert!(retunes >= 10, "retunes {retunes}");
    assert!(gain_changes >= 5, "gain changes {gain_changes}");
}

#[test]
fn virtual_tuning_is_opt_in_and_cannot_change_the_recorded_rate() {
    let mut m = meta(Datatype::Ci8, FS);
    m.global.provenance = Some(provenance("synthetic:t-009-replay", 100e6, FS));
    let plain =
        SigmfReplaySource::from_reader(m.clone(), Cursor::new(ramp_ci8(64)), Default::default())
            .unwrap();
    assert!(plain.control().tune(100.5e6).is_err());
    let virt = SigmfReplaySource::from_reader(m, Cursor::new(ramp_ci8(64)), Default::default())
        .unwrap()
        .with_virtual_tuning();
    let control = virt.control();
    assert!(control.tune(100.5e6).is_ok());
    assert!(control.set_sample_rate(FS).is_ok());
    assert!(control.set_sample_rate(4e6).is_err());
    assert!(control.set_bias_tee(true).is_err());
    assert!(!virt.capabilities().controllable);
}
