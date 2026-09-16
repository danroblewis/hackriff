//! Trunking control-channel hunting (C23, T-267).
//!
//! Two stages that must not be conflated:
//!
//! 1. **Candidacy** ([`raster`]): a channel is continuously occupied and lands on the LMR raster.
//!    Cheap, spectral, and *not evidence of a control channel* — continuous data emitters look
//!    exactly like this, which is C23's named false-CC pitfall.
//! 2. **Confirmation** ([`confirm`]): the channel's demodulated symbols carry a known frame sync
//!    **and** CRC-valid blocks.
//!
//! The types enforce the order. See [`confirm`] for how a [`confirm::ConfirmedCc`] is made
//! unconstructible without evidence.

pub mod confirm;
pub mod raster;

pub use confirm::{
    BLOCK_BYTES, BLOCK_DIBITS, CONFIRMED_FALSE_ALARM_MAX, CcCandidate, CcConfirmConfig,
    CcConfirmer, CcEvidence, ConfirmedCc, FRAME_DIBITS, MIN_CC_FCO, MIN_CRC_VALID, MIN_SYNC_HITS,
    P25_FRAME_SYNC_DIBITS, SYNC_FALSE_ALARM_FLOOR, SYNC_TOLERANCE_DIBITS, ScanOutcome,
};
pub use raster::{LMR_RASTERS_HZ, RASTER_TOLERANCE_HZ, RasterFit, best_lmr_raster, fit_raster};
