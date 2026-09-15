//! `slicer`, `diff_decode`, `nrzi`, `manchester`.

use hk_recipe::{Params, PortType};

use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::blocks::iq::common::*;
use crate::buffer::PortSlice;
use crate::registry::BuildCtx;
use crate::status::{Lock, Status};

// ------------------------------------------------------------------------------------- slicer

pub(crate) fn build_slicer(p: &Params, _: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let mut b = Slicer {
        threshold: 0.0,
        invert: 0,
        ones: 0,
        status: Status::default(),
    };
    b.apply(p);
    Ok(Box::new(b))
}

struct Slicer {
    threshold: f32,
    invert: u8,
    ones: u64,
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

    fn reset(&mut self) {}

    fn update_params(&mut self, p: &Params, _: &BuildCtx<'_>) -> Result<ParamUpdate, BlockError> {
        self.apply(p);
        Ok(ParamUpdate::Applied)
    }

    fn status(&self) -> Status {
        self.status
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
struct Differential {
    nrzi: bool,
    complement: u8,
    prev: Option<u8>,
    status: Status,
}

impl Differential {
    fn new(nrzi: bool) -> Self {
        Self {
            nrzi,
            complement: 0,
            prev: None,
            status: Status::default(),
        }
    }

    fn apply(&mut self, p: &Params) {
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
            hold_items: 1,
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
        let skip = usize::from(self.prev.is_none());
        let out = io.output(0)?;
        set_meta(
            out,
            &m,
            source_at(&m, (m.index + skip as u64) as f64),
            m.source_per_item,
        );
        let y = bits_out(out)?;
        let before = y.len();
        for &b in x {
            let b = b & 1;
            if let Some(p) = self.prev {
                y.push(b ^ p ^ self.complement);
            }
            self.prev = Some(b);
        }
        self.status.items_in += x.len() as u64;
        self.status.items_out += (y.len() - before) as u64;
        Ok(())
    }

    fn reset(&mut self) {
        self.prev = None;
    }

    fn update_params(&mut self, p: &Params, _: &BuildCtx<'_>) -> Result<ParamUpdate, BlockError> {
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

pub(crate) fn build_manchester(p: &Params, _: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let mut b = Manchester {
        ieee: 0,
        auto: true,
        phase: 0,
        prev: None,
        chip: 0,
        viol: [0.0; 2],
        pairs: [0; 2],
        status: Status::default(),
    };
    b.apply(p);
    Ok(Box::new(b))
}

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
    }
}

impl Block for Manchester {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = single_input(inputs, "manchester", &[PortType::Soft, PortType::Bits])?;
        Ok(vec![PortInfo {
            ty: PortType::Bits,
            rate_hz: input.rate_hz / 2.0,
            max_items: input.max_items / 2 + 1,
            hold_items: 2,
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
        let out = io.output(0)?;
        let y = bits_out(out)?;
        let before = y.len();
        let mut first_pair: Option<usize> = None;
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
                    first_pair.get_or_insert(i);
                    y.push(u8::from(p > c) ^ self.ieee);
                }
                if self.auto
                    && self.pairs[0] >= ALIGN_MIN_PAIRS
                    && self.pairs[1] >= ALIGN_MIN_PAIRS
                    && self.viol[1] + ALIGN_MARGIN < self.viol[0]
                {
                    self.phase ^= 1;
                    self.viol.swap(0, 1);
                    self.pairs.swap(0, 1);
                    self.status.extra.set("realigned", 1.0);
                }
            }
            self.prev = Some(c);
            self.chip += 1;
        }
        let produced = y.len() - before;
        // Output item k is the pair whose second chip is input item first_pair + 2k.
        let second = first_pair.map_or(m.index as f64 + 1.0, |i| (m.index + i as u64) as f64);
        set_meta(
            out,
            &m,
            source_at(&m, second - 1.0),
            2.0 * m.source_per_item,
        );
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
}
