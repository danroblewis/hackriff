//! T-331: a floor context belongs to one receive chain, and the bias tee is part of that chain.
//!
//! A bias tee powers an *external* LNA on the antenna port. Frames taken with it on and frames
//! taken with it off are two different receive chains, so pooling them into one floor context
//! averages two floors — the defect T-303 fixed one layer up for `BaselineKey`. Nothing else in
//! `GainKey` moves with the bias tee (`Tune`, and therefore LNA/VGA/amp, is byte-identical either
//! side of a switch; `antenna_port` is untouched; a bias-only change raises only
//! `PROVENANCE_CHANGE`, which is not in `FLOOR_RESET_ON`), so only the key can keep them apart.
//!
//! Two halves, because either alone is satisfiable by a broken tracker:
//!
//! - **The property** — frames either side of a switch do not pool, and `Unknown` pools with
//!   neither. Each test folds the **pooled key alongside as a live control**: the same PSD through
//!   a second tracker whose provenance never changes bias-tee state. The control must still show
//!   the bug (one segment, and the step confirmed as an air-side `Rise`), so the defect stays
//!   demonstrable in-test rather than merely absent.
//! - **The control** — a run with no bias-tee change produces exactly one segment and its floor
//!   still matures to the true level, so the fix cannot pass by having broken floor tracking.
//!
//! AWARE-006 (floor-change events); C12 occupancy/baseline.

mod common;
mod floor_common;

use common::provenance_with;
use floor_common::{GammaFrames, GammaPool};
use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::floor::{
    FloorConfig, FloorEvent, FloorEventKind, FloorKind, GainKey, NoiseFloorTracker,
};
use hk_model::{BiasTee, Provenance};

const AWARE_006: &str = "AWARE-006";

const BINS: usize = 1024;
const FS: f64 = 2.0e6;
const N_AVG: u32 = 10;
/// The floor step an active antenna's LNA puts on the chain, dB (T-303's control used 12 dB).
const LNA_STEP_DB: f64 = 12.0;

/// `provenance_with`'s record with `bias_tee` set. Everything else — device, tune, gains, ports —
/// is identical, which is the point: only the bias tee differs.
fn provenance_bias(bias: BiasTee) -> ProvenanceHandle {
    let base = provenance_with(1575.42e6, FS, 16.0);
    let mut p: Provenance = (*base).clone();
    p.bias_tee = bias;
    ProvenanceHandle::new(p)
}

/// One tracker fed a scripted sequence of (bias-tee state, floor level) stretches.
struct Run {
    tracker: NoiseFloorTracker,
    src: GammaFrames,
    pool: GammaPool,
    profile: Vec<f32>,
    events: Vec<FloorEvent>,
    segments: Vec<u64>,
    keys: Vec<GainKey>,
    last_slow_db: f64,
    last_ready: bool,
}

impl Run {
    fn new(seed: u64) -> Self {
        let p = provenance_bias(BiasTee::Off);
        Self {
            tracker: NoiseFloorTracker::new(FloorConfig::default()).unwrap(),
            src: GammaFrames::new(BINS, N_AVG, p, seed),
            pool: GammaPool::new(N_AVG, seed ^ 0x9e37_79b9),
            profile: vec![1.0; BINS],
            events: Vec::new(),
            segments: Vec::new(),
            keys: Vec::new(),
            last_slow_db: f64::NAN,
            last_ready: false,
        }
    }

    fn period(&self) -> f64 {
        self.src.frame_period_s()
    }

    /// Folds `secs` of flat noise at `level` (linear FS²/Hz) stamped with `bias`.
    fn stretch(&mut self, secs: f64, bias: BiasTee, level: f32) {
        self.src.provenance = provenance_bias(bias);
        self.profile.fill(level);
        let frames = (secs / self.period()).round() as usize;
        let mut frame = self.src.empty_frame();
        for _ in 0..frames {
            self.src.fill_pooled(
                &mut frame,
                &self.profile,
                Discontinuity::NONE,
                &mut self.pool,
            );
            let events = &mut self.events;
            let f = self.tracker.update(&frame, |e| events.push(e.clone()));
            self.segments.push(f.segment);
            self.keys.push(f.gain);
            self.last_slow_db = f64::from(f.band_floor_dbfs_per_hz(FloorKind::Slow));
            self.last_ready = f.slow_ready;
        }
    }

    fn rises(&self) -> usize {
        self.events
            .iter()
            .filter(|e| e.kind == FloorEventKind::Rise)
            .count()
    }

    fn distinct_segments(&self) -> usize {
        let mut s = self.segments.clone();
        s.dedup();
        s.len()
    }
}

/// The step an active antenna's LNA puts on the floor, as a linear profile level.
fn stepped(level: f32) -> f32 {
    level * 10f32.powf((LNA_STEP_DB / 10.0) as f32)
}

/// **The property.** A bias-tee switch is a context boundary: the step that comes with the LNA
/// starts a new segment and is never reported as an air-side floor rise. The pooled control —
/// the same PSD with the bias-tee state held constant — still shows the bug.
#[test]
fn t331_bias_tee_switch_does_not_pool_into_one_floor_context() {
    let mut keyed = Run::new(7);
    keyed.stretch(4.0, BiasTee::Off, 1.0);
    let before = keyed.distinct_segments();
    let pre_switch_db = keyed.last_slow_db;
    keyed.stretch(6.0, BiasTee::On, stepped(1.0));

    // Live control: identical PSD, identical everything, bias-tee state never changes.
    let mut pooled = Run::new(7);
    pooled.stretch(4.0, BiasTee::Off, 1.0);
    pooled.stretch(6.0, BiasTee::Off, stepped(1.0));

    assert_eq!(before, 1, "{AWARE_006}: the pre-switch run is one segment");
    assert_eq!(
        keyed.distinct_segments(),
        2,
        "{AWARE_006}: the bias-tee switch starts a new floor context ({:?})",
        keyed.keys.last().map(|k| k.bias_tee),
    );
    assert_eq!(
        keyed.rises(),
        0,
        "{AWARE_006}: the LNA's own step is a receiver change, not a floor rise; \
         the new segment seeds on post-switch frames",
    );

    // The control proves the bug is real and still reachable, not merely absent.
    assert_eq!(
        pooled.distinct_segments(),
        1,
        "control: with the bias tee pooled, the switch is invisible and the floors average",
    );
    assert!(
        pooled.rises() >= 1,
        "control: pooled, the LNA's {LNA_STEP_DB} dB step confirms as an air-side Rise \
         ({} events)",
        pooled.events.len(),
    );

    // The post-switch floor is the post-switch chain's floor, the full step above the pre-switch
    // one — not a mixture of the two. Measured against this run's own pre-switch floor, so the
    // assertion carries no assumption about the absolute scale.
    assert!(
        keyed.last_ready,
        "{AWARE_006}: the new context matures within the stretch",
    );
    let rise = keyed.last_slow_db - pre_switch_db;
    assert!(
        (rise - LNA_STEP_DB).abs() < 0.5,
        "{AWARE_006}: post-switch floor is {rise:.2} dB over the pre-switch floor; \
         it should be the powered chain's full {LNA_STEP_DB} dB, not a blend",
    );
}

/// **The property, `Unknown` half.** `Unknown` is its own context: it pools with neither `Off`
/// nor `On`, so a replay (which cannot report a bias tee) never inherits a measured device's
/// floor and never claims to be one. Same level throughout, so only the key can split these.
#[test]
fn t331_unknown_bias_tee_pools_with_neither_off_nor_on() {
    let mut run = Run::new(11);
    run.stretch(3.0, BiasTee::Off, 1.0);
    run.stretch(3.0, BiasTee::On, 1.0);
    run.stretch(3.0, BiasTee::Unknown, 1.0);
    run.stretch(3.0, BiasTee::Off, 1.0);

    assert_eq!(
        run.distinct_segments(),
        4,
        "{AWARE_006}: off → on → unknown → off is four contexts, not two and not one",
    );

    // And at the key level, with no tolerance anywhere to soften it.
    let tol = FS / BINS as f64;
    let key = |b| {
        let p = provenance_bias(b);
        let src = GammaFrames::new(BINS, N_AVG, p, 0);
        GainKey::of(&src.empty_frame())
    };
    let (off, on, unknown) = (key(BiasTee::Off), key(BiasTee::On), key(BiasTee::Unknown));
    assert!(!unknown.matches(&off, tol), "Unknown is never Off");
    assert!(!unknown.matches(&on, tol), "Unknown is never On");
    assert!(!off.matches(&on, tol), "Off is never On");
    assert!(off.matches(&off, tol), "a key matches itself");
    assert_eq!(off.bias_tee, BiasTee::Off, "the key carries what it read");
}

/// **The control.** No bias-tee change: one segment, and the floor still matures to the true
/// level with no spurious events. Without this, the property above passes just as well on a
/// tracker that resets on every frame.
#[test]
fn t331_unchanged_bias_tee_keeps_one_context_and_a_maturing_floor() {
    let mut run = Run::new(13);
    run.stretch(10.0, BiasTee::On, 1.0);

    assert_eq!(
        run.distinct_segments(),
        1,
        "{AWARE_006}: a steady bias-tee state is one context for the whole run",
    );
    assert_eq!(
        run.tracker.stats().resets,
        1,
        "{AWARE_006}: only the stream-start reset",
    );
    assert_eq!(run.rises(), 0, "{AWARE_006}: flat noise raises no rise");
    assert!(run.last_ready, "{AWARE_006}: the floor matured");

    // The profile level is the per-bin mean PSD, so a unit profile's floor is 0 dBFS/Hz.
    assert!(
        run.last_slow_db.abs() < 0.5,
        "{AWARE_006}: floor {:.2} dB should track the true 0 dB",
        run.last_slow_db,
    );
}
