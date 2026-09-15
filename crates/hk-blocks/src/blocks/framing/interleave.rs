//! `interleave` / `deinterleave`: bit permutations within a frame. `depth`: a column
//! interleaver (bits written row-wise into rows of `depth`, read column by column; a partial
//! last row is ragged). `permutation`: output bit `i` of each period is input bit
//! `permutation[i]`; a trailing partial period is copied. `deinterleave` is the exact inverse.

use hk_recipe::{Params, PortType};

use super::common::{P, extend_bits, frames_io, frames_port, one_input, update_hot};
use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::registry::BuildCtx;
use crate::status::Status;

enum Scheme {
    Depth(usize),
    Perm(Vec<usize>),
}

fn build(params: &Params, inverse: bool) -> Result<Box<dyn Block>, BlockError> {
    let p = P(params);
    let perr = |m: &str| BlockError::Params(m.into());
    let scheme = match (p.uint("depth")?, p.get("permutation")) {
        (Some(d), None) => Scheme::Depth(d.max(1) as usize),
        (None, Some(_)) => {
            let perm: Vec<usize> = p
                .list("permutation")
                .iter()
                .map(|v| {
                    v.as_u64()
                        .map(|x| x as usize)
                        .ok_or_else(|| perr("permutation entry"))
                })
                .collect::<Result<_, _>>()?;
            let mut seen = vec![false; perm.len()];
            for &i in &perm {
                if i >= perm.len() || std::mem::replace(&mut seen[i], true) {
                    return Err(perr("permutation must list 0..len once each"));
                }
            }
            Scheme::Perm(perm)
        }
        _ => return Err(perr("exactly one of depth or permutation")),
    };
    Ok(Box::new(Interleave {
        params: params.clone(),
        scheme,
        inverse,
        a: Vec::new(),
        b: Vec::new(),
        status: Status::default(),
    }))
}

/// Builds an `interleave`.
pub(crate) fn build_interleave(
    params: &Params,
    _ctx: &BuildCtx<'_>,
) -> Result<Box<dyn Block>, BlockError> {
    build(params, false)
}

/// Builds a `deinterleave`.
pub(crate) fn build_deinterleave(
    params: &Params,
    _ctx: &BuildCtx<'_>,
) -> Result<Box<dyn Block>, BlockError> {
    build(params, true)
}

/// The block (both directions).
pub struct Interleave {
    params: Params,
    scheme: Scheme,
    inverse: bool,
    a: Vec<u8>,
    b: Vec<u8>,
    status: Status,
}

impl Interleave {
    fn permute(&mut self) {
        let n = self.a.len();
        self.b.clear();
        self.b.resize(n, 0);
        let (a, b, inv) = (&self.a, &mut self.b, self.inverse);
        match &self.scheme {
            Scheme::Depth(d) => {
                let mut j = 0;
                for c in 0..*d {
                    let mut p = c;
                    while p < n {
                        if inv {
                            b[p] = a[j];
                        } else {
                            b[j] = a[p];
                        }
                        j += 1;
                        p += d;
                    }
                }
            }
            Scheme::Perm(perm) => {
                let period = perm.len();
                let whole = n - n % period;
                for s in (0..whole).step_by(period) {
                    for (i, &q) in perm.iter().enumerate() {
                        if inv {
                            b[s + q] = a[s + i];
                        } else {
                            b[s + i] = a[s + q];
                        }
                    }
                }
                b[whole..].copy_from_slice(&a[whole..]);
            }
        }
    }
}

impl Block for Interleave {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = one_input("interleave", inputs, &[PortType::Frames])?;
        Ok(vec![frames_port(input, input.max_items)])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let (_, frames, buf) = frames_io(io)?;
        for f in frames.iter() {
            self.a.clear();
            extend_bits(&mut self.a, f.bytes, 0, f.info.bit_len as usize);
            self.permute();
            let mut info = f.info.clone();
            info.layers = None;
            buf.push_bits(&self.b, info);
        }
        self.status.items_in += frames.len() as u64;
        self.status.items_out += frames.len() as u64;
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
