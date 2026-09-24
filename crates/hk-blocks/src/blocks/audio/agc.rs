//! `agc`: the Listen path's peak-envelope AGC (`hk_demod::audio`, C19 "AGC") as a real → real
//! block, at the block's own input rate, plus an optional hang (C19: 0.5–2 s for SSB).

use hk_recipe::{Params, PortType};

use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::blocks::iq::common::*;
use crate::buffer::ChunkFlags;
use crate::registry::BuildCtx;
use crate::status::Status;

pub(crate) fn build(p: &Params, _: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let mut a = Agc {
        cfg: Cfg::from(p),
        fs: 0.0,
        attack: 1.0,
        decay: 1.0,
        target: 1.0,
        max_gain: 1.0,
        hang_items: 0,
        env: 0.0,
        gain: 1.0,
        since_peak: 0,
        non_finite: 0,
        status: Status::default(),
    };
    a.derive();
    Ok(Box::new(a))
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Cfg {
    enabled: bool,
    target_dbfs: f64,
    max_gain_db: f64,
    attack_s: f64,
    decay_s: f64,
    hang_s: f64,
}

impl Cfg {
    fn from(p: &Params) -> Self {
        Self {
            enabled: bool_or(p, "enabled", true),
            target_dbfs: f64_or(p, "target_dbfs", -6.0),
            max_gain_db: f64_or(p, "max_gain_db", 60.0),
            attack_s: f64_or(p, "attack_s", 0.002),
            decay_s: f64_or(p, "decay_s", 0.5),
            hang_s: f64_or(p, "hang_s", 0.0),
        }
    }
}

/// `1 − e^(−1/(τ·fs))`, the Listen path's one-pole coefficient.
fn coefficient(tau_s: f64, fs: f64) -> f32 {
    if fs > 0.0 {
        (1.0 - (-1.0 / (tau_s * fs)).exp()) as f32
    } else {
        1.0
    }
}

struct Agc {
    cfg: Cfg,
    fs: f64,
    attack: f32,
    decay: f32,
    target: f32,
    max_gain: f32,
    hang_items: u64,
    env: f32,
    gain: f32,
    since_peak: u64,
    non_finite: u64,
    status: Status,
}

impl Agc {
    fn derive(&mut self) {
        self.attack = coefficient(self.cfg.attack_s, self.fs);
        self.decay = coefficient(self.cfg.decay_s, self.fs);
        self.target = 10f32.powf(self.cfg.target_dbfs as f32 / 20.0);
        self.max_gain = 10f32.powf(self.cfg.max_gain_db as f32 / 20.0);
        self.hang_items = (self.cfg.hang_s * self.fs).round() as u64;
    }

    fn restart(&mut self) {
        self.env = 0.0;
        self.gain = 1.0;
        self.since_peak = 0;
    }
}

impl Block for Agc {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = single_input(inputs, "agc", &[PortType::Real])?;
        self.fs = input.rate_hz;
        self.derive();
        self.restart();
        Ok(vec![PortInfo {
            hold_items: 0,
            ..input
        }])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let x = real_in(&input)?;
        let m = input.meta;
        // The gain is a level, not signal history: a squelch gap (`DISCONTINUITY`) keeps it, so
        // the re-opened channel is not blasted at `max_gain` until the attack catches up. A hot
        // edit's `RESET` starts over.
        if m.flags.contains(ChunkFlags::RESET) {
            self.restart();
        }
        let out = io.output(0)?;
        set_meta(out, &m, m.source_index, m.source_per_item);
        let y = real_out(out)?;
        for &s in x {
            let s = finite_or_zero(s, &mut self.non_finite);
            if !self.cfg.enabled {
                y.push(s);
                continue;
            }
            let a = s.abs();
            if a > self.env {
                self.env += self.attack * (a - self.env);
                self.since_peak = 0;
            } else if self.since_peak >= self.hang_items {
                self.env += self.decay * (a - self.env);
            } else {
                self.since_peak += 1;
            }
            self.gain = (self.target / self.env.max(1e-9)).min(self.max_gain);
            y.push((s * self.gain).clamp(-1.0, 1.0));
        }
        let st = &mut self.status;
        st.items_in += x.len() as u64;
        st.items_out += x.len() as u64;
        let gain_db = if self.cfg.enabled {
            20.0 * f64::from(self.gain.max(1e-9)).log10()
        } else {
            0.0
        };
        st.extra.set("gain_db", gain_db);
        st.extra.set(
            "envelope_dbfs",
            20.0 * f64::from(self.env.max(1e-9)).log10(),
        );
        report_non_finite(st, self.non_finite);
        Ok(())
    }

    fn reset(&mut self) {
        self.restart();
    }

    fn update_params(&mut self, p: &Params, _: &BuildCtx<'_>) -> Result<ParamUpdate, BlockError> {
        // Every parameter is hot; the envelope carries over.
        self.cfg = Cfg::from(p);
        self.derive();
        Ok(ParamUpdate::Applied)
    }

    fn status(&self) -> Status {
        self.status
    }
}
