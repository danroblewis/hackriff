//! The dBFS → dBm power calibration table (C05 `K(f, G)`; docs/07 §2.7).
//!
//! `P_dBm = P_dBFS + K(f, gain)`. A [`PowerCalTable`] is one immutable [`CalibrationState`]
//! version: per exact gain setting (LNA/VGA/amp), points `(f, K, σ_K)` sorted by frequency.
//!
//! # Lookup rules
//!
//! - **Gain:** exact setting, gains within [`GAIN_MATCH_TOLERANCE_DB`]; never interpolated across
//!   gain states and never inferred from nominal step sizes (S4 measured the amp at ~15 dB at
//!   98 MHz, not its nominal 11). Points without a gain setting are ignored
//!   ([`PowerCalTable::ignored_points`]).
//! - **Frequency:** linear in dB between the two bracketing points; `σ = max(σ_a, σ_b)`
//!   (conservative: no interpolation-error model). Outside the table's span only within
//!   [`PowerCalTable::with_edge_tolerance`] (default 0 Hz), holding the end value
//!   (`edge_held`).
//! - **Time:** only within the state's `valid` window, when it has one.
//! - **Provenance:** a frame is calibrated by the version its provenance pins
//!   (`calibration_state_ref`), never by "the latest" table.
//! - **Band:** K is evaluated at the band edges, the centre and every table point inside; the
//!   centre value is applied and half the spread is added to the uncertainty
//!   ([`BandCal::uncertainty_db`]).
//!
//! Anything else is **uncalibrated** ([`UncalibratedReason`]): the value stays dBFS/Hz. dBm is
//! never faked.

use std::collections::HashMap;

use hk_model::{
    CalibrationMethod, CalibrationState, CalibrationStateId, FreqRange, GainSetting, PowerCalPoint,
    Provenance, TimeRange, Timestamp,
};

/// Gains within this of each other are the same setting, dB (as `GainKey::matches`).
pub const GAIN_MATCH_TOLERANCE_DB: f64 = 0.01;

/// The gain setting of a provenance record.
pub fn gain_setting_of(p: &Provenance) -> GainSetting {
    GainSetting {
        lna_db: p.tune.lna_db,
        vga_db: p.tune.vga_db,
        amp_on: p.tune.amp_on,
    }
}

/// Same LNA/VGA within [`GAIN_MATCH_TOLERANCE_DB`] and the same amp state.
pub fn same_gain(a: &GainSetting, b: &GainSetting) -> bool {
    (a.lna_db - b.lna_db).abs() <= GAIN_MATCH_TOLERANCE_DB
        && (a.vga_db - b.vga_db).abs() <= GAIN_MATCH_TOLERANCE_DB
        && a.amp_on == b.amp_on
}

/// One table point.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CalPoint {
    /// Frequency, Hz.
    pub f_hz: f64,
    /// `K`, dB: dBm = dBFS + K.
    pub k_db: f64,
    /// Standard uncertainty of `k_db`, dB.
    pub uncertainty_db: f64,
}

/// `K` at one frequency.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CalValue {
    /// Version applied.
    pub calibration: CalibrationStateId,
    /// `K`, dB.
    pub k_db: f64,
    /// Standard uncertainty, dB.
    pub uncertainty_db: f64,
    /// The frequency was outside the table's span (within the edge tolerance).
    pub edge_held: bool,
}

/// `K` over a band.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BandCal {
    /// Version applied.
    pub calibration: CalibrationStateId,
    /// `K` at the band centre, dB (the value applied to a band floor).
    pub k_center_db: f64,
    /// Smallest `K` over the band, dB.
    pub k_min_db: f64,
    /// Largest `K` over the band, dB.
    pub k_max_db: f64,
    /// Largest point uncertainty over the band, dB.
    pub point_uncertainty_db: f64,
    /// `√(point_uncertainty² + ((k_max − k_min)/2)²)`, dB: the band term covers applying the
    /// centre value to the whole band.
    pub uncertainty_db: f64,
    /// Some frequency was held at the table's edge.
    pub edge_held: bool,
}

/// Why no calibration applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UncalibratedReason {
    /// The provenance names no calibration version.
    NoCalibrationRef,
    /// The named version is not loaded.
    UnknownCalibration,
    /// The version has no usable power points.
    NoPowerTable,
    /// The time is outside the version's validity window.
    OutsideValidity,
    /// The version has no points for this gain setting.
    GainNotCalibrated,
    /// The frequency (or part of the band) is outside the table's span.
    FrequencyNotCovered,
}

/// One calibration version's power table. See the [module docs](self) for the lookup rules.
#[derive(Clone, Debug, PartialEq)]
pub struct PowerCalTable {
    id: CalibrationStateId,
    valid: Option<TimeRange>,
    states: Vec<(GainSetting, Vec<CalPoint>)>,
    edge_tolerance_hz: f64,
    ignored_points: usize,
}

impl PowerCalTable {
    /// Loads a version's power table. Points without a gain setting, with non-finite values, or
    /// without an uncertainty (when `missing_uncertainty_db` is `None`) are ignored.
    pub fn from_state(state: &CalibrationState, missing_uncertainty_db: Option<f64>) -> Self {
        let mut states: Vec<(GainSetting, Vec<CalPoint>)> = Vec::new();
        let mut ignored = 0;
        for p in &state.power_table {
            let (Some(g), Some(u)) = (p.gain, p.uncertainty_db.or(missing_uncertainty_db)) else {
                ignored += 1;
                continue;
            };
            if !(p.f_hz.is_finite() && p.offset_db.is_finite() && u.is_finite() && u >= 0.0) {
                ignored += 1;
                continue;
            }
            let point = CalPoint {
                f_hz: p.f_hz,
                k_db: p.offset_db,
                uncertainty_db: u,
            };
            match states.iter_mut().find(|(s, _)| same_gain(s, &g)) {
                Some((_, v)) => v.push(point),
                None => states.push((g, vec![point])),
            }
        }
        for (_, v) in &mut states {
            v.sort_by(|a, b| a.f_hz.total_cmp(&b.f_hz));
        }
        Self {
            id: state.id,
            valid: state.valid,
            states,
            edge_tolerance_hz: 0.0,
            ignored_points: ignored,
        }
    }

    /// Allows lookups up to `hz` outside the table's span, holding the end value.
    pub fn with_edge_tolerance(mut self, hz: f64) -> Self {
        self.edge_tolerance_hz = hz.max(0.0);
        self
    }

    /// The version id.
    pub fn id(&self) -> CalibrationStateId {
        self.id
    }

    /// Points ignored on load.
    pub fn ignored_points(&self) -> usize {
        self.ignored_points
    }

    /// Calibrated gain settings.
    pub fn gain_settings(&self) -> impl Iterator<Item = &GainSetting> {
        self.states.iter().map(|(g, _)| g)
    }

    /// Points for a gain setting, ascending frequency.
    pub fn points(&self, gain: &GainSetting) -> Option<&[CalPoint]> {
        self.states
            .iter()
            .find(|(g, _)| same_gain(g, gain))
            .map(|(_, v)| &v[..])
    }

    fn checked_points(
        &self,
        gain: &GainSetting,
        t: Timestamp,
    ) -> Result<&[CalPoint], UncalibratedReason> {
        if self.states.is_empty() {
            return Err(UncalibratedReason::NoPowerTable);
        }
        if self.valid.is_some_and(|v| t < v.start || t > v.end) {
            return Err(UncalibratedReason::OutsideValidity);
        }
        self.points(gain)
            .ok_or(UncalibratedReason::GainNotCalibrated)
    }

    /// `K` at `f_hz` for `gain` at time `t`.
    pub fn at(
        &self,
        gain: &GainSetting,
        f_hz: f64,
        t: Timestamp,
    ) -> Result<CalValue, UncalibratedReason> {
        let points = self.checked_points(gain, t)?;
        let (k_db, uncertainty_db, edge_held) = interpolate(points, f_hz, self.edge_tolerance_hz)
            .ok_or(UncalibratedReason::FrequencyNotCovered)?;
        Ok(CalValue {
            calibration: self.id,
            k_db,
            uncertainty_db,
            edge_held,
        })
    }

    /// `K` over `band` for `gain` at time `t`: every frequency must be covered.
    pub fn band(
        &self,
        gain: &GainSetting,
        band: FreqRange,
        t: Timestamp,
    ) -> Result<BandCal, UncalibratedReason> {
        let points = self.checked_points(gain, t)?;
        let tol = self.edge_tolerance_hz;
        let eval =
            |f: f64| interpolate(points, f, tol).ok_or(UncalibratedReason::FrequencyNotCovered);
        let (k_center_db, mut sigma, mut held) = eval(band.center_hz())?;
        let (mut k_min, mut k_max) = (k_center_db, k_center_db);
        let inner = points
            .iter()
            .filter(|p| p.f_hz > band.lo_hz && p.f_hz < band.hi_hz)
            .map(|p| (p.k_db, p.uncertainty_db, false));
        for (k, s, h) in [eval(band.lo_hz)?, eval(band.hi_hz)?]
            .into_iter()
            .chain(inner)
        {
            k_min = k_min.min(k);
            k_max = k_max.max(k);
            sigma = sigma.max(s);
            held |= h;
        }
        Ok(BandCal {
            calibration: self.id,
            k_center_db,
            k_min_db: k_min,
            k_max_db: k_max,
            point_uncertainty_db: sigma,
            uncertainty_db: sigma.hypot((k_max - k_min) / 2.0),
            edge_held: held,
        })
    }
}

/// `(K, σ, held)` at `f` by linear interpolation in dB.
fn interpolate(points: &[CalPoint], f: f64, tol: f64) -> Option<(f64, f64, bool)> {
    if !f.is_finite() {
        return None;
    }
    let (first, last) = (points.first()?, points.last()?);
    if f < first.f_hz {
        return (first.f_hz - f <= tol).then_some((first.k_db, first.uncertainty_db, true));
    }
    if f > last.f_hz {
        return (f - last.f_hz <= tol).then_some((last.k_db, last.uncertainty_db, true));
    }
    let i = points.partition_point(|p| p.f_hz < f);
    let b = points[i];
    if i == 0 || b.f_hz == f {
        return Some((b.k_db, b.uncertainty_db, false));
    }
    let a = points[i - 1];
    let w = (f - a.f_hz) / (b.f_hz - a.f_hz);
    Some((
        a.k_db + w * (b.k_db - a.k_db),
        a.uncertainty_db.max(b.uncertainty_db),
        false,
    ))
}

/// The loaded calibration versions, by id.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PowerCalibrations {
    tables: HashMap<CalibrationStateId, PowerCalTable>,
}

impl PowerCalibrations {
    /// No versions: everything is uncalibrated.
    pub fn new() -> Self {
        Self::default()
    }

    /// Loads versions with [`PowerCalTable::from_state`].
    pub fn from_states<'a>(
        states: impl IntoIterator<Item = &'a CalibrationState>,
        missing_uncertainty_db: Option<f64>,
    ) -> Self {
        let mut s = Self::new();
        for state in states {
            s.insert(PowerCalTable::from_state(state, missing_uncertainty_db));
        }
        s
    }

    /// Adds (or replaces) a version.
    pub fn insert(&mut self, table: PowerCalTable) -> Option<PowerCalTable> {
        self.tables.insert(table.id(), table)
    }

    /// A version.
    pub fn get(&self, id: CalibrationStateId) -> Option<&PowerCalTable> {
        self.tables.get(&id)
    }

    /// Versions loaded.
    pub fn len(&self) -> usize {
        self.tables.len()
    }

    /// No versions loaded.
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    fn table(
        &self,
        cal_ref: Option<CalibrationStateId>,
    ) -> Result<&PowerCalTable, UncalibratedReason> {
        let id = cal_ref.ok_or(UncalibratedReason::NoCalibrationRef)?;
        self.tables
            .get(&id)
            .ok_or(UncalibratedReason::UnknownCalibration)
    }

    /// `K` at `f_hz` under the version a provenance pins.
    pub fn at(
        &self,
        cal_ref: Option<CalibrationStateId>,
        gain: &GainSetting,
        f_hz: f64,
        t: Timestamp,
    ) -> Result<CalValue, UncalibratedReason> {
        self.table(cal_ref)?.at(gain, f_hz, t)
    }

    /// `K` over `band` under the version a provenance pins.
    pub fn band(
        &self,
        cal_ref: Option<CalibrationStateId>,
        gain: &GainSetting,
        band: FreqRange,
        t: Timestamp,
    ) -> Result<BandCal, UncalibratedReason> {
        self.table(cal_ref)?.band(gain, band, t)
    }
}

/// One band of synthetic truth: the generator's `calibration_k_db` over `band` at `gain`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SyntheticCalSegment {
    /// Frequencies the constant applies to (points at both edges).
    pub band: FreqRange,
    /// Gain setting.
    pub gain: GainSetting,
    /// `dBm = dBFS + k_db`, in the pipeline's dBFS scale.
    pub k_db: f64,
}

/// A calibration version built from synthetic truth (T-023 `injected_floor`'s
/// `calibration_k_db`), for tests: two points per segment at the band edges, each with
/// `uncertainty_db`. Note the scale: hk-e2e's cf32 reader matches the generator's dBFS, while
/// hk-core's ci8 normalisation (`/128`) reads `20·log10(127/128)` lower, which the caller adds
/// to `k_db`.
pub fn synthetic_calibration_state(
    device_id: &str,
    segments: &[SyntheticCalSegment],
    uncertainty_db: f64,
    measured_at: Timestamp,
) -> CalibrationState {
    let power_table = segments
        .iter()
        .flat_map(|s| {
            [s.band.lo_hz, s.band.hi_hz].map(|f_hz| PowerCalPoint {
                f_hz,
                // Informational only (nominal amp); lookups use `gain`.
                gain_db: s.gain.lna_db + s.gain.vga_db + if s.gain.amp_on { 11.0 } else { 0.0 },
                offset_db: s.k_db,
                gain: Some(s.gain),
                uncertainty_db: Some(uncertainty_db),
            })
        })
        .collect();
    CalibrationState {
        id: CalibrationStateId::new(),
        supersedes: None,
        device_id: device_id.to_owned(),
        ppm: 0.0,
        method: CalibrationMethod::Manual,
        measured_at,
        valid: None,
        temperature_c: None,
        power_table,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const G1: GainSetting = GainSetting {
        lna_db: 24.0,
        vga_db: 20.0,
        amp_on: false,
    };
    const G2: GainSetting = GainSetting {
        lna_db: 16.0,
        vga_db: 28.0,
        amp_on: false,
    };

    fn point(f_hz: f64, k: f64, u: Option<f64>, gain: Option<GainSetting>) -> PowerCalPoint {
        PowerCalPoint {
            f_hz,
            gain_db: 44.0,
            offset_db: k,
            gain,
            uncertainty_db: u,
        }
    }

    fn state(points: Vec<PowerCalPoint>, valid: Option<TimeRange>) -> CalibrationState {
        CalibrationState {
            id: CalibrationStateId::new(),
            supersedes: None,
            device_id: "test".into(),
            ppm: 0.0,
            method: CalibrationMethod::Manual,
            measured_at: Timestamp::UNIX_EPOCH,
            valid,
            temperature_c: None,
            power_table: points,
        }
    }

    #[test]
    fn interpolates_in_frequency_per_exact_gain_state() {
        let s = state(
            vec![
                point(200e6, -60.0, Some(0.4), Some(G1)),
                point(100e6, -70.0, Some(0.2), Some(G1)),
                point(100e6, -50.0, Some(0.3), Some(G2)),
                point(150e6, -1.0, Some(0.3), None), // total gain only: ignored
                point(150e6, -1.0, None, Some(G1)),  // no uncertainty: ignored
            ],
            None,
        );
        let t = PowerCalTable::from_state(&s, None);
        assert_eq!(t.ignored_points(), 2);
        let t0 = Timestamp::UNIX_EPOCH;
        let v = t.at(&G1, 125e6, t0).unwrap();
        assert!((v.k_db + 67.5).abs() < 1e-12 && v.uncertainty_db == 0.4 && !v.edge_held);
        // Same total gain, different state: its own table, never G1's.
        assert_eq!(t.at(&G2, 100e6, t0).unwrap().k_db, -50.0);
        assert_eq!(
            t.at(&G2, 125e6, t0),
            Err(UncalibratedReason::FrequencyNotCovered)
        );
        let amp = GainSetting { amp_on: true, ..G1 };
        assert_eq!(
            t.at(&amp, 125e6, t0),
            Err(UncalibratedReason::GainNotCalibrated)
        );
        // With a missing-uncertainty default the third G1 point loads.
        let t2 = PowerCalTable::from_state(&s, Some(1.0));
        assert_eq!(t2.at(&G1, 150e6, t0).unwrap().k_db, -1.0);
        // Edge tolerance holds the end value.
        assert!(t.at(&G1, 99e6, t0).is_err());
        let held = t
            .clone()
            .with_edge_tolerance(2e6)
            .at(&G1, 99e6, t0)
            .unwrap();
        assert!(held.edge_held && held.k_db == -70.0);
    }

    #[test]
    fn band_uses_centre_and_spread_and_validity() {
        let valid = TimeRange::new(
            Timestamp::from_unix_nanos(10),
            Timestamp::from_unix_nanos(20),
        );
        let s = state(
            vec![
                point(100e6, -70.0, Some(0.2), Some(G1)),
                point(110e6, -66.0, Some(0.5), Some(G1)),
                point(120e6, -70.0, Some(0.2), Some(G1)),
            ],
            Some(valid),
        );
        let mut cals = PowerCalibrations::new();
        cals.insert(PowerCalTable::from_state(&s, None));
        let t = Timestamp::from_unix_nanos(15);
        let b = cals
            .band(Some(s.id), &G1, FreqRange::new(105e6, 115e6), t)
            .unwrap();
        assert_eq!(
            (b.k_center_db, b.k_min_db, b.k_max_db),
            (-66.0, -68.0, -66.0)
        );
        assert!((b.uncertainty_db - 0.5f64.hypot(1.0)).abs() < 1e-12);
        let late = Timestamp::from_unix_nanos(21);
        assert_eq!(
            cals.band(Some(s.id), &G1, FreqRange::new(105e6, 115e6), late),
            Err(UncalibratedReason::OutsideValidity)
        );
        assert_eq!(
            cals.band(None, &G1, FreqRange::new(105e6, 115e6), t),
            Err(UncalibratedReason::NoCalibrationRef)
        );
        assert_eq!(
            cals.band(
                Some(CalibrationStateId::new()),
                &G1,
                FreqRange::new(105e6, 115e6),
                t
            ),
            Err(UncalibratedReason::UnknownCalibration)
        );
        assert_eq!(
            cals.band(Some(s.id), &G1, FreqRange::new(95e6, 115e6), t),
            Err(UncalibratedReason::FrequencyNotCovered)
        );
    }

    #[test]
    fn synthetic_state_round_trips_through_the_loader() {
        let seg = SyntheticCalSegment {
            band: FreqRange::centered(144e6, 1e6),
            gain: G1,
            k_db: -72.3,
        };
        let s = synthetic_calibration_state("synthetic", &[seg], 0.1, Timestamp::UNIX_EPOCH);
        let t = PowerCalTable::from_state(&s, None);
        assert_eq!(t.ignored_points(), 0);
        let b = t.band(&G1, seg.band, Timestamp::UNIX_EPOCH).unwrap();
        assert_eq!(b.k_center_db, -72.3);
        assert_eq!(b.uncertainty_db, 0.1);
        let empty = PowerCalTable::from_state(&state(vec![], None), None);
        assert_eq!(
            empty.at(&G1, 1.0, Timestamp::UNIX_EPOCH),
            Err(UncalibratedReason::NoPowerTable)
        );
    }
}
