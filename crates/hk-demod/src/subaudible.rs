//! Blind sub-audible squelch identification on an FM channel (T-988, SIGNAL-090): a **CTCSS**
//! tone (the EIA/TIA 50-tone table, 67.0–254.1 Hz) or a **DCS** code (23-bit Golay code words at
//! 134.4 bit/s, normal or inverted), measured from the FM discriminator output alone — never
//! looked up, never pre-tuned to a known repeater's tone.
//!
//! # Chain
//! 1. **Sub-audible band.** The discriminator output (Hz of deviation) is low-passed below
//!    ~300 Hz and decimated to ≈ 1 kS/s in two FIR stages. Only squelch-open samples are kept
//!    (the caller gates), in a rolling window of [`SubaudibleConfig::window_s`].
//! 2. **DCS first.** The window is sliced (two-cluster threshold) at 16 bit phases; every 23-bit
//!    window, first-transmitted bit least significant, is checked against the code-word format
//!    (data bits 9–11 = `001` in transmission order, Golay (23,12) parity with
//!    g(x) = x¹¹+x¹⁰+x⁶+x⁵+x⁴+x²+1 — checked bit-for-bit against sdrtrunk's 105-code table) in
//!    both polarities, and kept only when it is a standard code. The code with the most words at
//!    one phase wins, with at least [`SubaudibleConfig::min_dcs_words`] of them and 40 % of the
//!    words the window can hold. Every standard code's stream is bit-for-bit another standard
//!    code in the other polarity (`023` normal ≡ `047` inverted); the normal reading is reported
//!    and the other named in `aliases`. **Polarity convention (unverified against a live DCS
//!    transmitter):** a `1` bit is a positive frequency deviation.
//! 3. **CTCSS.** A Welch-averaged (1 s Hann segments, 50 % overlap), zero-padded power spectrum;
//!    the strongest line in 55–270 Hz, refined by parabolic interpolation of the log power to
//!    ≈ 0.1 Hz. It must stand [`SubaudibleConfig::min_snr_db`] over the band's median (the noise
//!    guard: averaging is what makes the guard work — a single long FFT has enough bins that pure
//!    noise's best bin regularly clears it). It is snapped to the nearest EIA tone only within
//!    [`SubaudibleConfig::tolerance_hz`]; off the table it is reported raw (`kind: tone`).
//! 4. **Comb guard.** A periodic buzz (a TDMA radio's 60 ms frame, mains hum) is a *comb* of
//!    lines, and whichever member happens to be strongest is not a CTCSS tone. When the lines at
//!    three or more multiples of a sub-multiple `f/k` (not themselves harmonics of `f`) also
//!    clear the guard, the answer is `none`, with the comb's spacing in the reason. The
//!    explorer's window-3 capture at 461.125 MHz is exactly this: a 16.67 Hz comb, whose 100 Hz
//!    and 233.3 Hz members an oracle and an agent each read as a tone.
//! 5. **Second tone.** A second line clearing the guard, more than 8 Hz from the first (past the
//!    analysis window's sidelobes), not its harmonic and within 30 dB of it, is reported too
//!    (`tones[1]`): two tones present are both named.
//!
//! "No tone" is an answer (`kind: none`), distinct from not having looked (`kind: measuring`,
//! below [`SubaudibleConfig::min_analysed_s`] of on-air audio).

use std::collections::{HashMap, VecDeque};

use hk_dsp::{CpuFft, FftBackend};
use hk_model::{DcsCode, Subaudible, SubaudibleKind, SubaudibleTone};
use num_complex::Complex32;

use crate::dsp::{FirDecimator, lowpass_taps};
use crate::receiver::DemodError;

/// Detector id and version (`Subaudible.detector`).
pub const SUBAUDIBLE_DETECTOR: &str = "hk-demod/subaudible@0.1.0";

/// EIA/TIA-603 (RS-220) standard CTCSS tones, Hz.
pub const CTCSS_TONES_HZ: [f64; 50] = [
    67.0, 69.3, 71.9, 74.4, 77.0, 79.7, 82.5, 85.4, 88.5, 91.5, 94.8, 97.4, 100.0, 103.5, 107.2,
    110.9, 114.8, 118.8, 123.0, 127.3, 131.8, 136.5, 141.3, 146.2, 151.4, 156.7, 159.8, 162.2,
    165.5, 167.9, 171.3, 173.8, 177.3, 179.9, 183.5, 186.2, 189.9, 192.8, 196.6, 199.5, 203.5,
    206.5, 210.7, 218.1, 225.7, 229.1, 233.6, 241.8, 250.3, 254.1,
];

/// Standard DCS codes (octal digits as a 9-bit value): the 105 sdrtrunk lists.
pub const DCS_CODES: [u16; 105] = [
    0o023, 0o025, 0o026, 0o031, 0o032, 0o036, 0o043, 0o047, 0o051, 0o053, 0o054, 0o065, 0o071,
    0o072, 0o073, 0o074, 0o114, 0o115, 0o116, 0o122, 0o125, 0o131, 0o132, 0o134, 0o143, 0o145,
    0o152, 0o155, 0o156, 0o162, 0o165, 0o172, 0o174, 0o205, 0o212, 0o223, 0o225, 0o226, 0o243,
    0o244, 0o245, 0o246, 0o251, 0o252, 0o255, 0o261, 0o263, 0o265, 0o266, 0o271, 0o274, 0o306,
    0o311, 0o315, 0o325, 0o331, 0o332, 0o343, 0o346, 0o351, 0o356, 0o364, 0o365, 0o371, 0o411,
    0o412, 0o413, 0o423, 0o431, 0o432, 0o445, 0o446, 0o452, 0o454, 0o455, 0o462, 0o464, 0o465,
    0o466, 0o503, 0o506, 0o516, 0o523, 0o526, 0o532, 0o546, 0o565, 0o606, 0o612, 0o624, 0o627,
    0o631, 0o632, 0o645, 0o654, 0o662, 0o664, 0o703, 0o712, 0o723, 0o731, 0o732, 0o734, 0o743,
    0o754,
];

/// DCS bit rate, bit/s.
pub const DCS_BIT_RATE: f64 = 134.4;
const WORD_BITS: usize = 23;
const WORD_MASK: u32 = (1 << WORD_BITS) - 1;
/// Golay (23,12) generator used by DCS.
const GOLAY_G: u32 = 0xC75;
/// The CTCSS search band, Hz: every standard tone with margin.
const BAND_HZ: (f64, f64) = (55.0, 270.0);
/// Lowest frequency a comb member is looked for at, Hz (below the band: a 16.67 Hz comb's
/// 33 Hz and 50 Hz members are evidence too).
const COMB_LO_HZ: f64 = 20.0;
const COMB_HI_HZ: f64 = 290.0;

/// The 23-bit DCS code word for a 9-bit `code`, first-transmitted bit least significant:
/// bits 0–8 the code, 9–11 the fixed `001`, 12–22 the Golay parity.
pub fn dcs_word(code: u16) -> u32 {
    let data = 0x800 | u32::from(code & 0x1FF);
    let mut r = data << 11;
    for i in (11..WORD_BITS).rev() {
        if (r >> i) & 1 == 1 {
            r ^= GOLAY_G << (i - 11);
        }
    }
    data | ((r & 0x7FF) << 12)
}

/// Octal label of a 9-bit code, e.g. `023`.
pub fn dcs_label(code: u16) -> String {
    format!("{code:03o}")
}

/// Detector settings.
#[derive(Clone, Debug, PartialEq)]
pub struct SubaudibleConfig {
    /// Rolling window of on-air audio analysed, s.
    pub window_s: f64,
    /// On-air audio needed before any conclusion, s.
    pub min_analysed_s: f64,
    /// Noise guard: line power over the band median, dB.
    pub min_snr_db: f64,
    /// CTCSS snap tolerance, Hz.
    pub tolerance_hz: f64,
    /// Fewest DCS code words for an identification.
    pub min_dcs_words: u32,
    /// New on-air audio between two analyses, s (the report is cached in between).
    pub reanalyse_s: f64,
}

impl Default for SubaudibleConfig {
    fn default() -> Self {
        Self {
            window_s: 8.0,
            min_analysed_s: 2.0,
            min_snr_db: 10.0,
            tolerance_hz: 1.0,
            min_dcs_words: 3,
            reanalyse_s: 0.5,
        }
    }
}

/// Streaming sub-audible detector over discriminator output (see the [module docs](self)).
pub struct SubaudibleDetector {
    cfg: SubaudibleConfig,
    stage1: FirDecimator<f32>,
    stage2: FirDecimator<f32>,
    fs_sub: f64,
    buf: VecDeque<f32>,
    cap: usize,
    /// Sub-band samples kept over the detector's life.
    kept: u64,
    kept_at_report: u64,
    cached: Option<Subaudible>,
    /// Whether the previous input sample was gated in (a gap resets nothing but is not bridged
    /// by the filters: history is cleared on re-open so squelch-closed noise never leaks in).
    was_open: bool,
}

impl SubaudibleDetector {
    /// A detector for discriminator samples at `input_rate_hz` (≥ 2 kS/s).
    pub fn new(cfg: SubaudibleConfig, input_rate_hz: f64) -> Result<Self, DemodError> {
        if input_rate_hz.is_nan() || input_rate_hz < 2_000.0 {
            return Err(DemodError::InvalidRequest(format!(
                "sub-audible detector needs ≥ 2 kS/s, got {input_rate_hz}"
            )));
        }
        let f1 = ((input_rate_hz / 6_000.0).floor() as usize).max(1);
        let r1 = input_rate_hz / f1 as f64;
        let f2 = ((r1 / 1_000.0).floor() as usize).max(1);
        let fs_sub = r1 / f2 as f64;
        let stage1 = FirDecimator::new(
            lowpass_taps(input_rate_hz, 300.0, (r1 - 600.0).min(2_500.0), 60.0)?,
            f1,
        );
        let stage2 = FirDecimator::new(lowpass_taps(r1, 280.0, fs_sub * 0.5, 60.0)?, f2);
        let cap = (cfg.window_s * fs_sub).round().max(1.0) as usize;
        Ok(Self {
            cfg,
            stage1,
            stage2,
            fs_sub,
            buf: VecDeque::with_capacity(cap),
            cap,
            kept: 0,
            kept_at_report: 0,
            cached: None,
            was_open: false,
        })
    }

    /// Sub-band sample rate, S/s.
    pub fn sub_rate_hz(&self) -> f64 {
        self.fs_sub
    }

    /// Feeds discriminator samples (Hz). Samples with `open == false` (squelch closed) are
    /// dropped, and the filters restart on the next open one.
    pub fn push(&mut self, disc_hz: &[f32], open: bool) {
        if !open {
            self.was_open = false;
            return;
        }
        if !self.was_open {
            self.stage1.clear_history();
            self.stage2.clear_history();
            self.was_open = true;
        }
        for &x in disc_hz {
            if let Some(y) = self.stage1.push(x)
                && let Some(z) = self.stage2.push(y)
            {
                if self.buf.len() == self.cap {
                    self.buf.pop_front();
                }
                self.buf.push_back(z);
                self.kept += 1;
            }
        }
    }

    /// On-air audio currently in the analysis window, s.
    pub fn analysed_s(&self) -> f64 {
        self.buf.len() as f64 / self.fs_sub
    }

    /// The current conclusion (re-analysed at most every [`SubaudibleConfig::reanalyse_s`] of
    /// new on-air audio).
    pub fn report(&mut self) -> Subaudible {
        let fresh = (self.kept - self.kept_at_report) as f64 / self.fs_sub;
        if let Some(c) = &self.cached
            && fresh < self.cfg.reanalyse_s
        {
            return c.clone();
        }
        let x: Vec<f32> = self.buf.iter().copied().collect();
        let r = analyse(&x, self.fs_sub, &self.cfg);
        self.kept_at_report = self.kept;
        self.cached = Some(r.clone());
        r
    }
}

/// Analyses sub-band samples `x` at `fs` S/s (the whole chain of the [module docs](self)).
pub fn analyse(x: &[f32], fs: f64, cfg: &SubaudibleConfig) -> Subaudible {
    let analysed_s = x.len() as f64 / fs;
    let mut out = Subaudible {
        kind: SubaudibleKind::Measuring,
        analysed_s: (analysed_s * 100.0).round() / 100.0,
        tones: Vec::new(),
        dcs: None,
        reason: None,
        detector: SUBAUDIBLE_DETECTOR.into(),
    };
    if analysed_s < cfg.min_analysed_s || x.len() < 64 {
        return out;
    }
    if let Some(d) = decode_dcs(x, fs, cfg.min_dcs_words) {
        out.kind = SubaudibleKind::Dcs;
        out.dcs = Some(d);
        return out;
    }
    let psd = Psd::welch(x, fs);
    let Some(peak) = psd.peak(BAND_HZ.0, BAND_HZ.1) else {
        out.kind = SubaudibleKind::None;
        out.reason = Some("no sub-audible band measured".into());
        return out;
    };
    let snr1 = psd.snr_db(peak.1);
    if snr1 < cfg.min_snr_db {
        out.kind = SubaudibleKind::None;
        out.reason = Some(format!(
            "no sub-audible line clears the {:.0} dB guard (strongest {:.1} Hz at {:.1} dB)",
            cfg.min_snr_db, peak.0, snr1
        ));
        return out;
    }
    if let Some((f0, members)) = comb(&psd, peak.0, cfg.min_snr_db) {
        out.kind = SubaudibleKind::None;
        out.reason = Some(format!(
            "harmonic comb every {f0:.2} Hz ({members} further lines clear the guard; strongest \
             {:.1} Hz at {snr1:.1} dB): a periodic buzz, not a sub-audible tone",
            peak.0
        ));
        return out;
    }
    let tone = |f: f64, snr: f64| {
        let (t, d) = nearest_ctcss(f, cfg.tolerance_hz).unzip();
        SubaudibleTone {
            measured_hz: (f * 100.0).round() / 100.0,
            snr_db: (snr * 10.0).round() / 10.0,
            table_hz: t,
            delta_hz: d.map(|d| (d * 100.0).round() / 100.0),
            tolerance_hz: cfg.tolerance_hz,
        }
    };
    let first = tone(peak.0, snr1);
    out.kind = if first.table_hz.is_some() {
        SubaudibleKind::Ctcss
    } else {
        SubaudibleKind::Tone
    };
    out.tones.push(first);
    // A second tone: the strongest line away from the first and its harmonics. "Away" is past
    // the 1 s Hann window's sidelobes, which around a strong line sit ~31 dB down at ±2.5 Hz and
    // ~39 dB down at ±3.4 Hz (a 57 dB CTCSS line through the mock SDR showed a false "second
    // tone" at −3.4 Hz with a 3 Hz exclusion): 8 Hz out, and no more than 30 dB under the first.
    let f1 = peak.0;
    let excluded =
        |f: f64| (f - f1).abs() < 8.0 || (2..=5).any(|m| (f - m as f64 * f1).abs() < 1.5);
    if let Some(p2) = psd.peak_where(BAND_HZ.0, BAND_HZ.1, |f| !excluded(f)) {
        let snr2 = psd.snr_db(p2.1);
        if snr2 >= cfg.min_snr_db && snr2 >= snr1 - 30.0 {
            out.tones.push(tone(p2.0, snr2));
        }
    }
    out
}

/// Nearest standard CTCSS tone within `tol` Hz: `(table_hz, measured − table)`.
pub fn nearest_ctcss(f: f64, tol: f64) -> Option<(f64, f64)> {
    let t = CTCSS_TONES_HZ
        .iter()
        .copied()
        .min_by(|a, b| (a - f).abs().total_cmp(&(b - f).abs()))?;
    ((f - t).abs() <= tol).then_some((t, f - t))
}

/// A Welch power spectrum of the sub-band.
struct Psd {
    bin_hz: f64,
    p: Vec<f64>,
    median: f64,
}

impl Psd {
    fn welch(x: &[f32], fs: f64) -> Self {
        let seg = (fs.round() as usize).min(x.len()).max(16);
        let nfft = (seg * 16).next_power_of_two();
        let hop = (seg / 2).max(1);
        let win: Vec<f32> = (0..seg)
            .map(|n| {
                let a = std::f64::consts::TAU * n as f64 / seg as f64;
                (0.5 - 0.5 * a.cos()) as f32
            })
            .collect();
        let mut fft = CpuFft::new(nfft);
        let mut buf = vec![Complex32::default(); nfft];
        let mut p = vec![0.0f64; nfft / 2 + 1];
        let mut count = 0usize;
        let mut start = 0;
        while start + seg <= x.len() {
            let s = &x[start..start + seg];
            let mean = s.iter().map(|&v| f64::from(v)).sum::<f64>() / seg as f64;
            buf.fill(Complex32::default());
            for (k, (&v, &w)) in s.iter().zip(&win).enumerate() {
                buf[k] = Complex32::new((f64::from(v) - mean) as f32 * w, 0.0);
            }
            fft.forward(&mut buf);
            for (acc, c) in p.iter_mut().zip(&buf) {
                *acc += f64::from(c.norm_sqr());
            }
            count += 1;
            start += hop;
        }
        let bin_hz = fs / nfft as f64;
        if count > 0 {
            for v in &mut p {
                *v /= count as f64;
            }
        }
        let (lo, hi) = (
            (BAND_HZ.0 / bin_hz).ceil() as usize,
            ((BAND_HZ.1 / bin_hz).floor() as usize).min(p.len().saturating_sub(1)),
        );
        let mut band: Vec<f64> = p.get(lo..=hi).map(<[f64]>::to_vec).unwrap_or_default();
        band.sort_by(f64::total_cmp);
        let median = band.get(band.len() / 2).copied().unwrap_or(0.0).max(1e-30);
        Self { bin_hz, p, median }
    }

    fn snr_db(&self, power: f64) -> f64 {
        10.0 * (power.max(1e-30) / self.median).log10()
    }

    /// Strongest bin in `[lo, hi]` Hz: `(interpolated frequency, power)`.
    fn peak(&self, lo: f64, hi: f64) -> Option<(f64, f64)> {
        self.peak_where(lo, hi, |_| true)
    }

    fn peak_where(&self, lo: f64, hi: f64, keep: impl Fn(f64) -> bool) -> Option<(f64, f64)> {
        let a = ((lo / self.bin_hz).ceil() as usize).max(1);
        let b = ((hi / self.bin_hz).floor() as usize).min(self.p.len().saturating_sub(2));
        let k = (a..=b)
            .filter(|&k| keep(k as f64 * self.bin_hz))
            .max_by(|&i, &j| self.p[i].total_cmp(&self.p[j]))?;
        let (l, c, r) = (
            self.p[k - 1].max(1e-30).ln(),
            self.p[k].max(1e-30).ln(),
            self.p[k + 1].max(1e-30).ln(),
        );
        let den = l - 2.0 * c + r;
        let frac = if den.abs() > 1e-12 {
            (0.5 * (l - r) / den).clamp(-0.5, 0.5)
        } else {
            0.0
        };
        Some(((k as f64 + frac) * self.bin_hz, self.p[k]))
    }

    /// Strongest power within `±half` Hz of `f`.
    fn line(&self, f: f64, half: f64) -> f64 {
        let a = (((f - half) / self.bin_hz).floor().max(0.0)) as usize;
        let b = (((f + half) / self.bin_hz).ceil() as usize).min(self.p.len() - 1);
        self.p[a..=b].iter().copied().fold(0.0, f64::max)
    }
}

/// The comb guard: `Some((spacing, further members))` when `f1` is one line of a comb.
fn comb(psd: &Psd, f1: f64, min_snr_db: f64) -> Option<(f64, usize)> {
    for k in 2..=8usize {
        let f0 = f1 / k as f64;
        if f0 < 8.0 {
            break;
        }
        let members = (1..)
            .map(|m| m as f64 * f0)
            .take_while(|&f| f <= COMB_HI_HZ)
            .enumerate()
            .filter(|&(i, f)| (i + 1) % k != 0 && f >= COMB_LO_HZ)
            .filter(|&(_, f)| psd.snr_db(psd.line(f, 0.75)) >= min_snr_db)
            .count();
        if members >= 3 {
            return Some((f0, members));
        }
    }
    None
}

/// Words received for one reading of the stream: `((code, inverted), count)`.
type Vote = ((u16, bool), u32);

/// Slices, frames and votes DCS code words (see the [module docs](self)).
pub fn decode_dcs(x: &[f32], fs: f64, min_words: u32) -> Option<DcsCode> {
    let sp = fs / DCS_BIT_RATE;
    let nbits = ((x.len() as f64 - 1.0) / sp).floor() as usize;
    if nbits < 2 * WORD_BITS {
        return None;
    }
    let thr = two_cluster_threshold(x);
    let std: HashMap<u16, ()> = DCS_CODES.iter().map(|&c| (c, ())).collect();
    let valid = |w: u32| -> Option<u16> {
        if (w >> 9) & 7 != 0b100 {
            return None;
        }
        let code = (w & 0x1FF) as u16;
        (dcs_word(code) == w && std.contains_key(&code)).then_some(code)
    };
    let possible = (nbits - WORD_BITS + 1) as f64 / WORD_BITS as f64;
    let mut best: Option<(u32, Vec<Vote>)> = None;
    const PHASES: usize = 16;
    for ph in 0..PHASES {
        let t0 = (ph as f64 + 0.5) / PHASES as f64 * sp;
        let bits: Vec<u32> = (0..nbits)
            .map_while(|i| {
                let t = t0 + i as f64 * sp;
                let j = t.floor() as usize;
                let fr = (t - j as f64) as f32;
                let v = *x.get(j)? * (1.0 - fr) + x.get(j + 1).copied().unwrap_or(0.0) * fr;
                Some(u32::from(v > thr))
            })
            .collect();
        if bits.len() < WORD_BITS {
            continue;
        }
        let mut votes: HashMap<(u16, bool), u32> = HashMap::new();
        let mut w: u32 = 0;
        for (i, &b) in bits.iter().enumerate() {
            // First-transmitted bit least significant: shift the new bit in at the top.
            w = (w >> 1) | (b << (WORD_BITS - 1));
            if i + 1 < WORD_BITS {
                continue;
            }
            if let Some(c) = valid(w) {
                *votes.entry((c, false)).or_default() += 1;
            }
            if let Some(c) = valid(!w & WORD_MASK) {
                *votes.entry((c, true)).or_default() += 1;
            }
        }
        let top = votes.values().copied().max().unwrap_or(0);
        if top > best.as_ref().map_or(0, |b| b.0) {
            let mut v: Vec<_> = votes.into_iter().collect();
            v.sort_by(|a, b| {
                b.1.cmp(&a.1)
                    .then(a.0.1.cmp(&b.0.1))
                    .then(a.0.0.cmp(&b.0.0))
            });
            best = Some((top, v));
        }
    }
    let (top, votes) = best?;
    if top < min_words || f64::from(top) < 0.4 * possible {
        return None;
    }
    // The readings of one stream differ in count only by where the window starts and ends; among
    // them the normal reading leads (the stream cannot say which polarity was meant).
    let mut near: Vec<_> = votes
        .iter()
        .filter(|(_, n)| f64::from(*n) >= 0.8 * f64::from(top))
        .collect();
    near.sort_by(|a, b| {
        a.0.1
            .cmp(&b.0.1)
            .then(b.1.cmp(&a.1))
            .then(a.0.0.cmp(&b.0.0))
    });
    let ((code, inverted), words) = *near[0];
    let aliases = near[1..]
        .iter()
        .map(|((c, inv), _)| format!("{}{}", dcs_label(*c), if *inv { "I" } else { "N" }))
        .collect();
    Some(DcsCode {
        code: dcs_label(code),
        polarity: if inverted { "inverted" } else { "normal" }.into(),
        words,
        aliases,
    })
}

/// The midpoint of two clusters' means (a 1-D two-means), robust to an unbalanced code word.
fn two_cluster_threshold(x: &[f32]) -> f32 {
    let n = x.len().max(1) as f64;
    let mut t = x.iter().map(|&v| f64::from(v)).sum::<f64>() / n;
    for _ in 0..8 {
        let (mut hi, mut nh, mut lo, mut nl) = (0.0, 0usize, 0.0, 0usize);
        for &v in x {
            let v = f64::from(v);
            if v > t {
                hi += v;
                nh += 1;
            } else {
                lo += v;
                nl += 1;
            }
        }
        if nh == 0 || nl == 0 {
            break;
        }
        t = 0.5 * (hi / nh as f64 + lo / nl as f64);
    }
    t as f32
}

#[cfg(test)]
mod tests {
    use std::f64::consts::TAU;

    use super::*;

    const FS: f64 = 48_000.0;

    /// A deterministic noise source (xorshift, uniform ±0.5 scaled).
    struct Noise(u64);
    impl Noise {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64 - 0.5
        }
    }

    /// Discriminator output (Hz) of an NBFM channel: a 1 kHz "voice" at 2.5 kHz deviation, the
    /// sub-audible signal `sub(t)` (Hz), and discriminator noise.
    fn disc(seconds: f64, seed: u64, noise_hz: f64, sub: impl Fn(f64) -> f64) -> Vec<f32> {
        let mut rng = Noise(seed);
        (0..(seconds * FS) as usize)
            .map(|n| {
                let t = n as f64 / FS;
                (2_500.0 * (TAU * 1_000.0 * t).sin() + sub(t) + noise_hz * rng.next()) as f32
            })
            .collect()
    }

    fn dcs_level(code: u16, inverted: bool, dev: f64) -> impl Fn(f64) -> f64 {
        let w = dcs_word(code);
        move |t: f64| {
            let i = (t * DCS_BIT_RATE).floor() as usize % WORD_BITS;
            let b = (w >> i) & 1 == 1;
            if b != inverted { dev } else { -dev }
        }
    }

    fn run(x: &[f32]) -> Subaudible {
        let mut d = SubaudibleDetector::new(SubaudibleConfig::default(), FS).unwrap();
        for c in x.chunks(960) {
            d.push(c, true);
        }
        d.report()
    }

    #[test]
    fn dcs_words_match_the_published_table() {
        // sdrtrunk's DCSCode values are these words bit-reversed (first-transmitted bit MSB):
        // N023 = 6557239, N754 = 1822594.
        let rev = |w: u32| w.reverse_bits() >> (32 - WORD_BITS);
        assert_eq!(rev(dcs_word(0o023)), 6_557_239);
        assert_eq!(rev(dcs_word(0o754)), 1_822_594);
        assert_eq!(rev(dcs_word(0o025)), 5_508_971);
    }

    #[test]
    fn three_ctcss_tones_are_identified_to_a_tenth_of_a_hertz() {
        for (k, tone) in [67.0, 131.8, 233.6].into_iter().enumerate() {
            let x = disc(6.0, 11 + k as u64, 3_000.0, |t| {
                600.0 * (TAU * tone * t).sin()
            });
            let r = run(&x);
            assert_eq!(r.kind, SubaudibleKind::Ctcss, "{tone}: {r:?}");
            let t = &r.tones[0];
            assert_eq!(t.table_hz, Some(tone), "{r:?}");
            assert!((t.measured_hz - tone).abs() <= 0.1, "{tone}: {r:?}");
            assert_eq!(r.tones.len(), 1, "one tone present: {r:?}");
        }
    }

    #[test]
    fn two_dcs_codes_are_identified_with_their_alias() {
        for (code, alias) in [(0o023, "047I"), (0o754, "116I")] {
            let x = disc(6.0, 21, 3_000.0, dcs_level(code, false, 600.0));
            let r = run(&x);
            assert_eq!(r.kind, SubaudibleKind::Dcs, "{code:o}: {r:?}");
            let d = r.dcs.as_ref().unwrap();
            assert_eq!(d.code, dcs_label(code), "{r:?}");
            assert_eq!(d.polarity, "normal");
            assert!(d.words >= 20, "{r:?}");
            assert_eq!(d.aliases, vec![alias.to_owned()], "{r:?}");
        }
    }

    #[test]
    fn an_inverted_code_reads_as_its_normal_alias_and_names_itself() {
        let x = disc(6.0, 23, 3_000.0, dcs_level(0o047, true, 600.0));
        let d = run(&x).dcs.unwrap();
        assert_eq!((d.code.as_str(), d.polarity.as_str()), ("023", "normal"));
        assert_eq!(d.aliases, vec!["047I".to_owned()]);
    }

    #[test]
    fn toneless_and_noise_only_report_no_tone_not_absent() {
        for seed in 0..12 {
            let x = disc(8.0, 100 + seed, 3_000.0, |_| 0.0);
            let r = run(&x);
            assert_eq!(r.kind, SubaudibleKind::None, "seed {seed}: {r:?}");
            assert!(r.tones.is_empty() && r.dcs.is_none(), "{r:?}");
            assert!(r.reason.is_some());
        }
        // Pure discriminator noise, no voice (an unmodulated or noise-only channel).
        let mut rng = Noise(7);
        let x: Vec<f32> = (0..(8.0 * FS) as usize)
            .map(|_| (4_000.0 * rng.next()) as f32)
            .collect();
        assert_eq!(run(&x).kind, SubaudibleKind::None);
    }

    #[test]
    fn too_little_audio_is_measuring() {
        let x = disc(1.0, 3, 3_000.0, |t| 600.0 * (TAU * 100.0 * t).sin());
        assert_eq!(run(&x).kind, SubaudibleKind::Measuring);
    }

    #[test]
    fn a_harmonic_comb_is_not_a_tone() {
        // A 60 ms periodic buzz (a TDMA frame): lines every 16.67 Hz, the strongest at 100 Hz.
        let x = disc(8.0, 5, 3_000.0, |t| {
            (1..=16)
                .map(|m| {
                    let a = if m == 6 { 160.0 } else { 90.0 };
                    a * (TAU * m as f64 * 50.0 / 3.0 * t + m as f64).sin()
                })
                .sum()
        });
        let r = run(&x);
        assert_eq!(r.kind, SubaudibleKind::None, "{r:?}");
        assert!(r.reason.as_deref().unwrap().contains("comb"), "{r:?}");
    }

    #[test]
    fn a_strong_clean_tone_has_no_sidelobe_second_tone() {
        let x = disc(8.0, 29, 60.0, |t| 600.0 * (TAU * 131.8 * t).sin());
        let r = run(&x);
        assert_eq!(r.kind, SubaudibleKind::Ctcss, "{r:?}");
        assert_eq!(
            r.tones.len(),
            1,
            "a window sidelobe is not a second tone: {r:?}"
        );
    }

    #[test]
    fn two_tones_are_both_reported() {
        let x = disc(8.0, 9, 3_000.0, |t| {
            600.0 * (TAU * 100.0 * t).sin() + 400.0 * (TAU * 162.2 * t).sin()
        });
        let r = run(&x);
        assert_eq!(r.kind, SubaudibleKind::Ctcss, "{r:?}");
        let got: Vec<_> = r.tones.iter().map(|t| t.table_hz).collect();
        assert_eq!(got, vec![Some(100.0), Some(162.2)], "{r:?}");
    }

    #[test]
    fn an_off_table_tone_is_reported_raw() {
        let x = disc(6.0, 13, 3_000.0, |t| 600.0 * (TAU * 120.0 * t).sin());
        let r = run(&x);
        assert_eq!(r.kind, SubaudibleKind::Tone, "{r:?}");
        assert_eq!(r.tones[0].table_hz, None);
        assert!((r.tones[0].measured_hz - 120.0).abs() <= 0.1, "{r:?}");
    }

    #[test]
    fn squelch_closed_audio_is_not_analysed() {
        let mut d = SubaudibleDetector::new(SubaudibleConfig::default(), FS).unwrap();
        let x = disc(6.0, 17, 3_000.0, |t| 600.0 * (TAU * 100.0 * t).sin());
        for c in x.chunks(960) {
            d.push(c, false);
        }
        assert_eq!(d.analysed_s(), 0.0);
        assert_eq!(d.report().kind, SubaudibleKind::Measuring);
    }
}
