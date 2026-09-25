//! C23 control-channel hunt, as a chain on a running pipeline (T-287).
//!
//! T-267 built the capability and proved it blind through the device — 33 raster channels swept,
//! 6 candidates at FCO 1.0, exactly one confirmed, the continuous decoy rejected — but it stopped
//! short of a caller: everything it needed sat behind `pub(crate) Shared`, and a hook meant new
//! chain-contract variants. Nothing in a normal run called it, so a run wrote no control-channel
//! row and the capability's tests passed whether or not the pipeline could ever reach it. This
//! module is that caller.
//!
//! # The two stages, kept apart
//!
//! The split `hk_detect::trunk` enforces by type is the shape of this chain too, and this module
//! adds no way round it:
//!
//! 1. **Candidacy** is measured here: per-raster-channel frequency-channel occupancy over the
//!    chain's own dwell, against the band's own measured noise floor. Cheap, spectral, and *not*
//!    evidence of a control channel — C23's named pitfall is that continuous data emitters pass
//!    the FCO test.
//! 2. **Confirmation** is not measured here at all. The only value of type `ConfirmedCc` in the
//!    workspace still comes out of [`CcConfirmer::confirm`], which needs demodulated symbols and
//!    requires frame sync **and** CRC-valid blocks. This module calls it and writes down what it
//!    returns; it cannot manufacture one.
//!
//! Every threshold that decides anything (`MIN_CC_FCO`, the sync tolerance, the sync and CRC
//! counts) lives in `hk_detect::trunk` with its arithmetic, where a chain spec cannot reach it.
//! What a spec sets ([`super::spec::NodeSpec::TrunkCc`]) is only how much the hunt may *spend*.
//!
//! # Cost, on the capture and audio threads
//!
//! The hunt runs on its **own chain thread**, spawned by [`ChainManager`](super::ChainManager) on
//! the control thread, behind its own [`ChainReader`] and its own gate cursor — off the ring's
//! always-on readers, off the DSP readers' buffers, and off every audio chain. Attaching and
//! detaching never touch the capture thread (ADR-0001 S1); in lossless replay its cursor joins the
//! flow gate like any other chain's.
//!
//! Per pass the cost is bounded a priori: **at most one pass per `period_s` of stream time**, one
//! window of `window_s × fs` samples held at a time (nothing rolls over, so memory is one window,
//! not a stream), `floor(window_s × fs / 1024)` 1024-point power rows, `≤ max_channels` band-power
//! integrations of ~`raster/bin` bins per row, and **`≤ max_demods` down-conversions plus C4FM
//! demodulations** of that one window — the expensive half, and the one the admission cap exists
//! for. Then one `CcConfirmer` scan per demodulated channel, and at most one `TrunkSystem` upsert
//! per newly confirmed channel per run. No audio anywhere, no ring access outside the chain's own
//! reader, and the repository lock is taken only for the upsert.
//!
//! The one deliberate exception to the period bound: if the stream ends before the first full
//! window, one shorter pass is allowed, so a short recording still gets exactly one hunt. It can
//! happen at most once per chain.
//!
//! # Metadata only (M4)
//!
//! The chain writes a [`TrunkSystem`] row — a protocol, the measured control-channel frequency and
//! times — plus, since T-268, the band plan its identifier updates announced and the grants it
//! issued, plus, since T-269, the calls it followed, plus, since T-849, what each followed call's
//! own voice-frame *headers* said (LDU1 link control, LDU2 encryption sync). All of it metadata: no
//! demodulated audio — the IMBE voice codewords are skipped by position — no message payload, no
//! recording, no stream, and nothing decrypted. There is no
//! `CallAudio` in the workspace and no column that could hold one (docs/07 §2.29), so this is a
//! property of the data model rather than a habit of this module. That is what lets it run under
//! the fail-closed `metadata-only` class a 12.5 kHz LMR band derives, and the validator in
//! [`super::spec`] refuses any `trunk-cc` spec that sets `requires_content` or carries a record
//! node, so it stays true.
//!
//! # Following a grant (T-269)
//!
//! T-268 turned a grant's 16-bit channel number into a frequency. What happens next splits in two,
//! and the split is the C23 **span limit**: a trunked system's voice channels routinely fall
//! outside the ≤20 MHz the radio can hold at once ([`C23_SPAN_LIMIT_HZ`]).
//!
//! - **Inside the window:** [`follow_grants`] allocates a channelizer output on the granted
//!   channel — a real [`Ddc`] at the LMR channel bandwidth, the stream a voice demodulator will
//!   consume — and measures where energy on it starts and stops. Each transmission becomes a
//!   [`CallRecord`] with boundaries that were *measured*, ended by an observed
//!   [`SILENCE_TIMEOUT_S`], plus `call-start` / `call-end` events linking it to the grant stream.
//!
//! # Where the WINDOW ends, not the call (T-308)
//!
//! A transmission still keyed when the buffered window runs out has no observed end, and the one
//! thing this module must not do is write the window's edge into `t_end`: that manufactures a
//! boundary the radio never produced, and does it systematically — every call longer than the
//! dwell would read as ending exactly when the receiver stopped looking.
//!
//! So such a call is written **open and explicitly truncated**: `t_end` stays NULL, and
//! `observed_until` carries the last instant this channel was actually watched. Those two columns
//! are what separate "still running" from "we stopped looking" (docs/07 §2.29): the row's
//! [`hk_model::CallEnding`] is `Truncated`, its duration announces itself as a **lower bound**,
//! and `cc_calls_truncated` counts how often the schedule — not the radio — decided where
//! measurement stopped.
//!
//! A later pass may **continue** such a call rather than starting a second row, but only across a
//! gap no end could have hidden in: [`CONTINUATION_GAP_S`], which *is* [`SILENCE_TIMEOUT_S`]
//! rather than a second number, so one rule applies whether or not the receiver was looking. On
//! the built-in duty cycle (0.5 s of every 10 s) the gap is 9.5 s — 105× the bound — so
//! continuation does not fire and the truncated row stands, which is the honest answer: 9.5 s
//! unwatched can hold a complete end, a new grant and a new call. [`continues_truncated`] is that
//! decision, pure and stated clause by clause.
//! - **Outside it:** the grant becomes an [`GrantKind::OutsideWindow`] row carrying the frequency
//!   it resolved to and how far beyond the window that is, and a `CallRecord` whose `t_end` is
//!   NULL and whose reason says why. **It is never dropped.** A dropped grant is indistinguishable
//!   from a system with no traffic, and that silence is what teaches a person to trust a picture
//!   that is wrong. The refusal shows up in the run summary (`cc_grants_outside_window`), in the
//!   grant stream, and in the call list.
//!
//! Boundaries are a *comparison*, so the follower measures an empty raster channel through the
//! **same** DDC spec and compares against that: both sit on one scale by construction, and the
//! margin then means what it says whether the granted channel is busy for a tenth of the window or
//! all of it. With no quiet channel to measure against, it claims nothing
//! (`cc_follow_no_reference`).
//!
//! # The encryption check (T-270)
//!
//! A grant's service-options octet is read first: with the verified encryption bit set it produces
//! [`hk_model::Encryption::Encrypted`], and every other grant path is `Unknown` — a grant update
//! carries no such octet, a clear bit is a grant-time announcement rather than the call's own
//! statement, and a call joined in progress never saw either. [`CallRecord::from_grant`] carries
//! that verbatim.
//!
//! The authoritative statement is the call's own ALGID, in the LDU2s on the *granted* channel
//! (docs/04 §8.3). Those are demodulated per followed FDMA channel (T-849, [`voice_frames`]), and
//! the ALGIDs of the LDU2s attributed to a call are folded into its state by
//! [`CallHeader::fold`] (T-330): the header replaces an `Unknown` grant, and replaces a grant-time
//! `Encrypted` with its own `Encrypted` naming the algorithm and key — but a clear header never
//! walks back an encrypted grant; that contradiction stays encrypted and is recorded as
//! `algid-contradicts-grant`. So the only thing here that can make a call `clear` is its own ALGID
//! `0x80`, which is also the only thing that should — a late entry becomes known as soon as an
//! LDU2 of the call is heard.
//!
//! Before each followed call, [`VoicePermit::open`] is consulted at the point a voice path would be
//! opened, and its refusal is recorded on the call. The ALGID fold produces an `Encryption`, never
//! a permit, so a call that is clear by its own ALGID earns one the same way as any other — by
//! asking. M4 opens no voice path at all — there is no vocoder and no `CallAudio` — so even a
//! permitted call consumes no sample for voice. That is the point: the check is in place *before*
//! the thing it checks, so the thing cannot arrive without it.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_demod::fsk::{C4fmConfig, C4fmDemod, C4fmSymbols, FmStructure, measure_fm_structure};
use hk_detect::trunk::{
    AliasResolution, AliasScore, AliasUnresolved, CC_FRAMINGS, CSBK_BYTES, CallHeader, CcCandidate,
    CcConfirmer, CcFraming, ChannelMap, DmrGrant, EncryptionSync, Grant, GridFit, LDU_DIBITS,
    LduPayload, LduScan, MIN_CC_FCO, MIN_CRC_VALID, MIN_SYNC_HITS, NXDN_L3_BYTES, NxdnAssignment,
    RASTER_TOLERANCE_HZ, RECEIVER_CLOCK_BOUND_PPM, Resolved, VoicePermit, VoiceRefused, algid_name,
    best_lmr_raster, clock_offset_mod_grid, dmr_protocol_of, fit_grid_offset, grid_aliases,
    nxdn_protocol_of, protocol_of, resolve_alias, scan_blocks, scan_cacs, scan_csbks, scan_ldus,
};
use hk_dsp::{Ddc, DdcSpec, InputInfo, SegmentEngine, WelchConfig, WindowKind};
use hk_model::repo::synthesis::{AliasEvidence, AliasState, ReceiverAlias, ReceiverFit};
use hk_model::{
    CalibrationMethod, CalibrationState, CalibrationStateId, CallRecord, GrantEvent, GrantKind,
    SampleTime, Timestamp, TrunkProtocol, TrunkSystem, TrunkSystemId,
};
use num_complex::{Complex, Complex32};
use serde_json::json;

use super::{ChainMsg, ChainReader, Next};
use crate::ccverdict::{CcCandidacy, CcChannelVerdict, CcFramingVerdict, CcOutcome, CcPass};
use crate::events::Candidate;
use crate::gate::GateCursor;
use crate::run::Shared;
use crate::stats::{add, inc};
use crate::synth::CcObservation;

/// How far above the band's own **measured** noise floor a raster channel counts as occupied, dB.
///
/// A priori, and deliberately not a confirmation threshold: it decides only which channels are
/// worth a demodulation. Moving it can add or remove candidates; it can never confirm one, because
/// confirmation is frame sync plus CRC. 6 dB is the conventional "clearly above the floor", and it
/// is the value T-267's blind acceptance scene used.
const OCCUPIED_MARGIN_DB: f64 = 6.0;

/// FFT length of the occupancy sweep's power rows.
///
/// At 500 kHz this is a 488 Hz bin and a 2.05 ms frame: ~25 bins across a 12.5 kHz channel, and
/// frames short enough that a bursty neighbour reads as bursty rather than continuous.
const SWEEP_FFT_LEN: usize = 1024;

/// Samples per symbol the candidate down-conversion aims for (the plan floors to an integer
/// decimation, so the realised value is at least this).
const DEMOD_SPS: f64 = 10.0;

/// Fraction of the sample rate the sweep treats as usable (the window's flat middle).
///
/// It is also what "inside the dwell window" means to the grant follower, deliberately: one notion
/// of the window for both halves, and a grant landing in the front end's roll-off is one this
/// receiver cannot follow honestly, so it is refused and said so rather than followed badly.
const USABLE_FRACTION: f64 = 0.8;

/// How long a granted channel must be **silent** before the call it carried is declared over, s.
///
/// A priori, bounded from the protocol above and from propagation below:
///
/// - **Upper bound, 180 ms.** A keyed P25 Phase 1 voice channel emits a Logical Link Data Unit
///   every 180 ms (1728 bits at 9600 bit/s), back to back, and C4FM is constant-envelope — a
///   transmitter that is still keyed is never silent at all. Half a voice frame of silence is
///   already half a frame more than a keyed transmitter can produce.
/// - **Lower bound, ~35 ms.** The longest silence a *keyed* transmitter can appear to have is a
///   Rayleigh fade null. At 851 MHz (λ = 0.352 m) a null is crossed in about λ/2 ÷ v, which at a
///   slow 5 m/s is 35 ms.
///
/// 90 ms — half a voice frame — sits between them with 2.6× margin below and 2× above.
///
/// **Too short** and a deep fade or a momentary gap splits one transmission into two calls:
/// over-counts calls, under-states durations. **Too long** and two distinct keyings merge into one
/// call: under-counts, over-states, and a call that really ended near the end of a buffered window
/// is left open instead of closed.
///
/// **Scope, stated rather than implied:** this is the end of a *transmission*, not of a
/// conversation. Trunk Recorder's multi-second hang-time merge — several keyings under one grant
/// read as one call — needs a channel-hold policy this does not have, and is not attempted.
const SILENCE_TIMEOUT_S: f64 = 0.090;

/// The envelope frame a granted channel is measured in, s.
///
/// ~9.6 C4FM symbols at 4800 Bd: long enough that a frame's power is the channel's average rather
/// than one symbol's, short enough to place a boundary to 2 ms. The DDC's default output rate is
/// about 2× the channel bandwidth whatever the input rate, so a frame holds ~50 complex samples at
/// any front-end rate, and a power estimate from 50 samples spreads by 1/√50 ≈ 0.6 dB — ten times
/// under the [`OCCUPIED_MARGIN_DB`] it is then tested against, so the threshold is not marginal.
const FOLLOW_FRAME_S: f64 = 0.002;

/// Most occupancy a raster channel may show and still serve as the follower's noise reference.
///
/// The reference has to be a channel the call itself cannot move. 5 % of frames cannot shift a
/// median, and the sweep's own margin already says those frames sat [`OCCUPIED_MARGIN_DB`] above
/// the band floor.
const FOLLOW_REF_MAX_FCO: f64 = 0.05;

/// Integrate-and-dump fractions the control-channel demodulation tries, the demodulator's default
/// first (T-628).
///
/// **Chosen by the decode, not by a run**: each fraction is a demodulation of the same baseband,
/// and the one yielding the most CRC-valid blocks across every framing is kept — the product
/// vision's "tune from the processed output", with the CRC as the output-quality measure. Ties
/// keep the earlier entry, so where the default already does as well nothing changes. Why these
/// two: averaging a discriminator over more of a symbol averages away more noise until the
/// transitions start to bleed in, and a raised-cosine-shaped C4FM symbol is flat over most of its
/// period — half a symbol gives up ~3 dB of that averaging, four-fifths keeps it while staying off
/// the transitions. On the T-545 scene at 20 dB the default recovers 12–13 of 32 blocks and 0.8
/// recovers 29–30: the difference between a band plan being read and not.
///
/// A second fraction cannot make a false confirmation likely: each demodulation is one more
/// trial against a ~1.4e-16-per-frame CRC chance rate (`hk_detect::trunk::confirm`).
const DEMOD_INTEGRATE_LADDER: [f64; 2] = [0.5, 0.8];

/// The instantaneous window a HackRF-class front end can hold, Hz (C23's span limit, docs/01 §7.3
/// and `docs/capabilities/C23-trunking-follow.md` §Platform constraints). Recorded on the refusal
/// so the row says *which* limit it hit, not merely that it hit one.
const C23_SPAN_LIMIT_HZ: f64 = 20e6;

/// Machine reason: the grant resolved to a frequency outside the dwell window (C23's span limit).
const OUTSIDE_WINDOW: &str = "grant-outside-window";
/// Machine reason: a DMR Tier III grant names a logical channel, and no channel-parameter
/// announcement was decoded, so **no frequency is produced** (T-271).
///
/// Taken **from the decoder** rather than written out again, so the string in the row and the
/// string the refusal is defined by cannot drift apart — the other reasons in this module are
/// this chain's own, but this one is `hk_detect`'s statement and belongs to it.
const NO_CHANNEL_PARAMETERS: &str = hk_detect::trunk::DmrResolved::NoChannelParameters.reason();
/// Machine reason: an NXDN Type-C assignment names a channel number, and the air interface carries
/// no map and no base/step to resolve it through, so **no frequency is produced** (T-345).
///
/// Taken **from the decoder**, like the DMR reason above, so the string in the row and the string
/// the refusal is defined by cannot drift apart.
const NO_CHANNEL_MAP: &str = hk_detect::trunk::NxdnResolved::NoChannelMap.reason();
/// Machine reason: the call's end was **observed**, as silence on the granted channel.
const SILENCE_TIMEOUT: &str = "silence-timeout";
/// Machine reason: the buffered window ran out while the channel was still active, so no end was
/// observed and `t_end` stays NULL rather than borrowing the window's edge.
const WINDOW_ENDED: &str = "window-ended";
/// Machine reason: the channel was already active in the window's first frame, so the call was
/// joined in progress — and its encryption state is therefore `unknown`, never `clear`.
const LATE_ENTRY: &str = "late-entry";
/// Machine reason: the call is one slot of a TDMA carrier, so its boundaries were measured on the
/// **shared** carrier envelope rather than on that slot's own bursts (T-272).
///
/// Both slots of a P25 Phase 2 channel key one carrier, and this build demodulates no TDMA burst
/// timing, so the envelope says when the *channel* was up and the control channel says whose call
/// it was. Recording the reason is what keeps a two-slot call list from reading as two
/// independently timed measurements.
const TDMA_SHARED_ENVELOPE: &str = "tdma-shared-envelope";
/// Machine reason: this call is the continuation of a truncated one from an earlier pass, joined
/// across an unobserved gap short enough that no end could have happened in it
/// ([`CONTINUATION_GAP_S`], T-308).
const CONTINUED: &str = "continued-across-passes";
/// T-330: the grant announced encrypted and the call's own ALGID said clear. The call stays
/// encrypted (the safer answer); this reason is how the disagreement stays visible.
const ALGID_CONTRADICTS_GRANT: &str = "algid-contradicts-grant";
/// Every reason [`VoicePermit::open`] can refuse with, so a call re-asked on a later pass carries
/// only the current answer.
const VOICE_REFUSALS: [&str; 3] = [
    VoiceRefused::Encrypted { algid: None }.reason(),
    VoiceRefused::Unknown.reason(),
    VoiceRefused::UnauthoritativeClear.reason(),
];

/// How long an **unobserved** gap may be and still be crossed by one call, s (T-308).
///
/// It is [`SILENCE_TIMEOUT_S`] itself, and deliberately not a second number: inside a pass, a call
/// ends when the channel is silent for 90 ms, so a gap *shorter* than that could not have
/// contained an end even if it had been watched. Crossing it merges exactly what the in-pass
/// splitter already merges — one rule, applied whether or not the receiver happened to be looking.
///
/// **Arithmetic, and the error it leaves.** The hunt observes `window_s` out of every `period_s`
/// of stream time (the built-in chain uses 0.5 s in 10 s), so the ordinary gap between passes is
/// `period_s − window_s ≈ 9.5 s` — **105×** this bound. Continuation therefore does **not** fire
/// on a duty-cycled hunt, and must not: a P25 transmission is seconds long and its LLDU cadence is
/// 180 ms, so 9.5 s unwatched can hold a complete end, a new grant and a new call, and joining
/// across it would fuse distinct calls into one. It fires only where passes nearly abut (a
/// continuously-tuned dwell, `period_s ≤ window_s + 0.09`), which is the one case where continuity
/// is a measurement rather than a guess.
///
/// **Error direction, both ways.** Continuing over a gap ≤ 90 ms can over-state a call by at most
/// that gap, and only by merging two keyings that the same 90 ms rule would have merged anyway.
/// Refusing to continue never over-states: the earlier call stays **truncated** (its duration a
/// lower bound, its `t_end` still NULL) and the later one starts as late entry. What is never done
/// is the third option — closing the call at the window's edge — because that manufactures a
/// boundary the radio never produced and would systematically under-state every call longer than
/// the dwell.
const CONTINUATION_GAP_S: f64 = SILENCE_TIMEOUT_S;

/// A control channel this chain has already written, and the band plan decoded off it.
///
/// The map lives here, across passes, because that is what gives a channel-table entry an **age**:
/// an identifier decoded in one pass is still the thing a grant three passes later is resolved
/// through, and [`ChannelMap::resolve`] refuses when that gap grows past
/// [`hk_detect::trunk::IDEN_MAX_AGE_S`] rather than mapping to what the identifier used to mean.
struct KnownCc {
    system: TrunkSystem,
    map: ChannelMap,
    /// Calls left **truncated** by the previous pass, by rounded voice frequency and TDMA slot
    /// (T-308; the slot since T-272, so one slot's call is never continued as another's): the
    /// channel was still keyed when the buffered window ran out, so no end was observed. A later
    /// pass may continue one — but only across a gap no longer than [`CONTINUATION_GAP_S`], and
    /// the ordinary duty cycle's gap is 105× that, so ordinarily these simply expire unclaimed and
    /// their rows stay truncated. One entry per followed channel slot, bounded by `max_follows`
    /// channels times the plan's slot count.
    tails: HashMap<(i64, Option<u8>), CallRecord>,
}

/// The `grant_event` a decoded grant produces, resolved through `map` as of `t`.
///
/// Split out and pure so the refusal paths can be tested without a pipeline. The rule it encodes
/// is C23's stale-IDEN pitfall: a channel number the band plan cannot account for becomes an
/// [`GrantKind::UnmappedChannel`] event carrying **no frequency** and the reason it has none —
/// never a frequency borrowed from some other identifier.
///
/// Encryption is whatever **this message** was entitled to say ([`Grant::encryption`]): `Encrypted`
/// when the verified service-options bit is set, and `Unknown` otherwise — including for a grant
/// update, which carries no such octet. Never `clear`, which would need an ALGID this message does
/// not carry (T-270).
fn grant_event(system: TrunkSystemId, g: &Grant, map: &ChannelMap, t: Timestamp) -> GrantEvent {
    let opcode = if g.update {
        "grp-vch-grant-update"
    } else {
        "grp-vch-grant"
    };
    let mut ev = GrantEvent::new(
        system,
        if g.update {
            GrantKind::GrantUpdate
        } else {
            GrantKind::Grant
        },
        t,
    );
    ev.talkgroup = Some(g.talkgroup.to_string());
    ev.unit_id = (g.source != 0).then(|| g.source.to_string());
    ev.channel = Some(g.channel.to_string());
    // What this message said about encryption, and nothing more.
    ev.encryption = g.encryption();
    match map.resolve(g.channel, t) {
        Resolved::Mapped {
            f_hz,
            iden,
            channel_number,
            slots,
            slot,
            decoded_at,
        } => {
            ev.f_hz = Some(f_hz);
            // The TDMA slot, when the band plan is a TDMA one. `None` on an FDMA plan: there is no
            // slot, and writing 0 would claim a measurement nobody made (T-272).
            ev.slot = slot;
            ev.detail = json!({
                "opcode": opcode,
                "iden": iden,
                "channel_number": channel_number,
                "slots": slots,
                "slot": slot,
                "mapping": if slots > 1 {
                    "base + spacing * (channel / slots); slot = channel % slots"
                } else {
                    "base + spacing * channel"
                },
                "iden_decoded_at_ns": decoded_at.as_unix_nanos(),
            });
        }
        Resolved::Unmapped(why) => {
            ev.kind = GrantKind::UnmappedChannel;
            ev.f_hz = None;
            let mut detail = json!({
                "opcode": opcode,
                "iden": g.iden(),
                "channel_number": g.channel_number(),
                "reason": why.reason(),
            });
            if let hk_detect::trunk::Unmapped::Stale { age_s, max_age_s } = why {
                detail["iden_age_s"] = json!(age_s);
                detail["iden_max_age_s"] = json!(max_age_s);
            }
            ev.detail = detail;
        }
    }
    // The octet verbatim, including the bits this decoder deliberately does not interpret, so a
    // later task can revisit them without needing the capture again.
    if let Some(so) = g.service_options {
        ev.detail["service_options"] = json!(so.raw());
        ev.detail["service_options_encrypted"] = json!(so.is_encrypted());
    }
    ev
}

/// The `grant_event` a decoded **DMR Tier III** grant produces (T-271).
///
/// Split out and pure, like [`grant_event`] and [`outside_window_event`], so its refusal can be
/// tested without a pipeline. The refusal is the third shape in this chain of tasks, and it is worth
/// saying how it differs from the other two:
///
/// - **T-268** (`unmapped-channel` / `no-iden`): the band plan *could* have resolved the channel,
///   and the particular identifier it named was never announced. No frequency, because the table
///   cannot account for this one.
/// - **T-269** (`grant-outside-window`): the band plan *did* resolve it, correctly, and the radio
///   cannot reach it. The frequency **is** reported, with how far out of reach it fell.
/// - **Here**: there is no band plan at all, and there was never going to be one, because DMR
///   Tier III announces no channel parameters this build could corroborate. No frequency —
///   permanently, not this-time — and the reason says so.
///
/// Everything the message *did* say is recorded: the logical channel number, the timeslot (DMR is
/// two-slot TDMA and a call without its slot is under-attributed — C23's slot mix-up pitfall), the
/// target and source addresses, and the three payload bits whose meaning could not be corroborated,
/// verbatim and interpreted by nothing.
///
/// Encryption is [`hk_model::Encryption::Unknown`], from [`DmrGrant::encryption`]: a DMR privacy
/// indication lives in a PI header on the traffic channel, which nothing here demodulates. The call
/// this event opens therefore reaches T-270's [`VoicePermit`] as `Unknown` and is refused, through
/// the **same** gate a P25 call goes through.
fn dmr_grant_event(system: TrunkSystemId, g: &DmrGrant, t: Timestamp) -> GrantEvent {
    // `UnmappedChannel`, not a new kind: the statement is the same one the model already has a
    // variant for — a channel number that could not be turned into a frequency — and the `reason`
    // in the detail is what distinguishes "no identifier" from "no band plan exists".
    let mut ev = GrantEvent::new(system, GrantKind::UnmappedChannel, t);
    // A broadcast grant's target is a talkgroup; a private grant's is a radio. Recording a private
    // grant's target as a talkgroup would be a small lie that a call list would repeat forever.
    if g.is_voice() && g.csbko == hk_detect::trunk::CSBKO_BTV_GRANT {
        ev.talkgroup = Some(g.target.to_string());
    }
    ev.unit_id = (g.source != 0).then(|| g.source.to_string());
    ev.channel = Some(g.lpcn.to_string());
    ev.slot = Some(g.timeslot);
    ev.f_hz = None;
    ev.encryption = g.encryption();
    ev.detail = json!({
        "protocol": "dmr-tier3",
        "opcode": g.opcode_name().unwrap_or("unnamed"),
        "csbko": g.csbko,
        "lpcn": g.lpcn,
        "timeslot": g.timeslot,
        "target": g.target,
        "source": g.source,
        "voice": g.is_voice(),
        "reason": NO_CHANNEL_PARAMETERS,
        // Why this refusal is permanent rather than a gap that a longer dwell would close.
        "unresolvable": "DMR Tier III announces no channel-parameter message this build could \
                         corroborate, so a logical channel number has no on-air base or step to \
                         resolve through. Assuming one would produce a plausible wrong frequency, \
                         which is C23's stale-band-plan pitfall.",
        // UNVERIFIED, recorded verbatim, interpreted by nothing (T-268's discipline).
        "unverified_flag_bits": g.flags,
    });
    ev
}

/// The `grant_event` a decoded **NXDN Type-C** channel assignment produces (T-345).
///
/// Split out and pure, like its P25 and DMR siblings, so its refusal can be tested without a
/// pipeline. This is the **fourth** refusal shape in this chain of tasks, and it differs from
/// T-271's in the one way that matters:
///
/// - **T-271** (`no-channel-parameters`): no DMR Tier III channel-parameter announcement could be
///   *corroborated*, so nothing in this build knows how to resolve a logical channel number. The
///   refusal rests on what could not be verified.
/// - **Here** (`no-channel-map`): the air interface is fully corroborated, and what it says is that
///   it carries a channel **number** — §6.5.31, ten bits, 1..1023 — and defines no mapping from one
///   to hertz. Not one of the specification's information elements is a frequency; even `CCH_INFO`,
///   which tells a radio about its site's control channels, gives their channel numbers. The map
///   lives in the radio's configuration, so the refusal rests on what the standard *states*, and it
///   would be lifted by a channel map a person supplies — never by one this build invents.
///
/// Everything the message *did* say is recorded: the channel number, the call type, the source unit
/// and the destination group or unit, the call timer, the site's RAN, and the spare flag and CC
/// Option octets whose bits this decoder does not interpret.
///
/// Encryption is [`hk_model::Encryption::Unknown`], from [`NxdnAssignment::encryption`]: NXDN's
/// Cipher Type element rides in traffic-channel messages, and an assignment has no room for one.
/// The call this event opens therefore reaches T-270's [`VoicePermit`] as `Unknown` and is refused,
/// through the **same** gate a P25 or DMR call goes through.
fn nxdn_grant_event(system: TrunkSystemId, a: &NxdnAssignment, t: Timestamp) -> GrantEvent {
    // `UnmappedChannel`, not a new kind: the statement is the one the model already has a variant
    // for — a channel number that could not be turned into a frequency — and the `reason` in the
    // detail is what distinguishes "no identifier" from "no band plan exists" from "the air carries
    // no map at all".
    let mut ev = GrantEvent::new(system, GrantKind::UnmappedChannel, t);
    // A group call's destination is a talkgroup; an individual call's is a radio. Recording an
    // individual call's destination as a talkgroup would be a small lie a call list repeats forever.
    if a.is_group_call() {
        ev.talkgroup = Some(a.destination.to_string());
    }
    // 0x0000 is the specification's Null Unit ID — a filler, not a radio.
    ev.unit_id = (a.source != 0).then(|| a.source.to_string());
    ev.channel = Some(a.channel.to_string());
    ev.f_hz = None;
    ev.encryption = a.encryption();
    ev.detail = json!({
        "protocol": "nxdn-type-c",
        "message": a.message_name().unwrap_or("unnamed"),
        "message_type": a.message_type,
        "channel": a.channel,
        "call_type": a.call_type,
        "source": a.source,
        "destination": a.destination,
        "destination_is_group": a.is_group_call(),
        "call_timer": a.call_timer,
        "ran": a.ran,
        "voice": a.is_voice(),
        "late_entry": a.is_late_entry(),
        "reason": NO_CHANNEL_MAP,
        // Why this refusal is about configuration rather than about a gap a longer dwell closes.
        "unresolvable": "an NXDN Type-C assignment names a 10-bit channel NUMBER (1..1023) and the \
                         air interface defines no mapping from one to hertz - none of its \
                         information elements is a frequency, and even CCH_INFO names control \
                         channels by number. The map is configured in the radio, so no frequency \
                         is produced until one is supplied; assuming a base and step would give a \
                         plausible wrong frequency, which is C23's stale-band-plan pitfall.",
        // UNVERIFIED, recorded verbatim, interpreted by nothing (T-268's discipline).
        "unverified_spare_flags": a.flags,
        "unverified_cc_option": a.cc_option,
    });
    ev
}

/// What one hunt may spend. Built from [`super::spec::ChainShape::TrunkCc`] plus the spec's
/// raster; see the module docs for the bound each field carries.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TrunkCcNode {
    /// Samples collected per pass, s.
    pub window_s: f64,
    /// Most raster channels measured for occupancy per pass.
    pub max_channels: usize,
    /// Most candidates demodulated per pass.
    pub max_demods: usize,
    /// Least stream time between passes, s.
    pub period_s: f64,
    /// Most granted channels followed per pass (T-269).
    pub max_follows: usize,
    /// Channel raster, Hz (the a-priori LMR grid the spec names).
    pub raster_hz: f64,
}

/// Runs the hunt until the stream ends or the chain is detached.
pub(crate) fn run(
    shared: Arc<Shared>,
    rx: Receiver<ChainMsg>,
    cand: Candidate,
    node: TrunkCcNode,
    cursor: GateCursor,
) {
    let fs = shared.fs;
    let c = &shared.counters.chains;
    let window = ((node.window_s * fs) as usize).max(SWEEP_FFT_LEN);
    let min_window = (window / 2).max(SWEEP_FFT_LEN);
    let period = (node.period_s * fs).max(0.0) as u64;
    let mut cr = ChainReader::new(Arc::clone(&shared), cand.first_sample, cursor);
    let mut buf: Vec<Complex<i8>> = Vec::with_capacity(window);
    let mut prov: Option<ProvenanceHandle> = None;
    let mut base = 0u64;
    let mut base_time = Timestamp::UNIX_EPOCH;
    let mut next_pass = 0u64;
    let mut passes = 0u64;
    // Control channels this chain has already written, by rounded frequency: a repeat sighting
    // updates `last_seen` rather than minting a second system for the same channel.
    let mut known: HashMap<i64, KnownCc> = HashMap::new();
    // T-977: the verdict already filed onto an emitter for each channel, so an unchanged answer is
    // not re-written every pass. A hunt runs every `period_s` for the life of the run; a synthesis
    // row per rejected channel per pass would grow the database without bound to say the same
    // thing over and over. A CHANGED verdict is written, which is the part worth keeping.
    let mut filed: HashMap<i64, CcOutcome> = HashMap::new();
    let (mut detach, mut closed) = (false, false);
    loop {
        match rx.try_recv() {
            // The hunt is not track-driven: its trigger is the band prior plus measured
            // occupancy, so a member box tells it nothing it does not measure itself.
            Ok(ChainMsg::Member(_)) => {}
            Ok(ChainMsg::Detach) => detach = true,
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => detach = true,
        }
        if !closed {
            match cr.next() {
                Next::Data(ch) => {
                    if ch.end_sample() < next_pass {
                        // Between passes the chain reads and discards. The period bound therefore
                        // costs no memory and no DSP, only the ring read every chain does.
                        buf.clear();
                    } else {
                        let contiguous = !buf.is_empty()
                            && ch.first_sample() == base + buf.len() as u64
                            && prov.as_ref().is_some_and(|p| p.id() == ch.provenance.id());
                        if !contiguous {
                            buf.clear();
                            base = ch.first_sample();
                            base_time = ch.time.host_time;
                            prov = Some(ch.provenance.clone());
                        }
                        if buf.len() < window {
                            buf.extend_from_slice(&cr.buf[..ch.len]);
                        }
                    }
                    cr.release_to(ch.end_sample());
                }
                Next::Lost => buf.clear(),
                Next::Idle => {}
                Next::Closed => closed = true,
            }
        }
        // One shorter pass is allowed when the stream ends before the first full window, so a
        // short recording still gets exactly one hunt. At most once per chain.
        let last_chance = (closed || detach) && passes == 0 && buf.len() >= min_window;
        if buf.len() >= window || last_chance {
            if let Some(p) = prov.clone() {
                hunt(
                    &shared,
                    &node,
                    &Pass {
                        buf: &buf,
                        base,
                        t_start: base_time,
                        prov: &p,
                        track: cand.track,
                        index: passes + 1,
                    },
                    &mut known,
                    &mut filed,
                );
                passes += 1;
                inc(&c.cc_passes);
            }
            next_pass = base + buf.len() as u64 + period;
            buf.clear();
        }
        if detach || closed {
            break;
        }
    }
}

/// One pass: sweep the raster for occupancy, demodulate the candidates admission allows, and
/// confirm what framing confirms.
struct Pass<'a> {
    buf: &'a [Complex<i8>],
    base: u64,
    t_start: Timestamp,
    prov: &'a ProvenanceHandle,
    /// The detection track the chain was attached for, carried so a decode can be bound to it.
    track: Option<hk_model::TrackId>,
    /// 1-based pass number within this chain's life, carried onto the pass report (T-977).
    index: u64,
}

fn hunt(
    shared: &Shared,
    node: &TrunkCcNode,
    pass: &Pass<'_>,
    known: &mut HashMap<i64, KnownCc>,
    filed: &mut HashMap<i64, CcOutcome>,
) {
    let Pass {
        buf,
        base,
        t_start,
        prov,
        track,
        index: _,
    } = *pass;
    let c = &shared.counters.chains;
    let fs = prov.tune.sample_rate_hz;
    let raster = node.raster_hz;
    if !(fs.is_finite() && fs > 0.0 && raster > 0.0) {
        return;
    }
    // The ONLY frequency the hunt is given is where the device says it is tuned. The raster is an
    // a-priori standard and its origin is that centre — no band-plan lookup, no truth.
    let tune_center = prov.tune.center_hz;
    // ---- The receiver's own clock error, FITTED rather than assumed zero (docs/19 §7.6a,
    // §4.4 step 1). A HackRF One has a plain crystal and no TCXO; this project's own unit is
    // −9.6 ppm, which at 852 MHz is −8.2 kHz — ⅔ of a 12.5 kHz channel and 5.5× the raster
    // tolerance. Every emission moves by the same constant, so an uncorrected grid rejects the
    // whole band at once and candidacy never happens. A build that only works at 0 ppm works on
    // synthetic IQ and nothing else.
    //
    // The fit runs on every pass and is NOT replaced by a stored calibration (T-560): the phase it
    // measures is the clock error PLUS where this tuned centre sits against the grid, and only the
    // spectrum knows the second. The absolute clock the alias search settles is what is recorded
    // as C05 state ([`record_receiver_clock`]).
    let grid = grid_fit(buf, fs, raster);
    let grid_offset = grid.map_or(0.0, |g| g.offset_hz);
    if let Some(g) = grid {
        if g.offset_hz.abs() > RASTER_TOLERANCE_HZ {
            inc(&c.cc_grid_corrections);
            if crate::debug_enabled() {
                eprintln!(
                    "hk-pipeline: trunk-cc receiver grid offset {:+.0} Hz ({:+.2} ppm at \
                     {:.4} MHz), concentration {:.2} over {} bins: the {:.0} Hz raster is \
                     re-origined, because the raster tolerance is {:.0} Hz and this is {:.1}x it",
                    g.offset_hz,
                    1e6 * g.offset_hz / tune_center,
                    tune_center / 1e6,
                    g.concentration,
                    g.bins,
                    raster,
                    RASTER_TOLERANCE_HZ,
                    g.offset_hz.abs() / RASTER_TOLERANCE_HZ,
                );
            }
        }
    }
    let origin_hz = tune_center + grid_offset;
    let half = USABLE_FRACTION * fs / 2.0;
    let max_k = ((half - raster / 2.0) / raster).floor();
    if !max_k.is_finite() || max_k < 0.0 {
        return;
    }
    let mut ks: Vec<i64> = (-(max_k as i64)..=(max_k as i64)).collect();
    if ks.len() > node.max_channels {
        // Keep the channels nearest the tuned centre: deterministic, blind, and the part of the
        // window with the least front-end roll-off.
        ks.sort_by_key(|k| k.abs());
        ks.truncate(node.max_channels);
        ks.sort_unstable();
    }
    let Some(fco) = occupancy(buf, fs, raster, grid_offset, &ks) else {
        return;
    };
    add(&c.cc_channels, ks.len() as u64);

    // ---- Candidacy. A pure-FCO detector would stop here and be wrong.
    let mut cands: Vec<(usize, f64)> = (0..ks.len())
        .filter(|&i| fco[i] >= MIN_CC_FCO)
        .map(|i| (i, fco[i]))
        .collect();
    add(&c.cc_candidates, cands.len() as u64);
    // ---- Admission control. Candidacy is cheap; demodulation is not, and on a busy LMR band many
    // channels are continuously occupied. Above the cap the highest-FCO candidates win, ties going
    // to the channel nearest the tuned centre — deterministic, and decided without truth.
    cands.sort_by(|a, b| b.1.total_cmp(&a.1).then(ks[a.0].abs().cmp(&ks[b.0].abs())));
    // ---- T-977: the OTHER reason to spend a demodulation. A channel blind detection already has
    // an emitter on has an emission on it by the run's own decision, and the question "what is it"
    // has an answer a demodulation can give — but an intermittent burst train never reaches
    // `MIN_CC_FCO`, so before this the hunt walked past every one of them. That is how a P25 C4FM
    // voice channel came to read `family: unknown, resolution: not-searched` in a band where the
    // chain had run twelve demodulations.
    //
    // It is not a band-plan lookup and not a second control-channel rule: the emitter came from
    // blind detection, the channel it maps to is the receiver's own fitted grid, and such a channel
    // can never be *confirmed* here (`CcCandidate::new` refuses below the occupancy floor, and this
    // module cannot manufacture a `ConfirmedCc`). What it earns is a verdict.
    let reserved = emitter_channels(shared, &ks, &fco, origin_hz, raster, &cands)
        .into_iter()
        .take(node.max_demods.div_ceil(2))
        .collect::<Vec<_>>();
    add(&c.cc_emitter_candidates, reserved.len() as u64);
    // One budget, not two: the pass still runs at most `max_demods` down-conversions, so the a
    // priori cost in this module's header is unchanged. The reservation is only about *which*
    // channels get the slots when both rules want more than there are.
    let occupancy_budget = node.max_demods.saturating_sub(reserved.len());
    let refused = if cands.len() > occupancy_budget {
        let r = cands.split_off(occupancy_budget);
        add(&c.cc_admission_refused, r.len() as u64);
        r
    } else {
        Vec::new()
    };
    let work: Vec<(usize, f64, CcCandidacy)> = cands
        .iter()
        .map(|&(i, f)| (i, f, CcCandidacy::Occupancy))
        .chain(
            reserved
                .iter()
                .map(|&(i, f)| (i, f, CcCandidacy::DetectedEmitter)),
        )
        .collect();
    if crate::debug_enabled() {
        eprintln!(
            "hk-pipeline: trunk-cc pass at {:.4} MHz: {} raster channels, candidates {:?}, \
             detected-emitter channels {:?}",
            tune_center / 1e6,
            ks.len(),
            cands
                .iter()
                .map(|&(i, f)| (ks[i], format!("{f:.3}")))
                .collect::<Vec<_>>(),
            reserved
                .iter()
                .map(|&(i, f)| (ks[i], format!("{f:.3}")))
                .collect::<Vec<_>>(),
        );
    }
    // A channel the cap refused is a channel NOTHING was measured about beyond its occupancy, and
    // that is its own verdict rather than a silence — the pass report says so, and says how many.
    let mut verdicts: Vec<CcChannelVerdict> = refused
        .iter()
        .map(|&(i, f)| CcChannelVerdict {
            k: ks[i],
            center_hz: origin_hz + ks[i] as f64 * raster,
            bandwidth_hz: raster,
            fco: f,
            candidacy: CcCandidacy::Occupancy,
            outcome: CcOutcome::AdmissionRefused,
            reason: format!(
                "occupancy {:.0} % reached candidacy, but the pass's {} demodulation slots were \
                 already spent on higher-occupancy channels. Nothing was measured about this \
                 channel beyond its occupancy.",
                f * 100.0,
                node.max_demods,
            ),
            framings: Vec::new(),
            levels: None,
            symbol_rate_bd: None,
            symbols: 0,
            emitter_id: None,
        })
        .collect();
    let report = |shared: &Shared, verdicts: Vec<CcChannelVerdict>, t_end: Timestamp| {
        shared.cc_verdicts.record(CcPass {
            pass: pass.index,
            t_start,
            t_end,
            device_id: prov.device_id.clone(),
            tune_center_hz: tune_center,
            raster_hz: raster,
            grid_offset_hz: grid_offset,
            channels_swept: ks.len(),
            channels: verdicts,
        });
    };
    if work.is_empty() {
        report(shared, verdicts, t_start);
        return;
    }

    // ---- Confirmation: frame sync AND valid CRC, on demodulated symbols.
    let demod_cfg = C4fmConfig::default();
    let confirmer = CcConfirmer::default();
    let decim = (fs / (DEMOD_SPS * demod_cfg.symbol_rate_bd))
        .floor()
        .max(1.0);
    let out_rate = fs / decim;
    let t_end = t_start.saturating_add_nanos((buf.len() as f64 * 1e9 / fs) as i64);
    for (i, fco_i, candidacy) in work {
        let k = ks[i];
        // The raster index, kept under a name the confirmed branch's `known.entry(key)` binding
        // does not shadow.
        let channel_k = k;
        let offset = k as f64 * raster + grid_offset;
        let center_hz = origin_hz + k as f64 * raster;
        // Every channel this loop reaches produces a verdict, whatever happens to it. `push` is
        // the one exit: the outcomes below are the closed set of ways a look can end, and adding
        // a `continue` that skips it is how the answer went missing before T-977.
        let push = |verdicts: &mut Vec<CcChannelVerdict>,
                    outcome: CcOutcome,
                    reason: String,
                    framings: Vec<CcFramingVerdict>,
                    structure: Option<FmStructure>,
                    symbols: u64,
                    emitter: Option<hk_model::EmitterId>| {
            verdicts.push(CcChannelVerdict {
                k,
                center_hz,
                bandwidth_hz: raster,
                fco: fco_i,
                candidacy,
                outcome,
                reason,
                framings,
                levels: structure.and_then(|s: FmStructure| s.levels.order()),
                symbol_rate_bd: structure.map(|s: FmStructure| s.symbol_rate_bd),
                symbols,
                emitter_id: emitter.map(|e| e.to_string()),
            });
        };
        let Some(fit) = best_lmr_raster(center_hz, origin_hz, RASTER_TOLERANCE_HZ) else {
            continue;
        };
        // Only an occupancy candidate can become a `CcCandidate`, and only a `CcCandidate` can be
        // confirmed: `CcCandidate::new` refuses below `MIN_CC_FCO`, so a detected-emitter channel
        // is looked at and never promoted. That is the type system holding C23's line, not a
        // convention here.
        let candidate = CcCandidate::new(center_hz, raster, fco_i, fit);
        // A deliberately wide channel, not a 12.5 kHz brick wall: the C4FM demodulator applies its
        // own channel filter, and leaving adjacent energy in is realistic. An extra candidate
        // costs a demodulation and is then rejected by sync + CRC, which is the design.
        let mut spec = DdcSpec::new(offset, 2.0 * raster);
        spec.output_rate_hz = Some(out_rate);
        let mut ddc = match Ddc::new(spec, fs) {
            Ok(d) => d,
            Err(_) => {
                inc(&c.errors);
                push(
                    &mut verdicts,
                    CcOutcome::NotDemodulated,
                    "the channel could not be down-converted from this window".to_owned(),
                    Vec::new(),
                    None,
                    0,
                    None,
                );
                continue;
            }
        };
        let info = InputInfo {
            time: SampleTime {
                sample_index: base,
                host_time: t_start,
            },
            discontinuity: Discontinuity::NONE,
            dropped_before: 0,
            provenance: prov,
        };
        let baseband: Vec<Complex32> = match ddc.process(info, buf) {
            Ok(b) => b.samples.to_vec(),
            Err(_) => {
                inc(&c.errors);
                push(
                    &mut verdicts,
                    CcOutcome::NotDemodulated,
                    "the channel could not be down-converted from this window".to_owned(),
                    Vec::new(),
                    None,
                    0,
                    None,
                );
                continue;
            }
        };
        inc(&c.cc_demods);
        let rate = ddc.output_rate_hz();
        let Some(symbols) = demodulate_best(&baseband, rate, &confirmer) else {
            push(
                &mut verdicts,
                CcOutcome::NotDemodulated,
                "the four-level demodulator recovered no symbols from this window at any \
                 integration depth"
                    .to_owned(),
                Vec::new(),
                None,
                0,
                None,
            );
            continue;
        };
        // Every framing this build knows, not just P25 (T-271). Trying a second one cannot make a
        // false confirmation likely — each carries its own ~1.4e-16-per-frame chance rate — and it
        // is what lets a DMR control channel be found by the same blind hunt.
        //
        // Scanned once here rather than twice (T-977): the scores decide the verdict AND become the
        // synthesis row's "why not that one" trace, so one measurement answers both.
        let scores: Vec<crate::synth::FramingScore> = CC_FRAMINGS
            .iter()
            .map(|&f| {
                let o = confirmer.scan_framing(f, &symbols.dibits);
                crate::synth::FramingScore {
                    framing: f,
                    sync_hits: o.sync_hits,
                    crc_valid: o.crc_valid,
                    crc_checked: o.crc_checked,
                }
            })
            .collect();
        let wire: Vec<CcFramingVerdict> = scores
            .iter()
            .map(|f| CcFramingVerdict {
                framing: f.framing.name().to_owned(),
                sync_hits: f.sync_hits,
                crc_valid: f.crc_valid,
                crc_checked: f.crc_checked,
            })
            .collect();
        let confirmed = candidate
            .as_ref()
            .and_then(|cand| confirmer.confirm_any(cand, &symbols.dibits));
        let Some(cc) = confirmed else {
            if crate::debug_enabled() {
                // What each framing actually saw, so a candidate that should have confirmed can be
                // told apart from one that correctly did not — the decoy has to fail here too.
                for f in &scores {
                    eprintln!(
                        "hk-pipeline: trunk-cc {:.4} MHz unconfirmed under {}: sync {} crc {}/{} \
                         ({} symbols, margin {:.3})",
                        center_hz / 1e6,
                        f.framing.name(),
                        f.sync_hits,
                        f.crc_valid,
                        f.crc_checked,
                        symbols.dibits.len(),
                        symbols.level_margin,
                    );
                }
            }
            // ---- T-977: the verdict, and where it goes. `attach_to_inventory` writes the same
            // three objects it writes for a confirmed channel — the measured structure, the
            // analysis row, the family evidence — with `confirmed: None`, so the emitter reads
            // `resolution: unknown` with the sentence above rather than `not-searched`.
            let resembles = scores
                .iter()
                .filter(|f| f.sync_hits >= MIN_SYNC_HITS)
                .max_by_key(|f| (f.sync_hits, f.crc_valid));
            let outcome = if resembles.is_some() {
                CcOutcome::SyncWithoutCheck
            } else {
                CcOutcome::NoSync
            };
            let reason = match resembles {
                Some(r) => format!(
                    "{} frame syncs under {} at the expected spacing, {} of {} blocks CRC-valid \
                     (floor {}): the air interface is recognised and this is not a control \
                     channel — a voice or data channel of the same system looks exactly like this",
                    r.sync_hits,
                    r.framing.name(),
                    r.crc_valid,
                    r.crc_checked,
                    MIN_CRC_VALID,
                ),
                None => format!(
                    "none of the {} framings in this build found frame sync at the expected \
                     spacing (floor {}) over {} symbols",
                    scores.len(),
                    MIN_SYNC_HITS,
                    symbols.dibits.len(),
                ),
            };
            // Re-filed only when [`may_file_unconfirmed`] says so: the same verdict every half
            // second is one fact, and a confirmation is never walked back by a window that failed
            // to confirm.
            let stands = filed.get(&channel_k) == Some(&CcOutcome::Confirmed);
            let reason = if stands {
                format!(
                    "{reason}. A control channel was CONFIRMED here on an earlier pass, by frame \
                     sync AND CRC; this window produced no valid check, which is the absence of \
                     evidence and not evidence of absence (a fade, a shorter dwell or a drifted \
                     grid all look like this), so the confirmed analysis stands and nothing was \
                     filed over it."
                )
            } else {
                reason
            };
            let mut repo = shared.repo();
            let filing = may_file_unconfirmed(filed.get(&channel_k).copied(), outcome);
            let attached = if filing {
                attach_to_inventory(
                    shared,
                    &mut repo,
                    &AttachInput {
                        confirmed: None,
                        framings: &scores,
                        baseband: &baseband,
                        baseband_rate_hz: rate,
                        center_hz,
                        bandwidth_hz: raster,
                        fco: fco_i,
                        protocol: TrunkProtocol::Unknown,
                        grid,
                        alias: None,
                        tune_center_hz: tune_center,
                        raster_hz: raster,
                        t: t_end,
                        track,
                    },
                )
            } else {
                None
            };
            drop(repo);
            if attached.is_some() {
                inc(&c.cc_verdicts);
                filed.insert(channel_k, outcome);
            }
            push(
                &mut verdicts,
                outcome,
                reason,
                wire,
                attached.and_then(|a| a.structure),
                symbols.dibits.len() as u64,
                attached.map(|a| a.emitter),
            );
            continue;
        };
        inc(&c.cc_confirmed);
        let ev = cc.evidence();
        if crate::debug_enabled() {
            eprintln!(
                "hk-pipeline: trunk-cc confirmed {:.4} MHz (fco {fco_i:.3}): sync {} crc {}/{} \
                 pattern {}",
                cc.cc_freq_hz() / 1e6,
                ev.sync_hits(),
                ev.crc_valid(),
                ev.crc_checked(),
                ev.pattern()
            );
        }
        // ---- Decode (T-268): what the control channel SAID. `confirm` above decided *that* it is
        // one; this reads the blocks that confirmation already CRC-checked. Nothing here can
        // manufacture a `ConfirmedCc` — `crc_valid_blocks` yields bytes, not evidence.
        let key = cc.cc_freq_hz().round() as i64;
        let is_new = !known.contains_key(&key);
        let k = known.entry(key).or_insert_with(|| KnownCc {
            system: TrunkSystem::new(cc.protocol(), Some(cc.cc_freq_hz()), t_end),
            map: ChannelMap::new(),
            tails: HashMap::new(),
        });
        k.system.last_seen = t_end;
        k.system.updated_at = t_end;

        let system_id = k.system.id;
        // What the control channel said depends on what it IS, so the two decoders are kept apart
        // rather than one being asked to read the other's blocks. Only the P25 path builds a band
        // plan, because only P25 announces one (T-271).
        let mut new_entries: Vec<hk_model::ChannelPlanEntry> = Vec::new();
        let events: Vec<GrantEvent> = match cc.framing() {
            CcFraming::P25Phase1 => {
                let scan = scan_blocks(confirmer.crc_valid_blocks(&symbols.dibits).iter());
                add(&c.cc_tsbks, scan.blocks as u64);
                add(&c.cc_iden_ups, scan.iden_ups.len() as u64);
                // An identifier enters the band plan only once agreeing announcements corroborate
                // it, so `observe` returns an entry at most once per identifier — which is what
                // keeps the append-only channel table one row per thing actually learned.
                new_entries = scan
                    .iden_ups
                    .iter()
                    .filter_map(|i| k.map.observe(i, t_end))
                    .collect();
                // Naming the protocol is gated on that same corroboration, so opcode-shaped luck in
                // random blocks cannot name a system (2^-64; `hk_detect::trunk::tsbk`).
                let named = protocol_of(&k.map);
                if named != TrunkProtocol::Unknown {
                    k.system.protocol = named;
                }
                let events: Vec<GrantEvent> = scan
                    .grants
                    .iter()
                    .map(|g| grant_event(system_id, g, &k.map, t_end))
                    .collect();
                if crate::debug_enabled() && (scan.blocks > 0 || !events.is_empty()) {
                    eprintln!(
                        "hk-pipeline: trunk-cc decoded {} TSBK(s): {} iden-up ({} admitted, {} in \
                         plan), {} grant(s), {} unhandled, protocol {:?}",
                        scan.blocks,
                        scan.iden_ups.len(),
                        new_entries.len(),
                        k.map.admitted(),
                        events.len(),
                        scan.unhandled,
                        k.system.protocol
                    );
                }
                events
            }
            CcFraming::DmrBsData => {
                let blocks: Vec<[u8; CSBK_BYTES]> = confirmer
                    .crc_valid_blocks_framing(CcFraming::DmrBsData, &symbols.dibits)
                    .iter()
                    .filter_map(|b| <[u8; CSBK_BYTES]>::try_from(b.as_slice()).ok())
                    .collect();
                let scan = scan_csbks(blocks.iter());
                add(&c.cc_csbks, scan.blocks as u64);
                // A DMR *sync* says "DMR air interface", which a conventional Tier II repeater also
                // has. Naming a system `dmr-tier3` is gated on corroborated trunking messages
                // (3.1e-17 from random blocks; `hk_detect::trunk::dmr::MIN_DMR_CSBKS`).
                let named = dmr_protocol_of(&scan);
                if named != TrunkProtocol::Unknown {
                    k.system.protocol = named;
                }
                let events: Vec<GrantEvent> = scan
                    .grants
                    .iter()
                    .map(|g| dmr_grant_event(system_id, g, t_end))
                    .collect();
                add(&c.cc_dmr_grants, events.len() as u64);
                if crate::debug_enabled() && (scan.blocks > 0 || !events.is_empty()) {
                    eprintln!(
                        "hk-pipeline: trunk-cc decoded {} CSBK(s): {} Tier III, {} grant(s) (none \
                         resolvable: no channel parameters are announced), {} unhandled, \
                         protocol {:?}",
                        scan.blocks,
                        scan.tier3,
                        events.len(),
                        scan.unhandled,
                        k.system.protocol
                    );
                }
                events
            }
            CcFraming::NxdnCac => {
                let blocks: Vec<[u8; NXDN_L3_BYTES]> = confirmer
                    .crc_valid_blocks_framing(CcFraming::NxdnCac, &symbols.dibits)
                    .iter()
                    .filter_map(|b| <[u8; NXDN_L3_BYTES]>::try_from(b.as_slice()).ok())
                    .collect();
                let scan = scan_cacs(blocks.iter());
                add(&c.cc_cacs, scan.blocks as u64);
                // An NXDN *frame sync* says "NXDN air interface", which a conventional repeater
                // also has. Naming a system `nxdn-type-c` is gated on corroborated RCCH-outbound
                // trunking messages (`hk_detect::trunk::nxdn::MIN_NXDN_CACS`).
                let named = nxdn_protocol_of(&scan);
                if named != TrunkProtocol::Unknown {
                    k.system.protocol = named;
                }
                let events: Vec<GrantEvent> = scan
                    .assignments
                    .iter()
                    .map(|a| nxdn_grant_event(system_id, a, t_end))
                    .collect();
                add(&c.cc_nxdn_grants, events.len() as u64);
                if crate::debug_enabled() && (scan.blocks > 0 || !events.is_empty()) {
                    eprintln!(
                        "hk-pipeline: trunk-cc decoded {} CAC(s): {} Type-C, {} assignment(s) \
                         (none resolvable: the air interface carries a channel number and no map), \
                         {} unhandled, protocol {:?}",
                        scan.blocks,
                        scan.type_c,
                        events.len(),
                        scan.unhandled,
                        k.system.protocol
                    );
                }
                events
            }
        };

        // ---- Settle what following needs BEFORE the repository lock (T-628): the in-window
        // targets, the noise reference and — the new part — which absolute receiver offset the
        // modulo-raster grid fit really is. It is DSP over the window in hand, and its answer is
        // receiver provenance the analysis row below records.
        let win = FollowWindow {
            buf,
            base,
            t_start,
            prov,
        };
        let plan = plan_follow(shared, node, &win, &ks, &fco, &events, grid_offset);

        // Metadata only: a protocol, the measured frequency, when it was heard, the band plan it
        // announced and the grants it issued. No audio, no payload, no recording.
        let mut repo = shared.repo();
        match repo.put_trunk_system(&k.system) {
            Ok(()) => {
                if is_new {
                    inc(&c.cc_systems);
                }
            }
            Err(e) => {
                inc(&c.errors);
                eprintln!("hk-pipeline: trunk-cc write: {e}");
                continue;
            }
        }
        // The alias the search just settled is the RECEIVER's clock, measured: C05 calibration
        // state for this device, recorded when it is new or has moved (T-560).
        if let Err(e) = record_receiver_clock(
            &mut repo,
            &prov.device_id,
            &plan.alias,
            tune_center,
            t_start,
        ) {
            inc(&c.errors);
            eprintln!("hk-pipeline: trunk-cc receiver calibration: {e}");
        }
        for entry in &new_entries {
            match repo.append_channel_plan(system_id, entry) {
                Ok(()) => inc(&c.cc_iden_admitted),
                Err(e) => {
                    inc(&c.errors);
                    eprintln!("hk-pipeline: trunk-cc channel plan: {e}");
                }
            }
        }
        for ev in &events {
            match repo.append_grant(ev) {
                Ok(_) if ev.f_hz.is_some() => inc(&c.cc_grants_mapped),
                Ok(_) => inc(&c.cc_grants_unmapped),
                Err(e) => {
                    inc(&c.errors);
                    eprintln!("hk-pipeline: trunk-cc grant: {e}");
                }
            }
        }

        // ---- Confirm the SIGNAL, not just the system (T-546). Everything above wrote a
        // `trunk_system` row; the emission the run detected blind at this frequency is an
        // inventory emitter, and before this the two `docs/07` object graphs never met — the
        // emitter read `family: null`, `estimated_params: null`, while a CRC-valid decode of a
        // trunked control channel sat in a side table. "Successful decode confirms it" is the
        // product vision's step 4 and this is where it happens.
        //
        // The measurements come first and the decode is filed against them: what the emission
        // IS (four levels, 4800 Bd, ±1800 Hz) is measured blind from the same baseband the
        // dibits came from, so `estimated_params` carries measured values or nothing at all.
        let protocol = k.system.protocol;
        let attached = attach_to_inventory(
            shared,
            &mut repo,
            &AttachInput {
                confirmed: Some(&cc),
                framings: &scores,
                baseband: &baseband,
                baseband_rate_hz: rate,
                center_hz,
                bandwidth_hz: raster,
                fco: fco_i,
                protocol,
                grid,
                alias: Some(plan.alias),
                tune_center_hz: tune_center,
                raster_hz: raster,
                t: t_end,
                track,
            },
        );
        filed.insert(channel_k, CcOutcome::Confirmed);
        push(
            &mut verdicts,
            CcOutcome::Confirmed,
            format!(
                "{} frame syncs under {} AND {} of {} blocks CRC-valid: a control channel, and \
                 the system it names is {:?}",
                ev.sync_hits(),
                cc.framing().name(),
                ev.crc_valid(),
                ev.crc_checked(),
                protocol,
            ),
            wire,
            attached.and_then(|a| a.structure),
            symbols.dibits.len() as u64,
            attached.map(|a| a.emitter),
        );

        // ---- Follow (T-269): what the grants above entitle. The repository lock is released
        // first — the following is DSP over the window already in hand, and holding a database
        // lock across it would serialise every other writer behind a channelizer run.
        drop(repo);
        // The truncated-call tails live on the system, across passes: continuing a call is a
        // statement about one channel of one system, and the borrow ends with this call (T-308).
        if let Some(tails) = known.get_mut(&key).map(|k| &mut k.tails) {
            follow_grants(shared, node, &win, &plan, system_id, tails);
        }
    }
    // ---- The pass, as the API serves it (T-977). Recorded once per pass, replacing the last: it
    // is the answer to "which channels did this pass look at, and what did each come to", which no
    // counter can give and no durable row holds for the channels that never reached an emitter.
    report(shared, verdicts, t_end);
}

/// Whether an unconfirmed pass may file its verdict over what this chain has already filed for the
/// channel (T-977).
///
/// Two rules, and the first is the one that matters:
///
/// 1. **A confirmation is never walked back by a window that failed to confirm.** `Confirmed` means
///    frame sync *and* CRC-valid blocks were measured on this channel — 16 bits per valid block
///    against chance — and a later window with no valid check is the **absence of evidence**, not
///    evidence of absence: a fade, a shorter dwell, a grid that drifted, or the admission cap
///    spending its slots elsewhere all produce exactly that. Letting one such window file
///    `resolution: unknown, "this is NOT a control channel"` over a CRC-confirmed emitter would
///    make the strongest decode in the run retractable by the weakest observation, which is the
///    same defect as [`CallHeader::fold`] refusing to let a clear header walk back an encrypted
///    grant. A control channel that genuinely moves is a *new* confirmation elsewhere, not a denial
///    here.
/// 2. Otherwise, file only a **changed** answer. The hunt runs every `period_s` for the life of the
///    run; re-writing the same verdict every half-second would grow the database without adding a
///    fact.
///
/// `None` (nothing filed yet) always files: that is the case the whole ticket exists for.
fn may_file_unconfirmed(previous: Option<CcOutcome>, outcome: CcOutcome) -> bool {
    match previous {
        Some(CcOutcome::Confirmed) => false,
        Some(p) => p != outcome,
        None => true,
    }
}

/// The longest contiguous **keyed** run of an intermittent channel's baseband (T-977).
///
/// Block mean power at ~1 ms, thresholded at the geometric mean of the quietest and loudest block
/// — the midpoint in dB, which sits cleanly between a keying and the noise between keyings at any
/// SNR worth demodulating, and needs no absolute level. The longest run of blocks above it is the
/// span returned.
///
/// **It claims nothing when it cannot separate the two.** A run shorter than
/// [`MIN_KEYED_FRACTION`] of the window, or a window with too few blocks, gives the whole window
/// back: a bad split would be worse than no split, and a continuous channel has nothing to split.
fn keyed_span(baseband: &[Complex32]) -> std::ops::Range<usize> {
    let whole = 0..baseband.len();
    let block = (baseband.len() / 500).max(1);
    let n = baseband.len() / block;
    if n < 8 {
        return whole;
    }
    let power: Vec<f32> = (0..n)
        .map(|b| {
            let s = &baseband[b * block..(b + 1) * block];
            s.iter().map(|x| x.norm_sqr()).sum::<f32>() / block as f32
        })
        .collect();
    let (lo, hi) = power
        .iter()
        .fold((f32::MAX, 0.0f32), |(l, h), &p| (l.min(p), h.max(p)));
    if !(lo.is_finite() && hi.is_finite() && lo > 0.0 && hi > lo) {
        return whole;
    }
    let threshold = (lo * hi).sqrt();
    let (mut best, mut run) = (0..0usize, None::<usize>);
    // One sentinel "off" past the end closes a run that reaches the window's edge.
    let flags = power
        .iter()
        .map(|&p| p >= threshold)
        .chain(std::iter::once(false));
    for (b, on) in flags.enumerate() {
        match (on, run) {
            (true, None) => run = Some(b),
            (false, Some(start)) => {
                if b - start > best.end - best.start {
                    best = start..b;
                }
                run = None;
            }
            _ => {}
        }
    }
    if (best.end - best.start) * 100 < n * MIN_KEYED_FRACTION_PCT {
        return whole;
    }
    (best.start * block)..(best.end * block)
}

/// Least fraction of a window a keyed run must occupy for [`keyed_span`] to use it, per cent.
///
/// Below this the split is measuring something too short to be a symbol alphabet, and the honest
/// answer is the window — whose measurement will then abstain or read low, which is a *result*.
const MIN_KEYED_FRACTION_PCT: usize = 20;

/// Raster channels of the swept set that **blind detection already has an emitter on**, minus the
/// ones occupancy candidacy has already admitted (T-977).
///
/// One repository query for the whole swept span, not one per channel: the sweep is up to
/// `max_channels` wide and this runs every pass, so a per-channel query would put `max_channels`
/// round trips on the chain thread for an answer one range read gives.
///
/// **Why this is not a band-plan lookup.** Nothing here reads an allocation, a licence table or a
/// frequency list. It reads rows *this run* wrote from blind detection, and maps each to the
/// receiver's own fitted grid. An emitter is admitted only if its centre falls inside a swept
/// channel; the run's own decision that there is an emission there is the whole prior.
///
/// Ordered nearest-the-tuned-centre first, which is deterministic and is the part of the window
/// with the least front-end roll-off — the same tie-break admission already uses.
fn emitter_channels(
    shared: &Shared,
    ks: &[i64],
    fco: &[f64],
    origin_hz: f64,
    raster: f64,
    taken: &[(usize, f64)],
) -> Vec<(usize, f64)> {
    let (Some(&lo), Some(&hi)) = (ks.first(), ks.last()) else {
        return Vec::new();
    };
    let span_lo = origin_hz + lo as f64 * raster - raster / 2.0;
    let span_hi = origin_hz + hi as f64 * raster + raster / 2.0;
    if !(span_lo.is_finite() && span_hi > span_lo) {
        return Vec::new();
    }
    let ever = hk_model::TimeRange::new(
        Timestamp::from_unix_nanos(0),
        Timestamp::from_unix_nanos(i64::MAX / 2),
    );
    let rows = {
        let repo = shared.repo();
        match repo.emitters_in_region(&hk_model::Region::new(
            hk_model::FreqRange::new(span_lo, span_hi),
            ever,
        )) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        }
    };
    let mut out: Vec<(usize, f64)> = Vec::new();
    for e in rows {
        let k = ((e.f_center_hz - origin_hz) / raster).round();
        if !k.is_finite() {
            continue;
        }
        let k = k as i64;
        // Inside the channel, not merely nearest it: an emission half a raster away is a different
        // channel, and rounding to it would demodulate the wrong place.
        if (e.f_center_hz - (origin_hz + k as f64 * raster)).abs() > raster / 2.0 {
            continue;
        }
        let Some(i) = ks.iter().position(|&x| x == k) else {
            continue;
        };
        if taken.iter().any(|&(j, _)| j == i) || out.iter().any(|&(j, _)| j == i) {
            continue;
        }
        out.push((i, fco.get(i).copied().unwrap_or(0.0)));
    }
    out.sort_by_key(|&(i, _)| ks[i].abs());
    out
}

/// Demodulates `baseband` once per [`DEMOD_INTEGRATE_LADDER`] entry and keeps the dibits with the
/// most CRC-valid blocks across every framing this build knows (T-628). `None` only when no entry
/// demodulated at all.
fn demodulate_best(
    baseband: &[Complex32],
    rate_hz: f64,
    confirmer: &CcConfirmer,
) -> Option<C4fmSymbols> {
    let mut best: Option<(u64, C4fmSymbols)> = None;
    for integrate_fraction in DEMOD_INTEGRATE_LADDER {
        let cfg = C4fmConfig {
            integrate_fraction,
            ..C4fmConfig::default()
        };
        let Ok(symbols) = C4fmDemod::new(cfg).demodulate(baseband, rate_hz, 0.0) else {
            continue;
        };
        let crc: u64 = CC_FRAMINGS
            .iter()
            .map(|&f| u64::from(confirmer.scan_framing(f, &symbols.dibits).crc_valid))
            .sum();
        if best.as_ref().is_none_or(|(b, _)| crc > *b) {
            best = Some((crc, symbols));
        }
    }
    best.map(|(_, s)| s)
}

/// One raster channel of the buffered window, down-converted and reduced to per-frame mean power.
///
/// This is the **C11 allocation** a followed grant entitles: a real [`Ddc`] channel stream at the
/// LMR channel bandwidth, the one a voice demodulator consumes. What comes back from it here is
/// its *envelope* and nothing else — no symbols, no payload, no audio.
struct ChannelFrames {
    /// Mean power of each [`FOLLOW_FRAME_S`] frame, oldest first.
    powers: Vec<f64>,
    /// Source sample index the first output sample represents. The DDC has already removed its own
    /// group delay from this map (`hk_dsp::ChannelTime`), so no filter-delay term enters a
    /// boundary; what is left is under one input sample.
    source_index: f64,
    /// Source samples per output sample.
    source_per_output: f64,
    /// Output samples per frame.
    frame_len: usize,
}

/// Allocates a channel on `offset_hz` over the window and measures its envelope.
fn channel_frames(
    win: &FollowWindow<'_>,
    offset_hz: f64,
    bandwidth_hz: f64,
) -> Option<ChannelFrames> {
    let mut ddc = Ddc::new(
        DdcSpec::new(offset_hz, bandwidth_hz),
        win.prov.tune.sample_rate_hz,
    )
    .ok()?;
    let info = InputInfo {
        time: SampleTime {
            sample_index: win.base,
            host_time: win.t_start,
        },
        discontinuity: Discontinuity::NONE,
        dropped_before: 0,
        provenance: win.prov,
    };
    let blk = ddc.process(info, win.buf).ok()?;
    let frame_len = ((FOLLOW_FRAME_S * blk.header.sample_rate_hz).round() as usize).max(1);
    let powers: Vec<f64> = blk
        .samples
        .chunks_exact(frame_len)
        .map(|f| f.iter().map(|z| f64::from(z.norm_sqr())).sum::<f64>() / frame_len as f64)
        .collect();
    (!powers.is_empty()).then_some(ChannelFrames {
        powers,
        source_index: blk.header.time.source_index,
        source_per_output: blk.header.time.source_per_output,
        frame_len,
    })
}

/// Splits a channel's per-frame powers into transmissions: `(first frame, last occupied frame,
/// ended)`.
///
/// `ended` is true **only** when `silence_frames` of contiguous silence were actually observed
/// after the run. A run still going when the frames run out comes back unended, and the call it
/// makes keeps `t_end = NULL` rather than borrowing the window's edge for a boundary nobody
/// measured — the same discipline as refusing a frequency the band plan cannot account for.
fn split_keyings(
    powers: &[f64],
    threshold: f64,
    silence_frames: usize,
) -> Vec<(usize, usize, bool)> {
    let need = silence_frames.max(1);
    let mut out = Vec::new();
    let mut open: Option<(usize, usize)> = None;
    let mut quiet = 0usize;
    for (j, &p) in powers.iter().enumerate() {
        if p >= threshold {
            quiet = 0;
            match &mut open {
                Some((_, last)) => *last = j,
                None => open = Some((j, j)),
            }
        } else if let Some((first, last)) = open {
            quiet += 1;
            if quiet >= need {
                out.push((first, last, true));
                open = None;
                quiet = 0;
            }
        }
    }
    if let Some((first, last)) = open {
        out.push((first, last, false));
    }
    out
}

/// Appends a machine reason once. A call that spans passes is written more than once, and a
/// reason list that grew a copy per pass would hit [`hk_model::CALL_REASONS_MAX`] and stop
/// recording anything at all.
fn push_reason(reasons: &mut Vec<String>, reason: &str) {
    if !reasons.iter().any(|r| r == reason) {
        reasons.push(reason.to_owned());
    }
}

/// Whether a call an earlier pass left **truncated** is the same transmission as one already keyed
/// in this pass's first frame (T-308).
///
/// Pure, so the bound can be tested without a pipeline — and it is a *decision about evidence*,
/// which is why it is one function rather than a condition spread through the follower.
///
/// Every clause is a refusal to assume:
///
/// 1. **`first_frame`** — the channel was already keyed when observation resumed. A run that
///    starts later means the receiver *watched* the channel go from silent to keyed, and that
///    start is a measurement; joining it to an older call would overwrite it.
/// 2. **The earlier call is truncated, not ended.** A call with an observed `t_end` ended; nothing
///    continues it. A call with no `observed_until` was never watched at all (outside the window),
///    so there is no boundary to continue *from*.
/// 3. **The unobserved gap is `0 ≤ gap ≤` [`CONTINUATION_GAP_S`]** — short enough that no end
///    could have occurred in it under the same 90 ms silence rule the in-pass splitter uses. On
///    the built-in 0.5 s-in-10 s duty cycle the gap is 9.5 s and this clause refuses, which is the
///    intended answer: the call stays truncated and the new one starts fresh.
/// 4. **Nothing contradicts the identity.** Where both rows name a talkgroup they must name the
///    same one; an absent talkgroup is not evidence of difference, so it neither joins nor splits.
///
/// Frequency and slot are not re-checked here because the caller keys the tails by rounded
/// frequency and TDMA slot: only the same channel slot's tail is ever offered.
fn continues_truncated(
    prev: &CallRecord,
    g: &GrantEvent,
    start: Timestamp,
    first_frame: bool,
) -> bool {
    if !first_frame || prev.t_end.is_some() {
        return false;
    }
    let Some(observed_until) = prev.observed_until else {
        return false;
    };
    let gap_ns = start.as_unix_nanos() - observed_until.as_unix_nanos();
    let within = (0..=(CONTINUATION_GAP_S * 1e9) as i64).contains(&gap_ns);
    let same_talkgroup = match (prev.talkgroup.as_deref(), g.talkgroup.as_deref()) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    };
    within && same_talkgroup
}

/// The `outside-window` row a grant beyond the dwell gets.
///
/// Split out and pure so the refusal can be tested without a pipeline. It keeps the frequency the
/// band plan resolved — the grant is not in doubt, only this receiver's reach — and says how far
/// beyond the window it fell and against which limit, so the row is a measurement rather than a
/// shrug.
fn outside_window_event(
    system: TrunkSystemId,
    g: &GrantEvent,
    tune_center: f64,
    usable_hz: f64,
    fs: f64,
) -> GrantEvent {
    let f = g.f_hz.unwrap_or(f64::NAN);
    let mut ev = GrantEvent::new(system, GrantKind::OutsideWindow, g.t);
    ev.talkgroup = g.talkgroup.clone();
    ev.unit_id = g.unit_id.clone();
    ev.channel = g.channel.clone();
    ev.slot = g.slot;
    ev.f_hz = g.f_hz;
    ev.detail = json!({
        "reason": OUTSIDE_WINDOW,
        "slot": g.slot,
        "granted_by": g.kind.as_str(),
        "window_center_hz": tune_center,
        "window_usable_hz": usable_hz,
        "window_sample_rate_hz": fs,
        "offset_hz": f - tune_center,
        "beyond_usable_hz": (f - tune_center).abs() - usable_hz / 2.0,
        "span_limit_hz": C23_SPAN_LIMIT_HZ,
    });
    ev
}

/// The buffered window a pass follows grants over.
struct FollowWindow<'a> {
    buf: &'a [Complex<i8>],
    base: u64,
    t_start: Timestamp,
    prov: &'a ProvenanceHandle,
}

/// Everything following one pass's grants needs, settled **before** anything is written (T-628).
struct FollowPlan<'e> {
    /// Granted CHANNELS inside the window: one per distinct frequency, with every slot granted on
    /// it (T-272: the slots of a TDMA carrier are one RF channel, measured once). Nearest the
    /// tuned centre first, capped at `max_follows` channels.
    channels: Vec<(f64, Vec<&'e GrantEvent>)>,
    /// Granted frequencies beyond it (C23's span limit).
    outside: Vec<&'e GrantEvent>,
    /// Inside channels the `max_follows` cap refused.
    refused: usize,
    /// The quiet raster channel measured as the floor, and the occupancy threshold over it.
    reference: Option<(i64, f64)>,
    /// Which absolute receiver offset the modulo-raster grid fit is, and why.
    alias: ReceiverAlias,
    /// Per `channels` entry, its envelope under the resolved alias. Empty unless resolved.
    frames: Vec<Option<ChannelFrames>>,
}

/// The absolute receiver offset a plan resolved, Hz, or `None`.
fn resolved_offset(alias: &ReceiverAlias) -> Option<f64> {
    (alias.state == AliasState::Resolved)
        .then_some(alias.offset_hz)
        .flatten()
}

/// Plans one pass's following: which grants are targets, the noise reference, and **the receiver
/// alias** (T-628).
///
/// A grant resolves to an ABSOLUTE transmit frequency through the announced band plan, and the
/// grid fit is only known modulo the raster (see `grid_fit`): +4300 Hz and −8200 Hz name the same
/// grid, and down-converting at the wrong one measures the channel NEXT DOOR — a wrong
/// measurement ("granted channel not radiating", or worse, a neighbour's traffic filed as the
/// call). Until T-628 following was therefore skipped whenever the receiver was measurably
/// off-grid, which on a HackRF One at 800 MHz is always.
///
/// Two constraints settle it, and neither is a band-plan lookup:
///
/// 1. **The crystal's bound** ([`RECEIVER_CLOCK_BOUND_PPM`]) limits the absolute offset to a
///    handful of aliases ([`grid_aliases`]) — three at 851 MHz, one at VHF.
/// 2. **Which alias has energy**: each is tried on every granted channel the pass targets, and
///    the one that finds transmissions on more of them than any other wins
///    ([`resolve_alias`]). A tie is recorded as unresolved and nothing is followed.
///
/// The on-grid case goes through the same search — "the offset is +300 Hz, not −12 200 Hz" is a
/// claim too — so a receiver that is on frequency is *measured* to be, not assumed.
///
/// Cost, bounded a priori: one reference envelope, plus `aliases × min(targets, max_follows)`
/// channelizer runs over the window already held — ≤ 3 × 8 at 800 MHz on a 12.5 kHz raster —
/// and the winner's envelopes are reused for following rather than recomputed.
#[allow(clippy::too_many_arguments)]
fn plan_follow<'e>(
    shared: &Shared,
    node: &TrunkCcNode,
    win: &FollowWindow<'_>,
    ks: &[i64],
    fco: &[f64],
    events: &'e [GrantEvent],
    grid_offset: f64,
) -> FollowPlan<'e> {
    let c = &shared.counters.chains;
    let fs = win.prov.tune.sample_rate_hz;
    let (tune_center, raster) = (win.prov.tune.center_hz, node.raster_hz);
    let usable_hz = USABLE_FRACTION * fs;
    let mut alias = ReceiverAlias {
        state: AliasState::NotTried,
        evidence: AliasEvidence::NoGrantedChannel,
        offset_hz: None,
        ppm: None,
        bound_ppm: RECEIVER_CLOCK_BOUND_PPM,
        candidates: 0,
        targets: 0,
        occupied: 0,
        runner_up: 0,
    };
    let mut plan = FollowPlan {
        channels: Vec::new(),
        outside: Vec::new(),
        refused: 0,
        reference: None,
        alias,
        frames: Vec::new(),
    };
    if !(fs.is_finite() && fs > 0.0 && raster > 0.0) {
        return plan;
    }

    // One target per distinct resolved frequency **and slot**. A control channel repeats a grant
    // and its updates many times in half a second, and that is one call, not twenty — but on a
    // TDMA system two talkgroups share one carrier on alternating slots, and keying those on
    // frequency alone would merge them into a single call attributed to whichever grant arrived
    // first. That is C23's TDMA slot mix-up pitfall, and the slot in the key is what stops it
    // (T-272). An FDMA grant carries `slot: None`, so nothing about the Phase 1 path changes.
    let key = |g: &GrantEvent| (g.f_hz.unwrap_or(f64::NAN).round() as i64, g.slot);
    let mut targets: Vec<&GrantEvent> = Vec::new();
    for ev in events.iter().filter(|e| e.f_hz.is_some()) {
        match targets.iter_mut().find(|t| key(t) == key(ev)) {
            // A plain grant represents the call better than an update: an update is how late entry
            // joins one already in progress, and it carries no source unit.
            Some(rep) if rep.kind == GrantKind::GrantUpdate && ev.kind == GrantKind::Grant => {
                *rep = ev;
            }
            Some(_) => {}
            None => targets.push(ev),
        }
    }
    let reachable = |ev: &GrantEvent| {
        ev.f_hz
            .is_some_and(|f| (f - tune_center).abs() <= usable_hz / 2.0)
    };
    let mut inside: Vec<&GrantEvent> = targets.iter().copied().filter(|e| reachable(e)).collect();
    plan.outside = targets.iter().copied().filter(|e| !reachable(e)).collect();
    // Deterministic and blind: nearest the tuned centre first, where the front end rolls off
    // least; ties by frequency.
    inside.sort_by(|a, b| {
        let (fa, fb) = (a.f_hz.unwrap_or(f64::NAN), b.f_hz.unwrap_or(f64::NAN));
        (fa - tune_center)
            .abs()
            .total_cmp(&(fb - tune_center).abs())
            .then(fa.total_cmp(&fb))
    });
    // ---- One CHANNEL per distinct frequency, whatever the slot. The slots of a TDMA carrier are
    // one RF channel: down-converting it twice would pay the DDC twice for the same samples and
    // measure the same envelope twice — and would count one carrier twice when scoring an alias.
    // So the targets are grouped by frequency here, each channel is measured once, and each
    // slot's call is written from that one measurement (T-272). `inside` is already sorted by
    // distance from the tuned centre, so the members of a group are adjacent.
    let mut channels: Vec<(f64, Vec<&GrantEvent>)> = Vec::new();
    for g in inside {
        let f = g.f_hz.unwrap_or(f64::NAN);
        match channels.last_mut() {
            Some((cf, members)) if cf.to_bits() == f.to_bits() => members.push(g),
            _ => channels.push((f, vec![g])),
        }
    }
    if channels.len() > node.max_follows {
        plan.refused = channels.len() - node.max_follows;
        channels.truncate(node.max_follows);
    }
    plan.channels = channels;
    if plan.channels.is_empty() {
        return plan;
    }
    alias.targets = plan.channels.len() as u32;

    // ---- The noise reference: an empty raster channel through the SAME DDC spec, so the granted
    // channel and its floor are on one scale by construction. `k = 0` is excluded — on a HackRF
    // the tuned centre carries a DC spike, and a reference sitting in it would read as a floor no
    // real channel has, which would quietly cost calls rather than announce anything. It is
    // measured on the FITTED grid, which is where `fco` measured it quiet.
    plan.reference = ks
        .iter()
        .zip(fco)
        .filter(|&(&k, &f)| k != 0 && f <= FOLLOW_REF_MAX_FCO)
        .min_by(|a, b| a.1.total_cmp(b.1).then(a.0.abs().cmp(&b.0.abs())))
        .map(|(k, _)| *k)
        .and_then(|k| channel_frames(win, k as f64 * raster + grid_offset, raster).map(|f| (k, f)))
        .and_then(|(k, r)| {
            let floor = median(&r.powers);
            (floor.is_finite() && floor > 0.0)
                .then(|| (k, floor * 10f64.powf(OCCUPIED_MARGIN_DB / 10.0)))
        });
    let Some((_, threshold)) = plan.reference else {
        alias.evidence = AliasEvidence::NoReference;
        plan.alias = alias;
        return plan;
    };

    // ---- The alias: bounded by the crystal, chosen by which alias has energy. The grid fit's
    // phase is measured against the TUNED CENTRE, which need not sit on a channel (852.456 MHz is
    // 6 kHz off the 800 MHz raster); a granted frequency is on the real grid, so its phase against
    // the tuned centre is removed first and only the receiver's clock reaches the search (T-560).
    let aliases = receiver_aliases(grid_offset, raster, tune_center, plan.channels[0].0);
    alias.candidates = aliases.len() as u32;
    let mut per_alias: Vec<Vec<Option<ChannelFrames>>> = Vec::with_capacity(aliases.len());
    let mut scores: Vec<AliasScore> = Vec::with_capacity(aliases.len());
    for &a in &aliases {
        let frames: Vec<Option<ChannelFrames>> = plan
            .channels
            .iter()
            .map(|(f, _)| channel_frames(win, f - tune_center + a, raster))
            .collect();
        let occupied = frames
            .iter()
            .flatten()
            .filter(|ch| !keyings(ch, threshold, fs).0.is_empty())
            .count();
        scores.push(AliasScore {
            offset_hz: a,
            targets: plan.channels.len(),
            occupied,
        });
        per_alias.push(frames);
    }
    match resolve_alias(&scores) {
        AliasResolution::Resolved {
            offset_hz,
            by,
            occupied,
            runner_up,
            ..
        } => {
            inc(&c.cc_alias_resolved);
            alias.state = AliasState::Resolved;
            alias.evidence = match by {
                hk_detect::trunk::AliasEvidence::ClockBound => AliasEvidence::ClockBound,
                hk_detect::trunk::AliasEvidence::GrantedChannelEnergy => {
                    AliasEvidence::GrantedChannelEnergy
                }
            };
            alias.offset_hz = Some(offset_hz);
            alias.ppm = Some(1e6 * offset_hz / tune_center);
            alias.occupied = occupied as u32;
            alias.runner_up = runner_up as u32;
            let i = aliases
                .iter()
                .position(|&a| a == offset_hz)
                .unwrap_or_default();
            plan.frames = per_alias.swap_remove(i);
        }
        AliasResolution::Unresolved { best, why, .. } => {
            inc(&c.cc_alias_unresolved);
            alias.state = AliasState::Unresolved;
            alias.evidence = match why {
                AliasUnresolved::NoAliasInBound => AliasEvidence::NoAliasInBound,
                AliasUnresolved::NothingOccupied => AliasEvidence::NothingOccupied,
                AliasUnresolved::Tied => AliasEvidence::Tied,
            };
            alias.occupied = best as u32;
            alias.runner_up = best as u32;
        }
    }
    if crate::debug_enabled() {
        eprintln!(
            "hk-pipeline: trunk-cc receiver alias {:?} ({:?}) from grid fit {grid_offset:+.0} Hz: \
             tried {:?} Hz on {} granted channel(s), occupied {:?}",
            alias.state,
            alias.evidence,
            aliases.iter().map(|a| a.round()).collect::<Vec<_>>(),
            plan.channels.len(),
            scores.iter().map(|s| s.occupied).collect::<Vec<_>>(),
        );
    }
    plan.alias = alias;
    plan
}

/// Most voice frames of each kind one call-start event lists (T-849). A 0.5 s window holds at
/// most three LDUs, so this bounds a pathological window rather than trimming an ordinary one.
const VOICE_FRAMES_LISTED: usize = 16;

/// One followed channel's P25 Phase 1 voice frames over the buffered window (T-849).
struct VoiceFrames {
    /// What the scan found, frames in stream order.
    scan: LduScan,
    /// Capture time of each frame's first sync symbol, parallel to `scan.frames`.
    times: Vec<Timestamp>,
    /// One LDU's duration on the air, ns (864 symbols at the demodulated symbol rate).
    ldu_ns: i64,
}

impl VoiceFrames {
    /// One transmission's frames as `grant_event` detail: those whose **midpoint** lies in
    /// `[start, end)`. Link control and encryption sync are listed field by field; the message
    /// indicator is carried verbatim and used for nothing.
    ///
    /// The midpoint, not the first symbol: the transmission's boundaries come from the channel
    /// envelope in [`FOLLOW_FRAME_S`] steps, so a keying's first LDU can begin a step or two
    /// before the measured start, while its midpoint — 90 ms in — can only lie inside the
    /// transmission that carried it (a decoded LDU is 180 ms of continuous signal, and a
    /// transmission only ends after [`SILENCE_TIMEOUT_S`] of silence).
    fn detail(&self, start: Timestamp, end: Timestamp) -> serde_json::Value {
        let inside = |i: usize| self.inside(i, start, end);
        let mut lcs = Vec::new();
        let mut ess = Vec::new();
        for (i, f) in self
            .scan
            .frames
            .iter()
            .enumerate()
            .filter(|(i, _)| inside(*i))
        {
            let t = self.times[i].as_unix_nanos();
            match &f.payload {
                LduPayload::LinkControl(lc) => {
                    let gv = lc.group_voice();
                    lcs.push(json!({
                        "t_ns": t,
                        "nac": f.nid.nac,
                        "lco": lc.lco(),
                        "mfid": lc.mfid(),
                        "protected": lc.protected(),
                        "talkgroup": gv.map(|g| g.talkgroup),
                        "source": gv.map(|g| g.source),
                        "service_options": gv.map(|g| g.service_options),
                        "raw_hex": hex(&lc.bytes),
                        "rs_corrected": f.rs_corrected,
                    }));
                }
                LduPayload::EncryptionSync(es) => {
                    ess.push(json!({
                        "t_ns": t,
                        "nac": f.nid.nac,
                        "algid": es.algid,
                        "algid_name": algid_name(es.algid),
                        "key_id": es.key_id,
                        "mi_hex": hex(&es.mi),
                        "rs_corrected": f.rs_corrected,
                    }));
                }
            }
        }
        json!({
            "attempted": true,
            "decoder": "p25-phase1-ldu",
            "ldu1": lcs.len(),
            "ldu2": ess.len(),
            "link_control": lcs.into_iter().take(VOICE_FRAMES_LISTED).collect::<Vec<_>>(),
            "encryption_sync": ess.into_iter().take(VOICE_FRAMES_LISTED).collect::<Vec<_>>(),
            // The whole window's scan, so a channel whose frames all fell outside this
            // transmission (or never decoded) is legible rather than an empty list.
            "window": {
                "sync_hits": self.scan.sync_hits,
                "nid_valid": self.scan.nid_valid,
                "other_duid": self.scan.other_duid,
                "rs_failed": self.scan.rs_failed,
                "frames": self.scan.frames.len(),
            },
        })
    }
}

impl VoiceFrames {
    /// Whether frame `i`'s midpoint lies in `[start, end)` — the one attribution rule, shared by
    /// the recorded detail and the encryption fold so the two can never disagree about which
    /// frames were this call's.
    fn inside(&self, i: usize, start: Timestamp, end: Timestamp) -> bool {
        let mid = self.times[i].saturating_add_nanos(self.ldu_ns / 2);
        mid >= start && mid < end
    }

    /// This transmission's LDU2 encryption syncs, in stream order (T-330).
    fn syncs(&self, start: Timestamp, end: Timestamp) -> impl Iterator<Item = &EncryptionSync> {
        self.scan
            .frames
            .iter()
            .enumerate()
            .filter(move |(i, _)| self.inside(*i, start, end))
            .filter_map(|(_, f)| match &f.payload {
                LduPayload::EncryptionSync(es) => Some(es),
                LduPayload::LinkControl(_) => None,
            })
    }
}

/// Lower-case hex of `bytes`.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Demodulates one granted channel of the window and reads its P25 Phase 1 voice frames (T-849).
///
/// The same down-conversion and C4FM demodulation the control-channel confirmation uses, at the
/// resolved receiver alias, and the same [`DEMOD_INTEGRATE_LADDER`] chosen by the decode: the
/// fraction yielding the most Reed–Solomon-valid LDUs is kept, ties to the earlier. `None` only
/// when the channel could not be down-converted or demodulated at all.
///
/// Cost per followed FDMA channel, bounded a priori: one DDC over the window already held, at most
/// `DEMOD_INTEGRATE_LADDER.len()` C4FM demodulations of it, and one LDU scan per demodulation —
/// a frame-sync correlation per symbol plus, per sync hit only, a 2^16-codeword NID search. The
/// IMBE voice codewords are skipped by position: nothing here produces audio.
fn voice_frames(win: &FollowWindow<'_>, offset_hz: f64, raster_hz: f64) -> Option<VoiceFrames> {
    let fs = win.prov.tune.sample_rate_hz;
    let demod_cfg = C4fmConfig::default();
    let decim = (fs / (DEMOD_SPS * demod_cfg.symbol_rate_bd))
        .floor()
        .max(1.0);
    let mut spec = DdcSpec::new(offset_hz, 2.0 * raster_hz);
    spec.output_rate_hz = Some(fs / decim);
    let mut ddc = Ddc::new(spec, fs).ok()?;
    let info = InputInfo {
        time: SampleTime {
            sample_index: win.base,
            host_time: win.t_start,
        },
        discontinuity: Discontinuity::NONE,
        dropped_before: 0,
        provenance: win.prov,
    };
    let (baseband, src0, spo): (Vec<Complex32>, f64, f64) = {
        let blk = ddc.process(info, win.buf).ok()?;
        (
            blk.samples.to_vec(),
            blk.header.time.source_index,
            blk.header.time.source_per_output,
        )
    };
    let rate = ddc.output_rate_hz();
    let mut best: Option<(usize, LduScan, C4fmSymbols)> = None;
    for integrate_fraction in DEMOD_INTEGRATE_LADDER {
        let cfg = C4fmConfig {
            integrate_fraction,
            ..C4fmConfig::default()
        };
        let Ok(symbols) = C4fmDemod::new(cfg).demodulate(&baseband, rate, 0.0) else {
            continue;
        };
        let scan = scan_ldus(&symbols.dibits);
        let n = scan.frames.len();
        if best.as_ref().is_none_or(|(b, _, _)| n > *b) {
            best = Some((n, scan, symbols));
        }
    }
    let (_, scan, symbols) = best?;
    let sps = rate / symbols.rate_bd;
    let ldu_ns = (LDU_DIBITS as f64 * 1e9 / symbols.rate_bd) as i64;
    let times = scan
        .frames
        .iter()
        .map(|f| {
            let out_idx = symbols.timing_phase + f.start_dibit as f64 * sps;
            let src = src0 + out_idx * spo;
            win.t_start
                .saturating_add_nanos(((src - win.base as f64).max(0.0) * 1e9 / fs) as i64)
        })
        .collect();
    Some(VoiceFrames {
        scan,
        times,
        ldu_ns,
    })
}

/// A channel envelope's transmissions over `threshold`, and the frame length in seconds.
fn keyings(ch: &ChannelFrames, threshold: f64, fs: f64) -> (Vec<(usize, usize, bool)>, f64) {
    let frame_s = ch.frame_len as f64 * ch.source_per_output / fs;
    let silence_frames = (SILENCE_TIMEOUT_S / frame_s).ceil().max(1.0) as usize;
    (
        split_keyings(&ch.powers, threshold, silence_frames),
        frame_s,
    )
}

/// Follows the grants of one window: calls for the channels inside it, a logged refusal for the
/// channels outside it. See the module docs and [`plan_follow`].
fn follow_grants(
    shared: &Shared,
    node: &TrunkCcNode,
    win: &FollowWindow<'_>,
    plan: &FollowPlan<'_>,
    system: TrunkSystemId,
    tails: &mut HashMap<(i64, Option<u8>), CallRecord>,
) {
    let (t_start, base, prov) = (win.t_start, win.base, win.prov);
    let c = &shared.counters.chains;
    let fs = prov.tune.sample_rate_hz;
    let (tune_center, raster) = (prov.tune.center_hz, node.raster_hz);
    if !(fs.is_finite() && fs > 0.0 && raster > 0.0) {
        return;
    }
    let usable_hz = USABLE_FRACTION * fs;
    let (channels, outside) = (&plan.channels, &plan.outside);

    // Everything written in one repository section at the end, so no lock is held across the DSP.
    // `bool` = this row continues a call an earlier pass left truncated, so it is an UPDATE to an
    // existing row rather than a new call (T-308) and is counted as such.
    let mut writes: Vec<(CallRecord, Vec<GrantEvent>, bool)> = Vec::new();

    // ---- C23's span limit. A grant beyond the window the radio is holding is a ROW, not a
    // silence.
    for g in outside {
        let mut ev = outside_window_event(system, g, tune_center, usable_hz, fs);
        // The call happened; this receiver could not observe it. `t_end` stays NULL, which the
        // model defines as "still open, **or when its end was never observed**", and the reason
        // says which — so a call list shows the traffic instead of hiding it.
        let mut call = CallRecord::from_grant(&ev, g.kind == GrantKind::GrantUpdate);
        call.reasons.push(OUTSIDE_WINDOW.to_owned());
        ev.call = Some(call.id);
        writes.push((call, vec![ev], false));
        inc(&c.cc_grants_outside_window);
    }

    if !channels.is_empty() && plan.reference.is_none() {
        // No quiet channel, or no measurable floor on one. Nothing is claimed rather than measured
        // against a reference the call is itself sitting in.
        inc(&c.cc_follow_no_reference);
    }
    if !channels.is_empty() && plan.alias.state == AliasState::Unresolved {
        // Tried and not settled: every alias the crystal allows was measured and none stood out.
        // Following at a guessed one would file a neighbour's traffic as the granted call, so the
        // grants stand as rows and nothing is followed — and the analysis row records which
        // aliases were tried and why none won, distinct from "never tried".
        add(&c.cc_follow_unresolved, channels.len() as u64);
        if crate::debug_enabled() {
            eprintln!(
                "hk-pipeline: trunk-cc not following {} granted channel(s): receiver alias \
                 unresolved ({:?} over {} candidate(s))",
                channels.len(),
                plan.alias.evidence,
                plan.alias.candidates,
            );
        }
    }

    if let (Some((ref_k, threshold)), Some(offset_hz)) =
        (plan.reference, resolved_offset(&plan.alias))
    {
        add(&c.cc_follow_refused, plan.refused as u64);
        // One CHANNEL per distinct frequency, whatever the slot (T-272): `plan_follow` measured
        // each once under the resolved alias, and each slot's call is written from that one
        // envelope below.
        for ((f, members), ch) in channels.iter().zip(&plan.frames) {
            let f = *f;
            let Some(ch) = ch else {
                inc(&c.errors);
                continue;
            };
            inc(&c.cc_follows);
            let (runs, frame_s) = keyings(ch, threshold, fs);
            if runs.is_empty() {
                // Granted, and nothing was on it in this window. The grant row already records the
                // grant; inventing a call would claim an observation nobody made.
                inc(&c.cc_follow_silent);
                continue;
            }
            let at = |frame: usize| -> Timestamp {
                let src = ch.source_index + (frame * ch.frame_len) as f64 * ch.source_per_output;
                t_start.saturating_add_nanos(((src - base as f64).max(0.0) * 1e9 / fs) as i64)
            };
            // ---- The voice frames the granted channel carried (T-849): its LDU1 link control and
            // LDU2 encryption sync, demodulated from the same window at the same resolved alias.
            // An FDMA channel only — a P25 Phase 2 carrier's voice is TDMA bursts, not LDUs, and
            // reading its two slots as one LDU stream would attribute frames to no slot at all.
            let heard = members
                .iter()
                .all(|g| g.slot.is_none())
                .then(|| voice_frames(win, f - tune_center + offset_hz, raster))
                .flatten();
            if let Some(v) = &heard {
                inc(&c.cc_voice_frame_demods);
                add(&c.cc_ldu1, v.scan.ldu1().count() as u64);
                add(&c.cc_ldu2, v.scan.ldu2().count() as u64);
            }
            // Where observation of THIS channel stopped: the end of the last frame measured. It
            // is a fact about the receiver's schedule, not about the call, and it is what lets a
            // call with no observed end say "truncated here" instead of nothing at all (T-308).
            let observed_until = at(ch.powers.len());
            let fkey = f.round() as i64;
            // Any truncated call each slot of this channel left behind last pass, keyed by
            // frequency AND slot: two talkgroups on one TDMA carrier are two calls, and one must
            // never be continued as the other (T-272). Taken out unconditionally, so a tail that
            // cannot be continued expires here rather than accumulating.
            let member_tails: Vec<Option<CallRecord>> = members
                .iter()
                .map(|g| tails.remove(&(fkey, g.slot)))
                .collect();
            let run_count = runs.len();
            for (m, g, (i, (first, last, ended))) in members
                .iter()
                .enumerate()
                .flat_map(|(m, g)| runs.iter().copied().enumerate().map(move |r| (m, g, r)))
            {
                let (start, end) = (at(first), ended.then(|| at(last + 1)));
                // Active in the window's very first frame means the transmission began before this
                // window: late entry. C23's pitfall is that its encryption state is then UNKNOWN
                // rather than clear, and `from_grant` carries the grant's state verbatim — nothing
                // here reads an encryption bit, so nothing is claimed (T-266, T-270).
                let late = first == 0 || g.kind == GrantKind::GrantUpdate;
                // ---- Cross-pass continuation (T-308). Only the run already keyed in this
                // window's FIRST frame can be the same transmission as one truncated last pass,
                // and only across a gap too short to have hidden an end — see
                // `CONTINUATION_GAP_S`. Everything else starts a new call, and the truncated row
                // it leaves behind stays truncated rather than being closed at a window edge.
                let continued = (i == 0)
                    .then_some(member_tails[m].as_ref())
                    .flatten()
                    .filter(|prev| continues_truncated(prev, g, start, first == 0))
                    .cloned();
                let is_continuation = continued.is_some();
                let mut call = match continued {
                    // The SAME row: same id, same measured `t_start`, same late-entry state. Its
                    // duration grows by what this pass observed instead of a second row appearing
                    // for a transmission that never stopped.
                    Some(mut prev) => {
                        // It is no longer truncated: the next thing observed on this channel was
                        // the same transmission, so the reason that said "we stopped looking"
                        // goes, and the one that says why this row spans a gap arrives.
                        prev.reasons.retain(|r| r != WINDOW_ENDED);
                        push_reason(&mut prev.reasons, CONTINUED);
                        // Later evidence may sharpen encryption; it never walks back towards
                        // clear (`Encryption::refine`, and the repository refuses it again).
                        prev.refine_encryption(g.encryption);
                        prev
                    }
                    None => {
                        let mut call = CallRecord::from_grant(g, late);
                        call.t_start = start;
                        call
                    }
                };
                call.t_end = end;
                // What the receiver watched, for every followed call: with `t_end` it is what
                // separates an observed end from "we stopped looking" (docs/07 §2.29).
                call.observed_until = Some(observed_until);
                push_reason(
                    &mut call.reasons,
                    if ended { SILENCE_TIMEOUT } else { WINDOW_ENDED },
                );
                if first == 0 && !is_continuation {
                    push_reason(&mut call.reasons, LATE_ENTRY);
                }
                // The last run of a still-keyed channel is what the next pass may continue. Only
                // the last one can be unended; the others were closed by observed silence.
                if !ended && i + 1 == run_count {
                    tails.insert((fkey, g.slot), call.clone());
                }
                // A TDMA call's boundaries are the SHARED CARRIER's, not that slot's. Both slots
                // of a P25 Phase 2 channel key the same carrier, and nothing here demodulates the
                // two-slot bursts, so the envelope this measured cannot say which slot was
                // talking when. The slot attribution comes from the grant — which is exactly what
                // the control channel stated — and the row says that the timing does not
                // (T-272). Silence about it would present per-slot timing this build never
                // measured, the same defect as implying resolution nobody captured.
                if call.slot.is_some() {
                    push_reason(&mut call.reasons, TDMA_SHARED_ENVELOPE);
                }
                // ---- The call's own header (T-330): the ALGIDs its LDU2s carried, folded into
                // what the grant announced. The header outranks the grant, except that a clear
                // header never walks back an encrypted grant — see `CallHeader`. This yields an
                // `Encryption`, not a permit: the permit below is asked for like any other.
                let call_end = end.unwrap_or(observed_until);
                let header = heard
                    .as_ref()
                    .map(|v| CallHeader::fold(call.encryption, v.syncs(start, call_end)));
                if let Some(h) = &header {
                    call.encryption = h.call;
                    if h.decided_by_algid() {
                        inc(&c.cc_calls_algid);
                    }
                    if h.contradicts_grant {
                        push_reason(&mut call.reasons, ALGID_CONTRADICTS_GRANT);
                    }
                }
                // A continued call is re-asked every pass, so last pass's refusal must not outlive
                // the evidence that produced it.
                call.reasons
                    .retain(|r| !VOICE_REFUSALS.contains(&r.as_str()));

                // ---- The encryption check, at the point a voice path would be opened (C23
                // §Methods "encryption check before the vocoder", T-270).
                //
                // M4 has no vocoder and no `CallAudio`, so nothing consumes this channel for
                // voice and the permit is always refused today. It is consulted anyway, and its
                // refusal recorded on the call, so that an audio path added later cannot reach
                // samples by omission — it has to hold a `VoicePermit`, and the only way to get
                // one is to ask. `Unknown` fails closed exactly as hard as `Encrypted`, which is
                // C23's late-entry pitfall made structural rather than remembered.
                let voice = VoicePermit::open(call.encryption);
                let voice_reason = match &voice {
                    Ok(_) => "permitted",
                    Err(why) => {
                        push_reason(&mut call.reasons, why.reason());
                        inc(&c.cc_voice_refused);
                        why.reason()
                    }
                };
                if call.encryption.is_encrypted() {
                    inc(&c.cc_calls_encrypted);
                }
                // A continuation is the SAME call, so it gets no second `call-start`: the start
                // it already has was measured, and announcing another would put two beginnings in
                // the grant stream for one transmission. The row's `continued-across-passes`
                // reason is where the join is recorded.
                let mut evs: Vec<GrantEvent> = Vec::new();
                let mut open = GrantEvent::new(system, GrantKind::CallStart, start);
                open.call = Some(call.id);
                open.talkgroup = g.talkgroup.clone();
                open.unit_id = g.unit_id.clone();
                open.channel = g.channel.clone();
                open.slot = g.slot;
                open.f_hz = g.f_hz;
                open.detail = json!({
                    "reason": if late { LATE_ENTRY } else { "grant-followed" },
                    "slot": g.slot,
                    // What the boundaries are, and are not, on a shared TDMA carrier.
                    "timing": if g.slot.is_some() {
                        TDMA_SHARED_ENVELOPE
                    } else {
                        "channel envelope"
                    },
                    // What the encryption check decided, and therefore why no audio exists.
                    "voice": voice_reason,
                    "encryption": call.encryption.state(),
                    "granted_by": g.kind.as_str(),
                    "measured": "channel envelope, C11 DDC over the buffered window",
                    "frame_s": frame_s,
                    "margin_db": OCCUPIED_MARGIN_DB,
                    "reference_raster_channel": ref_k,
                    // Where the granted frequency was actually measured: the transmit frequency
                    // plus the receiver's resolved absolute offset (T-628), and what chose it.
                    "receiver_offset_hz": offset_hz,
                    "receiver_alias": plan.alias.evidence,
                });
                open.detail["continued_across_passes"] = json!(is_continuation);
                // Where the call's encryption state came from (T-330): the header's own
                // statement, and whether it disagreed with the grant.
                open.detail["encryption_evidence"] =
                    json!(call.encryption.evidence().map(|e| e.as_str()));
                open.detail["header_encryption"] = match &header {
                    Some(h) => json!({
                        "read": h.read,
                        "state": h.header.state(),
                        "algid": h.header.algid(),
                        "algid_name": h.header.algid().and_then(algid_name),
                        "key_id": h.header.key_id(),
                        "contradicts_grant": h.contradicts_grant,
                    }),
                    None => serde_json::Value::Null,
                };
                // What this transmission's own voice frames said (T-849), frame by frame. Their
                // ALGIDs were folded into `call.encryption` above (T-330).
                open.detail["voice_frames"] = match &heard {
                    Some(v) => v.detail(start, call_end),
                    None if g.slot.is_some() => json!({
                        "attempted": false,
                        "why": "tdma-carrier: phase 2 voice is not LDU-framed",
                    }),
                    None => json!({ "attempted": false, "why": "channel did not demodulate" }),
                };
                if !is_continuation {
                    evs.push(open);
                }
                if let Some(t_end) = end {
                    let mut close = GrantEvent::new(system, GrantKind::CallEnd, t_end);
                    close.call = Some(call.id);
                    close.talkgroup = g.talkgroup.clone();
                    close.channel = g.channel.clone();
                    close.slot = g.slot;
                    close.f_hz = g.f_hz;
                    close.detail = json!({
                        "reason": SILENCE_TIMEOUT,
                        "silence_timeout_s": SILENCE_TIMEOUT_S,
                    });
                    evs.push(close);
                }
                writes.push((call, evs, is_continuation));
            }
        }
    }

    if writes.is_empty() {
        return;
    }
    if crate::debug_enabled() {
        eprintln!(
            "hk-pipeline: trunk-cc followed {} of {} granted channel(s) ({} outside the \
             {:.3} MHz window at {:.4} MHz): {} call(s)",
            channels.len(),
            channels.len() + outside.len(),
            outside.len(),
            usable_hz / 1e6,
            tune_center / 1e6,
            writes.len(),
        );
    }
    let mut repo = shared.repo();
    for (call, evs, continued) in &writes {
        if let Err(e) = repo.put_call(call) {
            inc(&c.errors);
            eprintln!("hk-pipeline: trunk-cc call: {e}");
            continue;
        }
        if *continued {
            // Not a new call: the same row, grown by what this pass observed.
            inc(&c.cc_calls_continued);
        } else {
            inc(&c.cc_calls);
        }
        if call.t_end.is_some() {
            inc(&c.cc_calls_closed);
        } else if call.observed_until.is_some() {
            // Watched, and still keyed when the window ran out. Written open and TRUNCATED, never
            // closed at the window's edge — the boundary the radio never produced.
            inc(&c.cc_calls_truncated);
        }
        for ev in evs {
            if let Err(e) = repo.append_grant(ev) {
                inc(&c.errors);
                eprintln!("hk-pipeline: trunk-cc call event: {e}");
            }
        }
    }
}

/// Everything [`attach_to_inventory`] needs, gathered at the confirmation site.
struct AttachInput<'a> {
    /// The framing that confirmed, `None` for a channel the hunt looked at and rejected (T-977).
    confirmed: Option<&'a hk_detect::trunk::ConfirmedCc>,
    /// Every framing in the catalogue with what it scored, measured once by the caller.
    framings: &'a [crate::synth::FramingScore],
    baseband: &'a [Complex32],
    baseband_rate_hz: f64,
    center_hz: f64,
    bandwidth_hz: f64,
    fco: f64,
    protocol: TrunkProtocol,
    grid: Option<GridFit>,
    /// The absolute receiver clock the pass's alias search settled; `None` when no search ran,
    /// which is every unconfirmed channel (the search is driven by granted channels).
    alias: Option<ReceiverAlias>,
    tune_center_hz: f64,
    raster_hz: f64,
    t: Timestamp,
    track: Option<hk_model::TrackId>,
}

/// What [`attach_to_inventory`] measured and where it filed it (T-977).
#[derive(Clone, Copy, Debug)]
struct AttachOutcome {
    emitter: hk_model::EmitterId,
    /// The blind symbol-structure measurement, `None` when it abstained.
    structure: Option<FmStructure>,
}

/// Measures what the confirmed channel IS, then files the decode against the inventory emitter at
/// the same frequency (T-546).
///
/// Three things happen, in this order, and each is a `docs/07` object rather than a log line:
///
/// 1. **The symbol structure is measured blind** from the same baseband the dibits came from
///    ([`measure_fm_structure`]) — level count, symbol rate, outer deviation. The C4FM
///    demodulator's `rate_bd` is the rate it was *told*, so it is not the input here.
/// 2. **The analysis is written** as an `emitter_synthesis` row: the chosen pipeline, the
///    per-stage evidence, and the ADR-0021 trace saying what else was tried and why it lost.
/// 3. **The decode becomes evidence on the emitter** and the explanations are re-ranked, so a
///    ranked suggestion carries a measurement instead of the bare allocation
///    (`crate::family::record_decoder_evidence`'s call contract).
///
/// **It creates no emitter.** Like `Inventory::live_trust`, a measurement may be filed against an
/// entry something else made, never conjure one: the emission was found by blind detection, and
/// if detection has not offered a row yet there is nothing here to confirm.
fn attach_to_inventory(
    shared: &Shared,
    repo: &mut hk_model::Repository,
    input: &AttachInput<'_>,
) -> Option<AttachOutcome> {
    let c = &shared.counters.chains;
    // The emitter blind detection already has here, when it has one. A control channel confirms
    // inside the first window while the detector is still accumulating a track, so routinely it
    // does not yet — and `synth::attach` records the sighting rather than dropping the strongest
    // evidence in the run on the floor.
    let emitter = crate::refine::emitter_for_channel(repo, input.center_hz, input.bandwidth_hz)
        .unwrap_or(None);
    // An unconfirmed channel files a verdict against a row blind detection already has, and
    // creates nothing: see [`crate::synth::attach`]. Checked here too so the measurement below is
    // not paid for an answer with nowhere to go.
    if input.confirmed.is_none() && emitter.is_none() {
        return None;
    }

    // ---- T-977: measure the KEYED part of an intermittent channel, not the window.
    //
    // A control channel is continuous by definition, so for one the window *is* the emission and
    // this changes nothing. A voice channel is not: over a 0.5 s window a P25 keying occupies
    // ~0.36 s and the rest is noise, and a level histogram built over both reads the four-level
    // alphabet as two — which is exactly the `mod: 2fsk` the field row carried on a C4FM emission.
    // Gated on the channel's own measured occupancy, so the confirmed path cannot reach it.
    let span = if input.fco < MIN_CC_FCO {
        keyed_span(input.baseband)
    } else {
        0..input.baseband.len()
    };
    // The symbol rate search is bounded by the channel itself: a rate wider than the channel is
    // not physical. No expected rate is supplied.
    let structure = match measure_fm_structure(
        &input.baseband[span],
        input.baseband_rate_hz,
        Some(input.bandwidth_hz),
    ) {
        Ok(s) => {
            inc(&c.cc_structures);
            Some(s)
        }
        Err(e) => {
            if crate::debug_enabled() {
                eprintln!(
                    "hk-pipeline: trunk-cc {:.4} MHz symbol structure not measured: {e:?} -- the \
                     channel framed and CRC-checked, so this is a gap in the MEASUREMENT, not \
                     evidence about the signal",
                    input.center_hz / 1e6,
                );
            }
            None
        }
    };
    let receiver = input.grid.map(|g| ReceiverFit {
        grid_hz: input.raster_hz,
        offset_hz: g.offset_hz,
        concentration: g.concentration,
        ppm: 1e6 * g.offset_hz / input.tune_center_hz,
        alias: input.alias,
    });
    let obs = CcObservation {
        center_hz: input.center_hz,
        bandwidth_hz: input.bandwidth_hz,
        fco: input.fco,
        structure,
        framings: input.framings,
        confirmed: input.confirmed,
        protocol: input.protocol,
        receiver,
        t: input.t,
    };
    let attached = match crate::synth::attach(repo, emitter, &obs) {
        Ok(Some(a)) => {
            inc(&c.cc_attached);
            if crate::debug_enabled() {
                eprintln!(
                    "hk-pipeline: trunk-cc {:.4} MHz attached to emitter {:?} ({}): decoder \
                     evidence {}, structure {:?}",
                    input.center_hz / 1e6,
                    a.emitter,
                    if a.created { "created" } else { "existing" },
                    a.classified,
                    structure,
                );
            }
            a
        }
        // No emitter and nothing that may create one: nothing was written, and the caller says so.
        Ok(None) => return None,
        Err(e) => {
            inc(&c.errors);
            eprintln!("hk-pipeline: trunk-cc inventory attach: {e}");
            return None;
        }
    };
    // Re-rank at once: a decode that does not change what the inventory says the signal is has
    // confirmed nothing the user can see.
    let mut inv = shared
        .inventory
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Err(e) = inv.chain_emitter(repo, input.track, attached.emitter) {
        inc(&c.errors);
        eprintln!("hk-pipeline: trunk-cc chain emitter: {e}");
    }
    Some(AttachOutcome {
        emitter: attached.emitter,
        structure,
    })
}

/// The absolute receiver offsets a pass's alias search tries, Hz: every alias of the receiver's
/// clock within the crystal's bound, with the tuned centre's own phase against the grid removed
/// first via `channel_hz`, one granted frequency (T-560; see [`clock_offset_mod_grid`]).
fn receiver_aliases(grid_offset: f64, raster: f64, tune_center: f64, channel_hz: f64) -> Vec<f64> {
    let clock = clock_offset_mod_grid(grid_offset, raster, tune_center, channel_hz);
    grid_aliases(clock, raster, tune_center, RECEIVER_CLOCK_BOUND_PPM)
}

/// Records the receiver clock a pass's alias search settled as C05 [`CalibrationState`] for
/// `device_id` (T-560; docs/19 §7.6a), and returns the row written, if one was.
///
/// **Only a resolved alias is a clock.** The grid fit alone is known modulo the raster and
/// includes where the tuned centre sits against the grid, so it is never recorded as ppm; the
/// alias is the absolute offset the crystal's bound and the granted channels' energy chose
/// ([`plan_follow`]). An unresolved or untried pass records nothing rather than a guess.
///
/// **Sign.** [`ReceiverAlias::offset_hz`] is where an emission *lands* relative to its transmit
/// frequency; [`CalibrationState::ppm`] is the oscillator's own error, positive = fast. A fast
/// local oscillator puts every emission LOW, so `ppm = −offset / f`: the unit docs/19 §7.6a
/// measured, whose emissions sat 8.2 kHz low at 852 MHz ("−9.6 ppm" as an offset), is an
/// oscillator **+9.6 ppm fast**.
///
/// **Recorded once, not per pass.** A new version is written only when no raster calibration is
/// on record for the device or the measurement has moved by more than [`RASTER_TOLERANCE_HZ`] at
/// this centre — a smaller change cannot move any raster assignment, and re-writing it every
/// half-second pass would bury the version history in noise. A new version `supersedes` the last.
fn record_receiver_clock(
    repo: &mut hk_model::Repository,
    device_id: &str,
    alias: &ReceiverAlias,
    tune_center_hz: f64,
    t: Timestamp,
) -> Result<Option<CalibrationState>, hk_model::RepoError> {
    if alias.state != AliasState::Resolved {
        return Ok(None);
    }
    let Some(offset_ppm) = alias.ppm.filter(|p| p.is_finite()) else {
        return Ok(None);
    };
    let ppm = -offset_ppm;
    let prior =
        repo.latest_calibration_state_for_device(device_id, &CalibrationMethod::LmrRaster)?;
    let unchanged = prior
        .as_ref()
        .is_some_and(|p| (p.ppm - ppm).abs() * 1e-6 * tune_center_hz.abs() <= RASTER_TOLERANCE_HZ);
    if unchanged {
        return Ok(None);
    }
    let cal = CalibrationState {
        id: CalibrationStateId::new(),
        supersedes: prior.map(|p| p.id),
        device_id: device_id.to_owned(),
        ppm,
        method: CalibrationMethod::LmrRaster,
        measured_at: t,
        valid: None,
        temperature_c: None,
        power_table: Vec::new(),
    };
    repo.insert_calibration_state(&cal)?;
    Ok(Some(cal))
}

/// Fits the receiver's own offset from the `raster_hz` channel grid, over `buf` (T-546; docs/19
/// §7.6a, §4.4 step 1).
///
/// One averaged power spectrum of the whole window, a floor taken as its own median, and the
/// circular mean of the above-floor bins modulo the raster — which is
/// [`fit_grid_offset`]'s whole job. Every emission in a capture shares the same receiver offset,
/// so occupied channels reinforce each other and the estimate costs one extra FFT pass per hunt,
/// not one per channel.
///
/// **The fit is modulo the raster, and that is all the data says.** It re-aligns the *grid*; it
/// does not say which grid line an emission is on, so it is applied to the hunt's own channel
/// arithmetic and never directly to a frequency that is already unambiguous: reaching one (a
/// grant) goes through the alias [`plan_follow`] resolves (T-628).
fn grid_fit(buf: &[Complex<i8>], fs: f64, raster_hz: f64) -> Option<GridFit> {
    let n = SWEEP_FFT_LEN;
    let frames = buf.len() / n;
    if frames == 0 || !(fs.is_finite() && fs > 0.0) {
        return None;
    }
    let mut cfg = WelchConfig::new(n);
    cfg.overlap = 0;
    cfg.window = WindowKind::Hann;
    cfg.holds = false;
    cfg.spectral_kurtosis = false;
    let mut engine = SegmentEngine::new(cfg).ok()?;
    const INV: f32 = 1.0 / 128.0;
    let mut seg = vec![Complex32::default(); n];
    let mut avg = vec![0.0f64; n];
    for f in 0..frames {
        for (o, z) in seg.iter_mut().zip(&buf[f * n..(f + 1) * n]) {
            *o = Complex32::new(f32::from(z.re) * INV, f32::from(z.im) * INV);
        }
        engine.process(&seg);
        for (a, &v) in avg.iter_mut().zip(engine.last_power()) {
            *a += f64::from(v);
        }
    }
    // The floor is the spectrum's own median: most of a raster band is empty, so this is measured
    // rather than assumed, exactly as `occupancy` measures the band floor.
    let floor = median(&avg) * 10f64.powf(OCCUPIED_MARGIN_DB / 10.0);
    // Bin i is (i − N/2)·fs/N relative to the tuned centre, which is the frame the grid origin is
    // assumed in.
    let bin_hz = fs / n as f64;
    fit_grid_offset(
        avg.iter()
            .enumerate()
            .map(|(i, &p)| ((i as f64 - (n / 2) as f64) * bin_hz, p)),
        floor,
        raster_hz,
    )
}

/// Frequency-channel occupancy of each channel in `ks`, over `buf`.
///
/// One segmented power sweep gives every channel at once: per 1024-sample frame, the band power of
/// each channel; the channel's own floor is the median over frames, the **band's** floor is the
/// median of those (most raster channels are empty, so this is measured rather than assumed), and
/// a frame is occupied when the channel sits [`OCCUPIED_MARGIN_DB`] above it.
///
/// `None` when the window is too short to say anything, or when the measured floor is degenerate.
fn occupancy(
    buf: &[Complex<i8>],
    fs: f64,
    raster: f64,
    grid_offset_hz: f64,
    ks: &[i64],
) -> Option<Vec<f64>> {
    let n = SWEEP_FFT_LEN;
    let frames = buf.len() / n;
    if frames == 0 || ks.is_empty() {
        return None;
    }
    let mut cfg = WelchConfig::new(n);
    cfg.overlap = 0;
    cfg.window = WindowKind::Hann;
    cfg.holds = false;
    cfg.spectral_kurtosis = false;
    let mut engine = SegmentEngine::new(cfg).ok()?;
    // Each channel's bins in the DC-centred power row: bin i is (i − N/2)·fs/N.
    let bin =
        |hz: f64| ((hz * n as f64 / fs).round() as i64 + (n / 2) as i64).clamp(0, n as i64 - 1);
    let bins: Vec<(usize, usize)> = ks
        .iter()
        .map(|&k| {
            let centre = k as f64 * raster + grid_offset_hz;
            let lo = bin(centre - raster / 2.0) as usize;
            let hi = bin(centre + raster / 2.0) as usize;
            (lo, hi.max(lo))
        })
        .collect();
    const INV: f32 = 1.0 / 128.0;
    let mut seg = vec![Complex32::default(); n];
    let mut powers: Vec<Vec<f64>> = vec![Vec::with_capacity(frames); ks.len()];
    for f in 0..frames {
        for (o, z) in seg.iter_mut().zip(&buf[f * n..(f + 1) * n]) {
            *o = Complex32::new(f32::from(z.re) * INV, f32::from(z.im) * INV);
        }
        engine.process(&seg);
        let row = engine.last_power();
        for (ci, &(lo, hi)) in bins.iter().enumerate() {
            powers[ci].push(row[lo..=hi].iter().map(|&v| f64::from(v)).sum());
        }
    }
    let floors: Vec<f64> = powers.iter().map(|p| median(p)).collect();
    let band_floor = median(&floors);
    if !band_floor.is_finite() || band_floor <= 0.0 {
        return None;
    }
    let threshold = band_floor * 10f64.powf(OCCUPIED_MARGIN_DB / 10.0);
    Some(
        powers
            .iter()
            .map(|p| p.iter().filter(|&&v| v >= threshold).count() as f64 / p.len() as f64)
            .collect(),
    )
}

fn median(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    s[s.len() / 2]
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_detect::trunk::IDEN_MAX_AGE_S;

    /// A tone at `offset_hz` present for `duty` of the window, over noise.
    fn scene(n: usize, fs: f64, emissions: &[(f64, f64)]) -> Vec<Complex<i8>> {
        let mut out = vec![Complex::new(0i8, 0i8); n];
        let mut state = 0x1234_5678u64;
        let mut rand = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            ((state >> 33) as f64 / (1u64 << 31) as f64) - 1.0
        };
        for (i, s) in out.iter_mut().enumerate() {
            let (mut re, mut im) = (4.0 * rand(), 4.0 * rand());
            for &(offset, duty) in emissions {
                if (i as f64) / (n as f64) % 1.0 >= duty {
                    continue;
                }
                let ph = std::f64::consts::TAU * offset * i as f64 / fs;
                re += 40.0 * ph.cos();
                im += 40.0 * ph.sin();
            }
            *s = Complex::new(re.clamp(-128.0, 127.0) as i8, im.clamp(-128.0, 127.0) as i8);
        }
        out
    }

    /// Occupancy separates a continuous channel from a bursty one and from empty ones — which is
    /// candidacy, and candidacy alone. Nothing here confirms anything.
    /// **T-977 review: a confirmation is never walked back by a window that failed to confirm.**
    ///
    /// The defect this pins: `filed` was compared with `!=`, so `Confirmed` followed by `NoSync`
    /// read as "the answer changed" and an unconfirmed pass filed
    /// `resolution: unknown, "this is NOT a control channel"` over an emitter carrying a CRC-valid
    /// P25 decode. One fade, one shorter dwell, one drifted grid, or the admission cap spending its
    /// slots elsewhere is enough to produce that window — so the strongest decode in the run was
    /// retractable by the weakest observation.
    ///
    /// Re-inject the defect (`previous != Some(outcome)`) and the first two assertions go red.
    #[test]
    fn a_crc_confirmed_channel_is_never_denied_by_a_later_window_that_did_not_confirm() {
        for outcome in [
            CcOutcome::NoSync,
            CcOutcome::SyncWithoutCheck,
            CcOutcome::NotDemodulated,
        ] {
            assert!(
                !may_file_unconfirmed(Some(CcOutcome::Confirmed), outcome),
                "{outcome:?} filed over a CRC-valid confirmation: the absence of a check in one \
                 window is not evidence that the control channel is not one",
            );
        }
    }

    /// The other half of the rule, so the fix cannot be "never file anything": a channel that was
    /// never confirmed files its first verdict and every CHANGED one, and stops re-filing an
    /// unchanged one — the hunt runs every `period_s` for the life of the run.
    #[test]
    fn an_unconfirmed_channel_files_its_first_verdict_and_only_changes_after_it() {
        assert!(
            may_file_unconfirmed(None, CcOutcome::SyncWithoutCheck),
            "the first verdict is the whole point: without it the row reads `not-searched`",
        );
        assert!(!may_file_unconfirmed(
            Some(CcOutcome::SyncWithoutCheck),
            CcOutcome::SyncWithoutCheck
        ));
        assert!(may_file_unconfirmed(
            Some(CcOutcome::NoSync),
            CcOutcome::SyncWithoutCheck
        ));
        assert!(may_file_unconfirmed(
            Some(CcOutcome::SyncWithoutCheck),
            CcOutcome::NoSync
        ));
    }

    /// A continuous channel's window IS the emission, so [`keyed_span`] must give the whole of it
    /// back — the confirmed path cannot be changed by the intermittent-channel measurement.
    /// A keyed run too short to be a symbol alphabet also gives the window back rather than a
    /// fragment: a bad split is worse than no split.
    #[test]
    fn keyed_span_splits_an_intermittent_channel_and_leaves_a_continuous_one_whole() {
        let n = 20_000;
        let steady: Vec<Complex32> = (0..n).map(|_| Complex32::new(0.5, 0.0)).collect();
        assert_eq!(
            keyed_span(&steady),
            0..n,
            "a continuous channel has no split"
        );

        // 60 % keyed in the middle, 20 dB down either side.
        let keyed: Vec<Complex32> = (0..n)
            .map(|i| {
                let on = (n / 5..4 * n / 5).contains(&i);
                Complex32::new(if on { 0.5 } else { 0.005 }, 0.0)
            })
            .collect();
        let span = keyed_span(&keyed);
        let (lo, hi) = (span.start as f64 / n as f64, span.end as f64 / n as f64);
        assert!(
            (lo - 0.2).abs() < 0.02 && (hi - 0.8).abs() < 0.02,
            "the keyed run should be found at 20..80 %, got {lo:.2}..{hi:.2}",
        );

        // A 5 % blip is below MIN_KEYED_FRACTION_PCT: claim nothing, hand the window back.
        let blip: Vec<Complex32> = (0..n)
            .map(|i| {
                let on = (n / 2..n / 2 + n / 20).contains(&i);
                Complex32::new(if on { 0.5 } else { 0.005 }, 0.0)
            })
            .collect();
        assert_eq!(keyed_span(&blip), 0..n);
    }

    #[test]
    fn occupancy_separates_continuous_from_bursty_and_empty() {
        let (fs, raster) = (500_000.0, 12_500.0);
        let n = 1 << 17;
        // +3 continuous, −5 on for a quarter of the window, everything else empty.
        let buf = scene(n, fs, &[(3.0 * raster, 1.0), (-5.0 * raster, 0.25)]);
        let ks: Vec<i64> = (-8..=8).collect();
        let fco = occupancy(&buf, fs, raster, 0.0, &ks).expect("a measurable floor");
        let at = |k: i64| fco[ks.iter().position(|&x| x == k).unwrap()];
        assert!(at(3) >= MIN_CC_FCO, "continuous channel: fco {:.3}", at(3));
        assert!(
            at(-5) < MIN_CC_FCO,
            "a quarter-duty channel is not a candidate: fco {:.3}",
            at(-5)
        );
        assert!(at(7) < MIN_CC_FCO, "empty channel: fco {:.3}", at(7));
    }

    /// docs/19 §7.6a's own capture, through the hunt's arithmetic: tuned to 852.456 MHz (itself
    /// 6 kHz off the 851.0125 + k·12.5 kHz raster) on a HackRF whose emissions all land 9.6 ppm
    /// low. Blind, from the IQ alone: every on-raster emission still fits the raster (none is
    /// reported off-raster), and the alias search is offered the receiver's TRUE clock — which it
    /// is not if the tuned centre's own phase is left in the fit (T-560).
    #[test]
    fn a_minus_9_6_ppm_receiver_off_a_channel_still_fits_the_raster_and_offers_its_true_clock() {
        let (fs, raster) = (500_000.0, 12_500.0);
        let tune = 852.456e6;
        let clock = -9.6e-6 * tune; // -8184 Hz: where every emission lands, relative to truth
        // Four real channels on the published grid, including the one granted below.
        let channels = [852.4625e6, 852.4375e6, 852.5e6, 852.375e6];
        let buf = scene(1 << 17, fs, &channels.map(|f| (f - tune + clock, 1.0)));
        let g = grid_fit(&buf, fs, raster).expect("four on-grid emissions concentrate");
        let origin = tune + g.offset_hz;
        for f in channels {
            let seen = f + clock;
            let fit = best_lmr_raster(seen, origin, RASTER_TOLERANCE_HZ);
            assert!(
                fit.is_some(),
                "{:.4} MHz (seen at {:.4}) reported OFF-raster against origin {:.4} MHz",
                f / 1e6,
                seen / 1e6,
                origin / 1e6
            );
            // And without the fit, the uncorrected grid rejects it: this is what T-546 fixed.
            assert!(best_lmr_raster(seen, tune, RASTER_TOLERANCE_HZ).is_none());
        }
        let aliases = receiver_aliases(g.offset_hz, raster, tune, channels[0]);
        assert!(
            aliases.iter().any(|&a| (a - clock).abs() < 300.0),
            "the true clock {clock:.0} Hz is not among the aliases tried: {aliases:?} (grid fit \
             {:+.0} Hz)",
            g.offset_hz
        );
    }

    fn resolved(offset_hz: f64, tune: f64) -> ReceiverAlias {
        ReceiverAlias {
            state: AliasState::Resolved,
            evidence: AliasEvidence::GrantedChannelEnergy,
            offset_hz: Some(offset_hz),
            ppm: Some(1e6 * offset_hz / tune),
            bound_ppm: RECEIVER_CLOCK_BOUND_PPM,
            candidates: 3,
            targets: 2,
            occupied: 2,
            runner_up: 0,
        }
    }

    /// The settled clock is C05 calibration state, with its provenance, recorded ONCE and read
    /// back rather than re-derived; it is re-versioned only when it moves (T-560).
    #[test]
    fn a_resolved_clock_is_recorded_once_as_calibration_state_and_superseded_when_it_moves() {
        let mut repo = hk_model::Repository::open_in_memory().unwrap();
        let (dev, tune) = ("hackrf:t-560", 852.456e6);
        let t = Timestamp::from_unix_nanos(1_000);
        let first = record_receiver_clock(&mut repo, dev, &resolved(-8_184.0, tune), tune, t)
            .unwrap()
            .expect("a resolved alias is a measured clock");
        // Emissions 9.6 ppm LOW = an oscillator 9.6 ppm FAST, in CalibrationState's convention.
        assert!((first.ppm - 9.6).abs() < 0.01, "{}", first.ppm);
        assert_eq!(first.method, CalibrationMethod::LmrRaster);
        assert_eq!(first.device_id, dev);
        assert_eq!(first.measured_at, t);
        assert_eq!(first.supersedes, None);
        let stored = repo
            .latest_calibration_state_for_device(dev, &CalibrationMethod::LmrRaster)
            .unwrap();
        assert_eq!(stored.as_ref(), Some(&first));

        // The next passes measure the same clock within the raster tolerance: nothing is written.
        for jitter in [-300.0, 0.0, 450.0] {
            let again =
                record_receiver_clock(&mut repo, dev, &resolved(-8_184.0 + jitter, tune), tune, t);
            assert_eq!(again.unwrap(), None, "re-recorded at {jitter:+} Hz");
        }
        // Unresolved or never-tried passes are not clocks.
        let mut unresolved = resolved(4_316.0, tune);
        unresolved.state = AliasState::Unresolved;
        assert_eq!(
            record_receiver_clock(&mut repo, dev, &unresolved, tune, t).unwrap(),
            None
        );

        // A clock that has really moved (a warmer crystal, a different receiver on the port) is a
        // new version that names the one it replaces.
        let moved = record_receiver_clock(&mut repo, dev, &resolved(-4_000.0, tune), tune, t)
            .unwrap()
            .expect("moved by more than the raster tolerance");
        assert_eq!(moved.supersedes, Some(first.id));
        assert_eq!(
            repo.latest_calibration_state_for_device(dev, &CalibrationMethod::LmrRaster)
                .unwrap(),
            Some(moved)
        );
    }

    /// The age half of C23's stale-IDEN pitfall, at the row it produces (T-268).
    ///
    /// The e2e suite proves the *unknown*-identifier refusal blind through the device; this proves
    /// the *aged* one, which turns on a ten-minute threshold no two-second fixture can stage
    /// honestly. Same band plan, same grant, same code path — only the clock moves — and the
    /// frequency it used to resolve to must stop being reported rather than quietly go on being
    /// reported.
    #[test]
    fn a_stale_band_plan_reports_an_unmapped_channel_instead_of_the_frequency_it_used_to_mean() {
        let plan = iden_up(1, 170_201_250, 50);
        let mut map = ChannelMap::new();
        let t0 = Timestamp::UNIX_EPOCH.saturating_add_nanos(1_000_000_000);
        map.observe(&plan, t0);
        map.observe(&plan, t0);
        let g = Grant {
            update: false,
            channel: (1 << 12) | 117,
            talkgroup: 1234,
            source: 5678,
            service_options: None,
        };
        let system = TrunkSystemId::new();

        // Fresh, the plan maps — so the refusal below is about the age, not a broken map.
        let fresh = grant_event(system, &g, &map, t0);
        assert_eq!(fresh.kind, GrantKind::Grant);
        let was = fresh.f_hz.expect("a fresh plan resolves the channel");
        assert!((was - 851_737_500.0).abs() < 1e-6, "resolved to {was} Hz");
        fresh.validate().expect("a writable row");

        // The same plan, the same grant, past the limit.
        let later = t0.saturating_add_nanos(((IDEN_MAX_AGE_S + 1.0) * 1e9) as i64);
        let stale = grant_event(system, &g, &map, later);
        assert_eq!(
            stale.kind,
            GrantKind::UnmappedChannel,
            "a stale table must be reported, not used"
        );
        assert_eq!(
            stale.f_hz, None,
            "the stale table still produced a frequency ({was} Hz), which is exactly the pitfall"
        );
        assert_eq!(stale.detail["reason"].as_str(), Some("stale-iden"));
        assert!(stale.detail["iden_age_s"].as_f64().unwrap() > IDEN_MAX_AGE_S);
        assert_eq!(
            stale.detail["iden_max_age_s"].as_f64(),
            Some(IDEN_MAX_AGE_S)
        );
        // Nothing read an encryption bit, so nothing is claimed (T-266, T-270).
        assert_eq!(stale.encryption, hk_model::Encryption::Unknown);
        stale.validate().expect("a writable row");
    }

    /// An identifier the control channel never announced is refused the same way, and never
    /// borrows the parameters of one it did.
    #[test]
    fn an_unannounced_identifier_never_borrows_another_ones_band_plan() {
        let plan = iden_up(1, 170_201_250, 50);
        let mut map = ChannelMap::new();
        let t = Timestamp::UNIX_EPOCH.saturating_add_nanos(1_000_000_000);
        map.observe(&plan, t);
        map.observe(&plan, t);
        let g = Grant {
            update: false,
            channel: (7 << 12) | 300,
            talkgroup: 1,
            source: 0,
            service_options: None,
        };
        let ev = grant_event(TrunkSystemId::new(), &g, &map, t);
        assert_eq!(ev.kind, GrantKind::UnmappedChannel);
        assert_eq!(ev.f_hz, None);
        assert_eq!(ev.detail["reason"].as_str(), Some("no-iden"));
        assert_eq!(ev.detail["iden"].as_u64(), Some(7));
        assert_eq!(ev.unit_id, None, "a zero source unit is absent, not \"0\"");
        ev.validate().expect("a writable row");
    }

    /// An IDEN_UP as it arrives: through the block parser, because [`IdenUp`] has no public
    /// constructor for its raw argument bits.
    fn iden_up(iden: u8, base_field: u32, spacing_field: u16) -> hk_detect::trunk::IdenUp {
        let v: u64 = (u64::from(iden & 0xF) << 60)
            | (1u64 << 50)
            | (u64::from(spacing_field & 0x3FF) << 32)
            | u64::from(base_field);
        let mut block = [0u8; hk_detect::trunk::TSBK_BYTES];
        block[0] = hk_detect::trunk::OP_IDEN_UP;
        block[2..10].copy_from_slice(&v.to_be_bytes());
        hk_detect::trunk::Tsbk::parse(&block)
            .expect("a 12-byte block")
            .iden_up()
            .expect("an IDEN_UP")
    }

    /// The silence timeout is the number its own derivation gives. Asserting the derivation rather
    /// than the value is what stops the constant drifting to whatever makes a scene pass.
    #[test]
    fn the_silence_timeout_lies_between_a_fade_null_and_a_voice_frame() {
        // Both bounds are computed from the quantities they come from rather than written down,
        // so the derivation is the thing under test and the numbers cannot be quietly retuned.
        //
        // One P25 Phase 1 Logical Link Data Unit: 1728 bits at 9600 bit/s.
        let voice_frame_s = 1728.0 / 9600.0_f64;
        // Half a wavelength at 851 MHz, crossed at a slow 5 m/s: the longest a Rayleigh null can
        // make a still-keyed transmitter look silent.
        let fade_null_s = (299_792_458.0 / 851e6) / 2.0 / 5.0_f64;
        assert!(
            SILENCE_TIMEOUT_S > fade_null_s,
            "shorter than a fade null ({fade_null_s:.3} s) splits one keying into two calls"
        );
        assert!(
            SILENCE_TIMEOUT_S < voice_frame_s,
            "a keyed transmitter emits a voice frame every {voice_frame_s:.3} s, so a longer \
             timeout waits past proof that it unkeyed and merges two calls into one"
        );
        // And the window a pass buffers has to be able to HOLD a decision: a keying plus the
        // silence that ends it. `window_s` in the built-in `trunk-cc-hunt` spec.
        let pass_window_s = 0.5_f64;
        assert!(
            2.0 * SILENCE_TIMEOUT_S < pass_window_s,
            "a {pass_window_s} s pass must fit a keying and the silence that closes it"
        );
    }

    /// T-308: the bound on continuing a call across passes is the silence timeout **itself**, and
    /// the duty cycle the built-in hunt runs at is far outside it — so a long transmission is
    /// recorded as truncated rather than joined across nine unwatched seconds, and never closed at
    /// the window's edge.
    #[test]
    fn a_call_is_continued_across_passes_only_over_a_gap_no_end_could_hide_in() {
        // Not a second threshold: one rule, applied whether or not the receiver was looking.
        assert_eq!(
            CONTINUATION_GAP_S, SILENCE_TIMEOUT_S,
            "the gap a call may be joined across is the same silence that would have ended it"
        );
        // The built-in `trunk-cc-hunt` spec: 0.5 s of every 10 s, so 9.5 s is unwatched.
        let (window_s, period_s) = (0.5_f64, 10.0_f64);
        let duty_gap_s = period_s - window_s;
        assert!(
            duty_gap_s > 100.0 * CONTINUATION_GAP_S,
            "the ordinary gap is {duty_gap_s} s, {:.0}x the bound: continuation must NOT fire on              a duty-cycled hunt, because that gap can hold a whole end and a new call",
            duty_gap_s / CONTINUATION_GAP_S
        );

        let system = TrunkSystemId::new();
        let ns = |s: f64| Timestamp::from_unix_nanos((s * 1e9) as i64);
        let mut g = GrantEvent::new(system, GrantKind::Grant, ns(0.0));
        g.talkgroup = Some("4242".into());
        g.f_hz = Some(851.075e6);
        // What the previous pass left: watched to 1.0 s, still keyed, no end observed.
        let mut prev = CallRecord::from_grant(&g, false);
        prev.t_start = ns(0.6);
        prev.observed_until = Some(ns(1.0));
        assert!(prev.duration_is_lower_bound(), "truncated, not ended");

        // Gaps inside the bound join; the duty cycle's gap does not.
        for gap in [0.0, CONTINUATION_GAP_S / 2.0, CONTINUATION_GAP_S] {
            assert!(
                continues_truncated(&prev, &g, ns(1.0 + gap), true),
                "a {gap} s unobserved gap cannot have hidden an end"
            );
        }
        for gap in [CONTINUATION_GAP_S * 1.5, duty_gap_s] {
            assert!(
                !continues_truncated(&prev, &g, ns(1.0 + gap), true),
                "{gap} s unwatched can hold an end, a new grant and a new call"
            );
        }

        // A start the receiver actually WATCHED happen is a measurement, not a continuation.
        assert!(
            !continues_truncated(&prev, &g, ns(1.05), false),
            "the channel was seen going from silent to keyed; that start stands"
        );
        // A call whose end was observed is over, and one never watched has no boundary to
        // continue from.
        let mut ended = prev.clone();
        ended.t_end = Some(ns(1.0));
        assert!(!continues_truncated(&ended, &g, ns(1.05), true));
        let mut never_watched = prev.clone();
        never_watched.observed_until = None;
        assert!(!continues_truncated(&never_watched, &g, ns(1.05), true));
        // Time never runs backwards into a continuation either.
        assert!(!continues_truncated(&prev, &g, ns(0.95), true));

        // Where both name a talkgroup they must agree; an absent one is not evidence of
        // difference, so it neither joins nor splits.
        let mut other = g.clone();
        other.talkgroup = Some("7".into());
        assert!(!continues_truncated(&prev, &other, ns(1.05), true));
        other.talkgroup = None;
        assert!(continues_truncated(&prev, &other, ns(1.05), true));
    }

    /// T-308: the three answers a followed call can give about its end, and the one it may never
    /// give — an end at the window's edge that nothing measured.
    #[test]
    fn an_unfinished_call_reads_as_truncated_and_never_as_an_observed_end() {
        let system = TrunkSystemId::new();
        let ns = |s: f64| Timestamp::from_unix_nanos((s * 1e9) as i64);
        let mut g = GrantEvent::new(system, GrantKind::Grant, ns(0.0));
        g.f_hz = Some(851.075e6);

        // Still keyed when the buffered window (1.0 s) ran out.
        let mut open = CallRecord::from_grant(&g, false);
        open.t_start = ns(0.6);
        open.observed_until = Some(ns(1.0));
        push_reason(&mut open.reasons, WINDOW_ENDED);
        open.validate().expect("a writable row");
        assert_eq!(
            open.ending(),
            hk_model::CallEnding::Truncated {
                observed_until: ns(1.0)
            }
        );
        assert!(!open.end_is_observed());
        assert_eq!(open.duration_ns(), None, "no end was measured");
        assert_eq!(
            open.observed_duration_ns(),
            Some(400_000_000),
            "0.4 s is how long it was WATCHED; the call may have run for minutes"
        );

        // The same call closed by an observed silence is a different claim entirely.
        let mut closed = open.clone();
        closed.t_end = Some(ns(0.9));
        closed.reasons.retain(|r| r != WINDOW_ENDED);
        push_reason(&mut closed.reasons, SILENCE_TIMEOUT);
        closed.validate().expect("a writable row");
        assert_eq!(closed.ending(), hk_model::CallEnding::Observed(ns(0.9)));
        assert!(closed.end_is_observed() && !closed.duration_is_lower_bound());

        // And a reason list cannot grow a copy per pass: a call written five times still says
        // each thing once, so it never hits CALL_REASONS_MAX and starts dropping reasons.
        let mut c = open.clone();
        for _ in 0..5 {
            push_reason(&mut c.reasons, WINDOW_ENDED);
            push_reason(&mut c.reasons, CONTINUED);
        }
        assert_eq!(
            c.reasons,
            vec![WINDOW_ENDED.to_owned(), CONTINUED.to_owned()]
        );
        assert!(c.reasons.len() < hk_model::CALL_REASONS_MAX);
    }

    /// The frame walk: a transmission ends only when the silence after it was actually observed,
    /// and one still going when the window runs out stays open rather than being closed at the
    /// edge.
    #[test]
    fn a_keying_ends_on_observed_silence_and_an_unfinished_one_stays_open() {
        // on 0..3 | quiet 4..9 | on 10..12 | quiet 13,14 | on 15..17
        let mut p = vec![1.0; 18];
        for j in [0, 1, 2, 3, 10, 11, 12, 15, 16, 17] {
            p[j] = 100.0;
        }
        assert_eq!(
            split_keyings(&p, 4.0, 3),
            vec![(0, 3, true), (10, 17, false)],
            "two quiet frames are not three, so the later keyings are still one unfinished run"
        );
        assert_eq!(
            split_keyings(&p, 4.0, 2),
            vec![(0, 3, true), (10, 12, true), (15, 17, false)],
            "a shorter timeout separates them, and the last still runs off the window's end"
        );
        // Nothing above the threshold is no call at all, never a zero-length one.
        assert!(split_keyings(&p, 1000.0, 2).is_empty());
    }

    /// C23's span limit at the row it produces: a grant this receiver cannot reach is REPORTED,
    /// carrying the frequency it resolved to and how far outside the window that fell.
    ///
    /// The refusal is the opposite shape to T-268's: there, the band plan could not produce a
    /// frequency, so none is reported. Here it could, the frequency is right, and what is missing
    /// is the radio's reach — so the frequency IS reported, with the reason it was not followed.
    /// Dropping it would make a system whose voice channels sit outside the dwell look exactly
    /// like a system with no traffic.
    #[test]
    fn a_grant_beyond_the_dwell_window_is_reported_with_the_frequency_it_resolved_to() {
        let system = TrunkSystemId::new();
        let t = Timestamp::UNIX_EPOCH.saturating_add_nanos(1_000_000_000);
        let (center, fs) = (851.0125e6, 500e3);
        let usable = USABLE_FRACTION * fs;
        let mut g = GrantEvent::new(system, GrantKind::Grant, t);
        g.talkgroup = Some("1234".into());
        g.channel = Some("4626".into());
        g.f_hz = Some(851.7375e6);
        assert!(
            (g.f_hz.unwrap() - center).abs() > usable / 2.0,
            "this grant has to be outside the window, or the test proves nothing"
        );

        let ev = outside_window_event(system, &g, center, usable, fs);
        assert_eq!(ev.kind, GrantKind::OutsideWindow);
        assert_eq!(
            ev.f_hz, g.f_hz,
            "the row keeps the frequency it resolved to"
        );
        assert_eq!(ev.channel, g.channel);
        assert_eq!(ev.detail["reason"].as_str(), Some("grant-outside-window"));
        assert!((ev.detail["offset_hz"].as_f64().unwrap() - 725_000.0).abs() < 1.0);
        assert!(ev.detail["beyond_usable_hz"].as_f64().unwrap() > 0.0);
        assert_eq!(ev.detail["span_limit_hz"].as_f64(), Some(C23_SPAN_LIMIT_HZ));
        assert_eq!(ev.encryption, hk_model::Encryption::Unknown);
        ev.validate().expect("a writable row");

        // And the call it opens says why it has no end, rather than there being no row at all.
        let mut call = CallRecord::from_grant(&ev, false);
        call.reasons.push(OUTSIDE_WINDOW.to_owned());
        assert_eq!(call.t_end, None, "its end was never observed");
        assert_eq!(call.f_hz, g.f_hz);
        assert_eq!(call.encryption, hk_model::Encryption::Unknown);
        assert!(!call.encryption.is_clear(), "unknown is never clear");
        call.validate().expect("a writable row");
    }

    /// The third refusal shape in this chain of tasks, at the row it produces (T-271).
    ///
    /// T-268 refuses a channel whose identifier was never announced. T-269 reports a frequency it
    /// resolved and cannot reach. Here there is **no band plan to refuse from**: DMR Tier III
    /// announces no channel parameters this build could corroborate, so a logical channel number
    /// has nothing to resolve through — permanently, not this time — and the row says so while
    /// carrying everything the message actually stated.
    #[test]
    fn a_dmr_grant_is_fully_decoded_and_still_carries_no_frequency() {
        let system = TrunkSystemId::new();
        let t = Timestamp::UNIX_EPOCH.saturating_add_nanos(1_000_000_000);
        let g = DmrGrant {
            csbko: hk_detect::trunk::CSBKO_BTV_GRANT,
            lpcn: 5,
            timeslot: 1,
            flags: 0b101,
            target: 2468,
            source: 1357,
        };
        // What a decoder that assumed the obvious band plan would have produced: base = the tuned
        // centre, step = the 12.5 kHz LMR raster. Computed here only to name what must not appear.
        let plausible_but_unsupported_hz = 851.0125e6 + 12_500.0 * f64::from(g.lpcn);

        let ev = dmr_grant_event(system, &g, t);
        assert_eq!(ev.kind, GrantKind::UnmappedChannel);
        assert_eq!(
            ev.f_hz, None,
            "a DMR grant produced a frequency; the assumed band plan would give \
             {plausible_but_unsupported_hz} Hz, which no message supports"
        );
        // The machine reason the row carries is the decoder's own, not a second copy that could
        // drift away from it.
        assert_eq!(
            ev.detail["reason"].as_str(),
            Some(NO_CHANNEL_PARAMETERS),
            "the chain's reason and the decoder's have diverged: {}",
            ev.detail
        );
        assert_eq!(g.resolve().reason(), NO_CHANNEL_PARAMETERS);

        // Everything the message DID say is recorded — dropping a grant we cannot place is the
        // silence this milestone exists to remove.
        assert_eq!(ev.channel.as_deref(), Some("5"));
        assert_eq!(
            ev.slot,
            Some(1),
            "a two-slot system needs its slot attributed"
        );
        assert_eq!(ev.talkgroup.as_deref(), Some("2468"));
        assert_eq!(ev.unit_id.as_deref(), Some("1357"));
        assert_eq!(ev.detail["opcode"].as_str(), Some("btv-grant"));
        assert_eq!(ev.detail["unverified_flag_bits"].as_u64(), Some(0b101));
        // Nothing read an encryption indication, so nothing is claimed (T-266, T-270).
        assert_eq!(ev.encryption, hk_model::Encryption::Unknown);
        assert!(!ev.encryption.is_clear());
        ev.validate().expect("a writable row");

        // A PRIVATE grant's target is a radio, not a talkgroup, and recording it as one would be a
        // small lie a call list would repeat forever.
        let private = DmrGrant {
            csbko: hk_detect::trunk::CSBKO_P_GRANT,
            ..g
        };
        let ev = dmr_grant_event(system, &private, t);
        assert_eq!(ev.talkgroup, None);
        assert_eq!(ev.detail["target"].as_u64(), Some(2468));
        ev.validate().expect("a writable row");
    }

    /// The fourth refusal shape, at the row it produces (T-345).
    ///
    /// T-271's DMR refusal rests on what could not be *corroborated*. This one rests on what the
    /// air interface **states**: an NXDN Type-C assignment carries a ten-bit channel number and the
    /// standard defines no mapping from one to hertz, so the map is configuration and this build
    /// has none. The row says so, and carries everything the message actually stated.
    #[test]
    fn an_nxdn_assignment_is_fully_decoded_and_still_carries_no_frequency() {
        let system = TrunkSystemId::new();
        let t = Timestamp::UNIX_EPOCH.saturating_add_nanos(1_000_000_000);
        let a = NxdnAssignment {
            message_type: hk_detect::trunk::MSG_VCALL_ASSGN,
            flags: 0b10,
            cc_option: 0x5A,
            call_type: 0b000, // broadcast: a group call
            call_option: 0b00010,
            source: 1357,
            destination: 2468,
            call_timer: 4,
            channel: 6,
            ran: 0x1B,
        };
        // What a decoder that assumed the obvious band plan would have produced: base = the tuned
        // centre, step = the 12.5 kHz LMR raster. Computed here only to name what must not appear.
        let plausible_but_unsupported_hz = 851.0125e6 + 12_500.0 * f64::from(a.channel);

        let ev = nxdn_grant_event(system, &a, t);
        assert_eq!(ev.kind, GrantKind::UnmappedChannel);
        assert_eq!(
            ev.f_hz, None,
            "an NXDN assignment produced a frequency; the assumed band plan would give \
             {plausible_but_unsupported_hz} Hz, which the air interface does not support"
        );
        // The machine reason the row carries is the decoder's own, not a second copy that could
        // drift away from it.
        assert_eq!(
            ev.detail["reason"].as_str(),
            Some(NO_CHANNEL_MAP),
            "the chain's reason and the decoder's have diverged: {}",
            ev.detail
        );
        assert_eq!(a.resolve().reason(), NO_CHANNEL_MAP);
        assert_ne!(
            NO_CHANNEL_MAP, NO_CHANNEL_PARAMETERS,
            "the two refusals are different statements and must be distinguishable in a row"
        );

        // Everything the message DID say is recorded.
        assert_eq!(ev.channel.as_deref(), Some("6"));
        assert_eq!(ev.talkgroup.as_deref(), Some("2468"));
        assert_eq!(ev.unit_id.as_deref(), Some("1357"));
        assert_eq!(ev.detail["message"].as_str(), Some("vcall-assgn"));
        assert_eq!(ev.detail["ran"].as_u64(), Some(0x1B));
        assert_eq!(ev.detail["unverified_spare_flags"].as_u64(), Some(0b10));
        assert_eq!(ev.detail["unverified_cc_option"].as_u64(), Some(0x5A));
        // Nothing read an encryption indication, so nothing is claimed (T-266, T-270).
        assert_eq!(ev.encryption, hk_model::Encryption::Unknown);
        assert!(!ev.encryption.is_clear());
        ev.validate().expect("a writable row");

        // An INDIVIDUAL call's destination is a radio, not a talkgroup.
        let individual = NxdnAssignment {
            call_type: 0b100,
            source: 0,
            ..a
        };
        let ev = nxdn_grant_event(system, &individual, t);
        assert_eq!(ev.talkgroup, None);
        assert_eq!(ev.detail["destination"].as_u64(), Some(2468));
        assert_eq!(
            ev.unit_id, None,
            "the Null Unit ID filler is absent, not \"0\""
        );
        ev.validate().expect("a writable row");
    }

    /// A grant with no frequency entitles no channel, so the follower has nothing to do with it —
    /// which is what keeps a DMR system from producing calls it never observed.
    #[test]
    fn a_grant_with_no_frequency_is_neither_followed_nor_refused_as_out_of_window() {
        let system = TrunkSystemId::new();
        let t = Timestamp::UNIX_EPOCH.saturating_add_nanos(1_000_000_000);
        let g = DmrGrant {
            csbko: hk_detect::trunk::CSBKO_BTV_GRANT,
            lpcn: 5,
            timeslot: 0,
            flags: 0,
            target: 1,
            source: 2,
        };
        let ev = dmr_grant_event(system, &g, t);
        // The follower's own admission rule, stated here as the one line that matters: a target is
        // a distinct RESOLVED frequency, and this event has none.
        assert!(
            [ev.clone()].iter().filter(|e| e.f_hz.is_some()).count() == 0,
            "an unresolved grant became a follow target"
        );
    }

    #[test]
    fn a_window_too_short_or_a_flat_floor_yields_nothing_rather_than_candidates() {
        let (fs, raster) = (500_000.0, 12_500.0);
        assert!(occupancy(&[], fs, raster, 0.0, &[0]).is_none());
        assert!(occupancy(&vec![Complex::new(0i8, 0i8); 512], fs, raster, 0.0, &[0]).is_none());
        // An all-zero window has no floor to measure against, so it produces no candidates at all
        // rather than declaring every channel occupied above a zero threshold.
        let quiet = vec![Complex::new(0i8, 0i8); 1 << 14];
        assert!(occupancy(&quiet, fs, raster, 0.0, &[-1, 0, 1]).is_none());
    }
}
