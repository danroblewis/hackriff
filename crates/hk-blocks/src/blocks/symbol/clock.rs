//! `clock_recovery`: one soft value per symbol from a real waveform (or the I axis / envelope
//! of iq).
//!
//! **Matched statistic** at a symbol starting at input position `s` (sample `i` spans
//! `[i, i+1)`), from a running prefix sum with linear interpolation, so timing is continuous:
//! - `nrz`: the symbol's mean, `∫[s, s+T) / T`;
//! - `biphase`: `(∫first half − ∫second half) / T` (the `rds::demod` statistic);
//! - `rrc`: a root-raised-cosine matched filter (roll-off 0.35, 8 symbols) sampled at the
//!   symbol centre.
//!
//! **Timing:**
//! - `gardner`: `e = (y[k−1] − y[k]) · y[k−½] / power` (nrz, rrc). For `biphase`, whose
//!   statistic has no zero crossing between symbols, an early–late gate
//!   `e = (|y(s+T/4)| − |y(s−T/4)|) / level` is used instead.
//! - `mueller-muller`: `e = (y[k]·sgn y[k−1] − y[k−1]·sgn y[k]) / level`.
//!
//!   Both feed a second-order loop (normalised bandwidth `loop_bandwidth`, damping 0.707) whose
//!   integrator is the rate error, clamped to `max_deviation_ppm`.
//! - `max-contrast` (non-data-aided, the `rds::demod` method): 16 candidate phases are scored
//!   by mean |statistic| over 16-symbol windows, smoothed across windows with weight
//!   `20 × loop_bandwidth`; the parabolic peak is the phase. Symbols are emitted on the current
//!   phase and never closer than half a symbol, so a phase update neither repeats nor drops
//!   symbols. The nominal rate is used (drift shows as a slowly moving phase).
//!
//! Every decision depends only on the samples seen, never on chunk boundaries, so the output is
//! identical however the input is split.

use std::f64::consts::PI;

use hk_demod::dsp::FirDecimator;
use hk_recipe::{Params, PortType};

use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::blocks::iq::common::*;
use crate::buffer::PortSlice;
use crate::registry::BuildCtx;
use crate::status::{Lock, Status};

const HOT: &[&str] = &["loop_bandwidth"];
const CANDIDATES: usize = 16;
const WINDOW_SYMBOLS: usize = 16;
const RRC_ROLLOFF: f64 = 0.35;
const RRC_SPAN_SYMBOLS: f64 = 8.0;
/// Weight of the level/power and eye averages per symbol.
const LEVEL_ALPHA: f64 = 0.02;
const DAMPING: f64 = 0.707;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pulse {
    Nrz,
    Biphase,
    Rrc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Algo {
    Gardner,
    MuellerMuller,
    MaxContrast,
}

pub(crate) fn build(p: &Params, _: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let pulse = match str_or(p, "pulse", "nrz") {
        "biphase" => Pulse::Biphase,
        "rrc" => Pulse::Rrc,
        _ => Pulse::Nrz,
    };
    let algo = match str_or(p, "algorithm", "gardner") {
        "mueller-muller" => Algo::MuellerMuller,
        "max-contrast" => Algo::MaxContrast,
        _ => Algo::Gardner,
    };
    let mut b = Clock {
        params: p.clone(),
        rate_bd: require_f64(p, "symbol_rate_bd")?,
        pulse,
        algo,
        magnitude: str_or(p, "soft_from", "in-phase") == "magnitude",
        loop_bw: 0.01,
        max_dev: f64_or(p, "max_deviation_ppm", 500.0) * 1e-6,
        k1: 0.0,
        k2: 0.0,
        sized_k1: 0.0,
        min_step: 0.5,
        fs: 0.0,
        sps: 1.0,
        rrc_len: 0,
        rrc: None,
        rrc_delay: 0.0,
        acc: Vec::new(),
        acc_base: 0,
        samples: 0,
        origin: 0,
        next: 0.0,
        freq: 0.0,
        y_prev: 0.0,
        have_prev: false,
        level: 0.0,
        power: 0.0,
        symbols: 0,
        metrics: [0.0; CANDIDATES],
        have_metrics: false,
        win_start: 0.0,
        last_start: f64::NEG_INFINITY,
        tau: 0.0,
        contrast: 0.0,
        eye_abs: 0.0,
        eye_sq: 0.0,
        first: None,
        diag: Vec::new(),
        non_finite: 0,
        status: Status::default(),
    };
    b.set_loop(f64_or(p, "loop_bandwidth", 0.01));
    b.sized_k1 = b.k1;
    Ok(Box::new(b))
}

struct Clock {
    params: Params,
    rate_bd: f64,
    pulse: Pulse,
    algo: Algo,
    magnitude: bool,
    loop_bw: f64,
    max_dev: f64,
    k1: f64,
    k2: f64,
    /// `k1` the outputs were sized for: a hot change to a larger one is a rebuild.
    sized_k1: f64,
    /// Shortest loop step, as a fraction of the nominal period (the output bound's basis).
    min_step: f64,
    fs: f64,
    /// Samples per symbol.
    sps: f64,
    rrc_len: usize,
    rrc: Option<FirDecimator<f32>>,
    rrc_delay: f64,
    /// Integrating pulses: prefix sums (`acc[k]` = sum of samples `acc_base..acc_base+k`).
    /// RRC: matched-filter outputs (`acc[k]` = output at sample `acc_base + k`).
    acc: Vec<f64>,
    acc_base: u64,
    /// Samples appended since the restart.
    samples: u64,
    /// Input item index of sample position 0.
    origin: u64,
    // Loop timing (gardner, mueller-muller).
    next: f64,
    freq: f64,
    y_prev: f64,
    have_prev: bool,
    level: f64,
    power: f64,
    symbols: u64,
    // Max-contrast timing.
    metrics: [f64; CANDIDATES],
    have_metrics: bool,
    win_start: f64,
    last_start: f64,
    tau: f64,
    contrast: f64,
    // Readout.
    eye_abs: f64,
    eye_sq: f64,
    /// Position of the first symbol emitted in the current chunk.
    first: Option<f64>,
    diag: Vec<f32>,
    non_finite: u64,
    status: Status,
}

/// Loop gains `(k1, k2)` for `algo` at normalised bandwidth `bw`.
fn loop_gains(algo: Algo, bw: f64) -> (f64, f64) {
    // Detector gain after normalisation: ≈ 2 per symbol fraction for Gardner and the
    // early–late gate, ≈ 1 for Mueller–Müller.
    let kd = match algo {
        Algo::MuellerMuller => 1.0,
        _ => 2.0,
    };
    let theta = bw / (DAMPING + 1.0 / (4.0 * DAMPING));
    let d = 1.0 + 2.0 * DAMPING * theta + theta * theta;
    (4.0 * DAMPING * theta / d / kd, 4.0 * theta * theta / d / kd)
}

/// Root-raised-cosine impulse response at `t` symbols.
fn rrc(t: f64, a: f64) -> f64 {
    if t.abs() < 1e-9 {
        return 1.0 - a + 4.0 * a / PI;
    }
    if (t.abs() - 1.0 / (4.0 * a)).abs() < 1e-9 {
        let x = PI / (4.0 * a);
        return a / 2f64.sqrt() * ((1.0 + 2.0 / PI) * x.sin() + (1.0 - 2.0 / PI) * x.cos());
    }
    let num = (PI * t * (1.0 - a)).sin() + 4.0 * a * t * (PI * t * (1.0 + a)).cos();
    let den = PI * t * (1.0 - (4.0 * a * t).powi(2));
    num / den
}

impl Clock {
    fn set_loop(&mut self, bw: f64) {
        self.loop_bw = bw;
        (self.k1, self.k2) = loop_gains(self.algo, bw);
    }

    /// Shortest spacing of two emitted symbols, as a fraction of the nominal period.
    /// - Loops: the step `1 + freq + k1·e` with `|freq| ≤ max_dev`, `|e| ≤ 1` (and clamped to
    ///   this in `run_loop`).
    /// - Max-contrast: a fixed grid plus at most one half-period step per 16-symbol window.
    fn shortest_step(&self) -> f64 {
        match self.algo {
            Algo::MaxContrast => WINDOW_SYMBOLS as f64 / (WINDOW_SYMBOLS + 1) as f64,
            _ => (1.0 - self.max_dev - self.k1).max(0.25),
        }
    }

    fn integrating(&self) -> bool {
        self.pulse != Pulse::Rrc
    }

    fn restart(&mut self, item: u64) {
        self.origin = item;
        self.acc.clear();
        self.acc_base = 0;
        self.samples = 0;
        if self.integrating() {
            self.acc.push(0.0);
        } else if let Some(f) = &mut self.rrc {
            f.clear();
        }
        self.next = self.sps;
        self.freq = 0.0;
        self.y_prev = 0.0;
        self.have_prev = false;
        self.level = 0.0;
        self.power = 0.0;
        self.symbols = 0;
        self.metrics = [0.0; CANDIDATES];
        self.have_metrics = false;
        self.win_start = 0.0;
        self.last_start = f64::NEG_INFINITY;
        self.tau = 0.0;
        self.contrast = 0.0;
        self.eye_abs = 0.0;
        self.eye_sq = 0.0;
    }

    #[inline]
    fn append(&mut self, v: f32) {
        // A NaN/inf in the prefix sum would poison every later statistic.
        let v = finite_or_zero(v, &mut self.non_finite);
        if self.integrating() {
            let last = *self.acc.last().unwrap_or(&0.0);
            self.acc.push(last + f64::from(v));
        } else if let Some(f) = &mut self.rrc {
            let y = f.push(v).unwrap_or(0.0);
            self.acc.push(f64::from(y));
        }
        self.samples += 1;
    }

    /// Linear interpolation of `acc` at absolute position `x` (≥ `acc_base`).
    #[inline]
    fn interp(&self, x: f64) -> f64 {
        let rel = x - self.acc_base as f64;
        let max_i = self.acc.len().saturating_sub(2);
        let i = (rel.floor().max(0.0) as usize).min(max_i);
        let f = (rel - i as f64).clamp(0.0, 1.0);
        match (self.acc.get(i), self.acc.get(i + 1)) {
            (Some(a), Some(b)) => a + f * (b - a),
            (Some(a), None) => *a,
            _ => 0.0,
        }
    }

    /// Earliest symbol start whose statistic is computable.
    fn earliest(&self) -> f64 {
        if self.integrating() {
            self.acc_base as f64
        } else {
            self.acc_base as f64 - (self.sps / 2.0 - 0.5 + self.rrc_delay)
        }
    }

    /// Whether the statistic of a symbol starting at `s` is computable.
    #[inline]
    fn ready(&self, s: f64) -> bool {
        let last = if self.integrating() {
            s + self.sps
        } else {
            s + self.sps / 2.0 - 0.5 + self.rrc_delay
        };
        last + 1.0 < self.samples as f64 && s >= self.earliest()
    }

    /// Matched statistic of the symbol starting at `s`.
    #[inline]
    fn strobe(&self, s: f64) -> f64 {
        let t = self.sps;
        match self.pulse {
            Pulse::Nrz => (self.interp(s + t) - self.interp(s)) / t,
            Pulse::Biphase => {
                let a = self.interp(s);
                let m = self.interp(s + t / 2.0);
                let e = self.interp(s + t);
                ((m - a) - (e - m)) / t
            }
            Pulse::Rrc => self.interp(s + t / 2.0 - 0.5 + self.rrc_delay),
        }
    }

    #[inline]
    fn emit(&mut self, s: f64, y: f64, e: f64, out: &mut Vec<f32>, tapped: bool) {
        out.push(y as f32);
        if tapped {
            self.diag.push(e as f32);
        }
        self.first.get_or_insert(s);
        let a = LEVEL_ALPHA.max(1.0 / (self.symbols + 1) as f64);
        self.eye_abs += a * (y.abs() - self.eye_abs);
        self.eye_sq += a * (y * y - self.eye_sq);
        self.symbols += 1;
    }

    /// Gardner / Mueller–Müller / early–late loop: every symbol whose data has arrived.
    fn run_loop(&mut self, out: &mut Vec<f32>, tapped: bool) {
        let t = self.sps;
        let early_late = self.algo == Algo::Gardner && self.pulse == Pulse::Biphase;
        loop {
            let s = self.next;
            let (lo, hi) = if early_late {
                (s - t / 4.0, s + t / 4.0)
            } else {
                (s - t / 2.0, s)
            };
            if !self.ready(hi) {
                return;
            }
            if lo < self.earliest() {
                self.next += t;
                continue;
            }
            let y = self.strobe(s);
            let a = LEVEL_ALPHA.max(1.0 / (self.symbols + 1) as f64);
            self.level += a * (y.abs() - self.level);
            self.power += a * (y * y - self.power);
            let e = if early_late {
                (self.strobe(hi).abs() - self.strobe(lo).abs()) / self.level.max(1e-12)
            } else if !self.have_prev {
                0.0
            } else if self.algo == Algo::MuellerMuller {
                (y * self.y_prev.signum() - self.y_prev * y.signum()) / self.level.max(1e-12)
            } else {
                (self.y_prev - y) * self.strobe(lo) / self.power.max(1e-12)
            };
            let e = e.clamp(-1.0, 1.0);
            self.emit(s, y, e, out, tapped);
            self.freq = (self.freq + self.k2 * e).clamp(-self.max_dev, self.max_dev);
            self.next = s + t * (1.0 + self.freq + self.k1 * e).max(self.min_step);
            self.y_prev = y;
            self.have_prev = true;
        }
    }

    /// Max-contrast timing: every window whose data has arrived.
    fn run_contrast(&mut self, out: &mut Vec<f32>, tapped: bool) {
        let t = self.sps;
        let m = CANDIDATES;
        let smoothing = (self.loop_bw * 20.0).clamp(1e-3, 1.0);
        loop {
            let we = self.win_start + WINDOW_SYMBOLS as f64 * t;
            if !self.ready(we + 1.0) {
                return;
            }
            let base = self.earliest();
            let beta = if self.have_metrics { smoothing } else { 1.0 };
            for k in 0..m {
                let tau_k = k as f64 * t / m as f64;
                let j0 = ((self.win_start - tau_k) / t).ceil();
                let mut s = tau_k + j0 * t;
                let (mut sum, mut cnt) = (0.0, 0u32);
                while s < we {
                    if s >= base {
                        sum += self.strobe(s).abs();
                        cnt += 1;
                    }
                    s += t;
                }
                if cnt > 0 {
                    let v = sum / f64::from(cnt);
                    self.metrics[k] += beta * (v - self.metrics[k]);
                }
            }
            self.have_metrics = true;
            let (kmax, mmax) = self
                .metrics
                .iter()
                .copied()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .unwrap_or((0, 0.0));
            let mean = self.metrics.iter().sum::<f64>() / m as f64;
            self.contrast = if mean > 0.0 { mmax / mean } else { 0.0 };
            let lo = self.metrics[(kmax + m - 1) % m];
            let hi = self.metrics[(kmax + 1) % m];
            let den = lo - 2.0 * mmax + hi;
            let delta = if den < 0.0 {
                (0.5 * (lo - hi) / den).clamp(-0.5, 0.5)
            } else {
                0.0
            };
            let tau = ((kmax as f64 + delta) * t / m as f64).rem_euclid(t);
            let e = {
                let mut d = (tau - self.tau) / t;
                d -= d.round();
                d
            };
            self.tau = tau;

            let from = if self.last_start.is_finite() {
                self.last_start + t / 2.0
            } else {
                self.win_start.max(base)
            };
            let mut s = self.tau + ((from - self.tau) / t).ceil() * t;
            while s < we {
                let y = self.strobe(s);
                self.emit(s, y, e, out, tapped);
                self.last_start = s;
                s += t;
            }
            self.win_start = we;
        }
    }

    /// Drops history no longer needed (no allocation: a move within the buffer).
    fn compact(&mut self) {
        let t = self.sps;
        let keep_from = match self.algo {
            Algo::MaxContrast => {
                let a = if self.last_start.is_finite() {
                    self.last_start.min(self.win_start)
                } else {
                    self.win_start
                };
                a - 2.0 * t
            }
            _ => self.next - 2.0 * t,
        } - self.rrc_delay
            - 2.0;
        let drop = (keep_from - self.acc_base as f64).floor();
        if drop > 0.0 {
            let drop = (drop as usize).min(self.acc.len().saturating_sub(2));
            self.acc.drain(..drop);
            self.acc_base += drop as u64;
        }
    }
}

impl Block for Clock {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = single_input(inputs, "clock_recovery", &[PortType::Iq, PortType::Real])?;
        self.fs = input.rate_hz;
        self.sps = self.fs / self.rate_bd;
        let min_sps = if self.pulse == Pulse::Biphase {
            4.0
        } else {
            2.0
        };
        if self.sps < min_sps {
            return Err(BlockError::Unrealisable(format!(
                "clock_recovery needs at least {min_sps} samples per symbol"
            )));
        }
        if self.sps > MAX_SAMPLES_PER_SYMBOL {
            return Err(BlockError::Params(format!(
                "clock_recovery: {:.0} samples per symbol (limit {MAX_SAMPLES_PER_SYMBOL}): raise symbol_rate_bd or resample first",
                self.sps
            )));
        }
        self.rrc = None;
        self.rrc_len = 0;
        if self.pulse == Pulse::Rrc {
            let len = ((RRC_SPAN_SYMBOLS * self.sps).round() as usize) | 1;
            if len > MAX_TAPS {
                return Err(BlockError::Params(format!(
                    "clock_recovery: the rrc matched filter needs {len} taps (limit {MAX_TAPS}): resample first"
                )));
            }
            let mid = (len - 1) as f64 / 2.0;
            let raw: Vec<f64> = (0..len)
                .map(|i| rrc((i as f64 - mid) / self.sps, RRC_ROLLOFF))
                .collect();
            let sum: f64 = raw.iter().sum();
            let taps = raw.iter().map(|v| (v / sum) as f32).collect();
            self.rrc = Some(FirDecimator::new(taps, 1));
            self.rrc_len = len;
            self.rrc_delay = mid;
        }
        let hold = match self.algo {
            Algo::MaxContrast => ((WINDOW_SYMBOLS + 3) as f64 * self.sps) as usize,
            _ => (3.0 * self.sps) as usize,
        } + self.rrc_len;
        let n = input.max_items;
        self.acc = Vec::with_capacity(n + hold + 64);
        // Symbols of one chunk lie within its samples plus the history held (`hold`), spaced
        // at least the shortest step apart.
        self.sized_k1 = self.k1;
        self.min_step = self.shortest_step();
        let max_out = ((n + hold + 8) as f64 / (self.sps * self.min_step)).ceil() as usize + 4;
        self.diag = Vec::with_capacity(max_out);
        self.restart(0);
        Ok(vec![
            PortInfo {
                ty: PortType::Soft,
                rate_hz: self.rate_bd,
                max_items: max_out,
                hold_items: hold,
            },
            PortInfo {
                ty: PortType::Real,
                rate_hz: self.rate_bd,
                max_items: max_out,
                hold_items: hold,
            },
        ])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let m = input.meta;
        if restarts(m.flags) {
            self.restart(m.index);
        }
        match input.data {
            PortSlice::Real(x) => {
                for &v in x {
                    self.append(v);
                }
            }
            PortSlice::Iq(x) => {
                for z in x {
                    let v = if self.magnitude { z.norm() } else { z.re };
                    self.append(v);
                }
            }
            d => return Err(mismatch(0, PortType::Real, d.port_type())),
        }
        let tapped = io.tapped(1);
        self.first = None;
        self.diag.clear();
        let out = io.output(0)?;
        let before;
        {
            let soft = soft_out(out)?;
            before = soft.len();
            match self.algo {
                Algo::MaxContrast => self.run_contrast(soft, tapped),
                _ => self.run_loop(soft, tapped),
            }
        }
        let produced = out.data.len() - before;
        let pos = self.first.unwrap_or(match self.algo {
            Algo::MaxContrast => self.last_start.max(0.0) + self.sps,
            _ => self.next,
        });
        let center = self.origin as f64 + pos + self.sps / 2.0;
        let per = self.sps * m.source_per_item;
        set_meta(out, &m, source_at(&m, center), per);
        if tapped {
            let te = io.output(1)?;
            set_meta(te, &m, source_at(&m, center), per);
            real_out(te)?.extend_from_slice(&self.diag);
        }
        self.compact();

        self.status.items_in += input.data.len() as u64;
        self.status.items_out += produced as u64;
        if self.symbols > 0 && self.eye_sq > 0.0 {
            let q = (self.eye_abs * self.eye_abs / self.eye_sq).clamp(0.0, 1.0);
            self.status.quality = Some(q as f32);
            if q < 1.0 {
                self.status.snr_db = Some((10.0 * (q / (1.0 - q)).log10()) as f32);
            }
            self.status.lock = if q > 0.8 && self.symbols >= 32 {
                Lock::Locked
            } else {
                Lock::Searching
            };
        }
        self.status.extra.set("symbols", self.symbols as f64);
        match self.algo {
            Algo::MaxContrast => {
                self.status.extra.set("timing_contrast", self.contrast);
                self.status.extra.set("timing_phase", self.tau / self.sps);
            }
            _ => {
                // `freq` is the period error; the symbol rate error is its negative.
                self.status.extra.set("rate_ppm", -self.freq * 1e6);
            }
        }
        report_non_finite(&mut self.status, self.non_finite);
        Ok(())
    }

    fn reset(&mut self) {
        self.restart(0);
    }

    fn update_params(&mut self, p: &Params, _: &BuildCtx<'_>) -> Result<ParamUpdate, BlockError> {
        if !cold_equal(HOT, &self.params, p) {
            return Ok(ParamUpdate::Rebuild);
        }
        let bw = f64_or(p, "loop_bandwidth", 0.01);
        if self.algo != Algo::MaxContrast && loop_gains(self.algo, bw).0 > self.sized_k1 {
            // A wider loop can space symbols closer than the outputs were sized for.
            return Ok(ParamUpdate::Rebuild);
        }
        self.set_loop(bw);
        self.params = p.clone();
        Ok(ParamUpdate::Applied)
    }

    fn status(&self) -> Status {
        self.status
    }
}

#[cfg(test)]
mod tests {
    use hk_recipe::PortType;
    use serde_json::json;

    use crate::block::{Block, Io, ParamUpdate, PortInfo, TapMask};
    use crate::blocks::iq::testkit::*;
    use crate::buffer::{ChunkFlags, ChunkMeta, Input, Output, PortSlice, PortVec};

    fn real_info(fs: f64, max_items: usize) -> PortInfo {
        PortInfo {
            ty: PortType::Real,
            rate_hz: fs,
            max_items,
            hold_items: 0,
        }
    }

    /// Runs `block` over `x` in chunks cycling through `chunks` (each ≤ `info.max_items`),
    /// asserting every output chunk fits its declared `max_items`. Returns the symbol count.
    fn run_bounded(block: &mut dyn Block, info: PortInfo, x: &[f32], chunks: &[usize]) -> usize {
        let outs = block.init(&[info]).unwrap();
        let mut outputs: Vec<Output> = outs.iter().map(Output::for_port).collect();
        let (mut k, mut c, mut symbols) = (0, 0, 0);
        while k < x.len() {
            let e = (k + chunks[c % chunks.len()]).min(x.len());
            c += 1;
            let meta = ChunkMeta {
                index: k as u64,
                source_index: k as f64,
                source_per_item: 1.0,
                rate_hz: info.rate_hz,
                channel: 0,
                flags: if k == 0 {
                    ChunkFlags::DISCONTINUITY
                } else {
                    ChunkFlags::NONE
                },
            };
            for o in &mut outputs {
                o.begin_chunk();
            }
            let inputs = [Input {
                meta,
                data: PortSlice::Real(&x[k..e]),
            }];
            block
                .process(&mut Io::new(&inputs, &mut outputs).with_taps(TapMask(u32::MAX)))
                .unwrap();
            for (o, declared) in outputs.iter().zip(&outs) {
                assert!(
                    o.data.len() <= declared.max_items,
                    "{} items > declared {} at sample {k}",
                    o.data.len(),
                    declared.max_items
                );
            }
            symbols += outputs[0].data.len();
            k = e;
        }
        symbols
    }

    #[test]
    fn output_count_stays_within_the_declared_bound_at_max_loop_bandwidth() {
        let fs = 9_600.0;
        let mut rng = Lcg::new(42);
        let x: Vec<f32> = (0..200_000).map(|_| rng.gauss() as f32).collect();
        for (algo, pulse, rate) in [
            ("mueller-muller", "nrz", 4_800.0),
            ("mueller-muller", "rrc", 4_800.0),
            ("gardner", "nrz", 4_800.0),
            ("gardner", "rrc", 4_800.0),
            ("gardner", "biphase", 2_400.0),
            ("max-contrast", "biphase", 2_400.0),
        ] {
            let p = json!({"symbol_rate_bd": rate, "pulse": pulse, "algorithm": algo,
                           "loop_bandwidth": 0.25, "max_deviation_ppm": 20_000});
            let mut b = build("clock_recovery", p, PortType::Real);
            let n = run_bounded(b.as_mut(), real_info(fs, 64), &x, &[64, 1, 17, 64, 3, 40]);
            assert!(n > 0, "{algo} {pulse}");
        }
        // The bound holds even if every step is the shortest the loop allows (k1 ≈ 0.48,
        // max deviation 2 %): at 2 samples per symbol, one symbol per sample.
        let mut b = build(
            "clock_recovery",
            json!({"symbol_rate_bd": 4_800, "algorithm": "mueller-muller",
                   "loop_bandwidth": 0.25, "max_deviation_ppm": 20_000}),
            PortType::Real,
        );
        let outs = b.init(&[real_info(fs, 128)]).unwrap();
        assert!(outs[0].max_items >= 128, "{}", outs[0].max_items);
    }

    #[test]
    fn a_hot_loop_bandwidth_needing_more_output_is_a_rebuild() {
        let with_bw = |bw: f64| json!({"symbol_rate_bd": 1_200, "loop_bandwidth": bw});
        let mut b = build("clock_recovery", with_bw(0.01), PortType::Real);
        b.init(&[real_info(9_600.0, 256)]).unwrap();
        assert_eq!(
            update(b.as_mut(), with_bw(0.005), PortType::Real),
            ParamUpdate::Applied
        );
        assert_eq!(
            update(b.as_mut(), with_bw(0.01), PortType::Real),
            ParamUpdate::Applied
        );
        assert_eq!(
            update(b.as_mut(), with_bw(0.25), PortType::Real),
            ParamUpdate::Rebuild
        );
        // Max-contrast spacing does not depend on the bandwidth.
        let mc = |bw: f64| {
            json!({"symbol_rate_bd": 1_200, "pulse": "biphase", "algorithm": "max-contrast",
                   "loop_bandwidth": bw})
        };
        let mut b = build("clock_recovery", mc(0.01), PortType::Real);
        b.init(&[real_info(9_600.0, 256)]).unwrap();
        assert_eq!(
            update(b.as_mut(), mc(0.25), PortType::Real),
            ParamUpdate::Applied
        );
    }

    #[test]
    fn extreme_symbol_rates_are_rejected_at_init() {
        let mut b = build(
            "clock_recovery",
            json!({"symbol_rate_bd": 0.01}),
            PortType::Real,
        );
        let e = b
            .init(&[real_info(48_000.0, 4_096)])
            .expect_err("rejected")
            .to_string();
        assert!(e.contains("samples per symbol"), "{e}");
        let mut b = build(
            "clock_recovery",
            json!({"symbol_rate_bd": 3, "pulse": "rrc"}),
            PortType::Real,
        );
        let e = b
            .init(&[real_info(48_000.0, 4_096)])
            .expect_err("rejected")
            .to_string();
        assert!(e.contains("taps"), "{e}");
    }

    #[test]
    fn non_finite_input_does_not_poison_the_prefix_sum() {
        let fs = 9_600.0;
        let mut rng = Lcg::new(3);
        let bits: Vec<u8> = (0..2_000).map(|_| rng.bit()).collect();
        let clean: Vec<f32> = (0..bits.len() * 8)
            .map(|i| f32::from(bits[i / 8]) * 2.0 - 1.0 + 0.05 * rng.gauss() as f32)
            .collect();
        let (mut bad, mut zeroed) = (clean.clone(), clean);
        for i in [100, 101, 5_000, 9_999] {
            bad[i] = f32::NAN;
            zeroed[i] = 0.0;
        }
        bad[7_000] = f32::INFINITY;
        zeroed[7_000] = 0.0;
        for pulse in ["nrz", "rrc"] {
            let make = || {
                vec![build(
                    "clock_recovery",
                    json!({"symbol_rate_bd": 1_200, "pulse": pulse}),
                    PortType::Real,
                )]
            };
            let got = assert_chunk_invariant(
                make,
                PortType::Real,
                fs,
                &PortVec::Real(bad.clone()),
                &[512, 77],
            );
            let want = assert_chunk_invariant(
                make,
                PortType::Real,
                fs,
                &PortVec::Real(zeroed.clone()),
                &[512],
            );
            let y = &got.out(0, 0).real;
            assert_eq!(y, &want.out(0, 0).real, "{pulse}");
            assert!(
                y.len() > 1_900 && y.iter().all(|v| v.is_finite()),
                "{pulse}"
            );
            let st = got.block(0).status();
            assert_eq!(
                st.extra.iter().find(|e| e.0 == "non_finite").map(|e| e.1),
                Some(5.0)
            );
        }
    }
}
