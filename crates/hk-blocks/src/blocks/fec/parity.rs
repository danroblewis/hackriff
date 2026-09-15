//! `parity`: per-unit parity (e.g. 7-bit characters + parity bit) over a span of the frame;
//! optionally strips the parity bits, or (`zero`) replaces each with a constant 0 in place
//! (unit width unchanged) — for a check field computed, like ACARS's block check, over the
//! pre-parity data padded back to the unit width rather than over the as-sent bits or a
//! repacked (narrower, misaligned) bit stream.

use hk_recipe::{Params, PortType};

use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::blocks::framing::common::{
    P, RateMeter, Span, combine, extend_bits, frames_io, frames_port, one_input, update_hot,
};
use crate::registry::BuildCtx;
use crate::status::Status;

const HOT: &[&str] = &["drop_invalid"];

/// Builds a `parity`.
pub(crate) fn build(params: &Params, _ctx: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let p = P(params);
    let strip = p.bool_or("strip", false);
    let zero = p.bool_or("zero", false);
    if strip && zero {
        return Err(BlockError::Params("strip and zero are exclusive".into()));
    }
    Ok(Box::new(Parity {
        params: params.clone(),
        unit: p.req_uint("unit_bits")? as usize,
        odd: match p.str("parity") {
            Some("odd") => true,
            Some("even") => false,
            _ => return Err(BlockError::Params("parity must be even or odd".into())),
        },
        first: p.str("position") == Some("first"),
        span: Span::from_params(p)?,
        strip,
        zero,
        drop_invalid: p.bool_or("drop_invalid", false),
        bits: Vec::new(),
        out: Vec::new(),
        meter: RateMeter::new(256),
        units_ok: 0,
        units_bad: 0,
        status: Status::default(),
    }))
}

/// The block.
pub struct Parity {
    params: Params,
    unit: usize,
    odd: bool,
    first: bool,
    span: Span,
    strip: bool,
    zero: bool,
    drop_invalid: bool,
    bits: Vec<u8>,
    out: Vec<u8>,
    meter: RateMeter,
    units_ok: u64,
    units_bad: u64,
    status: Status,
}

impl Block for Parity {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = one_input("parity", inputs, &[PortType::Frames])?;
        Ok(vec![frames_port(input, input.max_items)])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let (_, frames, buf) = frames_io(io)?;
        let before = buf.len();
        for f in frames.iter() {
            self.bits.clear();
            extend_bits(&mut self.bits, f.bytes, 0, f.info.bit_len as usize);
            let len = self.bits.len();
            let end = len.saturating_sub(self.span.trim);
            let start = self.span.start.min(len);
            self.out.clear();
            self.out.extend_from_slice(&self.bits[..start]);
            let (mut pos, mut all, mut checked) = (start, true, false);
            while pos + self.unit <= end {
                let u = &self.bits[pos..pos + self.unit];
                let ones = u.iter().filter(|&&b| b == 1).count();
                let ok = (ones % 2 == 1) == self.odd;
                self.meter.push(!ok);
                if ok {
                    self.units_ok += 1;
                } else {
                    self.units_bad += 1;
                }
                all &= ok;
                checked = true;
                match (self.strip, self.zero, self.first) {
                    (false, false, _) => self.out.extend_from_slice(u),
                    (true, _, true) => self.out.extend_from_slice(&u[1..]),
                    (true, _, false) => self.out.extend_from_slice(&u[..self.unit - 1]),
                    (false, true, true) => {
                        self.out.push(0);
                        self.out.extend_from_slice(&u[1..]);
                    }
                    (false, true, false) => {
                        self.out.extend_from_slice(&u[..self.unit - 1]);
                        self.out.push(0);
                    }
                }
                pos += self.unit;
            }
            self.out.extend_from_slice(&self.bits[pos..]);
            if !all && self.drop_invalid {
                continue;
            }
            let mut info = f.info.clone();
            if checked {
                info.check = combine(info.check, all);
            }
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
        s.extra.set("units_ok", self.units_ok as f64);
        s.extra.set("units_bad", self.units_bad as f64);
        Ok(())
    }

    fn reset(&mut self) {}

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
