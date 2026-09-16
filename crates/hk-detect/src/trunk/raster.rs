//! LMR channel-raster fits (C23 candidacy).
//!
//! A raster fit is **evidence of candidacy, never of identity**. Land-mobile radio is channelised
//! on 12.5 kHz (narrowbanding, docs/04 §4), with 25 kHz legacy and 6.25 kHz NXDN variants, so an
//! emission whose centre lands on one of those grids is *worth examining*. Plenty of things land
//! on a 12.5 kHz grid without being a control channel, which is why nothing in this module can
//! confirm one — see [`super::confirm`].
//!
//! This mirrors `hk_context::occupancy::channels`, whose module doc states the same rule for
//! learned channels: "**Rasters are hints.**"

/// The LMR rasters worth testing, widest first (docs/04 §4, §7.2).
pub const LMR_RASTERS_HZ: [f64; 3] = [25_000.0, 12_500.0, 6_250.0];

/// How far off-grid a centre may sit and still count as on-raster, Hz.
///
/// A priori: a 12.5 kHz channel's own centre estimate is good to well under a kilohertz at the
/// bandwidths involved, and docs/04 §4.7 uses ±1.5 kHz for raster membership elsewhere in the
/// detector. Widening this admits off-grid emitters as candidates; it cannot promote one,
/// because candidacy is not confirmation.
pub const RASTER_TOLERANCE_HZ: f64 = 1_500.0;

/// An emission centre's fit to a channel grid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RasterFit {
    /// Grid spacing, Hz.
    pub spacing_hz: f64,
    /// Grid origin, Hz (the reference channel centre).
    pub origin_hz: f64,
    /// Channel index relative to `origin_hz`.
    pub channel: i64,
    /// Signed distance from the nearest grid centre, Hz.
    pub offset_hz: f64,
}

impl RasterFit {
    /// The grid centre this fit snapped to, Hz.
    pub fn channel_hz(&self) -> f64 {
        self.origin_hz + self.channel as f64 * self.spacing_hz
    }
}

/// Fits `center_hz` to the grid `origin_hz + k·spacing_hz`, within `tolerance_hz`.
pub fn fit_raster(
    center_hz: f64,
    origin_hz: f64,
    spacing_hz: f64,
    tolerance_hz: f64,
) -> Option<RasterFit> {
    let usable = center_hz.is_finite()
        && origin_hz.is_finite()
        && spacing_hz.is_finite()
        && spacing_hz > 0.0;
    if !usable {
        return None;
    }
    let k = ((center_hz - origin_hz) / spacing_hz).round();
    if !k.is_finite() {
        return None;
    }
    let offset_hz = center_hz - (origin_hz + k * spacing_hz);
    (offset_hz.abs() <= tolerance_hz).then_some(RasterFit {
        spacing_hz,
        origin_hz,
        channel: k as i64,
        offset_hz,
    })
}

/// The best LMR raster fit for `center_hz`: the widest spacing it lands on, which keeps a
/// 25 kHz emitter from being reported as an off-by-half 12.5 kHz one.
pub fn best_lmr_raster(center_hz: f64, origin_hz: f64, tolerance_hz: f64) -> Option<RasterFit> {
    LMR_RASTERS_HZ
        .iter()
        .find_map(|&s| fit_raster(center_hz, origin_hz, s, tolerance_hz))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGIN: f64 = 851_012_500.0;

    #[test]
    fn on_grid_centres_fit_and_report_their_channel() {
        let f = ORIGIN + 3.0 * 12_500.0;
        let fit = best_lmr_raster(f, ORIGIN, RASTER_TOLERANCE_HZ).expect("on-grid");
        // 3 × 12.5 kHz is also 1.5 × 25 kHz, so the widest grid it truly lands on is 12.5 kHz.
        assert_eq!(fit.spacing_hz, 12_500.0);
        assert_eq!(fit.channel, 3);
        assert!(fit.offset_hz.abs() < 1e-6);
        assert!((fit.channel_hz() - f).abs() < 1e-6);
    }

    #[test]
    fn a_centre_between_channels_does_not_fit_any_lmr_raster() {
        // 6.25 kHz off a 12.5 kHz grid is exactly half a channel: the worst case, and it must
        // not be rescued by the 6.25 kHz raster either, which it also sits half-way across.
        let f = ORIGIN + 3.0 * 12_500.0 + 3_125.0;
        assert!(best_lmr_raster(f, ORIGIN, RASTER_TOLERANCE_HZ).is_none());
    }

    #[test]
    fn tolerance_is_a_window_not_a_snap() {
        let f = ORIGIN + 12_500.0 + 900.0;
        let fit = best_lmr_raster(f, ORIGIN, RASTER_TOLERANCE_HZ).expect("within tolerance");
        // The measured centre is kept; only the offset records the mismatch. Snapping a
        // measurement to a grid is exactly what the project forbids the known-signal database
        // from doing, and a raster is no different.
        assert!((fit.offset_hz - 900.0).abs() < 1e-6);
        assert!(best_lmr_raster(f, ORIGIN, 500.0).is_none());
    }

    #[test]
    fn a_degenerate_grid_is_rejected_rather_than_dividing_by_zero() {
        assert!(fit_raster(ORIGIN, ORIGIN, 0.0, RASTER_TOLERANCE_HZ).is_none());
        assert!(fit_raster(f64::NAN, ORIGIN, 12_500.0, RASTER_TOLERANCE_HZ).is_none());
    }
}
