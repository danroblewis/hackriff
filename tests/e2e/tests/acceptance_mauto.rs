//! MAUTO acceptance suite — `SIGNAL-087`, the blind auto-decode target (T-545 phase 2, T-546
//! phase 3).
//!
//! ```text
//! cargo nextest run -p hk-e2e -E 'binary(acceptance_mauto)'
//! ```
//!
//! # What this suite is
//!
//! The user asked for a three-phase target: (1) research + capture, (2) **failing** blind
//! acceptance tests, (3) make them pass. T-545 wrote phase 2 with five red tests and two green
//! controls; **T-546 made all seven pass**, and the `#[ignore]` lines are gone, which was that
//! ticket's stated definition of done. What each test now proves, and what had to be built:
//!
//! | Test | Ticket assertion | What closing it took |
//! |---|---|---|
//! | `a_the_emission_is_detected_blind_as_a_time_frequency_region` | (1) detect centre, bandwidth, time extent | already passed — the control |
//! | `b_the_modulation_symbol_rate_and_deviation_are_estimated_from_the_signal` | (2) estimate parameters | `hk_demod::fsk::structure`: a blind clock line, a level count with an abstention, and a `mod_order` that can say 4 |
//! | `c_the_demod_and_decode_pipeline_is_auto_selected_from_the_measurements` | (3) auto-select the pipeline | `hk_pipeline::synth` + the `emitter_synthesis` row, served by `POST /api/analyze` with its ADR-0021 trace |
//! | `d_the_decode_reaches_the_emission_the_run_detected` | (4) decode to the expected output | the trunking chain files its decode against the **inventory emitter**, not only a side table |
//! | `e_a_sensible_explanation_ranks_among_the_top_suggestions` | (5) a sensible explanation ranks | already passed — the control |
//! | `e2_the_explanation_rests_on_measured_evidence_not_only_the_allocation` | (5), quality bar | a `p25-tsbk`/`dmr-csbk`/`nxdn-cac` → `public-safety` mapping, so the ranking carries a decode |
//! | `f_the_receiver_clock_error_is_measured_not_assumed_zero` | the one thing phase 1 measured off the air | `hk_detect::trunk::raster::fit_grid_offset`: the receiver's grid offset is fitted, not assumed zero |
//! | `g_a_grant_is_followed_off_grid_once_the_receiver_alias_is_resolved` | T-628: follow grants on an off-grid receiver | `hk_detect::trunk::raster::{grid_aliases, resolve_alias}`: the modulo-raster fit's alias is bounded by the crystal's ppm and chosen by which alias has energy on the granted channels; the CC demod keeps the integration with the most CRC-valid blocks |
//!
//! **The two controls are still the reason the rest means anything.** `a_…` proves the fixture,
//! the mock device and the truth plumbing are sound; `e_…` proves the explanation path runs. A
//! suite with no green control cannot tell "the capability is missing" from "the harness is
//! broken" — `docs/19 §5.4` is the same argument made about the capture.
//!
//! **Test `d_…` names its passing preconditions explicitly**, because two thirds of assertion (4)
//! already worked before T-546 (a normal run decodes TSBKs, names P25 Phase 1, reads the band plan
//! and resolves grants — `acceptance_m4::t268_tsbk`'s ground). Saying so is the only way the
//! remaining third is visible.
//!
//! # Why the fixture is synthetic, and what a real capture would change
//!
//! `SIGNAL-087`'s reference instance is BART's 800 MHz P25 trunked system. **T-544 tried to
//! capture it and could not** (`docs/19 §7`): re-measured at the corrected amp-ON gain against a
//! CFAR *local* floor, **0 of 16 BART channels exceed +4 dB** while neighbours 12.5 kHz away run
//! 43–93 % duty, and a 60 s sweep of 850.6–869.4 MHz found exactly **one** continuous channel in
//! the whole band — and it is analogue FM, not C4FM. BART is absent, not attenuated. That is a
//! physical-RF problem (antenna to a window / outdoors / a proper 850 MHz antenna) reserved to the
//! user; T-318 owns the capture session.
//!
//! So this suite runs on **synthetic IQ** through the same mock device, and says so everywhere.
//! Concretely, what is synthetic and what a real capture would change:
//!
//! | | This fixture | A real BART capture |
//! |---|---|---|
//! | Control channel | continuous C4FM, 4800 Bd, ±600/±1800 Hz, on the 12.5 kHz raster | the same, per `docs/19 §2.1` — this part is standards-quoted, not invented |
//! | Framing | **NOT standards-compliant** (see below) | compliant: NID/BCH, rate-1/2 trellis, 98-dibit interleave, status symbols, augmented CRC |
//! | Neighbours | bursty NBFM on the raster | what `docs/19 §7.4` actually measured at 800 MHz: analogue FM voice, kurtosis 2.9–3.6 |
//! | SNR | clean, 18–20 dB, no adjacent-band load | 8-bit ADC, no preselector, cellular at 869–894 MHz setting the usable gain (`marginal-8bit`) |
//! | Simulcast | none | both BART sites are simulcast; differential delay smears C4FM and looks exactly like a demodulator bug (`docs/19 §2.3`) |
//! | Receiver clock | exact, except in [`signal_087::f_the_receiver_clock_error_is_measured_not_assumed_zero`] | −9.6 ppm measured on this HackRF (`docs/19 §7.6a`) — ⅔ of a 12.5 kHz channel at 852 MHz |
//!
//! A real capture would therefore make every one of these tests *harder*, never easier, with one
//! exception: it would replace the framing caveat below with a genuine answer key.
//!
//! # The circularity caveat, stated because it cannot be avoided here (T-299, T-300)
//!
//! **This fixture's framing layer and this repo's P25 decoder were written from the same reading
//! of the same references, so a shared misreading passes both.** `py/hkpy/synth/trunking.py` says
//! so in its own header (`"coding": "none"`; *"nothing here should be read as a standards-compliant
//! P25 encoder"*), and `crates/hk-detect/src/trunk/confirm.rs:980-1035` lists the five ways the
//! decoder is non-compliant. That is T-299's open verification hole, and writing a
//! standards-compliant encoder here from memory would not close it — it would only move the
//! unverified reading from the decoder to the generator, where nobody is looking at it.
//!
//! What this suite does about it, since it cannot avoid it:
//!
//! 1. It **says so**, here and in the failing message of
//!    [`signal_087::d_the_decode_reaches_the_emission_the_run_detected`].
//! 2. Its strongest decode assertion is the one that is **not** circular: a grant names a channel
//!    number, `IDEN_UP` turns that into a frequency, and **RF energy must actually be radiating
//!    there, at that time, in the same recording** — checked against the Tier-A spectrogram truth,
//!    which rests on nothing but the FFT (`docs/19 §5.3`). Two independent chains have to agree.
//! 3. It records that **the only thing that closes T-299 is an independent oracle** (SDRTrunk /
//!    OP25 / DSD+ offline over the same IQ, behind the ADR-0010 plugin boundary, hardware-tier
//!    only) over a real capture, and neither exists yet.
//!
//! # No lookup-and-tune
//!
//! Nothing here passes a frequency, modulation or protocol into the system. The only frequency
//! anything is given is where the mock device says it is tuned, exactly as a real HackRF reports
//! it; the built-in chain registry's `[851, 869] MHz` trigger band is a *band gate*, not a
//! frequency lookup, and [`signal_087::NO_LOOKUP`] records that reading and its limit. Truth is
//! sealed by `TruthVault` and opened only after the run, only to check the answer.

// The shared harness modules carry helpers only the other suites use.
#![allow(dead_code)]

#[path = "acceptance/common.rs"]
mod common;

#[path = "acceptance/blind.rs"]
mod blind;

#[path = "acceptance/signal_087.rs"]
mod signal_087;
