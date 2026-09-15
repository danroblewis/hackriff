//! `fm_demod`, `am_demod`, `fsk_demod`, `msk_demod`: hk-demod's discriminator and de-emphasis
//! behind the block contract.

use hk_demod::dsp::{Deemphasis, Discriminator};
use hk_recipe::{Params, PortType};

use super::common::*;
use super::filter::Rate;
use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::registry::BuildCtx;
use crate::status::Status;

/// Time constant of the level/deviation estimators, s.
const ESTIMATE_TAU_S: f64 = 0.1;

/// A running mean and mean square with a fast start (`1/(n+1)` until the time constant).
#[derive(Clone, Copy, Debug, Default)]
struct Ema {
    alpha: f64,
    n: u64,
    value: f64,
}

impl Ema {
    fn new(alpha: f64) -> Self {
        Self {
            alpha,
            n: 0,
            value: 0.0,
        }
    }

    #[inline]
    fn push(&mut self, x: f64) -> f64 {
        let w = self.alpha.max(1.0 / (self.n + 1) as f64);
        self.n = self.n.saturating_add(1);
        self.value += w * (x - self.value);
        self.value
    }

    fn clear(&mut self) {
        self.n = 0;
        self.value = 0.0;
    }
}

fn applied_if(cold_same: bool) -> ParamUpdate {
    if cold_same {
        ParamUpdate::Applied
    } else {
        ParamUpdate::Rebuild
    }
}

// ----------------------------------------------------------------------------------- fm_demod

pub(crate) fn build_fm(p: &Params, _: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    Ok(Box::new(Fm {
        params: p.clone(),
        deviation: get_f64(p, "deviation_hz"),
        deemphasis_s: get_f64(p, "deemphasis_s").filter(|t| *t > 0.0),
        rate: get_f64(p, "output_rate_hz").map(|r| Rate::new(r, 0.8 * r, 60.0)),
        fs: 0.0,
        disc: Discriminator::new(1.0),
        de: None,
        mean: Ema::default(),
        ms: Ema::default(),
        status: Status::default(),
    }))
}

/// Discriminator (Hz) → optional de-emphasis → ÷ deviation → optional decimation.
struct Fm {
    params: Params,
    deviation: Option<f64>,
    deemphasis_s: Option<f64>,
    rate: Option<Rate>,
    fs: f64,
    disc: Discriminator,
    de: Option<Deemphasis>,
    mean: Ema,
    ms: Ema,
    status: Status,
}

impl Fm {
    fn restart(&mut self, item: u64) {
        self.disc = Discriminator::new(self.fs);
        self.de = self.deemphasis_s.map(|t| Deemphasis::new(t, self.fs));
        self.mean.clear();
        self.ms.clear();
        if let Some(r) = &mut self.rate {
            r.restart(item);
        }
    }

    /// Instantaneous frequency of one sample, scaled.
    #[inline]
    fn step(&mut self, s: num_complex::Complex32) -> f32 {
        let f = f64::from(self.disc.push(s));
        let mean = self.mean.push(f);
        let d = f - mean;
        let ms = self.ms.push(d * d);
        let f = match &mut self.de {
            Some(de) => f64::from(de.push(f as f32)),
            None => f,
        };
        let dev = self
            .deviation
            .unwrap_or_else(|| (2.0 * ms).sqrt())
            .max(1e-3);
        (f / dev) as f32
    }
}

impl Block for Fm {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = single_input(inputs, "fm_demod", &[PortType::Iq])?;
        self.fs = input.rate_hz;
        self.mean = Ema::new(ema_alpha(ESTIMATE_TAU_S, self.fs));
        self.ms = Ema::new(ema_alpha(ESTIMATE_TAU_S, self.fs));
        let real = PortInfo {
            ty: PortType::Real,
            ..input
        };
        let out = match &mut self.rate {
            Some(r) => {
                let (max_items, hold_items) = r.init(&real, 0.0, None)?;
                PortInfo {
                    ty: PortType::Real,
                    rate_hz: r.out_rate,
                    max_items,
                    hold_items,
                }
            }
            None => PortInfo {
                hold_items: 1,
                ..real
            },
        };
        self.restart(0);
        Ok(vec![out])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let x = iq_in(&input)?;
        let m = input.meta;
        if restarts(m.flags) {
            self.restart(m.index);
        }
        let out = io.output(0)?;
        if self.rate.is_some() {
            let mut rate = self.rate.take().expect("rate");
            let first = rate.next_source(&m);
            let per = rate.per_item(&m);
            rate.scratch_in.clear();
            for &s in x {
                let y = self.step(s);
                rate.scratch_in.push(num_complex::Complex32::new(y, 0.0));
            }
            rate.run();
            set_meta(out, &m, first, per);
            real_out(out)?.extend(rate.scratch_out.iter().map(|z| z.re));
            self.status.items_out += rate.scratch_out.len() as u64;
            self.rate = Some(rate);
        } else {
            set_meta(out, &m, m.source_index, m.source_per_item);
            let y = real_out(out)?;
            for &s in x {
                y.push(self.step(s));
            }
            self.status.items_out += x.len() as u64;
        }
        self.status.items_in += x.len() as u64;
        let dev = self
            .deviation
            .unwrap_or_else(|| (2.0 * self.ms.value).sqrt());
        self.status.extra.set("deviation_hz", dev);
        self.status.extra.set("offset_hz", self.mean.value);
        Ok(())
    }

    fn reset(&mut self) {
        self.restart(0);
    }

    fn update_params(&mut self, p: &Params, _: &BuildCtx<'_>) -> Result<ParamUpdate, BlockError> {
        let same = cold_equal(&["deviation_hz"], &self.params, p);
        if same {
            self.deviation = get_f64(p, "deviation_hz");
            self.params = p.clone();
        }
        Ok(applied_if(same))
    }

    fn status(&self) -> Status {
        self.status
    }
}

// ----------------------------------------------------------------------------------- am_demod

pub(crate) fn build_am(p: &Params, _: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let mut b = Am {
        params: p.clone(),
        normalized: true,
        tau_s: 0.05,
        fs: 0.0,
        level: Ema::default(),
        depth: Ema::default(),
        status: Status::default(),
    };
    b.apply(p);
    Ok(Box::new(b))
}

/// Envelope detector with carrier-level normalisation.
struct Am {
    params: Params,
    normalized: bool,
    tau_s: f64,
    fs: f64,
    level: Ema,
    depth: Ema,
    status: Status,
}

impl Am {
    fn apply(&mut self, p: &Params) {
        self.normalized = str_or(p, "mode", "normalized") == "normalized";
        self.tau_s = f64_or(p, "time_constant_s", 0.05);
        if self.fs > 0.0 {
            self.level.alpha = ema_alpha(self.tau_s, self.fs);
        }
    }
}

impl Block for Am {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = single_input(inputs, "am_demod", &[PortType::Iq])?;
        self.fs = input.rate_hz;
        self.level = Ema::new(ema_alpha(self.tau_s, self.fs));
        self.depth = Ema::new(ema_alpha(ESTIMATE_TAU_S, self.fs));
        Ok(vec![PortInfo {
            ty: PortType::Real,
            hold_items: 0,
            ..input
        }])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let x = iq_in(&input)?;
        let m = input.meta;
        if restarts(m.flags) {
            self.reset();
        }
        let out = io.output(0)?;
        set_meta(out, &m, m.source_index, m.source_per_item);
        let y = real_out(out)?;
        for &s in x {
            let a = f64::from(s.norm());
            let v = if self.normalized {
                let level = self.level.push(a);
                if level > 1e-20 { a / level - 1.0 } else { 0.0 }
            } else {
                a
            };
            if self.normalized {
                self.depth.push(v * v);
            }
            y.push(v as f32);
        }
        let n = x.len() as u64;
        self.status.items_in += n;
        self.status.items_out += n;
        self.status.extra.set("level", self.level.value);
        self.status
            .extra
            .set("depth", (2.0 * self.depth.value).sqrt());
        Ok(())
    }

    fn reset(&mut self) {
        self.level.clear();
        self.depth.clear();
    }

    fn update_params(&mut self, p: &Params, _: &BuildCtx<'_>) -> Result<ParamUpdate, BlockError> {
        let same = cold_equal(&["mode", "time_constant_s"], &self.params, p);
        if same {
            self.apply(p);
            self.params = p.clone();
        }
        Ok(applied_if(same))
    }

    fn status(&self) -> Status {
        self.status
    }
}

// -------------------------------------------------------------------------- fsk_demod/msk_demod

pub(crate) fn build_fsk(p: &Params, _: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    Ok(Box::new(Fsk::new(p, false)))
}

pub(crate) fn build_msk(p: &Params, _: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    Ok(Box::new(Fsk::new(p, true)))
}

/// Non-coherent FSK/MSK discriminator: frequency − offset − tracked mean, ÷ deviation.
struct Fsk {
    params: Params,
    msk: bool,
    deviation: Option<f64>,
    offset_hz: f64,
    tracking_s: f64,
    fs: f64,
    disc: Discriminator,
    track: Ema,
    ms: Ema,
    status: Status,
}

impl Fsk {
    fn new(p: &Params, msk: bool) -> Self {
        let mut b = Self {
            params: p.clone(),
            msk,
            deviation: None,
            offset_hz: 0.0,
            tracking_s: 0.0,
            fs: 0.0,
            disc: Discriminator::new(1.0),
            track: Ema::default(),
            ms: Ema::default(),
            status: Status::default(),
        };
        b.apply(p);
        b
    }

    fn apply(&mut self, p: &Params) {
        self.deviation = if self.msk {
            get_f64(p, "symbol_rate_bd").map(|r| r / 4.0)
        } else {
            get_f64(p, "deviation_hz")
        };
        self.offset_hz = f64_or(p, "offset_hz", 0.0);
        self.tracking_s = f64_or(p, "offset_tracking_s", 0.0);
        if self.fs > 0.0 {
            self.track.alpha = ema_alpha(self.tracking_s, self.fs);
        }
    }
}

impl Block for Fsk {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let name = if self.msk { "msk_demod" } else { "fsk_demod" };
        let input = single_input(inputs, name, &[PortType::Iq])?;
        self.fs = input.rate_hz;
        self.disc = Discriminator::new(self.fs);
        self.track = Ema::new(ema_alpha(self.tracking_s, self.fs));
        self.ms = Ema::new(ema_alpha(ESTIMATE_TAU_S, self.fs));
        Ok(vec![PortInfo {
            ty: PortType::Real,
            hold_items: 1,
            ..input
        }])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let x = iq_in(&input)?;
        let m = input.meta;
        if restarts(m.flags) {
            self.reset();
        }
        let out = io.output(0)?;
        set_meta(out, &m, m.source_index, m.source_per_item);
        let y = real_out(out)?;
        let tracking = self.tracking_s > 0.0;
        for &s in x {
            let mut f = f64::from(self.disc.push(s)) - self.offset_hz;
            if tracking {
                f -= self.track.push(f);
            }
            let ms = self.ms.push(f * f);
            let dev = self.deviation.unwrap_or_else(|| ms.sqrt()).max(1e-3);
            y.push((f / dev) as f32);
        }
        let n = x.len() as u64;
        self.status.items_in += n;
        self.status.items_out += n;
        let dev = self.deviation.unwrap_or_else(|| self.ms.value.sqrt());
        self.status.extra.set("deviation_hz", dev);
        self.status
            .extra
            .set("offset_hz", self.offset_hz + self.track.value);
        Ok(())
    }

    fn reset(&mut self) {
        self.disc = Discriminator::new(self.fs.max(1.0));
        self.track.clear();
        self.ms.clear();
    }

    fn update_params(&mut self, p: &Params, _: &BuildCtx<'_>) -> Result<ParamUpdate, BlockError> {
        let hot: &[&str] = if self.msk {
            &["symbol_rate_bd", "offset_hz", "offset_tracking_s"]
        } else {
            &["deviation_hz", "offset_hz", "offset_tracking_s"]
        };
        let same = cold_equal(hot, &self.params, p);
        if same {
            self.apply(p);
            self.params = p.clone();
        }
        Ok(applied_if(same))
    }

    fn status(&self) -> Status {
        self.status
    }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use crate::buffer::PortVec;
    use hk_recipe::PortType;
    use num_complex::Complex32;
    use serde_json::json;
    use std::f64::consts::TAU;

    fn fm(fs: f64, dev: f64, tone: f64, n: usize, noise: f64) -> Vec<Complex32> {
        let mut rng = Lcg::new(7);
        let mut ph = 0.3;
        (0..n)
            .map(|i| {
                ph += TAU * dev * (TAU * tone * i as f64 / fs).cos() / fs;
                Complex32::new(ph.cos() as f32, ph.sin() as f32) + rng.cnoise(noise)
            })
            .collect()
    }

    fn fit_amplitude(y: &[f32], rate: f64, tone: f64, t0: f64, per: f64) -> (f64, f64) {
        // Least-squares cos/sin fit at the tone; returns (amplitude, residual rms).
        let (mut c, mut s) = (0.0, 0.0);
        for (k, &v) in y.iter().enumerate() {
            let t = (t0 + k as f64 * per) / rate;
            c += f64::from(v) * (TAU * tone * t).cos();
            s += f64::from(v) * (TAU * tone * t).sin();
        }
        let n = y.len() as f64;
        let (a, b) = (2.0 * c / n, 2.0 * s / n);
        let res = y
            .iter()
            .enumerate()
            .map(|(k, &v)| {
                let t = (t0 + k as f64 * per) / rate;
                let e = f64::from(v) - a * (TAU * tone * t).cos() - b * (TAU * tone * t).sin();
                e * e
            })
            .sum::<f64>()
            / n;
        ((a * a + b * b).sqrt(), res.sqrt())
    }

    #[test]
    fn fm_tone_scaled_by_deviation_decimated_and_chunk_invariant() {
        let fs = 240_000.0;
        let x = PortVec::Iq(fm(fs, 50_000.0, 1_000.0, 96_000, 1e-3));
        let c = assert_chunk_invariant(
            || {
                vec![build(
                    "fm_demod",
                    json!({"deviation_hz": 75000, "output_rate_hz": 48000}),
                    PortType::Iq,
                )]
            },
            PortType::Iq,
            fs,
            &x,
            &[16_384, 999],
        );
        let out = c.out(0, 0);
        // 96 000 / 5, less the decimation filter's start-up.
        assert!(
            out.real.len() <= 19_200 && out.real.len() > 19_100,
            "{}",
            out.real.len()
        );
        let m = out.metas[0];
        let skip = 2_000;
        let (amp, res) = fit_amplitude(
            &out.real[skip..],
            fs,
            1_000.0,
            m.source_index + skip as f64 * m.source_per_item,
            m.source_per_item,
        );
        assert!((amp - 50.0 / 75.0).abs() < 0.01, "amplitude {amp}");
        assert!(res < 0.02, "residual {res}");
    }

    #[test]
    fn fm_estimates_deviation_when_absent() {
        let fs = 240_000.0;
        let x = PortVec::Iq(fm(fs, 30_000.0, 1_000.0, 48_000, 0.0));
        let c = assert_chunk_invariant(
            || vec![build("fm_demod", json!({}), PortType::Iq)],
            PortType::Iq,
            fs,
            &x,
            &[4096],
        );
        let (amp, _) = fit_amplitude(&c.out(0, 0).real[24_000..], fs, 1_000.0, 24_000.0, 1.0);
        assert!((amp - 1.0).abs() < 0.03, "amplitude {amp}");
        let dev = c
            .block(0)
            .status()
            .extra
            .iter()
            .find(|e| e.0 == "deviation_hz")
            .unwrap()
            .1;
        assert!((dev - 30_000.0).abs() < 1_000.0, "{dev}");
    }

    #[test]
    fn am_normalized_recovers_modulation_depth() {
        let fs = 24_000.0;
        let mut rng = Lcg::new(3);
        let x: Vec<Complex32> = (0..48_000)
            .map(|i| {
                let t = i as f64 / fs;
                let a = 0.2 * (1.0 + 0.5 * (TAU * 800.0 * t).cos());
                let ph = TAU * 120.0 * t + 1.0;
                Complex32::new((a * ph.cos()) as f32, (a * ph.sin()) as f32) + rng.cnoise(1e-5)
            })
            .collect();
        let c = assert_chunk_invariant(
            || {
                vec![build(
                    "am_demod",
                    json!({"time_constant_s": 0.2}),
                    PortType::Iq,
                )]
            },
            PortType::Iq,
            fs,
            &PortVec::Iq(x),
            &[2048, 501],
        );
        let (amp, res) = fit_amplitude(&c.out(0, 0).real[24_000..], fs, 800.0, 24_000.0, 1.0);
        assert!((amp - 0.5).abs() < 0.02, "depth {amp}");
        assert!(res < 0.05, "residual {res}");
    }

    #[test]
    fn fsk_offset_tracking_and_hot_params() {
        let fs = 24_000.0;
        let mut rng = Lcg::new(11);
        let mut ph = 0.0;
        let x: Vec<Complex32> = (0..24_000)
            .map(|i| {
                let bit = (i / 20) % 3 == 0;
                let f = 700.0 + if bit { 4_500.0 } else { -4_500.0 };
                ph += TAU * f / fs;
                Complex32::new(ph.cos() as f32, ph.sin() as f32) + rng.cnoise(1e-4)
            })
            .collect();
        let make = || {
            vec![build(
                "fsk_demod",
                json!({"deviation_hz": 4500, "offset_hz": 700}),
                PortType::Iq,
            )]
        };
        let c = assert_chunk_invariant(make, PortType::Iq, fs, &PortVec::Iq(x), &[4096, 37]);
        let y = &c.out(0, 0).real;
        for i in (100..24_000).filter(|i| i % 20 > 2 && i % 20 < 18) {
            let want = if (i / 20) % 3 == 0 { 1.0 } else { -1.0 };
            assert!((y[i] - want).abs() < 0.05, "{i}: {}", y[i]);
        }
        let mut b = build("fsk_demod", json!({}), PortType::Iq);
        assert_eq!(
            update(b.as_mut(), json!({"offset_tracking_s": 1.0}), PortType::Iq),
            crate::ParamUpdate::Applied
        );
    }
}
