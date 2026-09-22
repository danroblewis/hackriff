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

/// The receiver's own offset from a channel grid, fitted from the spectrum (docs/19 §4.4 step 1,
/// §7.6a).
///
/// **A receiver clock error is not a property of any signal.** A HackRF One has a plain crystal
/// and no TCXO; this project's own unit measures **−9.6 ppm**, which at 852 MHz is −8.2 kHz —
/// ⅔ of a 12.5 kHz channel and 5.5× [`RASTER_TOLERANCE_HZ`]. Every emission in a capture moves by
/// the same constant, so a raster fit that assumes the receiver is on frequency rejects the whole
/// band at once: `docs/19 §7.6a` observed exactly that, nine emissions 29–80 kHz off the published
/// raster until one constant correction put every one of them back with a 390 Hz median residual.
///
/// **A build that only works at 0 ppm works on synthetic IQ and nothing else**, so fitting this is
/// a capability rather than a workaround, and it is cheap: the grid is periodic, so the offset is
/// the argument of the power-weighted sum of `exp(j2πf/spacing)` over the occupied bins — the
/// standard circular mean, and the same estimator whether one channel is occupied or nine.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridFit {
    /// Grid spacing fitted against, Hz.
    pub spacing_hz: f64,
    /// Offset of the emissions from the assumed grid origin, in `(-spacing/2, spacing/2]`, Hz.
    ///
    /// Add it to the assumed origin and the grid lines up with what was received. It is reported
    /// **modulo the spacing**, because that is all the data says: which grid line an emission is
    /// on is a different question from where the lines are.
    pub offset_hz: f64,
    /// Concentration of the fit, 0–1 — the resultant length of the circular mean.
    ///
    /// 1 is every bin at one phase; 0 is uniform. It is *not* a probability: a single 12.5 kHz
    /// channel's own energy spans ±4.5 kHz of its centre, so even a perfect fit over one clean
    /// channel concentrates only to ~0.35. See [`MIN_GRID_CONCENTRATION`].
    pub concentration: f64,
    /// Bins the fit was computed over.
    pub bins: usize,
}

/// Least concentration a [`GridFit`] must reach before it is worth correcting by.
///
/// **A priori, from the geometry and not from a run.** A narrowband channel occupying the full
/// ±`spacing/2` would give `sinc(π) = 0`; a 12.5 kHz LMR channel occupying ±4.5 kHz of it gives
/// `sin(x)/x` at `x = 2π·4.5/12.5/2 ≈ 1.13`, i.e. ≈ 0.80 — and half that again once noise bins
/// and a second, differently-shaped emission are folded in. 0.25 sits below every one of those
/// and well above what unstructured noise reaches (which falls as `1/√bins`).
pub const MIN_GRID_CONCENTRATION: f64 = 0.25;

/// Fits the grid offset of `spacing_hz` over `bins` of `(frequency relative to the assumed origin,
/// power)`, counting only bins at or above `floor`.
///
/// `None` when nothing is above the floor or the fit does not concentrate
/// ([`MIN_GRID_CONCENTRATION`]) — **an unfitted grid is left uncorrected rather than corrected by a
/// guess**, which is the same rule the raster tolerance itself follows: a hint that does not hold
/// up buys nothing.
pub fn fit_grid_offset<I>(bins: I, floor: f64, spacing_hz: f64) -> Option<GridFit>
where
    I: IntoIterator<Item = (f64, f64)>,
{
    if !(spacing_hz.is_finite() && spacing_hz > 0.0) {
        return None;
    }
    let k = std::f64::consts::TAU / spacing_hz;
    let (mut re, mut im, mut w, mut n) = (0.0f64, 0.0f64, 0.0f64, 0usize);
    for (hz, power) in bins {
        if !(hz.is_finite() && power.is_finite() && power >= floor && power > 0.0) {
            continue;
        }
        let (s, c) = (hz * k).sin_cos();
        re += power * c;
        im += power * s;
        w += power;
        n += 1;
    }
    if n == 0 || w <= 0.0 || !w.is_finite() {
        return None;
    }
    let concentration = (re * re + im * im).sqrt() / w;
    // NaN-safe by construction: `w > 0` and every summed term is finite, so `concentration` is
    // finite here; a fit that does not hold up is left uncorrected rather than guessed at.
    if concentration < MIN_GRID_CONCENTRATION {
        return None;
    }
    // atan2 lands in (-pi, pi], so the offset lands in (-spacing/2, spacing/2] by construction:
    // the nearest grid line, never a wrap to the one beyond it.
    Some(GridFit {
        spacing_hz,
        offset_hz: im.atan2(re) / k,
        concentration,
        bins: n,
    })
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

    /// The test the whole clock-error path exists for, at the number this project **measured**
    /// on its own HackRF One (`docs/19 §7.6a`): −8200 Hz at 852 MHz, −9.6 ppm.
    #[test]
    fn a_measured_receiver_clock_error_is_fitted_from_the_spectrum() {
        // Three emissions on the 12.5 kHz grid, all shifted by the same LO error -- which is what
        // an LO error does. Each spreads +/-4.5 kHz of its own centre, like a 12.5 kHz channel.
        const ERR: f64 = -8_200.0;
        let mut bins = Vec::new();
        for ch in [-2.0, 0.0, 5.0] {
            let centre = ch * 12_500.0 + ERR;
            let mut hz = centre - 4_500.0;
            while hz <= centre + 4_500.0 {
                bins.push((hz, 1.0));
                hz += 100.0;
            }
        }
        let fit = fit_grid_offset(bins, 0.5, 12_500.0).expect("three emissions on one grid");
        // The offset is reported modulo the spacing: -8200 and +4300 name the same grid.
        let wrapped = ERR + 12_500.0;
        assert!(
            (fit.offset_hz - wrapped).abs() < 150.0,
            "fitted {:.0} Hz, wanted {wrapped:.0} Hz (the -8200 Hz error, mod 12.5 kHz)",
            fit.offset_hz,
        );
        // And the correction puts the emissions back on a grid through the fitted origin.
        let origin = fit.offset_hz;
        for ch in [-2.0, 0.0, 5.0] {
            let centre = ch * 12_500.0 + ERR;
            assert!(
                best_lmr_raster(centre, origin, RASTER_TOLERANCE_HZ).is_some(),
                "{centre:.0} Hz must fit the corrected grid",
            );
            assert!(
                best_lmr_raster(centre, 0.0, RASTER_TOLERANCE_HZ).is_none(),
                "{centre:.0} Hz must NOT fit the uncorrected one -- otherwise this proves nothing",
            );
        }
    }

    /// A receiver that really is on frequency must not be "corrected" off it.
    #[test]
    fn an_accurate_receiver_fits_a_zero_offset() {
        let bins: Vec<(f64, f64)> = (-45..=45)
            .map(|i| (f64::from(i) * 100.0, 1.0))
            .chain((-45..=45).map(|i| (12_500.0 + f64::from(i) * 100.0, 1.0)))
            .collect();
        let fit = fit_grid_offset(bins, 0.5, 12_500.0).expect("on-grid energy");
        assert!(
            fit.offset_hz.abs() < 100.0,
            "fitted {:.0} Hz",
            fit.offset_hz
        );
    }

    /// Noise must not produce a correction. Unstructured energy concentrates as `1/sqrt(bins)`,
    /// which is what [`MIN_GRID_CONCENTRATION`] is set above -- so the honest answer is `None`,
    /// and the grid is left where it was rather than moved by a guess.
    #[test]
    fn unstructured_energy_fits_nothing_rather_than_a_guess() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let bins: Vec<(f64, f64)> = (0..4000)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                let u = (state >> 11) as f64 / (1u64 << 53) as f64;
                (u * 1.0e6 - 0.5e6, 1.0)
            })
            .collect();
        assert!(fit_grid_offset(bins, 0.5, 12_500.0).is_none());
    }

    /// Bins below the floor are noise and must not vote.
    #[test]
    fn the_floor_excludes_bins_rather_than_weighting_them_down() {
        let mut bins: Vec<(f64, f64)> = (-45..=45)
            .map(|i| (4_300.0 + f64::from(i) * 100.0, 10.0))
            .collect();
        // A wall of sub-floor energy at the wrong phase, heavy enough to drag a weighted mean.
        bins.extend((0..4000).map(|i| (f64::from(i) * 3.0 - 6_000.0, 0.01)));
        let fit = fit_grid_offset(bins, 1.0, 12_500.0).expect("the above-floor emission");
        assert!((fit.offset_hz - 4_300.0).abs() < 150.0, "{:?}", fit);
        assert_eq!(fit.bins, 91, "only the above-floor bins voted");
    }

    #[test]
    fn a_degenerate_grid_or_an_empty_spectrum_fits_nothing() {
        assert!(fit_grid_offset([(0.0, 1.0)], 0.5, 0.0).is_none());
        assert!(fit_grid_offset([(0.0, 1.0)], 0.5, f64::NAN).is_none());
        assert!(fit_grid_offset(Vec::<(f64, f64)>::new(), 0.5, 12_500.0).is_none());
        assert!(fit_grid_offset([(f64::NAN, 1.0)], 0.5, 12_500.0).is_none());
        assert!(fit_grid_offset([(0.0, f64::NAN)], 0.5, 12_500.0).is_none());
    }
}
