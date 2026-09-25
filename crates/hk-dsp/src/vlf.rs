//! VLF/LF science analyses (T-277): SID flare amplitude tracking (SPACE-001), the
//! transmitter-phase measurement behind PROP-019, and the broadband/sferic front of SPACE-041.
//!
//! These are pure analyses over real samples. **The HackRF cannot receive them** (16–30 kHz is
//! below its 1 MHz floor): SPACE-001, SPACE-041 and PROP-019 stay `needs-accessory` (VLF
//! receiver + soundcard, or an upconverter) and nothing here claims them reachable without it.
//! [`VLF_ACCESSORY_MIN_HZ`] is the honest tuner floor callers can gate on.

use std::f64::consts::PI;

/// Lowest frequency the base HackRF front end tunes; anything below needs the accessory.
pub const VLF_ACCESSORY_MIN_HZ: f64 = 1.0e6;

/// True when `carrier_hz` is only receivable through an accessory.
pub fn needs_accessory(carrier_hz: f64) -> bool {
    carrier_hz < VLF_ACCESSORY_MIN_HZ
}

/// One block-averaged amplitude/phase measurement of a narrowband transmitter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VlfPoint {
    /// Block start, seconds from the first sample.
    pub t_s: f64,
    pub amplitude: f32,
    /// Unwrapped phase, radians (continuous across blocks).
    pub phase_rad: f64,
}

/// One block-averaged point from a [`CarrierTracker`], addressed by absolute sample index so a
/// streaming caller can place it on its own capture-time axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrackPoint {
    /// Absolute index of the block's first sample.
    pub start_index: u64,
    pub amplitude: f32,
    /// Unwrapped phase, radians (continuous across blocks, and across gaps: the mixer is
    /// referenced to the absolute sample index, so a gap does not reset it).
    pub phase_rad: f64,
}

/// Streaming coherent tracker of one carrier: mix by `e^{-jωn}` referenced to the **absolute**
/// sample index `n`, boxcar-average `block` samples, unwrap the phase. [`track_carrier`] is the
/// one-shot form over a buffer starting at index 0.
///
/// A forward jump in the index (a stream gap) discards the partial block and restarts block
/// alignment at the new index; it never fabricates samples across the gap.
#[derive(Debug, Clone)]
pub struct CarrierTracker {
    fs: f64,
    carrier_hz: f64,
    block: usize,
    acc_re: f64,
    acc_im: f64,
    count: usize,
    block_start: u64,
    next_index: u64,
    prev: Option<f64>,
    offset: f64,
}

impl CarrierTracker {
    /// A tracker whose first block starts at absolute index `start_index`. `block` must be > 0.
    pub fn new(fs: f64, carrier_hz: f64, block: usize, start_index: u64) -> Self {
        Self {
            fs,
            carrier_hz,
            block: block.max(1),
            acc_re: 0.0,
            acc_im: 0.0,
            count: 0,
            block_start: start_index,
            next_index: start_index,
            prev: None,
            offset: 0.0,
        }
    }

    /// The frequency this tracker mixes at, Hz.
    pub fn carrier_hz(&self) -> f64 {
        self.carrier_hz
    }

    /// Samples per point.
    pub fn block(&self) -> usize {
        self.block
    }

    /// Feeds `x`, whose first sample has absolute index `start_index`, appending finished points
    /// to `out`. Samples before the tracker's next expected index (an overlap) are skipped.
    pub fn push(&mut self, start_index: u64, x: &[f32], out: &mut Vec<TrackPoint>) {
        let mut x = x;
        let mut idx = start_index;
        if idx < self.next_index {
            let skip = (self.next_index - idx).min(x.len() as u64) as usize;
            x = &x[skip..];
            idx += skip as u64;
        }
        if x.is_empty() {
            return;
        }
        if idx > self.next_index {
            // A gap: drop the partial block, realign at the new index.
            self.acc_re = 0.0;
            self.acc_im = 0.0;
            self.count = 0;
            self.block_start = idx;
        }
        let w = -2.0 * PI * self.carrier_hz / self.fs;
        let (rot_im, rot_re) = w.sin_cos();
        let mut i = 0usize;
        while i < x.len() {
            // Exact phasor at the start of each run; the recurrence stays within one block, so
            // its drift is bounded by `block` rotations.
            let n0 = idx + i as u64;
            let ph = (w * n0 as f64).rem_euclid(2.0 * PI);
            let (mut p_im, mut p_re) = ph.sin_cos();
            let run = (self.block - self.count).min(x.len() - i);
            for &s in &x[i..i + run] {
                let s = f64::from(s);
                self.acc_re += s * p_re;
                self.acc_im += s * p_im;
                let re = p_re * rot_re - p_im * rot_im;
                p_im = p_re * rot_im + p_im * rot_re;
                p_re = re;
            }
            self.count += run;
            i += run;
            if self.count == self.block {
                let scale = 2.0 / self.block as f64;
                let (re, im) = (self.acc_re * scale, self.acc_im * scale);
                let raw = im.atan2(re);
                if let Some(prev) = self.prev {
                    let d = raw + self.offset - prev;
                    self.offset -= 2.0 * PI * (d / (2.0 * PI)).round();
                }
                let phase = raw + self.offset;
                self.prev = Some(phase);
                out.push(TrackPoint {
                    start_index: self.block_start,
                    amplitude: (re * re + im * im).sqrt() as f32,
                    phase_rad: phase,
                });
                self.acc_re = 0.0;
                self.acc_im = 0.0;
                self.count = 0;
                self.block_start += self.block as u64;
            }
        }
        self.next_index = idx + x.len() as u64;
    }
}

/// Track amplitude and unwrapped phase of a carrier at `carrier_hz` in real samples at `fs`,
/// one point per `block` samples (coherent mix + boxcar average).
pub fn track_carrier(x: &[f32], fs: f64, carrier_hz: f64, block: usize) -> Vec<VlfPoint> {
    if block == 0 {
        return Vec::new();
    }
    let mut tracker = CarrierTracker::new(fs, carrier_hz, block, 0);
    let mut pts = Vec::new();
    tracker.push(0, x, &mut pts);
    pts.into_iter()
        .map(|p| VlfPoint {
            t_s: p.start_index as f64 / fs,
            amplitude: p.amplitude,
            phase_rad: p.phase_rad,
        })
        .collect()
}

/// A narrowband carrier found blindly in the receiver band.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CarrierPeak {
    /// Interpolated peak frequency, Hz.
    pub carrier_hz: f64,
    /// Peak bin power over the band's median bin power, dB.
    pub snr_db: f32,
}

/// Blind carrier discovery (the exploration-first half of SPACE-001/SPACE-041: which
/// transmitters are audible is **measured**, never looked up): a Hann-windowed power spectrum of
/// the longest power-of-two prefix of `x` (capped at 2^20 samples), local maxima at least
/// `min_snr_db` over the median bin power between `min_hz` and Nyquist, strongest first, at most
/// `max_carriers`, separated by at least 8 bins, each refined by a parabola on log power.
///
/// Digital silence (a median of zero) answers nothing rather than calling every non-zero bin a
/// carrier.
pub fn find_carriers(
    x: &[f32],
    fs: f64,
    min_hz: f64,
    min_snr_db: f32,
    max_carriers: usize,
) -> Vec<CarrierPeak> {
    if x.len() < 1024 || fs <= 0.0 || max_carriers == 0 {
        return Vec::new();
    }
    let n = 1usize << (usize::BITS - 1 - x.len().leading_zeros()).min(20);
    let fft = rustfft::FftPlanner::<f64>::new().plan_fft_forward(n);
    let mut buf: Vec<num_complex::Complex64> = x[..n]
        .iter()
        .enumerate()
        .map(|(i, &s)| {
            let w = 0.5 - 0.5 * (2.0 * PI * i as f64 / n as f64).cos();
            num_complex::Complex64::new(f64::from(s) * w, 0.0)
        })
        .collect();
    fft.process(&mut buf);
    let half = n / 2;
    let p: Vec<f64> = buf[..half].iter().map(|c| c.norm_sqr()).collect();
    let k_lo = ((min_hz.max(0.0) * n as f64 / fs).ceil() as usize).max(2);
    let k_hi = half.saturating_sub(2);
    if k_hi <= k_lo + 2 {
        return Vec::new();
    }
    let mut band: Vec<f64> = p[k_lo..k_hi].to_vec();
    band.sort_by(f64::total_cmp);
    let noise = band[band.len() / 2];
    if noise <= 0.0 {
        return Vec::new();
    }
    let thr = noise * 10f64.powf(f64::from(min_snr_db) / 10.0);
    let mut cand: Vec<usize> = (k_lo..k_hi)
        .filter(|&k| p[k] > thr && p[k] > p[k - 1] && p[k] >= p[k + 1])
        .collect();
    cand.sort_by(|&a, &b| p[b].total_cmp(&p[a]));
    let mut picked: Vec<usize> = Vec::new();
    for k in cand {
        if picked.len() == max_carriers {
            break;
        }
        if picked.iter().all(|&q| q.abs_diff(k) >= 8) {
            picked.push(k);
        }
    }
    picked
        .into_iter()
        .map(|k| {
            let (a, b, c) = (p[k - 1].ln(), p[k].ln(), p[k + 1].ln());
            let den = a - 2.0 * b + c;
            let d = if den.abs() > 0.0 {
                (0.5 * (a - c) / den).clamp(-0.5, 0.5)
            } else {
                0.0
            };
            CarrierPeak {
                carrier_hz: (k as f64 + d) * fs / n as f64,
                snr_db: (10.0 * (p[k] / noise).log10()) as f32,
            }
        })
        .collect()
}

/// A sudden amplitude change (flare onset/recovery candidate).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AmplitudeStep {
    pub t_s: f64,
    /// Fractional change (after − before) / before.
    pub relative_change: f32,
}

/// Find steps where the mean amplitude of the `win` points after a point differs from the
/// `win` before by more than `min_rel`. Adjacent hits are collapsed to the strongest.
pub fn detect_amplitude_steps(pts: &[VlfPoint], win: usize, min_rel: f32) -> Vec<AmplitudeStep> {
    let amp: Vec<f64> = pts.iter().map(|p| f64::from(p.amplitude)).collect();
    windowed_steps(&amp, win, |b, a| {
        (b > 0.0 && ((a - b) / b).abs() >= f64::from(min_rel)).then(|| (a - b) / b)
    })
    .into_iter()
    .map(|(i, r)| AmplitudeStep {
        t_s: pts[i].t_s,
        relative_change: r as f32,
    })
    .collect()
}

/// A sudden phase change of a tracked carrier (PROP-019: a reflection-height change).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PhaseStep {
    pub t_s: f64,
    /// Phase after minus phase before, radians, with the carrier's linear phase drift removed.
    pub dphi_rad: f64,
}

/// The per-point phase drift of a track, radians: the median of successive differences. A
/// tracker mixing a hair off the true carrier sees a linear phase ramp; the median ignores the
/// one or two increments a real step lands in.
pub fn phase_drift_per_point(pts: &[VlfPoint]) -> f64 {
    let mut d: Vec<f64> = pts
        .windows(2)
        .map(|w| w[1].phase_rad - w[0].phase_rad)
        .collect();
    if d.is_empty() {
        return 0.0;
    }
    d.sort_by(f64::total_cmp);
    d[d.len() / 2]
}

/// Find phase steps of at least `min_rad` (window means of `win` points either side, after
/// removing [`phase_drift_per_point`]). Adjacent hits are collapsed to the strongest.
pub fn detect_phase_steps(pts: &[VlfPoint], win: usize, min_rad: f64) -> Vec<PhaseStep> {
    let drift = phase_drift_per_point(pts);
    let resid: Vec<f64> = pts
        .iter()
        .enumerate()
        .map(|(i, p)| p.phase_rad - drift * i as f64)
        .collect();
    windowed_steps(&resid, win, |b, a| {
        ((a - b).abs() >= min_rad).then_some(a - b)
    })
    .into_iter()
    .map(|(i, d)| PhaseStep {
        t_s: pts[i].t_s,
        dphi_rad: d,
    })
    .collect()
}

/// Windowed before/after comparison: `hit(mean_before, mean_after)` scores index `i`; runs of
/// adjacent hits collapse to the one with the largest |score|.
fn windowed_steps(
    v: &[f64],
    win: usize,
    hit: impl Fn(f64, f64) -> Option<f64>,
) -> Vec<(usize, f64)> {
    if win == 0 || v.len() < 2 * win {
        return Vec::new();
    }
    let mean = |s: &[f64]| s.iter().sum::<f64>() / s.len() as f64;
    let mut out: Vec<(usize, f64)> = Vec::new();
    let mut last: Option<usize> = None;
    let mut best: Option<(usize, f64)> = None;
    for i in win..=v.len() - win {
        let Some(r) = hit(mean(&v[i - win..i]), mean(&v[i..i + win])) else {
            continue;
        };
        if last.is_some_and(|l| i > l + 1) {
            out.extend(best.take());
        }
        if best.is_none_or(|(_, br)| r.abs() > br.abs()) {
            best = Some((i, r));
        }
        last = Some(i);
    }
    out.extend(best);
    out
}

const C_M_S: f64 = 299_792_458.0;

/// PROP-019: change in D-region reflection height (km) implied by a phase change `dphi_rad`
/// of a `carrier_hz` transmitter over a ground path of `path_km`, single-hop flat-earth model
/// (sky path = 2·sqrt((d/2)² + h²)). With track_carrier's e^{-jωt} convention a delay τ gives φ = −ωτ, so a phase ADVANCE (Δφ>0) = shorter path = LOWER reflection (the flare signature).
/// Requires a GPS-disciplined receiver for the phase to be meaningful.
pub fn reflection_height_change_km(
    dphi_rad: f64,
    carrier_hz: f64,
    path_km: f64,
    base_height_km: f64,
) -> f64 {
    let lambda_km = C_M_S / carrier_hz / 1000.0;
    let dpath = -dphi_rad / (2.0 * PI) * lambda_km;
    let half = path_km / 2.0;
    let p0 = 2.0 * (half * half + base_height_km * base_height_km).sqrt();
    let h1 = (((p0 + dpath) / 2.0).powi(2) - half * half).max(0.0).sqrt();
    h1 - base_height_km
}

/// Lowest sferic threshold, in full-scale units (about −120 dBFS): below it a "sferic" is
/// numerical noise, not an impulse.
pub const SFERIC_MIN_THRESHOLD: f32 = 1e-6;

/// One impulsive broadband event (SPACE-041).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sferic {
    /// Index of the first sample over threshold.
    pub index: usize,
    /// Samples from `index` to the last over-threshold sample within the refractory window.
    pub len: usize,
    /// Largest |x| in that extent.
    pub peak: f32,
    /// The threshold it crossed.
    pub threshold: f32,
}

/// The sferic threshold for `x`: `k` × the median absolute value of its **non-zero** samples,
/// floored at [`SFERIC_MIN_THRESHOLD`]. `None` when every sample is exactly zero.
///
/// The median runs over non-zero samples because a muted or gated soundcard delivers runs of
/// exact zeros: with more than half of them the plain median is 0, the threshold 0, and every
/// non-zero sample a "sferic" (the T-277 review finding).
pub fn sferic_threshold(x: &[f32], k: f32) -> Option<f32> {
    let mut a: Vec<f32> = x.iter().map(|v| v.abs()).filter(|v| *v > 0.0).collect();
    if a.is_empty() {
        return None;
    }
    let mid = a.len() / 2;
    let (_, med, _) = a.select_nth_unstable_by(mid, f32::total_cmp);
    Some((k * *med).max(SFERIC_MIN_THRESHOLD))
}

/// SPACE-041 sferic front: events where |x| exceeds [`sferic_threshold`], with `dead` samples of
/// refractory time after each hit (an event's extent runs to its last over-threshold sample
/// inside that window).
pub fn detect_sferic_events(x: &[f32], k: f32, dead: usize) -> Vec<Sferic> {
    let Some(thr) = sferic_threshold(x, k) else {
        return Vec::new();
    };
    let dead = dead.max(1);
    let mut out = Vec::new();
    let mut i = 0;
    while i < x.len() {
        if x[i].abs() > thr {
            let end = (i + dead).min(x.len());
            let mut last = i;
            let mut peak = 0.0f32;
            for (j, v) in x[i..end].iter().enumerate() {
                let a = v.abs();
                if a > thr {
                    last = i + j;
                }
                peak = peak.max(a);
            }
            out.push(Sferic {
                index: i,
                len: last - i + 1,
                peak,
                threshold: thr,
            });
            i += dead;
        } else {
            i += 1;
        }
    }
    out
}

/// SPACE-041 sferic front: sample indices of [`detect_sferic_events`].
pub fn detect_sferics(x: &[f32], k: f32, dead: usize) -> Vec<usize> {
    detect_sferic_events(x, k, dead)
        .into_iter()
        .map(|s| s.index)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(
        fs: f64,
        f: f64,
        n: usize,
        amp: impl Fn(usize) -> f32,
        ph: impl Fn(usize) -> f64,
    ) -> Vec<f32> {
        (0..n)
            .map(|i| amp(i) * (2.0 * PI * f * i as f64 / fs + ph(i)).cos() as f32)
            .collect()
    }

    #[test]
    fn hackrf_cannot_reach_vlf() {
        assert!(needs_accessory(24_000.0));
        assert!(!needs_accessory(100e6));
    }

    #[test]
    fn space_001_flare_step_found() {
        let fs = 48_000.0;
        let x = tone(
            fs,
            24_000.0 - 1500.0,
            48_000 * 20,
            |i| if i < 48_000 * 10 { 1.0 } else { 1.5 },
            |_| 0.3,
        );
        let pts = track_carrier(&x, fs, 22_500.0, 4800);
        let s = detect_amplitude_steps(&pts, 3, 0.2);
        assert_eq!(s.len(), 1);
        assert!((s[0].t_s - 10.0).abs() < 0.5);
        assert!((s[0].relative_change - 0.5).abs() < 0.1);
    }

    #[test]
    fn prop_019_phase_advance_means_lower_reflection() {
        let fs = 48_000.0;
        let x = tone(
            fs,
            20_000.0,
            48_000 * 4,
            |_| 1.0,
            |i| if i < 48_000 * 2 { 0.0 } else { 1.0 },
        );
        let pts = track_carrier(&x, fs, 20_000.0, 4800);
        let d = pts.last().unwrap().phase_rad - pts[0].phase_rad;
        assert!(
            (d - 1.0).abs() < 0.05,
            "{d} {:?}",
            pts.iter().map(|p| p.phase_rad).collect::<Vec<_>>()
        );
        let dh = reflection_height_change_km(d, 20_000.0, 2000.0, 70.0);
        assert!((dh + 19.9).abs() < 0.5, "{dh}");
        assert!(reflection_height_change_km(-d, 20_000.0, 2000.0, 70.0) > 0.0);
    }

    #[test]
    fn space_041_sferics_counted() {
        let mut x = vec![0.01f32; 10_000];
        for p in [1000, 4000, 7000] {
            x[p] = 1.0;
            x[p + 1] = -0.9;
        }
        assert_eq!(detect_sferics(&x, 20.0, 100), vec![1000, 4000, 7000]);
    }

    #[test]
    fn space_041_muted_soundcard_does_not_make_every_sample_a_sferic() {
        // 60 % exact zeros (a gated soundcard), low noise elsewhere, three real impulses. The
        // plain median would be 0 and every noise sample a sferic.
        let mut x = vec![0.0f32; 10_000];
        for (i, v) in x.iter_mut().enumerate() {
            if i % 5 >= 3 {
                *v = if i % 2 == 0 { 0.01 } else { -0.012 };
            }
        }
        for p in [1003, 4003, 7003] {
            x[p] = 1.0;
        }
        assert_eq!(detect_sferics(&x, 20.0, 100), vec![1003, 4003, 7003]);
        // All silence: nothing, not everything.
        assert!(detect_sferics(&[0.0; 1000], 20.0, 10).is_empty());
        assert_eq!(sferic_threshold(&[0.0; 10], 20.0), None);
        // A floor even for denormal-level noise.
        assert!(sferic_threshold(&[1e-12; 10], 20.0).unwrap() >= SFERIC_MIN_THRESHOLD);
    }

    #[test]
    fn sferic_events_carry_extent_and_peak() {
        let mut x = vec![0.01f32; 2000];
        x[500] = 0.5;
        x[503] = -0.9;
        let e = detect_sferic_events(&x, 20.0, 50);
        assert_eq!(e.len(), 1);
        assert_eq!((e[0].index, e[0].len), (500, 4));
        assert!((e[0].peak - 0.9).abs() < 1e-6);
    }

    #[test]
    fn streaming_tracker_matches_one_shot_and_survives_a_gap() {
        let fs = 48_000.0;
        let x = tone(fs, 18_000.0, 48_000, |_| 0.7, |_| 0.4);
        let whole = track_carrier(&x, fs, 18_000.0, 4800);
        let mut t = CarrierTracker::new(fs, 18_000.0, 4800, 0);
        let mut pts = Vec::new();
        for (ci, c) in x.chunks(1234).enumerate() {
            t.push((ci * 1234) as u64, c, &mut pts);
        }
        assert_eq!(pts.len(), whole.len());
        for (a, b) in pts.iter().zip(&whole) {
            assert!((f64::from(a.amplitude) - f64::from(b.amplitude)).abs() < 1e-4);
            assert!((a.phase_rad - b.phase_rad).abs() < 1e-4);
        }
        // A gap: the partial block is dropped, the phase stays referenced to the absolute index.
        let mut t = CarrierTracker::new(fs, 18_000.0, 4800, 0);
        let mut pts = Vec::new();
        t.push(0, &x[..7000], &mut pts);
        t.push(20_000, &x[20_000..], &mut pts);
        assert_eq!(pts[0].start_index, 0);
        assert_eq!(pts[1].start_index, 20_000);
        assert!((pts[1].phase_rad - pts[0].phase_rad).abs() < 1e-3);
        assert!((pts[1].amplitude - 0.7).abs() < 0.01);
    }

    #[test]
    fn carriers_are_found_blind_and_silence_finds_none() {
        let fs = 48_000.0;
        let mut x = tone(fs, 19_812.3, 1 << 18, |_| 0.2, |_| 0.0);
        let y = tone(fs, 23_400.0, 1 << 18, |_| 0.05, |_| 1.0);
        let mut seed = 7u32;
        for (a, b) in x.iter_mut().zip(y) {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *a += b + ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.02;
        }
        let c = find_carriers(&x, fs, 1000.0, 20.0, 8);
        assert_eq!(c.len(), 2, "{c:?}");
        assert!((c[0].carrier_hz - 19_812.3).abs() < 0.05, "{c:?}");
        assert!((c[1].carrier_hz - 23_400.0).abs() < 0.05, "{c:?}");
        assert!(c[0].snr_db > c[1].snr_db);
        assert!(find_carriers(&vec![0.0; 1 << 16], fs, 1000.0, 20.0, 8).is_empty());
    }

    #[test]
    fn prop_019_phase_step_found_despite_a_mistuned_tracker() {
        // Tracking 0.03 Hz off the carrier ramps the phase ~0.019 rad per point; the 0.8 rad
        // advance at 6 s must still come out as one step of ~0.8 rad.
        let fs = 48_000.0;
        let x = tone(
            fs,
            20_000.03,
            48_000 * 12,
            |_| 1.0,
            |i| if i < 48_000 * 6 { 0.0 } else { 0.8 },
        );
        let pts = track_carrier(&x, fs, 20_000.0, 4800);
        let s = detect_phase_steps(&pts, 10, 0.3);
        assert_eq!(s.len(), 1, "{s:?}");
        assert!((s[0].t_s - 6.0).abs() < 0.15, "{s:?}");
        assert!((s[0].dphi_rad - 0.8).abs() < 0.05, "{s:?}");
        let drift = phase_drift_per_point(&pts);
        assert!((drift - 2.0 * PI * 0.03 * 0.1).abs() < 2e-3, "{drift}");
    }
}
