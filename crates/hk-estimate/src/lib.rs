//! hackriff signal characterisation. Parameter estimation covers occupied bandwidth, CFO and
//! SNR (C13). Blind symbol estimation covers symbol rate, modulation order, deviation and
//! roll-off, with confidence (C14). Estimates feed demodulation so parameters are never picked
//! by hand.
//!
//! # C13 parameter estimation (T-010)
//!
//! The chain follows spike S5 (`spikes/s5-blind-estimation/REPORT.md` §2 steps 1–2, §5 T-010):
//!
//! 1. [`snippet`]: [`SnippetExtractor`] cuts a [`ChannelSnippet`] for one detection box through
//!    the hk-dsp [`Ddc`](hk_dsp::Ddc): centred on the box, at ≥ 1.25× a guarded bandwidth, with
//!    signal-free pre/post pads, the output→source time map and provenance. Boxes that cross
//!    ±fs/2 are extracted in a Nyquist-wrapped frame.
//! 2. [`params`]: [`ParamEstimator`] turns a snippet into a [`ParameterSet`]:
//!    - N0 = **mean** Welch density of a signal-free pad (never the median: S5 found it inflates
//!      SNR by ~+9 dB on pure noise), a caller-supplied floor, or in-channel sidebands.
//!    - OBW99 and x-dB bandwidths on the 5-bin-smoothed, noise-subtracted PSD.
//!    - SNR = unclipped in-band power / (N0·OBW99), over the box and re-measured over the burst
//!      extent (first→last sample above N0 + 6 dB after a ±0.75·OBW channel filter).
//!    - CFO: spectral centroid by default; x² line (DSB/BPSK) and x⁴ line (QPSK) on a family
//!      hint; on an FSK hint the mid-point of the *settled* levels (IF samples ≥ one unit
//!      interval from any mid-crossing). T-011 can refine it with symbol-centre clusters through
//!      [`params::cfo_from_fsk_levels`].
//!    - Spectral-shape features ([`ShapeFeatures`]) for the classifier.
//!
//! **Deviations from the S5 prototype.** (a) OBW99 integrates the *unclipped* noise-subtracted
//! PSD: S5 zeroed negative bins, which adds the positive noise half to the tails and widens
//! OBW99 with the snippet width (its 915 MHz truth bandwidths are 10–55 % above ours and above
//! Carson). (b) The centroid uses the same unclipped weights. (c) FSK CFO uses settled levels:
//! cluster means are pulled by bit imbalance (17–31 % of Rs on the synthetic bursts).
//!
//! **Floor.** Bandwidths and the centroid need the in-band power to be ≥ `min_band_z` (50)
//! standard errors above the noise, N0 uncertainty included (≈ 8 dB in-band for a 3 ms 915 MHz
//! burst). Below it SNR is still reported and the hinted line estimators still run under their
//! own significance tests.
//! 3. [`clock`]: tone frequency by phase slope and the pilot → clock ppm helper (feeds C05).
//! 4. [`normalise`]: [`NormalisedSnippet`] recentred at the CFO, resampled to a target
//!    samples-per-OBW, power-normalised, carrying the estimates, provenance and time map.
//!
//! **No silent guesses.** Every value is an [`Estimate`]: measured with a one-sigma
//! uncertainty, method and evidence, or abstained with a [`Reason`].
//!
//! **Known bias.** S5 (§3.2) measured box SNR ≈ 0 dB low at ≥ 20 dB true SNR and −1 to −5 dB
//! low at 9–15 dB (OBW99 shrinking as the skirts vanish into the noise); floors stated in
//! in-band SNR are therefore conservative in true SNR. With unclipped integration this crate's
//! synthetic sweep reads within ±0.3 dB at ≥ 15 dB and −0.6 to −1.1 dB at 10 dB, where noise in
//! the tails widens OBW99 by 10–20 %.
//!
//! **Cost.** Allocation is per snippet (buffers sized to the snippet), never per sample; Welch
//! engines and FFT plans are cached in the estimator. Each [`ParameterSet`] reports its own
//! estimation time.

pub mod clock;
pub(crate) mod dsp;
pub mod estimate;
pub mod normalise;
pub mod params;
pub mod snippet;

pub use estimate::{Estimate, Evidence, Method, Reason};
pub use normalise::{NormaliseConfig, NormaliseFlags, NormalisedSnippet, normalise};
pub use params::{
    CfoEstimates, EstimateFlags, EstimatorConfig, Extent, FamilyHint, Hints, NoiseReference,
    ParamEstimator, ParameterSet, ShapeFeatures, XdbBandwidth,
};
pub use snippet::{
    ChannelSnippet, EstimateError, SnippetConfig, SnippetExtractor, SnippetFlags, SnippetRequest,
};

/// Estimator id and version recorded in every [`ParameterSet`].
pub const ESTIMATOR_VERSION: &str = "hk-estimate/c13@0.1.0";

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_the_model() {
        let _ = hk_model::EmitterId::new();
    }
}
