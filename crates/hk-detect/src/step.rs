//! Floor-step guard: the floor branch is switched off near sharp floor steps and notches, where a
//! block floor estimate is biased low (T-006 review).
//!
//! Block FCME (256 bins, hop 64) reads the low side of a sharp floor step for about one block past
//! each edge (a notch, an accessory filter edge, an RF path switch), so `P > T·F` then runs 64–91×
//! its design false-alarm rate on the high side. The OS branch adapts within its 32 reference cells
//! and keeps working there.
//!
//! **Jumps.** Each block floor is compared with the block one block width further on (`i` and
//! `i + ⌈block/hop⌉`, so the two do not overlap). A jump above `jump_db` held for `persist_frames`
//! consecutive frames activates the pair (with its direction, up or down in frequency); the pair
//! stays active `hold_frames` frames after the jump disappears. Contiguous active pairs with the
//! same direction form one **event**.
//!
//! **Plateaus are exempt.** The segment between an up event and the next down event is a plateau:
//! a flat signal wider than about a block, or the receiver passband between its roll-offs. There
//! the per-frame floor is biased low only over bins that carry the signal, so floor-branch
//! detections are true (they find the signal's edges). The segment between a down event and the
//! next up event is a dip (a notch). An event is guarded when it bounds a dip, or when it bounds no
//! plateau (an isolated or staircase step): bins from `centre(first pair) − margin` to
//! `centre(last pair + sep) + margin` (margin = `margin_blocks` × block) lose the floor branch. So a
//! notch inside the passband is guarded while the passband's own roll-offs and a wide signal are
//! not. Bins within the margin of a configured known response edge
//! (`DetectorConfig::response_edges_hz`) are always guarded.
//!
//! **What does not trigger it.** A narrowband emitter (an FM station is ~40 of 256 bins) does not
//! move a block floor. A wide signal cut by a band edge, or next to another step, is a step, not a
//! plateau: its visible edge is guarded and found by the OS branch only where it can.
//!
//! The guard needs the floor tracker's block layout (`blocks`, default 256/64). When the frame's
//! block count does not match it, the guard is unavailable for that frame and only the configured
//! edges are guarded (counted by the detector).

use hk_dsp::floor::BlockConfig;

use crate::rules::Geometry;

/// Floor-step guard settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StepGuardConfig {
    /// A block-floor jump above this is a step, dB (6).
    pub jump_db: f64,
    /// Consecutive frames a jump must persist to activate, frames (2).
    pub persist_frames: u32,
    /// Frames a pair stays active after its jump disappears (32).
    pub hold_frames: u32,
    /// Guard margin beyond the event's block centres, in blocks (1).
    pub margin_blocks: f64,
    /// The floor tracker's block layout ([`BlockConfig::default`]: 256 / 64).
    pub blocks: BlockConfig,
}

impl Default for StepGuardConfig {
    fn default() -> Self {
        Self {
            jump_db: 6.0,
            persist_frames: 2,
            hold_frames: 32,
            margin_blocks: 1.0,
            blocks: BlockConfig::default(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Event {
    dir: i8,
    last_pair: usize,
    lo: isize,
    hi: isize,
}

/// Per-segment guard state. Buffers are sized per resolution.
#[derive(Clone, Debug)]
pub struct StepGuard {
    cfg: StepGuardConfig,
    persist: Vec<u32>,
    hold: Vec<u32>,
    dir: Vec<i8>,
    events: Vec<Event>,
    mask: Vec<bool>,
    available: bool,
    guarded: usize,
}

impl StepGuard {
    /// A guard with `cfg`.
    pub fn new(cfg: StepGuardConfig) -> Self {
        Self {
            cfg,
            persist: Vec::new(),
            hold: Vec::new(),
            dir: Vec::new(),
            events: Vec::new(),
            mask: Vec::new(),
            available: false,
            guarded: 0,
        }
    }

    /// Starts a segment of `bins` bins (clears the persistence state).
    pub fn reset(&mut self, bins: usize) {
        self.mask.resize(bins, true);
        self.mask.fill(true);
        self.persist.fill(0);
        self.hold.fill(0);
        self.dir.fill(0);
        self.guarded = 0;
        self.available = false;
    }

    /// Updates the mask from one frame's block floors (linear) and returns it: `true` where the
    /// floor branch may run.
    pub fn update(
        &mut self,
        block_floor: &[f32],
        geometry: &Geometry,
        edges_hz: &[f64],
    ) -> &[bool] {
        let n = geometry.bins;
        self.mask.resize(n, true);
        self.mask.fill(true);
        let block = self.cfg.blocks.block_bins.min(n).max(1);
        let hop = self.cfg.blocks.hop_bins.max(1);
        let margin = (self.cfg.margin_blocks * block as f64).round() as isize;
        let expected = (n - block) / hop + 1;
        self.available = block_floor.len() == expected;
        self.events.clear();
        if self.available {
            if self.persist.len() != expected {
                self.persist.clear();
                self.persist.resize(expected, 0);
                self.hold.clear();
                self.hold.resize(expected, 0);
                self.dir.clear();
                self.dir.resize(expected, 0);
                self.events.reserve(expected);
            }
            let sep = block.div_ceil(hop);
            let ratio = 10f64.powf(self.cfg.jump_db / 10.0) as f32;
            let centre = |i: usize| (i * hop) as isize + (block as isize - 1) / 2;
            for i in 0..expected.saturating_sub(sep) {
                let (a, b) = (block_floor[i], block_floor[i + sep]);
                let d = if !(a > 0.0 && b > 0.0) {
                    0
                } else if b > a * ratio {
                    1
                } else if a > b * ratio {
                    -1
                } else {
                    0
                };
                if d != 0 {
                    self.persist[i] = self.persist[i].saturating_add(1);
                    self.dir[i] = d;
                    if self.persist[i] >= self.cfg.persist_frames {
                        self.hold[i] = self.cfg.hold_frames.max(1);
                    }
                } else {
                    self.persist[i] = 0;
                    self.hold[i] = self.hold[i].saturating_sub(1);
                }
                if self.hold[i] > 0 {
                    let (lo, hi) = (centre(i) - margin, centre(i + sep) + margin + 1);
                    match self.events.last_mut() {
                        Some(e) if e.dir == self.dir[i] && e.last_pair + 1 == i => {
                            e.last_pair = i;
                            e.hi = hi;
                        }
                        _ => self.events.push(Event {
                            dir: self.dir[i],
                            last_pair: i,
                            lo,
                            hi,
                        }),
                    }
                }
            }
            for k in 0..self.events.len() {
                let e = self.events[k];
                let prev = (k > 0).then(|| self.events[k - 1].dir);
                let next = self.events.get(k + 1).map(|x| x.dir);
                // The segment before / after this event is a plateau (up … down) or a dip
                // (down … up).
                let plateau_before = prev == Some(1) && e.dir < 0;
                let plateau_after = e.dir > 0 && next == Some(-1);
                let dip_before = prev == Some(-1) && e.dir > 0;
                let dip_after = e.dir < 0 && next == Some(1);
                let guard = dip_before || dip_after || !(plateau_before || plateau_after);
                if guard {
                    let lo = e.lo.clamp(0, n as isize) as usize;
                    let hi = e.hi.clamp(0, n as isize) as usize;
                    self.mask[lo..hi].fill(false);
                }
            }
        }
        for &f in edges_hz {
            let b = ((f - geometry.center_hz) / geometry.bin_width_hz + (n / 2) as f64).round()
                as isize;
            if b + margin >= 0 && b - margin < n as isize {
                let lo = (b - margin).clamp(0, n as isize) as usize;
                let hi = (b + margin + 1).clamp(0, n as isize) as usize;
                self.mask[lo..hi].fill(false);
            }
        }
        self.guarded = self.mask.iter().filter(|&&ok| !ok).count();
        &self.mask
    }

    /// The last mask (`true`: floor branch allowed).
    pub fn mask(&self) -> &[bool] {
        &self.mask
    }

    /// Bins guarded in the last frame.
    pub fn guarded_bins(&self) -> usize {
        self.guarded
    }

    /// The block layout matched the last frame.
    pub fn available(&self) -> bool {
        self.available
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EdgeRule;

    fn geometry() -> Geometry {
        Geometry::new(98e6, 20e6, 4096, 15e6, &EdgeRule::default())
    }

    fn blocks_with(f: impl Fn(usize) -> f32) -> Vec<f32> {
        (0..(4096 - 256) / 64 + 1).map(f).collect()
    }

    #[test]
    fn a_persistent_notch_guards_its_neighbourhood_and_releases_after_the_hold() {
        let g = geometry();
        let mut s = StepGuard::new(StepGuardConfig::default());
        s.reset(4096);
        // Blocks 38–41 read a notch 20 dB down.
        let notch = blocks_with(|i| if (38..=41).contains(&i) { 0.01 } else { 1.0 });
        let flat = blocks_with(|_| 1.0);
        assert!(
            s.update(&notch, &g, &[]).iter().all(|&ok| ok),
            "one frame is not persistent"
        );
        let m = s.update(&notch, &g, &[]).to_vec();
        assert!(s.available());
        // The biased interpolation span (block centres 37..42 → bins 2495..2815) is guarded.
        assert!(m[2400..2900].iter().all(|&ok| !ok));
        assert!(m[..1500].iter().all(|&ok| ok) && m[3500..].iter().all(|&ok| ok));
        for _ in 0..31 {
            s.update(&flat, &g, &[]);
        }
        assert!(s.guarded_bins() > 0, "held");
        s.update(&flat, &g, &[]);
        assert_eq!(s.guarded_bins(), 0, "released");
        // A layout mismatch disables the jump test; a known edge still applies.
        let m = s.update(&[1.0; 7], &g, &[100e6]).to_vec();
        assert!(!s.available());
        let b = 2048 + (2e6 / g.bin_width_hz) as usize;
        assert!(!m[b] && m[b + 300]);
    }

    #[test]
    fn plateaus_are_exempt_and_isolated_steps_are_guarded() {
        let g = geometry();
        let mut s = StepGuard::new(StepGuardConfig::default());
        s.reset(4096);
        // A flat signal 10 dB up over blocks 20–40 (a plateau): not guarded.
        let plateau = blocks_with(|i| if (20..=40).contains(&i) { 10.0 } else { 1.0 });
        for _ in 0..3 {
            s.update(&plateau, &g, &[]);
        }
        assert_eq!(s.guarded_bins(), 0);
        // A lone step (an accessory edge): guarded.
        let mut s = StepGuard::new(StepGuardConfig::default());
        s.reset(4096);
        let step = blocks_with(|i| if i >= 30 { 10.0 } else { 1.0 });
        for _ in 0..3 {
            s.update(&step, &g, &[]);
        }
        let m = s.mask();
        assert!(!m[30 * 64] && m[100] && m[4000]);
        // A notch inside a plateau (passband roll-offs around it): the notch is guarded.
        let mut s = StepGuard::new(StepGuardConfig::default());
        s.reset(4096);
        let band = blocks_with(|i| match i {
            0..=5 | 55.. => 0.1,
            30..=33 => 0.01,
            _ => 1.0,
        });
        for _ in 0..3 {
            s.update(&band, &g, &[]);
        }
        let m = s.mask();
        assert!(!m[31 * 64 + 128], "notch guarded");
        assert!(
            m[15 * 64] && m[45 * 64],
            "passband away from the notch is not"
        );
    }

    #[test]
    fn narrow_signals_do_not_move_block_floors() {
        let g = geometry();
        let mut s = StepGuard::new(StepGuardConfig::default());
        s.reset(4096);
        let near = blocks_with(|i| 1.0 + 0.2 * ((i % 3) as f32));
        for _ in 0..5 {
            s.update(&near, &g, &[]);
        }
        assert_eq!(s.guarded_bins(), 0);
    }
}
