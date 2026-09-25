//! `checksum`: additive / XOR / ones'-complement checksums over units of the frame, compared
//! with a check field at the end of the span (NMEA-style XOR, IP-style ones'-complement).

use hk_recipe::{Params, PortType};

use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::blocks::framing::common::{
    P, RateMeter, Span, combine, extend_bits, frames_io, frames_port, one_input, read_bits,
    update_hot,
};
use crate::evidence::CheckTally;
use crate::registry::BuildCtx;
use crate::status::Status;
use hk_model::CrcStatus;
use hk_model::synth::EvidenceSet;

const HOT: &[&str] = &["drop_invalid"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Algorithm {
    Sum,
    Xor,
    OnesComplement,
}

/// Builds a `checksum`.
pub(crate) fn build(params: &Params, _ctx: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let p = P(params);
    let perr = |m: &str| BlockError::Params(m.into());
    let algorithm = match p.str("algorithm") {
        Some("sum") => Algorithm::Sum,
        Some("xor") => Algorithm::Xor,
        Some("ones-complement") => Algorithm::OnesComplement,
        _ => return Err(perr("algorithm must be sum, xor or ones-complement")),
    };
    let unit = p.uint_or("unit_bits", 8)? as usize;
    if !matches!(unit, 8 | 16 | 32) {
        return Err(perr("unit_bits must be 8, 16 or 32"));
    }
    let width = p.uint_or("width", unit as u32)? as usize;
    let init = p.hex_or("init", 0);
    if init >> width != 0 {
        return Err(perr("init wider than width"));
    }
    Ok(Box::new(Checksum {
        params: params.clone(),
        algorithm,
        unit,
        width,
        little: p.str("endianness") == Some("little"),
        init,
        complement: p.bool_or("complement", false),
        span: Span::from_params(p)?,
        strip: p.bool_or("strip", true),
        drop_invalid: p.bool_or("drop_invalid", false),
        bits: Vec::new(),
        out: Vec::new(),
        meter: RateMeter::new(64),
        frames_ok: 0,
        frames_bad: 0,
        ev: CheckTally::default(),
        status: Status::default(),
    }))
}

/// The block.
pub struct Checksum {
    params: Params,
    algorithm: Algorithm,
    unit: usize,
    width: usize,
    little: bool,
    init: u64,
    complement: bool,
    span: Span,
    strip: bool,
    drop_invalid: bool,
    bits: Vec<u8>,
    out: Vec<u8>,
    meter: RateMeter,
    frames_ok: u64,
    frames_bad: u64,
    /// Evidence (T-853), since `reset()`.
    ev: CheckTally,
    status: Status,
}

impl Checksum {
    /// A multi-byte value at `pos` in the configured byte order (`n` a whole number of bytes
    /// when little-endian).
    fn value(&self, bytes: &[u8], pos: usize, n: usize) -> u64 {
        if self.little && n % 8 == 0 {
            (0..n / 8).fold(0, |a, j| a | (read_bits(bytes, pos + 8 * j, 8) << (8 * j)))
        } else {
            read_bits(bytes, pos, n)
        }
    }

    /// (passed, check position) or `None` when the frame is too short.
    fn check(&self, bytes: &[u8], len: usize) -> Option<(bool, usize)> {
        let (start, w) = (self.span.start, self.width);
        let cpos = len
            .checked_sub(self.span.trim + w)
            .filter(|&c| c >= start)?;
        let mask = if w >= 64 { u64::MAX } else { (1 << w) - 1 };
        let mut acc = self.init;
        let mut pos = start;
        while pos < cpos {
            let n = self.unit.min(cpos - pos);
            // A trailing partial unit is zero-padded on the right.
            let u = self.value(bytes, pos, n) << (self.unit - n);
            acc = match self.algorithm {
                Algorithm::Sum => acc.wrapping_add(u) & mask,
                Algorithm::Xor => (acc ^ u) & mask,
                Algorithm::OnesComplement => {
                    let mut s = acc + u;
                    while s >> w != 0 {
                        s = (s & mask) + (s >> w);
                    }
                    s
                }
            };
            pos += n;
        }
        if self.complement {
            acc = !acc & mask;
        }
        Some((acc == self.value(bytes, cpos, w), cpos))
    }
}

impl Block for Checksum {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = one_input("checksum", inputs, &[PortType::Frames])?;
        Ok(vec![frames_port(input, input.max_items)])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let (_, frames, buf) = frames_io(io)?;
        let before = buf.len();
        for f in frames.iter() {
            let len = f.info.bit_len as usize;
            let result = self.check(f.bytes, len);
            let ok = result.is_some_and(|r| r.0);
            self.bits.clear();
            extend_bits(&mut self.bits, f.bytes, 0, len);
            let upstream_clean = !matches!(f.info.check, CrcStatus::Corrected | CrcStatus::Invalid);
            // The tally sees the bits the check covers (`start_bit` .. the end of the check
            // field), never the whole frame: the degenerate-frame guard must trim the register
            // off the *covered* span (T-928, ADR-0022 §4.3.1 hole A).
            let covered = result.map_or(0..0, |(_, cpos)| self.span.start..cpos + self.width);
            self.ev.record(
                &self.bits[covered],
                ok && upstream_clean,
                self.width as f64,
                self.width,
            );
            self.meter.push(!ok);
            if ok {
                self.frames_ok += 1;
            } else {
                self.frames_bad += 1;
            }
            if !ok && self.drop_invalid {
                continue;
            }
            self.out.clear();
            match result {
                Some((_, cpos)) if self.strip => {
                    self.out.extend_from_slice(&self.bits[..cpos]);
                    self.out.extend_from_slice(&self.bits[cpos + self.width..]);
                }
                _ => self.out.extend_from_slice(&self.bits),
            }
            let mut info = f.info.clone();
            info.check = combine(info.check, ok);
            if self.strip {
                info.layers = None;
            }
            buf.push_bits(&self.out, info);
        }
        let s = &mut self.status;
        s.items_in += frames.len() as u64;
        s.items_out += (buf.len() - before) as u64;
        s.error_rate = self.meter.rate();
        s.quality = self.meter.rate().map(|r| 1.0 - r);
        s.extra.set("frames_ok", self.frames_ok as f64);
        s.extra.set("frames_bad", self.frames_bad as f64);
        Ok(())
    }

    fn reset(&mut self) {
        self.ev.clear();
    }

    /// S5 `check_distinct_valid` (ADR-0015 §2.2, ADR-0022 §4.2, analytic): independent frames
    /// valid **without FEC correction** (T-210) among those tested — see `CheckTally`.
    fn evidence(&self, out: &mut EvidenceSet) {
        self.ev.evidence(out);
    }

    fn update_params(
        &mut self,
        params: &Params,
        _ctx: &BuildCtx<'_>,
    ) -> Result<ParamUpdate, BlockError> {
        let drop = &mut self.drop_invalid;
        update_hot(&mut self.params, params, HOT, |p| {
            *drop = p.bool_or("drop_invalid", false)
        })
    }

    fn status(&self) -> Status {
        self.status
    }
}
