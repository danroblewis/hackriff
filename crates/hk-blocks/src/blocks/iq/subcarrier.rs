//! `subcarrier`: real waveform → complex baseband at a subcarrier, optionally phase-locked to
//! a pilot (hk-demod `PilotPll`) with BPSK/QPSK residual-phase tracking (the `rds::demod`
//! squared-baseband estimator).

use std::f64::consts::PI;

use hk_demod::pilot::{PilotConfig, PilotPll};
use hk_recipe::{Params, PortType};
use num_complex::{Complex32, Complex64};
use serde_json::Value;

use super::common::*;
use super::filter::Rate;
use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::evidence::calibrated;
use crate::registry::BuildCtx;
use crate::status::{Lock, Status};
use hk_model::synth::{EvidenceSet, GroupId, MetricId, Stage};

/// Residual-phase averaging time constant, s.
const PHASE_TAU_S: f64 = 0.25;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Reference {
    pilot_hz: f64,
    multiple: u32,
    pll_bandwidth_hz: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tracking {
    None,
    Bpsk,
    Qpsk,
}

fn tracking(p: &Params) -> Tracking {
    match str_or(p, "phase_tracking", "none") {
        "bpsk" => Tracking::Bpsk,
        "qpsk" => Tracking::Qpsk,
        _ => Tracking::None,
    }
}

pub(crate) fn build(p: &Params, _: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let carrier = require_f64(p, "carrier_hz")?;
    let bandwidth = require_f64(p, "bandwidth_hz")?;
    let out_rate = require_f64(p, "output_rate_hz")?;
    let reference = match p.get("reference") {
        None => None,
        Some(Value::Object(r)) => {
            let pilot_hz = r.get("pilot_hz").and_then(Value::as_f64).unwrap_or(0.0);
            let multiple = r.get("multiple").and_then(Value::as_u64).unwrap_or(0) as u32;
            if pilot_hz <= 0.0
                || multiple == 0
                || (pilot_hz * f64::from(multiple) - carrier).abs() > 1e-6 * carrier
            {
                return Err(BlockError::Params(
                    "reference: carrier_hz must equal multiple × pilot_hz".into(),
                ));
            }
            Some(Reference {
                pilot_hz,
                multiple,
                pll_bandwidth_hz: r
                    .get("pll_bandwidth_hz")
                    .and_then(Value::as_f64)
                    .unwrap_or(10.0),
            })
        }
        Some(_) => return Err(BlockError::Params("reference must be an object".into())),
    };
    Ok(Box::new(Subcarrier {
        params: p.clone(),
        carrier,
        reference,
        tracking: tracking(p),
        rate: Rate::new(out_rate, bandwidth, 60.0),
        fs: 0.0,
        pll: None,
        z2: Complex64::new(0.0, 0.0),
        power: 0.0,
        psi: 0.0,
        n_z: 0,
        alpha: 1.0,
        ev_m: Complex64::new(0.0, 0.0),
        ev_power: 0.0,
        ev_n: 0,
        non_finite: 0,
        status: Status::default(),
    }))
}

struct Subcarrier {
    params: Params,
    carrier: f64,
    reference: Option<Reference>,
    tracking: Tracking,
    rate: Rate,
    fs: f64,
    pll: Option<PilotPll>,
    z2: Complex64,
    power: f64,
    psi: f64,
    n_z: u64,
    alpha: f64,
    /// Evidence (T-853): Σ z^m, Σ |z|^m and the samples since `reset()` (phase tracking only).
    ev_m: Complex64,
    ev_power: f64,
    ev_n: u64,
    non_finite: u64,
    status: Status,
}

impl Subcarrier {
    fn new_pll(&self) -> Option<PilotPll> {
        self.reference.map(|r| {
            let config = PilotConfig {
                nominal_hz: r.pilot_hz,
                pull_range_hz: (r.pilot_hz * 0.0053).max(1.0),
                update_rate_hz: 4_000.0f64.min(self.fs / 4.0),
                loop_bandwidth_hz: r.pll_bandwidth_hz,
                min_deviation_hz: 0.0,
                ..PilotConfig::default()
            };
            PilotPll::new(config, self.fs)
        })
    }

    fn restart(&mut self, item: u64) {
        self.rate.restart(item);
        self.pll = self.new_pll();
        self.clear_phase();
    }

    fn clear_phase(&mut self) {
        self.z2 = Complex64::new(0.0, 0.0);
        self.power = 0.0;
        self.psi = 0.0;
        self.n_z = 0;
    }

    /// Removes the residual carrier phase (BPSK: from z², QPSK: from z⁴).
    #[inline]
    fn track(&mut self, z: Complex32) -> Complex32 {
        let order = match self.tracking {
            Tracking::None => return z,
            Tracking::Bpsk => 2,
            Tracking::Qpsk => 4,
        };
        let b = Complex64::new(f64::from(z.re), f64::from(z.im));
        let a = self.alpha.max(1.0 / (self.n_z + 1) as f64);
        self.n_z += 1;
        let zz = b * b;
        let m = if order == 2 { zz } else { zz * zz };
        let norm = b.norm_sqr();
        self.z2 += (m - self.z2) * a;
        self.ev_m += m;
        self.ev_power += if order == 2 { norm } else { norm * norm };
        self.ev_n += 1;
        self.power += (if order == 2 { norm } else { norm * norm } - self.power) * a;
        let step = 2.0 * PI / f64::from(order);
        let est = self.z2.im.atan2(self.z2.re) / f64::from(order);
        let mut d = est - self.psi;
        d -= step * (d / step).round();
        self.psi += d;
        let r = Complex64::from_polar(1.0, -self.psi);
        let y = b * r;
        Complex32::new(y.re as f32, y.im as f32)
    }
}

impl Block for Subcarrier {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = single_input(inputs, "subcarrier", &[PortType::Real])?;
        self.fs = input.rate_hz;
        if self.carrier + self.rate.bandwidth / 2.0 > self.fs / 2.0 {
            return Err(BlockError::Unrealisable(
                "subcarrier band beyond the input Nyquist frequency".into(),
            ));
        }
        let center = if self.reference.is_some() {
            0.0
        } else {
            self.carrier
        };
        // Mixing a real waveform down leaves its negative-frequency image at −2·carrier; keep
        // the stopband edge below it.
        let image_edge = 2.0 * self.carrier - self.rate.bandwidth / 2.0;
        let (max_items, hold_items) = self.rate.init(&input, center, Some(image_edge))?;
        self.alpha = ema_alpha(PHASE_TAU_S, self.rate.out_rate);
        self.restart(0);
        Ok(vec![PortInfo {
            ty: PortType::Iq,
            rate_hz: self.rate.out_rate,
            max_items,
            hold_items,
        }])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let x = real_in(&input)?;
        let m = input.meta;
        if restarts(m.flags) {
            self.restart(m.index);
        }
        let first = self.rate.next_source(&m);
        let per = self.rate.per_item(&m);
        // Bad samples are zeroed before the PLL and the phase averages see them.
        let bad = &mut self.non_finite;
        match (&mut self.pll, self.reference) {
            (Some(pll), Some(r)) => {
                self.rate.scratch_in.clear();
                let k = f64::from(r.multiple);
                for &s in x {
                    let s = finite_or_zero(s, bad);
                    let th = pll.step(s);
                    let (sn, c) = (-k * th).sin_cos();
                    self.rate
                        .scratch_in
                        .push(Complex32::new(s * c as f32, s * sn as f32));
                }
            }
            _ => {
                self.rate.scratch_in.clear();
                self.rate.scratch_in.extend(
                    x.iter()
                        .map(|&s| Complex32::new(finite_or_zero(s, bad), 0.0)),
                );
            }
        }
        self.rate.run();
        let out = io.output(0)?;
        set_meta(out, &m, first, per);
        let y = iq_out(out)?;
        let produced = self.rate.scratch_out.len();
        for i in 0..produced {
            let z = self.rate.scratch_out[i];
            let t = self.track(z);
            y.push(t);
        }
        self.status.items_in += x.len() as u64;
        self.status.items_out += produced as u64;
        if let Some(pll) = &self.pll {
            self.status.lock = if pll.is_locked() {
                Lock::Locked
            } else {
                Lock::Searching
            };
            self.status
                .extra
                .set("pilot_locked", f64::from(u8::from(pll.is_locked())));
        }
        if self.tracking != Tracking::None && self.power > 0.0 {
            self.status.quality = Some((self.z2.norm() / self.power).min(1.0) as f32);
            self.status.extra.set("phase_rad", self.psi);
        }
        report_non_finite(&mut self.status, self.non_finite);
        Ok(())
    }

    fn reset(&mut self) {
        self.restart(0);
        self.ev_m = Complex64::new(0.0, 0.0);
        self.ev_power = 0.0;
        self.ev_n = 0;
    }

    /// S1 `pilot_lock` (group `pilot`): the window's phase coherence `|Σ z^m| / Σ |z|^m` of the
    /// sub-carrier at the tracked order (BPSK m = 2, QPSK m = 4) — about `1/√n` for noise, 1 for
    /// a clean phase-modulated sub-carrier. Only with phase tracking on.
    fn evidence(&self, out: &mut EvidenceSet) {
        if self.ev_n > 0 && self.ev_power > 0.0 {
            let raw = self.ev_m.norm() / self.ev_power;
            calibrated(
                out,
                Stage::S1,
                MetricId::PilotLock,
                GroupId::Pilot,
                raw,
                self.ev_n,
            );
        }
    }

    fn update_params(&mut self, p: &Params, _: &BuildCtx<'_>) -> Result<ParamUpdate, BlockError> {
        if !cold_equal(&["phase_tracking"], &self.params, p) {
            return Ok(ParamUpdate::Rebuild);
        }
        let t = tracking(p);
        if t != self.tracking {
            self.tracking = t;
            self.clear_phase();
        }
        self.params = p.clone();
        Ok(ParamUpdate::Applied)
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
    use serde_json::json;
    use std::f64::consts::TAU;

    #[test]
    fn free_running_mix_extracts_a_tone_and_pilot_reference_fixes_phase() {
        let fs = 240_000.0;
        // Pilot 19 kHz + BPSK-ish constant symbol at 57 kHz in quadrature with 3θ.
        let x: Vec<f32> = (0..96_000)
            .map(|i| {
                let t = i as f64 / fs;
                let th = TAU * 19_000.0 * t + 0.4;
                (0.1 * th.cos() + 0.05 * (3.0 * th + std::f64::consts::FRAC_PI_2).cos()) as f32
            })
            .collect();
        let data = PortVec::Real(x);
        let c = assert_chunk_invariant(
            || {
                vec![build(
                    "subcarrier",
                    json!({"carrier_hz": 57000, "bandwidth_hz": 4800, "output_rate_hz": 9500,
                           "reference": {"pilot_hz": 19000, "multiple": 3},
                           "phase_tracking": "bpsk"}),
                    PortType::Real,
                )]
            },
            PortType::Real,
            fs,
            &data,
            &[8192, 1500],
        );
        let y = &c.out(0, 0).iq;
        // 96 000 × 9 500 / 240 000, less the filters' start-up.
        assert!(y.len() <= 3_800 && y.len() > 3_740, "{}", y.len());
        // After lock and tracking: all energy on the I axis, amplitude 0.025.
        for z in &y[2_000..] {
            assert!(
                (z.re.abs() - 0.025).abs() < 0.002 && z.im.abs() < 0.003,
                "{z}"
            );
        }
        let st = c.block(0).status();
        assert_eq!(st.lock, crate::status::Lock::Locked);
        assert!(st.quality.unwrap() > 0.95);

        // Free-running: magnitude only.
        let c = assert_chunk_invariant(
            || {
                vec![build(
                    "subcarrier",
                    json!({"carrier_hz": 57000, "bandwidth_hz": 4800, "output_rate_hz": 9500}),
                    PortType::Real,
                )]
            },
            PortType::Real,
            fs,
            &data,
            &[8192],
        );
        for z in &c.out(0, 0).iq[200..] {
            assert!((z.norm() - 0.025).abs() < 0.002, "{z}");
        }
    }

    #[test]
    fn reference_must_match_carrier_and_tracking_is_hot() {
        let reg = crate::Registry::builtin();
        let maps = std::collections::BTreeMap::new();
        let ctx = crate::BuildCtx {
            field_maps: &maps,
            input_types: &[PortType::Real],
        };
        let p = params(
            json!({"carrier_hz": 57000, "bandwidth_hz": 4800, "output_rate_hz": 9500,
                              "reference": {"pilot_hz": 19000, "multiple": 2}}),
        );
        assert!(reg.build("subcarrier", &p, &ctx).is_err());
        let mut b = build(
            "subcarrier",
            json!({"carrier_hz": 1800, "bandwidth_hz": 2400, "output_rate_hz": 12000}),
            PortType::Real,
        );
        assert_eq!(
            update(
                b.as_mut(),
                json!({"carrier_hz": 1800, "bandwidth_hz": 2400, "output_rate_hz": 12000, "phase_tracking": "qpsk"}),
                PortType::Real
            ),
            crate::ParamUpdate::Applied
        );
        assert_eq!(
            update(
                b.as_mut(),
                json!({"carrier_hz": 1900, "bandwidth_hz": 2400, "output_rate_hz": 12000}),
                PortType::Real
            ),
            crate::ParamUpdate::Rebuild
        );
    }
}
