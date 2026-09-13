//! hackriff demodulation. Analog auto-mode AM/FM/WFM with estimated squelch and AGC, plus RDS
//! (C19). Own digital demodulators (FSK/PSK/QAM to soft symbols and bits, C20) feed Bitstreams
//! and decoder plugins. GPL decoders stay out of this crate, behind the plugin process boundary
//! (ADR-0010).
//!
//! # C19 analog auto-mode and RDS (T-012, SIGNAL-062)
//!
//! - [`mode`]: [`ModeSelector`] picks WFM / NBFM / AM / SSB / CW / unknown from the T-010
//!   [`hk_estimate::ParameterSet`] plus an envelope-variation measurement and a trial-discriminator
//!   19 kHz pilot check. No manual mode input; every decision carries confidence, features and
//!   candidates, or is `unknown` with a reason.
//! - [`receiver`]: [`AnalogReceiver`] runs estimate → mode → demodulation for one channel
//!   request and returns an [`AnalogSession`].
//! - [`wfm`]: [`WfmDemod`] — discriminator → MPX, pilot PLL ([`pilot`]), de-emphasised mono
//!   audio at 48 kS/s, RDS.
//! - [`rds`]: 57 kHz subcarrier from 3 × pilot, biphase matched filter with timing recovery,
//!   differential decoding, EN 50067 block sync/syndromes, 0A/0B groups → PI, scrolling PS
//!   frames, PTY, TP/TA, block and group error rates.
//! - [`record`]: [`write_session`] writes the Demodulation, RDS Decode rows (content class
//!   unrestricted), the Emitter identity `rds-pi` and a label from the most frequent PS frame.
//!
//! Not yet: NBFM/AM/SSB/CW audio, squelch, AGC, CTCSS/DCS, stereo L−R audio, RDS without a
//! pilot, RadioText.

pub(crate) mod dsp;
pub mod mode;
pub mod pilot;
pub mod rds;
pub mod receiver;
pub mod record;
pub mod wfm;

pub use mode::{
    AnalogMode, MODE_RULES_VERSION, ModeCandidate, ModeConfig, ModeDecision, ModeFeatures,
    ModeSelector, PilotCheck,
};
pub use pilot::{PilotConfig, PilotPll, PilotReport};
pub use rds::{RdsConfig, RdsDecoder, RdsDemod, RdsReport};
pub use receiver::{
    AnalogReceiver, AnalogSession, AudioBuffer, DemodError, MpxTimeMap, ReceiverConfig,
};
pub use record::{RecordContext, WrittenSession, rds_decodes, rds_label, write_session};
pub use wfm::{MPX_RATE_HZ, WfmConfig, WfmDemod, WfmReport};

/// Demodulator id and version recorded in `Demodulation.demod_version`.
pub const DEMOD_VERSION: &str = "hk-demod/c19-analog@0.1.0";
/// RDS decoder id (`Decode.decoder_id`).
pub const RDS_DECODER_ID: &str = "hk-rds";
/// RDS decoder version (`Decode.decoder_version`).
pub const RDS_DECODER_VERSION: &str = "0.1.0";
