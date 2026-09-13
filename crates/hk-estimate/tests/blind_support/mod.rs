//! T-011 test support: S5-matched synthetic generators (`spikes/s5-blind-estimation/synth.py`),
//! 8-bit embedding at a stated in-band SNR, and the extraction → C13 → normalise → C14 chain.
#![allow(dead_code)]

use std::f64::consts::{PI, TAU};

use hk_dsp::IqSample;
use hk_dsp::synth::{Rng, quantize_ci8};
use hk_estimate::blind::{BlindConfig, BlindEstimator, SymbolParameters};
use hk_estimate::{
    ChannelSnippet, Hints, NormalisedSnippet, ParamEstimator, ParameterSet, SnippetConfig,
    SnippetExtractor, SnippetRequest, normalise,
};
use num_complex::{Complex, Complex32};

/// Real HackRF ADC noise at 915 MHz (S5), LSB rms of the complex magnitude.
pub const NOISE_RMS_LSB: f64 = 3.21;

fn gauss(rng: &mut Rng) -> f64 {
    rng.gaussian_pair().0
}

fn conv_same(x: &[f64], h: &[f64]) -> Vec<f64> {
    let (n, m) = (x.len(), h.len());
    let off = (m - 1) / 2;
    (0..n)
        .map(|i| {
            // full[k] = Σ_j x[j]·h[k − j]; same[i] = full[i + off]
            let k = i + off;
            let j0 = k.saturating_sub(m - 1);
            let j1 = k.min(n - 1);
            (j0..=j1).map(|j| x[j] * h[k - j]).sum()
        })
        .collect()
}

fn bits(rng: &mut Rng, nsym: usize, preamble: usize, first: u8) -> Vec<u8> {
    (0..nsym)
        .map(|i| {
            if i < preamble {
                if i % 2 == 0 { first } else { 1 - first }
            } else {
                (rng.next_u64() & 1) as u8
            }
        })
        .collect()
}

/// 2-FSK / GFSK: `preamble` bits of 0101 then random bits; Gaussian BT when `bt` is given.
pub fn gen_fsk(
    rng: &mut Rng,
    rate: f64,
    dev: f64,
    nsym: usize,
    fs: f64,
    bt: Option<f64>,
    preamble: usize,
) -> Vec<Complex32> {
    let sps = fs / rate;
    let b = bits(rng, nsym, preamble, 0);
    let n = (nsym as f64 * sps).ceil() as usize;
    let mut nrz: Vec<f64> = (0..n)
        .map(|i| 2.0 * f64::from(b[((i as f64 / sps) as usize).min(nsym - 1)]) - 1.0)
        .collect();
    if let Some(bt) = bt {
        let span = 4.0;
        let a = (2f64.ln() / 2.0).sqrt() / bt;
        let m = (2.0 * span * sps) as usize + 1;
        let h: Vec<f64> = (0..m)
            .map(|k| {
                let t = (k as f64 - span * sps) / sps;
                (-(PI * t / a).powi(2)).exp()
            })
            .collect();
        let s: f64 = h.iter().sum();
        let h: Vec<f64> = h.iter().map(|v| v / s).collect();
        nrz = conv_same(&nrz, &h);
    }
    let mut ph = 0.0;
    nrz.iter()
        .map(|v| {
            ph += TAU * dev * v / fs;
            Complex32::new(ph.cos() as f32, ph.sin() as f32)
        })
        .collect()
}

/// NRZ OOK with a 1010 preamble and a boxcar edge of sps/8.
pub fn gen_ook(rng: &mut Rng, rate: f64, nsym: usize, fs: f64) -> Vec<Complex32> {
    let sps = fs / rate;
    let b = bits(rng, nsym, 16, 1);
    let n = (nsym as f64 * sps).ceil() as usize;
    let env: Vec<f64> = (0..n)
        .map(|i| f64::from(b[((i as f64 / sps) as usize).min(nsym - 1)]))
        .collect();
    let l = ((sps / 8.0) as usize).max(1);
    let env = conv_same(&env, &vec![1.0 / l as f64; l]);
    env.iter().map(|&v| Complex32::new(v as f32, 0.0)).collect()
}

/// Raised-cosine BPSK (span ±6 symbols); returns the samples and the exact rate `fs / sps`.
pub fn gen_bpsk(rng: &mut Rng, rate: f64, nsym: usize, fs: f64, alpha: f64) -> (Vec<f64>, f64) {
    let sps = (fs / rate).round() as usize;
    let mut up = vec![0.0; nsym * sps];
    for k in 0..nsym {
        up[k * sps] = if rng.next_u64() & 1 == 1 { 1.0 } else { -1.0 };
    }
    let m = 12 * sps + 1;
    let h: Vec<f64> = (0..m)
        .map(|k| {
            let t = (k as f64 - 6.0 * sps as f64) / sps as f64;
            let sinc = |x: f64| {
                if x == 0.0 {
                    1.0
                } else {
                    (PI * x).sin() / (PI * x)
                }
            };
            let den = 1.0 - (2.0 * alpha * t).powi(2);
            if den.abs() < 1e-9 {
                PI / 4.0 * sinc(1.0 / (2.0 * alpha))
            } else {
                sinc(t) * (PI * alpha * t).cos() / den
            }
        })
        .collect();
    (conv_same(&up, &h), fs / sps as f64)
}

pub fn gen_bpsk_c(rng: &mut Rng, rate: f64, nsym: usize, fs: f64) -> (Vec<Complex32>, f64) {
    let (i, r) = gen_bpsk(rng, rate, nsym, fs, 0.35);
    (
        i.iter().map(|&v| Complex32::new(v as f32, 0.0)).collect(),
        r,
    )
}

pub fn gen_qpsk(rng: &mut Rng, rate: f64, nsym: usize, fs: f64) -> (Vec<Complex32>, f64) {
    let (i, r) = gen_bpsk(rng, rate, nsym, fs, 0.35);
    let (q, _) = gen_bpsk(rng, rate, nsym, fs, 0.35);
    let s = 1.0 / 2f64.sqrt();
    (
        i.iter()
            .zip(&q)
            .map(|(a, b)| Complex32::new((a * s) as f32, (b * s) as f32))
            .collect(),
        r,
    )
}

/// Voice-like NBFM: band-passed (300–3000 Hz) noise, peak deviation `dev`.
pub fn gen_nbfm_voice(rng: &mut Rng, dur_s: f64, fs: f64, dev: f64) -> Vec<Complex32> {
    let n = (dur_s * fs) as usize;
    let lp = |fc: f64| -> Vec<f64> {
        let m = 255;
        (0..m)
            .map(|k| {
                let t = k as f64 - 127.0;
                let w = 0.54 - 0.46 * (TAU * k as f64 / (m - 1) as f64).cos();
                let x = 2.0 * fc / fs;
                w * if t == 0.0 {
                    x
                } else {
                    (PI * x * t).sin() / (PI * t)
                }
            })
            .collect()
    };
    let h: Vec<f64> = lp(3000.0)
        .iter()
        .zip(lp(300.0))
        .map(|(a, b)| a - b)
        .collect();
    let noise: Vec<f64> = (0..n).map(|_| gauss(rng)).collect();
    let audio = conv_same(&noise, &h);
    let peak = audio.iter().fold(0.0f64, |m, v| m.max(v.abs())) + 1e-9;
    let mut ph = 0.0;
    audio
        .iter()
        .map(|v| {
            ph += TAU * dev * v / peak / fs;
            Complex32::new(ph.cos() as f32, ph.sin() as f32)
        })
        .collect()
}

/// LoRa-like up-chirps: bandwidth `bw`, spreading factor `sf`, `nsym` symbols.
pub fn gen_chirp(bw: f64, sf: u32, nsym: usize, fs: f64) -> Vec<Complex32> {
    let t_sym = f64::from(1u32 << sf) / bw;
    let n = (t_sym * fs) as usize;
    let one: Vec<Complex32> = (0..n)
        .map(|k| {
            let t = k as f64 / fs;
            let ph = TAU * (-bw / 2.0 * t + bw / (2.0 * t_sym) * t * t);
            Complex32::new(ph.cos() as f32, ph.sin() as f32)
        })
        .collect();
    one.iter().cycle().take(n * nsym).copied().collect()
}

pub fn gen_cw(len: usize, f: f64, fs: f64) -> Vec<Complex32> {
    (0..len)
        .map(|k| {
            let ph = TAU * f * k as f64 / fs;
            Complex32::new(ph.cos() as f32, ph.sin() as f32)
        })
        .collect()
}

/// A burst embedded in 8-bit-quantised noise.
pub struct Embedded {
    pub iq: Vec<Complex<i8>>,
    pub fs: f64,
    pub start: usize,
    pub len: usize,
}

/// `sig` at in-band SNR `snr_db` over `obw_hz` (S / (N0·OBW)), CFO `cfo_hz`, in complex noise of
/// `NOISE_RMS_LSB` rms, between pads of `pad_s`, quantised to ci8 (S5 `embed`).
pub fn embed(
    rng: &mut Rng,
    sig: &[Complex32],
    fs: f64,
    snr_db: f64,
    obw_hz: f64,
    pad_s: f64,
    cfo_hz: f64,
) -> Embedded {
    let npad = (pad_s * fs) as usize;
    let sigma2 = (NOISE_RMS_LSB / 128.0).powi(2);
    let n0 = sigma2 / fs;
    let p_sig = sig.iter().map(|s| f64::from(s.norm_sqr())).sum::<f64>() / sig.len().max(1) as f64;
    let p_sig = if p_sig > 0.0 { p_sig } else { 1.0 };
    let amp = (10f64.powf(snr_db / 10.0) * n0 * obw_hz / p_sig).sqrt();
    let s = (sigma2 / 2.0).sqrt();
    let total = sig.len() + 2 * npad;
    let x: Vec<Complex32> = (0..total)
        .map(|k| {
            let mut v = Complex::new(gauss(rng) * s, gauss(rng) * s);
            if k >= npad && k < npad + sig.len() {
                let ph = TAU * (cfo_hz / fs * k as f64).fract();
                let z = sig[k - npad];
                v += Complex::new(f64::from(z.re), f64::from(z.im))
                    * Complex::new(ph.cos(), ph.sin())
                    * amp;
            }
            Complex32::new(v.re as f32, v.im as f32)
        })
        .collect();
    let (iq, _) = quantize_ci8(&x);
    Embedded {
        iq,
        fs,
        start: npad,
        len: sig.len(),
    }
}

/// OBW99 of a clean signal (Hann Welch, 0.5 % tails).
pub fn clean_obw(sig: &[Complex32], fs: f64) -> f64 {
    let nfft = 8192.min(sig.len().next_power_of_two() / 2);
    let s = hk_dsp::welch(sig, fs, 0.0, &hk_dsp::WelchConfig::new(nfft)).unwrap();
    let p: Vec<f64> = s.psd.iter().map(|&v| f64::from(v)).collect();
    let total: f64 = p.iter().sum();
    let (mut lo, mut acc) = (0, 0.0);
    while acc + p[lo] < 0.005 * total {
        acc += p[lo];
        lo += 1;
    }
    let (mut hi, mut acc) = (p.len() - 1, 0.0);
    while acc + p[hi] < 0.005 * total {
        acc += p[hi];
        hi -= 1;
    }
    (hi - lo + 1) as f64 * fs / nfft as f64
}

/// Everything the chain produced for one box.
pub struct ChainOut {
    pub snip: ChannelSnippet,
    pub params: ParameterSet,
    pub norm: Option<NormalisedSnippet>,
    /// `None` when C13 gave no OBW99 (nothing to normalise: unknown and untrusted).
    pub sym: Option<SymbolParameters>,
}

impl ChainOut {
    pub fn trusted_rate(&self) -> Option<f64> {
        self.sym.as_ref().and_then(|s| s.symbol_rate_bd.value())
    }

    pub fn best_rate(&self) -> Option<f64> {
        self.sym.as_ref().and_then(|s| s.best_candidate_bd())
    }

    pub fn family(&self) -> hk_estimate::Family {
        self.sym
            .as_ref()
            .map_or(hk_estimate::Family::Unknown, |s| s.family)
    }

    pub fn deviation(&self) -> Option<f64> {
        self.sym.as_ref().and_then(|s| s.deviation_hz.value())
    }
}

/// The chain with reusable engines.
pub struct Chain {
    pub blind: BlindEstimator,
    pub c13: ParamEstimator,
    /// Snippet rate floor, Hz (`None`: `samples_per_obw × box bandwidth`).
    pub min_rate_hz: Option<f64>,
}

impl Default for Chain {
    fn default() -> Self {
        Self {
            blind: BlindEstimator::new(BlindConfig::default()),
            c13: ParamEstimator::default(),
            min_rate_hz: None,
        }
    }
}

impl Chain {
    /// Extract `request` from `iq` (stream index 0, tuned `center_hz`, rate `fs`), C13 with
    /// `hints`, normalise for C14, C14.
    pub fn run<T: IqSample>(
        &mut self,
        iq: &[T],
        prov: &hk_core::ProvenanceHandle,
        request: &SnippetRequest,
        hints: &Hints,
    ) -> ChainOut {
        let cfg = self.blind.config().clone();
        let min_rate = self
            .min_rate_hz
            .unwrap_or(cfg.samples_per_obw * request.bandwidth_hz);
        let mut ex = SnippetExtractor::new(SnippetConfig {
            min_rate_hz: min_rate,
            ..Default::default()
        });
        let info = hk_dsp::InputInfo {
            time: hk_model::SampleTime {
                sample_index: 0,
                host_time: hk_model::Timestamp::from_unix_nanos(1_000_000_000),
            },
            discontinuity: hk_core::Discontinuity::STREAM_START,
            dropped_before: 0,
            provenance: prov,
        };
        let snip = ex.extract(info, iq, request).expect("extract");
        let params = self.c13.estimate(&snip, hints);
        let norm = normalise(&snip, &params, &cfg.normalise_config()).ok();
        let sym = Some(self.blind.estimate_snippet(&snip, &params));
        ChainOut {
            snip,
            params,
            norm,
            sym,
        }
    }

    /// An embedded synthetic burst: box = the burst in time, `box_bw` wide at 0 Hz.
    pub fn run_embedded(&mut self, e: &Embedded, box_bw: f64) -> ChainOut {
        let prov = provenance(e.fs);
        let req = SnippetRequest {
            start_index: e.start as u64,
            end_index: (e.start + e.len) as u64,
            center_offset_hz: 0.0,
            bandwidth_hz: box_bw,
        };
        self.run(&e.iq, &prov, &req, &Hints::default())
    }
}

pub fn provenance(fs: f64) -> hk_core::ProvenanceHandle {
    let p: hk_model::Provenance = serde_json::from_value(serde_json::json!({
        "device_id": "synthetic:hk-estimate-t011",
        "tune": {
            "center_hz": 100e6, "sample_rate_hz": fs, "lna_db": 16.0, "vga_db": 20.0,
            "amp_on": false, "bandwidth_hz": fs * 0.75,
        },
        "overload": false, "quantisation_limited": false, "clock_source": "internal",
        "clock_locked": true, "timestamp_method": "synthetic", "timestamp_error_budget_ns": 0,
    }))
    .unwrap();
    hk_core::ProvenanceHandle::new(p)
}

/// One summary line for a result.
pub fn describe(o: &ChainOut) -> String {
    match &o.sym {
        None => format!(
            "C13 abstained (obw {:?}, snr {:?})",
            o.params.obw99_hz.reason(),
            o.params.snr_box_db.reason()
        ),
        Some(s) => format!(
            "rate {:?} (best {:?}, trusted {}, groups {:?}, fit {:?}) fam {:?} conf {:.2} scores \
             o{:.2} f{:.2} b{:.2} q{:.2} dev {:?} snr {:?} obw {:.0} fs {:.0} n {} reasons {:?} \
             lines {:?} cost {} us",
            s.symbol_rate_bd.value(),
            s.best_candidate_bd(),
            s.rate_trusted(),
            s.rate_trust.strong_groups,
            s.transition_fit
                .as_ref()
                .map(|f| (f.jitter_ui, f.segments, f.factor)),
            s.family,
            s.family_confidence,
            s.family_scores.ook,
            s.family_scores.fsk,
            s.family_scores.bpsk,
            s.family_scores.qpsk,
            s.deviation_hz.value(),
            s.snr_ext_db,
            o.params.obw99_hz.value().unwrap_or(f64::NAN),
            s.sample_rate_hz,
            s.samples,
            s.reasons,
            s.lines
                .iter()
                .map(|l| (
                    l.freq_hz.map(|f| f.round()),
                    (l.significance_db * 10.0).round() / 10.0
                ))
                .collect::<Vec<_>>(),
            s.cost_us
        ),
    }
}
