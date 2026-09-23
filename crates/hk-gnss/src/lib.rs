//! C36 GNSS observables — **and the one documented exception to this project's blind-first
//! rule**.
//!
//! # The exception, stated out loud
//!
//! CLAUDE.md's central rule is that signals are found by blind detection from RF data first, and
//! that the known-signal database never leads: it is never the starting point, never
//! pre-populates the inventory, and never overrides what was measured.
//!
//! **GPS L1 C/A cannot be found that way, because there is nothing there to find.** The signal
//! arrives at roughly −128 dBm into a bandwidth of about 2 MHz, which puts it **20–30 dB below
//! the thermal noise floor**. It is not weak-but-visible; it is invisible. No amount of CFAR,
//! spectral kurtosis, or cyclostationary search finds a bump that does not exist, and the
//! project must not pretend otherwise or quietly tune a "sensitivity" knob until something
//! appears. `tests/below_noise_acquisition.rs` measures this directly: the periodogram of
//! signal-plus-noise is statistically indistinguishable from the periodogram of noise alone,
//! while known-code correlation on *that same IQ* recovers the satellite cleanly.
//!
//! Recovery requires **despreading against the published PRN codes** ([`prn`]): the known-signal
//! pipeline *leading* rather than *suggesting*. GNSS-SDR is the reference implementation for the
//! full receiver.
//!
//! # Why the exception does not leak
//!
//! A comment saying "GNSS only" is not a boundary. Three things confine it, in decreasing order
//! of strength:
//!
//! 1. **The crate dependency graph (compile-time, machine-checked).** The blind detection path is
//!    `hk-detect`, built on `hk-core`/`hk-dsp`/`hk-estimate`/`hk-model`. None of them depends on
//!    this crate, and none of them depends on `hk-context` (the band-plan prior crate) either.
//!    A `PrnCodebook` is therefore *un-nameable* inside `hk-detect`: reaching it would require
//!    adding a dependency edge to `crates/hk-detect/Cargo.toml`, which is a visible, reviewable
//!    act. `tests/blind_path_boundary.rs` parses those manifests and **fails** if the edge
//!    appears. The exception cannot be reached from the ordinary path by accident.
//! 2. **The detector's input type.** `hk_detect::Detector::process` takes a `SpectrumFrame`, a
//!    `FloorFrame` and a clip count — measurements only. Its config carries receiver knowledge
//!    (tuned-centre sensitivity profiles, hardware filter edges, a measured spur mask) and
//!    nothing that knows what a frequency *means*. There is no seam to pass a code, a
//!    frequency list or an allocation into detection, so the leak has no shape to take.
//! 3. **A witness on the entry point.** [`acquire`] demands a [`KnownCodeLed`], whose only
//!    constructor takes a [`PrnCodebook`]. Every known-code-led call site therefore names the
//!    exception in its own signature and is greppable, and every result carries
//!    [`AcquisitionEvidence`] recording which codebook led it. This is the weakest of the three
//!    — a marker, not a wall — and it exists so review can *see* the path, not to enforce it.
//!
//! # The blind half stays blind
//!
//! The on-mission, attack-map half of GNSS is **jamming and spoofing**, and it is blindly
//! detectable precisely because a jammer sits *above* the noise floor. [`integrity`] keeps it
//! that way:
//!
//! - [`assess_jamming`] takes power-domain evidence and only *optionally* takes receiver
//!   observables. With `None` for the observables — no codes, no correlator, no receiver running
//!   at all — a floor rise alone still raises a jamming suspicion. That is asserted by
//!   `integrity::tests::jamming_fires_without_any_observables`, so the blind half can never
//!   silently acquire a dependency on the exception.
//! - Nothing in this crate tells the detector where to look. A GNSS floor rise is found wherever
//!   it happens, by the ordinary blind path, and the band plan then *suggests* "GNSS allocation
//!   here" through the existing `hk-context` machinery — allocation-only explanations already
//!   score low and cannot set an emitter's status. That is suggesting, not leading, and it is
//!   unchanged by this crate.
//!
//! # What is here and what is not
//!
//! Built and tested offline: the PRN codebook ([`prn`]), FFT parallel code-phase acquisition
//! ([`acquire`]), observable epochs and the S4 scintillation index ([`observable`]), and blind
//! jamming plus spoofing tell-tales ([`integrity`]).
//!
//! **Not built here, by design:** tracking loops (DLL/PLL), navigation message decode,
//! ephemeris handling and PVT. Those come from **GNSS-SDR wrapped as a C22 plugin** (T-323):
//! [`receiver`] generates its config and reads its RINEX/NMEA output back into this crate's
//! types, and the `hk-plugin-gnss-sdr` binary (`plugins/gnss-sdr/manifest.json`) runs it as a
//! subprocess over one recorded dwell — GPL-3.0 behind the process boundary (ADR-0010). The
//! wrapper consumes a dwell and emits evidence, never a detection or an identity. SBAS/WAAS and
//! Galileo OSNMA are still not built. Acquisition alone (T-274) still cannot produce a fix.
//!
//! Every claim about real-world sensitivity here is **unverified against hardware** — it rests on
//! synthetic IQ at a stated C/N0. Settling it needs an active GNSS antenna on the bias-tee and a
//! real L1 capture, which is user-triggered.

pub mod acquire;
pub mod integrity;
pub mod observable;
pub mod prn;
pub mod receiver;

pub use acquire::{
    AcquireError, AcquisitionConfig, AcquisitionEvidence, AcquisitionResult, AcquisitionThreshold,
    DEFAULT_FALSE_ALARM, KnownCodeLed, SvAcquisition, acquire, acquisition_threshold,
};
pub use integrity::{
    IntegrityConfig, JammingAssessment, JammingVerdict, LockEvidence, PowerEvidence, SpoofTell,
    SpoofingAssessment, assess_jamming, assess_spoofing,
};
pub use observable::{Ecef, GnssObservableEpoch, SvObservable, s4_index};
pub use prn::{
    CHIP_RATE_HZ, CODE_LENGTH, CODE_PERIOD_S, CaCode, L1_HZ, L5_HZ, MAX_PRN, PrnCodebook, PrnError,
};
pub use receiver::{
    DwellSummary, GNSS_SDR_SCHEMA, GnssSdrRun, ReceiverEpochEvidence, RunOutcome,
    epochs_from_evidence, lock_evidence, s4_by_prn,
};
