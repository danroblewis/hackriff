//! Floor-step guard: near sharp floor steps and notches, where a block floor estimate is biased
//! low (T-006 review), the floor branch leaves the per-frame reference.
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
//! **Plateaus are exempt.** An up event followed by a down event bounds a plateau only when both
//! outer sides (the lowest block floor on the low side of each event) lie within
//! `plateau_match_db` of the band's median block floor: a signal-like bump above the common floor,
//! such as a flat signal wider than about a block. There the per-frame floor is biased low only
//! over bins that carry the signal, so floor-branch detections are true (they find the signal's
//! edges). A plateau with a notch on one side, or between two different floor levels (a notch plus
//! a second down-step, a staircase), is not one (T-028). The segment between a down event and the
//! next up event is a dip (a notch). An event is guarded when it bounds a dip, or when it bounds no
//! plateau: bins from `centre(first pair) − margin` to `centre(last pair + sep) + margin`
//! (margin = `margin_blocks` × block) are guarded. Bins within the margin of a configured known
//! response edge (`DetectorConfig::response_edges_hz`) are always guarded.
//!
//! **Guarded bins use the wide reference where the shape explains the step** (T-028). The
//! tracker's wide reference is built on shape-normalised block floors (T-005), so a notch or
//! roll-off that the learned shape `S` explains does not bias it; an unlearned step, or a floor
//! shelf *above* the band floor (`S` never learns upward features, so the wide reference reads it
//! as a signal), does. Once the shape is trusted (the detector passes it from the floor segment's
//! `wide_min_frames`-th frame), the guard reconstructs the normalised block floors
//! (`floor / S` at each block centre when some block's shape varies by more than 1 dB, as the
//! tracker does; the raw block floors otherwise) and flags a normalised block as *unexplained*
//! when it sits more than `wide_residual_db` below their median, lies in a residual jump above
//! `wide_step_db` that is guarded by the same dip/plateau rule, or lies in a residual plateau wider
//! than `wide_max_plateau_blocks` (a shelf above the band floor and a wide signal are the same
//! after the shape; only a narrow one is taken for a signal). A guarded zone with no unexplained
//! block within one block of it runs the floor branch against `FloorFrame::wide_floor`
//! ([`StepGuard::wide_mask`]); otherwise it is OS-only. A flat signal up to about 1 MHz next to a
//! notch leaves only a narrow residual plateau, so it keeps the floor branch.
//!
//! **Limits.** A floor shelf narrower than `wide_max_plateau_blocks` next to a learned step reads
//! as a signal (the wide reference is low over it); a shelf whose raw sides are both at the band
//! floor is a plateau and is exempt (a filter-bank passband with sharp edges).
//!
//! **What does not trigger it.** A narrowband emitter (an FM station is ~40 of 256 bins) does not
//! move a block floor.
//!
//! The guard needs the floor tracker's block layout (`blocks`, default 256/64). When the frame's
//! block count does not match it, the guard is unavailable for that frame and only the configured
//! edges are guarded (OS-only; counted by the detector).

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
    /// Both sides of a plateau must lie within this of the median block floor, dB (3).
    pub plateau_match_db: f64,
    /// A shape-normalised block floor more than this below their median is unexplained, dB (1.5).
    pub wide_residual_db: f64,
    /// A shape-normalised block-floor jump above this that bounds no plateau is unexplained, dB (3).
    pub wide_step_db: f64,
    /// A residual (shape-normalised) plateau wider than this is unexplained, blocks (16: ≈ 1 MHz
    /// at 20 Msps / 4096 bins). A floor shelf above the band floor and a wide signal look the
    /// same after the shape; only a narrow one is taken for a signal.
    pub wide_max_plateau_blocks: usize,
    /// Frames of a floor segment before the learned shape is trusted (32: the tracker applies it
    /// from the segment's 32nd frame).
    pub wide_min_frames: u64,
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
            plateau_match_db: 3.0,
            wide_residual_db: 1.5,
            wide_step_db: 3.0,
            wide_max_plateau_blocks: 16,
            wide_min_frames: 32,
            blocks: BlockConfig::default(),
        }
    }
}

/// The floor tracker's learned response shape and its shaped per-frame floor
/// (`FloorFrame::{shape, floor}`), per bin.
#[derive(Clone, Copy, Debug)]
pub struct ShapeView<'a> {
    /// `FloorFrame::shape`.
    pub shape: &'a [f32],
    /// `FloorFrame::floor`.
    pub floor: &'a [f32],
}

/// A shape block whose range exceeds this is normalised by the tracker (hk-dsp `ACTIVE_RANGE_DB`).
const SHAPE_ACTIVE_RANGE_DB: f64 = 1.0;

#[derive(Clone, Copy, Debug)]
struct Event {
    dir: i8,
    first_pair: usize,
    last_pair: usize,
    lo: isize,
    hi: isize,
    guarded: bool,
}

/// Per-segment guard state. Buffers are sized per resolution.
#[derive(Clone, Debug)]
pub struct StepGuard {
    cfg: StepGuardConfig,
    persist: Vec<u32>,
    hold: Vec<u32>,
    dir: Vec<i8>,
    events: Vec<Event>,
    norm_events: Vec<Event>,
    mask: Vec<bool>,
    wide: Vec<bool>,
    norm: Vec<f32>,
    unexplained: Vec<bool>,
    scratch: Vec<f32>,
    available: bool,
    guarded: usize,
    wide_bins: usize,
}

fn ratio_of(db: f64) -> f32 {
    10f64.powf(db / 10.0) as f32
}

/// Median of `x` (total order), using `scratch` (no allocation once sized).
fn median(x: &[f32], scratch: &mut Vec<f32>) -> f32 {
    scratch.clear();
    scratch.extend_from_slice(x);
    if scratch.is_empty() {
        return 0.0;
    }
    let mid = scratch.len() / 2;
    let (_, &mut m, _) = scratch.select_nth_unstable_by(mid, f32::total_cmp);
    m
}

fn min_of(x: &[f32]) -> f32 {
    x.iter().copied().fold(f32::INFINITY, f32::min)
}

/// Direction of the jump from `a` to `b` beyond `ratio` (0: none or not comparable).
fn jump_dir(a: f32, b: f32, ratio: f32) -> i8 {
    if !(a > 0.0 && b > 0.0) {
        0
    } else if b > a * ratio {
        1
    } else if a > b * ratio {
        -1
    } else {
        0
    }
}

/// Groups contiguous active pairs of one direction into events.
fn group_events(
    events: &mut Vec<Event>,
    pairs: usize,
    dir_of: impl Fn(usize) -> i8,
    span: impl Fn(usize) -> (isize, isize),
) {
    events.clear();
    for i in 0..pairs {
        let d = dir_of(i);
        if d == 0 {
            continue;
        }
        let (lo, hi) = span(i);
        match events.last_mut() {
            Some(e) if e.dir == d && e.last_pair + 1 == i => {
                e.last_pair = i;
                e.hi = hi;
            }
            _ => events.push(Event {
                dir: d,
                first_pair: i,
                last_pair: i,
                lo,
                hi,
                guarded: false,
            }),
        }
    }
}

/// Marks guarded events: those bounding a dip, or bounding no plateau whose two low sides lie
/// within `near` of `band` (`level` holds the block values the events were built from).
fn classify_events(events: &mut [Event], level: &[f32], sep: usize, band: f32, near: f32) {
    let floor_level = |x: f32| x > 0.0 && x <= band * near && x * near >= band;
    let low_side = |e: &Event| {
        if e.dir > 0 {
            min_of(&level[e.first_pair..=e.last_pair])
        } else {
            min_of(&level[e.first_pair + sep..=e.last_pair + sep])
        }
    };
    let plateau = |up: &Event, down: &Event| {
        up.dir > 0 && down.dir < 0 && floor_level(low_side(up)) && floor_level(low_side(down))
    };
    for k in 0..events.len() {
        let e = events[k];
        let prev = k.checked_sub(1).map(|p| events[p]);
        let next = events.get(k + 1).copied();
        let plateau_before = prev.is_some_and(|p| plateau(&p, &e));
        let plateau_after = next.is_some_and(|x| plateau(&e, &x));
        let dip_before = prev.is_some_and(|p| p.dir < 0) && e.dir > 0;
        let dip_after = e.dir < 0 && next.is_some_and(|x| x.dir > 0);
        events[k].guarded = dip_before || dip_after || !(plateau_before || plateau_after);
    }
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
            norm_events: Vec::new(),
            mask: Vec::new(),
            wide: Vec::new(),
            norm: Vec::new(),
            unexplained: Vec::new(),
            scratch: Vec::new(),
            available: false,
            guarded: 0,
            wide_bins: 0,
        }
    }

    /// Settings.
    pub fn config(&self) -> &StepGuardConfig {
        &self.cfg
    }

    /// Starts a segment of `bins` bins (clears the persistence state).
    pub fn reset(&mut self, bins: usize) {
        self.mask.resize(bins, true);
        self.mask.fill(true);
        self.wide.resize(bins, false);
        self.wide.fill(false);
        self.persist.fill(0);
        self.hold.fill(0);
        self.dir.fill(0);
        self.guarded = 0;
        self.wide_bins = 0;
        self.available = false;
    }

    /// Updates the masks from one frame's block floors (linear, raw block FCME) and returns the
    /// per-frame mask: `true` where the floor branch may run on the per-frame reference. `shape`
    /// is the tracker's learned shape once it is trusted (`None`: guarded zones are OS-only).
    pub fn update(
        &mut self,
        block_floor: &[f32],
        geometry: &Geometry,
        edges_hz: &[f64],
        shape: Option<ShapeView<'_>>,
    ) -> &[bool] {
        let n = geometry.bins;
        self.mask.resize(n, true);
        self.mask.fill(true);
        self.wide.resize(n, false);
        self.wide.fill(false);
        let block = self.cfg.blocks.block_bins.min(n).max(1);
        let hop = self.cfg.blocks.hop_bins.max(1);
        let margin = (self.cfg.margin_blocks * block as f64).round() as isize;
        let expected = (n - block) / hop + 1;
        self.available = block_floor.len() == expected;
        self.events.clear();
        let near = ratio_of(self.cfg.plateau_match_db);
        let mut wide_ready = false;
        if self.available {
            if self.persist.len() != expected {
                self.persist.clear();
                self.persist.resize(expected, 0);
                self.hold.clear();
                self.hold.resize(expected, 0);
                self.dir.clear();
                self.dir.resize(expected, 0);
                self.norm.clear();
                self.norm.resize(expected, 0.0);
                self.unexplained.clear();
                self.unexplained.resize(expected, false);
                self.scratch.reserve(expected);
                self.events.reserve(expected);
                self.norm_events.reserve(expected);
            }
            let sep = block.div_ceil(hop);
            let pairs = expected.saturating_sub(sep);
            let ratio = ratio_of(self.cfg.jump_db);
            let centre = |i: usize| (i * hop) as isize + (block as isize - 1) / 2;
            let span = |i: usize| (centre(i) - margin, centre(i + sep) + margin + 1);
            for i in 0..pairs {
                let d = jump_dir(block_floor[i], block_floor[i + sep], ratio);
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
            }
            let (hold, dir) = (&self.hold, &self.dir);
            group_events(
                &mut self.events,
                pairs,
                |i| if hold[i] > 0 { dir[i] } else { 0 },
                span,
            );
            if !self.events.is_empty() {
                let band = median(block_floor, &mut self.scratch);
                classify_events(&mut self.events, block_floor, sep, band, near);
                for e in self.events.iter().filter(|e| e.guarded) {
                    let lo = e.lo.clamp(0, n as isize) as usize;
                    let hi = e.hi.clamp(0, n as isize) as usize;
                    self.mask[lo..hi].fill(false);
                }
            }
            if let Some(v) = shape.filter(|v| v.shape.len() == n && v.floor.len() == n)
                && (!self.events.is_empty() || !edges_hz.is_empty())
            {
                // The tracker's normalised block floors.
                let active = ratio_of(SHAPE_ACTIVE_RANGE_DB);
                let any_active = (0..expected).any(|j| {
                    let s = &v.shape[j * hop..j * hop + block];
                    let (lo, hi) = s
                        .iter()
                        .fold((f32::INFINITY, 0.0f32), |(a, b), &x| (a.min(x), b.max(x)));
                    hi > lo * active
                });
                for (j, x) in self.norm.iter_mut().enumerate() {
                    let c = (j * hop + block / 2).min(n - 1);
                    *x = if any_active {
                        v.floor[c] / v.shape[c].max(1e-30)
                    } else {
                        block_floor[j]
                    };
                }
                let med = median(&self.norm, &mut self.scratch);
                if med > 0.0 {
                    let residual = ratio_of(self.cfg.wide_residual_db);
                    for (u, &x) in self.unexplained.iter_mut().zip(&self.norm) {
                        // NaN (incomparable) counts as unexplained.
                        *u = (x * residual).partial_cmp(&med).is_none_or(|o| o.is_lt());
                    }
                    let norm = &self.norm;
                    let step = ratio_of(self.cfg.wide_step_db);
                    group_events(
                        &mut self.norm_events,
                        pairs,
                        |i| jump_dir(norm[i], norm[i + sep], step),
                        span,
                    );
                    classify_events(&mut self.norm_events, norm, sep, med, near);
                    let max_width = self.cfg.wide_max_plateau_blocks;
                    for (k, e) in self.norm_events.iter().enumerate() {
                        if e.guarded {
                            self.unexplained[e.first_pair..=e.last_pair + sep].fill(true);
                        } else if e.dir > 0
                            && let Some(down) = self.norm_events.get(k + 1)
                            && down.last_pair + sep - e.first_pair > max_width
                        {
                            // An unguarded up event is a plateau with the next (down) event.
                            self.unexplained[e.first_pair..=down.last_pair + sep].fill(true);
                        }
                    }
                    wide_ready = true;
                }
            }
        }
        let edge_range = |f: f64| {
            let b = ((f - geometry.center_hz) / geometry.bin_width_hz + (n / 2) as f64).round()
                as isize;
            (b + margin >= 0 && b - margin < n as isize).then(|| {
                (
                    (b - margin).clamp(0, n as isize) as usize,
                    (b + margin + 1).clamp(0, n as isize) as usize,
                )
            })
        };
        for &f in edges_hz {
            if let Some((lo, hi)) = edge_range(f) {
                self.mask[lo..hi].fill(false);
            }
        }
        if wide_ready {
            // A zone is explained when no block within one block of it is unexplained (the
            // interpolation reaches one block centre beyond the zone).
            let nb = expected;
            let valid = |lo: usize, hi: usize| {
                let j_hi = ((hi + block) / hop).min(nb - 1);
                let j_lo = (lo.saturating_sub(block) / hop).min(j_hi);
                !self.unexplained[j_lo..=j_hi].iter().any(|&u| u)
            };
            // Valid zones first, then invalid ones, so an overlap stays OS-only.
            for pass in [true, false] {
                for e in self.events.iter().filter(|e| e.guarded) {
                    let lo = e.lo.clamp(0, n as isize) as usize;
                    let hi = e.hi.clamp(0, n as isize) as usize;
                    if lo < hi && valid(lo, hi) == pass {
                        self.wide[lo..hi].fill(pass);
                    }
                }
                for &f in edges_hz {
                    if let Some((lo, hi)) = edge_range(f)
                        && lo < hi
                        && valid(lo, hi) == pass
                    {
                        self.wide[lo..hi].fill(pass);
                    }
                }
            }
        }
        self.guarded = self.mask.iter().filter(|&&ok| !ok).count();
        self.wide_bins = self.wide.iter().filter(|&&w| w).count();
        &self.mask
    }

    /// The last per-frame mask (`true`: floor branch allowed on the per-frame reference).
    pub fn mask(&self) -> &[bool] {
        &self.mask
    }

    /// The last wide mask (`true`: a guarded bin whose floor branch runs on the wide reference).
    pub fn wide_mask(&self) -> &[bool] {
        &self.wide
    }

    /// Bins guarded in the last frame (both wide-reference and OS-only).
    pub fn guarded_bins(&self) -> usize {
        self.guarded
    }

    /// Guarded bins that ran the floor branch on the wide reference in the last frame.
    pub fn wide_bins(&self) -> usize {
        self.wide_bins
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

    /// Block floors that read the lowest bin of each block (FCME is biased toward the low side).
    fn blocks_of(profile: &[f32]) -> Vec<f32> {
        blocks_with(|j| min_of(&profile[j * 64..j * 64 + 256]))
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
            s.update(&notch, &g, &[], None).iter().all(|&ok| ok),
            "one frame is not persistent"
        );
        let m = s.update(&notch, &g, &[], None).to_vec();
        assert!(s.available());
        // The biased interpolation span (block centres 37..42 → bins 2495..2815) is guarded.
        assert!(m[2400..2900].iter().all(|&ok| !ok));
        assert!(m[..1500].iter().all(|&ok| ok) && m[3500..].iter().all(|&ok| ok));
        for _ in 0..31 {
            s.update(&flat, &g, &[], None);
        }
        assert!(s.guarded_bins() > 0, "held");
        s.update(&flat, &g, &[], None);
        assert_eq!(s.guarded_bins(), 0, "released");
        // A layout mismatch disables the jump test; a known edge still applies.
        let m = s.update(&[1.0; 7], &g, &[100e6], None).to_vec();
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
            s.update(&plateau, &g, &[], None);
        }
        assert_eq!(s.guarded_bins(), 0);
        // A lone step (an accessory edge): guarded.
        let mut s = StepGuard::new(StepGuardConfig::default());
        s.reset(4096);
        let step = blocks_with(|i| if i >= 30 { 10.0 } else { 1.0 });
        for _ in 0..3 {
            s.update(&step, &g, &[], None);
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
            s.update(&band, &g, &[], None);
        }
        let m = s.mask();
        assert!(!m[31 * 64 + 128], "notch guarded");
        assert!(
            m[15 * 64] && m[45 * 64],
            "passband away from the notch is not"
        );
    }

    #[test]
    fn a_notch_side_or_a_different_floor_level_is_not_a_plateau() {
        // T-006 re-probe: up … down around a segment whose sides are a notch and a lower step.
        let g = geometry();
        let cases = [
            // Notch over blocks 9–15, back up, then a 10 dB down-step at block 47.
            blocks_with(|i| match i {
                9..=15 => 0.01,
                47.. => 0.1,
                _ => 1.0,
            }),
            // 20 dB low below block 19, a 10 dB down-step at block 50.
            blocks_with(|i| match i {
                0..=18 => 0.01,
                50.. => 0.1,
                _ => 1.0,
            }),
        ];
        for (k, blocks) in cases.iter().enumerate() {
            let mut s = StepGuard::new(StepGuardConfig::default());
            s.reset(4096);
            for _ in 0..3 {
                s.update(blocks, &g, &[], None);
            }
            let down = if k == 0 { 47 } else { 50 };
            let m = s.mask();
            assert!(!m[down * 64 - 60], "case {k}: second down-step guarded");
            assert!(m[35 * 64 - 32], "case {k}: interior away from the steps");
            assert!(s.wide_mask().iter().all(|&w| !w), "no shape: OS-only");
        }
    }

    #[test]
    fn guarded_zones_use_the_wide_reference_once_the_shape_explains_the_step() {
        let g = geometry();
        // A −20 dB notch over bins 1920..2240; the tracker floor follows it once learned.
        let mut profile = vec![1.0f32; 4096];
        profile[1920..2240].fill(0.01);
        let blocks = blocks_of(&profile);
        let unlearned = vec![1.0f32; 4096];
        for (shape, want_wide) in [
            (Some(&profile), true),
            (Some(&unlearned), false),
            (None, false),
        ] {
            let view = shape.map(|s| ShapeView {
                shape: s,
                floor: &profile,
            });
            let mut s = StepGuard::new(StepGuardConfig::default());
            s.reset(4096);
            for _ in 0..3 {
                s.update(&blocks, &g, &[], view);
            }
            let (m, w) = (s.mask(), s.wide_mask());
            assert!(!m[2000] && !m[2300] && m[1000] && m[3500]);
            assert_eq!(w[2000] && w[2300], want_wide, "{want_wide}");
            assert!(!w[1000] && !w[3500], "only guarded bins");
            assert_eq!(s.wide_bins() > 0, want_wide);
        }
    }

    #[test]
    fn a_floor_shelf_above_the_band_floor_stays_os_only() {
        // 0 dB below bin 1200, −10 dB to 3200, −20 dB above; the shape learns only the −20 dB
        // region (below the band floor). The 0 dB shelf is unexplained (the wide reference reads
        // it as a signal): its step stays OS-only, the learned step uses the wide reference.
        let g = geometry();
        let profile: Vec<f32> = (0..4096)
            .map(|b| match b {
                0..1200 => 1.0,
                1200..3200 => 0.1,
                _ => 0.01,
            })
            .collect();
        let shape: Vec<f32> = (0..4096)
            .map(|b| if b < 3200 { 1.0 } else { 0.1 })
            .collect();
        let blocks = blocks_of(&profile);
        let mut s = StepGuard::new(StepGuardConfig::default());
        s.reset(4096);
        let view = ShapeView {
            shape: &shape,
            floor: &profile,
        };
        for _ in 0..3 {
            s.update(&blocks, &g, &[], Some(view));
        }
        let (m, w) = (s.mask(), s.wide_mask());
        assert!(!m[1216] && !m[3200], "both steps guarded");
        assert!(!w[1216], "shelf step OS-only");
        assert!(w[3200], "learned step on the wide reference");
    }

    #[test]
    fn a_signal_next_to_a_learned_notch_is_explained() {
        // A +10 dB 300-bin signal 60 bins above a learned notch: only a residual plateau.
        let g = geometry();
        let mut shape = vec![1.0f32; 4096];
        shape[1600..2000].fill(0.01);
        let mut profile = shape.clone();
        for v in &mut profile[2060..2360] {
            *v += 10.0;
        }
        let blocks = blocks_of(&profile);
        let mut s = StepGuard::new(StepGuardConfig::default());
        s.reset(4096);
        let view = ShapeView {
            shape: &shape,
            floor: &profile,
        };
        for _ in 0..3 {
            s.update(&blocks, &g, &[], Some(view));
        }
        let (m, w) = (s.mask(), s.wide_mask());
        assert!((2060..2360).all(|b| !m[b] && w[b]));
    }

    #[test]
    fn narrow_signals_do_not_move_block_floors() {
        let g = geometry();
        let mut s = StepGuard::new(StepGuardConfig::default());
        s.reset(4096);
        let near = blocks_with(|i| 1.0 + 0.2 * ((i % 3) as f32));
        for _ in 0..5 {
            s.update(&near, &g, &[], None);
        }
        assert_eq!(s.guarded_bins(), 0);
    }
}
