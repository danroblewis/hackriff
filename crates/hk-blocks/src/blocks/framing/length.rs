//! Frame length (ADR-0011 §1.5, `schema::frame_length`): a frame ends at the first of its
//! `length_from` length, its `terminator` (plus trailer bits), or the maximum `frame_bits`.
//! Evaluated bit by bit as a frame is collected, so a frame is emitted as soon as its last bit
//! arrives. `pub` so every frame-producing block (e.g. `ppm_demod`) shares one evaluator.

use hk_recipe::Params;

use super::common::P;
use crate::block::BlockError;

#[derive(Clone, Debug, PartialEq)]
struct Case {
    min: u64,
    max: u64,
    frame_bits: u32,
}

#[derive(Clone, Debug, PartialEq)]
struct LengthFrom {
    offset_bits: usize,
    bits: usize,
    cases: Vec<Case>,
    scale: u64,
    add: i64,
    default_bits: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
struct Terminator {
    words: Vec<u64>,
    bits: usize,
    step_bits: usize,
    trailer_bits: u32,
}

/// Parsed frame-length rules.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameLength {
    max_bits: u32,
    length_from: Option<LengthFrom>,
    terminator: Option<Terminator>,
}

/// Per-frame evaluation state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LengthState {
    target: Option<u32>,
}

impl FrameLength {
    /// A fixed length.
    pub fn fixed(bits: u32) -> Self {
        Self {
            max_bits: bits,
            length_from: None,
            terminator: None,
        }
    }

    /// From a block's `length_from`/`terminator` params with `max_bits` (its `frame_bits`).
    pub fn from_params(params: &Params, max_bits: u32) -> Result<Self, BlockError> {
        let p = P(params);
        let perr = |m: &str| BlockError::Params(m.into());
        if max_bits == 0 {
            return Err(perr("frame_bits must be positive"));
        }
        let length_from = match p.obj("length_from") {
            None => None,
            Some(lf) => {
                let bits = lf.req_uint("bits")? as usize;
                let offset_bits = lf.req_uint("offset_bits")? as usize;
                if offset_bits + bits > max_bits as usize {
                    return Err(perr("length_from field ends past frame_bits"));
                }
                let cases = lf
                    .list("cases")
                    .iter()
                    .filter_map(|c| c.as_object().map(P))
                    .map(|c| {
                        Ok(Case {
                            min: u64::from(c.req_uint("min")?),
                            max: u64::from(c.req_uint("max")?),
                            frame_bits: c.req_uint("frame_bits")?,
                        })
                    })
                    .collect::<Result<Vec<_>, BlockError>>()?;
                Some(LengthFrom {
                    offset_bits,
                    bits,
                    cases,
                    scale: u64::from(lf.uint_or("scale", 0)?),
                    add: lf.int_or("add", 0),
                    default_bits: lf.uint("default_bits")?,
                })
            }
        };
        let terminator = match p.obj("terminator") {
            None => None,
            Some(t) => {
                let bits = t.req_uint("bits")? as usize;
                let words = t
                    .list("words")
                    .iter()
                    .map(|w| {
                        w.as_str()
                            .and_then(hk_recipe::parse_hex)
                            .filter(|&v| bits >= 64 || v >> bits == 0)
                            .ok_or_else(|| perr("terminator word wider than bits"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Some(Terminator {
                    words,
                    bits,
                    step_bits: t.uint_or("step_bits", 1)?.max(1) as usize,
                    trailer_bits: t.uint_or("trailer_bits", 0)?,
                })
            }
        };
        Ok(Self {
            max_bits,
            length_from,
            terminator,
        })
    }

    /// The maximum (or fixed) length, bits.
    pub fn max_bits(&self) -> u32 {
        self.max_bits
    }

    /// Whether every frame has `max_bits`.
    pub fn is_fixed(&self) -> bool {
        self.length_from.is_none() && self.terminator.is_none()
    }

    /// The shortest frame these rules can produce, bits (≥ 1).
    pub fn min_bits(&self) -> u32 {
        let mut m = self.max_bits;
        if let Some(lf) = &self.length_from {
            m = m.min((lf.offset_bits + lf.bits) as u32);
        }
        if let Some(t) = &self.terminator {
            m = m.min(t.bits as u32);
        }
        m.max(1)
    }

    /// Call after appending bit `bits.len()` of the frame (unpacked, 0/1, frame order, from the
    /// frame start). Returns whether the frame ends with this bit.
    pub fn after_bit(&self, st: &mut LengthState, bits: &[u8]) -> bool {
        let n = bits.len();
        let read = |start: usize, len: usize| {
            bits[start..start + len]
                .iter()
                .fold(0u64, |a, &b| (a << 1) | u64::from(b & 1))
        };
        if let Some(lf) = &self.length_from
            && n == lf.offset_bits + lf.bits
        {
            let v = read(lf.offset_bits, lf.bits);
            let t = lf
                .cases
                .iter()
                .find(|c| (c.min..=c.max).contains(&v))
                .map(|c| i64::from(c.frame_bits))
                .or_else(|| {
                    (lf.scale > 0)
                        .then(|| (v.saturating_mul(lf.scale) as i64).saturating_add(lf.add))
                })
                .or(lf.default_bits.map(i64::from))
                .unwrap_or(i64::from(self.max_bits));
            let t = t.clamp(n as i64, i64::from(self.max_bits)) as u32;
            st.target = Some(st.target.map_or(t, |x| x.min(t)));
        }
        if let Some(term) = &self.terminator
            && n >= term.bits
            && (n - term.bits) % term.step_bits == 0
            && term.words.contains(&read(n - term.bits, term.bits))
        {
            let t = (n as u32)
                .saturating_add(term.trailer_bits)
                .min(self.max_bits);
            st.target = Some(st.target.map_or(t, |x| x.min(t)));
        }
        n as u32 >= self.max_bits || st.target.is_some_and(|t| n as u32 >= t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rules(v: serde_json::Value, max: u32) -> FrameLength {
        FrameLength::from_params(v.as_object().unwrap(), max).unwrap()
    }

    fn end_of(r: &FrameLength, bits: &[u8]) -> usize {
        let mut st = LengthState::default();
        (1..=bits.len())
            .find(|&n| r.after_bit(&mut st, &bits[..n]))
            .unwrap_or(0)
    }

    fn bits_of(v: u64, n: usize) -> Vec<u8> {
        (0..n).rev().map(|k| ((v >> k) & 1) as u8).collect()
    }

    #[test]
    fn length_from_table_scale_and_default() {
        let adsb = rules(
            json!({"length_from": {"offset_bits": 0, "bits": 5,
                "cases": [{"min": 16, "max": 31, "frame_bits": 112}], "default_bits": 56}}),
            112,
        );
        let mut df17 = bits_of(17, 5);
        df17.resize(200, 0);
        assert_eq!(end_of(&adsb, &df17), 112);
        let mut df11 = bits_of(11, 5);
        df11.resize(200, 1);
        assert_eq!(end_of(&adsb, &df11), 56);
        // A length byte counting payload bytes after a 16-bit header, capped at the maximum.
        let scaled = rules(
            json!({"length_from": {"offset_bits": 8, "bits": 8, "scale": 8, "add": 16}}),
            64,
        );
        let mut f = bits_of(0xAA03, 16);
        f.resize(100, 0);
        assert_eq!(end_of(&scaled, &f), 40);
        let mut big = bits_of(0xAAFF, 16);
        big.resize(100, 0);
        assert_eq!(end_of(&scaled, &big), 64);
    }

    #[test]
    fn terminator_at_character_steps_with_trailer() {
        let acars = rules(
            json!({"terminator": {"words": ["0x83", "0x97"], "bits": 8, "step_bits": 8, "trailer_bits": 16}}),
            2048,
        );
        // 0x83 straddling a character boundary does not end the frame; aligned it does.
        let mut f = bits_of(0x0108_3F00, 32); // 0000 1000 | 0011 1111: 0x83 at bit 12, unaligned
        f.extend(bits_of(0x41_83, 16));
        f.extend(bits_of(0xBEEF, 16));
        f.extend(bits_of(0x5555, 16));
        assert_eq!(end_of(&acars, &f), 32 + 16 + 16);
        let unstepped = rules(json!({"terminator": {"words": ["0x3"], "bits": 2}}), 100);
        assert_eq!(end_of(&unstepped, &bits_of(0b0011, 4)), 4);
        assert_eq!(unstepped.min_bits(), 2);
        assert!(!unstepped.is_fixed() && FrameLength::fixed(8).is_fixed());
    }
}
