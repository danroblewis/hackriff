//! Observable epochs — what a GNSS receiver reports once per second, and the science derived
//! from it.
//!
//! These are the C36 card's `GnssObservableEpoch` in concrete form, reduced to the fields this
//! milestone can actually produce or mock. Pseudorange and carrier phase are **absent on
//! purpose**: they require tracking loops and a decoded navigation message, neither of which is
//! built (crate docs). Adding empty fields for them would imply a capability that does not
//! exist.

use hk_model::Timestamp;

/// An Earth-centred, Earth-fixed position in metres.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Ecef {
    /// X, metres.
    pub x: f64,
    /// Y, metres.
    pub y: f64,
    /// Z, metres.
    pub z: f64,
}

impl Ecef {
    /// Straight-line distance to `other`, metres.
    pub fn distance_m(&self, other: &Ecef) -> f64 {
        let (dx, dy, dz) = (self.x - other.x, self.y - other.y, self.z - other.z);
        (dx * dx + dy * dy + dz * dz).sqrt()
    }
}

/// One satellite's state at an epoch.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SvObservable {
    /// Which satellite.
    pub prn: u8,
    /// Carrier-to-noise density, dB-Hz.
    pub cn0_dbhz: f32,
    /// Doppler, Hz.
    pub doppler_hz: f64,
    /// Elevation above the horizon, where known. Real constellations spread C/N0 by elevation,
    /// which is what makes uniformity suspicious (see [`crate::integrity::assess_spoofing`]).
    pub elevation_deg: Option<f32>,
    /// Whether the receiver holds lock on this satellite.
    pub locked: bool,
}

/// Everything observed at one epoch.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GnssObservableEpoch {
    /// When.
    pub t: Timestamp,
    /// Per-satellite state.
    pub svs: Vec<SvObservable>,
    /// Receiver position, when a solution exists. `None` here is normal: this milestone has no
    /// PVT solver.
    pub position: Option<Ecef>,
    /// Receiver clock bias against system time, seconds, when known.
    pub clock_bias_s: Option<f64>,
}

impl GnssObservableEpoch {
    /// Satellites currently locked.
    pub fn locked(&self) -> impl Iterator<Item = &SvObservable> {
        self.svs.iter().filter(|s| s.locked)
    }

    /// How many satellites are locked.
    pub fn locked_count(&self) -> usize {
        self.locked().count()
    }

    /// Mean C/N0 over locked satellites, dB-Hz.
    pub fn mean_cn0_dbhz(&self) -> Option<f32> {
        let (sum, n) = self
            .locked()
            .fold((0f32, 0usize), |(s, n), sv| (s + sv.cn0_dbhz, n + 1));
        (n > 0).then(|| sum / n as f32)
    }

    /// Population standard deviation of C/N0 over locked satellites, dB.
    pub fn cn0_sigma_db(&self) -> Option<f32> {
        let mean = self.mean_cn0_dbhz()?;
        let (sum_sq, n) = self.locked().fold((0f32, 0usize), |(s, n), sv| {
            let d = sv.cn0_dbhz - mean;
            (s + d * d, n + 1)
        });
        (n > 1).then(|| (sum_sq / n as f32).sqrt())
    }
}

/// The S4 amplitude-scintillation index over a run of signal-intensity samples (PROP-033).
///
/// `S4² = (⟨I²⟩ − ⟨I⟩²) / ⟨I⟩²` — the normalised standard deviation of received power. Returns
/// `None` for fewer than two samples or a non-positive mean.
///
/// Computing it needs high-rate intensity from a tracking loop, which this milestone does not
/// have; the function is exercised against mocked intensity series. **Real scintillation
/// measurement is unverified** and needs a field capture under a disturbed ionosphere.
pub fn s4_index(intensities: &[f32]) -> Option<f32> {
    if intensities.len() < 2 {
        return None;
    }
    let n = intensities.len() as f64;
    let mean = intensities.iter().map(|&i| f64::from(i)).sum::<f64>() / n;
    if mean <= 0.0 {
        return None;
    }
    let mean_sq = intensities
        .iter()
        .map(|&i| f64::from(i) * f64::from(i))
        .sum::<f64>()
        / n;
    let variance = (mean_sq - mean * mean).max(0.0);
    Some((variance.sqrt() / mean) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sv(prn: u8, cn0: f32, locked: bool) -> SvObservable {
        SvObservable {
            prn,
            cn0_dbhz: cn0,
            doppler_hz: 0.0,
            elevation_deg: None,
            locked,
        }
    }

    fn epoch(svs: Vec<SvObservable>) -> GnssObservableEpoch {
        GnssObservableEpoch {
            t: Timestamp::from_unix_nanos(0),
            svs,
            position: None,
            clock_bias_s: None,
        }
    }

    #[test]
    fn unlocked_satellites_are_excluded_from_the_statistics() {
        let e = epoch(vec![
            sv(1, 45.0, true),
            sv(2, 35.0, true),
            sv(3, 10.0, false),
        ]);
        assert_eq!(e.locked_count(), 2);
        assert_eq!(e.mean_cn0_dbhz(), Some(40.0));
        assert_eq!(e.cn0_sigma_db(), Some(5.0));
    }

    #[test]
    fn statistics_are_absent_rather_than_zero_when_nothing_is_locked() {
        let e = epoch(vec![sv(1, 45.0, false)]);
        assert_eq!(e.mean_cn0_dbhz(), None);
        assert_eq!(e.cn0_sigma_db(), None);
    }

    #[test]
    fn s4_is_zero_for_a_steady_signal_and_rises_with_fading() {
        let steady = [1.0f32; 64];
        assert!(s4_index(&steady).unwrap() < 1e-6);

        let fading: Vec<f32> = (0..64)
            .map(|i| if i % 2 == 0 { 0.5 } else { 1.5 })
            .collect();
        let s4 = s4_index(&fading).unwrap();
        assert!((s4 - 0.5).abs() < 1e-5, "S4 was {s4}");
    }

    #[test]
    fn s4_refuses_degenerate_input() {
        assert_eq!(s4_index(&[]), None);
        assert_eq!(s4_index(&[1.0]), None);
        assert_eq!(s4_index(&[0.0, 0.0]), None);
    }

    #[test]
    fn distance_is_euclidean() {
        let a = Ecef {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        };
        let b = Ecef {
            x: 3.0,
            y: 4.0,
            z: 0.0,
        };
        assert!((a.distance_m(&b) - 5.0).abs() < 1e-9);
    }
}
