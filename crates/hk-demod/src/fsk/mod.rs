//! C20 digital demodulation for 2-FSK / GFSK bursts, and the path from bursts to framed,
//! gated records (T-013, AWARE-036).
//!
//! - [`demod`]: [`FskDemod`] — CFO-corrected snippet → channel filter → quadrature
//!   discriminator → integrate (moving average over 0.7 symbol) → level mid-point → timing seed
//!   (T-011 [`rate_transitions_ls`](hk_estimate::blind::transitions::rate_transitions_ls) rate +
//!   transition phase fit, or a 16-phase eye search) → Gardner loop (BnT 0.01, ζ 0.707) → hard
//!   bits, LLR-like soft values, deviation at settled symbols, lock quality (timing RMS, eye
//!   opening, slips) and a per-symbol source-index time map.
//! - [`receiver`]: [`FskReceiver`] — box → snippet at ≥ 6 × box bandwidth → C13 (FSK hint) →
//!   C14 → demodulation. Below the C14 trust floor it runs **prior-led trials**: emitter-cluster
//!   priors ([`ClusterPrior::from_trusted`]: rate/deviation/raster from trusted bursts), then a
//!   standard-rate table, confirmed by a [`SyncPrior`] from a framing model, and reports the
//!   prior used ([`DemodSeed`]).
//! - Framing inference across bursts lives in [`hk_estimate::framing`].
//! - [`record`]: [`write_framed_bursts`] — Emitter, Demodulation, Decode, Bitstream descriptor,
//!   ground-truth Annotation, known status. **Content fails closed** (metadata-only) unless the
//!   caller classifies the emitter.
//! - [`stream`]: [`publish_framed_bits`] — a gated hk-stream `bits` stream whose class is the
//!   emitter's.

pub mod demod;
pub mod receiver;
pub mod record;
pub mod stream;

pub use demod::{
    FSK_DEMOD_VERSION, FskDemod, FskDemodConfig, FskDemodError, FskDemodRequest, FskLock,
    FskSymbols, TimingSeedMethod,
};
pub use receiver::{
    ClusterPrior, DemodPriors, DemodSeed, FskBurst, FskReceiver, FskReceiverConfig,
    STANDARD_RATES_BD, SeedSource, SyncPrior, TrialOutcome,
};
pub use record::{
    EmitterClassification, FRAMING_IDENTITY_SCHEME, FramedRecordContext, INFER_DECODER_ID,
    INFER_DECODER_VERSION, WrittenFraming, effective_content_class, framing_identity,
    write_framed_bursts,
};
pub use stream::{BitsPublishStats, bits_stream_header, publish_framed_bits};
