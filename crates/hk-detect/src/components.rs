//! Streaming 4-connected component labelling over time × frequency, with S4's order of
//! operations: raw components of the region that contain a seed and last ≥ `min_frames` frames
//! are **kept**; only then are kept components merged across gaps of ≤ `gap_frames` frames.
//!
//! # How it streams
//!
//! Each frame's region cells become runs `[lo, hi)`. A run joins every component of an
//! overlapping run in the previous frame (4-connectivity across time), merging them; a run with
//! no overlap starts a component. A component not continued in a frame is **closed**: an unkept
//! closed component is discarded at once (it can never become kept).
//!
//! **Kept** is monotone: a component with a seed whose frame extent reaches `min_frames` is kept
//! from then on, so the decision never waits for the component to end (S4 tests the extent of the
//! raw component, `last − first + 1`).
//!
//! **Gap merge** is S4's per-bin closing in time: a cell in frame `t` and a cell of another
//! component in the same bin at `t − 2 … t − (gap + 1)` (nothing between) link the two
//! components. A per-bin history of the last `gap + 2` frames' component ids finds the links; a
//! link becomes a merge when both components are kept, and is dropped when either is discarded.
//! (S4's closing also connects a filled gap cell to a horizontally adjacent third component; that
//! corner case, which can only split a box, is not reproduced.)
//!
//! **Emission.** A kept, closed component is final once no later frame can link to it
//! (`t ≥ last + gap + 1`) and it has no link to an undecided component (waiting at most
//! `max_hold_frames` more). A kept component still open after `max_frames` is emitted as a split
//! and continues with fresh statistics.
//!
//! Components are merged eagerly (the smaller extent is relabelled in the run lists, the history
//! and the links), so every live id is a root. Per-bin accumulators cover only a component's bin
//! extent; the pool, extents and lists keep their capacity, so steady state allocates nothing.

use std::ops::Range;

use hk_model::Timestamp;

use crate::cfar::{CELL_NONE, CELL_SEED};

const NO_COMP: u32 = u32::MAX;
const NO_STAMP: u64 = u64::MAX;

/// Per-bin sums over a component's cells.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BinAcc {
    /// Cells in this bin.
    pub count: u32,
    /// Σ `P/F`.
    pub ratio: f32,
    /// Σ `max(P − F, 0)`, FS²/Hz.
    pub excess: f32,
    /// Σ `P/F` at the mirror bin `N − b` in the same frames.
    pub mirror_ratio: f32,
    /// Σ excess at the mirror bin.
    pub mirror_excess: f32,
}

/// Accumulators over a bin extent `[base, base + len)`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Extent {
    base: usize,
    data: Vec<BinAcc>,
}

impl Extent {
    /// Empties it (keeps capacity).
    pub fn clear(&mut self) {
        self.base = 0;
        self.data.clear();
    }

    /// Bins covered.
    pub fn range(&self) -> Range<usize> {
        self.base..self.base + self.data.len()
    }

    /// Covered bins.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Nothing covered.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Sums at bin `b` (must be covered).
    pub fn get(&self, b: usize) -> &BinAcc {
        &self.data[b - self.base]
    }

    /// `(bin, sums)` over the extent.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (usize, &BinAcc)> + '_ {
        self.data
            .iter()
            .enumerate()
            .map(|(i, a)| (self.base + i, a))
    }

    fn ensure(&mut self, lo: usize, hi: usize) {
        let zero = BinAcc::default();
        if self.data.is_empty() {
            self.base = lo;
            self.data.resize(hi - lo, zero);
            return;
        }
        if lo < self.base {
            let n = self.base - lo;
            let old = self.data.len();
            self.data.resize(old + n, zero);
            self.data.copy_within(0..old, n);
            self.data[..n].fill(zero);
            self.base = lo;
        }
        let end = self.base + self.data.len();
        if hi > end {
            self.data.resize(hi - self.base, zero);
        }
    }

    fn absorb(&mut self, other: &Extent) {
        if other.data.is_empty() {
            return;
        }
        let r = other.range();
        self.ensure(r.start, r.end);
        for (i, a) in other.data.iter().enumerate() {
            let t = &mut self.data[other.base + i - self.base];
            t.count += a.count;
            t.ratio += a.ratio;
            t.excess += a.excess;
            t.mirror_ratio += a.mirror_ratio;
            t.mirror_excess += a.mirror_excess;
        }
    }
}

/// Running per-segment counters, sampled before and after each frame so a box can count what
/// happened over its whole span (including gap frames) by difference.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cumulative {
    /// Frames.
    pub frames: u64,
    /// Clipped samples in frames over the clip-fraction threshold.
    pub clip_samples: u64,
    /// Frames over the clip-fraction threshold.
    pub clip_frames: u64,
    /// Frames whose floor was quantisation-limited.
    pub quantisation_frames: u64,
    /// Frames with no valid floor.
    pub invalid_frames: u64,
    /// Impulsive frames.
    pub impulsive_frames: u64,
}

/// A frame boundary: stream sample index and host time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    /// Stream sample index.
    pub sample: u64,
    /// Host time.
    pub time: Timestamp,
}

impl Default for Mark {
    fn default() -> Self {
        Self {
            sample: 0,
            time: Timestamp::UNIX_EPOCH,
        }
    }
}

/// Scalar statistics of a component.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stats {
    /// Passed the seed + duration test.
    pub kept: bool,
    /// Contains a seed cell.
    pub seed: bool,
    /// No cell since creation or the last split.
    pub fresh: bool,
    /// First frame with a cell (since the last split).
    pub first_frame: u64,
    /// Last frame with a cell.
    pub last_frame: u64,
    /// Start of the first frame.
    pub start: Mark,
    /// End of the last frame.
    pub end: Mark,
    /// Bin extent `[lo, hi)` since the last split.
    pub bin_lo: usize,
    /// See `bin_lo`.
    pub bin_hi: usize,
    hist_lo: usize,
    hist_hi: usize,
    /// Cells.
    pub pixels: u64,
    /// Cells in impulsive frames.
    pub impulsive_pixels: u64,
    /// Latest impulsive run the component had cells in.
    pub impulsive_run: u64,
    /// Peak `P/F`.
    pub ratio_peak: f32,
    /// Σ `P/F`.
    pub ratio_sum: f64,
    /// Σ SK.
    pub sk_sum: f64,
    /// Cells with SK.
    pub sk_count: u64,
    stamp: u64,
    frame_power: f64,
    /// Largest per-frame integrated excess over the component's cells, FS².
    pub peak_power: f64,
    /// Counters before the first frame.
    pub cum_before: Cumulative,
    /// Counters after the last frame.
    pub cum_after: Cumulative,
    /// Kept raw components merged across gaps into this one.
    pub boxes: u32,
}

impl Stats {
    fn fresh() -> Self {
        Self {
            kept: false,
            seed: false,
            fresh: true,
            first_frame: u64::MAX,
            last_frame: 0,
            start: Mark::default(),
            end: Mark::default(),
            bin_lo: usize::MAX,
            bin_hi: 0,
            hist_lo: usize::MAX,
            hist_hi: 0,
            pixels: 0,
            impulsive_pixels: 0,
            impulsive_run: 0,
            ratio_peak: 0.0,
            ratio_sum: 0.0,
            sk_sum: 0.0,
            sk_count: 0,
            stamp: NO_STAMP,
            frame_power: 0.0,
            peak_power: 0.0,
            cum_before: Cumulative::default(),
            cum_after: Cumulative::default(),
            boxes: 1,
        }
    }
}

/// A live component.
#[derive(Clone, Debug)]
pub struct Component {
    alive: bool,
    live_pos: usize,
    /// Scalars.
    pub s: Stats,
    /// Per-bin sums.
    pub acc: Extent,
}

impl Default for Component {
    fn default() -> Self {
        Self {
            alive: false,
            live_pos: 0,
            s: Stats::fresh(),
            acc: Extent::default(),
        }
    }
}

/// One frame's input to the labeller.
#[derive(Clone, Copy, Debug)]
pub struct FrameView<'a> {
    /// Frame index (consecutive within a segment, starting at ≥ 1).
    pub index: u64,
    /// Start of the frame.
    pub start: Mark,
    /// End of the frame.
    pub end: Mark,
    /// PSD.
    pub psd: &'a [f32],
    /// Floor reference.
    pub floor: &'a [f32],
    /// Spectral kurtosis (empty when off).
    pub sk: &'a [f32],
    /// Cell classes.
    pub codes: &'a [u8],
    /// Bin width, Hz.
    pub bin_width_hz: f64,
    /// Impulsive run id when the frame is impulsive.
    pub impulsive_run: Option<u64>,
    /// Counters before this frame.
    pub cum_before: Cumulative,
    /// Counters after this frame.
    pub cum_after: Cumulative,
}

/// Duration rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LabelParams {
    /// Minimum raw-component frames.
    pub min_frames: u32,
    /// Gap-merge frames.
    pub gap_frames: u32,
    /// Split an open kept component after this many frames.
    pub max_frames: u64,
    /// Extra frames a final component waits for undecided linked components.
    pub max_hold_frames: u32,
}

/// Why a component is ready.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ready {
    /// Finished: build a record, then [`Labeler::release`].
    Final,
    /// Too long: build a record, then [`Labeler::restart`].
    Split,
}

#[derive(Clone, Copy, Debug)]
struct Run {
    lo: usize,
    hi: usize,
    seed: bool,
    comp: u32,
}

/// The streaming labeller.
#[derive(Clone, Debug, Default)]
pub struct Labeler {
    pool: Vec<Component>,
    free: Vec<u32>,
    live: Vec<u32>,
    prev: Vec<Run>,
    cur: Vec<Run>,
    hist: Vec<u32>,
    rows: usize,
    bins: usize,
    links: Vec<(u32, u32)>,
    others: Vec<u32>,
    closed: Vec<u32>,
    /// Components ready after the last [`Labeler::process_frame`] / [`Labeler::close_all`].
    pub ready: Vec<(u32, Ready)>,
}

impl Labeler {
    /// A labeller for `bins` bins.
    pub fn new(bins: usize, gap_frames: u32) -> Self {
        let mut l = Self {
            pool: Vec::with_capacity(64),
            free: Vec::with_capacity(64),
            live: Vec::with_capacity(64),
            links: Vec::with_capacity(64),
            others: Vec::with_capacity(64),
            closed: Vec::with_capacity(64),
            ready: Vec::with_capacity(64),
            ..Self::default()
        };
        l.reset(bins, gap_frames);
        l
    }

    /// Discards every component and resizes for `bins` bins and `gap_frames`.
    pub fn reset(&mut self, bins: usize, gap_frames: u32) {
        while let Some(&id) = self.live.last() {
            self.free_comp(id);
        }
        if bins > self.bins {
            for c in &mut self.pool {
                c.acc.data.reserve_exact(bins);
            }
        }
        self.bins = bins;
        self.rows = gap_frames as usize + 2;
        self.hist.resize(self.rows * bins, NO_COMP);
        self.hist.fill(NO_COMP);
        let runs = bins / 2 + 1;
        self.prev.clear();
        self.cur.clear();
        self.prev.reserve(runs);
        self.cur.reserve(runs);
        self.links.clear();
        self.ready.clear();
    }

    /// Live components.
    pub fn live_count(&self) -> usize {
        self.live.len()
    }

    /// A component by id.
    pub fn component(&self, id: u32) -> &Component {
        &self.pool[id as usize]
    }

    fn alloc(&mut self) -> u32 {
        let id = match self.free.pop() {
            Some(id) => id,
            None => {
                // A new slot (a high-water event) reserves a full-band extent once, so reusing
                // slots for components of any width never reallocates.
                let mut c = Component::default();
                c.acc.data.reserve_exact(self.bins);
                self.pool.push(c);
                (self.pool.len() - 1) as u32
            }
        };
        let c = &mut self.pool[id as usize];
        c.alive = true;
        c.live_pos = self.live.len();
        c.s = Stats::fresh();
        c.acc.clear();
        self.live.push(id);
        id
    }

    fn free_comp(&mut self, id: u32) {
        let (lo, hi, alive, pos) = {
            let c = &self.pool[id as usize];
            (c.s.hist_lo, c.s.hist_hi, c.alive, c.live_pos)
        };
        if !alive {
            return;
        }
        if lo < hi && self.bins > 0 {
            for row in 0..self.rows {
                let base = row * self.bins;
                for h in &mut self.hist[base + lo..base + hi] {
                    if *h == id {
                        *h = NO_COMP;
                    }
                }
            }
        }
        self.links.retain(|&(a, b)| a != id && b != id);
        let c = &mut self.pool[id as usize];
        c.alive = false;
        c.acc.clear();
        self.live.swap_remove(pos);
        if pos < self.live.len() {
            let moved = self.live[pos];
            self.pool[moved as usize].live_pos = pos;
        }
        self.free.push(id);
    }

    /// Frees a [`Ready::Final`] component.
    pub fn release(&mut self, id: u32) {
        self.free_comp(id);
    }

    /// Resets a [`Ready::Split`] component's statistics; it stays kept and continues.
    pub fn restart(&mut self, id: u32) {
        let c = &mut self.pool[id as usize];
        let old = c.s;
        c.s = Stats {
            kept: old.kept,
            seed: old.seed,
            last_frame: old.last_frame,
            end: old.end,
            hist_lo: old.hist_lo,
            hist_hi: old.hist_hi,
            impulsive_run: old.impulsive_run,
            ..Stats::fresh()
        };
        c.acc.clear();
    }

    fn add_link(&mut self, a: u32, b: u32) {
        let key = (a.min(b), a.max(b));
        if a != b && !self.links.contains(&key) {
            self.links.push(key);
        }
    }

    /// Merges two live components; returns the survivor.
    fn merge(&mut self, a: u32, b: u32, t: u64, gap: bool) -> u32 {
        let (s, v) = if self.pool[a as usize].acc.len() >= self.pool[b as usize].acc.len() {
            (a, b)
        } else {
            (b, a)
        };
        let vs = self.pool[v as usize].s;
        let mut vacc = std::mem::take(&mut self.pool[v as usize].acc);
        self.pool[s as usize].acc.absorb(&vacc);
        vacc.clear();
        self.pool[v as usize].acc = vacc;
        {
            let ss = &mut self.pool[s as usize].s;
            ss.kept |= vs.kept;
            ss.seed |= vs.seed;
            if vs.first_frame < ss.first_frame {
                ss.first_frame = vs.first_frame;
                ss.start = vs.start;
                ss.cum_before = vs.cum_before;
            }
            ss.fresh &= vs.fresh;
            if vs.last_frame > ss.last_frame {
                ss.last_frame = vs.last_frame;
                ss.end = vs.end;
                ss.cum_after = vs.cum_after;
            }
            ss.bin_lo = ss.bin_lo.min(vs.bin_lo);
            ss.bin_hi = ss.bin_hi.max(vs.bin_hi);
            ss.hist_lo = ss.hist_lo.min(vs.hist_lo);
            ss.hist_hi = ss.hist_hi.max(vs.hist_hi);
            ss.pixels += vs.pixels;
            ss.impulsive_pixels += vs.impulsive_pixels;
            ss.impulsive_run = ss.impulsive_run.max(vs.impulsive_run);
            ss.ratio_peak = ss.ratio_peak.max(vs.ratio_peak);
            ss.ratio_sum += vs.ratio_sum;
            ss.sk_sum += vs.sk_sum;
            ss.sk_count += vs.sk_count;
            if vs.stamp == t {
                if ss.stamp == t {
                    ss.frame_power += vs.frame_power;
                } else {
                    ss.stamp = t;
                    ss.frame_power = vs.frame_power;
                }
            }
            ss.peak_power = ss.peak_power.max(vs.peak_power);
            ss.boxes = if gap {
                ss.boxes + vs.boxes
            } else {
                ss.boxes.max(vs.boxes)
            };
        }
        for r in self.prev.iter_mut().chain(self.cur.iter_mut()) {
            if r.comp == v {
                r.comp = s;
            }
        }
        if vs.hist_lo < vs.hist_hi {
            for row in 0..self.rows {
                let base = row * self.bins;
                for h in &mut self.hist[base + vs.hist_lo..base + vs.hist_hi] {
                    if *h == v {
                        *h = s;
                    }
                }
            }
        }
        let mut i = 0;
        while i < self.links.len() {
            let (mut x, mut y) = self.links[i];
            if x == v {
                x = s;
            }
            if y == v {
                y = s;
            }
            let key = (x.min(y), x.max(y));
            if x == y || self.links[..i].contains(&key) {
                self.links.swap_remove(i);
            } else {
                self.links[i] = key;
                i += 1;
            }
        }
        // Free the victim without touching history (already relabelled).
        let pos = self.pool[v as usize].live_pos;
        self.pool[v as usize].alive = false;
        self.live.swap_remove(pos);
        if pos < self.live.len() {
            let moved = self.live[pos];
            self.pool[moved as usize].live_pos = pos;
        }
        self.free.push(v);
        s
    }

    fn add_run(&mut self, id: u32, ri: usize, v: &FrameView<'_>, gap: u32) {
        let Run { lo, hi, seed, .. } = self.cur[ri];
        let t = v.index;
        let n = self.bins;
        let rows = self.rows as u64;
        let row = (t % rows) as usize;
        let Labeler {
            pool, hist, others, ..
        } = self;
        let c = &mut pool[id as usize];
        let s = &mut c.s;
        if s.fresh {
            s.fresh = false;
            s.first_frame = t;
            s.start = v.start;
            s.cum_before = v.cum_before;
        }
        if s.stamp != t {
            s.stamp = t;
            s.frame_power = 0.0;
        }
        s.last_frame = t;
        s.end = v.end;
        s.cum_after = v.cum_after;
        s.seed |= seed;
        s.bin_lo = s.bin_lo.min(lo);
        s.bin_hi = s.bin_hi.max(hi);
        s.hist_lo = s.hist_lo.min(lo);
        s.hist_hi = s.hist_hi.max(hi);
        if let Some(run) = v.impulsive_run {
            s.impulsive_run = run;
            s.impulsive_pixels += (hi - lo) as u64;
        }
        c.acc.ensure(lo, hi);
        others.clear();
        let have_sk = v.sk.len() == n;
        let df = v.bin_width_hz;
        let s = &mut c.s;
        for b in lo..hi {
            let p = v.psd[b];
            let f = v.floor[b].max(f32::MIN_POSITIVE);
            let ratio = p / f;
            let excess = (p - f).max(0.0);
            let mb = n - b;
            let (mr, me) = if mb < n && v.psd[mb].is_finite() {
                let mf = v.floor[mb].max(f32::MIN_POSITIVE);
                (v.psd[mb] / mf, (v.psd[mb] - mf).max(0.0))
            } else {
                (0.0, 0.0)
            };
            let a = &mut c.acc.data[b - c.acc.base];
            a.count += 1;
            a.ratio += ratio;
            a.excess += excess;
            a.mirror_ratio += mr;
            a.mirror_excess += me;
            s.ratio_peak = s.ratio_peak.max(ratio);
            s.ratio_sum += f64::from(ratio);
            s.pixels += 1;
            if have_sk && v.sk[b].is_finite() {
                s.sk_sum += f64::from(v.sk[b]);
                s.sk_count += 1;
            }
            s.frame_power += f64::from(excess) * df;
            hist[row * n + b] = id;
            for back in 2..=(u64::from(gap) + 1) {
                if back > t {
                    break;
                }
                let r = ((t - back) % rows) as usize;
                let o = hist[r * n + b];
                if o != NO_COMP && o != id && !others.contains(&o) {
                    others.push(o);
                }
            }
        }
        for k in 0..self.others.len() {
            let o = self.others[k];
            self.add_link(id, o);
        }
    }

    /// Labels one frame; afterwards [`Labeler::ready`] lists components to emit.
    pub fn process_frame(&mut self, v: &FrameView<'_>, p: &LabelParams) {
        assert_eq!(
            v.codes.len(),
            self.bins,
            "frame does not match the labeller"
        );
        let t = v.index;
        let n = self.bins;
        let row = (t % self.rows as u64) as usize;
        self.hist[row * n..(row + 1) * n].fill(NO_COMP);
        self.ready.clear();
        self.cur.clear();
        let mut b = 0;
        while b < n {
            if v.codes[b] == CELL_NONE {
                b += 1;
                continue;
            }
            let lo = b;
            let mut seed = false;
            while b < n && v.codes[b] != CELL_NONE {
                seed |= v.codes[b] == CELL_SEED;
                b += 1;
            }
            self.cur.push(Run {
                lo,
                hi: b,
                seed,
                comp: NO_COMP,
            });
        }
        let mut j = 0;
        for ri in 0..self.cur.len() {
            let (lo, hi) = (self.cur[ri].lo, self.cur[ri].hi);
            while j < self.prev.len() && self.prev[j].hi <= lo {
                j += 1;
            }
            let mut comp = NO_COMP;
            let mut m = j;
            while m < self.prev.len() && self.prev[m].lo < hi {
                let c = self.prev[m].comp;
                if comp == NO_COMP {
                    comp = c;
                } else if c != comp {
                    comp = self.merge(comp, c, t, false);
                }
                m += 1;
            }
            if comp == NO_COMP {
                comp = self.alloc();
            }
            self.cur[ri].comp = comp;
            self.add_run(comp, ri, v, p.gap_frames);
        }
        self.end_frame(t, p);
        std::mem::swap(&mut self.prev, &mut self.cur);
    }

    fn end_frame(&mut self, t: u64, p: &LabelParams) {
        for k in 0..self.live.len() {
            let id = self.live[k];
            let s = &mut self.pool[id as usize].s;
            if s.stamp == t {
                s.peak_power = s.peak_power.max(s.frame_power);
                if !s.kept && s.seed && t + 1 - s.first_frame >= u64::from(p.min_frames) {
                    s.kept = true;
                }
            }
        }
        while let Some(i) = self
            .links
            .iter()
            .position(|&(a, b)| self.pool[a as usize].s.kept && self.pool[b as usize].s.kept)
        {
            let (a, b) = self.links.swap_remove(i);
            self.merge(a, b, t, true);
        }
        self.closed.clear();
        for &id in &self.live {
            if self.pool[id as usize].s.last_frame < t {
                self.closed.push(id);
            }
        }
        let gap = u64::from(p.gap_frames);
        for k in 0..self.closed.len() {
            let id = self.closed[k];
            let s = self.pool[id as usize].s;
            if !s.kept {
                self.free_comp(id);
            } else if t > s.last_frame + gap {
                let pool = &self.pool;
                let pending = self.links.iter().any(|&(a, b)| {
                    (a == id && !pool[b as usize].s.kept) || (b == id && !pool[a as usize].s.kept)
                });
                if !pending || t >= s.last_frame + gap + 1 + u64::from(p.max_hold_frames) {
                    self.links.retain(|&(a, b)| a != id && b != id);
                    self.ready.push((id, Ready::Final));
                }
            }
        }
        for &id in &self.live {
            let s = &self.pool[id as usize].s;
            if s.kept && !s.fresh && s.last_frame == t && t + 1 - s.first_frame >= p.max_frames {
                self.ready.push((id, Ready::Split));
            }
        }
    }

    /// Closes everything (a transition or the end of the stream): kept components with cells
    /// become [`Ready::Final`] at once, the rest are discarded.
    pub fn close_all(&mut self) {
        self.ready.clear();
        self.closed.clear();
        for &id in &self.live {
            let s = &self.pool[id as usize].s;
            if s.kept && s.pixels > 0 {
                self.ready.push((id, Ready::Final));
            } else {
                self.closed.push(id);
            }
        }
        for k in 0..self.closed.len() {
            let id = self.closed[k];
            self.free_comp(id);
        }
        self.prev.clear();
        self.cur.clear();
        self.links.clear();
        self.hist.fill(NO_COMP);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cfar::CELL_REGION;

    struct Grid {
        bins: usize,
        frames: Vec<Vec<u8>>,
    }

    /// Runs a code grid through a labeller; returns `(first, last, lo, hi, boxes)` per emission.
    fn label(g: &Grid, p: LabelParams) -> Vec<(u64, u64, usize, usize, u32)> {
        let mut l = Labeler::new(g.bins, p.gap_frames);
        let psd = vec![10.0f32; g.bins];
        let floor = vec![1.0f32; g.bins];
        let mut out = Vec::new();
        let collect = |l: &mut Labeler, out: &mut Vec<_>| {
            let ready = std::mem::take(&mut l.ready);
            for &(id, kind) in &ready {
                let s = l.component(id).s;
                if s.pixels > 0 {
                    out.push((s.first_frame, s.last_frame, s.bin_lo, s.bin_hi, s.boxes));
                }
                match kind {
                    Ready::Final => l.release(id),
                    Ready::Split => l.restart(id),
                }
            }
        };
        for (i, codes) in g.frames.iter().enumerate() {
            let v = FrameView {
                index: i as u64 + 1,
                start: Mark::default(),
                end: Mark::default(),
                psd: &psd,
                floor: &floor,
                sk: &[],
                codes,
                bin_width_hz: 1.0,
                impulsive_run: None,
                cum_before: Cumulative::default(),
                cum_after: Cumulative::default(),
            };
            l.process_frame(&v, &p);
            collect(&mut l, &mut out);
        }
        l.close_all();
        collect(&mut l, &mut out);
        out
    }

    fn grid(bins: usize, rows: &[&str]) -> Grid {
        Grid {
            bins,
            frames: rows
                .iter()
                .map(|r| {
                    let mut v: Vec<u8> = r
                        .chars()
                        .map(|c| match c {
                            'S' => CELL_SEED,
                            'r' => CELL_REGION,
                            _ => CELL_NONE,
                        })
                        .collect();
                    v.resize(bins, CELL_NONE);
                    v
                })
                .collect(),
        }
    }

    const P: LabelParams = LabelParams {
        min_frames: 3,
        gap_frames: 2,
        max_frames: 1000,
        max_hold_frames: 64,
    };

    #[test]
    fn min_duration_is_tested_before_gap_merge() {
        // Single-frame seeds every other frame: closing first would make a 5-frame component.
        let g = grid(8, &["..S.", "....", "..S.", "....", "..S.", "....", "...."]);
        assert!(label(&g, P).is_empty());
        // Two 3-frame survivors 2 frames apart merge into one box.
        let g = grid(
            8,
            &[
                "..S.", "..r.", "..r.", "....", "....", "..r.", "..S.", "..r.", "....", "....",
                "....",
            ],
        );
        assert_eq!(label(&g, P), vec![(1, 8, 2, 3, 2)]);
        // A 3-frame gap does not merge.
        let g = grid(
            8,
            &[
                "..S.", "..r.", "..r.", "....", "....", "....", "..r.", "..S.", "..r.", "....",
                "....", "....", "....",
            ],
        );
        assert_eq!(label(&g, P).len(), 2);
    }

    #[test]
    fn seedless_regions_and_short_components_are_dropped() {
        let g = grid(8, &["rrr.", "rrr.", "rrr.", "rrr.", "...."]);
        assert!(label(&g, P).is_empty());
        let g = grid(8, &["SS..", "SS..", "....", "....", "...."]);
        assert!(label(&g, P).is_empty());
    }

    #[test]
    fn u_shapes_merge_and_diagonals_do_not() {
        // Two arms joined at the bottom: one component.
        let g = grid(8, &["S..S", "r..r", "r..r", "rrrr", "...."]);
        assert_eq!(label(&g, P), vec![(1, 4, 0, 4, 1)]);
        // Diagonal neighbours are not 4-connected (and each is 1 frame: nothing kept).
        let g = grid(8, &["S...", ".S..", "..S.", "...S", "...."]);
        assert!(label(&g, P).is_empty());
    }

    #[test]
    fn splits_long_components_and_continues() {
        let rows: Vec<&str> = std::iter::repeat_n(".SS.", 10)
            .chain(std::iter::repeat_n("....", 4))
            .collect();
        let g = grid(8, &rows);
        let p = LabelParams { max_frames: 4, ..P };
        assert_eq!(
            label(&g, p),
            vec![(1, 4, 1, 3, 1), (5, 8, 1, 3, 1), (9, 10, 1, 3, 1)]
        );
    }

    #[test]
    fn extent_grows_both_ways() {
        let mut e = Extent::default();
        e.ensure(10, 12);
        e.data[0].count = 5;
        e.ensure(8, 14);
        assert_eq!(e.range(), 8..14);
        assert_eq!(e.get(10).count, 5);
        assert_eq!(e.get(8).count, 0);
    }
}
