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
//! **Floor features and signals (T-033).** A filter-bank passband and a flat signal wider than a
//! block both raise the block floors over a span with sharp edges. What separates them is the
//! power statistics over time: inside a floor feature every bin is noise, so its frame-to-frame
//! power variance is that of the floor, `var(P)·n/mean(P)² ≈ 1` (`Gamma(n)`, `n` =
//! `FloorFrame::n_avg_effective`); a signal adds power whose variance differs (a steady signal
//! lowers the ratio to `(N/(S+N))²`, a keyed or fading one raises it). The guard keeps per-bin
//! running statistics (EMA, `stat_time_constant_frames`; impulsive frames skipped) and classifies
//! a feature by the median ratio over its **elevated** bins (above the geometric mean of its side
//! level and the jump): within `[floor_like_min, floor_like_max]` it is *floor-like*, outside it
//! *signal-like*, and with fewer than `stat_min_frames` frames or `stat_min_bins` elevated bins it
//! is *undecided*, which is handled as floor-like (conservative).
//!
//! **Plateaus.** An up event followed by a down event bounds a plateau when both outer sides (the
//! lowest block floor on the low side of each event) lie within `plateau_match_db` of the band's
//! median block floor. A **signal-like** plateau is exempt: there the per-frame floor is biased
//! low only over bins that carry the signal, so floor-branch detections are true (they find the
//! signal's edges). A floor-like plateau (a filter-bank passband) is guarded like a step. A
//! plateau with a notch on one side, or between two different floor levels (a notch plus a second
//! down-step, a staircase), is not one (T-028). The segment between a down event and the next up
//! event is a dip (a notch). An event is guarded when it bounds a dip, or when it bounds no exempt
//! plateau: bins from `centre(first pair) − margin` to `centre(last pair + sep) + margin` (margin
//! = `margin_blocks` × block) are guarded. Bins within the margin of a configured known response
//! edge (`DetectorConfig::response_edges_hz`) are always guarded.
//!
//! **Guarded bins use the wide reference where the shape explains the step** (T-028). The
//! tracker's wide reference is built on the shape-normalised block floors
//! (`FloorFrame::norm_block_floor`), so a notch or roll-off that the learned shape explains does
//! not bias it; an unlearned step, or a floor feature *above* the band floor (the shape never
//! learns upward features, so the wide reference reads it as a signal), does. Once the shape is
//! trusted (the detector passes it from the floor segment's `wide_min_frames`-th frame), a
//! normalised block is *unexplained* when it sits more than `wide_residual_db` below their median,
//! or lies in a residual jump above `wide_step_db` that the same dip/plateau rule guards (so a
//! residual plateau is explained only when it is signal-like, at any width: a narrow floor shelf
//! next to a learned step is unexplained, a flat signal next to a notch is not). A guarded zone
//! with no unexplained block within one block of it runs the floor branch against
//! `FloorFrame::wide_floor` ([`StepGuard::wide_mask`]); otherwise it is OS-only.
//!
//! **Wide reference over floor features.** When the floor branch runs on the wide reference
//! ([`WideView`]), the guard also finds where it has cut a plateau (`wide_floor` more than
//! `wide_cut_db` below `floor`, extended while it is below at all): a signal-like cut keeps the
//! wide reference (a wide signal's interior); a floor-like or undecided one is a floor feature and
//! runs on the per-frame floor instead ([`StepGuard::frame_ref_mask`]; guarded bins there are
//! OS-only).
//!
//! **OS-only bins** hold the OS branch's guard against the upper envelope of the raw block floors
//! within one block ([`StepGuard::os_floor`]): next to a sharp step the per-frame floor is biased
//! low and the OS reference cells straddle the step, so the plain guard let the OS branch fire on
//! the high side of a +6 dB edge. While a pair's jump stays above `release_db` in its direction,
//! an active pair is refreshed and a pending pair keeps its persistence count, so a step close to
//! `jump_db` neither flickers nor waits for consecutive exceedances.
//!
//! **Narrow floor features (T-316).** Everything above works on block floors, so the narrowest
//! feature it can see is about a block (256 bins, hop 64 ≈ 1.2 MHz of reference resolution). A
//! span of raised *noise* narrower than that falls between the two background estimators: the
//! floor branch compares it against a reference that is effectively the band level, and the OS
//! branch's reference cells at `±(G+1 … G+R)` straddle its edges, so `Z` is pulled towards the
//! level outside and every cell in the span reads as a target. Spans wider than the OS guard band
//! (`2G+1`, so from `2G+2`) and narrower than its reference span (`2(G+R)+1`) whose running mean sits more than
//! `narrow_feature_db` above the reference are therefore classified by the same power statistics
//! as a block-scale feature; a **floor-like** one takes its own per-bin running mean as the floor
//! ([`StepGuard::narrow_floor`]) and runs the floor branch alone ([`StepGuard::narrow_mask`]).
//! Outside the width band nothing changes: a narrower span fits inside the cell under test's guard
//! band and biases no reference cell, and a wider one already has a homogeneous OS reference.
//!
//! Three rules keep a real emission out of it. A span whose **median** running mean stands
//! `narrow_feature_max_db` (10 dB) or more above the reference is never classified at all — it is
//! too loud to be raised noise and is vetoed outright (T-937; see below). A bin must classify
//! floor-like for `persist_frames` consecutive frames before the guard acts there; and a span that
//! classifies **signal-like even once** is disqualified for the rest of the segment, because this
//! guard is only ever about a *stationary* noise feature and one signal-like verdict refutes that
//! premise. The asymmetry is
//! measured: on the 2026-09-15 FM capture every one of the three noise shelves reads floor-like in
//! 100.0 % of frames, while the three measured emissions read floor-like in 33 %, 3.0 % and 0.1 %
//! — a WFM station reads floor-like through a quiet passage of its programme audio. Neither a
//! longer time constant nor a timed hold separates them (at 512 frames the 99.6999 MHz station
//! still reads floor-like in 7.8 % of frames); latching the signal-like verdict does, and it fails
//! in the safe direction — a noise shelf misjudged once is merely detected as it was before.
//!
//! **The level cap (T-937).** The width band above is measured in *bins*, so it scales with the
//! resolution: at the 2.4 Msps fine sweep the pipeline picks 512 bins (4.69 kHz) and a 180 kHz
//! broadcast-FM station is ~38 of them — inside the band, and handed to the variance test like a
//! noise shelf. Programme audio is noise-like at 2 ms / 4.7 kHz cells, so a station reads
//! floor-like, and from the segment's `stat_min_frames`-th frame the guard made the station its
//! own floor and the station stopped being detected: the live FM survey of 2026-09-25 reported 349
//! candidates for ~19 stations, one station appearing as 13 boxes of 9–47 kHz — the loudest
//! excursions still clearing its own mean — with no WFM-width box for a chain to attach to. The
//! signal-like veto could not rescue it, because a sweep restarts the segment at every retune.
//! So the guard now refuses any span whose median elevation reaches `narrow_feature_max_db`
//! (10 dB, the SNR at which `Rules::marginal_snr_db` stops calling a detection marginal): raised
//! noise is a few dB — the case this guard was built on is 4.6 dB — and deleting a non-marginal
//! emission on a statistic measured to read a real emission floor-like in up to a third of its
//! frames is the wrong trade. The median rather than the peak, because a few bins 10 dB up are
//! what a noise shelf's own fluctuation looks like.
//!
//! **Limits.** A stationary noise-like emission (Gaussian at the bin level: OFDM, a wideband noise
//! jammer) has the floor's statistics and is taken for a floor feature: its edges are OS-only and,
//! on the wide reference, its interior runs on the per-frame floor (which reads it as floor). The
//! floor tracker's change episodes (AWARE-006) report such rises. During a feature's first
//! `stat_min_frames` frames it is guarded.
//!
//! **Impact on AWARE-006 (T-036, measured; `tests/aware_006_wide_emissions.rs`).** 2 Msps at
//! GNSS L1, 1024 bins × 10 averages (5.12 ms frames), +10 dB from t0 for 2 s:
//!
//! | Emission | Floor episode | CFAR (Wide reference) | Steady reference bias |
//! |---|---|---|---|
//! | Broadband noise jammer (whole span) | one `NoiseLike` Rise, step 9.6 dB, SK 1.00, onset −1.6 ms, Returned End | no wide detection (as before T-033: every reference reads a span-wide rise as floor) | +9.6 dB |
//! | Partial-band noise jammer (800 kHz) | one `NoiseLike` Rise, step 9.6 dB, SK 1.00, onset −1.6 ms, Returned End; extent 375 kHz of whole 256-bin blocks at T-036, refined per bin to the emission's edges since T-038 | one edge-to-edge detection from t0 to t0 + 0.75 s, then nothing | +9.7 dB (wide floor +0.05, per-frame +6.8) |
//! | Steady OFDM (1 MHz, 64 QPSK subcarriers) | T-036: one `NoiseLike` Rise (SK 0.89); since T-038 `Structured` (anti-correlated bin power fluctuations), so no floor-rise Anomaly | one or two detections (seed-dependent split) reaching both edges from t0 + 0.1 s to t0 + 0.83 s, then nothing | +9.6 dB |
//!
//! **Decision: accepted, no change.** AWARE-006 does not run on the detection reference: the
//! floor tracker's episodes come from the raw block floors, and every noise jammer still yields
//! the `NoiseLike` Rise on which `hk_context::FloorAnomalies` opens the Anomaly (end to end:
//! `hk-context/tests/aware_006_e2e.rs`). The CFAR sees such an emission's edges only until the
//! guard classifies it floor-like (≈ 0.75 s); afterwards it is covered by the episode, not by
//! detections. The two consequences T-036 left open were closed by T-038 (`hk_dsp::floor`
//! discriminator and episode extent): a steady OFDM transmitter is a `Structured` episode, and a
//! partial-band episode's frequency extent is refined per bin (within 20 % of the bandwidth).
//!
//! **What does not trigger it.** A narrowband emitter (an FM station is ~40 of 256 bins) does not
//! move a block floor.
//!
//! The guard needs the floor tracker's block layout (`blocks`, default 256/64). When the frame's
//! block count does not match it, the jump test is unavailable for that frame and only the
//! configured edges are guarded (OS-only; counted by the detector).

use std::ops::Range;

use hk_dsp::floor::BlockConfig;

use crate::config::CfarWindow;
use crate::rules::Geometry;

/// Floor-step guard settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StepGuardConfig {
    /// A block-floor jump above this is a step, dB (5: a +6 dB passband's expected block jump is
    /// 6.0 dB, so a 6 dB threshold caught it in only about half the frames).
    pub jump_db: f64,
    /// Consecutive frames a jump must persist to activate, frames (2).
    pub persist_frames: u32,
    /// Frames a pair stays active after its jump disappears (32).
    pub hold_frames: u32,
    /// An active pair's hold is refreshed while its jump stays above this in the same direction,
    /// dB (3), so a step near `jump_db` (a +6 dB passband) does not flicker.
    pub release_db: f64,
    /// Guard margin beyond the event's block centres, in blocks (1).
    pub margin_blocks: f64,
    /// Both sides of a plateau must lie within this of the median block floor, dB (3).
    pub plateau_match_db: f64,
    /// A shape-normalised block floor more than this below their median is unexplained, dB (1.5).
    pub wide_residual_db: f64,
    /// A shape-normalised block-floor jump above this that bounds no exempt plateau is
    /// unexplained, dB (3).
    pub wide_step_db: f64,
    /// Where the wide reference lies more than this below the per-frame floor it has cut a
    /// plateau, dB (3).
    pub wide_cut_db: f64,
    /// Lower bound of the floor-like band of a feature's median `var(P)·n/mean(P)²` (0.5).
    pub floor_like_min: f64,
    /// Upper bound of the floor-like band (2).
    pub floor_like_max: f64,
    /// Frames of power statistics before a feature is classified (16).
    pub stat_min_frames: u64,
    /// Time constant of the per-bin power statistics, frames (32).
    pub stat_time_constant_frames: f64,
    /// A feature with fewer elevated bins than this is undecided (32).
    pub stat_min_bins: usize,
    /// Frames of a floor segment before the learned shape is trusted (32: the tracker applies it
    /// from the segment's 32nd frame).
    pub wide_min_frames: u64,
    /// The floor tracker's block layout ([`BlockConfig::default`]: 256 / 64).
    pub blocks: BlockConfig,
    /// A span whose running mean sits more than this above the floor reference is a candidate
    /// **narrow floor feature**, dB (1.5). Same budget as `wide_residual_db`, and for the same
    /// reason: it is the residual the floor model already admits it cannot explain, not a
    /// detection threshold.
    pub narrow_feature_db: f64,
    /// T-937: the guard never acts on a span whose running mean sits, in the **median** over the
    /// span, this far above the floor reference, dB (10). *Raised noise* is what this guard is for, and raised
    /// noise is a few dB: the case it was built on is a 75 kHz shelf 4.6 dB above the band floor.
    /// A span standing 10 dB clear of the reference is an emission by the detector's own
    /// published standard — [`crate::Rules::marginal_snr_db`] is the SNR at which a detection
    /// stops being `marginal` — and suppressing a non-marginal emission on a variance statistic
    /// that reads a real emission floor-like in up to a third of its frames (see the asymmetry
    /// measured below) is the wrong trade in the wrong direction. Above the cap the span is
    /// **vetoed** for the segment, exactly as a signal-like verdict vetoes it.
    pub narrow_feature_max_db: f64,
    /// The detector's OS-CFAR window: the narrow-feature width band is derived from it — wider
    /// than the guard band (`2G+1`, so from `2G+2`) and narrower than the reference span
    /// (`2(G+R)+1`). Outside that band the OS branch is sound and nothing is done.
    pub cfar_window: CfarWindow,
}

impl Default for StepGuardConfig {
    fn default() -> Self {
        Self {
            jump_db: 5.0,
            persist_frames: 2,
            hold_frames: 32,
            release_db: 3.0,
            margin_blocks: 1.0,
            plateau_match_db: 3.0,
            wide_residual_db: 1.5,
            wide_step_db: 3.0,
            wide_cut_db: 3.0,
            floor_like_min: 0.5,
            floor_like_max: 2.0,
            stat_min_frames: 16,
            stat_time_constant_frames: 32.0,
            stat_min_bins: 32,
            wide_min_frames: 32,
            blocks: BlockConfig::default(),
            narrow_feature_db: 1.5,
            narrow_feature_max_db: 10.0,
            cfar_window: CfarWindow::default(),
        }
    }
}

/// The floor tracker's learned shape and shape-normalised block floors, once trusted.
#[derive(Clone, Copy, Debug)]
pub struct ShapeView<'a> {
    /// `FloorFrame::shape`.
    pub shape: &'a [f32],
    /// `FloorFrame::norm_block_floor`.
    pub norm_block_floor: &'a [f32],
}

/// The per-frame and wide floors, when the floor branch runs on the wide reference.
#[derive(Clone, Copy, Debug)]
pub struct WideView<'a> {
    /// `FloorFrame::floor`.
    pub floor: &'a [f32],
    /// `FloorFrame::wide_floor`.
    pub wide_floor: &'a [f32],
}

/// One frame's guard inputs.
#[derive(Clone, Copy, Debug)]
pub struct GuardFrame<'a> {
    /// The frame's PSD (`Spectrum::psd`).
    pub psd: &'a [f32],
    /// `FloorFrame::n_avg_effective`.
    pub n_avg_effective: f64,
    /// `FloorFrame::impulsive` (the frame does not update the power statistics).
    pub impulsive: bool,
    /// `FloorFrame::block_floor` (raw block FCME).
    pub block_floor: &'a [f32],
    /// The trusted shape (`None`: guarded zones are OS-only).
    pub shape: Option<ShapeView<'a>>,
    /// The per-frame and wide floors when the configured reference is the wide one.
    pub wide: Option<WideView<'a>>,
    /// The floor reference the detector is about to use (the configured [`FloorReference`]
    /// trace), against which narrow floor features are found.
    ///
    /// [`FloorReference`]: crate::FloorReference
    pub reference: &'a [f32],
}

#[derive(Clone, Copy, Debug)]
struct Event {
    dir: i8,
    first_pair: usize,
    last_pair: usize,
    lo: isize,
    hi: isize,
    guarded: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Undecided,
    FloorLike,
    SignalLike,
}

/// Per-bin running power statistics.
#[derive(Clone, Copy)]
struct Stats<'a> {
    mean: &'a [f32],
    var: &'a [f32],
    ready: bool,
    n_eff: f32,
    lo: f32,
    hi: f32,
    min_bins: usize,
}

/// Median of `v` (reorders it); `None` when empty or all-NaN.
fn median_of(v: &mut [f32]) -> Option<f32> {
    if v.is_empty() {
        return None;
    }
    let mid = v.len() / 2;
    let (_, &mut m, _) = v.select_nth_unstable_by(mid, f32::total_cmp);
    (!m.is_nan()).then_some(m)
}

impl Stats<'_> {
    /// Classifies the bins of `bins` for which `elevated(bin, mean)` holds.
    fn class(
        &self,
        bins: Range<usize>,
        elevated: impl Fn(usize, f32) -> bool,
        scratch: &mut Vec<f32>,
    ) -> Class {
        if !self.ready {
            return Class::Undecided;
        }
        scratch.clear();
        for b in bins {
            if elevated(b, self.mean[b]) {
                scratch.push(self.var[b]);
            }
        }
        if scratch.len() < self.min_bins.max(1) {
            return Class::Undecided;
        }
        let mid = scratch.len() / 2;
        let (_, &mut v, _) = scratch.select_nth_unstable_by(mid, f32::total_cmp);
        let r = v * self.n_eff;
        if r.is_nan() {
            Class::Undecided
        } else if (self.lo..=self.hi).contains(&r) {
            Class::FloorLike
        } else {
            Class::SignalLike
        }
    }
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
    plateaus: Vec<bool>,
    mask: Vec<bool>,
    wide: Vec<bool>,
    frame_ref: Vec<bool>,
    os_floor: Vec<f32>,
    unexplained: Vec<bool>,
    scratch: Vec<f32>,
    mean: Vec<f32>,
    var: Vec<f32>,
    narrow: Vec<bool>,
    narrow_floor: Vec<f32>,
    narrow_persist: Vec<u32>,
    narrow_veto: Vec<bool>,
    stat_frames: u64,
    available: bool,
    guarded: usize,
    wide_bins: usize,
    frame_ref_bins: usize,
    narrow_bins: usize,
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

/// Marks guarded events: those bounding a dip, or bounding no exempt plateau. A plateau is an up
/// event and the next down event whose two low sides lie within `near` of `band` (`level` holds
/// the block values the events were built from) and for which `exempt(up, down, side)` holds
/// (`side`: the higher of the two low sides). `plateaus` is scratch.
fn classify_events(
    events: &mut [Event],
    plateaus: &mut Vec<bool>,
    level: &[f32],
    sep: usize,
    band: f32,
    near: f32,
    mut exempt: impl FnMut(&Event, &Event, f32) -> bool,
) {
    let floor_level = |x: f32| x > 0.0 && x <= band * near && x * near >= band;
    let low_side = |e: &Event| {
        if e.dir > 0 {
            min_of(&level[e.first_pair..=e.last_pair])
        } else {
            min_of(&level[e.first_pair + sep..=e.last_pair + sep])
        }
    };
    plateaus.clear();
    for w in events.windows(2) {
        let (up, down) = (&w[0], &w[1]);
        let (a, b) = (low_side(up), low_side(down));
        plateaus.push(
            up.dir > 0
                && down.dir < 0
                && floor_level(a)
                && floor_level(b)
                && exempt(up, down, a.max(b)),
        );
    }
    for k in 0..events.len() {
        let e = events[k];
        let prev = k.checked_sub(1).map(|p| events[p]);
        let next = events.get(k + 1).copied();
        let plateau_before = k > 0 && plateaus[k - 1];
        let plateau_after = plateaus.get(k).copied().unwrap_or(false);
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
            plateaus: Vec::new(),
            mask: Vec::new(),
            wide: Vec::new(),
            frame_ref: Vec::new(),
            os_floor: Vec::new(),
            unexplained: Vec::new(),
            scratch: Vec::new(),
            mean: Vec::new(),
            var: Vec::new(),
            narrow: Vec::new(),
            narrow_floor: Vec::new(),
            narrow_persist: Vec::new(),
            narrow_veto: Vec::new(),
            stat_frames: 0,
            available: false,
            guarded: 0,
            wide_bins: 0,
            frame_ref_bins: 0,
            narrow_bins: 0,
        }
    }

    /// Settings.
    pub fn config(&self) -> &StepGuardConfig {
        &self.cfg
    }

    fn size(&mut self, bins: usize) {
        if self.mask.len() != bins {
            self.mask.resize(bins, true);
            self.wide.resize(bins, false);
            self.frame_ref.resize(bins, false);
            self.os_floor.resize(bins, 0.0);
            self.mean.resize(bins, 0.0);
            self.var.resize(bins, 0.0);
            self.narrow.resize(bins, false);
            self.narrow_floor.resize(bins, 0.0);
            self.narrow_persist.resize(bins, 0);
            self.narrow_veto.resize(bins, false);
            self.scratch.reserve(bins);
            self.stat_frames = 0;
        }
    }

    /// Starts a segment of `bins` bins (clears the persistence state and the power statistics).
    pub fn reset(&mut self, bins: usize) {
        self.size(bins);
        self.mask.fill(true);
        self.wide.fill(false);
        self.frame_ref.fill(false);
        self.os_floor.fill(0.0);
        self.mean.fill(0.0);
        self.var.fill(0.0);
        self.narrow.fill(false);
        self.narrow_floor.fill(0.0);
        self.narrow_persist.fill(0);
        self.narrow_veto.fill(false);
        self.narrow_bins = 0;
        self.stat_frames = 0;
        self.persist.fill(0);
        self.hold.fill(0);
        self.dir.fill(0);
        self.guarded = 0;
        self.wide_bins = 0;
        self.frame_ref_bins = 0;
        self.available = false;
    }

    /// Folds one frame into the per-bin power statistics.
    fn observe(&mut self, psd: &[f32], impulsive: bool) {
        if impulsive || psd.len() != self.mean.len() {
            return;
        }
        let k = self.stat_frames as f64;
        let tau = self.cfg.stat_time_constant_frames.max(1.0);
        let am = (1.0 / (k + 1.0)).max(1.0 / tau) as f32;
        let av = (1.0 / k.max(1.0)).max(1.0 / tau) as f32;
        for ((m, v), &p) in self.mean.iter_mut().zip(self.var.iter_mut()).zip(psd) {
            if !(p.is_finite() && p > 0.0) {
                continue;
            }
            if (*m).partial_cmp(&0.0).is_none_or(|o| o.is_le()) {
                *m = p;
                *v = 0.0;
                continue;
            }
            let d = (p - *m) / *m;
            *v += av * (d * d - *v);
            *m += am * (p - *m);
        }
        self.stat_frames += 1;
    }

    /// Updates the masks from one frame and returns the per-frame mask: `true` where the floor
    /// branch may run on the configured reference.
    pub fn update(
        &mut self,
        frame: GuardFrame<'_>,
        geometry: &Geometry,
        edges_hz: &[f64],
    ) -> &[bool] {
        let n = geometry.bins;
        self.size(n);
        self.mask.fill(true);
        self.wide.fill(false);
        self.frame_ref.fill(false);
        self.os_floor.fill(0.0);
        self.narrow.fill(false);
        self.narrow_floor.fill(0.0);
        self.observe(frame.psd, frame.impulsive);
        let block = self.cfg.blocks.block_bins.min(n).max(1);
        let hop = self.cfg.blocks.hop_bins.max(1);
        let margin = (self.cfg.margin_blocks * block as f64).round() as isize;
        let expected = (n - block) / hop + 1;
        let block_floor = frame.block_floor;
        self.available = block_floor.len() == expected;
        self.events.clear();
        let near = ratio_of(self.cfg.plateau_match_db);
        let Self {
            cfg,
            persist,
            hold,
            dir,
            events,
            norm_events,
            plateaus,
            mask,
            wide,
            frame_ref,
            os_floor,
            unexplained,
            scratch,
            mean,
            var,
            narrow,
            narrow_floor,
            narrow_persist,
            narrow_veto,
            ..
        } = self;
        let stats = Stats {
            mean,
            var,
            ready: self.stat_frames >= cfg.stat_min_frames,
            n_eff: frame.n_avg_effective as f32,
            lo: cfg.floor_like_min as f32,
            hi: cfg.floor_like_max as f32,
            min_bins: cfg.stat_min_bins,
        };
        let centre = |i: usize| (i * hop) as isize + (block as isize - 1) / 2;
        let sep = block.div_ceil(hop);
        let bins_of = |up: &Event, down: &Event| {
            let lo = centre(up.first_pair).clamp(0, n as isize) as usize;
            let hi = (centre(down.last_pair + sep) + 1).clamp(0, n as isize) as usize;
            lo..hi.max(lo)
        };
        let mut wide_ready = false;
        if self.available {
            if persist.len() != expected {
                persist.clear();
                persist.resize(expected, 0);
                hold.clear();
                hold.resize(expected, 0);
                dir.clear();
                dir.resize(expected, 0);
                unexplained.clear();
                unexplained.resize(expected, false);
                events.reserve(expected);
                norm_events.reserve(expected);
                plateaus.reserve(expected);
            }
            let pairs = expected.saturating_sub(sep);
            let ratio = ratio_of(cfg.jump_db);
            let release = ratio_of(cfg.release_db);
            let span = |i: usize| (centre(i) - margin, centre(i + sep) + margin + 1);
            for i in 0..pairs {
                let d = jump_dir(block_floor[i], block_floor[i + sep], ratio);
                if d != 0 {
                    persist[i] = persist[i].saturating_add(1);
                    dir[i] = d;
                    if persist[i] >= cfg.persist_frames {
                        hold[i] = cfg.hold_frames.max(1);
                    }
                } else {
                    // Above `release_db` in the same direction: an active pair is refreshed and a
                    // pending one keeps its count (a step near `jump_db` activates promptly).
                    let near_jump =
                        jump_dir(block_floor[i], block_floor[i + sep], release) == dir[i];
                    if !near_jump {
                        persist[i] = 0;
                    }
                    hold[i] = if near_jump && hold[i] > 0 {
                        cfg.hold_frames.max(1)
                    } else {
                        hold[i].saturating_sub(1)
                    };
                }
            }
            {
                let (hold, dir) = (&*hold, &*dir);
                group_events(
                    events,
                    pairs,
                    |i| if hold[i] > 0 { dir[i] } else { 0 },
                    span,
                );
            }
            if !events.is_empty() {
                let band = median(block_floor, scratch);
                let thr = ratio.sqrt();
                // `scratch` is reused inside: take it out for the closure.
                let mut buf = std::mem::take(scratch);
                classify_events(
                    events,
                    plateaus,
                    block_floor,
                    sep,
                    band,
                    near,
                    |up, down, side| {
                        let t = side * thr;
                        stats.class(bins_of(up, down), |_, m| m > t, &mut buf) == Class::SignalLike
                    },
                );
                *scratch = buf;
                for e in events.iter().filter(|e| e.guarded) {
                    let lo = e.lo.clamp(0, n as isize) as usize;
                    let hi = e.hi.clamp(0, n as isize) as usize;
                    mask[lo..hi].fill(false);
                }
            }
            if let Some(v) = frame
                .shape
                .filter(|v| v.shape.len() == n && v.norm_block_floor.len() == expected)
                && (!events.is_empty() || !edges_hz.is_empty())
            {
                let norm = v.norm_block_floor;
                let med = median(norm, scratch);
                if med > 0.0 {
                    let residual = ratio_of(cfg.wide_residual_db);
                    for (u, &x) in unexplained.iter_mut().zip(norm) {
                        // NaN (incomparable) counts as unexplained.
                        *u = (x * residual).partial_cmp(&med).is_none_or(|o| o.is_lt());
                    }
                    let step = ratio_of(cfg.wide_step_db);
                    group_events(
                        norm_events,
                        pairs,
                        |i| jump_dir(norm[i], norm[i + sep], step),
                        span,
                    );
                    let thr = step.sqrt();
                    let shape = v.shape;
                    let mut buf = std::mem::take(scratch);
                    classify_events(
                        norm_events,
                        plateaus,
                        norm,
                        sep,
                        med,
                        near,
                        |up, down, side| {
                            let t = side * thr;
                            stats.class(bins_of(up, down), |b, m| m > t * shape[b], &mut buf)
                                == Class::SignalLike
                        },
                    );
                    *scratch = buf;
                    for e in norm_events.iter().filter(|e| e.guarded) {
                        unexplained[e.first_pair..=e.last_pair + sep].fill(true);
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
                mask[lo..hi].fill(false);
            }
        }
        if wide_ready {
            // A zone is explained when no block within one block of it is unexplained (the
            // interpolation reaches one block centre beyond the zone).
            let nb = expected;
            let valid = |lo: usize, hi: usize| {
                let j_hi = ((hi + block) / hop).min(nb - 1);
                let j_lo = (lo.saturating_sub(block) / hop).min(j_hi);
                !unexplained[j_lo..=j_hi].iter().any(|&u| u)
            };
            // Valid zones first, then invalid ones, so an overlap stays OS-only.
            for pass in [true, false] {
                for e in events.iter().filter(|e| e.guarded) {
                    let lo = e.lo.clamp(0, n as isize) as usize;
                    let hi = e.hi.clamp(0, n as isize) as usize;
                    if lo < hi && valid(lo, hi) == pass {
                        wide[lo..hi].fill(pass);
                    }
                }
                for &f in edges_hz {
                    if let Some((lo, hi)) = edge_range(f)
                        && lo < hi
                        && valid(lo, hi) == pass
                    {
                        wide[lo..hi].fill(pass);
                    }
                }
            }
        }
        if let Some(w) = frame
            .wide
            .filter(|w| w.floor.len() == n && w.wide_floor.len() == n)
        {
            // Where the wide reference cut a plateau: a floor-like or undecided cut is a floor
            // feature and runs on the per-frame floor.
            let seed = ratio_of(cfg.wide_cut_db);
            let below = |b: usize| w.wide_floor[b] < w.floor[b];
            let mut b = 0;
            while b < n {
                if (w.wide_floor[b] * seed)
                    .partial_cmp(&w.floor[b])
                    .is_none_or(|o| o.is_ge())
                {
                    b += 1;
                    continue;
                }
                let mut lo = b;
                while lo > 0 && below(lo - 1) {
                    lo -= 1;
                }
                let mut hi = b + 1;
                while hi < n && below(hi) {
                    hi += 1;
                }
                let class = stats.class(lo..hi, |k, m| m > w.wide_floor[k] * seed, scratch);
                if class != Class::SignalLike {
                    frame_ref[lo..hi].fill(true);
                }
                b = hi;
            }
            for (x, &f) in wide.iter_mut().zip(frame_ref.iter()) {
                *x &= !f;
            }
        }
        // OS-only bins: the OS guard holds against the upper envelope of the block floors within
        // one block (the per-frame floor is biased low next to a sharp step, where the OS
        // branch's reference cells straddle it too).
        if self.available && mask.iter().any(|&ok| !ok) {
            for (j, &f) in block_floor.iter().enumerate() {
                let c = centre(j);
                let lo = (c - block as isize).clamp(0, n as isize) as usize;
                let hi = (c + block as isize + 1).clamp(0, n as isize) as usize;
                for b in lo..hi {
                    if !mask[b] && !wide[b] && f.is_finite() {
                        os_floor[b] = os_floor[b].max(f);
                    }
                }
            }
        }
        // Narrow floor features (T-316). A span of raised *noise* between the OS window's guard
        // band and its reference span is invisible to everything above: the block floor (256 bins,
        // hop 64) cannot resolve it, so the floor branch runs against a reference that is the band
        // level rather than the local one; and the OS branch's reference cells straddle its edges,
        // so `Z` is pulled towards the level outside and the cell reads as a target. On the real
        // 2026-09-15 FM capture a 75 kHz shelf 4.6 dB above the band floor put the OS branch's
        // per-cell seed rate at 3.9e-3 against its 1e-6 design and made 875 boxes in 45 s.
        // The span's own running mean is the only unbiased reference there.
        if frame.reference.len() == n {
            let w = cfg.cfar_window;
            let min_bins = 2 * w.guard_per_side + 2;
            let max_bins = 2 * (w.guard_per_side + w.reference_per_side) + 1;
            let thr = ratio_of(cfg.narrow_feature_db);
            let cap = ratio_of(cfg.narrow_feature_max_db);
            let reference = frame.reference;
            // A span of at most `2G+1` bins fits inside the cell under test's guard band and
            // reaches no reference cell, so it cannot bias `Z` and is a target by construction;
            // one of `2(G+R)+1` or more leaves every reference cell of its interior inside it,
            // where the OS branch is homogeneous and already sound.
            let nstats = Stats { min_bins, ..stats };
            let elevated =
                |mean: &[f32], b: usize| reference[b] > 0.0 && mean[b] > reference[b] * thr;
            let mut b = 0;
            while b < n {
                if !elevated(mean, b) {
                    b += 1;
                    continue;
                }
                let lo = b;
                while b < n && elevated(mean, b) {
                    b += 1;
                }
                // Only a *confidently* floor-like span becomes floor: acting here removes
                // detections, so `Undecided` — and every signal-like span — is left alone. This is
                // the opposite of the block-scale guard's convention, where acting means being
                // more careful and `Undecided` is handled as floor-like.
                if (min_bins..=max_bins).contains(&(b - lo)) {
                    // T-937: too loud to be raised noise. A span whose median elevation reaches
                    // `narrow_feature_max_db` above the reference is an emission the detector
                    // would report non-marginal, so it is vetoed rather than classified. The
                    // median, not the peak: a few loud bins are what a noise shelf's own
                    // fluctuation looks like (the +8 dB shelf in `false_alarm.rs` has bins 10 dB
                    // up), while a broadcast station is elevated across its whole span. Before this cap the
                    // fine-sweep geometry (2.4 Msps, 512 bins: a 180 kHz broadcast-FM station is
                    // ~38 bins, inside the width band) put every FM station in the band through
                    // the variance test, and a station whose programme audio is noise-like reads
                    // floor-like: the guard took the station's own running mean as the floor from
                    // the 16th frame of the segment and the station stopped being detected
                    // altogether, leaving only the sporadic narrow boxes its loudest excursions
                    // still made. A sweep restarts the segment at every retune, so the veto latch
                    // below never got the signal-like frame that would have saved it.
                    scratch.clear();
                    scratch.extend(
                        (lo..b)
                            .filter(|&k| reference[k] > 0.0)
                            .map(|k| mean[k] / reference[k]),
                    );
                    if median_of(scratch).is_some_and(|m| m > cap) {
                        narrow_veto[lo..b].fill(true);
                        continue;
                    }
                    match nstats.class(lo..b, |k, m| m > reference[k] * thr, scratch) {
                        Class::FloorLike => {
                            narrow[lo..b].fill(true);
                            narrow_floor[lo..b].copy_from_slice(&mean[lo..b]);
                        }
                        // Signal-like even **once** disqualifies the span for the rest of the
                        // segment. This guard exists only for a *stationary* noise feature, so
                        // any evidence of non-stationarity refutes its premise outright; a
                        // segment ends on a retune, gain change or discontinuity, so the latch is
                        // bounded and self-cleaning.
                        //
                        // The asymmetry is measured, not assumed. On the 2026-09-15 capture at
                        // the default 32-frame (68 ms) time constant, the share of frames whose
                        // statistic lands inside the floor-like band is 100.0 % for all three
                        // noise shelves but 33 %, 3.0 % and 0.1 % for the three measured
                        // emissions: noise reads as noise *always*, a real emission only
                        // sometimes — a WFM station reads floor-like through a quiet passage of
                        // its programme audio. Neither a longer time constant nor a timed hold
                        // fixes that: at 512 frames the 99.6999 MHz station still reads
                        // floor-like in 7.8 % of frames, and a 32-frame hold expires inside a
                        // quiet passage, leaving it cut from 46 boxes into 68.
                        Class::SignalLike => narrow_veto[lo..b].fill(true),
                        Class::Undecided => {}
                    }
                }
            }
            // A bin must classify floor-like for `persist_frames` consecutive frames, and must
            // never have classified signal-like in this segment, before the guard acts there.
            for ((p, on), veto) in narrow_persist
                .iter_mut()
                .zip(narrow.iter_mut())
                .zip(narrow_veto.iter())
            {
                if *veto {
                    *on = false;
                    *p = 0;
                } else if *on {
                    *p = p.saturating_add(1);
                    *on = *p >= cfg.persist_frames;
                } else {
                    *p = 0;
                }
            }
        }
        self.guarded = self.mask.iter().filter(|&&ok| !ok).count();
        self.wide_bins = self.wide.iter().filter(|&&w| w).count();
        self.frame_ref_bins = self.frame_ref.iter().filter(|&&f| f).count();
        self.narrow_bins = self.narrow.iter().filter(|&&x| x).count();
        &self.mask
    }

    /// The last per-frame mask (`true`: floor branch allowed on the configured reference).
    pub fn mask(&self) -> &[bool] {
        &self.mask
    }

    /// The last wide mask (`true`: a guarded bin whose floor branch runs on the wide reference).
    pub fn wide_mask(&self) -> &[bool] {
        &self.wide
    }

    /// The last frame-reference mask (`true`: the wide reference cut a floor feature there, so the
    /// floor branch and the OS guard use the per-frame floor). Only set when the frame carried a
    /// [`WideView`].
    pub fn frame_ref_mask(&self) -> &[bool] {
        &self.frame_ref
    }

    /// The last OS-guard floor of OS-only bins (guarded, not on the wide reference): the upper
    /// envelope of the raw block floors within one block; 0 elsewhere. The detector raises its
    /// floor reference there to at least this.
    pub fn os_floor(&self) -> &[f32] {
        &self.os_floor
    }

    /// Bins guarded in the last frame (both wide-reference and OS-only).
    pub fn guarded_bins(&self) -> usize {
        self.guarded
    }

    /// Guarded bins that ran the floor branch on the wide reference in the last frame.
    pub fn wide_bins(&self) -> usize {
        self.wide_bins
    }

    /// Bins of the last frame on the per-frame floor instead of the wide reference.
    pub fn frame_ref_bins(&self) -> usize {
        self.frame_ref_bins
    }

    /// The last narrow-floor-feature mask (`true`: the bin is inside a span of raised noise that
    /// neither the block floor nor the OS reference window can estimate; the detector takes
    /// [`StepGuard::narrow_floor`] as the floor there and runs the floor branch alone).
    pub fn narrow_mask(&self) -> &[bool] {
        &self.narrow
    }

    /// The per-bin running mean inside narrow floor features; 0 elsewhere.
    pub fn narrow_floor(&self) -> &[f32] {
        &self.narrow_floor
    }

    /// Bins inside a narrow floor feature in the last frame.
    pub fn narrow_bins(&self) -> usize {
        self.narrow_bins
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

    const N_EFF: f64 = 10.0;

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

    /// A bin profile from block values (each block's hop), for the power statistics.
    fn profile_of(blocks: &[f32]) -> Vec<f32> {
        (0..4096)
            .map(|b| blocks[(b / 64).min(blocks.len() - 1)])
            .collect()
    }

    /// Frame `t` of a PSD whose `noise` part has the floor's normalised variance `1/n` and whose
    /// `steady` part is constant.
    fn psd(noise: &[f32], steady: &[f32], t: usize) -> Vec<f32> {
        let s = (1.0 / N_EFF).sqrt() as f32;
        (0..noise.len())
            .map(|b| noise[b] * (1.0 + if (b + t) % 2 == 0 { s } else { -s }) + steady[b])
            .collect()
    }

    struct Run<'a> {
        noise: &'a [f32],
        steady: &'a [f32],
        blocks: &'a [f32],
        shape: Option<ShapeView<'a>>,
        wide: Option<WideView<'a>>,
    }

    impl<'a> Run<'a> {
        fn new(noise: &'a [f32], steady: &'a [f32], blocks: &'a [f32]) -> Self {
            Self {
                noise,
                steady,
                blocks,
                shape: None,
                wide: None,
            }
        }

        /// `frames` frames through `s` (from frame `t0`).
        fn go(&self, s: &mut StepGuard, t0: usize, frames: usize, edges: &[f64]) {
            let g = geometry();
            for t in t0..t0 + frames {
                let p = psd(self.noise, self.steady, t);
                s.update(
                    GuardFrame {
                        psd: &p,
                        n_avg_effective: N_EFF,
                        impulsive: false,
                        block_floor: self.blocks,
                        shape: self.shape,
                        wide: self.wide,
                        // These cases are block-scale; an empty reference skips the
                        // narrow-feature pass, which is covered end to end in
                        // `tests/false_alarm.rs`.
                        reference: &[],
                    },
                    &g,
                    edges,
                );
            }
        }
    }

    fn guard() -> StepGuard {
        let mut s = StepGuard::new(StepGuardConfig::default());
        s.reset(4096);
        s
    }

    #[test]
    fn a_persistent_notch_guards_its_neighbourhood_and_releases_after_the_hold() {
        let mut s = guard();
        let zero = vec![0.0f32; 4096];
        // Blocks 38–41 read a notch 20 dB down.
        let notch = blocks_with(|i| if (38..=41).contains(&i) { 0.01 } else { 1.0 });
        let flat = blocks_with(|_| 1.0);
        let p_notch = profile_of(&notch);
        let p_flat = profile_of(&flat);
        Run::new(&p_notch, &zero, &notch).go(&mut s, 0, 1, &[]);
        assert!(s.mask().iter().all(|&ok| ok), "one frame is not persistent");
        Run::new(&p_notch, &zero, &notch).go(&mut s, 1, 1, &[]);
        let m = s.mask().to_vec();
        assert!(s.available());
        // The biased interpolation span (block centres 37..42 → bins 2495..2815) is guarded.
        assert!(m[2400..2900].iter().all(|&ok| !ok));
        assert!(m[..1500].iter().all(|&ok| ok) && m[3500..].iter().all(|&ok| ok));
        Run::new(&p_flat, &zero, &flat).go(&mut s, 2, 31, &[]);
        assert!(s.guarded_bins() > 0, "held");
        Run::new(&p_flat, &zero, &flat).go(&mut s, 33, 1, &[]);
        assert_eq!(s.guarded_bins(), 0, "released");
        // A layout mismatch disables the jump test; a known edge still applies.
        let g = geometry();
        let p = psd(&p_flat, &zero, 0);
        let m = s
            .update(
                GuardFrame {
                    psd: &p,
                    n_avg_effective: N_EFF,
                    impulsive: false,
                    block_floor: &[1.0; 7],
                    shape: None,
                    wide: None,
                    reference: &[],
                },
                &g,
                &[100e6],
            )
            .to_vec();
        assert!(!s.available());
        let b = 2048 + (2e6 / g.bin_width_hz) as usize;
        assert!(!m[b] && m[b + 300]);
    }

    #[test]
    fn signal_plateaus_are_exempt_floor_plateaus_and_isolated_steps_are_guarded() {
        let zero = vec![0.0f32; 4096];
        // A steady signal 10 dB up over blocks 20–40 (a signal-like plateau): not guarded once
        // classified; guarded before.
        let plateau = blocks_with(|i| if (20..=40).contains(&i) { 10.0 } else { 1.0 });
        let flat = vec![1.0f32; 4096];
        let signal: Vec<f32> = profile_of(&plateau).iter().map(|&x| x - 1.0).collect();
        let mut s = guard();
        let run = Run::new(&flat, &signal, &plateau);
        run.go(&mut s, 0, 3, &[]);
        assert!(s.guarded_bins() > 0, "undecided: guarded");
        run.go(&mut s, 3, 20, &[]);
        assert_eq!(s.guarded_bins(), 0, "signal-like plateau exempt");
        // The same plateau of noise (a passband): guarded.
        let mut s = guard();
        let passband = profile_of(&plateau);
        Run::new(&passband, &zero, &plateau).go(&mut s, 0, 40, &[]);
        let m = s.mask();
        assert!(!m[20 * 64 + 128] && !m[40 * 64 + 128] && m[100] && m[4000]);
        // A lone step (an accessory edge): guarded.
        let mut s = guard();
        let step = blocks_with(|i| if i >= 30 { 10.0 } else { 1.0 });
        Run::new(&profile_of(&step), &zero, &step).go(&mut s, 0, 3, &[]);
        let m = s.mask();
        assert!(!m[30 * 64] && m[100] && m[4000]);
        // A notch inside a signal plateau: the notch is guarded, the plateau away from it is not.
        let mut s = guard();
        let band = blocks_with(|i| match i {
            0..=5 | 55.. => 0.1,
            30..=33 => 0.01,
            _ => 1.0,
        });
        let base = vec![0.1f32; 4096];
        let steady: Vec<f32> = profile_of(&band)
            .iter()
            .map(|&x| (x - 0.1).max(0.0))
            .collect();
        let noise: Vec<f32> = profile_of(&band).iter().map(|&x| x.min(0.1)).collect();
        let _ = base;
        Run::new(&noise, &steady, &band).go(&mut s, 0, 20, &[]);
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
        let zero = vec![0.0f32; 4096];
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
            let mut s = guard();
            // Even a steady (signal-like) middle is not an exempt plateau.
            Run::new(&vec![1e-3; 4096], &profile_of(blocks), blocks).go(&mut s, 0, 20, &[]);
            let _ = &zero;
            let down = if k == 0 { 47 } else { 50 };
            let m = s.mask();
            assert!(!m[down * 64 - 60], "case {k}: second down-step guarded");
            assert!(m[35 * 64 - 32], "case {k}: interior away from the steps");
            assert!(s.wide_mask().iter().all(|&w| !w), "no shape: OS-only");
        }
    }

    #[test]
    fn guarded_zones_use_the_wide_reference_once_the_shape_explains_the_step() {
        // A −20 dB notch over bins 1920..2240; the tracker's normalised blocks are flat once the
        // shape has learned it.
        let zero = vec![0.0f32; 4096];
        let mut profile = vec![1.0f32; 4096];
        profile[1920..2240].fill(0.01);
        let blocks = blocks_of(&profile);
        let flat_blocks = blocks_with(|_| 1.0);
        let unlearned = vec![1.0f32; 4096];
        for (shape, norm, want_wide) in [
            (Some(&profile), &flat_blocks, true),
            (Some(&unlearned), &blocks, false),
            (None, &blocks, false),
        ] {
            let mut s = guard();
            let mut run = Run::new(&profile, &zero, &blocks);
            run.shape = shape.map(|sh| ShapeView {
                shape: sh,
                norm_block_floor: norm,
            });
            run.go(&mut s, 0, 20, &[]);
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
        let zero = vec![0.0f32; 4096];
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
        let norm: Vec<f32> = profile.iter().zip(&shape).map(|(&p, &s)| p / s).collect();
        let blocks = blocks_of(&profile);
        let norm_blocks = blocks_of(&norm);
        let mut s = guard();
        let mut run = Run::new(&profile, &zero, &blocks);
        run.shape = Some(ShapeView {
            shape: &shape,
            norm_block_floor: &norm_blocks,
        });
        run.go(&mut s, 0, 20, &[]);
        let (m, w) = (s.mask(), s.wide_mask());
        assert!(!m[1216] && !m[3200], "both steps guarded");
        assert!(!w[1216], "shelf step OS-only");
        assert!(w[3200], "learned step on the wide reference");
    }

    /// A −20 dB learned notch over 1600..2000 and a +10 dB feature over 2060..2060 + `width`
    /// (`steady`: a signal; otherwise a floor shelf); returns the guard after 40 frames.
    fn next_to_notch(width: usize, steady: bool) -> StepGuard {
        let mut shape = vec![1.0f32; 4096];
        shape[1600..2000].fill(0.01);
        let (lo, hi) = (2060, 2060 + width);
        let mut noise = shape.clone();
        let mut sig = vec![0.0f32; 4096];
        if steady {
            sig[lo..hi].fill(10.0);
        } else {
            noise[lo..hi].fill(10.0);
        }
        let raw: Vec<f32> = noise.iter().zip(&sig).map(|(&a, &b)| a + b).collect();
        let norm: Vec<f32> = raw.iter().zip(&shape).map(|(&p, &s)| p / s).collect();
        let (blocks, norm_blocks) = (blocks_of(&raw), blocks_of(&norm));
        let mut s = guard();
        let mut run = Run::new(&noise, &sig, &blocks);
        run.shape = Some(ShapeView {
            shape: &shape,
            norm_block_floor: &norm_blocks,
        });
        run.go(&mut s, 0, 40, &[]);
        s
    }

    #[test]
    fn a_signal_next_to_a_learned_notch_is_explained() {
        // A +10 dB 300-bin signal 60 bins above a learned notch: a signal-like residual plateau.
        let s = next_to_notch(300, true);
        let (m, w) = (s.mask(), s.wide_mask());
        assert!((2060..2360).all(|b| !m[b] && w[b]));
    }

    #[test]
    fn a_narrow_floor_shelf_next_to_a_learned_notch_is_unexplained() {
        // T-033: the same feature as noise (a floor shelf narrower than 16 blocks) no longer
        // reads as a signal. (The min-of-block model here needs a full block inside the shelf.)
        for width in [600, 900] {
            let s = next_to_notch(width, false);
            let (m, w) = (s.mask(), s.wide_mask());
            assert!(
                (2060..2060 + width).all(|b| m[b] || !w[b]),
                "{width}: shelf guarded bins OS-only"
            );
            assert!(!m[2060], "{width}: shelf edge guarded");
        }
    }

    #[test]
    fn the_wide_reference_keeps_signal_cuts_and_drops_floor_feature_cuts() {
        // The wide reference cut a +6 dB plateau over 1536..2560 (it reads the floor there).
        let zero = vec![0.0f32; 4096];
        let flat_blocks = blocks_with(|_| 1.0);
        let mut floor = vec![1.0f32; 4096];
        floor[1536..2560].fill(4.0);
        let wide_floor = vec![1.0f32; 4096];
        let view = WideView {
            floor: &floor,
            wide_floor: &wide_floor,
        };
        let flat = vec![1.0f32; 4096];
        let signal: Vec<f32> = floor.iter().map(|&x| x - 1.0).collect();
        for (noise, steady, want) in [(&floor, &zero, true), (&flat, &signal, false)] {
            let mut s = guard();
            let mut run = Run::new(noise, steady, &flat_blocks);
            run.wide = Some(view);
            run.go(&mut s, 0, 20, &[]);
            let f = s.frame_ref_mask();
            assert_eq!(f[2000], want, "floor feature {want}");
            assert!(!f[1000] && !f[3000]);
        }
    }

    #[test]
    fn narrow_signals_do_not_move_block_floors() {
        let zero = vec![0.0f32; 4096];
        let mut s = guard();
        let near = blocks_with(|i| 1.0 + 0.2 * ((i % 3) as f32));
        Run::new(&profile_of(&near), &zero, &near).go(&mut s, 0, 5, &[]);
        assert_eq!(s.guarded_bins(), 0);
    }
}
