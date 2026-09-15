//! `bch`: binary cyclic (BCH) codeword decoding with bounded-distance correction by syndrome
//! table, plus an optional overall parity bit (POCSAG: BCH(31,21), g = 0x769, even parity).
//! The frame splits into `word_bits` words; each word's first `n` bits are the codeword (k data
//! bits then n − k check bits, MSB first), bit `n` the parity bit, the rest ignored.

use hk_recipe::{Params, PortType};

use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::blocks::framing::common::{
    P, RateMeter, combine, extend_bits, frames_io, frames_port, one_input, read_bits, update_hot,
};
use crate::registry::BuildCtx;
use crate::status::Status;

const HOT: &[&str] = &["drop_invalid"];
const AMBIGUOUS: u64 = u64::MAX;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Parity {
    None,
    Even,
    Odd,
}

/// Builds a `bch`.
pub(crate) fn build(params: &Params, _ctx: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let p = P(params);
    let perr = |m: &str| BlockError::Params(m.into());
    let n = p.req_uint("n")? as usize;
    let k = p.req_uint("k")? as usize;
    if k >= n || n > 63 {
        return Err(perr("need k < n ≤ 63"));
    }
    let r = n - k;
    if r > 16 {
        return Err(perr("n − k above 16 check bits"));
    }
    let poly = p.hex("poly").ok_or_else(|| perr("poly is required"))?;
    let generator = match poly >> r {
        0 => poly | 1 << r,
        1 => poly,
        _ => return Err(perr("poly degree above n − k")),
    };
    if generator & 1 == 0 {
        return Err(perr("poly must have a constant term"));
    }
    let parity = match p.str("parity").unwrap_or("none") {
        "even" => Parity::Even,
        "odd" => Parity::Odd,
        _ => Parity::None,
    };
    let word_bits = p.uint_or("word_bits", 32)? as usize;
    if word_bits < n + usize::from(parity != Parity::None) {
        return Err(perr(
            "word_bits too short for the codeword (and parity bit)",
        ));
    }
    let t = p.uint_or("correct_bits", 1)? as usize;
    let mut bch = Bch {
        params: params.clone(),
        word_bits,
        n,
        r,
        generator,
        parity,
        t,
        table: vec![0; 1 << r],
        drop_invalid: p.bool_or("drop_invalid", false),
        bits: Vec::new(),
        meter: RateMeter::new(256),
        words_ok: 0,
        words_corrected: 0,
        words_bad: 0,
        corrected_bits: 0,
        status: Status::default(),
    };
    bch.fill_table();
    Ok(Box::new(bch))
}

/// The block.
pub struct Bch {
    params: Params,
    word_bits: usize,
    n: usize,
    r: usize,
    generator: u64,
    parity: Parity,
    t: usize,
    /// Syndrome → lowest-weight error pattern (0: none within `t`; `AMBIGUOUS`).
    table: Vec<u64>,
    drop_invalid: bool,
    bits: Vec<u8>,
    meter: RateMeter,
    words_ok: u64,
    words_corrected: u64,
    words_bad: u64,
    corrected_bits: u64,
    status: Status,
}

impl Bch {
    /// Remainder of the n-bit codeword `c` (bit n−1 = first on air) by the generator.
    fn syndrome(&self, mut c: u64) -> u64 {
        for i in (self.r..self.n).rev() {
            if c >> i & 1 == 1 {
                c ^= self.generator << (i - self.r);
            }
        }
        c
    }

    fn fill_table(&mut self) {
        // By increasing weight, so a lower weight always claims its syndrome first; two
        // patterns of the same lowest weight make the syndrome ambiguous (uncorrectable).
        for w in 1..=self.t {
            fn exact(b: &mut Bch, from: usize, left: usize, pattern: u64, w: u32) {
                for pos in from..b.n {
                    let e = pattern | 1 << pos;
                    if left == 1 {
                        let s = b.syndrome(e) as usize;
                        let slot = b.table[s];
                        if slot == 0 {
                            b.table[s] = e;
                        } else if slot != AMBIGUOUS && slot.count_ones() == w {
                            b.table[s] = AMBIGUOUS;
                        }
                    } else {
                        exact(b, pos + 1, left - 1, e, w);
                    }
                }
            }
            exact(self, 0, w, 0, w as u32);
        }
    }

    /// Decodes the word at `base` of `self.bits` in place: (valid, corrected bits).
    fn word(&mut self, bytes: &[u8], base: usize) -> (bool, u32) {
        let c = read_bits(bytes, base, self.n);
        let s = self.syndrome(c);
        let mut e = 0u64;
        if s != 0 {
            match self.table[s as usize] {
                0 | AMBIGUOUS => return (false, 0),
                pat => e = pat,
            }
        }
        let mut corrected = e.count_ones();
        if self.parity != Parity::None {
            let ones = (c ^ e).count_ones() + u32::from(self.bits[base + self.n]);
            if (ones % 2 == 1) != (self.parity == Parity::Odd) {
                if (corrected as usize) < self.t {
                    self.bits[base + self.n] ^= 1;
                    corrected += 1;
                } else {
                    return (false, 0);
                }
            }
        }
        let mut m = e;
        while m != 0 {
            let i = m.trailing_zeros() as usize; // bit i of c = position n−1−i
            self.bits[base + self.n - 1 - i] ^= 1;
            m &= m - 1;
        }
        (true, corrected)
    }
}

impl Block for Bch {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = one_input("bch", inputs, &[PortType::Frames])?;
        Ok(vec![frames_port(input, input.max_items)])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let (_, frames, buf) = frames_io(io)?;
        let before = buf.len();
        for f in frames.iter() {
            self.bits.clear();
            extend_bits(&mut self.bits, f.bytes, 0, f.info.bit_len as usize);
            let words = self.bits.len() / self.word_bits;
            let (mut all, mut corrected) = (words > 0, 0);
            for w in 0..words {
                let (ok, c) = self.word(f.bytes, w * self.word_bits);
                self.meter.push(!ok);
                match (ok, c) {
                    (false, _) => self.words_bad += 1,
                    (true, 0) => self.words_ok += 1,
                    (true, _) => self.words_corrected += 1,
                }
                all &= ok;
                corrected += c;
            }
            self.corrected_bits += u64::from(corrected);
            if !all && self.drop_invalid {
                continue;
            }
            let mut info = f.info.clone();
            info.check = combine(info.check, all);
            info.corrected_bits += corrected;
            if corrected > 0 {
                info.layers = None;
            }
            buf.push_bits(&self.bits, info);
        }
        let s = &mut self.status;
        s.items_in += frames.len() as u64;
        s.items_out += (buf.len() - before) as u64;
        s.error_rate = self.meter.rate();
        s.quality = self.meter.rate().map(|r| 1.0 - r);
        s.extra.set("words_ok", self.words_ok as f64);
        s.extra.set("words_corrected", self.words_corrected as f64);
        s.extra.set("words_bad", self.words_bad as f64);
        s.extra.set("corrected_bits", self.corrected_bits as f64);
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
