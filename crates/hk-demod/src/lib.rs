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
//!   unrestricted), the Emitter identity `rds-pi` and a label from the most frequent PS frame;
//!   [`write_declined`] writes the Demodulation alone for a probe that measured a window and
//!   declined it, so a refusal is distinguishable from never having looked (T-416).
//!
//! Not yet: NBFM/AM/SSB/CW audio, squelch, AGC, CTCSS/DCS, stereo L−R audio, RDS without a
//! pilot, RadioText.
//!
//! # C20 digital demodulation: 2-FSK / GFSK (T-013, AWARE-036)
//!
//! [`fsk`]: discriminator demodulator with T-011-seeded Gardner timing and soft values,
//! prior-led trial demodulation below the C14 trust floor, framed records (content fails
//! closed unless the caller classifies the emitter) and a gated `bits` stream. Framing inference
//! is [`hk_estimate::framing`].

/// Streaming analog audio with squelch and AGC for Listen (T-043).
pub mod audio;
/// Streaming FIR decimator, discriminator and de-emphasis (public for the decoder-workbench
/// blocks, ADR-0011 §1.6).
pub mod dsp;
/// C20 2-FSK/GFSK demodulation, prior-led trials, framed records and bits streams (T-013).
pub mod fsk;
pub mod mode;
pub mod pilot;
pub mod rds;
pub mod receiver;
pub mod record;
/// Output-driven parameter refinement: the generic loop and the WFM objective (T-070).
pub mod refine;
pub mod wfm;

pub use mode::{
    AdjacentChannels, AnalogMode, MODE_RULES_VERSION, ModeCandidate, ModeConfig, ModeDecision,
    ModeFeatures, ModeSelector, PilotCheck,
};
pub use pilot::{PilotConfig, PilotPll, PilotReport};
pub use rds::{RdsConfig, RdsDecoder, RdsDemod, RdsReport};
pub use receiver::{
    AnalogReceiver, AnalogSession, AudioBuffer, DemodError, MpxTimeMap, ReceiverConfig,
};
pub use record::{
    RecordContext, WrittenSession, rds_decodes, rds_label, write_declined, write_session,
};
pub use wfm::{MPX_RATE_HZ, WfmConfig, WfmDemod, WfmReport};

/// Demodulator id and version recorded in `Demodulation.demod_version`.
pub const DEMOD_VERSION: &str = "hk-demod/c19-analog@0.1.0";
/// RDS decoder id (`Decode.decoder_id`).
pub const RDS_DECODER_ID: &str = "hk-rds";
/// RDS decoder version (`Decode.decoder_version`).
pub const RDS_DECODER_VERSION: &str = "0.1.0";
