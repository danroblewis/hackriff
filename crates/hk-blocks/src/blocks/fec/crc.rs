//! `crc`: checks a CRC (RevEng model, hk-estimate `BitCrc`) over the frame (`span`) or per
//! block with offset words (`blocks`, RDS), optionally corrects error bursts by syndrome, strips
//! the check bits and sets the frame's check status.
//!
//! **Correction bound.** Burst correction is refused wherever it would turn more than
//! `MAX_FALSE_CORRECTION` of random blocks valid (`false_correction`): at build for block mode
//! and the shortest span frame, and per span length at run time (`correction_skipped`). A
//! 10-bit RDS check allows no correction; CRC-24 over a 112-bit Mode S frame allows bursts ≤ 5.
//!
//! **Check field.** Read MSB first from the frame, except a reflected (`refout`) whole-byte
//! CRC, which is read as little-endian bytes: the order a LSB-first protocol sends it once the
//! framing block has reversed its characters (ACARS CRC-16/KERMIT).

use hk_estimate::framing::crc::BitCrc;
use hk_recipe::{Params, PortType};

use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::blocks::framing::common::{
    P, RateMeter, Span, combine, extend_bits, frames_io, frames_port, one_input, read_bits,
    update_hot,
};
use crate::registry::BuildCtx;
use crate::status::Status;

const HOT: &[&str] = &["drop_invalid"];
const CACHE: usize = 4;

enum Mode {
    Span(Span),
    Blocks {
        data: usize,
        offsets: Vec<Vec<u32>>,
        units: Vec<u32>,
    },
}

/// Builds a `crc`.
pub(crate) fn build(params: &Params, _ctx: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let p = P(params);
    let perr = |m: &str| BlockError::Params(m.into());
    let width = p.req_uint("width")? as usize;
    let poly = p.hex("poly").ok_or_else(|| perr("poly is required"))?;
    let (init, xorout) = (p.hex_or("init", 0), p.hex_or("xorout", 0));
    if (init | xorout) >> width != 0 {
        return Err(perr("init/xorout wider than width"));
    }
    let refout = p.bool_or("refout", false);
    let crc = BitCrc::new(
        width as u8,
        poly,
        init as u32,
        p.bool_or("refin", false),
        refout,
        xorout as u32,
    )
    .ok_or_else(|| perr("poly wider than width"))?;
    let le = refout && width % 8 == 0;
    let burst = p.uint_or("correct_burst_bits", 0)? as usize;
    if burst * 2 > width {
        return Err(perr("correct_burst_bits must be at most width / 2"));
    }
    let refuse = |len: usize, accepted: usize| {
        let pf = false_correction(len, burst, accepted, width);
        (pf > MAX_FALSE_CORRECTION).then(|| {
            BlockError::Params(format!(
                "correct_burst_bits {burst} would turn {pf:.1e} of random {len}-bit blocks valid \
                 (limit {MAX_FALSE_CORRECTION:.0e}); lower it or use a wider CRC"
            ))
        })
    };
    let mode = match (p.obj("span"), p.obj("blocks")) {
        (Some(_), Some(_)) => return Err(perr("span and blocks are exclusive")),
        (_, Some(b)) => {
            let data = b.req_uint("data_bits")? as usize;
            if b.req_uint("check_bits")? as usize != width {
                return Err(perr("blocks.check_bits must equal width"));
            }
            let offsets = b
                .list("offsets")
                .iter()
                .map(|pos| {
                    pos.as_array()
                        .ok_or_else(|| perr("offsets entry"))?
                        .iter()
                        .map(|w| {
                            w.as_str()
                                .and_then(hk_recipe::parse_hex)
                                .filter(|v| v >> width == 0)
                                .map(|v| v as u32)
                                .ok_or_else(|| perr("offset word wider than width"))
                        })
                        .collect::<Result<Vec<_>, _>>()
                })
                .collect::<Result<Vec<_>, _>>()?;
            if offsets.is_empty() {
                return Err(perr("blocks.offsets is empty"));
            }
            let accepted = offsets.iter().map(Vec::len).max().unwrap_or(1);
            if let Some(e) = refuse(data + width, accepted) {
                return Err(e);
            }
            Mode::Blocks {
                data,
                units: units(&crc, data, width, le),
                offsets,
            }
        }
        _ => {
            // The shortest frame (check word alone); longer frames are bounded per length in
            // `check`.
            if let Some(e) = refuse(width, 1) {
                return Err(e);
            }
            Mode::Span(Span::from_params(p)?)
        }
    };
    let window = match mode {
        Mode::Span(_) => 64,
        Mode::Blocks { .. } => 256,
    };
    Ok(Box::new(Crc {
        params: params.clone(),
        crc,
        width,
        le,
        mode,
        strip: p.bool_or("strip", true),
        drop_invalid: p.bool_or("drop_invalid", false),
        burst,
        bits: Vec::new(),
        out: Vec::new(),
        cache: Vec::with_capacity(CACHE),
        meter: RateMeter::new(window),
        frames_ok: 0,
        frames_bad: 0,
        blocks_bad: 0,
        corrected_bits: 0,
        correction_skipped: 0,
        status: Status::default(),
    }))
}

/// Largest accepted chance that burst correction turns a random (garbage) block valid.
const MAX_FALSE_CORRECTION: f64 = 1e-3;

/// Upper bound on the chance that syndrome correction of bursts up to `burst` bits turns a
/// uniformly random `len`-bit block (data + check) valid: correctable burst patterns (start,
/// mask with both end bits set) × accepted syndromes (offset words) / 2^width.
fn false_correction(len: usize, burst: usize, accepted: usize, width: usize) -> f64 {
    let patterns: f64 = (1..=burst.min(len))
        .map(|t| (len + 1 - t) as f64 * 2f64.powi(t.saturating_sub(2) as i32))
        .sum();
    patterns * accepted as f64 / 2f64.powi(width as i32)
}

/// The check field at `pos`.
fn read_check(bytes: &[u8], pos: usize, width: usize, le: bool) -> u32 {
    if le {
        (0..width / 8).fold(0u64, |a, j| {
            a | (read_bits(bytes, pos + 8 * j, 8) << (8 * j))
        }) as u32
    } else {
        read_bits(bytes, pos, width) as u32
    }
}

/// Syndrome of a single-bit error at each position of `data` data bits then `width` check bits.
fn units(crc: &BitCrc, data: usize, width: usize, le: bool) -> Vec<u32> {
    let mut v = Vec::with_capacity(data + width);
    let mut buf = vec![0u8; data.div_ceil(8).max(1)];
    for i in 0..data {
        buf[i / 8] = 1 << (7 - i % 8);
        v.push(crc.linear(&buf, 0, data));
        buf[i / 8] = 0;
    }
    let mut cb = vec![0u8; width.div_ceil(8)];
    for j in 0..width {
        cb[j / 8] = 1 << (7 - j % 8);
        v.push(read_check(&cb, 0, width, le));
        cb[j / 8] = 0;
    }
    v
}

/// The unique burst (start, mask with bit 0 set, length ≤ `burst`) whose syndrome `accept`s;
/// `None` when there is none or more than one.
fn find_burst(units: &[u32], burst: usize, accept: impl Fn(u32) -> bool) -> Option<(usize, u32)> {
    let mut found = None;
    for st in 0..units.len() {
        for mask in (1u32..(1 << burst)).step_by(2) {
            let top = 32 - mask.leading_zeros() as usize;
            if st + top > units.len() {
                continue;
            }
            let mut x = 0;
            let mut m = mask;
            while m != 0 {
                x ^= units[st + m.trailing_zeros() as usize];
                m &= m - 1;
            }
            if accept(x) {
                if found.is_some() {
                    return None;
                }
                found = Some((st, mask));
            }
        }
    }
    found
}

/// The block.
pub struct Crc {
    params: Params,
    crc: BitCrc,
    width: usize,
    le: bool,
    mode: Mode,
    strip: bool,
    drop_invalid: bool,
    burst: usize,
    bits: Vec<u8>,
    out: Vec<u8>,
    /// Single-bit syndromes per span data length; `None` where correction at that length
    /// exceeds `MAX_FALSE_CORRECTION`.
    cache: Vec<(usize, Option<Vec<u32>>)>,
    meter: RateMeter,
    frames_ok: u64,
    frames_bad: u64,
    blocks_bad: u64,
    corrected_bits: u64,
    correction_skipped: u64,
    status: Status,
}

impl Crc {
    /// Checks one frame (unpacked into `self.bits`, corrected in place); fills `self.out`.
    /// Returns (passed, corrected bits).
    fn check(&mut self, bytes: &[u8]) -> (bool, u32) {
        let len = self.bits.len();
        let w = self.width;
        self.out.clear();
        match &self.mode {
            Mode::Span(span) => {
                if len < span.start + span.trim + w {
                    self.meter.push(true);
                    self.out.extend_from_slice(&self.bits);
                    return (false, 0);
                }
                let data = len - span.trim - w - span.start;
                let cpos = span.start + data;
                let s =
                    self.crc.compute(bytes, span.start, data) ^ read_check(bytes, cpos, w, self.le);
                let mut result = (s == 0, 0);
                if s != 0 && self.burst > 0 {
                    let i = match self.cache.iter().position(|(l, _)| *l == data) {
                        Some(i) => i,
                        None => {
                            if self.cache.len() == CACHE {
                                self.cache.remove(0);
                            }
                            let u = (false_correction(data + w, self.burst, 1, w)
                                <= MAX_FALSE_CORRECTION)
                                .then(|| units(&self.crc, data, w, self.le));
                            self.cache.push((data, u));
                            self.cache.len() - 1
                        }
                    };
                    let found = match &self.cache[i].1 {
                        Some(u) => find_burst(u, self.burst, |x| x == s),
                        None => {
                            self.correction_skipped += 1;
                            None
                        }
                    };
                    if let Some((st, mask)) = found {
                        let mut m = mask;
                        while m != 0 {
                            let idx = st + m.trailing_zeros() as usize;
                            let pos = if idx < data {
                                span.start + idx
                            } else {
                                cpos + idx - data
                            };
                            self.bits[pos] ^= 1;
                            m &= m - 1;
                        }
                        result = (true, mask.count_ones());
                    }
                }
                self.meter.push(!result.0);
                if self.strip {
                    self.out.extend_from_slice(&self.bits[..cpos]);
                    self.out.extend_from_slice(&self.bits[cpos + w..]);
                } else {
                    self.out.extend_from_slice(&self.bits);
                }
                result
            }
            Mode::Blocks {
                data,
                offsets,
                units,
            } => {
                let bb = data + w;
                let n = offsets.len();
                if len < n * bb {
                    self.meter.push(true);
                    self.out.extend_from_slice(&self.bits);
                    return (false, 0);
                }
                let (mut all, mut corrected) = (true, 0);
                for (b, allowed) in offsets.iter().enumerate() {
                    let base = b * bb;
                    let s = self.crc.compute(bytes, base, *data)
                        ^ read_check(bytes, base + data, w, self.le);
                    let mut ok = allowed.contains(&s);
                    if !ok
                        && self.burst > 0
                        && let Some((st, mask)) =
                            find_burst(units, self.burst, |x| allowed.contains(&(s ^ x)))
                    {
                        let mut m = mask;
                        while m != 0 {
                            self.bits[base + st + m.trailing_zeros() as usize] ^= 1;
                            m &= m - 1;
                        }
                        corrected += mask.count_ones();
                        ok = true;
                    }
                    self.meter.push(!ok);
                    if !ok {
                        self.blocks_bad += 1;
                    }
                    all &= ok;
                }
                if self.strip {
                    for b in 0..n {
                        self.out
                            .extend_from_slice(&self.bits[b * bb..b * bb + data]);
                    }
                    self.out.extend_from_slice(&self.bits[n * bb..]);
                } else {
                    self.out.extend_from_slice(&self.bits);
                }
                (all, corrected)
            }
        }
    }
}

impl Block for Crc {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = one_input("crc", inputs, &[PortType::Frames])?;
        Ok(vec![frames_port(input, input.max_items)])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let (_, frames, buf) = frames_io(io)?;
        let before = buf.len();
        for f in frames.iter() {
            self.bits.clear();
            extend_bits(&mut self.bits, f.bytes, 0, f.info.bit_len as usize);
            let (ok, corrected) = self.check(f.bytes);
            if ok {
                self.frames_ok += 1;
            } else {
                self.frames_bad += 1;
            }
            self.corrected_bits += u64::from(corrected);
            if !ok && self.drop_invalid {
                continue;
            }
            let mut info = f.info.clone();
            info.check = combine(info.check, ok);
            info.corrected_bits += corrected;
            if self.strip || corrected > 0 {
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
        s.extra.set("corrected_bits", self.corrected_bits as f64);
        if matches!(self.mode, Mode::Blocks { .. }) {
            s.extra.set("blocks_bad", self.blocks_bad as f64);
        } else if self.burst > 0 {
            s.extra
                .set("correction_skipped", self.correction_skipped as f64);
        }
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
