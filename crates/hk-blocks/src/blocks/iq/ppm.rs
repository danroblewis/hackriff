//! `ppm_demod`: pulse-position frames (ADS-B Mode S) with DF-decided length.

use hk_recipe::{Params, PortType};

use super::common::*;
use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::blocks::framing::length::{FrameLength, LengthState};
use crate::buffer::{FrameInfo, PortVec};
use crate::registry::BuildCtx;
use crate::status::{Lock, Status};

/// Most samples of preamble plus frame a detector holds.
const MAX_WINDOW: usize = 1 << 22;

pub(crate) fn build(p: &Params, _: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let pchips = i64_or(p, "preamble_chips", 0) as usize;
    let preamble =
        get_hex(p, "preamble").ok_or_else(|| BlockError::Params("preamble required".into()))?;
    if !(1..=64).contains(&pchips) || (pchips < 64 && preamble >> pchips != 0) {
        return Err(BlockError::Params(
            "preamble does not fit preamble_chips".into(),
        ));
    }
    let ones = preamble.count_ones() as usize;
    if ones == 0 || ones == pchips {
        return Err(BlockError::Params(
            "preamble needs both pulse and quiet chips".into(),
        ));
    }
    let max_bits = i64_or(p, "frame_bits", 0).clamp(0, i64::from(u32::MAX)) as u32;
    let length = FrameLength::from_params(p, max_bits)?;
    Ok(Box::new(Ppm {
        params: p.clone(),
        bit_rate: require_f64(p, "bit_rate_bd")?,
        cpb: i64_or(p, "chips_per_bit", 2) as usize,
        preamble,
        pchips,
        min_snr_db: f64_or(p, "min_snr_db", 6.0) as f32,
        max_bits: max_bits as usize,
        length,
        non_finite: 0,
        fs: 0.0,
        spc: 1.0,
        chip_len: 1,
        mags: Vec::new(),
        base: 0,
        scan: 0,
        bits: Vec::new(),
        softs: Vec::new(),
        diag: Vec::new(),
        frames: 0,
        preambles: 0,
        snr_db: 0.0,
        quality: 0.0,
        last_frame_item: None,
        status: Status::default(),
    }))
}

/// Magnitude history scanned for preambles; a frame is decided once its last chip arrived.
struct Ppm {
    params: Params,
    bit_rate: f64,
    cpb: usize,
    preamble: u64,
    pchips: usize,
    min_snr_db: f32,
    max_bits: usize,
    /// The shared frame-length evaluator (`length_from`, capped at `frame_bits`).
    length: FrameLength,
    non_finite: u64,
    fs: f64,
    /// Samples per chip.
    spc: f64,
    chip_len: usize,
    mags: Vec<f32>,
    /// Input item index of `mags[0]`.
    base: u64,
    scan: usize,
    bits: Vec<u8>,
    softs: Vec<f32>,
    diag: Vec<f32>,
    frames: u64,
    preambles: u64,
    snr_db: f32,
    quality: f32,
    last_frame_item: Option<u64>,
    status: Status,
}

/// Sub-sample timing search per detected preamble, in steps of this fraction of a chip.
const PHASE_STEPS_PER_CHIP: f64 = 8.0;

impl Ppm {
    fn chip_start(&self, k: usize) -> usize {
        (k as f64 * self.spc).round() as usize
    }

    /// Samples spanned by the first `chips` chips.
    fn span(&self, chips: usize) -> usize {
        if chips == 0 {
            0
        } else {
            self.chip_start(chips - 1) + self.chip_len
        }
    }

    /// Lowest and highest phase offsets (samples, relative to the detecting integer position)
    /// searched per preamble, and the step.
    fn phases(&self) -> (f64, f64, f64) {
        (
            -(0.5 * self.spc).max(1.0),
            self.spc.max(1.5),
            self.spc / PHASE_STEPS_PER_CHIP,
        )
    }

    /// Samples kept before `scan` so the most negative phase never reads before `mags[0]`.
    fn history(&self) -> usize {
        (-self.phases().0 + 0.5 * self.spc).ceil() as usize + 1
    }

    /// Sample coordinate of the last interpolation point of chip `k` for a frame at `origin`.
    #[inline]
    fn last_point(&self, origin: f64, k: usize) -> f64 {
        let n = self.chip_len as f64;
        origin + k as f64 * self.spc + 0.5 * (self.spc - 1.0) + 0.5 * (n - 1.0) * self.spc / n
    }

    /// Whether chips `0..chips` of a frame at `origin` lie inside the magnitude history.
    #[inline]
    fn has_chips(&self, origin: f64, chips: usize) -> bool {
        chips == 0 || (self.last_point(origin, chips - 1).floor() as usize + 1) < self.mags.len()
    }

    /// Chip `k` of a frame whose chip 0 starts at the fractional sample `origin`: the linearly
    /// interpolated magnitude at `chip_len` points spread over the chip, centred on it (at one
    /// sample per chip and an integer origin, exactly the sample). Frame timing is therefore
    /// not quantised to whole samples: a burst's arbitrary sub-sample phase (a squitter at
    /// 2 Msps is ~1 sample per chip) no longer decides which bit patterns survive.
    #[inline]
    fn chip(&self, origin: f64, k: usize) -> f32 {
        let n = self.chip_len;
        let step = self.spc / n as f64;
        let first =
            origin + k as f64 * self.spc + 0.5 * (self.spc - 1.0) - 0.5 * (n as f64 - 1.0) * step;
        let mut acc = 0.0;
        for j in 0..n {
            let t = first + j as f64 * step;
            let i = t.floor();
            let f = (t - i) as f32;
            let i = i as usize;
            acc += self.mags[i] * (1.0 - f) + self.mags[i + 1] * f;
        }
        acc
    }

    /// Preamble level ratio in dB at `origin`, if it passes.
    fn score(&self, origin: f64) -> Option<f32> {
        let (mut on, mut non, mut off, mut noff, mut min_on) = (0.0, 0, 0.0, 0, f32::INFINITY);
        for k in 0..self.pchips {
            let e = self.chip(origin, k);
            if (self.preamble >> (self.pchips - 1 - k)) & 1 == 1 {
                on += e;
                non += 1;
                min_on = min_on.min(e);
            } else {
                off += e;
                noff += 1;
            }
        }
        let on_mean = on / non as f32;
        let off_mean = off / noff as f32;
        // Every pulse chip (not just their mean) must clear min_snr_db over the quiet level:
        // noise alone then passes with probability ~P(chip > k·mean)^pulses.
        let threshold = off_mean * 10f32.powf(self.min_snr_db / 20.0);
        if on_mean <= 0.0 || min_on <= threshold.max(off_mean) {
            return None;
        }
        let db = if off_mean > 0.0 {
            20.0 * (on_mean / off_mean).log10()
        } else {
            60.0
        };
        Some(db)
    }

    /// Soft data bit `j` of the frame at `origin`: (early − late) / (early + late).
    #[inline]
    fn soft_bit(&self, origin: f64, j: usize) -> f32 {
        let c0 = self.pchips + j * self.cpb;
        let half = self.cpb / 2;
        let (mut e, mut l) = (0.0, 0.0);
        for i in 0..half {
            e += self.chip(origin, c0 + i);
            l += self.chip(origin, c0 + self.cpb - half + i);
        }
        if e + l > 0.0 { (e - l) / (e + l) } else { 0.0 }
    }

    /// Decodes the frame at `origin` into `bits`/`softs` until the frame-length rules end it.
    /// `None` while its last chip has not arrived; otherwise the summed decision margin |soft|.
    fn decode_at(&mut self, origin: f64) -> Option<f32> {
        self.bits.clear();
        self.softs.clear();
        let mut st = LengthState::default();
        let mut sum = 0.0;
        for j in 0..self.max_bits {
            if !self.has_chips(origin, self.pchips + (j + 1) * self.cpb) {
                return None;
            }
            let s = self.soft_bit(origin, j);
            sum += s.abs();
            self.softs.push(s);
            self.bits.push(u8::from(s > 0.0));
            if self.length.after_bit(&mut st, &self.bits) {
                return Some(sum);
            }
        }
        None
    }

    fn clear(&mut self, item: u64) {
        self.mags.clear();
        self.base = item;
        // The first samples after a restart are history only (never a frame origin), so the
        // phase search reads the same samples however the input was chunked.
        self.scan = self.history();
    }
}

impl Block for Ppm {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = single_input(inputs, "ppm_demod", &[PortType::Iq])?;
        self.fs = input.rate_hz;
        self.spc = self.fs / (self.bit_rate * self.cpb as f64);
        if self.spc < 1.0 - 1e-9 {
            return Err(BlockError::Unrealisable(
                "ppm_demod needs at least one sample per chip".into(),
            ));
        }
        if self.spc > MAX_SAMPLES_PER_SYMBOL {
            return Err(BlockError::Params(format!(
                "ppm_demod: {:.0} samples per chip (limit {MAX_SAMPLES_PER_SYMBOL}): raise bit_rate_bd",
                self.spc
            )));
        }
        self.chip_len = (self.spc + 1e-9).floor().max(1.0) as usize;
        // A frame at the latest searched phase, plus the history kept before the scan position.
        let window = self.span(self.pchips + self.max_bits * self.cpb)
            + self.phases().1.ceil() as usize
            + self.history()
            + 2;
        if window > MAX_WINDOW {
            return Err(BlockError::Params(format!(
                "ppm_demod: a frame spans {window} samples (limit {MAX_WINDOW}): lower frame_bits or raise bit_rate_bd"
            )));
        }
        let n = input.max_items;
        self.mags = Vec::with_capacity(window + n + 2);
        self.bits = Vec::with_capacity(self.max_bits);
        self.softs = Vec::with_capacity(self.max_bits);
        let min_bits = self.length.min_bits() as usize;
        let min_span = self.span(self.pchips + min_bits * self.cpb).max(1);
        let frames_max = (n + window) / min_span + 1;
        let per_bit = (self.cpb as f64 * self.spc).floor().max(1.0) as usize;
        let soft_max = (n + window) / per_bit + self.max_bits;
        self.diag = Vec::with_capacity(soft_max);
        self.clear(0);
        Ok(vec![
            PortInfo {
                ty: PortType::Frames,
                rate_hz: self.bit_rate
                    / (self.max_bits as f64 + self.pchips as f64 / self.cpb as f64),
                max_items: frames_max,
                hold_items: window,
            },
            PortInfo {
                ty: PortType::Soft,
                rate_hz: self.bit_rate,
                max_items: soft_max,
                hold_items: window,
            },
        ])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let x = iq_in(&input)?;
        let m = input.meta;
        if restarts(m.flags) || self.mags.is_empty() {
            self.clear(m.index);
        }
        // A non-finite magnitude would make every preamble comparison false and pass.
        let mut bad = 0;
        self.mags
            .extend(x.iter().map(|z| finite_or_zero(z.norm(), &mut bad)));
        self.non_finite += bad;
        let tapped = io.tapped(1);
        self.diag.clear();

        let out = io.output(0)?;
        set_meta(out, &m, m.source_index, m.source_per_item);
        let PortVec::Frames(frames) = &mut out.data else {
            return Err(mismatch(0, PortType::Frames, out.data.port_type()));
        };
        let min_bits = self.length.min_bits() as usize;
        let data_start = self.chip_start(self.pchips);
        let (ph_lo, ph_hi, ph_step) = self.phases();
        let hist = self.history();
        let n_phases = ((ph_hi - ph_lo) / ph_step).round() as usize + 1;
        while self.has_chips(self.scan as f64 + ph_hi, self.pchips + min_bits * self.cpb) {
            // Detection on a half-sample grid: at ~1 sample per chip a burst half a sample off
            // the integer grid spreads each pulse into its neighbours, which can fail every
            // integer position's per-pulse test.
            let Some(db0) = self
                .score(self.scan as f64)
                .or_else(|| self.score(self.scan as f64 + 0.5))
            else {
                self.scan += 1;
                continue;
            };
            // Sub-sample timing: decode the frame at each phase around the detection and keep
            // the one with the largest decision margin whose preamble still passes (a whole-chip
            // slip fails the preamble). Wait for more input if a phase's last chip is missing.
            let pos = self.scan;
            let mut best: Option<(f64, f32, f32)> = None;
            let mut waiting = false;
            for p in 0..n_phases {
                let origin = pos as f64 + ph_lo + p as f64 * ph_step;
                let Some(db) = self.score(origin) else {
                    continue;
                };
                let Some(margin) = self.decode_at(origin) else {
                    waiting = true;
                    break;
                };
                let margin = margin / self.bits.len() as f32;
                if best.is_none_or(|(_, m, _)| margin > m) {
                    best = Some((origin, margin, db));
                }
            }
            if waiting {
                break;
            }
            let (origin, db) = match best {
                Some((o, _, d)) => (o, d),
                None => (pos as f64, db0),
            };
            let Some(sum) = self.decode_at(origin) else {
                break;
            };
            let start = origin.round().max(0.0) as usize;
            let len = self.bits.len();
            let item = self.base + (start + data_start) as u64;
            let source = source_at(&m, item as f64).round().max(0.0) as u64;
            frames.push_bits(&self.bits, FrameInfo::new(self.frames, source, m.channel));
            if tapped {
                self.diag.extend_from_slice(&self.softs);
            }
            self.frames += 1;
            self.preambles += 1;
            self.snr_db = if self.frames == 1 {
                db
            } else {
                self.snr_db + 0.1 * (db - self.snr_db)
            };
            self.quality = sum / len as f32;
            self.last_frame_item = Some(self.base + start as u64);
            self.scan = (start + self.span(self.pchips + len * self.cpb)).max(pos + 1);
        }
        let produced = frames.len() as u64;
        if self.scan > hist {
            let cut = self.scan - hist;
            self.mags.drain(..cut);
            self.base += cut as u64;
            self.scan = hist;
        }

        if tapped {
            let soft = io.output(1)?;
            set_meta(soft, &m, m.source_index, m.source_per_item);
            soft_out(soft)?.extend_from_slice(&self.diag);
        }

        let end = m.index + x.len() as u64;
        self.status.items_in += x.len() as u64;
        self.status.items_out += produced;
        self.status.lock = match self.last_frame_item {
            Some(i) if end.saturating_sub(i) as f64 <= self.fs => Lock::Locked,
            _ => Lock::Searching,
        };
        if self.frames > 0 {
            self.status.snr_db = Some(self.snr_db);
            self.status.quality = Some(self.quality);
        }
        self.status.extra.set("frames", self.frames as f64);
        report_non_finite(&mut self.status, self.non_finite);
        Ok(())
    }

    fn reset(&mut self) {
        self.clear(0);
    }

    fn update_params(&mut self, p: &Params, _: &BuildCtx<'_>) -> Result<ParamUpdate, BlockError> {
        if !cold_equal(&["min_snr_db"], &self.params, p) {
            return Ok(ParamUpdate::Rebuild);
        }
        self.min_snr_db = f64_or(p, "min_snr_db", 6.0) as f32;
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
    use super::{Lock, ParamUpdate, PortType, PortVec};
    use crate::block::PortInfo;
    use num_complex::Complex32;
    use serde_json::json;

    fn adsb_params() -> serde_json::Value {
        json!({
            "bit_rate_bd": 1000000, "chips_per_bit": 2, "preamble": "0xA140",
            "preamble_chips": 16, "frame_bits": 112,
            "length_from": {"offset_bits": 0, "bits": 5,
                "cases": [{"min": 16, "max": 31, "frame_bits": 112}], "default_bits": 56}
        })
    }

    /// 2 Msps IQ with Mode S frames at the given starts; returns the samples and the truth
    /// (start sample of the first data bit, bits).
    fn adsb_signal(seed: u64) -> (Vec<Complex32>, Vec<(u64, Vec<u8>)>) {
        let mut rng = Lcg::new(seed);
        let n = 6_000;
        let noise = 0.01; // amplitude 1 pulses: ~20 dB SNR per sample
        let mut x: Vec<Complex32> = (0..n).map(|_| rng.cnoise(noise)).collect();
        let mut truth = Vec::new();
        for (start, df) in [(300usize, 17u8), (1_000, 11), (1_400, 17), (3_333, 4)] {
            let len = if df >= 16 { 112 } else { 56 };
            let mut bits: Vec<u8> = (0..5).map(|i| (df >> (4 - i)) & 1).collect();
            bits.extend((5..len).map(|_| rng.bit()));
            let phase = rng.unit() * std::f64::consts::TAU;
            let carrier = Complex32::new(phase.cos() as f32, phase.sin() as f32);
            let mut chips = vec![0u8; 16];
            for k in [0, 2, 7, 9] {
                chips[k] = 1;
            }
            for &b in &bits {
                chips.extend(if b == 1 { [1, 0] } else { [0, 1] });
            }
            for (k, &c) in chips.iter().enumerate() {
                if c == 1 {
                    x[start + k] += carrier;
                }
            }
            truth.push(((start + 16) as u64, bits));
        }
        (x, truth)
    }

    fn try_init(p: serde_json::Value, fs: f64) -> Result<(), String> {
        let reg = crate::Registry::builtin();
        let maps = std::collections::BTreeMap::new();
        let ctx = crate::BuildCtx {
            field_maps: &maps,
            input_types: &[PortType::Iq],
        };
        let mut b = reg
            .build("ppm_demod", &params(p), &ctx)
            .map_err(|e| e.to_string())?;
        let info = PortInfo {
            ty: PortType::Iq,
            rate_hz: fs,
            max_items: 4096,
            hold_items: 0,
        };
        b.init(&[info]).map(|_| ()).map_err(|e| e.to_string())
    }

    #[test]
    fn frame_length_uses_the_shared_evaluator_and_its_validations() {
        assert!(try_init(adsb_params(), 2e6).is_ok());
        // The length field must end inside frame_bits.
        let mut p = adsb_params();
        p["frame_bits"] = json!(4);
        let e = try_init(p, 2e6).unwrap_err();
        assert!(e.contains("frame_bits"), "{e}");
        // A case needs min ≤ max (FrameLength's validation, not a ppm-specific copy).
        let mut p = adsb_params();
        p["length_from"]["cases"] = json!([{"min": 31, "max": 16, "frame_bits": 112}]);
        let e = try_init(p, 2e6).unwrap_err();
        assert!(e.contains("min"), "{e}");
    }

    #[test]
    fn extreme_rates_are_rejected_at_init() {
        // 1 Bd at 2 Msps: a million samples per chip.
        let mut p = adsb_params();
        p["bit_rate_bd"] = json!(1);
        let e = try_init(p, 2e6).unwrap_err();
        assert!(e.contains("samples per chip"), "{e}");
        let mut p = adsb_params();
        p["bit_rate_bd"] = json!(1000);
        p["frame_bits"] = json!(4096);
        p["chips_per_bit"] = json!(16);
        let e = try_init(p, 2e6).unwrap_err();
        assert!(e.contains("spans"), "{e}");
    }

    #[test]
    fn non_finite_magnitudes_never_pass_the_preamble_check() {
        let (mut x, truth) = adsb_signal(77);
        // NaN and inf runs away from the frames: an all-NaN window used to pass every
        // preamble comparison.
        for z in &mut x[2_000..2_400] {
            *z = Complex32::new(f32::NAN, 0.0);
        }
        for z in &mut x[4_500..4_700] {
            *z = Complex32::new(f32::INFINITY, f32::NAN);
        }
        let c = assert_chunk_invariant(
            || vec![build("ppm_demod", adsb_params(), PortType::Iq)],
            PortType::Iq,
            2e6,
            &PortVec::Iq(x),
            &[4096, 333],
        );
        let frames = &c.out(0, 0).frames;
        assert_eq!(frames.len(), truth.len(), "{frames:?}");
        for ((_, info), (start, _)) in frames.iter().zip(&truth) {
            assert_eq!(info.source_index, *start);
        }
        let st = c.block(0).status();
        let bad = st.extra.iter().find(|e| e.0 == "non_finite").unwrap().1;
        assert_eq!(bad, 600.0);
    }

    #[test]
    fn adsb_frames_have_df_length_exact_bits_and_sample_index() {
        let (x, truth) = adsb_signal(1090);
        let c = assert_chunk_invariant(
            || vec![build("ppm_demod", adsb_params(), PortType::Iq)],
            PortType::Iq,
            2e6,
            &PortVec::Iq(x),
            &[4096, 777, 250],
        );
        let frames = &c.out(0, 0).frames;
        assert_eq!(frames.len(), truth.len(), "{frames:?}");
        for ((bytes, info), (start, bits)) in frames.iter().zip(&truth) {
            assert_eq!(info.bit_len as usize, bits.len());
            let got: Vec<u8> = (0..info.bit_len as usize)
                .map(|i| (bytes[i / 8] >> (7 - i % 8)) & 1)
                .collect();
            assert_eq!(&got, bits);
            assert_eq!(info.source_index, *start);
        }
        let soft = &c.out(0, 1).real;
        assert_eq!(soft.len(), truth.iter().map(|t| t.1.len()).sum::<usize>());
        let st = c.block(0).status();
        assert!(st.snr_db.unwrap() > 15.0, "{st:?}");
        assert_eq!(st.lock, Lock::Locked);
    }

    /// T-110 regression: the four DF17 squitters T-097's blind acceptance never decoded
    /// (`py/hkpy/synth` seed 5: a0b1c2/4ca853 airborne-position-even and velocity), rendered as
    /// band-limited pulses (±1 MHz) at 2.4 Msps — 1.2 samples per chip — at every tenth-sample
    /// timing phase. Chips used to be read at whole-sample positions only, so a burst's
    /// sub-sample phase decided which bit patterns survived (0 of these 40 decoded exactly).
    #[test]
    fn band_limited_squitters_decode_at_every_sub_sample_phase() {
        const HEX: [&str; 4] = [
            "8da0b1c299050290e88000f70173",
            "8da0b1c258c592b5fe17d25af925",
            "8d4ca85358c112ff4bcd88dfa335",
            "8d4ca85399012602c03c008259dc",
        ];
        // 24 Msps rendering (12 samples per chip), decimated by 10 to 2.4 Msps.
        let (up, dec, phases, spacing) = (12usize, 10usize, 10usize, 3_600usize);
        let n_hi = spacing * (HEX.len() * phases + 2);
        let mut hi = vec![0f32; n_hi];
        let mut truth = Vec::new();
        for (m, hex) in HEX.iter().enumerate() {
            let bits: Vec<u8> = (0..112)
                .map(|i| {
                    let byte = u8::from_str_radix(&hex[i / 8 * 2..i / 8 * 2 + 2], 16).unwrap();
                    (byte >> (7 - i % 8)) & 1
                })
                .collect();
            let mut chips = vec![0u8; 16];
            for k in [0, 2, 7, 9] {
                chips[k] = 1;
            }
            for &b in &bits {
                chips.extend(if b == 1 { [1, 0] } else { [0, 1] });
            }
            for o in 0..phases {
                let start = spacing * (1 + m * phases + o) + o;
                for (k, &c) in chips.iter().enumerate() {
                    if c == 1 {
                        hi[start + k * up..start + (k + 1) * up].fill(1.0);
                    }
                }
                truth.push((start, bits.clone()));
            }
        }
        // Windowed-sinc low-pass, 1 MHz cutoff at 24 Msps, zero delay (centred taps).
        let half = 120isize;
        let fc = 1.0 / 24.0;
        let taps: Vec<f32> = (-half..=half)
            .map(|i| {
                let x = i as f64;
                let sinc = if i == 0 {
                    2.0 * fc
                } else {
                    (std::f64::consts::TAU * fc * x).sin() / (std::f64::consts::PI * x)
                };
                let w = 0.54 + 0.46 * (std::f64::consts::PI * x / half as f64).cos();
                (sinc * w) as f32
            })
            .collect();
        let mut rng = Lcg::new(1110);
        let carrier = Complex32::from_polar(1.0, 0.7);
        let x: Vec<Complex32> = (0..n_hi / dec)
            .map(|j| {
                let c = (j * dec) as isize;
                let env: f32 = taps
                    .iter()
                    .enumerate()
                    .map(|(t, &h)| {
                        let s = c + t as isize - half;
                        if (0..n_hi as isize).contains(&s) {
                            h * hi[s as usize]
                        } else {
                            0.0
                        }
                    })
                    .sum();
                // ~30 dB pulse SNR per sample (the seed-5 fixture's squitters are ~35 dB).
                carrier * env + rng.cnoise(0.001)
            })
            .collect();
        let c = assert_chunk_invariant(
            || vec![build("ppm_demod", adsb_params(), PortType::Iq)],
            PortType::Iq,
            2.4e6,
            &PortVec::Iq(x),
            &[4096, 1000, 333],
        );
        let frames = &c.out(0, 0).frames;
        assert_eq!(frames.len(), truth.len(), "{frames:?}");
        for (i, ((bytes, info), (start, bits))) in frames.iter().zip(&truth).enumerate() {
            let got: Vec<u8> = (0..info.bit_len as usize)
                .map(|b| (bytes[b / 8] >> (7 - b % 8)) & 1)
                .collect();
            assert_eq!(
                &got,
                bits,
                "frame {i} (message {}, phase {})",
                i / phases,
                i % phases
            );
            // First data chip (chip 16 = 192 samples at 24 Msps) at 2.4 Msps, ±1 sample.
            let want = (start + 16 * up) as f64 / dec as f64;
            assert!(
                (info.source_index as f64 - want).abs() <= 1.0,
                "frame {i}: source {} vs {want}",
                info.source_index
            );
        }
    }

    #[test]
    fn noise_alone_gives_no_frames_and_min_snr_is_hot() {
        let mut rng = Lcg::new(5);
        let x: Vec<Complex32> = (0..200_000).map(|_| rng.cnoise(1.0)).collect();
        let mut p = adsb_params();
        p["min_snr_db"] = json!(12.0);
        let c = assert_chunk_invariant(
            || vec![build("ppm_demod", p.clone(), PortType::Iq)],
            PortType::Iq,
            2e6,
            &PortVec::Iq(x),
            &[65_536],
        );
        assert!(c.out(0, 0).frames.len() < 5, "{}", c.out(0, 0).frames.len());
        let mut b = build("ppm_demod", adsb_params(), PortType::Iq);
        let mut hot = adsb_params();
        hot["min_snr_db"] = json!(9.0);
        assert_eq!(update(b.as_mut(), hot, PortType::Iq), ParamUpdate::Applied);
        let mut cold = adsb_params();
        cold["frame_bits"] = json!(56);
        assert_eq!(update(b.as_mut(), cold, PortType::Iq), ParamUpdate::Rebuild);
    }
}
