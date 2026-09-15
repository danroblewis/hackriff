//! Radiometry (C33, T-021): the calibrated noise-floor product for SPACE-050.
//!
//! - [`calibration`]: the dBFS → dBm power table `K(f, gain)` loaded from a
//!   [`hk_model::CalibrationState`], per exact gain setting, interpolated in frequency, with
//!   uncertainty and validity; never faked (uncalibrated readings stay dBFS/Hz).
//! - [`series`]: [`FloorPoint`] / [`FloorSeries`], the calibrated slow-floor series from
//!   [`FloorFrame`](crate::floor::FloorFrame)s: dBm/Hz, noise temperature, dB above `kT₀`,
//!   uncertainty `√(σ_model² + σ_stat² + σ_cal²)`, and [`FloorFlags`].
//! - [`bias`]: the Gamma-derived percentile bias of spectrum-history cells, which hk-store's
//!   floor-vs-time product removes from tile p10s.
//! - [`p372`]: an optional ITU-R P.372 comparison hook (coefficients unverified).
//!
//! Units follow [`crate::spectrum`] (linear FS²/Hz, full-scale complex sinusoid = 0 dBFS) and
//! [`hk_model::PowerUnit`] (`Dbfs` / `Dbm`, both per Hz here).

pub mod bias;
pub mod calibration;
pub mod p372;
pub mod series;

pub use bias::{
    averaged_bin_covariance, bin_power_correlation, cell_value_sd_db, cell_value_shape,
    exact_percentile_probability, mixture_percentile_bias_db, percentile_bias_db,
};
pub use calibration::{
    BandCal, CalPoint, CalValue, PowerCalTable, PowerCalibrations, SyntheticCalSegment,
    UncalibratedReason, gain_setting_of, same_gain, synthetic_calibration_state,
};
pub use p372::{NoiseEnvironment, P372Comparison, compare_p372};
pub use series::{
    BOLTZMANN_J_PER_K, FloorFlags, FloorPoint, FloorSeries, FloorSeriesConfig, T0_K,
    calibrate_band, calibrate_channel, combine_uncertainty, db_above_kt0, frame_flags,
    frame_period_ns, kt0_dbm_per_hz, noise_temperature_k,
};
