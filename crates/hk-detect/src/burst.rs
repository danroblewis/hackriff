//! Short-burst time-domain detector (T-075): bursts shorter than the STFT path resolves (ADS-B
//! squitters, OOK key-fob bursts), found blind on the raw ci8 samples beside the STFT path.
//!
//! # Method
//!
//! 1. **Power.** `|x|²` (integer ci8 units) summed over sub-blocks of `sub_window_s` (8 µs); the
//!    test statistic is the sliding sum of the last `sub_blocks` (4 → a 32 µs window), stepped
//!    every sub-block. Per sample: one square and one ring write; no allocation.
//! 2. **Noise.** Every non-overlapping window's power enters a ring of `noise_windows` (2048 ≈
//!    66 ms); after 16, 32, 64, … windows and then every `noise_refresh` windows the median is
//!    taken (a copy + `select_nth`, no allocation) and turned into a per-sample mean with the Gamma(`W`) median factor. The
//!    median ignores bursts up to half the span, so a squitter-dense stream keeps its floor.
//! 3. **Thresholds** (Gamma, [`hk_dsp::floor::gamma`]): a window of `W` complex-Gaussian samples
//!    is `Gamma(W)` in units of its mean. Onset: `T_on = mean_threshold(W, pfa)` with
//!    `pfa = false_alarm_rate_hz · sub_window_s` (one test per sub-block, so the target rate is an
//!    upper bound: overlapping windows are correlated). Extent: a sub-block is *hot* above
//!    `mean_threshold(S, extent_pfa)`.
//! 4. **Burst.** The onset window's earliest hot sub-block starts the burst (timing resolution
//!    one sub-block). It ends at its last hot sub-block once `hold_s` (300 µs) of sub-blocks
//!    stayed cold. A burst longer than `max_duration_s` is a long signal — the STFT path owns it —
//!    and is suppressed until it ends, so long signals are never detected twice.
//! 5. **Emission** (per burst, not per sample): a token bucket of `max_rate_hz` (stream time)
//!    caps the rows; a short periodogram over the burst's first samples (≤ `max_fft_segments` ×
//!    an FFT of bins ≈ `target_bin_hz`) gives a coarse centre (peak bin, parabolic interpolation)
//!    and a 99 % bandwidth of the power above the noise. Records are untracked
//!    [`DetectionRecord`]s: µs start/end, peak and mean SNR over the sub-block noise, peak level in
//!    dBFS (sub-block mean power, full scale 1 = ci8 / 128), clip count, `marginal` below 10 dB.
//!
//! # Transitions
//!
//! Any block discontinuity flag, a provenance change or a non-contiguous sample index restarts
//! the detector (noise estimate included): an open burst is dropped (`dropped_transition`).

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::floor::gamma::{mean_quantile, mean_threshold};
use hk_dsp::{CpuFft, FftBackend};
use hk_model::{Detection, DetectionFlags, DetectionId, SampleTime, SurveyId, TimeRange};
use num_complex::{Complex, Complex32};

use crate::alpha::db;
use crate::record::{Candidate, CloseReason, DetectionRecord};

/// Windows before the first noise estimate (then at every doubling up to `noise_refresh`).
const NOISE_FIRST: usize = 16;

/// Detector version prefix of every burst detection (`detector_version`).
pub const BURST_DETECTOR: &str = "hk-detect/burst@0.1.0";

/// [`BurstDetector`] settings.
#[derive(Clone, Debug, PartialEq)]
pub struct BurstConfig {
    /// Sub-block length, s (timing resolution and hop).
    pub sub_window_s: f64,
    /// Sub-blocks per detection window.
    pub sub_blocks: usize,
    /// Target onset false-alarm rate on noise, per second (sets the window Pfa).
    pub false_alarm_rate_hz: f64,
    /// Pfa of the per-sub-block extent ("hot") test.
    pub extent_pfa: f64,
    /// Cold time that ends a burst, s.
    pub hold_s: f64,
    /// Longest emitted burst, s; longer ones are long signals (the STFT path's).
    pub max_duration_s: f64,
    /// Windows in the median noise ring.
    pub noise_windows: usize,
    /// Windows between noise refreshes (and before the first estimate).
    pub noise_refresh: usize,
    /// Emitted bursts per second (stream time), token bucket.
    pub max_rate_hz: f64,
    /// Token-bucket depth, bursts.
    pub rate_burst: f64,
    /// Target FFT bin width for the coarse centre, Hz.
    pub target_bin_hz: f64,
    /// Largest FFT.
    pub max_fft_len: usize,
    /// Periodogram segments per burst.
    pub max_fft_segments: usize,
    /// Peak SNR below which a burst is `marginal`, dB.
    pub marginal_snr_db: f64,
}

impl Default for BurstConfig {
    fn default() -> Self {
        Self {
            sub_window_s: 8e-6,
            sub_blocks: 4,
            false_alarm_rate_hz: 0.01,
            extent_pfa: 1e-3,
            hold_s: 300e-6,
            max_duration_s: 5e-3,
            noise_windows: 2048,
            noise_refresh: 256,
            max_rate_hz: 500.0,
            rate_burst: 500.0,
            target_bin_hz: 5e3,
            max_fft_len: 4096,
            max_fft_segments: 8,
            marginal_snr_db: 10.0,
        }
    }
}

/// Counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BurstStats {
    /// Samples pushed.
    pub samples: u64,
    /// Bursts that ended (emitted or dropped).
    pub bursts: u64,
    /// Detections emitted.
    pub emitted: u64,
    /// Bursts longer than `max_duration_s` (left to the STFT path).
    pub dropped_long: u64,
    /// Bursts over the rate cap.
    pub dropped_rate: u64,
    /// Bursts dropped because their power filled the window, so they had no measurable centre or
    /// bandwidth (T-237), or their samples had left the ring.
    pub dropped_unlocalised: u64,
    /// Open bursts cut by a transition.
    pub dropped_transition: u64,
    /// Detector restarts (transitions).
    pub resets: u64,
    /// Noise-estimate refreshes.
    pub noise_updates: u64,
}

#[derive(Clone, Copy, Debug)]
struct Active {
    start: u64,
    last_hot_end: u64,
    quiet: u32,
    peak_sub: u64,
    sum_hot: u64,
    samples_hot: u64,
    clips_hot: u32,
    sum: u64,
    samples: u64,
    clips: u32,
    long: bool,
}

/// The short-burst detector (see the module docs). Push every ci8 block of the stream.
pub struct BurstDetector {
    cfg: BurstConfig,
    survey_id: SurveyId,
    // Segment state.
    prov: Option<ProvenanceHandle>,
    anchor: SampleTime,
    fs: f64,
    s_len: usize,
    w_len: usize,
    hold_subs: u32,
    max_samples: u64,
    thr_on: f64,
    thr_sub: f64,
    med_factor: f64,
    segment: u64,
    seg_start: u64,
    next_sample: u64,
    sub_idx: u64,
    fill: usize,
    acc: u64,
    clips_acc: u32,
    hist: Vec<(u64, u32)>,
    noise: Vec<f64>,
    noise_pos: usize,
    noise_len: usize,
    since_refresh: usize,
    scratch: Vec<f64>,
    sigma2: Option<f64>,
    active: Option<Active>,
    ring: Vec<Complex<i8>>,
    ring_head: u64,
    tokens: f64,
    token_sample: u64,
    fft: Option<CpuFft>,
    fft_buf: Vec<Complex32>,
    psd: Vec<f64>,
    version: String,
    stats: BurstStats,
}

impl BurstDetector {
    /// A detector for `survey_id`. Buffers are sized at the first block (and at a rate change).
    pub fn new(survey_id: SurveyId, cfg: BurstConfig) -> Self {
        assert!(cfg.sub_blocks >= 1 && cfg.noise_windows >= cfg.noise_refresh.max(1));
        Self {
            hist: vec![(0, 0); cfg.sub_blocks],
            noise: vec![0.0; cfg.noise_windows],
            scratch: vec![0.0; cfg.noise_windows],
            tokens: cfg.rate_burst,
            cfg,
            survey_id,
            prov: None,
            anchor: SampleTime {
                sample_index: 0,
                host_time: hk_model::Timestamp::UNIX_EPOCH,
            },
            fs: 0.0,
            s_len: 1,
            w_len: 1,
            hold_subs: 1,
            max_samples: 0,
            thr_on: f64::INFINITY,
            thr_sub: f64::INFINITY,
            med_factor: 1.0,
            segment: 0,
            seg_start: 0,
            next_sample: 0,
            sub_idx: 0,
            fill: 0,
            acc: 0,
            clips_acc: 0,
            noise_pos: 0,
            noise_len: 0,
            since_refresh: 0,
            sigma2: None,
            active: None,
            ring: Vec::new(),
            ring_head: 0,
            token_sample: 0,
            fft: None,
            fft_buf: Vec::new(),
            psd: Vec::new(),
            version: String::new(),
            stats: BurstStats::default(),
        }
    }

    /// Settings.
    pub fn config(&self) -> &BurstConfig {
        &self.cfg
    }

    /// Counters.
    pub fn stats(&self) -> BurstStats {
        self.stats
    }

    /// Mean noise power per sample, full scale 1 (`None` before the first estimate).
    pub fn noise_power(&self) -> Option<f64> {
        self.sigma2.map(|s| s / (128.0 * 128.0))
    }

    /// The `detector_version` of the current segment (empty before the first block).
    pub fn detector_version(&self) -> &str {
        &self.version
    }

    fn reset(&mut self, time: SampleTime, prov: &ProvenanceHandle) {
        if self.active.take().is_some() {
            self.stats.dropped_transition += 1;
        }
        self.stats.resets += 1;
        self.segment += 1;
        let fs = prov.tune.sample_rate_hz;
        if fs != self.fs {
            self.fs = fs;
            let c = &self.cfg;
            self.s_len = ((c.sub_window_s * fs).round() as usize).max(1);
            self.w_len = self.s_len * c.sub_blocks;
            let sub_s = self.s_len as f64 / fs;
            self.hold_subs = ((c.hold_s / sub_s).ceil() as u32).max(1);
            self.max_samples = (c.max_duration_s * fs).round() as u64;
            let pfa = (c.false_alarm_rate_hz * sub_s).clamp(1e-300, 0.5);
            self.thr_on = mean_threshold(self.w_len as f64, pfa);
            self.thr_sub = mean_threshold(self.s_len as f64, c.extent_pfa);
            self.med_factor = mean_quantile(self.w_len as f64, 0.5);
            let fft_len = ((fs / c.target_bin_hz).max(16.0) as usize)
                .next_power_of_two()
                .min(c.max_fft_len.max(16));
            if self.fft.as_ref().is_none_or(|f| f.len() != fft_len) {
                self.fft = Some(CpuFft::new(fft_len));
                self.fft_buf = vec![Complex32::default(); fft_len];
                self.psd = vec![0.0; fft_len];
            }
            let need = self.max_samples as usize
                + (self.hold_subs as usize + 2 * c.sub_blocks + 2) * self.s_len
                + c.max_fft_segments * fft_len;
            let cap = need.next_power_of_two();
            if self.ring.len() != cap {
                self.ring = vec![Complex::default(); cap];
            }
            self.version = format!(
                "{BURST_DETECTOR};sub={:.1}us;win={};far={}/s;pfa={pfa:.1e};ext={:.0e};hold={:.0}us;max={:.1}ms;noise=median{};fft={fft_len}",
                sub_s * 1e6,
                c.sub_blocks,
                c.false_alarm_rate_hz,
                c.extent_pfa,
                c.hold_s * 1e6,
                c.max_duration_s * 1e3,
                c.noise_windows,
            );
        }
        self.prov = Some(prov.clone());
        self.seg_start = time.sample_index;
        self.next_sample = time.sample_index;
        self.sub_idx = 0;
        self.fill = 0;
        self.acc = 0;
        self.clips_acc = 0;
        self.hist.iter_mut().for_each(|h| *h = (0, 0));
        self.noise_pos = 0;
        self.noise_len = 0;
        self.since_refresh = 0;
        self.sigma2 = None;
    }

    /// Processes one contiguous ci8 block starting at `time` under `prov`. Bursts that ended
    /// inside it are handed to `emit`.
    pub fn push(
        &mut self,
        time: SampleTime,
        discontinuity: Discontinuity,
        prov: &ProvenanceHandle,
        samples: &[Complex<i8>],
        emit: &mut dyn FnMut(DetectionRecord),
    ) {
        if samples.is_empty() {
            return;
        }
        let changed = self
            .prov
            .as_ref()
            .is_none_or(|p| p.id() != prov.id() || p != prov);
        if changed || discontinuity != Discontinuity::NONE || time.sample_index != self.next_sample
        {
            self.reset(time, prov);
        }
        self.anchor = time;
        self.stats.samples += samples.len() as u64;
        self.next_sample = time.sample_index + samples.len() as u64;
        let mut i = 0;
        while i < samples.len() {
            let take = (self.s_len - self.fill).min(samples.len() - i);
            // The ring holds the samples up to the processing position (not the block end).
            self.write_ring(time.sample_index + i as u64, &samples[i..i + take]);
            let mut acc = 0u64;
            let mut clips = 0u32;
            for x in &samples[i..i + take] {
                let (re, im) = (i32::from(x.re), i32::from(x.im));
                acc += (re * re + im * im) as u64;
                clips += u32::from(x.re == 127 || x.re == -128 || x.im == 127 || x.im == -128);
            }
            self.acc += acc;
            self.clips_acc += clips;
            self.fill += take;
            i += take;
            if self.fill == self.s_len {
                self.step(emit);
            }
        }
    }

    /// Ends the stream: an open burst that is not long is emitted.
    pub fn finish(&mut self, emit: &mut dyn FnMut(DetectionRecord)) {
        if let Some(a) = self.active.take() {
            self.close(a, emit);
        }
    }

    fn write_ring(&mut self, first: u64, samples: &[Complex<i8>]) {
        let cap = self.ring.len();
        let src = if samples.len() > cap {
            &samples[samples.len() - cap..]
        } else {
            samples
        };
        let start = first + (samples.len() - src.len()) as u64;
        self.ring_head = first + samples.len() as u64;
        let at = (start % cap as u64) as usize;
        let head = (cap - at).min(src.len());
        self.ring[at..at + head].copy_from_slice(&src[..head]);
        self.ring[..src.len() - head].copy_from_slice(&src[head..]);
    }

    fn step(&mut self, emit: &mut dyn FnMut(DetectionRecord)) {
        let k = self.sub_idx;
        let nb = self.cfg.sub_blocks;
        let sub = (self.acc, self.clips_acc);
        self.hist[(k % nb as u64) as usize] = sub;
        self.acc = 0;
        self.clips_acc = 0;
        self.fill = 0;
        self.sub_idx += 1;
        let s = self.s_len as u64;
        let end = self.seg_start + (k + 1) * s;
        if k + 1 < nb as u64 {
            return;
        }
        let win: u64 = self.hist.iter().map(|h| h.0).sum();
        if (k + 1) % nb as u64 == 0 {
            self.noise[self.noise_pos] = win as f64;
            self.noise_pos = (self.noise_pos + 1) % self.noise.len();
            self.noise_len = (self.noise_len + 1).min(self.noise.len());
            self.since_refresh += 1;
            // First estimates at 16, 32, 64, … windows (0.5 ms at 32 µs), then every refresh.
            let due = if self.noise_len < self.cfg.noise_refresh {
                self.noise_len >= NOISE_FIRST && self.noise_len.is_power_of_two()
            } else {
                self.since_refresh >= self.cfg.noise_refresh
            };
            if due {
                self.since_refresh = 0;
                let n = self.noise_len;
                self.scratch[..n].copy_from_slice(&self.noise[..n]);
                let (_, med, _) = self.scratch[..n].select_nth_unstable_by(n / 2, f64::total_cmp);
                self.sigma2 = Some((*med / self.med_factor / self.w_len as f64).max(1e-3));
                self.stats.noise_updates += 1;
            }
        }
        let Some(s2) = self.sigma2 else { return };
        let sub_level = self.thr_sub * s2 * s as f64;
        let hot = sub.0 as f64 > sub_level;
        match self.active.as_mut() {
            None => {
                if (win as f64) <= self.thr_on * s2 * self.w_len as f64 {
                    return;
                }
                // Oldest to newest sub-block of the onset window.
                let first = k + 1 - nb as u64;
                // The extent test defines a burst: a crossing with no hot sub-block is a noise
                // fluctuation spread over the window (the joint test lowers the false-alarm rate).
                let Some(j) = (0..nb as u64)
                    .find(|&j| self.hist[((first + j) % nb as u64) as usize].0 as f64 > sub_level)
                else {
                    return;
                };
                let mut a = Active {
                    start: self.seg_start + (first + j) * s,
                    last_hot_end: end,
                    quiet: 0,
                    peak_sub: 0,
                    sum_hot: 0,
                    samples_hot: 0,
                    clips_hot: 0,
                    sum: 0,
                    samples: 0,
                    clips: 0,
                    long: false,
                };
                for jj in j..nb as u64 {
                    let (p, c) = self.hist[((first + jj) % nb as u64) as usize];
                    a.peak_sub = a.peak_sub.max(p);
                    a.sum += p;
                    a.samples += s;
                    a.clips += c;
                }
                if !hot {
                    a.quiet = 1;
                    // The last hot sub-block of the window (there is one: the window crossed).
                    if let Some(jh) = (j..nb as u64 - 1).rev().find(|&jj| {
                        self.hist[((first + jj) % nb as u64) as usize].0 as f64 > sub_level
                    }) {
                        a.last_hot_end = self.seg_start + (first + jh + 1) * s;
                        a.quiet = (nb as u64 - 1 - jh) as u32;
                    }
                }
                (a.sum_hot, a.samples_hot, a.clips_hot) = (a.sum, a.samples, a.clips);
                self.active = Some(a);
            }
            Some(a) => {
                a.sum += sub.0;
                a.samples += s;
                a.clips += sub.1;
                a.peak_sub = a.peak_sub.max(sub.0);
                if hot {
                    a.last_hot_end = end;
                    a.quiet = 0;
                    (a.sum_hot, a.samples_hot, a.clips_hot) = (a.sum, a.samples, a.clips);
                } else {
                    a.quiet += 1;
                }
                if end - a.start > self.max_samples {
                    a.long = true;
                }
                if a.quiet >= self.hold_subs {
                    let a = *a;
                    self.active = None;
                    self.close(a, emit);
                }
            }
        }
    }

    fn close(&mut self, a: Active, emit: &mut dyn FnMut(DetectionRecord)) {
        self.stats.bursts += 1;
        let dur = a.last_hot_end.saturating_sub(a.start);
        if a.long || dur > self.max_samples {
            self.stats.dropped_long += 1;
            return;
        }
        // Token bucket in stream time.
        let refill = (a.start.saturating_sub(self.token_sample)) as f64 / self.fs;
        self.token_sample = self.token_sample.max(a.start);
        self.tokens = (self.tokens + refill * self.cfg.max_rate_hz).min(self.cfg.rate_burst);
        if self.tokens < 1.0 {
            self.stats.dropped_rate += 1;
            return;
        }
        self.tokens -= 1.0;
        let (Some(s2), Some(prov)) = (self.sigma2, self.prov.clone()) else {
            return;
        };
        let fs = self.fs;
        let tuned = prov.tune.center_hz;
        // T-237: a burst whose 99 % bandwidth fills the window — or whose samples have left the
        // ring — has no measurable centre or bandwidth. The old fallback invented one
        // (`(0.0, fs)`: centred on the tune, the whole window occupied). On the mock's noise fill
        // the onset test crosses at its designed Pfa, the "burst" is noise, its power above the
        // noise is spread over every bin, and the 99 % width then spans the window: T-231 saw
        // 0.9375 of a 3 MHz window, and this run 0.9678 and 0.9941, all at ≈ −1 dB peak SNR.
        // Report nothing rather than claim 3 MHz of occupancy that was never measured.
        let Some((offset, obw)) = self.coarse_spectrum(a.start, dur, s2) else {
            self.stats.dropped_unlocalised += 1;
            return;
        };
        let s = self.s_len as f64;
        let excess = |p: f64| db((p / s2 - 1.0).max(1e-3));
        let snr_peak_db = excess(a.peak_sub as f64 / s);
        let snr_mean_db = excess(a.sum_hot as f64 / a.samples_hot.max(1) as f64);
        let clipped = a.clips_hot > 0 || prov.overload;
        let flags = DetectionFlags {
            clipped,
            marginal: snr_peak_db < self.cfg.marginal_snr_db || prov.quantisation_limited,
            ..DetectionFlags::default()
        };
        let f_center = tuned + offset;
        let detection = Detection {
            id: DetectionId::new(),
            survey_id: self.survey_id,
            time: TimeRange::new(
                self.anchor.time_of(a.start, fs),
                self.anchor.time_of(a.last_hot_end, fs),
            ),
            f_center_hz: f_center,
            obw_hz: obw,
            xdb_bandwidth_hz: None,
            xdb_level_db: None,
            snr_peak_db,
            snr_mean_db,
            peak_level_dbfs: db(a.peak_sub as f64 / s / (128.0 * 128.0)) as f32,
            peak_level_dbm: None,
            sk: None,
            clip_count: a.clips_hot,
            detector_version: self.version.clone(),
            provenance_ref: prov.id(),
            flags,
        };
        self.stats.emitted += 1;
        emit(DetectionRecord {
            detection,
            provenance: prov,
            segment: self.segment,
            bins: 0..0,
            f_lo_hz: f_center - obw / 2.0,
            f_hi_hz: f_center + obw / 2.0,
            frames: 0..0,
            samples: a.start..a.last_hot_end,
            pixels: 0,
            close: CloseReason::Ended,
            continues: false,
            candidate: Candidate::Unconfirmed,
            image: None,
            spur_harmonic_hz: None,
            merged_boxes: 1,
            inconclusive: false,
        });
    }

    /// `(centre offset from tune, 99 % bandwidth)` Hz from a periodogram of the burst's first
    /// samples; `None` when they left the ring.
    fn coarse_spectrum(&mut self, start: u64, dur: u64, s2: f64) -> Option<(f64, f64)> {
        let fft = self.fft.as_mut()?;
        let n = fft.len();
        let cap = self.ring.len() as u64;
        let total = dur.min((self.cfg.max_fft_segments * n) as u64).max(1);
        if self.ring_head.saturating_sub(start) > cap {
            return None;
        }
        self.psd.iter_mut().for_each(|p| *p = 0.0);
        let mut used = 0u64;
        while used < total {
            let len = (total - used).min(n as u64) as usize;
            for (i, b) in self.fft_buf.iter_mut().enumerate() {
                *b = if i < len {
                    let x = self.ring[((start + used + i as u64) % cap) as usize];
                    Complex32::new(f32::from(x.re), f32::from(x.im))
                } else {
                    Complex32::default()
                };
            }
            fft.forward(&mut self.fft_buf);
            for (p, x) in self.psd.iter_mut().zip(&self.fft_buf) {
                *p += f64::from(x.norm_sqr());
            }
            used += len as u64;
        }
        let bin_hz = self.fs / n as f64;
        let shifted = |k: usize| (k + n / 2) % n; // FFT index of fftshift position k
        let (kmax, _) = (0..n)
            .map(|k| (k, self.psd[shifted(k)]))
            .max_by(|a, b| a.1.total_cmp(&b.1))?;
        let at = |k: isize| self.psd[shifted(k.clamp(0, n as isize - 1) as usize)];
        let (l, c, r) = (
            at(kmax as isize - 1),
            at(kmax as isize),
            at(kmax as isize + 1),
        );
        let den = l - 2.0 * c + r;
        let frac = if den.abs() > 0.0 {
            (0.5 * (l - r) / den).clamp(-0.5, 0.5)
        } else {
            0.0
        };
        let offset = (kmax as f64 + frac - (n / 2) as f64) * bin_hz;
        // 99 % of the power above the expected noise (E|X|² = samples · σ² per bin).
        let noise_bin = used as f64 * s2;
        let excess = |k: usize| (self.psd[shifted(k)] - noise_bin).max(0.0);
        let sum: f64 = (0..n).map(excess).sum();
        if sum <= 0.0 {
            return Some((offset, bin_hz));
        }
        let (mut cum, mut lo, mut hi) = (0.0, 0, n - 1);
        let mut lo_set = false;
        for k in 0..n {
            cum += excess(k);
            if !lo_set && cum >= 0.005 * sum {
                lo = k;
                lo_set = true;
            }
            if cum >= 0.995 * sum {
                hi = k;
                break;
            }
        }
        // A short burst legitimately fills the window (a 120 µs squitter at 2.4 Msps is wider than
        // the span), so the width alone says nothing about whether this was a signal: T-237 tried
        // rejecting on it and lost every ADS-B squitter at 3 and 6 dB.
        Some((offset, ((hi + 1).saturating_sub(lo)) as f64 * bin_hz))
    }
}
