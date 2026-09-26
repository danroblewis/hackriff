//! `sync_search`: bits → frames by a sync word (hk-estimate `SyncCorrelator`, variable length
//! by `length_from`/`terminator`, `bit_order: lsb` character reversal) or by block-code
//! syndromes with offset words (RDS; the generic form of hk-demod `rds::block`).

use hk_estimate::framing::crc::BitCrc;
use hk_estimate::framing::sync::SyncCorrelator;
use hk_recipe::{Params, PortType};

use super::common::{Clock, P, RateMeter, drops_history, one_input, update_hot};
use super::length::{FrameLength, LengthState};
use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::buffer::{ChunkFlags, ChunkMeta, FrameBuf, FrameInfo, PortSlice, PortVec};
use crate::evidence::{emit, saturate};
use crate::registry::BuildCtx;
use crate::status::{Lock, Status};
use hk_model::synth::null::{check_bits, sync_excess_bits};
use hk_model::synth::{Evidence, EvidenceSet, GroupId, MetricId, Stage};

const HOT: &[&str] = &["max_errors"];

/// Builds a `sync_search`.
pub(crate) fn build(params: &Params, _ctx: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let p = P(params);
    let mode = match p.str("mode") {
        Some("sync-word") => Mode::Word(Box::new(WordSearch::new(p)?)),
        Some("offset-words") => Mode::Offsets(Box::new(OffsetSearch::new(p)?)),
        _ => {
            return Err(BlockError::Params(
                "mode must be sync-word or offset-words".into(),
            ));
        }
    };
    Ok(Box::new(SyncSearch {
        params: params.clone(),
        mode,
        status: Status::default(),
        frame_index: 0,
        bit_index: 0,
        frame_rate_hz: 0.0,
        per_frame: 1,
    }))
}

/// The block.
pub struct SyncSearch {
    params: Params,
    mode: Mode,
    status: Status,
    frame_index: u64,
    /// Bits consumed since build.
    bit_index: u64,
    frame_rate_hz: f64,
    per_frame: u32,
}

enum Mode {
    Word(Box<WordSearch>),
    Offsets(Box<OffsetSearch>),
}

struct WordSearch {
    corr: SyncCorrelator,
    /// `polarity: either`: the complemented word also syncs.
    either: bool,
    /// 1 while the current frame synced on the complemented word.
    invert: u8,
    max_errors: u32,
    include_sync: bool,
    lsb: bool,
    length: FrameLength,
    // frame being collected
    in_frame: bool,
    body: Vec<u8>,
    body_off: usize,
    chr: [u8; 8],
    chr_fill: usize,
    st: LengthState,
    first_bit: u64,
    errors: u32,
    // stats
    frames: u64,
    frames_cut: u64,
    last_end: Option<u64>,
    sync_ber: Option<f32>,
    /// Evidence (T-853): positions the word was tested at, and syncs, since `reset()`.
    ev_positions: u64,
    ev_hits: u64,
}

impl WordSearch {
    fn new(p: P<'_>) -> Result<Self, BlockError> {
        let perr = |m: &str| BlockError::Params(m.into());
        let word = p
            .hex("sync_word")
            .ok_or_else(|| perr("sync-word needs sync_word"))?;
        let bits = p
            .uint("sync_bits")?
            .ok_or_else(|| perr("sync-word needs sync_bits"))?;
        let corr = SyncCorrelator::new(word, bits)
            .ok_or_else(|| perr("sync_word wider than sync_bits"))?;
        let max = p
            .uint("frame_bits")?
            .ok_or_else(|| perr("sync-word needs frame_bits"))?;
        let length = FrameLength::from_params(p.0, max)?;
        let include_sync = p.bool_or("include_sync", false);
        let lsb = p.str("bit_order") == Some("lsb");
        if length.reopens() {
            if include_sync {
                return Err(perr("terminator.reopen needs include_sync false"));
            }
            if lsb && length.terminator_step_bits() % 8 != 0 {
                return Err(perr(
                    "terminator.reopen with bit_order lsb needs step_bits a multiple of 8 (the \
                     closing word must end on a character boundary)",
                ));
            }
        }
        let body_off = if include_sync { bits as usize } else { 0 };
        Ok(Self {
            corr,
            max_errors: p.uint_or("max_errors", 0)?,
            include_sync,
            lsb,
            either: p.str("polarity") == Some("either"),
            invert: 0,
            body: Vec::with_capacity(body_off + max as usize),
            body_off,
            length,
            in_frame: false,
            chr: [0; 8],
            chr_fill: 0,
            st: LengthState::default(),
            first_bit: 0,
            errors: 0,
            frames: 0,
            frames_cut: 0,
            last_end: None,
            sync_ber: None,
            ev_positions: 0,
            ev_hits: 0,
        })
    }

    fn reset(&mut self) {
        self.corr.reset();
        self.in_frame = false;
        self.body.clear();
        self.chr_fill = 0;
    }

    /// Appends one packed-order bit; whether the frame is complete.
    fn append(&mut self, b: u8) -> bool {
        self.body.push(b);
        self.length
            .after_bit(&mut self.st, &self.body[self.body_off..])
    }

    /// Ends the frame whose last bit is `end_bit`; with `terminator.reopen` (T-1054) a frame
    /// that ended on its terminator word opens the next frame on that same word (shared-flag
    /// HDLC), with the polarity it synced on, instead of restarting the sync search.
    fn end_frame(
        &mut self,
        out: &mut FrameBuf,
        clock: &Clock,
        index: &mut u64,
        channel: u16,
        end_bit: u64,
    ) {
        let reopen = self
            .length
            .ended_on_reopening_terminator(&self.st, self.body.len() - self.body_off);
        let invert = self.invert;
        self.emit(out, clock, index, channel, end_bit);
        if reopen {
            self.in_frame = true;
            self.invert = invert;
            self.errors = 0;
            self.st = LengthState::default();
            self.first_bit = end_bit + 1;
        }
    }

    fn emit(
        &mut self,
        out: &mut FrameBuf,
        clock: &Clock,
        index: &mut u64,
        channel: u16,
        end_bit: u64,
    ) {
        if self.body.len() > self.body_off {
            let info = FrameInfo::new(*index, clock.source(self.first_bit), channel);
            out.push_bits(&self.body, info);
            *index += 1;
            self.frames += 1;
            if (self.body.len() - self.body_off) as u32 >= self.length.max_bits()
                && !self.length.is_fixed()
            {
                self.frames_cut += 1;
            }
        }
        self.in_frame = false;
        self.body.clear();
        self.chr_fill = 0;
        self.corr.reset();
        self.last_end = Some(end_bit);
    }

    fn push(
        &mut self,
        b: u8,
        bit: u64,
        clock: &Clock,
        out: &mut FrameBuf,
        index: &mut u64,
        channel: u16,
    ) {
        if !self.in_frame {
            let Some(direct) = self.corr.push(b) else {
                return;
            };
            self.ev_positions += 1;
            let sync_bits = self.corr.bits();
            let complemented = sync_bits - direct;
            let (errors, invert) = if self.either && complemented < direct {
                (complemented, 1)
            } else {
                (direct, 0)
            };
            if errors > self.max_errors {
                return;
            }
            self.invert = invert;
            self.ev_hits += 1;
            self.in_frame = true;
            self.errors = errors;
            let ber = errors as f32 / sync_bits as f32;
            self.sync_ber = Some(self.sync_ber.map_or(ber, |x| 0.9 * x + 0.1 * ber));
            self.st = LengthState::default();
            self.body.clear();
            self.chr_fill = 0;
            if self.include_sync {
                let reg = self.corr.register();
                let inv = self.invert;
                self.body
                    .extend((0..sync_bits).rev().map(|k| ((reg >> k) & 1) as u8 ^ inv));
                self.first_bit = bit + 1 - u64::from(sync_bits);
            } else {
                self.first_bit = bit + 1;
            }
            return;
        }
        let b = b ^ self.invert;
        if !self.lsb {
            if self.append(b) {
                self.end_frame(out, clock, index, channel, bit);
            }
            return;
        }
        self.chr[self.chr_fill] = b;
        self.chr_fill += 1;
        if self.chr_fill == 8 {
            self.chr_fill = 0;
            let chr = self.chr;
            for &c in chr.iter().rev() {
                if self.append(c) {
                    self.end_frame(out, clock, index, channel, bit);
                    return;
                }
            }
        }
    }

    /// `END`: a partial frame is emitted (its check block marks it); a trailing partial
    /// character is reversed within its own length.
    fn flush(
        &mut self,
        out: &mut FrameBuf,
        clock: &Clock,
        index: &mut u64,
        channel: u16,
        bit: u64,
    ) {
        if !self.in_frame {
            return;
        }
        let chr = self.chr;
        let fill = self.chr_fill;
        self.chr_fill = 0;
        for &c in chr[..fill].iter().rev() {
            if self.append(c) {
                break;
            }
        }
        self.emit(out, clock, index, channel, bit);
    }
}

/// Offset-word block synchronisation (RDS, EN 50067 Annex C, generalised).
struct OffsetSearch {
    block_bits: u32,
    /// Syndrome contribution of window bit `k` counted from the newest (k = 0).
    h: Vec<u32>,
    offsets: Vec<u32>,
    /// Allowed offset indices per position.
    sequence: Vec<Vec<usize>>,
    /// Syndrome hits on one block lattice, in sequence order, that lock.
    lock_blocks: u32,
    /// Most blocks between two successive hits of an acquisition chain.
    lock_gap_blocks: u64,
    unlock_errors: u16,
    /// Consecutive invalid blocks that drop lock (0: off).
    unlock_run: u32,
    bad_run: u32,
    reg: u64,
    history: Vec<u8>,
    scratch: Vec<u8>,
    hpos: usize,
    filled: u64,
    locked: bool,
    next_pos: usize,
    bits_since: u32,
    /// Recent syndrome hits while searching: (bit index, position, chain length), a chain being
    /// hits on one lattice whose positions follow the sequence.
    hits: [(u64, u16, u32); 32],
    hit_next: usize,
    window: RateMeter,
    blocks_ok: u64,
    blocks_bad: u64,
    acquisitions: u64,
    frames: u64,
    /// Check-word width (syndrome bits).
    check_bits: u32,
    /// Evidence (T-853): blocks tested **while locked**, and those that matched, since
    /// `reset()`. Acquisition hits are excluded: they were found by searching every position.
    ev_tested: u64,
    ev_ok: u64,
}

impl OffsetSearch {
    fn new(p: P<'_>) -> Result<Self, BlockError> {
        let perr = |m: &str| BlockError::Params(m.into());
        let block_bits = p
            .uint("block_bits")?
            .ok_or_else(|| perr("offset-words needs block_bits"))?;
        let check_bits = p
            .uint("check_bits")?
            .ok_or_else(|| perr("offset-words needs check_bits"))?;
        if check_bits >= block_bits || block_bits > 64 {
            return Err(perr("check_bits must be below block_bits ≤ 64"));
        }
        let poly = p
            .hex("poly")
            .ok_or_else(|| perr("offset-words needs poly"))?;
        let crc = BitCrc::new(check_bits as u8, poly, 0, false, false, 0)
            .ok_or_else(|| perr("poly wider than check_bits"))?;
        let data_bits = (block_bits - check_bits) as usize;
        // Syndrome = crc(data) ^ check bits: linear in the block, so one contribution per bit.
        let mut h = vec![0u32; block_bits as usize];
        for (k, hk) in h.iter_mut().enumerate() {
            let pos = block_bits as usize - 1 - k; // from the block's first bit
            *hk = if pos < data_bits {
                let v = 1u64 << (63 - pos);
                crc.linear(&v.to_be_bytes(), 0, data_bits)
            } else {
                1 << (block_bits as usize - 1 - pos)
            };
        }
        let mut names = Vec::new();
        let mut offsets = Vec::new();
        for o in p.list("offsets") {
            let o = o.as_object().map(P).ok_or_else(|| perr("offsets entry"))?;
            let word = o.hex("word").ok_or_else(|| perr("offset word"))?;
            if word >> check_bits != 0 {
                return Err(perr("offset word wider than check_bits"));
            }
            names.push(o.str("name").unwrap_or_default().to_owned());
            offsets.push(word as u32);
        }
        let sequence = p
            .list("sequence")
            .iter()
            .map(|pos| {
                pos.as_array()
                    .ok_or_else(|| perr("sequence entry"))?
                    .iter()
                    .map(|n| {
                        n.as_str()
                            .and_then(|n| names.iter().position(|x| x == n))
                            .ok_or_else(|| perr("sequence names an unknown offset"))
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .collect::<Result<Vec<_>, _>>()?;
        if offsets.is_empty() || sequence.is_empty() {
            return Err(perr("offset-words needs offsets and sequence"));
        }
        let frame_bits = block_bits as usize * sequence.len();
        if frame_bits > 1_000_000 {
            return Err(perr("sequence too long"));
        }
        let unlock_window = p.uint_or("unlock_window", 50)? as usize;
        let unlock_errors = p.uint_or("unlock_errors", 20)?;
        if unlock_errors as usize > unlock_window {
            return Err(perr("unlock_errors must be at most unlock_window"));
        }
        Ok(Self {
            block_bits,
            h,
            offsets,
            sequence,
            lock_blocks: p.uint_or("lock_blocks", 2)?.max(1),
            lock_gap_blocks: u64::from(p.uint_or("lock_gap_blocks", 1)?.clamp(1, 16)),
            unlock_errors: unlock_errors.clamp(1, 1024) as u16,
            unlock_run: p.uint_or("unlock_run", 0)?,
            bad_run: 0,
            reg: 0,
            history: vec![0; frame_bits],
            scratch: Vec::with_capacity(frame_bits),
            hpos: 0,
            filled: 0,
            locked: false,
            next_pos: 0,
            bits_since: 0,
            hits: [(u64::MAX, 0, 0); 32],
            hit_next: 0,
            window: RateMeter::new(unlock_window),
            blocks_ok: 0,
            blocks_bad: 0,
            acquisitions: 0,
            frames: 0,
            check_bits,
            ev_tested: 0,
            ev_ok: 0,
        })
    }

    fn frame_bits(&self) -> usize {
        self.history.len()
    }

    fn reset(&mut self) {
        self.reg = 0;
        self.filled = 0;
        self.locked = false;
        self.bad_run = 0;
        self.hits = [(u64::MAX, 0, 0); 32];
        self.window.clear();
    }

    fn syndrome(&self) -> u32 {
        let mut w = self.reg & (u64::MAX >> (64 - self.block_bits));
        let mut s = 0;
        while w != 0 {
            s ^= self.h[w.trailing_zeros() as usize];
            w &= w - 1;
        }
        s
    }

    fn matches(&self, s: u32, pos: usize) -> bool {
        self.sequence[pos].iter().any(|&o| self.offsets[o] == s)
    }

    fn push(
        &mut self,
        b: u8,
        bit: u64,
        clock: &Clock,
        out: &mut FrameBuf,
        index: &mut u64,
        channel: u16,
    ) {
        self.reg = (self.reg << 1) | u64::from(b & 1);
        self.history[self.hpos] = b & 1;
        self.hpos = (self.hpos + 1) % self.history.len();
        self.filled += 1;
        let bb = u64::from(self.block_bits);
        if self.filled < bb {
            return;
        }
        let len = self.sequence.len();
        if !self.locked {
            let s = self.syndrome();
            for pos in 0..len {
                if !self.matches(s, pos) {
                    continue;
                }
                // Extend the longest chain ending k ≤ lock_gap_blocks blocks earlier at the
                // position k before this one.
                let gap = self.lock_gap_blocks;
                let chain = 1 + self
                    .hits
                    .iter()
                    .filter(|h| {
                        h.0 < bit && (bit - h.0) % bb == 0 && {
                            let k = (bit - h.0) / bb;
                            k <= gap && (usize::from(h.1) + k as usize) % len == pos
                        }
                    })
                    .map(|h| h.2)
                    .max()
                    .unwrap_or(0);
                self.hits[self.hit_next] = (bit, pos as u16, chain);
                self.hit_next = (self.hit_next + 1) % self.hits.len();
                if chain >= self.lock_blocks {
                    self.locked = true;
                    self.acquisitions += 1;
                    self.bits_since = 0;
                    self.bad_run = 0;
                    self.next_pos = (pos + 1) % len;
                    for _ in 0..chain.min(len as u32) {
                        self.window.push(false);
                    }
                    self.blocks_ok += u64::from(chain);
                    self.hits = [(u64::MAX, 0, 0); 32];
                    if pos == len - 1 {
                        self.emit(bit, clock, out, index, channel);
                    }
                    return;
                }
            }
            return;
        }
        self.bits_since += 1;
        if self.bits_since < self.block_bits {
            return;
        }
        self.bits_since = 0;
        let pos = self.next_pos;
        self.next_pos = (pos + 1) % len;
        let ok = self.matches(self.syndrome(), pos);
        self.ev_tested += 1;
        self.ev_ok += u64::from(ok);
        self.window.push(!ok);
        if ok {
            self.blocks_ok += 1;
            self.bad_run = 0;
        } else {
            self.blocks_bad += 1;
            self.bad_run += 1;
        }
        if pos == len - 1 {
            self.emit(bit, clock, out, index, channel);
        }
        let run_lost = self.unlock_run > 0 && self.bad_run >= self.unlock_run;
        if run_lost || self.window.bad() >= self.unlock_errors {
            self.locked = false;
            self.bad_run = 0;
            self.window.clear();
            self.hits = [(u64::MAX, 0, 0); 32];
        }
    }

    fn emit(&mut self, bit: u64, clock: &Clock, out: &mut FrameBuf, index: &mut u64, channel: u16) {
        let n = self.frame_bits();
        if self.filled < n as u64 {
            return;
        }
        // The ring's oldest bit is at hpos: history rotated is the frame in order.
        let (tail, head) = self.history.split_at(self.hpos);
        self.scratch.clear();
        self.scratch.extend_from_slice(head);
        self.scratch.extend_from_slice(tail);
        let info = FrameInfo::new(*index, clock.source(bit + 1 - n as u64), channel);
        out.push_bits(&self.scratch, info);
        *index += 1;
        self.frames += 1;
    }
}

impl Block for SyncSearch {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = one_input("sync_search", inputs, &[PortType::Bits])?;
        let (per_frame, min_frame) = match &self.mode {
            Mode::Word(w) => {
                let sync = w.corr.bits();
                (sync + w.length.max_bits(), sync + w.length.min_bits())
            }
            Mode::Offsets(o) => (o.frame_bits() as u32, o.block_bits),
        };
        self.per_frame = per_frame;
        self.frame_rate_hz = input.rate_hz / f64::from(per_frame);
        Ok(vec![PortInfo {
            ty: PortType::Frames,
            rate_hz: self.frame_rate_hz,
            max_items: input.max_items / min_frame as usize + 2,
            hold_items: per_frame as usize,
        }])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let PortSlice::Bits(bits) = input.data else {
            return Err(BlockError::PortType {
                port: 0,
                expected: PortType::Bits,
                got: input.data.port_type(),
            });
        };
        let out = io.output(0)?;
        let got = out.data.port_type();
        let PortVec::Frames(buf) = &mut out.data else {
            return Err(BlockError::PortType {
                port: 0,
                expected: PortType::Frames,
                got,
            });
        };
        out.meta = ChunkMeta {
            index: out.meta.index,
            source_index: input.meta.source_index,
            source_per_item: input.meta.source_per_item * f64::from(self.per_frame),
            rate_hz: self.frame_rate_hz,
            channel: input.meta.channel,
            flags: input.meta.flags,
        };
        if drops_history(input.meta.flags) {
            match &mut self.mode {
                Mode::Word(w) => w.reset(),
                Mode::Offsets(o) => o.reset(),
            }
        }
        let clock = Clock {
            first_bit: self.bit_index,
            source_index: input.meta.source_index,
            per_bit: input.meta.source_per_item,
        };
        let channel = input.meta.channel;
        let before = buf.len();
        let index = &mut self.frame_index;
        match &mut self.mode {
            Mode::Word(w) => {
                for (k, &b) in bits.iter().enumerate() {
                    w.push(b, self.bit_index + k as u64, &clock, buf, index, channel);
                }
                if input.meta.flags.contains(ChunkFlags::END) {
                    let last = (self.bit_index + bits.len() as u64).saturating_sub(1);
                    w.flush(buf, &clock, index, channel, last);
                }
            }
            Mode::Offsets(o) => {
                for (k, &b) in bits.iter().enumerate() {
                    o.push(b, self.bit_index + k as u64, &clock, buf, index, channel);
                }
            }
        }
        self.bit_index += bits.len() as u64;
        self.status.items_in += bits.len() as u64;
        self.status.items_out += (buf.len() - before) as u64;
        self.refresh_status();
        Ok(())
    }

    fn reset(&mut self) {
        match &mut self.mode {
            Mode::Word(w) => {
                w.reset();
                w.ev_positions = 0;
                w.ev_hits = 0;
            }
            Mode::Offsets(o) => {
                o.reset();
                o.ev_tested = 0;
                o.ev_ok = 0;
            }
        }
        self.refresh_status();
    }

    /// S4 `sync_excess` (ADR-0015 §2.2, analytic). **Sync word:** the Poisson tail of the syncs
    /// over the positions searched, minus the word's width (`null::sync_excess_bits`); `raw` =
    /// syncs, `n` = syncs. **Offset words:** blocks tested on the locked lattice are at
    /// positions the lattice fixes (no search), each matching one of the allowed offsets by
    /// chance with probability `offsets · 2^−check_bits`; the binomial tail over them, `raw` =
    /// matching blocks, `n` = blocks tested.
    fn evidence(&self, out: &mut EvidenceSet) {
        match &self.mode {
            Mode::Word(w) => {
                if w.ev_positions == 0 {
                    return;
                }
                let bits = sync_excess_bits(
                    w.ev_hits,
                    w.ev_positions,
                    w.corr.bits(),
                    w.max_errors,
                    w.either,
                );
                emit(
                    out,
                    Evidence::new(
                        Stage::S4,
                        MetricId::SyncExcess,
                        GroupId::Undeclared,
                        w.ev_hits as f32,
                        saturate(w.ev_hits),
                        bits,
                    ),
                );
            }
            Mode::Offsets(o) => {
                if o.ev_tested == 0 {
                    return;
                }
                let most = o.sequence.iter().map(Vec::len).max().unwrap_or(1).max(1);
                let width = f64::from(o.check_bits) - (most as f64).log2();
                emit(
                    out,
                    Evidence::new(
                        Stage::S4,
                        MetricId::SyncExcess,
                        GroupId::Undeclared,
                        o.ev_ok as f32,
                        saturate(o.ev_tested),
                        check_bits(o.ev_ok, o.ev_tested, width),
                    ),
                );
            }
        }
    }

    fn update_params(
        &mut self,
        params: &Params,
        _ctx: &BuildCtx<'_>,
    ) -> Result<ParamUpdate, BlockError> {
        let mode = &mut self.mode;
        update_hot(&mut self.params, params, HOT, |p| {
            if let Mode::Word(w) = mode {
                w.max_errors = p.uint_or("max_errors", 0).unwrap_or(w.max_errors);
            }
        })
    }

    fn status(&self) -> Status {
        self.status
    }
}

impl SyncSearch {
    fn refresh_status(&mut self) {
        let s = &mut self.status;
        match &self.mode {
            Mode::Word(w) => {
                let recent = w
                    .last_end
                    .is_some_and(|e| self.bit_index.saturating_sub(e) <= u64::from(self.per_frame));
                s.lock = if w.in_frame || recent {
                    Lock::Locked
                } else {
                    Lock::Searching
                };
                s.error_rate = w.sync_ber;
                s.quality = w.sync_ber.map(|x| 1.0 - x);
                s.extra.set("frames", w.frames as f64);
                s.extra.set("frames_cut", w.frames_cut as f64);
                s.extra.set("sync_errors", f64::from(w.errors));
            }
            Mode::Offsets(o) => {
                s.lock = if o.locked {
                    Lock::Locked
                } else {
                    Lock::Searching
                };
                s.error_rate = o.window.rate();
                s.quality = Some(if o.locked {
                    1.0 - o.window.rate().unwrap_or(0.0)
                } else {
                    0.0
                });
                s.extra.set("frames", o.frames as f64);
                s.extra.set("blocks_ok", o.blocks_ok as f64);
                s.extra.set("blocks_bad", o.blocks_bad as f64);
                s.extra.set("acquisitions", o.acquisitions as f64);
            }
        }
    }
}
