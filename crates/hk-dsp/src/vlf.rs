//! VLF/LF science analyses (T-277): SID flare amplitude tracking (SPACE-001), the
//! transmitter-phase measurement behind PROP-019, and the broadband/sferic front of SPACE-041.
//!
//! These are pure analyses over real samples. **The HackRF cannot receive them** (16–30 kHz is
//! below its 1 MHz floor): SPACE-001, SPACE-041 and PROP-019 stay `needs-accessory` (VLF
//! receiver + soundcard, or an upconverter) and nothing here claims them reachable without it.
//! [`VLF_ACCESSORY_MIN_HZ`] is the honest tuner floor callers can gate on.

use num_complex::Complex32;
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

/// Track amplitude and unwrapped phase of a carrier at `carrier_hz` in real samples at `fs`,
/// one point per `block` samples (coherent mix + boxcar average).
pub fn track_carrier(x: &[f32], fs: f64, carrier_hz: f64, block: usize) -> Vec<VlfPoint> {
    let mut out = Vec::new();
    if block == 0 {
        return out;
    }
    let mut prev = 0.0f64;
    let mut offset = 0.0f64;
    for (bi, chunk) in x.chunks_exact(block).enumerate() {
        let base = bi * block;
        let mut acc = Complex32::new(0.0, 0.0);
        for (i, &s) in chunk.iter().enumerate() {
            let ph = -2.0 * PI * carrier_hz * (base + i) as f64 / fs;
            acc += Complex32::new(ph.cos() as f32, ph.sin() as f32) * s;
        }
        acc *= 2.0 / block as f32;
        let raw = f64::from(acc.arg());
        if bi > 0 {
            let d = raw + offset - prev;
            offset -= 2.0 * PI * (d / (2.0 * PI)).round();
        }
        let phase = raw + offset;
        prev = phase;
        out.push(VlfPoint {
            t_s: base as f64 / fs,
            amplitude: acc.norm(),
            phase_rad: phase,
        });
    }
    out
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
    let mut hits: Vec<(usize, f32)> = Vec::new();
    if win == 0 || pts.len() < 2 * win {
        return Vec::new();
    }
    let mean = |s: &[VlfPoint]| s.iter().map(|p| p.amplitude).sum::<f32>() / s.len() as f32;
    for i in win..=pts.len() - win {
        let (b, a) = (mean(&pts[i - win..i]), mean(&pts[i..i + win]));
        if b > 0.0 && ((a - b) / b).abs() >= min_rel {
            hits.push((i, (a - b) / b));
        }
    }
    let mut out: Vec<AmplitudeStep> = Vec::new();
    let mut last: Option<usize> = None;
    let mut best: Option<(usize, f32)> = None;
    for (i, r) in hits {
        if last.is_some_and(|l| i > l + 1) {
            if let Some((bi, br)) = best.take() {
                out.push(AmplitudeStep {
                    t_s: pts[bi].t_s,
                    relative_change: br,
                });
            }
        }
        if best.is_none_or(|(_, br)| r.abs() > br.abs()) {
            best = Some((i, r));
        }
        last = Some(i);
    }
    if let Some((bi, br)) = best {
        out.push(AmplitudeStep {
            t_s: pts[bi].t_s,
            relative_change: br,
        });
    }
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

/// SPACE-041 sferic front: sample indices where |x| exceeds `k`× the median absolute value,
/// with `dead` samples of refractory time after each hit.
pub fn detect_sferics(x: &[f32], k: f32, dead: usize) -> Vec<usize> {
    if x.is_empty() {
        return Vec::new();
    }
    let mut a: Vec<f32> = x.iter().map(|v| v.abs()).collect();
    a.sort_by(|p, q| p.total_cmp(q));
    let thr = k * a[a.len() / 2];
    let mut out = Vec::new();
    let mut i = 0;
    while i < x.len() {
        if x[i].abs() > thr {
            out.push(i);
            i += dead.max(1);
        } else {
            i += 1;
        }
    }
    out
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
}
