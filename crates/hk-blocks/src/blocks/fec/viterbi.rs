//! `viterbi` (streaming, `soft|bits → bits`) and `viterbi_frames` (per frame, `frames →
//! frames`): the two shapes of one trellis engine (ADR-0011 §9.3, T-610). The code conventions
//! are in [`super::trellis`].
//!
//! ## `viterbi` — streaming
//!
//! - **Decision depth.** Survivors are traced back every `B = ⌈D/2⌉` steps from the best state
//!   over a window of `D + B` steps and the oldest `B` decided, where `D = ⌈traceback_bits / k⌉`.
//!   Every emitted bit therefore had **at least `traceback_bits`** of trellis after it (≥ 5 K is
//!   the textbook floor; the default 64 is ≈ 9 K for CCSDS K = 7). An `END` chunk flushes the
//!   tail from the best state.
//! - **Soft vs hard.** On `soft` input the branch metric is the correlation `Σ ±s` (unquantised
//!   soft decision). On `bits` input each bit becomes ±1, which is hard-decision decoding and
//!   costs about 2 dB; the block then reports status `hard_decision = 1`, so that performance is
//!   never presented as soft-decision (§9.3).
//! - **Alignment.** Where a step (`n`-tuple) and puncturing period start in a stream is not
//!   known. `align: auto` runs one decoder per phase (`L` = transmitted bits per period: 2 for
//!   CCSDS rate 1/2) and follows the one whose best path metric grows fastest relative to the
//!   input's magnitude (`quality`, 1 = every branch agrees); it takes over when its unexplained
//!   fraction `1 − quality` is under 0.7 × the followed phase's. A switch is **continuous in
//!   time**: the new phase resumes at its step containing the old phase's first undecided item,
//!   so no air time is skipped and less than one step is repeated (the bits the
//!   wrong phase already emitted are what they are — garbage — and the downstream sync search
//!   re-finds its word). It is not flagged `DISCONTINUITY`, which would need a second time map
//!   inside one chunk; `realignments` counts it. `align: fixed` decodes phase 0, the first item
//!   after each restart, with one decoder.
//! - **Restart.** `DISCONTINUITY`/`RESET` drop every undecided bit (counted in `dropped_bits`)
//!   and restart with every state equally likely.
//! - **Time map.** A decided bit's source index is that of the first input item of its step;
//!   `source_per_item` is the average (input spacing × `L / (P · k)`), exact for unpunctured codes.
//!   Each chunk's map starts at its first bit's exact source; after a realignment inside a chunk
//!   the rest of that chunk is off by less than one input item.
//! - **Status.** `quality` (above), `error_rate` = the channel bit error rate estimated by
//!   re-encoding the decided path against the hard decisions received (a refinement objective,
//!   ADR-0011 §1.3), `lock` (auto only): locked while the followed phase leads every other by the
//!   switching ratio. Extras: `hard_decision`, `phase`, `realignments`, `dropped_bits`,
//!   `non_finite`.
//!
//! ## `viterbi_frames` — per frame
//!
//! Decodes the coded span of each frame (`span`) as one code block, hard decision (frames carry
//! no soft values until §9.2's framed-soft type exists; status `hard_decision = 1` always). The
//! output frame is the uncoded prefix, the decoded bits and the uncoded suffix; `corrected_bits`
//! grows by the channel bits the decoder overruled, `check` is left alone (a Viterbi decoder
//! always produces *something*: a CRC after it decides validity), and the layer tree is dropped.
//! `termination`: `terminated` starts and ends in state 0 and strips the tail steps (the zero
//! inputs that return the encoder to 0: `K − 1` for polynomial codes); `truncated` starts in 0
//! and ends in the best state; `tail-biting` starts and ends in the same unknown state and is
//! decoded by wrap-around: the last `w` steps are decoded ahead of the frame and the first `w`
//! after it, `w = min(steps, 8 · memory + 32)`. A frame too short for its termination, or too
//! long for the survivor bound, is dropped and counted (`frames_refused`).

use hk_recipe::{Params, PortType};

use super::trellis::{Code, MAX_N, MAX_SURVIVORS, StepSoft, Trellis, reencode_errors};
use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::blocks::framing::common::{
    P, Span, extend_bits, frames_io, frames_port, one_input, update_hot,
};
use crate::blocks::iq::common::{
    bits_out, finite_or_zero, report_non_finite, restarts, set_meta, single_input, source_at,
};
use crate::buffer::{ChunkFlags, PortSlice};
use crate::registry::BuildCtx;
use crate::status::{Lock, Status};

/// Steps a phase must have been scored over before alignment can change.
const ALIGN_MIN_STEPS: u64 = 128;
/// A phase takes over when its unexplained fraction `1 − quality` is below this share of the
/// followed phase's. Relative, not absolute: at 8 dB a wrong rate-5/6 phase still scores 0.95
/// against the right one's 0.99999, while at 3 dB rate 1/2 it is 0.83 against 0.95.
const ALIGN_RATIO: f64 = 0.7;
/// Averaging length of the alignment score, steps.
const ALIGN_TAU: f64 = 256.0;
/// Averaging length of the channel error-rate estimate, coded bits.
const BER_TAU: f64 = 4096.0;

/// One decoder at one alignment phase.
struct Lane {
    trellis: Trellis,
    /// Items still to skip before this lane's first step (its phase).
    skip: usize,
    /// Next transmitted item's index into `code.kept`.
    pos: usize,
    acc: StepSoft,
    rx: u16,
    /// Steps whose bits are decided (emitted, or skipped while not followed).
    decided: u64,
    gain: f64,
    mag: f64,
    scored: u64,
    /// Absolute input item index of the lane's first item.
    item0: u64,
}

impl Lane {
    fn restart(&mut self, phase: usize, item0: u64) {
        self.trellis.start(false);
        self.skip = phase;
        self.pos = 0;
        self.acc = [0.0; MAX_N];
        self.rx = 0;
        self.decided = 0;
        self.gain = 0.0;
        self.mag = 0.0;
        self.scored = 0;
        self.item0 = item0 + phase as u64;
    }

    fn quality(&self) -> f64 {
        if self.mag > 0.0 {
            self.gain / self.mag
        } else {
            0.0
        }
    }

    /// Feeds one item; returns whether it completed a step.
    #[inline]
    fn feed(&mut self, code: &Code, x: f32) -> bool {
        if self.skip > 0 {
            self.skip -= 1;
            return false;
        }
        let (t, j) = code.kept[self.pos];
        self.acc[j] = x;
        if x > 0.0 {
            self.rx |= 1 << (code.n - 1 - j);
        }
        self.pos += 1;
        if self.pos == code.kept.len() {
            self.pos = 0;
        }
        let done = self.pos == 0 || code.kept[self.pos].0 != t;
        if done {
            let gain = self.trellis.step(code, &self.acc, self.rx);
            let mag: f32 = self.acc[..code.n].iter().map(|v| v.abs()).sum();
            let a = (1.0 / ALIGN_TAU).max(1.0 / (self.scored + 1) as f64);
            self.gain += a * (f64::from(gain) - self.gain);
            self.mag += a * (f64::from(mag) - self.mag);
            self.scored += 1;
            self.acc = [0.0; MAX_N];
            self.rx = 0;
        }
        done
    }
}

/// Builds a `viterbi`.
pub(crate) fn build_stream(
    params: &Params,
    _ctx: &BuildCtx<'_>,
) -> Result<Box<dyn Block>, BlockError> {
    let p = P(params);
    let code = Code::from_params(p)?;
    let bits = p.uint_or("traceback_bits", 64)? as usize;
    let depth = bits.div_ceil(code.input_bits).max(1);
    let block = depth.div_ceil(2);
    let auto = p.str("align").unwrap_or("auto") == "auto";
    let lanes = if auto { code.kept_per_period() } else { 1 };
    if lanes * (depth + block) * code.states > MAX_SURVIVORS {
        return Err(BlockError::Params(format!(
            "{} states × {} steps × {lanes} phases exceeds the survivor bound",
            code.states,
            depth + block
        )));
    }
    Ok(Box::new(Viterbi {
        params: params.clone(),
        code,
        depth,
        block,
        auto,
        n_lanes: lanes,
        lanes: Vec::new(),
        sel: 0,
        hard: false,
        path: Vec::new(),
        out: Vec::new(),
        out_source: None,
        ber: None,
        realignments: 0,
        dropped: 0,
        non_finite: 0,
        status: Status::default(),
    }))
}

/// The streaming decoder.
pub struct Viterbi {
    params: Params,
    code: Code,
    /// Decision depth `D`, steps.
    depth: usize,
    /// Steps decided per traceback `B`.
    block: usize,
    auto: bool,
    n_lanes: usize,
    lanes: Vec<Lane>,
    /// The followed lane.
    sel: usize,
    hard: bool,
    path: Vec<(u8, u16, u16)>,
    /// Output bits of this chunk, and the source index of the first.
    out: Vec<u8>,
    out_source: Option<f64>,
    ber: Option<f64>,
    realignments: u64,
    dropped: u64,
    non_finite: u64,
    status: Status,
}

impl Viterbi {
    fn restart(&mut self, item0: u64) {
        if let Some(l) = self.lanes.get(self.sel) {
            self.dropped += l.trellis.steps.saturating_sub(l.decided) * self.code.input_bits as u64;
        }
        for (phase, lane) in self.lanes.iter_mut().enumerate() {
            lane.restart(phase, item0);
        }
        self.sel = 0;
    }

    /// Decides the followed lane's steps `decided..upto` by tracing back from `state`.
    fn emit(&mut self, upto: u64, state: usize, meta: &crate::buffer::ChunkMeta) {
        let lane = &self.lanes[self.sel];
        // Never trace back past the survivor window (its oldest slots are overwritten): steps
        // older than `D + B` can no longer be decided, so they are counted as dropped rather
        // than read stale. `realign` keeps this from happening; this holds in release too.
        let win = (self.depth + self.block) as u64;
        let lo = lane.decided.max(lane.trellis.steps.saturating_sub(win));
        let stale = lo - lane.decided;
        if upto <= lo {
            return;
        }
        lane.trellis.traceback(state, lo, &mut self.path);
        self.dropped += stale * self.code.input_bits as u64;
        let n = (upto - lo) as usize;
        let (errs, cmp) = reencode_errors(&self.code, &self.path[..n], lo);
        if cmp > 0 {
            let r = errs as f64 / cmp as f64;
            let a = (cmp as f64 / BER_TAU).min(1.0);
            self.ber = Some(self.ber.map_or(r, |b| b + a * (r - b)));
        }
        let k = self.code.input_bits;
        if self.out_source.is_none() {
            let item = lane.item0 + self.code.item_of_step(lo);
            self.out_source = Some(source_at(meta, item as f64));
        }
        for &(u, _, _) in &self.path[..n] {
            self.out.extend((0..k).rev().map(|b| (u >> b) & 1));
        }
        self.lanes[self.sel].decided = upto;
    }

    fn realign(&mut self) {
        let sel = &self.lanes[self.sel];
        if sel.scored < ALIGN_MIN_STEPS {
            return;
        }
        let mut best = self.sel;
        let mut miss_best = (1.0 - sel.quality()) * ALIGN_RATIO;
        for (i, l) in self.lanes.iter().enumerate() {
            let miss = 1.0 - l.quality();
            if i != self.sel && l.scored >= ALIGN_MIN_STEPS && miss < miss_best {
                best = i;
                miss_best = miss;
            }
        }
        if best == self.sel {
            return;
        }
        // Continue in time: the new lane's step that contains the old lane's first undecided
        // item (the last one starting at or before it), so no air time is skipped and less
        // than one step is repeated. Its decisions up to here were skipped while not followed.
        let old = &self.lanes[self.sel];
        let resume = old.item0 + self.code.item_of_step(old.decided);
        // At most `D + B − 1` undecided steps: the next step then leaves exactly `D + B`, which
        // the emit check decides within the window. Starting at `steps − (D + B)` left a gap the
        // next step pushed one past the window (T-610 review). If the old lane lags further, the
        // one oldest step is skipped — a slip loses air time anyway.
        let win = (self.depth + self.block) as u64;
        let new = &mut self.lanes[best];
        let mut s = new.trellis.steps.saturating_sub(win - 1);
        while s + 1 < new.trellis.steps && new.item0 + self.code.item_of_step(s + 1) <= resume {
            s += 1;
        }
        new.decided = s;
        self.sel = best;
        self.realignments += 1;
    }
}

impl Block for Viterbi {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = single_input(inputs, "viterbi", &[PortType::Soft, PortType::Bits])?;
        self.hard = input.ty == PortType::Bits;
        let win = self.depth + self.block;
        self.lanes = (0..self.n_lanes)
            .map(|_| Lane {
                trellis: Trellis::new(&self.code, win),
                skip: 0,
                pos: 0,
                acc: [0.0; MAX_N],
                rx: 0,
                decided: 0,
                gain: 0.0,
                mag: 0.0,
                scored: 0,
                item0: 0,
            })
            .collect();
        let k = self.code.input_bits;
        let l = self.code.kept_per_period();
        let p = self.code.period;
        let steps_per_chunk = (input.max_items * p).div_ceil(l) + 1;
        // One chunk's decisions plus the END flush.
        let max_items = (steps_per_chunk + win) * k;
        self.path = Vec::with_capacity(win);
        self.out = Vec::with_capacity(max_items);
        self.out_source = None;
        self.ber = None;
        self.restart(0);
        self.dropped = 0;
        Ok(vec![PortInfo {
            ty: PortType::Bits,
            rate_hz: input.rate_hz * self.code.rate(),
            max_items,
            hold_items: (win * l).div_ceil(p),
        }])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let m = input.meta;
        let hard = matches!(input.data, PortSlice::Bits(_));
        if !hard && !matches!(input.data, PortSlice::Soft(_)) {
            return Err(BlockError::PortType {
                port: 0,
                expected: PortType::Soft,
                got: input.data.port_type(),
            });
        }
        self.hard = hard;
        if restarts(m.flags) {
            self.restart(m.index);
        }
        self.out.clear();
        self.out_source = None;
        let n = input.data.len();
        let win = (self.depth + self.block) as u64;
        for i in 0..n {
            let x = match input.data {
                PortSlice::Soft(v) => finite_or_zero(v[i], &mut self.non_finite),
                PortSlice::Bits(v) => {
                    if v[i] & 1 == 1 {
                        1.0
                    } else {
                        -1.0
                    }
                }
                _ => 0.0,
            };
            let mut stepped = false;
            for li in 0..self.lanes.len() {
                let lane = &mut self.lanes[li];
                if !lane.feed(&self.code, x) {
                    continue;
                }
                stepped = true;
                if lane.trellis.steps - lane.decided < win {
                    continue;
                }
                if li != self.sel {
                    // Not followed: its decisions are skipped, keeping it level in time.
                    lane.decided += self.block as u64;
                    continue;
                }
                let upto = lane.decided + self.block as u64;
                let best = lane.trellis.best_state();
                self.emit(upto, best, &m);
            }
            if stepped && self.auto && self.lanes.len() > 1 {
                self.realign();
            }
        }
        if m.flags.contains(ChunkFlags::END) {
            let lane = &self.lanes[self.sel];
            let (steps, best) = (lane.trellis.steps, lane.trellis.best_state());
            self.emit(steps, best, &m);
        }
        let k = self.code.input_bits as f64;
        let per =
            m.source_per_item * self.code.kept_per_period() as f64 / (self.code.period as f64 * k);
        let produced = self.out.len();
        {
            let out = io.output(0)?;
            out.meta.rate_hz = m.rate_hz * self.code.rate();
            let source = self.out_source.unwrap_or(m.source_index);
            set_meta(out, &m, source, per);
            bits_out(out)?.extend_from_slice(&self.out);
        }
        let q = self.lanes[self.sel].quality();
        let s = &mut self.status;
        s.items_in += n as u64;
        s.items_out += produced as u64;
        s.quality = Some(q.clamp(0.0, 1.0) as f32);
        s.error_rate = self.ber.map(|b| b as f32);
        s.lock = if !self.auto {
            Lock::None
        } else if self.lanes.len() == 1 {
            Lock::Locked
        } else {
            let sel = &self.lanes[self.sel];
            let others = self
                .lanes
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != self.sel)
                .map(|(_, l)| l.quality())
                .fold(f64::NEG_INFINITY, f64::max);
            if sel.scored >= ALIGN_MIN_STEPS && (1.0 - q) < (1.0 - others) * ALIGN_RATIO {
                Lock::Locked
            } else {
                Lock::Searching
            }
        };
        s.extra
            .set("hard_decision", if self.hard { 1.0 } else { 0.0 });
        s.extra.set("phase", self.sel as f64);
        s.extra.set("realignments", self.realignments as f64);
        s.extra.set("dropped_bits", self.dropped as f64);
        report_non_finite(s, self.non_finite);
        Ok(())
    }

    fn reset(&mut self) {
        self.restart(0);
        self.dropped = 0;
    }

    fn update_params(
        &mut self,
        params: &Params,
        _ctx: &BuildCtx<'_>,
    ) -> Result<ParamUpdate, BlockError> {
        update_hot(&mut self.params, params, &[], |_| {})
    }

    fn status(&self) -> Status {
        self.status
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Termination {
    Terminated,
    TailBiting,
    Truncated,
}

/// Builds a `viterbi_frames`.
pub(crate) fn build_frames(
    params: &Params,
    _ctx: &BuildCtx<'_>,
) -> Result<Box<dyn Block>, BlockError> {
    let p = P(params);
    let code = Code::from_params(p)?;
    let termination = match p.str("termination") {
        Some("terminated") => Termination::Terminated,
        Some("tail-biting") => Termination::TailBiting,
        Some("truncated") => Termination::Truncated,
        _ => return Err(BlockError::Params("termination is required".into())),
    };
    if termination == Termination::Terminated && code.tail_steps.is_none() {
        return Err(BlockError::Params(
            "terminated: zero input does not return this code to state 0".into(),
        ));
    }
    let trellis = Trellis::new(&code, 1);
    Ok(Box::new(ViterbiFrames {
        params: params.clone(),
        span: Span::from_params(p)?,
        code,
        termination,
        trellis,
        steps: Vec::new(),
        path: Vec::new(),
        bits: Vec::new(),
        out: Vec::new(),
        ber: None,
        frames: 0,
        refused: 0,
        corrected: 0,
        status: Status::default(),
    }))
}

/// The per-frame decoder.
pub struct ViterbiFrames {
    params: Params,
    span: Span,
    code: Code,
    termination: Termination,
    trellis: Trellis,
    /// One frame's received steps: soft values and hard word.
    steps: Vec<(StepSoft, u16)>,
    path: Vec<(u8, u16, u16)>,
    bits: Vec<u8>,
    out: Vec<u8>,
    ber: Option<f64>,
    frames: u64,
    refused: u64,
    corrected: u64,
    status: Status,
}

impl ViterbiFrames {
    /// Collects the coded span's steps (a trailing partial step padded with erasures).
    fn gather(&mut self, coded: &[u8]) {
        self.steps.clear();
        let code = &self.code;
        let mut acc: StepSoft = [0.0; MAX_N];
        let mut rx = 0u16;
        let mut pos = 0;
        let mut open = false;
        for &b in coded {
            let (t, j) = code.kept[pos];
            acc[j] = if b & 1 == 1 { 1.0 } else { -1.0 };
            rx |= u16::from(b & 1) << (code.n - 1 - j);
            open = true;
            pos = (pos + 1) % code.kept.len();
            if pos == 0 || code.kept[pos].0 != t {
                self.steps.push((acc, rx));
                acc = [0.0; MAX_N];
                rx = 0;
                open = false;
            }
        }
        if open {
            self.steps.push((acc, rx));
        }
    }

    /// Decodes the gathered steps into `self.out`; returns the channel bits overruled, or
    /// `None` when the frame is refused.
    fn decode(&mut self) -> Option<u64> {
        let n = self.steps.len();
        let code = &self.code;
        let tail = match self.termination {
            Termination::Terminated => code.tail_steps.unwrap_or(0),
            _ => 0,
        };
        if n == 0 || n <= tail {
            return None;
        }
        let w = match self.termination {
            Termination::TailBiting => n.min(8 * code.memory_steps + 32),
            _ => 0,
        };
        let total = n + 2 * w;
        if total * code.states > MAX_SURVIVORS {
            return None;
        }
        self.trellis.ensure_window(total);
        self.trellis
            .start(self.termination != Termination::TailBiting);
        for i in (n - w..n).chain(0..n).chain(0..w) {
            let (soft, rx) = &self.steps[i];
            self.trellis.step(code, soft, *rx);
        }
        let end = if self.termination == Termination::Terminated && self.trellis.reachable(0) {
            0
        } else {
            self.trellis.best_state()
        };
        self.trellis.traceback(end, 0, &mut self.path);
        let middle = &self.path[w..w + n];
        // Re-encoding the middle compares against the frame's own received steps: with
        // puncturing the step phase is the frame's, from its first coded bit.
        let (errs, cmp) = reencode_errors(code, middle, 0);
        if cmp > 0 {
            let r = errs as f64 / cmp as f64;
            let a = (cmp as f64 / BER_TAU).min(1.0);
            self.ber = Some(self.ber.map_or(r, |b| b + a * (r - b)));
        }
        let k = code.input_bits;
        self.out.clear();
        for &(u, _, _) in &middle[..n - tail] {
            self.out.extend((0..k).rev().map(|b| (u >> b) & 1));
        }
        Some(errs)
    }
}

impl Block for ViterbiFrames {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = one_input("viterbi_frames", inputs, &[PortType::Frames])?;
        Ok(vec![frames_port(input, input.max_items)])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let (_, frames, buf) = frames_io(io)?;
        let before = buf.len();
        for f in frames.iter() {
            let len = f.info.bit_len as usize;
            self.frames += 1;
            if self.span.start + self.span.trim >= len {
                self.refused += 1;
                continue;
            }
            self.bits.clear();
            extend_bits(&mut self.bits, f.bytes, 0, len);
            let (start, end) = (self.span.start, len - self.span.trim);
            let coded = std::mem::take(&mut self.bits);
            self.gather(&coded[start..end]);
            let Some(errs) = self.decode() else {
                self.bits = coded;
                self.refused += 1;
                continue;
            };
            self.corrected += errs;
            let mut frame = std::mem::take(&mut self.out);
            // prefix ++ decoded ++ suffix, reusing the decoded buffer.
            frame.splice(0..0, coded[..start].iter().copied());
            frame.extend_from_slice(&coded[end..]);
            let mut info = f.info.clone();
            info.corrected_bits = info
                .corrected_bits
                .saturating_add(u32::try_from(errs).unwrap_or(u32::MAX));
            info.layers = None;
            buf.push_bits(&frame, info);
            self.out = frame;
            self.bits = coded;
        }
        let s = &mut self.status;
        s.items_in += frames.len() as u64;
        s.items_out += (buf.len() - before) as u64;
        s.error_rate = self.ber.map(|b| b as f32);
        s.extra.set("hard_decision", 1.0);
        s.extra.set("frames_refused", self.refused as f64);
        s.extra.set("corrected_bits", self.corrected as f64);
        Ok(())
    }

    fn reset(&mut self) {}

    fn update_params(
        &mut self,
        params: &Params,
        _ctx: &BuildCtx<'_>,
    ) -> Result<ParamUpdate, BlockError> {
        update_hot(&mut self.params, params, &[], |_| {})
    }

    fn status(&self) -> Status {
        self.status
    }
}
