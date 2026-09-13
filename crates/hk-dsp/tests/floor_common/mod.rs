//! Gamma-domain spectrum frames for the noise-floor tests (T-005): each bin is its profile value
//! times a unit-mean `Gamma(K)` variate, exactly the statistics of a `K`-average periodogram of
//! complex Gaussian noise without overlap (as S4's synthetic checks).
#![allow(dead_code)]

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::floor::gamma;
use hk_dsp::synth::Rng;
use hk_dsp::window::{Window, WindowKind};
use hk_dsp::{Resolution, Spectrum, SpectrumFrame};
use hk_model::{SampleTime, Timestamp};

/// Generator of Gamma-domain frames with a running sequence counter.
pub struct GammaFrames {
    pub fs: f64,
    pub bins: usize,
    pub n_avg: u32,
    pub provenance: ProvenanceHandle,
    pub rng: Rng,
    pub seq: u64,
}

impl GammaFrames {
    pub fn new(bins: usize, n_avg: u32, provenance: ProvenanceHandle, seed: u64) -> Self {
        Self {
            fs: provenance.tune.sample_rate_hz,
            bins,
            n_avg,
            provenance,
            rng: Rng::new(seed),
            seq: 0,
        }
    }

    /// Seconds per frame (`K·N/fs`, no overlap).
    pub fn frame_period_s(&self) -> f64 {
        f64::from(self.n_avg) * self.bins as f64 / self.fs
    }

    pub fn empty_frame(&self) -> SpectrumFrame {
        let n = self.bins;
        let metrics = Window::new(WindowKind::Hann, n).metrics();
        let resolution = Resolution {
            window: WindowKind::Hann,
            fft_len: n,
            overlap: 0,
            n_avg: self.n_avg,
            bin_width_hz: self.fs / n as f64,
            rbw_hz: metrics.enbw_bins * self.fs / n as f64,
            window_metrics: metrics,
        };
        SpectrumFrame {
            seq: 0,
            t: SampleTime {
                sample_index: 0,
                host_time: Timestamp::UNIX_EPOCH,
            },
            sample_count: u64::from(self.n_avg) * n as u64,
            provenance: self.provenance.clone(),
            provenance_changed: false,
            discontinuity: Discontinuity::NONE,
            dropped_samples: 0,
            spectrum: Spectrum {
                f_center_hz: self.provenance.tune.center_hz,
                sample_rate_hz: self.fs,
                resolution,
                psd: vec![0.0; n],
                max_hold: Vec::new(),
                min_hold: Vec::new(),
                sk: Vec::new(),
            },
        }
    }

    /// Regenerates `frame` in place (no allocation) with per-bin means `profile`.
    pub fn fill(&mut self, frame: &mut SpectrumFrame, profile: &[f32], flags: Discontinuity) {
        assert_eq!(profile.len(), self.bins);
        for (p, &m) in frame.spectrum.psd.iter_mut().zip(profile) {
            *p = m * gamma::sample_unit_mean(&mut self.rng, self.n_avg) as f32;
        }
        self.stamp(frame, flags);
    }

    /// As [`fill`](Self::fill), with pooled variates (cheap; for long event scenarios, not for
    /// tail statistics).
    pub fn fill_pooled(
        &mut self,
        frame: &mut SpectrumFrame,
        profile: &[f32],
        flags: Discontinuity,
        pool: &mut GammaPool,
    ) {
        assert_eq!(profile.len(), self.bins);
        pool.fill(&mut frame.spectrum.psd, profile);
        self.stamp(frame, flags);
    }

    fn stamp(&mut self, frame: &mut SpectrumFrame, flags: Discontinuity) {
        let index = self.seq * u64::from(self.n_avg) * self.bins as u64;
        frame.seq = self.seq;
        frame.t = SampleTime {
            sample_index: index,
            host_time: Timestamp::from_unix_nanos((index as f64 * 1e9 / self.fs) as i64),
        };
        if frame.provenance != self.provenance {
            frame.provenance = self.provenance.clone();
        }
        frame.spectrum.f_center_hz = self.provenance.tune.center_hz;
        frame.discontinuity = flags;
        self.seq += 1;
    }

    pub fn next(&mut self, profile: &[f32]) -> SpectrumFrame {
        let mut f = self.empty_frame();
        self.fill(&mut f, profile, Discontinuity::NONE);
        f
    }
}

/// 65 536 pre-drawn unit-mean `Gamma(K)` variates, read at a random offset and odd stride per
/// frame.
pub struct GammaPool {
    values: Vec<f32>,
    rng: Rng,
}

impl GammaPool {
    pub fn new(n_avg: u32, seed: u64) -> Self {
        let mut rng = Rng::new(seed);
        let values = (0..1 << 16)
            .map(|_| gamma::sample_unit_mean(&mut rng, n_avg) as f32)
            .collect();
        Self { values, rng }
    }

    pub fn fill(&mut self, psd: &mut [f32], profile: &[f32]) {
        let mask = self.values.len() - 1;
        let start = self.rng.next_u64() as usize & mask;
        let stride = ((self.rng.next_u64() as usize & 0x3ff) << 1) | 1;
        for (i, (p, &m)) in psd.iter_mut().zip(profile).enumerate() {
            *p = m * self.values[(start + i * stride) & mask];
        }
    }
}

/// A flat unit profile with signals 5–200 bins wide at 3–30 dB SNR covering exactly
/// `round(occupancy·bins)` bins (S4 §3.1's recipe).
pub fn occupied_profile(rng: &mut Rng, bins: usize, occupancy: f64) -> Vec<f32> {
    let target = (occupancy * bins as f64).round() as usize;
    if target == 0 {
        return vec![1.0; bins];
    }
    let mean_gap = 102.5 * (1.0 - occupancy) / occupancy;
    loop {
        let mut p = vec![1.0f32; bins];
        let mut starts = Vec::new();
        let mut pos = (rng.unit() * mean_gap) as usize;
        while pos < bins {
            let w = 5 + (rng.unit() * 196.0) as usize;
            let snr_db = 3.0 + 27.0 * rng.unit();
            let level = 1.0 + 10f64.powf(snr_db / 10.0) as f32;
            let end = (pos + w).min(bins);
            p[pos..end].fill(level);
            starts.push(pos..end);
            pos = end + (rng.unit() * 2.0 * mean_gap) as usize;
        }
        let mut count = p.iter().filter(|&&v| v > 1.0).count();
        if count < target {
            continue;
        }
        // Trim signals from the top of the band down to the exact count.
        while count > target {
            let r = starts.pop().expect("signals left");
            let excess = count - target;
            let len = r.len();
            if len <= excess {
                p[r].fill(1.0);
                count -= len;
            } else {
                p[r.end - excess..r.end].fill(1.0);
                count = target;
            }
        }
        return p;
    }
}

pub fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        0.5 * (v[n / 2 - 1] + v[n / 2])
    }
}

pub fn db32(x: f32) -> f64 {
    10.0 * f64::from(x).log10()
}
