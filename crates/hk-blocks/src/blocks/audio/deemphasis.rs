//! `deemphasis`: `hk_demod::dsp::Deemphasis` (the single pole `fm_demod`'s `deemphasis_s` also
//! uses) as its own block, so an FM chain can de-emphasise after its noise squelch.

use hk_demod::dsp::Deemphasis;
use hk_recipe::{Params, PortType};

use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::blocks::iq::common::*;
use crate::registry::BuildCtx;
use crate::status::Status;

pub(crate) fn build(p: &Params, _: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let tau_s = require_f64(p, "tau_s")?;
    Ok(Box::new(De {
        tau_s,
        fs: 0.0,
        filter: Deemphasis::new(tau_s, 1.0),
        non_finite: 0,
        status: Status::default(),
    }))
}

struct De {
    tau_s: f64,
    fs: f64,
    filter: Deemphasis,
    non_finite: u64,
    status: Status,
}

impl Block for De {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = single_input(inputs, "deemphasis", &[PortType::Real])?;
        self.fs = input.rate_hz;
        self.filter = Deemphasis::new(self.tau_s, self.fs);
        Ok(vec![PortInfo {
            hold_items: 1,
            ..input
        }])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let x = real_in(&input)?;
        let m = input.meta;
        if restarts(m.flags) {
            self.filter = Deemphasis::new(self.tau_s, self.fs);
        }
        let out = io.output(0)?;
        set_meta(out, &m, m.source_index, m.source_per_item);
        let y = real_out(out)?;
        for &s in x {
            y.push(self.filter.push(finite_or_zero(s, &mut self.non_finite)));
        }
        self.status.items_in += x.len() as u64;
        self.status.items_out += x.len() as u64;
        self.status.extra.set("tau_s", self.tau_s);
        report_non_finite(&mut self.status, self.non_finite);
        Ok(())
    }

    fn reset(&mut self) {
        self.filter = Deemphasis::new(self.tau_s, self.fs);
    }

    fn update_params(&mut self, p: &Params, _: &BuildCtx<'_>) -> Result<ParamUpdate, BlockError> {
        // `tau_s` is hot: a new pole at the chunk boundary (a one-sample state restart).
        self.tau_s = require_f64(p, "tau_s")?;
        self.filter = Deemphasis::new(self.tau_s, self.fs);
        Ok(ParamUpdate::Applied)
    }

    fn status(&self) -> Status {
        self.status
    }
}
