//! `mix`, `lowpass`, `resample`: adapters over hk-dsp's NCO, Kaiser design and DDC stages.

use hk_demod::dsp::FirDecimator;
use hk_dsp::filter::Nco;
use hk_dsp::{DdcKernel, DdcSpec, LowpassSpec, design_lowpass};
use hk_recipe::{Params, PortType};
use num_complex::Complex32;

use super::common::*;
use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::buffer::PortSlice;
use crate::registry::BuildCtx;
use crate::status::Status;

const TWO_POW_64: f64 = 18_446_744_073_709_551_616.0;

// ---------------------------------------------------------------------------------------- mix

pub(crate) fn build_mix(p: &Params, _: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    Ok(Box::new(Mix {
        offset_hz: require_f64(p, "offset_hz")?,
        params: p.clone(),
        fs: 0.0,
        inc: 0,
        phase: 0,
        status: Status::default(),
    }))
}

/// Mixes by `e^{−j2π·offset·t}` with a `u64` phase accumulator (continuous across hot
/// frequency changes, exact across chunking).
struct Mix {
    params: Params,
    offset_hz: f64,
    fs: f64,
    inc: u64,
    phase: u64,
    status: Status,
}

impl Mix {
    fn plan(&mut self) -> Result<(), BlockError> {
        if self.fs > 0.0 && self.offset_hz.abs() > self.fs / 2.0 {
            return Err(BlockError::Unrealisable(
                "mix offset beyond the input Nyquist band".into(),
            ));
        }
        if self.fs > 0.0 {
            self.inc = Nco::new(self.offset_hz / self.fs).increment();
        }
        self.status.extra.set("offset_hz", self.offset_hz);
        Ok(())
    }
}

impl Block for Mix {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = single_input(inputs, "mix", &[PortType::Iq])?;
        self.fs = input.rate_hz;
        self.plan()?;
        Ok(vec![PortInfo {
            hold_items: 0,
            ..input
        }])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let x = iq_in(&input)?;
        if restarts(input.meta.flags) {
            self.phase = 0;
        }
        let out = io.output(0)?;
        set_meta(
            out,
            &input.meta,
            input.meta.source_index,
            input.meta.source_per_item,
        );
        let y = iq_out(out)?;
        for &s in x {
            let turns = self.phase as f64 / TWO_POW_64;
            let (sn, c) = (-std::f64::consts::TAU * turns).sin_cos();
            y.push(s * Complex32::new(c as f32, sn as f32));
            self.phase = self.phase.wrapping_add(self.inc);
        }
        self.status.items_in += x.len() as u64;
        self.status.items_out += x.len() as u64;
        Ok(())
    }

    fn reset(&mut self) {
        self.phase = 0;
    }

    fn update_params(&mut self, p: &Params, _: &BuildCtx<'_>) -> Result<ParamUpdate, BlockError> {
        if !cold_equal(&["offset_hz"], &self.params, p) {
            return Ok(ParamUpdate::Rebuild);
        }
        let old = self.offset_hz;
        self.offset_hz = require_f64(p, "offset_hz")?;
        if let Err(e) = self.plan() {
            self.offset_hz = old;
            self.plan()?;
            return Err(e);
        }
        self.params = p.clone();
        Ok(ParamUpdate::Applied)
    }

    fn status(&self) -> Status {
        self.status
    }
}

// ------------------------------------------------------------------------------------ lowpass

pub(crate) fn build_lowpass(p: &Params, _: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let cutoff = require_f64(p, "cutoff_hz")?;
    Ok(Box::new(Lowpass {
        cutoff,
        transition: get_f64(p, "transition_hz").unwrap_or(cutoff / 4.0),
        stopband_db: f64_or(p, "stopband_db", 60.0),
        params: p.clone(),
        fir: None,
        delay: 0.0,
        status: Status::default(),
    }))
}

enum Fir {
    Real(FirDecimator<f32>),
    Iq(FirDecimator<Complex32>),
}

/// Same-rate Kaiser low-pass (hk-dsp design, hk-demod streaming FIR).
struct Lowpass {
    params: Params,
    cutoff: f64,
    transition: f64,
    stopband_db: f64,
    fir: Option<(Fir, Vec<f32>)>,
    delay: f64,
    status: Status,
}

impl Block for Lowpass {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = single_input(inputs, "lowpass", &[PortType::Iq, PortType::Real])?;
        let fs = input.rate_hz;
        let stop = self.cutoff + self.transition;
        if stop > fs / 2.0 {
            return Err(BlockError::Unrealisable(
                "lowpass cutoff + transition beyond the input Nyquist frequency".into(),
            ));
        }
        let design = design_lowpass(LowpassSpec::new(fs, self.cutoff, stop, self.stopband_db))
            .map_err(|e| BlockError::Unrealisable(e.to_string()))?;
        let taps = design.taps;
        self.delay = (taps.len() as f64 - 1.0) / 2.0;
        let fir = match input.ty {
            PortType::Iq => Fir::Iq(FirDecimator::new(taps.clone(), 1)),
            _ => Fir::Real(FirDecimator::new(taps.clone(), 1)),
        };
        let hold = taps.len();
        self.fir = Some((fir, taps));
        self.status.extra.set("taps", hold as f64);
        Ok(vec![PortInfo {
            hold_items: hold,
            ..input
        }])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        if restarts(input.meta.flags) {
            self.reset();
        }
        let Some((fir, _)) = &mut self.fir else {
            return Err(BlockError::Ports("lowpass not initialised".into()));
        };
        let out = io.output(0)?;
        let m = input.meta;
        set_meta(
            out,
            &m,
            m.source_index - self.delay * m.source_per_item,
            m.source_per_item,
        );
        match (input.data, fir) {
            (PortSlice::Iq(x), Fir::Iq(f)) => {
                let y = iq_out(out)?;
                y.extend(x.iter().filter_map(|&s| f.push(s)));
            }
            (PortSlice::Real(x), Fir::Real(f)) => {
                let y = real_out(out)?;
                y.extend(x.iter().filter_map(|&s| f.push(s)));
            }
            (d, _) => return Err(mismatch(0, PortType::Iq, d.port_type())),
        }
        let n = input.data.len() as u64;
        self.status.items_in += n;
        self.status.items_out += n;
        Ok(())
    }

    fn reset(&mut self) {
        if let Some((fir, taps)) = &mut self.fir {
            *fir = match fir {
                Fir::Iq(_) => Fir::Iq(FirDecimator::new(taps.clone(), 1)),
                Fir::Real(_) => Fir::Real(FirDecimator::new(taps.clone(), 1)),
            };
        }
    }

    fn update_params(&mut self, p: &Params, _: &BuildCtx<'_>) -> Result<ParamUpdate, BlockError> {
        Ok(if cold_equal(&[], &self.params, p) {
            ParamUpdate::Applied
        } else {
            ParamUpdate::Rebuild
        })
    }

    fn status(&self) -> Status {
        self.status
    }
}

// ----------------------------------------------------------------------------------- resample

pub(crate) fn build_resample(p: &Params, _: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let out = require_f64(p, "output_rate_hz")?;
    Ok(Box::new(Resample {
        rate: Rate::new(
            out,
            get_f64(p, "bandwidth_hz").unwrap_or(0.8 * out),
            f64_or(p, "stopband_db", 60.0),
        ),
        params: p.clone(),
        status: Status::default(),
    }))
}

/// A DDC kernel at centre 0 plus the scratch and time map shared by `resample`, `fm_demod`
/// and `subcarrier`: filters complex samples (real inputs as `re`) and stamps the output time.
pub(crate) struct Rate {
    pub out_rate: f64,
    pub bandwidth: f64,
    pub stopband_db: f64,
    pub kernel: Option<DdcKernel>,
    pub scratch_in: Vec<Complex32>,
    pub scratch_out: Vec<Complex32>,
    /// Input item index of the kernel's position 0.
    pub start_item: u64,
    pub in_rate: f64,
}

impl Rate {
    pub(crate) fn new(out_rate: f64, bandwidth: f64, stopband_db: f64) -> Self {
        Self {
            out_rate,
            bandwidth,
            stopband_db,
            kernel: None,
            scratch_in: Vec::new(),
            scratch_out: Vec::new(),
            start_item: 0,
            in_rate: 0.0,
        }
    }

    /// Plans for `input` (centre offset `center_hz`; `max_stop_hz` caps the stopband edge
    /// below the default output Nyquist, e.g. to reject a mixing image); returns (max outputs,
    /// hold items).
    pub(crate) fn init(
        &mut self,
        input: &PortInfo,
        center_hz: f64,
        max_stop_hz: Option<f64>,
    ) -> Result<(usize, usize), BlockError> {
        if self.out_rate > input.rate_hz * (1.0 + 1e-9) {
            return Err(BlockError::Unrealisable(
                "output rate above the input rate (resampling only reduces the rate)".into(),
            ));
        }
        let out_rate = self.out_rate.min(input.rate_hz);
        let mut spec = DdcSpec::new(center_hz, self.bandwidth)
            .with_output_rate(out_rate)
            .with_stopband_db(self.stopband_db);
        let fp = self.bandwidth / 2.0;
        if let Some(stop) = max_stop_hz.filter(|s| *s < out_rate / 2.0 && *s > fp * 1.05) {
            spec = spec.with_transition(stop - fp);
        }
        let kernel = DdcKernel::new(&spec, input.rate_hz)
            .map_err(|e| BlockError::Unrealisable(e.to_string()))?;
        let max_out = kernel.max_outputs(input.max_items);
        let hold = kernel.span_samples() * max_out.max(1) / input.max_items.max(1) + 1;
        self.scratch_in = Vec::with_capacity(input.max_items);
        self.scratch_out = Vec::with_capacity(max_out);
        self.kernel = Some(kernel);
        self.in_rate = input.rate_hz;
        Ok((max_out, hold))
    }

    /// Restarts the filters at input item `item`.
    pub(crate) fn restart(&mut self, item: u64) {
        if let Some(k) = &mut self.kernel {
            k.reset();
        }
        self.start_item = item;
    }

    /// Source index of the next output, from the input chunk's time map.
    pub(crate) fn next_source(&self, meta: &crate::buffer::ChunkMeta) -> f64 {
        let pos = self
            .kernel
            .as_ref()
            .map_or(0.0, DdcKernel::next_input_position);
        source_at(meta, self.start_item as f64 + pos)
    }

    /// Source samples per output item.
    pub(crate) fn per_item(&self, meta: &crate::buffer::ChunkMeta) -> f64 {
        meta.source_per_item * self.in_rate / self.out_rate
    }

    /// Filters `scratch_in` into `scratch_out` (cleared first).
    pub(crate) fn run(&mut self) {
        self.scratch_out.clear();
        if let Some(k) = &mut self.kernel {
            k.process(&self.scratch_in, &mut self.scratch_out);
        }
    }
}

/// Anti-aliased rate reduction through the DDC stages.
struct Resample {
    params: Params,
    rate: Rate,
    status: Status,
}

impl Block for Resample {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = single_input(inputs, "resample", &[PortType::Iq, PortType::Real])?;
        let (max_items, hold_items) = self.rate.init(&input, 0.0, None)?;
        if let Some(k) = &self.rate.kernel {
            self.status.extra.set("decimation", k.plan().decimation());
        }
        Ok(vec![PortInfo {
            ty: input.ty,
            rate_hz: self.rate.out_rate,
            max_items,
            hold_items,
        }])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let m = input.meta;
        if restarts(m.flags) {
            self.rate.restart(m.index);
        }
        let first = self.rate.next_source(&m);
        let per = self.rate.per_item(&m);
        match input.data {
            PortSlice::Iq(x) => {
                self.rate.scratch_in.clear();
                self.rate.scratch_in.extend_from_slice(x);
            }
            PortSlice::Real(x) => real_to_complex(x, &mut self.rate.scratch_in),
            d => return Err(mismatch(0, PortType::Iq, d.port_type())),
        }
        self.rate.run();
        let out = io.output(0)?;
        set_meta(out, &m, first, per);
        match input.data {
            PortSlice::Iq(_) => iq_out(out)?.extend_from_slice(&self.rate.scratch_out),
            _ => real_out(out)?.extend(self.rate.scratch_out.iter().map(|z| z.re)),
        }
        self.status.items_in += input.data.len() as u64;
        self.status.items_out += self.rate.scratch_out.len() as u64;
        Ok(())
    }

    fn reset(&mut self) {
        self.rate.restart(0);
    }

    fn update_params(&mut self, p: &Params, _: &BuildCtx<'_>) -> Result<ParamUpdate, BlockError> {
        Ok(if cold_equal(&[], &self.params, p) {
            ParamUpdate::Applied
        } else {
            ParamUpdate::Rebuild
        })
    }

    fn status(&self) -> Status {
        self.status
    }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use crate::block::PortInfo;
    use crate::buffer::PortVec;
    use hk_recipe::PortType;
    use num_complex::Complex32;
    use serde_json::json;
    use std::f64::consts::TAU;

    fn tone(fs: f64, f: f64, n: usize) -> Vec<Complex32> {
        (0..n)
            .map(|i| {
                let ph = TAU * f * i as f64 / fs;
                Complex32::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect()
    }

    fn power(x: &[Complex32]) -> f64 {
        x.iter().map(|z| f64::from(z.norm_sqr())).sum::<f64>() / x.len() as f64
    }

    #[test]
    fn mix_moves_the_offset_to_dc_and_is_chunk_invariant() {
        let fs = 48_000.0;
        let x = PortVec::Iq(tone(fs, 5_000.0, 10_000));
        let c = assert_chunk_invariant(
            || vec![build("mix", json!({"offset_hz": 5000}), PortType::Iq)],
            PortType::Iq,
            fs,
            &x,
            &[4096, 777, 1],
        );
        let y = &c.out(0, 0).iq;
        assert!(
            y.iter()
                .all(|z| (z.re - 1.0).abs() < 1e-4 && z.im.abs() < 1e-4)
        );
    }

    #[test]
    fn mix_offset_is_hot() {
        let mut b = build("mix", json!({"offset_hz": 100}), PortType::Iq);
        assert_eq!(
            update(b.as_mut(), json!({"offset_hz": 200}), PortType::Iq),
            crate::ParamUpdate::Applied
        );
        assert_eq!(b.status().extra.iter().next(), Some(("offset_hz", 200.0)));
    }

    #[test]
    fn lowpass_passes_in_band_and_rejects_out_of_band_real_and_iq() {
        let fs = 48_000.0;
        let make = || {
            vec![build(
                "lowpass",
                json!({"cutoff_hz": 3000, "transition_hz": 1000}),
                PortType::Iq,
            )]
        };
        let pass = PortVec::Iq(tone(fs, 2_000.0, 8_000));
        let c = assert_chunk_invariant(make, PortType::Iq, fs, &pass, &[1024, 333]);
        assert!((power(&c.out(0, 0).iq[1000..]) - 1.0).abs() < 0.02);
        let stop = PortVec::Iq(tone(fs, 6_000.0, 8_000));
        let c = assert_chunk_invariant(make, PortType::Iq, fs, &stop, &[1024]);
        assert!(power(&c.out(0, 0).iq[1000..]) < 1e-5);
        // Real input keeps its type.
        let re = PortVec::Real(tone(fs, 6_000.0, 8_000).iter().map(|z| z.re).collect());
        let info = PortInfo {
            ty: PortType::Real,
            rate_hz: fs,
            max_items: 512,
            hold_items: 0,
        };
        let mut ch = Chain::new(make(), info);
        ch.run(&re, 512);
        let y = &ch.out(0, 0).real;
        assert_eq!(y.len(), 8_000);
        assert!(y[1000..].iter().all(|v| v.abs() < 3e-3));
    }

    #[test]
    fn resample_changes_rate_keeps_tone_and_time_map() {
        let fs = 240_000.0;
        let x = PortVec::Iq(tone(fs, 1_000.0, 48_000));
        let c = assert_chunk_invariant(
            || {
                vec![build(
                    "resample",
                    json!({"output_rate_hz": 9500}),
                    PortType::Iq,
                )]
            },
            PortType::Iq,
            fs,
            &x,
            &[8192, 1001],
        );
        let out = c.out(0, 0);
        let expected = 48_000.0 * 9_500.0 / fs;
        assert!(
            (out.iq.len() as f64 - expected).abs() < 40.0,
            "{}",
            out.iq.len()
        );
        assert!((power(&out.iq[200..]) - 1.0).abs() < 0.02);
        // Time map: the phase of the output matches the tone at its source index.
        let m = out.metas[0];
        for k in [300usize, 1000, 1500] {
            let src = m.source_index + k as f64 * m.source_per_item;
            let want = TAU * 1_000.0 * src / fs;
            let got = f64::from(out.iq[k].arg());
            let d = (got - want).rem_euclid(TAU);
            assert!(d.min(TAU - d) < 0.05, "k {k}: {got} vs {want}");
        }
    }

    #[test]
    fn resample_refuses_upsampling() {
        let mut b = build("resample", json!({"output_rate_hz": 96000}), PortType::Real);
        let info = PortInfo {
            ty: PortType::Real,
            rate_hz: 48_000.0,
            max_items: 64,
            hold_items: 0,
        };
        assert!(b.init(&[info]).is_err());
    }
}
