//! Where C14 blind symbol estimation runs for the classification path (T-238).
//!
//! `features@1` carries six symbol-derived dimensions — `cyclic_db`, `obw_over_rs` and C14's four
//! family scores — and until this module existed **every one of them abstained**, because no call
//! site ever passed a [`SymbolParameters`]. Six of the most discriminating dimensions were dead,
//! and every classification carried the reason `no_symbol_estimate` (measured on both OTA
//! fixtures, T-235).
//!
//! # Why C14 cannot reuse the classifier's snippet
//!
//! The classifier is handed a snippet at [`crate::synth::SAMPLES_PER_OBW`] = 2 samples per OBW99,
//! which is what `hk_estimate::normalise` delivers by default. C14 measures a **symbol clock**, and
//! a symbol rate is typically within a small factor of the occupied bandwidth — so at 2 samples per
//! OBW99 a symbol period is about two samples, and C14's own search range (`rate_max_fs` = fs/2.5)
//! stops below it. There is no cyclic line left to find. Handing C14 the classifier's samples would
//! therefore produce an abstention dressed up as an invocation.
//!
//! So C14 keeps its own geometry, exactly as it does in production: [`BlindEstimator::prepare`]
//! re-normalises the snippet to [`SYMBOL_SAMPLES_PER_OBW`] before estimating. Both entry points
//! below preserve that — [`SymbolEstimator::from_snippet`] by going through C14's own preparation,
//! and [`SymbolEstimator::from_samples`] by taking a view the caller already decimated to that
//! geometry (the synthetic grid's [`crate::synth::SynthSignal::symbol_samples`]).
//!
//! # Abstention stays honest
//!
//! When C14 could not look at the window at all — too few samples, or no bandwidth to scale its
//! constants — it returns an all-default [`SymbolParameters`] whose family scores are all `0.0`.
//! Feeding that to [`crate::features`] would put four fabricated zeros into the vector and let a
//! class be scored on them. Both entry points return `None` in that case, so the features abstain
//! with `no_symbol_estimate` instead. Everything C14 reports *after* it has run is already an
//! honest measurement or an explicit abstention: `symbol_rate_bd` has a value only when the rate is
//! **trusted**, `cyclic_db` is absent when no line was found, and a family score of 0 means
//! "measured, no evidence" rather than "not measured".
//!
//! # Cost, and where it runs
//!
//! Per classification event on the CPU, at the same call site as the feature vector and
//! `family::explain_emitter` — off the ring and DSP real-time threads (ADR-0007; ADR-0016 §4,
//! "Placement"). Every estimate reports its own [`SymbolParameters::cost_us`], and the estimator
//! caches its FFT plans across windows, which is why callers hold one [`SymbolEstimator`] rather
//! than building one per event.
//!
//! Cost is bounded **by the window, not by a timeout**: C14 is given at most
//! [`MAX_WINDOW_SAMPLES`] samples, so its cost has a ceiling that does not grow with the length of
//! the recording. This is not a detail — a classification of a *continuous* emission would
//! otherwise hand C14 the whole extent: measured on the FM broadcast fixture, a one-second snippet
//! cost **4.9 seconds** of C14 time, which is not a per-event cost any real-time system can carry.
//! Truncating costs nothing a symbol-rate estimate needs (see [`MAX_WINDOW_SAMPLES`]).

use hk_estimate::blind::{BlindEstimator, BlindInput, BlindReason, ObwSource, SymbolParameters};
use hk_estimate::{ChannelSnippet, ParameterSet};
use num_complex::Complex32;

/// Samples per OBW99 C14 analyses at: `BlindConfig::default().samples_per_obw`, the geometry
/// [`BlindEstimator::prepare`] normalises to in production.
pub const SYMBOL_SAMPLES_PER_OBW: f64 = 6.0;

/// Channel bandwidth of a prepared C14 window, as a multiple of OBW99: C14's own channel filter is
/// ±0.75 × OBW99 (`BlindConfig::channel_filter_obw`).
const CHANNEL_BANDWIDTH_OBW: f64 = 1.5;

/// Largest window C14 analyses for a classification, in samples: what bounds its cost.
///
/// At [`SYMBOL_SAMPLES_PER_OBW`] this is about 11 000 symbol periods for a symbol rate near the
/// occupied bandwidth — orders of magnitude more than a rate estimate needs. The periodogram
/// resolution over this window is `fs/N` ≈ OBW/11 000, far finer than C14's own ±1 % rate
/// tolerance, and its consensus rule wants independent line evidence rather than a longer record.
/// So the cap costs the estimate nothing measurable while turning an unbounded per-event cost into
/// a bounded one. A burst shorter than this is unaffected, which is every keyed emission; it binds
/// only on continuous ones, where the extent is as long as the dwell.
pub const MAX_WINDOW_SAMPLES: usize = 65_536;

/// A C14 estimate together with **the window it was measured on** (T-200).
///
/// The post-sync verifier needs both: the trusted symbol rate, and the samples at the geometry that
/// rate was measured at. Recovering the window separately would re-do C14's preparation and could
/// drift from it; returning them together makes "the verifier tests the same samples C14 synced on"
/// true by construction.
#[derive(Clone, Debug)]
pub struct SymbolWindow {
    /// What C14 measured.
    pub params: SymbolParameters,
    /// The prepared window, at [`SYMBOL_SAMPLES_PER_OBW`] and capped at [`MAX_WINDOW_SAMPLES`].
    pub samples: Vec<Complex32>,
    /// Sample rate of [`SymbolWindow::samples`], Hz.
    pub sample_rate_hz: f64,
}

/// The C14 estimator used by the classification path, with its FFT plans cached across events.
#[derive(Default)]
pub struct SymbolEstimator {
    inner: BlindEstimator,
}

impl SymbolEstimator {
    /// A new estimator at C14's default configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// The underlying C14 estimator (for a caller that needs its configuration).
    pub fn inner(&mut self) -> &mut BlindEstimator {
        &mut self.inner
    }

    /// Runs C14 over a detection box the way the pipeline does: C14 prepares the snippet at its own
    /// geometry from the C13 [`ParameterSet`], so the classifier's 2-samples-per-OBW99 view is not
    /// reused.
    ///
    /// `None` when C14 could not run at all (see the module docs): the features then abstain rather
    /// than reading an all-default vector as a measurement.
    pub fn from_snippet(
        &mut self,
        snippet: &ChannelSnippet,
        params: &ParameterSet,
    ) -> Option<SymbolParameters> {
        self.window_from_snippet(snippet, params).map(|w| w.params)
    }

    /// [`Self::from_snippet`], also returning the prepared window the estimate was measured on —
    /// what the T-200 verifier tests its likelihoods over.
    pub fn window_from_snippet(
        &mut self,
        snippet: &ChannelSnippet,
        params: &ParameterSet,
    ) -> Option<SymbolWindow> {
        // `prepare` is what puts the snippet at C14's own geometry; going through it by hand
        // (rather than `estimate_snippet`) is what lets the window be capped before estimating.
        // A snippet C14 cannot prepare is one it cannot run on at all.
        let window = self.inner.prepare(snippet, params).ok()?;
        let input = window.input();
        let n = input.samples.len().min(MAX_WINDOW_SAMPLES);
        let samples = input.samples[..n].to_vec();
        let sample_rate_hz = input.sample_rate_hz;
        let params = ran(self.inner.estimate(&input.window(0..n)))?;
        Some(SymbolWindow {
            params,
            samples,
            sample_rate_hz,
        })
    }

    /// Runs C14 over samples the caller has already brought to [`SYMBOL_SAMPLES_PER_OBW`],
    /// recentred on the emission (the synthetic grid's symbol view).
    ///
    /// `None` on the same terms as [`Self::from_snippet`].
    pub fn from_samples(
        &mut self,
        samples: &[Complex32],
        sample_rate_hz: f64,
        obw_hz: Option<f64>,
        snr_db: Option<f64>,
    ) -> Option<SymbolParameters> {
        // Without a measured OBW99 there is nothing to scale C14's constants or bound its search
        // with, and C14 itself would abstain on every field. Saying so here keeps the "it ran"
        // test in one place.
        let obw_hz = obw_hz.filter(|o| o.is_finite() && *o > 0.0)?;
        if !(sample_rate_hz.is_finite() && sample_rate_hz > 0.0) {
            return None;
        }
        ran(self.inner.estimate(&BlindInput {
            samples: &samples[..samples.len().min(MAX_WINDOW_SAMPLES)],
            sample_rate_hz,
            obw_hz,
            obw_source: ObwSource::Obw99,
            snr_ext_db: snr_db,
            // Measured from the samples by C14 itself when absent.
            noise_power: None,
            channel_bandwidth_hz: (CHANNEL_BANDWIDTH_OBW * obw_hz).min(sample_rate_hz),
            // The caller's symbol view is recentred on the emission, as C13 recentres a snippet.
            center_offset_hz: 0.0,
        }))
    }
}

/// `Some` only when C14 actually analysed the window.
///
/// [`BlindReason::TooShort`] and [`BlindReason::InvalidInput`] are the two cases where C14 returns
/// before measuring anything, leaving default family scores of `0.0`. Those are not measurements,
/// and passing them on would be exactly the fabricated-value failure T-235 reverted twice.
fn ran(p: SymbolParameters) -> Option<SymbolParameters> {
    let refused = p
        .reasons
        .iter()
        .any(|r| matches!(r, BlindReason::TooShort | BlindReason::InvalidInput));
    (!refused).then_some(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::{Class, SynthConfig, generate};

    /// The honesty property of this module: when C14 cannot look at a window, the caller is told
    /// nothing rather than told zeros. A fabricated family score is what makes a classifier
    /// confidently wrong (T-235), so this is asserted, not commented.
    #[test]
    fn a_window_c14_cannot_analyse_yields_no_estimate_rather_than_zeros() {
        let mut e = SymbolEstimator::new();
        // Too short for C14's minimum window.
        assert!(
            e.from_samples(
                &[Complex32::new(0.5, 0.5); 16],
                1e6,
                Some(100e3),
                Some(30.0)
            )
            .is_none()
        );
        // No measured OBW99: nothing scales C14's constants.
        let s = generate(Class::Fsk2, &SynthConfig::new(30.0, 11));
        assert!(
            e.from_samples(&s.symbol_samples, s.symbol_sample_rate_hz, None, Some(30.0))
                .is_none()
        );
        // An invalid rate is refused too, rather than producing a default vector.
        assert!(
            e.from_samples(&s.symbol_samples, 0.0, Some(s.obw_hz), Some(30.0))
                .is_none()
        );
    }

    /// C14 at its own geometry finds the symbol structure of a keyed waveform that the classifier's
    /// 2-samples-per-OBW99 view cannot show it. This is the whole reason the symbol view exists.
    #[test]
    fn c14_sees_a_symbol_clock_at_its_own_geometry_and_not_at_the_classifiers() {
        let mut e = SymbolEstimator::new();
        let s = generate(Class::Fsk2, &SynthConfig::new(30.0, 11));
        let at_c14 = e
            .from_samples(
                &s.symbol_samples,
                s.symbol_sample_rate_hz,
                Some(s.obw_hz),
                Some(30.0),
            )
            .expect("C14 ran");
        let best = |p: &SymbolParameters| {
            p.lines
                .iter()
                .map(|l| l.significance_db)
                .fold(f64::NEG_INFINITY, f64::max)
        };
        assert!(
            best(&at_c14) > 12.0,
            "a 2-FSK burst has a cyclic line at C14's geometry: {:.1} dB",
            best(&at_c14)
        );
        assert!(
            at_c14.family_scores.fsk > 0.5,
            "and it scores as FSK: {:?}",
            at_c14.family_scores
        );
        assert!(
            s.symbol_sample_rate_hz > s.sample_rate_hz,
            "the symbol view is the wider-rate one ({} vs {})",
            s.symbol_sample_rate_hz,
            s.sample_rate_hz
        );
    }
}
