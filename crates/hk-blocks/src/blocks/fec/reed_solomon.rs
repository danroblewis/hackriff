//! `reed_solomon` (T-611): per-frame Reed–Solomon decoding, `frames → frames`, beside `bch`
//! (ADR-0011 §9.3). The kernel and its conventions are in [`super::rs`].
//!
//! - **Layout.** The coded part of a frame is `span` (default: all of it). It is read as
//!   consecutive **code blocks** of `depth × n` symbols of `symbol_bits` bits, MSB first; a
//!   trailing partial block (and the `span`'s untouched prefix and suffix) passes through
//!   unchanged, as `bch` passes a partial word. A frame too short for one block is marked
//!   invalid and counted (`frames_short`).
//! - **Interleaving** (CCSDS `I`, `depth`): on-air symbol `q` of a block belongs to codeword
//!   `q mod depth`, as its symbol `q div depth`. So the block's first `depth × k` symbols are
//!   exactly the data, in on-air order, and `strip` keeps them and drops the last
//!   `depth × (n − k)` — for CCSDS, the transfer frame.
//! - **Dual basis** (`dual_basis`, the CCSDS convention; CCSDS field `0x187` only): symbols are
//!   Berlekamp-basis on air, converted to the conventional basis to decode and back after, so
//!   the output carries the (corrected) symbols as sent. Getting this wrong is silent: every
//!   codeword fails, which reads as a bad channel.
//! - **Result.** A frame is `valid` when every codeword in it decoded (combined with any
//!   earlier check, as every `fec` block does), otherwise `invalid`, and an uncorrectable
//!   codeword is left exactly as received. The **channel bits the decoder changed** go into
//!   `FrameInfo::corrected_bits` (the wire's `fec_corrected_bits`), as `crc`'s and `bch`'s
//!   corrections do; the symbol counts are status extras.
//! - **Status.** `error_rate`: the uncorrectable fraction of the last 256 codewords (a
//!   refinement objective, ADR-0011 §1.3); `quality` = 1 − that. Extras: `codewords_ok`,
//!   `codewords_corrected`, `codewords_bad`, `corrected_symbols`, `corrected_bits`,
//!   `frames_short`.
//! - **Allocation** (§1.4 rule 1): the scratch is sized at build; the frame buffers grow to the
//!   longest frame seen and are reused, so steady-state `process` allocates nothing.

use hk_recipe::{Params, PortType};

use super::rs::{Decoder, DualBasis, Field, Sym};
use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::blocks::framing::common::{
    P, RateMeter, Span, combine, extend_bits, frames_io, frames_port, one_input, update_hot,
};
use crate::registry::BuildCtx;
use crate::status::Status;

const HOT: &[&str] = &["drop_invalid"];

/// Builds a `reed_solomon`.
pub(crate) fn build(params: &Params, _ctx: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let p = P(params);
    let perr = |m: String| BlockError::Params(m);
    let m = p.uint_or("symbol_bits", 8)?;
    let n = p.req_uint("n")? as usize;
    let k = p.req_uint("k")? as usize;
    let poly = p
        .hex("poly")
        .ok_or_else(|| perr("poly is required".into()))?;
    let fcr = p.req_uint("fcr")? as usize;
    let prim = p.uint_or("prim", 1)? as usize;
    let field = Field::new(m, poly).map_err(perr)?;
    let dual = if p.bool_or("dual_basis", false) {
        Some(DualBasis::ccsds(&field).ok_or_else(|| {
            perr("dual_basis is the CCSDS representation: symbol_bits 8, poly 0x187".into())
        })?)
    } else {
        None
    };
    let code = Decoder::new(field, n, k, fcr, prim).map_err(perr)?;
    let depth = p.uint_or("depth", 1)? as usize;
    if depth == 0 {
        return Err(perr("depth must be at least 1".into()));
    }
    Ok(Box::new(ReedSolomon {
        params: params.clone(),
        span: Span::from_params(p)?,
        m: m as usize,
        depth,
        strip: p.bool_or("strip", true),
        drop_invalid: p.bool_or("drop_invalid", false),
        dual,
        syms: vec![0; depth * n],
        cw: vec![0; n],
        code,
        bits: Vec::new(),
        out: Vec::new(),
        meter: RateMeter::new(256),
        ok: 0,
        corrected: 0,
        bad: 0,
        corrected_symbols: 0,
        corrected_bits: 0,
        short: 0,
        status: Status::default(),
    }))
}

/// The block.
pub struct ReedSolomon {
    params: Params,
    span: Span,
    m: usize,
    depth: usize,
    strip: bool,
    drop_invalid: bool,
    dual: Option<DualBasis>,
    code: Decoder,
    /// One code block's symbols, on-air order, conventional basis.
    syms: Vec<Sym>,
    /// One codeword.
    cw: Vec<Sym>,
    bits: Vec<u8>,
    out: Vec<u8>,
    meter: RateMeter,
    ok: u64,
    corrected: u64,
    bad: u64,
    corrected_symbols: u64,
    corrected_bits: u64,
    short: u64,
    status: Status,
}

impl ReedSolomon {
    /// The symbol at bit `at` of the frame, as sent.
    fn air(&self, at: usize) -> Sym {
        self.bits[at..at + self.m]
            .iter()
            .fold(0, |acc, &b| (acc << 1) | Sym::from(b))
    }

    fn to_conv(&self, s: Sym) -> Sym {
        self.dual
            .as_ref()
            .map_or(s, |d| Sym::from(d.to_conv[s as usize]))
    }

    fn to_air(&self, s: Sym) -> Sym {
        self.dual
            .as_ref()
            .map_or(s, |d| Sym::from(d.to_dual[s as usize]))
    }

    /// Decodes the block at bit `base` and appends its output to `self.out`: (all codewords
    /// decoded, channel bits changed).
    fn block(&mut self, base: usize) -> (bool, u32) {
        let (n, depth, m) = (self.code.n, self.depth, self.m);
        for q in 0..depth * n {
            self.syms[q] = self.to_conv(self.air(base + q * m));
        }
        let mut all = true;
        for c in 0..depth {
            for j in 0..n {
                self.cw[j] = self.syms[j * depth + c];
            }
            let res = self.code.decode(&mut self.cw);
            self.meter.push(res.is_none());
            match res {
                None => {
                    self.bad += 1;
                    all = false;
                }
                Some(0) => self.ok += 1,
                Some(e) => {
                    self.corrected += 1;
                    self.corrected_symbols += e as u64;
                    for j in 0..n {
                        self.syms[j * depth + c] = self.cw[j];
                    }
                }
            }
        }
        let keep = depth * if self.strip { self.code.k } else { n };
        let mut flipped = 0;
        for q in 0..depth * n {
            let sent = self.air(base + q * m);
            let now = self.to_air(self.syms[q]);
            flipped += (sent ^ now).count_ones();
            if q < keep {
                self.out.extend((0..m).rev().map(|b| (now >> b) as u8 & 1));
            }
        }
        (all, flipped)
    }
}

impl Block for ReedSolomon {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = one_input("reed_solomon", inputs, &[PortType::Frames])?;
        Ok(vec![frames_port(input, input.max_items)])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let (_, frames, buf) = frames_io(io)?;
        let before = buf.len();
        let block_bits = self.depth * self.code.n * self.m;
        for f in frames.iter() {
            let len = f.info.bit_len as usize;
            self.bits.clear();
            extend_bits(&mut self.bits, f.bytes, 0, len);
            let start = self.span.start.min(len);
            let covered = len.saturating_sub(self.span.start + self.span.trim);
            let blocks = covered / block_bits;
            self.out.clear();
            self.out.extend_from_slice(&self.bits[..start]);
            let (mut all, mut flipped) = (blocks > 0, 0u32);
            if blocks == 0 {
                self.short += 1;
            }
            for b in 0..blocks {
                let (ok, c) = self.block(start + b * block_bits);
                all &= ok;
                flipped += c;
            }
            self.out
                .extend_from_slice(&self.bits[start + blocks * block_bits..]);
            self.corrected_bits += u64::from(flipped);
            if !all && self.drop_invalid {
                continue;
            }
            let mut info = f.info.clone();
            info.check = combine(info.check, all);
            info.corrected_bits = info.corrected_bits.saturating_add(flipped);
            if flipped > 0 || self.strip {
                info.layers = None;
            }
            buf.push_bits(&self.out, info);
        }
        let s = &mut self.status;
        s.items_in += frames.len() as u64;
        s.items_out += (buf.len() - before) as u64;
        s.error_rate = self.meter.rate();
        s.quality = self.meter.rate().map(|r| 1.0 - r);
        s.extra.set("codewords_ok", self.ok as f64);
        s.extra.set("codewords_corrected", self.corrected as f64);
        s.extra.set("codewords_bad", self.bad as f64);
        s.extra
            .set("corrected_symbols", self.corrected_symbols as f64);
        s.extra.set("corrected_bits", self.corrected_bits as f64);
        s.extra.set("frames_short", self.short as f64);
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
