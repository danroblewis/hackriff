//! Broadcast WFM: complex baseband at the MPX rate → quadrature discriminator → MPX, then
//!
//! - pilot PLL (lock indicator, pilot frequency and deviation; stereo flag),
//! - mono audio: 15 kHz FIR low-pass and ÷5 to 48 kS/s, de-emphasis (75 µs Americas default,
//!   50 µs configurable), scaled so ±75 kHz deviation is ±1,
//! - RDS on the 57 kHz subcarrier, phase-locked to 3 × the pilot,
//! - optionally ([`WfmDemod::enable_stereo`], T-874, ADR-0015 §12.13) the L−R channel: the
//!   38 kHz DSB-SC subcarrier brought to baseband on the pilot PLL's `2θ`, through a copy of the
//!   mono path's own low-pass and de-emphasis (identical delay), so `L = M + S`, `R = M − S`.
//!   **Honest mono fallback:** while the PLL is unlocked nothing is fed to the L−R path, so it
//!   decays to zero and `L = R = M` — never an L−R guessed from an unlocked carrier. The mono
//!   path is untouched by it: a demodulator that never enables stereo computes exactly what it
//!   did before.
//!
//! State carries across [`WfmDemod::process`] calls; allocation is per call (output buffers),
//! never per sample.

use num_complex::Complex32;
use serde::{Deserialize, Serialize};

use crate::dsp::{Deemphasis, Discriminator, FirDecimator, lowpass_taps};
use crate::pilot::{PilotConfig, PilotPll, PilotReport};
use crate::rds::{RdsConfig, RdsDecoder, RdsDemod, RdsReport};

/// MPX sample rate the receiver down-converts WFM channels to, Hz (÷5 → 48 kS/s audio,
/// ÷10 → 24 kS/s RDS).
pub const MPX_RATE_HZ: f64 = 240_000.0;

/// De-emphasis time constants.
pub mod deemphasis {
    /// Americas, South Korea, s.
    pub const US_75_US: f64 = 75e-6;
    /// Europe and most other regions, s.
    pub const EU_50_US: f64 = 50e-6;
}

/// WFM demodulator settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WfmConfig {
    /// De-emphasis τ, s (0 disables).
    pub deemphasis_tau_s: f64,
    /// MPX → audio decimation.
    pub audio_decimation: usize,
    /// Audio low-pass passband, Hz.
    pub audio_passband_hz: f64,
    /// Audio low-pass stopband, Hz (below the pilot).
    pub audio_stopband_hz: f64,
    /// Audio low-pass attenuation, dB.
    pub audio_stopband_db: f64,
    /// Deviation mapped to audio full scale, Hz.
    pub full_scale_deviation_hz: f64,
    /// Pilot PLL.
    pub pilot: PilotConfig,
    /// RDS (`None` disables).
    pub rds: Option<RdsConfig>,
}

impl Default for WfmConfig {
    fn default() -> Self {
        Self {
            deemphasis_tau_s: deemphasis::US_75_US,
            audio_decimation: 5,
            audio_passband_hz: 15_000.0,
            audio_stopband_hz: 18_500.0,
            audio_stopband_db: 60.0,
            full_scale_deviation_hz: 75_000.0,
            pilot: PilotConfig::default(),
            rds: Some(RdsConfig::default()),
        }
    }
}

/// What the WFM chain measured and decoded.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WfmReport {
    /// MPX rate, Hz.
    pub mpx_rate_hz: f64,
    /// MPX samples processed.
    pub mpx_samples: u64,
    /// Mean discriminator output: residual carrier offset of the channel, Hz.
    pub carrier_offset_hz: f64,
    /// 99.9th percentile of |deviation| about the carrier, Hz (1 kHz bins).
    pub peak_deviation_hz: Option<f64>,
    /// Pilot PLL.
    pub pilot: PilotReport,
    /// Stereo broadcast (pilot present).
    pub stereo: bool,
    /// De-emphasis τ applied, s.
    pub deemphasis_tau_s: f64,
    /// Audio rate, Hz.
    pub audio_rate_hz: f64,
    /// RDS results (`None` when disabled).
    pub rds: Option<RdsReport>,
    /// RDS timing-metric contrast (≈ 1 without RDS).
    pub rds_timing_contrast: Option<f64>,
}

/// The L−R path of a stereo [`WfmDemod`].
#[derive(Clone, Debug)]
struct Side {
    /// A copy of the mono path's low-pass (same taps, same decimation phase).
    fir: FirDecimator<f32>,
    deemph: Deemphasis,
    out: Vec<f32>,
    /// The pilot PLL was locked at the last sample.
    locked: bool,
    lock_losses: u64,
}

const DEV_BINS: usize = 256;
const DEV_BIN_HZ: f64 = 1000.0;

/// Streaming WFM demodulator.
#[derive(Clone, Debug)]
pub struct WfmDemod {
    config: WfmConfig,
    fs: f64,
    disc: Discriminator,
    pll: PilotPll,
    audio_fir: FirDecimator<f32>,
    deemph: Deemphasis,
    rds: Option<(RdsDemod, RdsDecoder)>,
    /// MPX sample index (from the start of the stream, i.e. `n` at the sample) of the first
    /// sample fed to the RDS demodulator — `None` before the pilot PLL first locks. RDS only
    /// runs once locked (`PilotPll::ever_locked`), so its own bit/group positions start counting
    /// from that first sample, not from stream start; this offset is added back so positions and
    /// timestamps read from the stream's true start (see [`Self::take_rds_groups`]).
    rds_start: Option<u64>,
    audio: Vec<f32>,
    /// The L−R path, when stereo is enabled.
    side: Option<Box<Side>>,
    n: u64,
    mean_sum: f64,
    mean_prev: f64,
    dev_hist: [u64; DEV_BINS],
}

impl WfmDemod {
    /// A demodulator for complex baseband at `mpx_rate_hz` (an integer multiple of the audio
    /// rate is expected; [`MPX_RATE_HZ`] is the receiver's choice).
    pub fn new(config: WfmConfig, mpx_rate_hz: f64) -> Result<Self, hk_dsp::DesignError> {
        let dec = config.audio_decimation.max(1);
        let audio_rate = mpx_rate_hz / dec as f64;
        let taps = lowpass_taps(
            mpx_rate_hz,
            config.audio_passband_hz,
            config.audio_stopband_hz.min(audio_rate / 2.0),
            config.audio_stopband_db,
        )?;
        let rds = match config.rds {
            Some(rc) => Some((
                RdsDemod::new(rc.demod, mpx_rate_hz)?,
                RdsDecoder::new(rc.groups, mpx_rate_hz),
            )),
            None => None,
        };
        Ok(Self {
            fs: mpx_rate_hz,
            disc: Discriminator::new(mpx_rate_hz),
            pll: PilotPll::new(config.pilot, mpx_rate_hz),
            audio_fir: FirDecimator::new(taps, dec),
            deemph: Deemphasis::new(config.deemphasis_tau_s, audio_rate),
            rds,
            rds_start: None,
            audio: Vec::new(),
            side: None,
            n: 0,
            mean_sum: 0.0,
            mean_prev: 0.0,
            dev_hist: [0; DEV_BINS],
            config,
        })
    }

    /// Audio rate, Hz.
    pub fn audio_rate_hz(&self) -> f64 {
        self.fs / self.audio_fir.factor() as f64
    }

    /// MPX index that audio sample 0 represents (the audio filter's delay removed).
    pub fn audio_mpx_offset(&self) -> f64 {
        (self.audio_fir.factor() - 1) as f64 - self.audio_fir.group_delay()
    }

    /// Demodulates contiguous baseband samples.
    /// Also decodes the L−R channel from now on (see the module docs): [`Self::take_side`]
    /// then yields one `S = (L−R)/2` sample per mono sample, aligned with it. Idempotent.
    pub fn enable_stereo(&mut self) {
        if self.side.is_some() {
            return;
        }
        let mut fir = self.audio_fir.clone();
        fir.clear_history();
        self.side = Some(Box::new(Side {
            fir,
            deemph: Deemphasis::new(self.config.deemphasis_tau_s, self.audio_rate_hz()),
            out: Vec::new(),
            locked: false,
            lock_losses: 0,
        }));
    }

    /// Whether the L−R channel is decoded ([`Self::enable_stereo`]).
    pub fn is_stereo(&self) -> bool {
        self.side.is_some()
    }

    /// Stereo only: L−R is being decoded right now (the pilot PLL is locked at the last sample).
    pub fn stereo_locked(&self) -> bool {
        self.side.as_ref().is_some_and(|s| s.locked)
    }

    /// Stereo only: locked → unlocked transitions of the pilot since stereo was enabled.
    pub fn stereo_lock_losses(&self) -> u64 {
        self.side.as_ref().map_or(0, |s| s.lock_losses)
    }

    /// `S = (L−R)/2` samples produced since the last call, one per [`Self::take_audio`] sample
    /// (empty unless stereo is enabled). Zero while the pilot is unlocked.
    pub fn take_side(&mut self) -> Vec<f32> {
        self.side
            .as_mut()
            .map(|s| std::mem::take(&mut s.out))
            .unwrap_or_default()
    }

    pub fn process(&mut self, iq: &[Complex32]) {
        self.audio
            .reserve(iq.len() / self.audio_fir.factor().max(1) + 1);
        let scale = (1.0 / self.config.full_scale_deviation_hz) as f32;
        let mean = self.mean_prev as f32;
        let run_rds = self.rds.is_some();
        for (i, &x) in iq.iter().enumerate() {
            let f = self.disc.push(x);
            let theta = self.pll.step(f);
            if run_rds
                && self.pll.ever_locked()
                && let Some((demod, _)) = self.rds.as_mut()
            {
                self.rds_start.get_or_insert(self.n + i as u64);
                demod.push(f, theta);
            }
            if let Some(a) = self.audio_fir.push(f) {
                self.audio.push(self.deemph.push((a - mean) * scale));
            }
            if let Some(side) = self.side.as_deref_mut() {
                // 2·x·sin 2ωt with sin 2ωt = −sin 2θ = −2 sin θ cos θ (as `stereo_decode`).
                let locked = self.pll.is_locked();
                let v = if locked {
                    let (sn, c) = theta.sin_cos();
                    (-4.0 * f64::from(f - mean) * sn * c) as f32
                } else {
                    0.0
                };
                if side.locked && !locked {
                    side.lock_losses += 1;
                }
                side.locked = locked;
                if let Some(s) = side.fir.push(v) {
                    side.out.push(side.deemph.push(s * scale));
                }
            }
            let bin = ((f - mean).abs() as f64 / DEV_BIN_HZ) as usize;
            self.dev_hist[bin.min(DEV_BINS - 1)] += 1;
            self.mean_sum += f64::from(f);
        }
        self.n += iq.len() as u64;
        if self.n > 0 {
            self.mean_prev = self.mean_sum / self.n as f64;
        }
        if let Some((demod, dec)) = self.rds.as_mut() {
            // `demod`'s own positions count from its first sample (`rds_start`, set above),
            // not from the MPX stream's start; shift them back onto the stream's timeline.
            let offset = self.rds_start.unwrap_or(0) as f64;
            for b in demod.take_bits() {
                dec.push_bit(b.bit, b.position + offset);
            }
        }
    }

    /// Audio produced since the last call (48 kS/s mono, f32, ±1 = full-scale deviation).
    pub fn take_audio(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.audio)
    }

    /// RDS groups parsed since the last call (drains the decoder's buffer; empty without RDS).
    /// Positions are MPX sample indexes from the start of the stream fed to [`Self::process`]
    /// (not from pilot lock, even though RDS only runs once locked). The report's totals are
    /// unaffected.
    pub fn take_rds_groups(&mut self) -> Vec<crate::rds::RdsGroup> {
        self.rds
            .as_mut()
            .map(|(_, dec)| dec.take_groups())
            .unwrap_or_default()
    }

    /// Results so far.
    pub fn report(&self) -> WfmReport {
        let total: u64 = self.dev_hist.iter().sum();
        let peak = (total > 0).then(|| {
            let target = (total as f64 * 0.999).ceil() as u64;
            let mut acc = 0;
            let mut k = DEV_BINS - 1;
            for (i, c) in self.dev_hist.iter().enumerate() {
                acc += c;
                if acc >= target {
                    k = i;
                    break;
                }
            }
            (k + 1) as f64 * DEV_BIN_HZ
        });
        let pilot = self.pll.report();
        WfmReport {
            mpx_rate_hz: self.fs,
            mpx_samples: self.n,
            carrier_offset_hz: self.mean_prev,
            peak_deviation_hz: peak,
            stereo: pilot.present,
            pilot,
            deemphasis_tau_s: self.config.deemphasis_tau_s,
            audio_rate_hz: self.audio_rate_hz(),
            rds: self.rds.as_ref().map(|(_, dec)| dec.report()),
            rds_timing_contrast: self.rds.as_ref().map(|(d, _)| d.timing_contrast()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::f64::consts::TAU;

    use super::*;
    use crate::rds::RDS_BITRATE_BD;
    use crate::rds::group::tests::group_0a_bits;
    use hk_dsp::synth::{Rng, complex_noise};

    /// T-106: `RdsGroup`/`RdsBit` positions must count MPX samples from the start of the stream
    /// fed to [`WfmDemod::process`], not from the sample the pilot PLL first locked (RDS only
    /// runs once locked). Builds a synthetic MPX signal with a silent prefix (no pilot, so RDS
    /// stays off for a while) followed by a real pilot + biphase-modulated RDS subcarrier, FM
    /// modulates it to complex baseband, and checks each decoded group's position lands within a
    /// few symbols of where its first bit was actually transmitted — `prefix_samples +
    /// (bit_index + 1) * sps` — regardless of how long the pilot took to lock.
    #[test]
    fn rds_positions_count_from_stream_start_not_pilot_lock() {
        let fs = MPX_RATE_HZ;
        let pi = 0xC0DEu16;
        let ps = b"HACKRIFF";
        let pty = 10u16;
        let tp = true;

        // Message bits: repeated 0A group cycles (4 PS segments each).
        let repeats = 16;
        let mut bits = Vec::new();
        for _ in 0..repeats {
            for seg in 0..4 {
                bits.extend(group_0a_bits(pi, ps, seg, pty, tp));
            }
        }

        // Differential encoding for transmission: d[0] is an arbitrary reference symbol, then
        // d[k] = d[k-1] ^ bits[k-1] (inverse of the decoder's `b[k] = d[k] ⊕ d[k-1]`).
        let mut d = false;
        let mut symbols = Vec::with_capacity(bits.len() + 1);
        symbols.push(if d { 1.0 } else { -1.0 });
        for &b in &bits {
            d ^= b != 0;
            symbols.push(if d { 1.0 } else { -1.0 });
        }

        let sps = fs / RDS_BITRATE_BD;
        let pilot_hz = 19_000.0_f64;
        let pilot_dev_hz = 6_750.0_f64;
        let sub_hz = 3.0 * pilot_hz;
        let sub_dev_hz = 3_000.0_f64;

        // A silent prefix (no pilot: RDS can't run yet) long enough that pilot lock is clearly
        // not at the stream's start, then the pilot + RDS for as long as it takes to send every
        // symbol plus settling margin.
        let prefix_samples = (0.3 * fs).round() as usize;
        let active_samples = (symbols.len() as f64 * sps).ceil() as usize + fs as usize;

        let mut phase = 0.0_f64;
        let mut iq = Vec::with_capacity(prefix_samples + active_samples);
        for _ in 0..prefix_samples {
            iq.push(Complex32::new(phase.cos() as f32, phase.sin() as f32));
        }
        for n in 0..active_samples {
            let t = n as f64 / fs;
            let sym_f = n as f64 / sps;
            let k = sym_f.floor();
            let frac = sym_f - k;
            let sym = symbols.get(k as usize).copied().unwrap_or(0.0);
            let half = if frac < 0.5 { 1.0 } else { -1.0 };
            let mpx = pilot_dev_hz * (TAU * pilot_hz * t).cos()
                + sub_dev_hz * sym * half * (TAU * sub_hz * t).cos();
            phase += TAU * mpx / fs;
            iq.push(Complex32::new(phase.cos() as f32, phase.sin() as f32));
        }

        let mut wfm = WfmDemod::new(WfmConfig::default(), fs).unwrap();
        let mut groups = Vec::new();
        for chunk in iq.chunks(1 << 15) {
            wfm.process(chunk);
            groups.extend(wfm.take_rds_groups());
        }

        let report = wfm.report();
        let first_lock_samples = report.pilot.first_lock_s.expect("pilot locked") * fs;
        // The lock really did happen well after stream start: otherwise this synthetic signal
        // isn't exercising the bug (pre-fix, positions counted from that lock sample).
        assert!(
            first_lock_samples > prefix_samples as f64 + 500.0,
            "pilot locked implausibly early ({first_lock_samples} samples); test not exercising \
             the pre-lock gap"
        );

        let valid: Vec<_> = groups
            .iter()
            .filter(|g| g.pi == Some(pi) && g.blocks_ok.iter().all(|&ok| ok))
            .collect();
        assert!(
            valid.len() >= 10,
            "{} CRC-valid groups: {groups:?}",
            valid.len()
        );

        // Group k's first bit is message bit 104k, transmitted (symbol index 104k + 1, the +1
        // for the differential reference symbol) at local sample (104k + 1) * sps.
        let tol = 20.0 * sps;
        for g in &valid {
            let bit0 = (g.position - prefix_samples as f64) / sps - 1.0;
            let k = (bit0 / 104.0).round();
            assert!(
                k >= 0.0 && k < repeats as f64 * 4.0,
                "group position {} (bit0 {bit0}, k {k}) outside the transmitted stream \
                 (pilot locked at {first_lock_samples} samples)",
                g.position
            );
            let expected = prefix_samples as f64 + (104.0 * k + 1.0) * sps;
            assert!(
                (g.position - expected).abs() <= tol,
                "group position {} expected within {tol} of {expected} (k {k}); pilot locked at \
                 {first_lock_samples} samples — positions should count from stream start, not \
                 pilot lock",
                g.position
            );
        }
    }

    #[test]
    fn noise_gives_no_pilot_and_no_pi() {
        let mut rng = Rng::new(21);
        let iq = complex_noise(&mut rng, 3 * MPX_RATE_HZ as usize, 1e-3);
        let mut wfm = WfmDemod::new(WfmConfig::default(), MPX_RATE_HZ).unwrap();
        for chunk in iq.chunks(65_536) {
            wfm.process(chunk);
        }
        let r = wfm.report();
        assert!(!r.pilot.present, "{:?}", r.pilot);
        let rds = r.rds.unwrap();
        assert!(rds.pi.is_none() && rds.frame_log.is_empty(), "{rds:?}");
    }
}
