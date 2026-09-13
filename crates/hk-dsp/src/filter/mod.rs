//! FIR design and the inner-loop kernels shared by the channelizer ([`crate::channelizer`]) and
//! the DDC ([`crate::ddc`]).
//!
//! - [`design`]: Kaiser-window low-pass design with a verified [`Response`], the polyphase
//!   prototype variant and the 2× oversampled PFB prototype ([`pfb_prototype`]).
//! - [`kernels`]: lane-split dot products and the polyphase folds.
//! - [`nco`]: the exact `u64` phase-increment [`Nco`] for absolute-index mixing.

pub mod design;
pub(crate) mod history;
pub mod kernels;
pub mod nco;

pub use design::{
    DEFAULT_STOPBAND_DB, DesignError, FirDesign, LowpassSpec, MAX_TAPS, Response, design_lowpass,
    design_lowpass_with, kaiser_beta, kaiser_taps, measure_response, pfb_prototype,
};
pub use nco::Nco;
