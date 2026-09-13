//! T-005 floor-change episodes (AWARE-006) under adversarial sequences: one named test per
//! re-review repro against 854614c, plus randomised sequences of rises, falls, overlapping
//! regions, ramps, resets, gain changes, background drift and impulsive bursts, checked against
//! the event-model invariants (`hk_dsp::floor`, "Floor-change events"):
//! - no Extend/Update/End/Unknown without a Rise, nothing after a close, one Rise per episode;
//! - every Rise closes (End or Unknown) once the scene is quiet long enough;
//! - extents are consistent (Extend adds inside the new extent, Update removes from the old);
//! - open episodes never touch (one physical event, one episode);
//! - no block's slow floor jumps by the change threshold without an event covering it.

mod common;
mod floor_common;

use std::collections::HashMap;
use std::ops::Range;

use common::*;
use floor_common::*;
use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::SpectrumFrame;
use hk_dsp::floor::{
    EndReason, FloorChangeClass, FloorChangeConfig, FloorConfig, FloorEvent, FloorEventKind,
    FloorKind, ImpulsiveGateConfig, NoiseFloorTracker, SlowFloorConfig,
};
use hk_dsp::synth::Rng;
use hk_model::SampleTime;

use FloorEventKind::{Extend, Fall, Rise, Unknown, Update};

const AWARE_006: &str = "AWARE-006";

struct Rec {
    seq: u64,
    reset: bool,
    ready: bool,
    active: u32,
    slow_band_db: f64,
    block_slow: Vec<f32>,
}

struct Sim {
    src: GammaFrames,
    pool: GammaPool,
    tracker: NoiseFloorTracker,
    frame: SpectrumFrame,
    profile: Vec<f32>,
    sk: bool,
    /// Per-bin SK for every frame (overrides `sk`).
    sk_values: Option<Vec<f32>>,
    events: Vec<FloorEvent>,
    recs: Vec<Rec>,
    low: ProvenanceHandle,
    high: ProvenanceHandle,
}

impl Sim {
    fn new(bins: usize, fs: f64, cfg: FloorConfig, seed: u64) -> Self {
        let low = provenance_with(1575.42e6, fs, 16.0);
        let high = provenance_with(1575.42e6, fs, 24.0);
        let src = GammaFrames::new(bins, 10, low.clone(), seed);
        Self {
            pool: GammaPool::new(10, seed ^ 0x9e37_79b9),
            tracker: NoiseFloorTracker::new(cfg).unwrap(),
            frame: src.empty_frame(),
            profile: vec![1.0; bins],
            sk: false,
            sk_values: None,
            events: Vec::new(),
            recs: Vec::new(),
            src,
            low,
            high,
        }
    }

    fn period(&self) -> f64 {
        self.src.frame_period_s()
    }

    fn t(&self) -> f64 {
        self.src.seq as f64 * self.period()
    }

    /// Runs frames starting before `t_end`. `scene(t, profile)` scales a unit profile and returns
    /// the frame's discontinuity flags and whether the receiver is at high gain.
    fn run(&mut self, t_end: f64, mut scene: impl FnMut(f64, &mut [f32]) -> (Discontinuity, bool)) {
        while self.t() < t_end - 1e-9 {
            let t = self.t();
            self.profile.fill(1.0);
            let (flags, high) = scene(t, &mut self.profile);
            self.src.provenance = if high {
                self.high.clone()
            } else {
                self.low.clone()
            };
            self.src
                .fill_pooled(&mut self.frame, &self.profile, flags, &mut self.pool);
            if let Some(v) = &self.sk_values {
                self.frame.spectrum.sk.clone_from(v);
            } else if self.sk {
                self.frame.spectrum.sk.resize(self.profile.len(), 1.0);
            }
            let events = &mut self.events;
            let f = self.tracker.update(&self.frame, |e| events.push(e.clone()));
            self.recs.push(Rec {
                seq: f.seq,
                reset: f.reset,
                ready: f.slow_ready,
                active: f.active_episodes,
                slow_band_db: f64::from(f.band_floor_dbfs_per_hz(FloorKind::Slow)),
                block_slow: f.block_slow.clone(),
            });
        }
    }

    fn secs(&self, t: SampleTime) -> f64 {
        t.sample_index as f64 / self.src.fs
    }

    fn seq_at(&self, t: f64) -> u64 {
        (t / self.period() - 1e-9).ceil() as u64
    }

    /// Slow floor near `bin` at frame `seq` (nearest block), dB.
    fn slow_db(&self, seq: u64, bin: usize) -> f64 {
        let layout = self.tracker.layout().unwrap();
        let b = (0..layout.count())
            .min_by(|&a, &b| {
                (layout.centre(a) - bin as f64)
                    .abs()
                    .total_cmp(&(layout.centre(b) - bin as f64).abs())
            })
            .unwrap();
        db32(self.recs[seq as usize].block_slow[b])
    }

    fn show(&self, name: &str) {
        eprintln!("{name}: {} events", self.events.len());
        for e in &self.events {
            eprintln!(
                "  {:?} #{} {:?}{}{}{} onset {:.3} s confirmed {:.3} s bins {:?} change {:?} step {:+.2} dB baseline {:.2}",
                e.kind,
                e.episode,
                e.class,
                e.end_reason.map_or(String::new(), |r| format!(" {r:?}")),
                e.merged_into
                    .map_or(String::new(), |m| format!(" into #{m}")),
                if e.interrupted { " interrupted" } else { "" },
                self.secs(e.onset_t),
                self.secs(e.confirmed_t),
                e.bins,
                e.change_bins,
                e.step_db,
                e.baseline_dbfs_per_hz
            );
        }
    }

    fn kinds(&self) -> Vec<FloorEventKind> {
        self.events.iter().map(|e| e.kind).collect()
    }
}

fn scale(p: &mut [f32], r: Range<usize>, db: f64) {
    let g = 10f64.powf(db / 10.0) as f32;
    for v in &mut p[r] {
        *v *= g;
    }
}

fn quiet() -> (Discontinuity, bool) {
    (Discontinuity::NONE, false)
}

fn near(a: usize, b: usize, tol: usize) -> bool {
    a.abs_diff(b) <= tol
}

fn contains(outer: &Range<usize>, inner: &Range<usize>) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

/// The event-model invariants over a whole run. With `closed`, every Rise must have closed.
fn check_invariants(sim: &Sim, cfg: &FloorConfig, closed: bool) {
    let bins = sim.profile.len();
    let layout = sim.tracker.layout().unwrap();
    #[derive(PartialEq)]
    enum State {
        Open,
        Closed,
    }
    let mut state: HashMap<u64, State> = HashMap::new();
    let mut open: HashMap<u64, Range<usize>> = HashMap::new();
    let mut by_seq: HashMap<u64, Vec<&FloorEvent>> = HashMap::new();
    let mut i = 0;
    while i < sim.events.len() {
        let seq = sim.events[i].confirmed_seq;
        while i < sim.events.len() && sim.events[i].confirmed_seq == seq {
            let e = &sim.events[i];
            by_seq.entry(seq).or_default().push(e);
            let ctx = || format!("{AWARE_006}: event {i} {e:?}");
            assert!(e.bins.start < e.bins.end && e.bins.end <= bins, "{}", ctx());
            assert!(e.change_bins.start < e.change_bins.end, "{}", ctx());
            assert!(
                e.band_fraction > 0.0
                    && e.band_fraction <= e.bins.len() as f32 / bins as f32 + 1e-6,
                "{}",
                ctx()
            );
            match e.kind {
                Rise | Fall => {
                    assert!(
                        !state.contains_key(&e.episode),
                        "duplicate opening: {}",
                        ctx()
                    );
                    assert_eq!(e.change_bins, e.bins, "{}", ctx());
                    if let Some(parent) = e.split_from {
                        assert!(
                            state.get(&parent) == Some(&State::Open)
                                && contains(&open[&parent], &e.bins),
                            "split outside its open parent: {}",
                            ctx()
                        );
                    }
                    if e.kind == Rise {
                        state.insert(e.episode, State::Open);
                        open.insert(e.episode, e.bins.clone());
                    } else {
                        state.insert(e.episode, State::Closed);
                    }
                }
                Extend | Update | FloorEventKind::End | Unknown => {
                    assert!(
                        state.get(&e.episode) == Some(&State::Open),
                        "{:?} without an open Rise: {}",
                        e.kind,
                        ctx()
                    );
                    let before = open[&e.episode].clone();
                    match e.kind {
                        Extend => {
                            assert!(contains(&e.bins, &e.change_bins), "{}", ctx());
                            assert!(contains(&e.bins, &before), "{}", ctx());
                        }
                        Update => {
                            assert!(contains(&before, &e.change_bins), "{}", ctx());
                            assert!(contains(&before, &e.bins), "{}", ctx());
                        }
                        _ => assert_eq!(e.bins, before, "{}", ctx()),
                    }
                    if matches!(e.kind, FloorEventKind::End | Unknown) {
                        state.insert(e.episode, State::Closed);
                        open.remove(&e.episode);
                        if e.end_reason == Some(EndReason::Merged) {
                            let into = e.merged_into.expect("merged_into");
                            assert!(
                                state.get(&into) == Some(&State::Open),
                                "merged into a closed episode: {}",
                                ctx()
                            );
                        }
                    } else {
                        open.insert(e.episode, e.bins.clone());
                    }
                }
            }
            i += 1;
        }
        // One physical event, one episode: open extents never touch.
        let spans: Vec<_> = open.values().cloned().collect();
        for (a, x) in spans.iter().enumerate() {
            for y in &spans[a + 1..] {
                assert!(
                    x.end < y.start || y.end < x.start,
                    "{AWARE_006}: open episodes touch at frame {seq}: {x:?} / {y:?}"
                );
            }
        }
    }
    if closed {
        assert!(
            open.is_empty(),
            "{AWARE_006}: episodes never closed: {open:?}"
        );
    }
    // No silent re-seed: a slow-floor jump of the change threshold has an event over the block.
    let thr = cfg.change.threshold_db;
    for w in sim.recs.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        if !(a.ready && b.ready) || b.reset {
            continue;
        }
        for (k, (&x, &y)) in a.block_slow.iter().zip(&b.block_slow).enumerate() {
            let jump = (db32(y) - db32(x)).abs();
            if jump < thr - 0.01 {
                continue;
            }
            let centre = layout.centre(k).round() as usize;
            let covered = by_seq.get(&b.seq).is_some_and(|evs| {
                evs.iter()
                    .any(|e| e.change_bins.contains(&centre) || e.bins.contains(&centre))
            });
            assert!(
                covered,
                "{AWARE_006}: silent slow-floor re-seed of block {k} by {jump:+.2} dB at frame {}",
                b.seq
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Named repros (4096 bins at 4 Msps: 10.24 ms frames; 1024 bins at 1 Msps: 10.24 ms frames).

#[test]
fn repro2_sub_band_fall_emits_one_fall() {
    let cfg = FloorConfig::default();
    let mut sim = Sim::new(4096, 4e6, cfg, 2);
    sim.run(6.0, |t, p| {
        if t >= 2.0 {
            scale(p, 1000..2000, -6.0);
        }
        quiet()
    });
    sim.show("sub-band −6 dB fall on bins 1000..2000");
    check_invariants(&sim, &cfg, true);
    assert_eq!(sim.kinds(), [Fall], "{AWARE_006}");
    let e = &sim.events[0];
    assert!(!e.interrupted, "a clean fall is not interrupted");
    assert!(near(e.bins.start, 1000, 128) && near(e.bins.end, 2000, 128));
    assert!((e.step_db + 6.0).abs() < 0.5, "step {}", e.step_db);
    assert!((sim.secs(e.onset_t) - 2.0).abs() <= sim.period());
}

#[test]
fn repro3_end_waits_until_every_block_returned() {
    let cfg = FloorConfig::default();
    let mut sim = Sim::new(4096, 4e6, cfg, 3);
    // Noise-like (SK = 1): only a verified noise rise moves the slow floor (T-029).
    sim.sk = true;
    sim.run(14.0, |t, p| {
        if (1.0..5.1).contains(&t) {
            scale(p, 1000..2300, 6.0);
        }
        if (1.0..9.0).contains(&t) {
            scale(p, 2300..3000, 6.0);
        }
        quiet()
    });
    sim.show("+6 dB on 1000..3000; 1000..2300 off at 5.1 s, 2300..3000 off at 9 s");
    check_invariants(&sim, &cfg, true);
    assert_eq!(
        sim.kinds(),
        [Rise, Update, FloorEventKind::End],
        "{AWARE_006}"
    );
    let (rise, update, end) = (&sim.events[0], &sim.events[1], &sim.events[2]);
    assert!(near(rise.bins.start, 1000, 128) && near(rise.bins.end, 3000, 128));
    assert!((sim.secs(update.onset_t) - 5.1).abs() <= 2.0 * sim.period());
    assert!(near(update.bins.start, 2300, 128) && near(update.bins.end, 3000, 128));
    assert!(near(update.change_bins.start, 1000, 128) && near(update.change_bins.end, 2300, 128));
    assert!((sim.secs(end.onset_t) - 9.0).abs() <= 2.0 * sim.period());
    assert_eq!(end.end_reason, Some(EndReason::Returned));
    // Between the Update and the End: the still-elevated blocks keep their elevated slow floor,
    // the returned ones are back at the floor.
    let s = sim.seq_at(8.5);
    let (inside, returned) = (sim.slow_db(s, 2650), sim.slow_db(s, 1600));
    eprintln!(
        "slow floor at 8.5 s: elevated part {inside:+.2} dB, returned part {returned:+.2} dB"
    );
    assert!((inside - 6.0).abs() < 0.5 && returned.abs() < 0.5);
}

#[test]
fn repro4_overlapping_rise_extends_the_episode() {
    let cfg = FloorConfig::default();
    let mut sim = Sim::new(4096, 4e6, cfg, 4);
    sim.run(13.0, |t, p| {
        if (1.0..8.0).contains(&t) {
            scale(p, 800..1800, 6.0);
        }
        if (4.1..8.0).contains(&t) {
            scale(p, 1600..3000, 6.0);
        }
        quiet()
    });
    sim.show("A 800..1800 at 1 s, B 1600..3000 at 4.1 s, both off at 8 s");
    check_invariants(&sim, &cfg, true);
    assert_eq!(
        sim.kinds(),
        [Rise, Extend, FloorEventKind::End],
        "{AWARE_006}"
    );
    let (rise, extend, end) = (&sim.events[0], &sim.events[1], &sim.events[2]);
    assert!(rise.episode == extend.episode && extend.episode == end.episode);
    assert!((sim.secs(extend.onset_t) - 4.1).abs() <= 2.0 * sim.period());
    assert!(near(extend.change_bins.end, 3000, 128) && extend.change_bins.start >= 1600);
    assert!(near(extend.bins.start, 800, 128) && near(extend.bins.end, 3000, 128));
    assert!((sim.secs(extend.episode_onset_t) - 1.0).abs() <= sim.period());
}

#[test]
fn repro4_wider_rise_during_holdoff_is_one_episode() {
    let cfg = FloorConfig::default();
    let mut sim = Sim::new(4096, 4e6, cfg, 41);
    sim.run(12.0, |t, p| {
        if (1.0..4.0).contains(&t) {
            scale(p, 1500..2500, 6.0);
        }
        if (5.5..9.0).contains(&t) {
            scale(p, 1000..3000, 6.0);
        }
        quiet()
    });
    sim.show("1500..2500 at 1–4 s, then 1000..3000 at 5.5 s inside the hold-off");
    check_invariants(&sim, &cfg, true);
    assert_eq!(
        sim.kinds(),
        [Rise, FloorEventKind::End, Rise, FloorEventKind::End],
        "{AWARE_006}"
    );
    let second = &sim.events[2];
    assert!((sim.secs(second.onset_t) - 5.5).abs() <= 2.0 * sim.period());
    assert!(near(second.bins.start, 1000, 128) && near(second.bins.end, 3000, 128));
}

#[test]
fn repro4_bridging_rise_merges_two_episodes() {
    let cfg = FloorConfig::default();
    let mut sim = Sim::new(4096, 4e6, cfg, 42);
    sim.run(10.0, |t, p| {
        if (1.0..6.0).contains(&t) {
            scale(p, 500..1200, 6.0);
            scale(p, 2000..2800, 6.0);
        }
        if (3.0..6.0).contains(&t) {
            scale(p, 1100..2100, 6.0);
        }
        quiet()
    });
    sim.show("500..1200 and 2000..2800 at 1 s, bridged by 1100..2100 at 3 s");
    check_invariants(&sim, &cfg, true);
    assert_eq!(
        sim.kinds(),
        [Rise, Rise, FloorEventKind::End, Extend, FloorEventKind::End],
        "{AWARE_006}"
    );
    let merged = &sim.events[2];
    assert_eq!(merged.end_reason, Some(EndReason::Merged));
    assert_eq!(merged.merged_into, Some(sim.events[0].episode));
    assert!(near(sim.events[3].bins.start, 500, 128) && near(sim.events[3].bins.end, 2800, 128));
    assert_eq!(sim.events[4].episode, sim.events[0].episode);
}

#[test]
fn repro5_onset_ramps_are_noise_like() {
    let cfg = FloorConfig::default();
    for (k, ramp) in [0.1, 0.3, 1.0, 3.0].into_iter().enumerate() {
        let mut sim = Sim::new(1024, 1e6, cfg, 50 + k as u64);
        sim.sk = true;
        sim.run(1.0 + ramp + 2.5, |t, p| {
            let x = ((t - 1.0) / ramp).clamp(0.0, 1.0);
            if x > 0.0 {
                scale(p, 0..1024, 20.0 * x);
            }
            quiet()
        });
        sim.show(&format!("0 → +20 dB full-band ramp over {ramp} s"));
        check_invariants(&sim, &cfg, false);
        assert_eq!(
            sim.kinds(),
            [Rise],
            "{AWARE_006}: one Rise for a {ramp} s ramp"
        );
        let e = &sim.events[0];
        eprintln!(
            "  class {:?}, excess std {:.2} dB, onset {:.3} s",
            e.class,
            e.excess_std_db,
            sim.secs(e.onset_t)
        );
        assert_eq!(
            e.class,
            FloorChangeClass::NoiseLike,
            "{AWARE_006}: {ramp} s ramp"
        );
        // Onset: the frame the ramp passes the threshold (3 dB of 20 dB).
        let crossing = 1.0 + ramp * 3.0 / 20.0;
        assert!((sim.secs(e.onset_t) - crossing).abs() <= 2.0 * sim.period() + 0.05 * ramp);
    }
}

#[test]
fn repro6_episode_ends_despite_background_drift() {
    let cfg = FloorConfig::default();
    for (k, drift) in [1.6, 2.5].into_iter().enumerate() {
        let mut sim = Sim::new(1024, 1e6, cfg, 60 + k as u64);
        sim.run(28.0, |t, p| {
            scale(p, 0..1024, drift * ((t - 1.0) / 19.0).clamp(0.0, 1.0));
            if (1.0..20.0).contains(&t) || (25.0..28.0).contains(&t) {
                scale(p, 300..700, 4.0);
            }
            quiet()
        });
        sim.show(&format!(
            "+4 dB on 300..700 at 1–20 s over a +{drift} dB background drift, again at 25 s"
        ));
        check_invariants(&sim, &cfg, false);
        assert_eq!(
            sim.kinds(),
            [Rise, FloorEventKind::End, Rise],
            "{AWARE_006}"
        );
        let end = &sim.events[1];
        assert_eq!(end.end_reason, Some(EndReason::Returned));
        assert!((sim.secs(end.onset_t) - 20.0).abs() <= 2.0 * sim.period());
        assert!((sim.secs(sim.events[2].onset_t) - 25.0).abs() <= 2.0 * sim.period());
    }
}

#[test]
fn repro7_impulsive_bursts_do_not_corrupt_the_slow_floor() {
    let cfg = FloorConfig::default();
    let cases: [(&str, Range<usize>, f64, f64, f64); 7] = [
        ("band +1 dB 150 ms 5 %", 0..1024, 1.0, 0.15, 0.05),
        ("band +2 dB 300 ms 10 %", 0..1024, 2.0, 0.3, 0.10),
        ("band +3 dB 150 ms 20 %", 0..1024, 3.0, 0.15, 0.20),
        ("band +3 dB 300 ms 20 %", 0..1024, 3.0, 0.3, 0.20),
        ("band +1 dB 300 ms 20 %", 0..1024, 1.0, 0.3, 0.20),
        ("40 % band +2 dB 300 ms 20 %", 0..410, 2.0, 0.3, 0.20),
        ("40 % band +3 dB 150 ms 10 %", 300..710, 3.0, 0.15, 0.10),
    ];
    for (k, (name, region, level, on, duty)) in cases.into_iter().enumerate() {
        let mut sim = Sim::new(1024, 1e6, cfg, 70 + k as u64);
        let period = on / duty;
        sim.run(20.0, |t, p| {
            if t >= 1.0 && (t - 1.0) % period < on {
                scale(p, region.clone(), level);
            }
            quiet()
        });
        let mid = (region.start + region.end) / 2;
        let mut errs: Vec<f64> = sim
            .recs
            .iter()
            .filter(|r| r.ready)
            .map(|r| r.slow_band_db)
            .collect();
        let mut local: Vec<f64> = (sim.seq_at(1.0)..sim.recs.len() as u64)
            .map(|s| sim.slow_db(s, mid))
            .collect();
        let stats = |v: &mut Vec<f64>| {
            let m = v.iter().sum::<f64>() / v.len() as f64;
            v.sort_by(f64::total_cmp);
            (m, v[v.len() * 95 / 100], v[v.len() - 1])
        };
        let (bm, bp95, bmax) = stats(&mut errs);
        let (lm, lp95, lmax) = stats(&mut local);
        eprintln!(
            "{name}: slow band floor error mean {bm:+.3} dB p95 {bp95:+.3} max {bmax:+.3}; under the bursts mean {lm:+.3} p95 {lp95:+.3} max {lmax:+.3}; {} events",
            sim.events.len()
        );
        assert!(
            sim.events.is_empty(),
            "{AWARE_006}: {name}: {:?}",
            sim.kinds()
        );
        for (m, p95) in [(bm, bp95), (lm, lp95)] {
            assert!(
                m.abs() <= 0.2 && p95 <= 0.5,
                "{name}: slow floor error mean {m:+.3} dB, p95 {p95:+.3} dB"
            );
        }
    }
}

#[test]
fn repro8_slow_and_fast_full_band_ramps_are_one_episode() {
    let cfg = FloorConfig::default();
    for (k, ramp) in [10.0, 1.0].into_iter().enumerate() {
        let mut sim = Sim::new(1024, 1e6, cfg, 80 + k as u64);
        // Noise-like (SK = 1): only a verified noise rise moves the slow floor (T-029).
        sim.sk = true;
        sim.run(1.0 + ramp + 5.0, |t, p| {
            scale(p, 0..1024, 20.0 * ((t - 1.0) / ramp).clamp(0.0, 1.0));
            quiet()
        });
        sim.show(&format!("0 → +20 dB full-band ramp over {ramp} s"));
        check_invariants(&sim, &cfg, false);
        assert_eq!(
            sim.kinds(),
            [Rise],
            "{AWARE_006}: one Rise for a {ramp} s ramp"
        );
        let onset = sim.secs(sim.events[0].onset_t);
        assert!((1.0..1.0 + ramp).contains(&onset), "onset {onset}");
        let last = sim.recs.last().unwrap().slow_band_db;
        assert!(
            (last - 20.0).abs() < 1.0,
            "slow floor after the ramp {last:+.2} dB"
        );
    }
}

#[test]
fn repro9_comparable_reset_keeps_the_open_episode() {
    let cfg = FloorConfig::default();
    let mut sim = Sim::new(1024, 1e6, cfg, 90);
    let gap = sim.seq_at(3.0);
    sim.run(9.0, |t, p| {
        if (1.0..6.0).contains(&t) {
            scale(p, 0..1024, 6.0);
        }
        let flags = if (t / 0.01024).round() as u64 == gap {
            Discontinuity::GAP
        } else {
            Discontinuity::NONE
        };
        (flags, false)
    });
    sim.show("+6 dB at 1–6 s with a gap (same receiver state) at 3 s");
    check_invariants(&sim, &cfg, true);
    assert!(sim.recs[gap as usize].reset);
    assert_eq!(sim.kinds(), [Rise, FloorEventKind::End], "{AWARE_006}");
    assert_eq!(sim.events[1].end_reason, Some(EndReason::Returned));
    assert!((sim.secs(sim.events[1].onset_t) - 6.0).abs() <= 2.0 * sim.period());
    assert!(
        sim.recs[gap as usize..sim.seq_at(5.9) as usize]
            .iter()
            .all(|r| r.active == 1),
        "the episode stays open across the reset"
    );
}

#[test]
fn repro9_rise_three_frames_after_a_reset() {
    let cfg = FloorConfig::default();
    let mut sim = Sim::new(1024, 1e6, cfg, 91);
    let gap = sim.seq_at(2.0);
    sim.run(5.0, |t, p| {
        let seq = (t / 0.01024).round() as u64;
        if seq >= gap + 3 {
            scale(p, 0..1024, 6.0);
        }
        let flags = if seq == gap {
            Discontinuity::GAP
        } else {
            Discontinuity::NONE
        };
        (flags, false)
    });
    sim.show("gap at 2 s, +6 dB three frames later");
    check_invariants(&sim, &cfg, false);
    assert_eq!(sim.kinds(), [Rise], "{AWARE_006}");
    let e = &sim.events[0];
    assert_eq!(e.onset_seq, gap + 3);
    assert!((e.step_db - 6.0).abs() < 0.5 && e.baseline_dbfs_per_hz.abs() < 0.5);
}

#[test]
fn repro9_incomparable_reset_is_unknown_and_a_later_drop_is_a_bare_fall() {
    let cfg = FloorConfig::default();
    let mut sim = Sim::new(1024, 1e6, cfg, 92);
    sim.run(9.0, |t, p| {
        if (1.0..6.0).contains(&t) {
            scale(p, 0..1024, 6.0);
        }
        (Discontinuity::NONE, t >= 3.0)
    });
    sim.show("+6 dB at 1–6 s, gain change at 3 s");
    check_invariants(&sim, &cfg, true);
    assert_eq!(sim.kinds(), [Rise, Unknown, Fall], "{AWARE_006}");
    assert_eq!(sim.events[1].episode, sim.events[0].episode);
    assert_eq!(sim.events[1].onset_seq, sim.seq_at(3.0));
    assert!(!sim.events[2].interrupted);
}

#[test]
fn repro9_comparable_reset_after_the_floor_returned_ends_with_reset() {
    let cfg = FloorConfig::default();
    let mut sim = Sim::new(1024, 1e6, cfg, 93);
    let gap = sim.seq_at(3.0);
    sim.run(6.0, |t, p| {
        let seq = (t / 0.01024).round() as u64;
        if t >= 1.0 && seq < gap {
            scale(p, 0..1024, 6.0);
        }
        let flags = if seq == gap {
            Discontinuity::GAP
        } else {
            Discontinuity::NONE
        };
        (flags, false)
    });
    sim.show("+6 dB at 1 s; after a gap at 3 s the floor is back");
    check_invariants(&sim, &cfg, true);
    assert_eq!(sim.kinds(), [Rise, FloorEventKind::End], "{AWARE_006}");
    let end = &sim.events[1];
    assert_eq!(end.end_reason, Some(EndReason::Reset));
    assert_eq!(end.onset_seq, gap);
}

// ---------------------------------------------------------------------------------------------
// Randomised sequences.

#[derive(Clone, Debug)]
enum Perturb {
    Level {
        region: Range<usize>,
        db: f64,
        t0: f64,
        t1: f64,
        ramp: f64,
    },
    Drift {
        db: f64,
        t0: f64,
        t1: f64,
    },
    Bursts {
        region: Range<usize>,
        db: f64,
        on: f64,
        period: f64,
        t0: f64,
        t1: f64,
    },
}

impl Perturb {
    fn apply(&self, t: f64, p: &mut [f32]) {
        match self {
            Perturb::Level {
                region,
                db,
                t0,
                t1,
                ramp,
            } => {
                if t >= *t0 && t < *t1 {
                    let w = if *ramp > 0.0 {
                        ((t - t0) / ramp).min((t1 - t) / ramp).min(1.0)
                    } else {
                        1.0
                    };
                    scale(p, region.clone(), db * w);
                }
            }
            Perturb::Drift { db, t0, t1 } => {
                let w = ((t - t0) / (t1 - t0)).clamp(0.0, 1.0);
                if w > 0.0 {
                    let n = p.len();
                    scale(p, 0..n, db * w);
                }
            }
            Perturb::Bursts {
                region,
                db,
                on,
                period,
                t0,
                t1,
            } => {
                if t >= *t0 && t < *t1 && (t - t0) % period < *on {
                    scale(p, region.clone(), *db);
                }
            }
        }
    }
}

struct Scenario {
    perturbs: Vec<Perturb>,
    gaps: Vec<f64>,
    high_gain: Option<(f64, f64)>,
}

fn random_scenario(rng: &mut Rng, bins: usize) -> Scenario {
    fn u(rng: &mut Rng, a: f64, b: f64) -> f64 {
        a + (b - a) * rng.unit()
    }
    let region = |rng: &mut Rng| {
        let w = u(rng, 128.0, bins as f64) as usize;
        let s = u(rng, 0.0, (bins - w) as f64) as usize;
        s..s + w
    };
    let mut perturbs = Vec::new();
    for _ in 0..3 + (rng.unit() * 5.0) as usize {
        let t0 = u(rng, 1.0, 9.0);
        let t1 = (t0 + u(rng, 0.3, 4.0)).min(12.0);
        let db = if rng.unit() < 0.7 {
            u(rng, 4.0, 12.0)
        } else {
            -u(rng, 4.0, 8.0)
        };
        let ramp = if rng.unit() < 0.4 {
            u(rng, 0.0, 1.5).min((t1 - t0) / 2.0)
        } else {
            0.0
        };
        perturbs.push(Perturb::Level {
            region: region(rng),
            db,
            t0,
            t1,
            ramp,
        });
    }
    if rng.unit() < 0.5 {
        let t0 = u(rng, 1.0, 5.0);
        perturbs.push(Perturb::Drift {
            db: u(rng, -2.5, 2.5),
            t0,
            t1: t0 + u(rng, 3.0, 7.0),
        });
    }
    if rng.unit() < 0.5 {
        let t0 = u(rng, 1.0, 6.0);
        perturbs.push(Perturb::Bursts {
            region: region(rng),
            db: u(rng, 1.0, 3.0),
            on: u(rng, 0.05, 0.3),
            period: u(rng, 0.5, 3.0),
            t0,
            t1: t0 + 5.0,
        });
    }
    let mut gaps: Vec<f64> = (0..(rng.unit() * 3.0) as usize)
        .map(|_| u(rng, 1.0, 12.0))
        .collect();
    gaps.sort_by(f64::total_cmp);
    let high_gain = (rng.unit() < 0.4).then(|| {
        let t0 = u(rng, 2.0, 9.0);
        (t0, t0 + u(rng, 1.0, 3.0))
    });
    Scenario {
        perturbs,
        gaps,
        high_gain,
    }
}

#[test]
fn randomised_sequences_keep_the_event_model_invariants() {
    // Short time constants so each 17.5 s scene holds many lifecycles.
    let defaults = FloorConfig::default();
    let cfg = FloorConfig {
        slow: SlowFloorConfig {
            settle_s: 0.1,
            ..defaults.slow
        },
        impulsive: ImpulsiveGateConfig {
            max_duration_s: 0.05,
            ..defaults.impulsive
        },
        change: FloorChangeConfig {
            confirm_s: 0.2,
            end_s: 0.2,
            holdoff_s: 0.3,
            long_time_constant_s: 5.0,
            rebaseline_s: Some(3.0),
            ..defaults.change
        },
        ..defaults
    };
    let bins = 1024;
    let mut rng = Rng::new(0x7005);
    let mut totals = [0usize; 6];
    for n in 0..10 {
        let sc = random_scenario(&mut rng, bins);
        let mut sim = Sim::new(bins, 1e6, cfg, 1000 + n);
        sim.sk = n % 2 == 0;
        let mut next_gap = 0;
        let gaps = sc.gaps.clone();
        sim.run(17.5, |t, p| {
            for pt in &sc.perturbs {
                pt.apply(t, p);
            }
            let high = sc.high_gain.is_some_and(|(a, b)| (a..b).contains(&t));
            if high {
                scale(p, 0..bins, 6.0);
            }
            let mut flags = Discontinuity::NONE;
            while next_gap < gaps.len() && gaps[next_gap] <= t {
                flags = Discontinuity::GAP;
                next_gap += 1;
            }
            (flags, high)
        });
        for e in &sim.events {
            let k = [Rise, Extend, Update, FloorEventKind::End, Fall, Unknown]
                .iter()
                .position(|&x| x == e.kind)
                .unwrap();
            totals[k] += 1;
        }
        let summary = format!(
            "scenario {n}: {} perturbations, gaps {:?}, high gain {:?}: {:?}",
            sc.perturbs.len(),
            sc.gaps,
            sc.high_gain,
            sim.kinds()
        );
        eprintln!("{summary}");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            check_invariants(&sim, &cfg, true);
        }));
        if let Err(panic) = result {
            eprintln!("{:#?}", sc.perturbs);
            sim.show(&format!("failing scenario {n}"));
            std::panic::resume_unwind(panic);
        }
    }
    eprintln!("randomised totals (rise, extend, update, end, fall, unknown): {totals:?}");
    assert!(totals[0] >= 10 && totals[3] >= 5 && totals[4] >= 3);
}

// ---------------------------------------------------------------------------------------------
// Third review against the fix (T-005 fix 2): phantom falls, merges, splits, widening.

/// Worst |slow floor| (dB, truth 0) at `bins` over frames from `t0` on.
fn slow_error_after(sim: &Sim, t0: f64, bins: &[usize]) -> f64 {
    let mut worst = 0.0f64;
    for seq in sim.seq_at(t0)..sim.recs.len() as u64 {
        for &b in bins {
            worst = worst.max(sim.slow_db(seq, b).abs());
        }
    }
    worst
}

fn count(sim: &Sim, pred: impl Fn(&FloorEvent) -> bool) -> usize {
    sim.events.iter().filter(|e| pred(e)).count()
}

#[test]
fn fix2_dropout_frames_open_no_phantom_episode() {
    let cfg = FloorConfig::default();
    let cases = [
        ("1 frame -10 dB", 1u64, -10.0),
        ("3 frames -6 dB", 3, -6.0),
        ("1 frame -4 dB", 1, -4.0),
    ];
    for (k, (name, frames, level)) in cases.into_iter().enumerate() {
        let mut sim = Sim::new(4096, 4e6, cfg, 200 + k as u64);
        let (start, period) = (sim.seq_at(3.0), sim.period());
        sim.run(12.0, |t, p| {
            let seq = (t / period).round() as u64;
            if seq >= start && seq - start < frames {
                scale(p, 0..2048, level);
            }
            quiet()
        });
        sim.show(&format!("{name} over bins 0..2048 at 3 s"));
        check_invariants(&sim, &cfg, true);
        let worst = slow_error_after(&sim, 1.0, &[500, 1000, 3000]);
        eprintln!("  worst slow-floor error {worst:.3} dB");
        assert!(
            sim.events.is_empty(),
            "{AWARE_006}: {name} made a phantom episode"
        );
        assert!(worst < 0.2, "{name}: slow floor {worst:.3} dB off");
    }
}

#[test]
fn fix2_persistent_sub_threshold_drop_is_adopted_without_falls() {
    let cfg = FloorConfig::default();
    let mut sim = Sim::new(4096, 4e6, cfg, 210);
    sim.run(15.0, |t, p| {
        if t >= 3.0 {
            scale(p, 0..2048, -2.8);
        }
        quiet()
    });
    sim.show("persistent -2.8 dB over bins 0..2048 from 3 s");
    check_invariants(&sim, &cfg, true);
    assert!(
        count(&sim, |e| e.kind == Fall) <= 1,
        "{AWARE_006}: repeated falls"
    );
    let last = sim.recs.len() as u64 - 1;
    let err = sim.slow_db(last, 1000) + 2.8;
    eprintln!("  slow floor error under the drop {err:+.3} dB");
    assert!(err.abs() < 0.2);
}

#[test]
fn fix2_static_in_block_step_makes_no_episodes() {
    let cfg = FloorConfig::default();
    // A passband +6 dB over 1024 bins with sharp edges, aligned with and between block starts.
    for (k, lo) in [1536usize, 1500].into_iter().enumerate() {
        let mut sim = Sim::new(4096, 4e6, cfg, 220 + k as u64);
        sim.run(20.0, |_, p| {
            scale(p, lo..lo + 1024, 6.0);
            quiet()
        });
        sim.show(&format!("static +6 dB step over {lo}..{}", lo + 1024));
        check_invariants(&sim, &cfg, false);
        let last = sim.recs.len() as u64 - 1;
        let (inside, outside) = (sim.slow_db(last, lo + 512) - 6.0, sim.slow_db(last, 500));
        eprintln!("  slow floor inside {inside:+.3} dB, outside {outside:+.3} dB");
        assert!(sim.events.is_empty(), "{AWARE_006}: spurious episodes");
        assert!(inside.abs() < 0.2 && outside.abs() < 0.2);
    }
}

#[test]
fn fix2_rebaselined_elevated_region_makes_no_fall_rise_chains() {
    let mut cfg = FloorConfig::default();
    cfg.change.rebaseline_s = Some(20.0);
    let mut sim = Sim::new(4096, 4e6, cfg, 230);
    sim.run(75.0, |t, p| {
        if (1.0..60.0).contains(&t) {
            scale(p, 1000..2000, 6.0);
        }
        if (30.0..60.0).contains(&t) {
            scale(p, 1900..2600, 6.0);
        }
        quiet()
    });
    sim.show("+6 dB 1000..2000 1-60 s (rebaseline 20 s), adjacent 1900..2600 30-60 s");
    check_invariants(&sim, &cfg, true);
    let worst = slow_error_after(&sim, 66.0, &[1500, 2300, 3500]);
    eprintln!("  worst slow-floor error after 66 s {worst:.3} dB");
    assert_eq!(
        count(&sim, |e| e.interrupted),
        0,
        "{AWARE_006}: interrupted falls"
    );
    assert_eq!(
        count(&sim, |e| e.kind == Rise),
        2,
        "{AWARE_006}: rise chain"
    );
    assert!(worst < 0.2);
    assert_eq!(sim.recs.last().unwrap().active, 0);
}

#[test]
fn fix2_bridge_vanishing_splits_the_episode_and_update_shrinks_the_extent() {
    let cfg = FloorConfig::default();
    let mut sim = Sim::new(4096, 4e6, cfg, 240);
    sim.run(20.0, |t, p| {
        if (1.0..16.0).contains(&t) {
            scale(p, 500..1200, 6.0);
            scale(p, 2000..2800, 6.0);
        }
        if (3.0..6.0).contains(&t) {
            scale(p, 1100..2100, 6.0);
        }
        if (10.0..16.0).contains(&t) {
            scale(p, 1450..1750, 6.0);
        }
        quiet()
    });
    sim.show("A 500..1200 + B 2000..2800, bridge 3-6 s, rise in the gap at 10 s");
    check_invariants(&sim, &cfg, true);
    let split = sim
        .events
        .iter()
        .find(|e| e.split_from.is_some())
        .expect("a split Rise");
    let parent = split.split_from.unwrap();
    let update = sim
        .events
        .iter()
        .find(|e| e.kind == Update && e.episode == parent && e.confirmed_seq == split.confirmed_seq)
        .expect("the parent's Update");
    let mut parts = [split.bins.clone(), update.bins.clone()];
    parts.sort_by_key(|r| r.start);
    eprintln!("  after the bridge returned: {parts:?}");
    assert!(near(parts[0].start, 500, 128) && near(parts[0].end, 1200, 128));
    assert!(near(parts[1].start, 2000, 128) && near(parts[1].end, 2800, 128));
    assert!((sim.secs(split.onset_t) - 1.0).abs() <= 2.0 * sim.period());
    let hole = sim
        .events
        .iter()
        .find(|e| e.kind == Rise && e.split_from.is_none() && sim.secs(e.onset_t) > 9.0)
        .expect("the gap rise");
    assert!(
        hole.bins.start >= 1200 && hole.bins.end <= 2000,
        "{:?}",
        hole.bins
    );
}

#[test]
fn fix2_widening_rise_extends_only_the_added_ranges() {
    let cfg = FloorConfig::default();
    let mut sim = Sim::new(4096, 4e6, cfg, 250);
    sim.run(19.0, |t, p| {
        if t >= 1.0 {
            let g = ((t - 1.0).min(15.0) * 100.0) as usize;
            scale(p, 1800 - g..2300 + g, 6.0);
        }
        quiet()
    });
    sim.show("1800..2300 widening 100 bins/s per side for 15 s");
    check_invariants(&sim, &cfg, false);
    assert_eq!(count(&sim, |e| e.kind == Rise), 1);
    let extends: Vec<&FloorEvent> = sim.events.iter().filter(|e| e.kind == Extend).collect();
    assert!(extends.len() >= 4);
    for e in extends {
        assert!(
            !e.change_bins.contains(&2048) && e.change_bins.len() <= 1024,
            "{AWARE_006}: Extend reports more than it added: {:?}",
            e.change_bins
        );
    }
    let last = sim.events.last().unwrap();
    assert!(near(last.bins.start, 300, 192) && near(last.bins.end, 3800, 192));
}

#[test]
fn fix2_merge_keeps_the_noise_like_episode() {
    let cfg = FloorConfig::default();
    let mut sim = Sim::new(4096, 4e6, cfg, 260);
    let mut sk = vec![1.0f32; 4096];
    sk[500..1200].fill(1.6);
    sim.sk_values = Some(sk);
    sim.run(9.0, |t, p| {
        if t >= 1.0 {
            scale(p, 500..1200, 6.0);
        }
        if t >= 2.0 {
            scale(p, 2000..2800, 6.0);
        }
        if t >= 4.0 {
            scale(p, 1100..2100, 6.0);
        }
        quiet()
    });
    sim.show("Structured A 500..1200 (older), NoiseLike B 2000..2800, NoiseLike bridge at 4 s");
    check_invariants(&sim, &cfg, false);
    let rise = |class| {
        sim.events
            .iter()
            .find(|e| e.kind == Rise && e.class == class)
            .unwrap_or_else(|| panic!("a {class:?} Rise"))
    };
    let (a, b) = (
        rise(FloorChangeClass::Structured),
        rise(FloorChangeClass::NoiseLike),
    );
    let merged = sim
        .events
        .iter()
        .find(|e| e.end_reason == Some(EndReason::Merged))
        .expect("a merge");
    assert_eq!(
        (merged.episode, merged.merged_into),
        (a.episode, Some(b.episode))
    );
    let ext = sim
        .events
        .iter()
        .find(|e| e.kind == Extend && e.episode == b.episode)
        .expect("the survivor's Extend");
    eprintln!("  survivor Extend change {:?}", ext.change_bins);
    // T-029: the structured blocks are their own Extend run, reported Structured; the survivor's
    // noise-like added extent is the bridge only.
    let extends: Vec<&FloorEvent> = sim
        .events
        .iter()
        .filter(|e| e.kind == Extend && e.episode == b.episode)
        .collect();
    let plain: Vec<_> = extends
        .iter()
        .filter(|e| e.class != FloorChangeClass::Structured)
        .collect();
    let structured: Vec<_> = extends
        .iter()
        .filter(|e| e.class == FloorChangeClass::Structured)
        .collect();
    assert!(!plain.is_empty() && !structured.is_empty(), "{extends:?}");
    for e in &plain {
        assert!(
            e.change_bins.start >= 1200 - 128 && e.change_bins.end >= 1900,
            "{AWARE_006}: noise-like Extend covers the structured region: {:?}",
            e.change_bins
        );
    }
    assert!(structured.iter().any(|e| e.change_bins.start <= 600));
}

/// T-029 (T-021 follow-up): a steady wide signal that is not verified noise-like must not move
/// the slow floor, with SK away from 1 (Structured) or without SK (Unverified).
#[test]
fn t029_steady_wide_signal_does_not_lift_the_slow_floor() {
    for (name, sk) in [("no SK", None), ("SK 0.6", Some(0.6f32))] {
        let cfg = FloorConfig::default();
        let mut sim = Sim::new(4096, 4e6, cfg, 290);
        if let Some(v) = sk {
            let mut s = vec![1.0f32; 4096];
            s[1024..2560].fill(v);
            sim.sk_values = Some(s);
        }
        sim.run(12.0, |t, p| {
            if t >= 1.0 {
                scale(p, 1024..2560, 6.0);
            }
            quiet()
        });
        sim.show(&format!("steady +6 dB OFDM-like 1024..2560, {name}"));
        check_invariants(&sim, &cfg, false);
        let rise = sim
            .events
            .iter()
            .find(|e| e.kind == Rise)
            .expect("the wide signal is reported");
        assert_ne!(rise.class, FloorChangeClass::NoiseLike);
        let before = sim.seq_at(0.9);
        let mut worst = 0.0f64;
        for seq in sim.seq_at(1.0)..sim.recs.len() as u64 {
            for bin in [1400, 1792, 2200] {
                worst = worst.max((sim.slow_db(seq, bin) - sim.slow_db(before, bin)).abs());
            }
        }
        eprintln!("  worst slow-floor change inside the signal {worst:.2} dB");
        assert!(worst <= 0.5, "{name}: slow floor moved {worst:.2} dB");
    }
}
