//! The calibrated floor series from [`FloorFrame`] slow floors.
//!
//! One [`FloorPoint`] per frame: the slow floor (band: median of the slow block floors; channel:
//! mean of the slow per-bin floor), calibrated by the version its provenance pins.
//!
//! # Units
//!
//! - `value_db_per_hz` is **dBm/Hz** when calibrated (`unit` = `Dbm`), else **dBFS/Hz** with
//!   [`FloorFlags::UNCALIBRATED`]. `dbfs_per_hz` is always the raw reading.
//! - Noise temperature `T = S/k_B`, `S = 10^((dBm/Hz − 30)/10)` W/Hz, `k_B` = 1.380649e−23 J/K
//!   (exact SI): the equivalent temperature at the calibration plane (antenna + receiver noise,
//!   i.e. a system temperature). `P = k·T·B` over a bandwidth `B`.
//! - dB above `kT₀`, `T₀` = 290 K: `dBm/Hz − 10·log10(k_B·T₀·1000)` = `dBm/Hz + 173.975` (the
//!   usual "−174 dBm/Hz" rounded). `T = T₀·10^(dB above kT₀ / 10)`.
//!
//! # Uncertainty
//!
//! Standard uncertainties of the same coverage, combined in quadrature:
//!
//! `σ_est = √(σ_model² + σ_stat²)`, `σ = √(σ_est² + σ_cal²)` (uncalibrated: `σ = σ_est`, relative
//! to full scale), with `σ_model` = [`FloorFrame::uncertainty_db`] (T-005's ±0.5 dB),
//! `σ_stat` = [`FloorFrame::statistical_uncertainty_db`] (per frame; the slow floor's IIR only
//! reduces it, so this is conservative) and `σ_cal` = [`BandCal::uncertainty_db`]. Temperature
//! bounds are `T·10^(±σ/10)` (asymmetric in kelvin).
//!
//! # Flags and gaps
//!
//! See [`FloorFlags`]. [`FloorSeries`] marks the first point after a gap in time
//! ([`FloorFlags::GAP_BEFORE`]) and records the gap; it never interpolates across one.

use std::fmt;
use std::ops::{BitOr, BitOrAssign, Range};

use hk_model::{FreqRange, GainSetting, PowerUnit, TimeRange, Timestamp};

use super::calibration::{BandCal, PowerCalibrations, UncalibratedReason, gain_setting_of};
use crate::floor::{FloorFrame, FloorKind};

/// Boltzmann constant, J/K (exact since the 2019 SI).
pub const BOLTZMANN_J_PER_K: f64 = 1.380_649e-23;

/// Reference temperature `T₀`, K.
pub const T0_K: f64 = 290.0;

/// `10·log10(k_B·T₀) + 30`, dBm/Hz (≈ −173.975).
pub fn kt0_dbm_per_hz() -> f64 {
    10.0 * (BOLTZMANN_J_PER_K * T0_K).log10() + 30.0
}

/// Noise temperature of a PSD, K.
pub fn noise_temperature_k(dbm_per_hz: f64) -> f64 {
    10f64.powf((dbm_per_hz - 30.0) / 10.0) / BOLTZMANN_J_PER_K
}

/// A PSD relative to `kT₀`, dB.
pub fn db_above_kt0(dbm_per_hz: f64) -> f64 {
    dbm_per_hz - kt0_dbm_per_hz()
}

/// Root-sum-square of standard uncertainties, dB.
pub fn combine_uncertainty(parts: &[f64]) -> f64 {
    parts.iter().map(|x| x * x).sum::<f64>().sqrt()
}

/// Quality and state flags on a floor point or a floor-vs-time step.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct FloorFlags(u32);

impl FloorFlags {
    /// None.
    pub const NONE: Self = Self(0);
    /// No applicable power calibration: the value is dBFS/Hz, never dBm.
    pub const UNCALIBRATED: Self = Self(1 << 0);
    /// The floor is within the quantisation margin of the receiver's low-gain floor: the RF floor
    /// is at or below the value.
    pub const QUANTISATION_LIMITED: Self = Self(1 << 1);
    /// An impulsive frame: the slow floor held its value.
    pub const IMPULSIVE_PAUSED: Self = Self(1 << 2);
    /// The impulsive gate released (a sustained level change re-seeded the floor).
    pub const GATE_RELEASED: Self = Self(1 << 3);
    /// A new tracker segment began: gain, tune, rate or resolution change, stream start or gap.
    pub const SEGMENT_START: Self = Self(1 << 4);
    /// The slow floor was still warming up in its segment.
    pub const NOT_READY: Self = Self(1 << 5);
    /// No valid FCME block in the frame.
    pub const INVALID: Self = Self(1 << 6);
    /// A floor-change episode was active.
    pub const EPISODE: Self = Self(1 << 7);
    /// Calibration was held at the table's frequency edge.
    pub const CAL_EDGE_HELD: Self = Self(1 << 8);
    /// Time is missing before this point.
    pub const GAP_BEFORE: Self = Self(1 << 9);
    /// Nothing was observed in this step.
    pub const GAP: Self = Self(1 << 10);
    /// More than one gain setting contributed to the step.
    pub const MIXED_GAIN: Self = Self(1 << 11);
    /// More than one calibration version contributed to the step.
    pub const MIXED_CALIBRATION: Self = Self(1 << 12);
    /// A calibrated step also had uncalibrated observations (not included in its value).
    pub const PARTLY_UNCALIBRATED: Self = Self(1 << 13);

    const NAMES: [(Self, &'static str); 14] = [
        (Self::UNCALIBRATED, "uncalibrated"),
        (Self::QUANTISATION_LIMITED, "quantisation-limited"),
        (Self::IMPULSIVE_PAUSED, "impulsive-paused"),
        (Self::GATE_RELEASED, "gate-released"),
        (Self::SEGMENT_START, "segment-start"),
        (Self::NOT_READY, "not-ready"),
        (Self::INVALID, "invalid"),
        (Self::EPISODE, "episode"),
        (Self::CAL_EDGE_HELD, "cal-edge-held"),
        (Self::GAP_BEFORE, "gap-before"),
        (Self::GAP, "gap"),
        (Self::MIXED_GAIN, "mixed-gain"),
        (Self::MIXED_CALIBRATION, "mixed-calibration"),
        (Self::PARTLY_UNCALIBRATED, "partly-uncalibrated"),
    ];

    /// The raw bits.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// From raw bits, dropping unknown ones.
    pub const fn from_bits_truncate(bits: u32) -> Self {
        Self(bits & ((1 << 14) - 1))
    }

    /// All of `other` is set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Any of `other` is set.
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Nothing set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Sets `other` when `on`.
    pub fn set(&mut self, other: Self, on: bool) {
        if on {
            self.0 |= other.0;
        } else {
            self.0 &= !other.0;
        }
    }

    /// Names of the set flags.
    pub fn names(self) -> impl Iterator<Item = &'static str> {
        Self::NAMES
            .into_iter()
            .filter(move |(f, _)| self.contains(*f))
            .map(|(_, n)| n)
    }
}

impl BitOr for FloorFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for FloorFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl fmt::Debug for FloorFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FloorFlags(")?;
        for (i, n) in self.names().enumerate() {
            if i > 0 {
                f.write_str(" | ")?;
            }
            f.write_str(n)?;
        }
        f.write_str(")")
    }
}

/// The frame-state flags of a floor frame (not calibration, quantisation or gaps).
pub fn frame_flags(frame: &FloorFrame) -> FloorFlags {
    let mut f = FloorFlags::NONE;
    f.set(FloorFlags::IMPULSIVE_PAUSED, frame.impulsive);
    f.set(FloorFlags::GATE_RELEASED, frame.gate_released);
    f.set(FloorFlags::SEGMENT_START, frame.reset);
    f.set(FloorFlags::NOT_READY, !frame.slow_ready);
    f.set(FloorFlags::INVALID, !frame.valid);
    f.set(FloorFlags::EPISODE, frame.active_episodes > 0);
    f
}

/// Frame period `K·hop/fs` of a floor frame, ns.
pub fn frame_period_ns(frame: &FloorFrame) -> i64 {
    let g = &frame.gain;
    let hop = g.fft_len.saturating_sub(g.overlap).max(1);
    if g.sample_rate_hz > 0.0 {
        (f64::from(g.n_avg) * hop as f64 * 1e9 / g.sample_rate_hz).round() as i64
    } else {
        0
    }
}

/// One calibrated floor reading. See the [module docs](self) for units and uncertainty.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FloorPoint {
    /// Spectrum frame `seq`.
    pub seq: u64,
    /// Frame start.
    pub t: Timestamp,
    /// Frame period, ns.
    pub duration_ns: i64,
    /// Tracker segment.
    pub segment: u64,
    /// Frequencies the reading covers (bin centres).
    pub band: FreqRange,
    /// Gain setting.
    pub gain: GainSetting,
    /// `Dbm` when calibrated, else `Dbfs`.
    pub unit: PowerUnit,
    /// The floor in `unit`/Hz.
    pub value_db_per_hz: f64,
    /// The raw floor, dBFS/Hz.
    pub dbfs_per_hz: f64,
    /// Calibration applied.
    pub calibration: Option<BandCal>,
    /// Why not calibrated.
    pub uncalibrated_reason: Option<UncalibratedReason>,
    /// `σ_model`, dB.
    pub model_uncertainty_db: f64,
    /// `σ_stat`, dB.
    pub statistical_uncertainty_db: f64,
    /// `σ`, dB (see the module docs).
    pub uncertainty_db: f64,
    /// Flags.
    pub flags: FloorFlags,
}

impl FloorPoint {
    /// The floor in dBm/Hz, only when calibrated.
    pub fn dbm_per_hz(&self) -> Option<f64> {
        (self.unit == PowerUnit::Dbm).then_some(self.value_db_per_hz)
    }

    /// `σ_est = √(σ_model² + σ_stat²)`, dB.
    pub fn estimator_uncertainty_db(&self) -> f64 {
        self.model_uncertainty_db
            .hypot(self.statistical_uncertainty_db)
    }

    /// `σ_cal`, dB (0 when uncalibrated).
    pub fn calibration_uncertainty_db(&self) -> f64 {
        self.calibration.map_or(0.0, |c| c.uncertainty_db)
    }

    /// Noise temperature, K (calibrated only).
    pub fn noise_temperature_k(&self) -> Option<f64> {
        self.dbm_per_hz().map(noise_temperature_k)
    }

    /// `(T·10^(−σ/10), T·10^(+σ/10))`, K (calibrated only).
    pub fn noise_temperature_bounds_k(&self) -> Option<(f64, f64)> {
        let t = self.noise_temperature_k()?;
        let r = 10f64.powf(self.uncertainty_db / 10.0);
        Some((t / r, t * r))
    }

    /// dB above `kT₀` (calibrated only).
    pub fn db_above_kt0(&self) -> Option<f64> {
        self.dbm_per_hz().map(db_above_kt0)
    }
}

fn point(
    cals: &PowerCalibrations,
    frame: &FloorFrame,
    dbfs_per_hz: f64,
    band: FreqRange,
    quantisation_limited: bool,
) -> FloorPoint {
    let prov = frame.provenance.get();
    let gain = gain_setting_of(prov);
    let t = frame.t.host_time;
    let model = f64::from(frame.uncertainty_db);
    let stat = f64::from(frame.statistical_uncertainty_db);
    let est = model.hypot(stat);
    let mut flags = frame_flags(frame);
    flags.set(FloorFlags::QUANTISATION_LIMITED, quantisation_limited);
    let (unit, value, calibration, reason, sigma) =
        match cals.band(prov.calibration_state_ref, &gain, band, t) {
            Ok(c) => {
                flags.set(FloorFlags::CAL_EDGE_HELD, c.edge_held);
                (
                    PowerUnit::Dbm,
                    dbfs_per_hz + c.k_center_db,
                    Some(c),
                    None,
                    est.hypot(c.uncertainty_db),
                )
            }
            Err(r) => {
                flags |= FloorFlags::UNCALIBRATED;
                (PowerUnit::Dbfs, dbfs_per_hz, None, Some(r), est)
            }
        };
    FloorPoint {
        seq: frame.seq,
        t,
        duration_ns: frame_period_ns(frame),
        segment: frame.segment,
        band,
        gain,
        unit,
        value_db_per_hz: value,
        dbfs_per_hz,
        calibration,
        uncalibrated_reason: reason,
        model_uncertainty_db: model,
        statistical_uncertainty_db: stat,
        uncertainty_db: sigma,
        flags,
    }
}

fn bin_centre_hz(frame: &FloorFrame, bin: usize) -> f64 {
    let n = frame.slow_floor.len();
    frame.f_center_hz + (bin as f64 - (n / 2) as f64) * frame.bin_width_hz
}

/// The band point: the slow band floor over the whole span (`K` over the bin-centre span).
pub fn calibrate_band(cals: &PowerCalibrations, frame: &FloorFrame) -> FloorPoint {
    let n = frame.slow_floor.len().max(1);
    let band = FreqRange::new(bin_centre_hz(frame, 0), bin_centre_hz(frame, n - 1));
    point(
        cals,
        frame,
        f64::from(frame.band_floor_dbfs_per_hz(FloorKind::Slow)),
        band,
        frame.quantisation_limited,
    )
}

/// A channel point: the mean slow floor over `bins`.
pub fn calibrate_channel(
    cals: &PowerCalibrations,
    frame: &FloorFrame,
    bins: Range<usize>,
) -> FloorPoint {
    let ch = frame.channel_floor(bins.clone(), FloorKind::Slow);
    let band = FreqRange::new(
        bin_centre_hz(frame, bins.start),
        bin_centre_hz(frame, bins.end - 1),
    );
    point(
        cals,
        frame,
        f64::from(ch.dbfs_per_hz),
        band,
        ch.quantisation_limited,
    )
}

/// [`FloorSeries`] settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FloorSeriesConfig {
    /// A start-to-start spacing above this many frame periods is a gap (1.5).
    pub gap_factor: f64,
}

impl Default for FloorSeriesConfig {
    fn default() -> Self {
        Self { gap_factor: 1.5 }
    }
}

/// The calibrated band-floor series of one stream. Appends one point per frame (a science
/// series at frame rate, not the per-sample path).
#[derive(Clone, Debug, Default)]
pub struct FloorSeries {
    config: FloorSeriesConfig,
    calibrations: PowerCalibrations,
    points: Vec<FloorPoint>,
    gaps: Vec<TimeRange>,
}

impl FloorSeries {
    /// An empty series calibrating with `calibrations`.
    pub fn new(calibrations: PowerCalibrations, config: FloorSeriesConfig) -> Self {
        Self {
            config,
            calibrations,
            points: Vec::new(),
            gaps: Vec::new(),
        }
    }

    /// The calibration versions.
    pub fn calibrations(&self) -> &PowerCalibrations {
        &self.calibrations
    }

    /// Adds calibration versions as C05 measures them.
    pub fn calibrations_mut(&mut self) -> &mut PowerCalibrations {
        &mut self.calibrations
    }

    /// Appends the band point of `frame`.
    pub fn push(&mut self, frame: &FloorFrame) -> &FloorPoint {
        let mut p = calibrate_band(&self.calibrations, frame);
        if let Some(prev) = self.points.last() {
            let spacing = p.t.as_unix_nanos() - prev.t.as_unix_nanos();
            if spacing as f64 > self.config.gap_factor * prev.duration_ns.max(1) as f64 {
                p.flags |= FloorFlags::GAP_BEFORE;
                self.gaps.push(TimeRange::new(
                    prev.t.saturating_add_nanos(prev.duration_ns),
                    p.t,
                ));
            }
        }
        self.points.push(p);
        self.points.last().expect("just pushed")
    }

    /// Points in arrival order.
    pub fn points(&self) -> &[FloorPoint] {
        &self.points
    }

    /// Missing time between points.
    pub fn gaps(&self) -> &[TimeRange] {
        &self.gaps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kt0_and_temperature() {
        assert!((kt0_dbm_per_hz() + 173.975).abs() < 1e-3);
        assert!((noise_temperature_k(kt0_dbm_per_hz()) - T0_K).abs() < 1e-9);
        // 3 dB above kT0 is 2·T0 (to 0.1 %).
        assert!((noise_temperature_k(kt0_dbm_per_hz() + 3.0103) / T0_K - 2.0).abs() < 1e-3);
        assert!((db_above_kt0(-164.0) - 9.975).abs() < 1e-3);
        assert!((combine_uncertainty(&[0.3, 0.4]) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn flags_compose() {
        let mut f = FloorFlags::UNCALIBRATED | FloorFlags::GAP;
        f |= FloorFlags::MIXED_GAIN;
        assert!(f.contains(FloorFlags::UNCALIBRATED | FloorFlags::MIXED_GAIN));
        assert!(!f.contains(FloorFlags::QUANTISATION_LIMITED));
        f.set(FloorFlags::GAP, false);
        assert_eq!(
            f.names().collect::<Vec<_>>(),
            ["uncalibrated", "mixed-gain"]
        );
        assert_eq!(FloorFlags::from_bits_truncate(u32::MAX).names().count(), 14);
        assert_eq!(format!("{:?}", FloorFlags::GAP), "FloorFlags(gap)");
    }
}
