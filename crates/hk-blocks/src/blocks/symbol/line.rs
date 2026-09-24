//! `slicer`, `diff_decode`, `nrzi`, `manchester`.

use hk_recipe::{Params, PortType};

use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::blocks::iq::common::*;
use crate::buffer::PortSlice;
use crate::evidence::{BitStructure, calibrated};
use crate::registry::BuildCtx;
use crate::status::{Lock, Status};
use hk_model::synth::{EvidenceSet, GroupId, MetricId, Stage};

// ------------------------------------------------------------------------------------- slicer

pub(crate) fn build_slicer(p: &Params, _: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let mut b = Slicer {
        threshold: 0.0,
        invert: 0,
        ones: 0,
        ev: BitStructure::default(),
        status: Status::default(),
    };
    b.apply(p);
    Ok(Box::new(b))
}

struct Slicer {
    threshold: f32,
    invert: u8,
    ones: u64,
    /// Evidence (T-853): the output bits' structure since `reset()`.
    ev: BitStructure,
    status: Status,
}

impl Slicer {
    fn apply(&mut self, p: &Params) {
        self.threshold = f64_or(p, "threshold", 0.0) as f32;
        self.invert = u8::from(bool_or(p, "invert", false));
    }
}

impl Block for Slicer {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = single_input(inputs, "slicer", &[PortType::Soft])?;
        Ok(vec![PortInfo {
            ty: PortType::Bits,
            hold_items: 0,
            ..input
        }])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let x = soft_in(&input)?;
        let m = input.meta;
        let out = io.output(0)?;
        set_meta(out, &m, m.source_index, m.source_per_item);
        let y = bits_out(out)?;
        for &v in x {
            let b = u8::from(v > self.threshold) ^ self.invert;
            self.ones += u64::from(b);
            self.ev.push(b);
            y.push(b);
        }
        let n = x.len() as u64;
        self.status.items_in += n;
        self.status.items_out += n;
        if self.status.items_out > 0 {
            self.status.extra.set(
                "ones_fraction",
                self.ones as f64 / self.status.items_out as f64,
            );
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.ev.clear();
    }

    fn update_params(&mut self, p: &Params, _: &BuildCtx<'_>) -> Result<ParamUpdate, BlockError> {
        self.apply(p);
        Ok(ParamUpdate::Applied)
    }

    fn status(&self) -> Status {
        self.status
    }

    /// S3 `bit_structure` (group `bit_shape`) of the sliced bits.
    fn evidence(&self, out: &mut EvidenceSet) {
        self.ev.evidence(out);
    }
}

// ---------------------------------------------------------------------- diff_decode and nrzi

pub(crate) fn build_diff(p: &Params, _: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let mut b = Differential::new(false);
    b.apply(p);
    Ok(Box::new(b))
}

pub(crate) fn build_nrzi(p: &Params, _: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let mut b = Differential::new(true);
    b.apply(p);
    Ok(Box::new(b))
}

/// `out[n] = in[n] ⊕ in[n−1] ⊕ complement`: diff_decode (xor / xnor) and NRZI
/// (transition-is-1 / transition-is-0). The first bit after a reset is only the reference.
///
/// NRZI `direction: encode` is the inverse, a running level: `level ⊕= in[n] ⊕ complement`, one
/// output per input, level 1 after a reset. A non-coherent MSK receiver needs it where the data
/// are the coherent chips and the tone marks chip transitions (ACARS: acarsdec `msk.c`); the
/// level's polarity is arbitrary, so the sync search takes either polarity.
struct Differential {
    nrzi: bool,
    encode: bool,
    complement: u8,
    prev: Option<u8>,
    /// Evidence (T-853): the output bits' structure since `reset()`.
    ev: BitStructure,
    status: Status,
}

impl Differential {
    fn new(nrzi: bool) -> Self {
        Self {
            nrzi,
            encode: false,
            complement: 0,
            prev: None,
            ev: BitStructure::default(),
            status: Status::default(),
        }
    }

    fn apply(&mut self, p: &Params) {
        self.encode = self.nrzi && str_or(p, "direction", "decode") == "encode";
        self.complement = if self.nrzi {
            u8::from(str_or(p, "mode", "transition-is-0") == "transition-is-0")
        } else {
            u8::from(str_or(p, "mode", "xor") == "xnor")
        };
    }
}

impl Block for Differential {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let name = if self.nrzi { "nrzi" } else { "diff_decode" };
        let input = single_input(inputs, name, &[PortType::Bits])?;
        Ok(vec![PortInfo {
            hold_items: usize::from(!self.encode),
            ..input
        }])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let x = bits_in(&input)?;
        let m = input.meta;
        if restarts(m.flags) {
            self.prev = None;
        }
        let skip = usize::from(self.prev.is_none() && !self.encode);
        let out = io.output(0)?;
        set_meta(
            out,
            &m,
            source_at(&m, (m.index + skip as u64) as f64),
            m.source_per_item,
        );
        let y = bits_out(out)?;
        let before = y.len();
        if self.encode {
            let mut level = self.prev.unwrap_or(1);
            for &b in x {
                level ^= (b & 1) ^ self.complement;
                y.push(level);
            }
            self.prev = Some(level);
        } else {
            for &b in x {
                let b = b & 1;
                if let Some(p) = self.prev {
                    y.push(b ^ p ^ self.complement);
                }
                self.prev = Some(b);
            }
        }
        for &b in &y[before..] {
            self.ev.push(b);
        }
        self.status.items_in += x.len() as u64;
        self.status.items_out += (y.len() - before) as u64;
        Ok(())
    }

    fn reset(&mut self) {
        self.prev = None;
        self.ev.clear();
    }

    /// S3 `bit_structure` (group `bit_shape`) of the decoded bits.
    fn evidence(&self, out: &mut EvidenceSet) {
        self.ev.evidence(out);
    }

    fn update_params(&mut self, p: &Params, _: &BuildCtx<'_>) -> Result<ParamUpdate, BlockError> {
        if self.nrzi && (str_or(p, "direction", "decode") == "encode") != self.encode {
            return Ok(ParamUpdate::Rebuild);
        }
        self.apply(p);
        Ok(ParamUpdate::Applied)
    }

    fn status(&self) -> Status {
        self.status
    }
}

// --------------------------------------------------------------------------------- manchester

/// Violation-average weight per pair.
const VIOLATION_ALPHA: f64 = 1.0 / 32.0;
/// Pairs of each alignment seen before re-pairing is considered.
const ALIGN_MIN_PAIRS: u64 = 16;
/// How much lower the other alignment's violation average must be to re-pair.
const ALIGN_MARGIN: f64 = 0.2;
/// Most output segments (runs of items with one pair alignment) held at once.
const MAX_SEGMENTS: usize = 4;

pub(crate) fn build_manchester(p: &Params, _: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let mut b = Manchester {
        ieee: 0,
        auto: true,
        phase: 0,
        prev: None,
        chip: 0,
        viol: [0.0; 2],
        pairs: [0; 2],
        held: Vec::new(),
        cap: 0,
        segments: [Segment::default(); MAX_SEGMENTS],
        n_segments: 0,
        open: false,
        dropped: 0,
        ev_viol: 0.0,
        ev_pairs: 0,
        ev_bits: BitStructure::default(),
        status: Status::default(),
    };
    b.apply(p);
    Ok(Box::new(b))
}

/// A run of held items spaced exactly two chips apart.
#[derive(Clone, Copy, Debug, Default)]
struct Segment {
    /// Offset of its first held item in `held`.
    start: usize,
    /// Source index of its first held item (its pair's first chip).
    source: f64,
    /// Source samples per item.
    per_item: f64,
}

/// Chip-pair decoder. A realignment shifts the pairs by one chip, so items after it are not
/// on the previous items' two-chip grid. Decoded bits therefore go through `held` in
/// segments of one alignment, and each chunk emits one segment (oldest first) with its own
/// exact time map; items after a mid-chunk realignment follow in the next chunk.
struct Manchester {
    ieee: u8,
    auto: bool,
    /// Parity of the chip index that starts a pair.
    phase: u64,
    prev: Option<f32>,
    /// Chips since reset.
    chip: u64,
    /// Violation averages: [current alignment, other alignment].
    viol: [f64; 2],
    pairs: [u64; 2],
    /// Decoded bits not yet emitted, oldest first (at most `cap`).
    held: Vec<u8>,
    cap: usize,
    segments: [Segment; MAX_SEGMENTS],
    n_segments: usize,
    /// Whether the last segment continues with the current alignment.
    open: bool,
    /// Bits dropped: held at a restart, or beyond `cap`/`MAX_SEGMENTS` (realigning on nearly
    /// every chunk).
    dropped: u64,
    /// Evidence (T-853): Σ violation over aligned pairs, the pairs, and the decoded bits'
    /// structure, since `reset()`.
    ev_viol: f64,
    ev_pairs: u64,
    ev_bits: BitStructure,
    status: Status,
}

impl Manchester {
    fn apply(&mut self, p: &Params) {
        self.ieee = u8::from(str_or(p, "convention", "thomas") == "ieee");
        self.auto = str_or(p, "align", "auto") == "auto";
    }

    fn clear(&mut self) {
        self.phase = 0;
        self.prev = None;
        self.chip = 0;
        self.viol = [0.0; 2];
        self.pairs = [0; 2];
        self.dropped += self.held.len() as u64;
        self.held.clear();
        self.n_segments = 0;
        self.open = false;
    }

    /// Held items of the oldest segment.
    fn oldest_len(&self) -> usize {
        if self.n_segments > 1 {
            self.segments[1].start
        } else {
            self.held.len()
        }
    }

    /// Removes the first `n` held items (`n ≤ oldest_len()`; all of it for a closed segment).
    fn consume(&mut self, n: usize) {
        self.held.drain(..n);
        if self.n_segments > 1 && self.segments[1].start == n {
            self.segments.copy_within(1..self.n_segments, 0);
            self.n_segments -= 1;
            for s in &mut self.segments[..self.n_segments] {
                s.start -= n;
            }
        } else if self.n_segments > 0 {
            let s = &mut self.segments[0];
            s.source += n as f64 * s.per_item;
            s.start = 0;
        }
    }

    fn drop_oldest(&mut self) {
        let n = self.oldest_len();
        self.dropped += n as u64;
        self.consume(n);
    }

    /// Holds one decoded bit whose pair starts at `source`.
    #[inline]
    fn hold(&mut self, bit: u8, source: f64, per_item: f64) {
        if !self.open || self.n_segments == 0 {
            if self.n_segments > 0 && self.segments[self.n_segments - 1].start == self.held.len() {
                // The last segment is empty: reuse its slot.
                self.n_segments -= 1;
            } else if self.n_segments == MAX_SEGMENTS {
                self.drop_oldest();
            }
            self.segments[self.n_segments] = Segment {
                start: self.held.len(),
                source,
                per_item,
            };
            self.n_segments += 1;
            self.open = true;
        }
        if self.held.len() == self.cap {
            // Only with several segments held (one is emptied every chunk).
            self.drop_oldest();
        }
        self.held.push(bit);
    }
}

impl Block for Manchester {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = single_input(inputs, "manchester", &[PortType::Soft, PortType::Bits])?;
        let per_chunk = input.max_items / 2 + 1;
        // Room for a few segments; an output chunk carries at most one of them.
        self.cap = MAX_SEGMENTS * per_chunk;
        self.held = Vec::with_capacity(self.cap);
        self.clear();
        self.dropped = 0;
        Ok(vec![PortInfo {
            ty: PortType::Bits,
            rate_hz: input.rate_hz / 2.0,
            max_items: self.cap,
            hold_items: per_chunk + 2,
        }])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let m = input.meta;
        if restarts(m.flags) {
            self.clear();
        }
        let n = input.data.len();
        let chip_at = |i: usize| -> f32 {
            match input.data {
                PortSlice::Soft(x) => x[i],
                PortSlice::Bits(x) => {
                    if x[i] & 1 == 1 {
                        1.0
                    } else {
                        -1.0
                    }
                }
                _ => 0.0,
            }
        };
        if !matches!(input.data, PortSlice::Soft(_) | PortSlice::Bits(_)) {
            return Err(mismatch(0, PortType::Soft, input.data.port_type()));
        }
        let per_item = 2.0 * m.source_per_item;
        for i in 0..n {
            let c = chip_at(i);
            if let Some(p) = self.prev {
                let aligned = (self.chip - 1) % 2 == self.phase;
                let denom = (p.abs() + c.abs()).max(1e-12);
                let v = f64::from(1.0 - (p - c).abs() / denom).clamp(0.0, 1.0);
                let k = usize::from(!aligned);
                self.viol[k] +=
                    VIOLATION_ALPHA.max(1.0 / (self.pairs[k] + 1) as f64) * (v - self.viol[k]);
                self.pairs[k] += 1;
                if aligned {
                    // The pair's first chip is input item index + i − 1.
                    let source = source_at(&m, (m.index + i as u64) as f64 - 1.0);
                    let bit = u8::from(p > c) ^ self.ieee;
                    self.ev_viol += v;
                    self.ev_pairs += 1;
                    self.ev_bits.push(bit);
                    self.hold(bit, source, per_item);
                }
                if self.auto
                    && self.pairs[0] >= ALIGN_MIN_PAIRS
                    && self.pairs[1] >= ALIGN_MIN_PAIRS
                    && self.viol[1] + ALIGN_MARGIN < self.viol[0]
                {
                    self.phase ^= 1;
                    self.viol.swap(0, 1);
                    self.pairs.swap(0, 1);
                    self.open = false;
                    self.status.extra.set("realigned", 1.0);
                }
            }
            self.prev = Some(c);
            self.chip += 1;
        }
        // Emit the oldest segment: its items are exactly two chips apart.
        let (source, per) = match self.segments[..self.n_segments].first() {
            Some(s) => (s.source, s.per_item),
            None => (m.source_index, per_item),
        };
        let produced = self.oldest_len();
        let out = io.output(0)?;
        set_meta(out, &m, source, per);
        bits_out(out)?.extend_from_slice(&self.held[..produced]);
        self.consume(produced);
        if self.dropped > 0 {
            self.status.extra.set("dropped_bits", self.dropped as f64);
        }
        self.status.items_in += n as u64;
        self.status.items_out += produced as u64;
        self.status.error_rate = Some(self.viol[0] as f32);
        self.status.quality = Some((1.0 - self.viol[0]) as f32);
        self.status.lock = if self.pairs[0] >= ALIGN_MIN_PAIRS && self.viol[0] < 0.25 {
            Lock::Locked
        } else {
            Lock::Searching
        };
        Ok(())
    }

    fn reset(&mut self) {
        self.clear();
        self.ev_viol = 0.0;
        self.ev_pairs = 0;
        self.ev_bits.clear();
    }

    /// S3, both in group `bit_shape` (ρ 0.706, ADR-0015 §13.1): `line_violations` = the mean
    /// violation over the pairs decoded at the chosen alignment (0 for clean Manchester, 0.5 for
    /// random chips; **smaller is evidence**), and `bit_structure` of the decoded bits.
    fn evidence(&self, out: &mut EvidenceSet) {
        if self.ev_pairs > 0 {
            calibrated(
                out,
                Stage::S3,
                MetricId::LineViolations,
                GroupId::BitShape,
                self.ev_viol / self.ev_pairs as f64,
                self.ev_pairs,
            );
        }
        self.ev_bits.evidence(out);
    }

    fn update_params(&mut self, p: &Params, _: &BuildCtx<'_>) -> Result<ParamUpdate, BlockError> {
        self.apply(p);
        Ok(ParamUpdate::Applied)
    }

    fn status(&self) -> Status {
        self.status
    }
}

#[cfg(test)]
mod tests {
    use crate::blocks::iq::testkit::*;
    use crate::buffer::PortVec;
    use hk_recipe::PortType;
    use serde_json::json;

    #[test]
    fn slicer_threshold_and_invert_are_hot() {
        let x = PortVec::Soft(vec![0.5, -0.5, 0.1, -0.1, 0.0]);
        let c = assert_chunk_invariant(
            || vec![build("slicer", json!({"threshold": 0.05}), PortType::Soft)],
            PortType::Soft,
            100.0,
            &x,
            &[5, 2, 1],
        );
        assert_eq!(c.out(0, 0).bits, vec![1, 0, 1, 0, 0]);
        let mut b = build("slicer", json!({}), PortType::Soft);
        assert_eq!(
            update(b.as_mut(), json!({"invert": true}), PortType::Soft),
            crate::ParamUpdate::Applied
        );
    }

    #[test]
    fn diff_decode_and_nrzi_recover_encoded_bits() {
        let mut rng = Lcg::new(9);
        let truth: Vec<u8> = (0..500).map(|_| rng.bit()).collect();
        // Differential encoding: e[k] = e[k−1] ⊕ d[k], e[−1] = 1.
        let mut e = vec![1u8];
        for &d in &truth {
            let last = *e.last().unwrap();
            e.push(last ^ d);
        }
        let c = assert_chunk_invariant(
            || vec![build("diff_decode", json!({"mode": "xor"}), PortType::Bits)],
            PortType::Bits,
            1187.5,
            &PortVec::Bits(e.clone()),
            &[64, 7, 1],
        );
        assert_eq!(c.out(0, 0).bits, truth);
        assert_eq!(c.out(0, 0).metas[0].source_index, 1.0);
        // NRZI (HDLC): 0 = transition.
        let mut level = 0u8;
        let mut nrzi = vec![level];
        for &d in &truth {
            if d == 0 {
                level ^= 1;
            }
            nrzi.push(level);
        }
        let c = assert_chunk_invariant(
            || vec![build("nrzi", json!({}), PortType::Bits)],
            PortType::Bits,
            1200.0,
            &PortVec::Bits(nrzi),
            &[64, 3],
        );
        assert_eq!(c.out(0, 0).bits, truth);
        // NRZI encode (the running level) inverts NRZI decode: level 1 after a reset.
        let enc = assert_chunk_invariant(
            || {
                vec![
                    build("nrzi", json!({"direction": "encode"}), PortType::Bits),
                    build("nrzi", json!({}), PortType::Bits),
                ]
            },
            PortType::Bits,
            1200.0,
            &PortVec::Bits(truth.clone()),
            &[64, 5, 1],
        );
        assert_eq!(enc.out(0, 0).bits.len(), truth.len());
        assert_eq!(enc.out(0, 0).bits[0], 1 ^ truth[0] ^ 1);
        assert_eq!(enc.out(1, 0).bits, truth[1..]);
        let mut b = build("nrzi", json!({}), PortType::Bits);
        assert_eq!(
            update(b.as_mut(), json!({"direction": "encode"}), PortType::Bits),
            crate::ParamUpdate::Rebuild
        );
        let xnor = assert_chunk_invariant(
            || {
                vec![build(
                    "diff_decode",
                    json!({"mode": "xnor"}),
                    PortType::Bits,
                )]
            },
            PortType::Bits,
            1187.5,
            &PortVec::Bits(e),
            &[64],
        );
        assert!(
            xnor.out(0, 0)
                .bits
                .iter()
                .zip(&truth)
                .all(|(a, b)| a ^ b == 1)
        );
    }

    #[test]
    fn manchester_realigns_and_decodes_soft_and_bits() {
        let mut rng = Lcg::new(21);
        let truth: Vec<u8> = (0..400).map(|_| rng.bit()).collect();
        // Thomas: 1 = high, low. Start one chip late (a stray chip first).
        let mut chips = vec![1.0f32];
        for &b in &truth {
            let (a, c) = if b == 1 { (1.0, -1.0) } else { (-1.0, 1.0) };
            chips.push(a + 0.2 * (rng.gauss() as f32));
            chips.push(c + 0.2 * (rng.gauss() as f32));
        }
        let c = assert_chunk_invariant(
            || vec![build("manchester", json!({}), PortType::Soft)],
            PortType::Soft,
            2400.0,
            &PortVec::Soft(chips.clone()),
            &[128, 5],
        );
        let (errs, n) = bit_errors(&truth, &c.out(0, 0).bits, 60, 40);
        assert_eq!(errs, 0, "{errs}/{n}");
        assert!(n > 300);
        let hard: Vec<u8> = chips.iter().map(|&v| u8::from(v > 0.0)).collect();
        let c = assert_chunk_invariant(
            || {
                vec![build(
                    "manchester",
                    json!({"convention": "ieee"}),
                    PortType::Bits,
                )]
            },
            PortType::Bits,
            2400.0,
            &PortVec::Bits(hard),
            &[128],
        );
        let inverted: Vec<u8> = truth.iter().map(|b| b ^ 1).collect();
        let (errs, _) = bit_errors(&inverted, &c.out(0, 0).bits, 60, 40);
        assert_eq!(errs, 0);
    }

    #[test]
    fn manchester_time_map_stays_exact_across_mid_chunk_realignment() {
        let mut rng = Lcg::new(33);
        // Hard chips with a stray chip first and another mid-stream: two realignments.
        // Long enough after the second one for held segments to drain before END.
        let mut chips = vec![1u8];
        for k in 0..2_100 {
            if k == 300 {
                chips.push(0);
            }
            chips.extend(if rng.bit() == 1 { [1, 0] } else { [0, 1] });
        }
        let data = PortVec::Bits(chips.clone());
        let make = || vec![build("manchester", json!({}), PortType::Bits)];
        let mut outputs = Vec::new();
        for chunk in [1_000usize, 97, 8] {
            let c = assert_chunk_invariant(make, PortType::Bits, 2400.0, &data, &[chunk]);
            let out = c.out(0, 0).clone();
            // Every item decodes exactly the chip pair its time map points at (a one-chip
            // error would decide the neighbouring, straddling pair).
            let mut k0 = 0;
            for (meta, &len) in out.metas.iter().zip(&out.lens) {
                for j in 0..len {
                    let s = meta.source_index + j as f64 * meta.source_per_item;
                    assert_eq!(s.fract(), 0.0, "chunk {chunk}");
                    let s = s as usize;
                    let want = u8::from(chips[s] > chips[s + 1]);
                    assert_eq!(
                        out.bits[k0 + j],
                        want,
                        "chunk {chunk} item {} chip {s}",
                        k0 + j
                    );
                }
                k0 += len;
            }
            assert_eq!(k0, out.bits.len());
            assert!(out.bits.len() > 2_050, "chunk {chunk}: {}", out.bits.len());
            outputs.push(out.bits);
        }
        assert!(outputs.iter().all(|b| *b == outputs[0]));
        // Both realignments happened: the tail decodes the true bits on the true pairs.
        let tail = &outputs[0][outputs[0].len() - 200..];
        let n = chips.len();
        let truth: Vec<u8> = (0..200)
            .map(|k| u8::from(chips[n - 400 + 2 * k] > chips[n - 399 + 2 * k]))
            .collect();
        assert_eq!(tail, &truth[..]);
    }
}
