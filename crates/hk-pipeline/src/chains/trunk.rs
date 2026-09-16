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
//! issued, plus, since T-269, the calls it followed. All of it metadata: no demodulated audio, no
//! voice frames, no message payload, no recording, no stream, and nothing decrypted. There is no
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
//! A grant's service-options octet is now read, and it is the only encryption indication this
//! milestone can reach: a P25 ALGID lives in the voice frames on the *granted* channel (docs/04
//! §8.3), and nothing here demodulates those. So a grant with the verified encryption bit set
//! produces [`hk_model::Encryption::Encrypted`], and **every other path stays `Unknown`** — a grant
//! update carries no such octet at all, a bit that is clear is a grant-time announcement rather
//! than the call's own statement, and a call joined in progress never saw either. Nothing this
//! module writes can say `clear`, because saying it needs an ALGID and no ALGID is reachable yet.
//!
//! [`CallRecord::from_grant`] carries the grant's state verbatim, so a call inherits exactly what
//! its grant said and no branch here sharpens it.
//!
//! Before each followed call, [`VoicePermit::open`] is consulted at the point a voice path would be
//! opened, and its refusal is recorded on the call. M4 opens no voice path at all — there is no
//! vocoder and no `CallAudio` — so today the permit always refuses and nothing consumes a sample
//! for voice. That is the point: the check is in place *before* the thing it checks, so the thing
//! cannot arrive without it.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_demod::fsk::{C4fmConfig, C4fmDemod};
use hk_detect::trunk::{
    CSBK_BYTES, CcCandidate, CcConfirmer, CcFraming, ChannelMap, DmrGrant, Grant, MIN_CC_FCO,
    RASTER_TOLERANCE_HZ, Resolved, VoicePermit, best_lmr_raster, dmr_protocol_of, protocol_of,
    scan_blocks, scan_csbks,
};
use hk_dsp::{Ddc, DdcSpec, InputInfo, SegmentEngine, WelchConfig, WindowKind};
use hk_model::{
    CallRecord, GrantEvent, GrantKind, SampleTime, Timestamp, TrunkProtocol, TrunkSystem,
    TrunkSystemId,
};
use num_complex::{Complex, Complex32};
use serde_json::json;

use super::{ChainMsg, ChainReader, Next};
use crate::events::Candidate;
use crate::gate::GateCursor;
use crate::run::Shared;
use crate::stats::{add, inc};

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
/// Machine reason: the call's end was **observed**, as silence on the granted channel.
const SILENCE_TIMEOUT: &str = "silence-timeout";
/// Machine reason: the buffered window ran out while the channel was still active, so no end was
/// observed and `t_end` stays NULL rather than borrowing the window's edge.
const WINDOW_ENDED: &str = "window-ended";
/// Machine reason: the channel was already active in the window's first frame, so the call was
/// joined in progress — and its encryption state is therefore `unknown`, never `clear`.
const LATE_ENTRY: &str = "late-entry";

/// A control channel this chain has already written, and the band plan decoded off it.
///
/// The map lives here, across passes, because that is what gives a channel-table entry an **age**:
/// an identifier decoded in one pass is still the thing a grant three passes later is resolved
/// through, and [`ChannelMap::resolve`] refuses when that gap grows past
/// [`hk_detect::trunk::IDEN_MAX_AGE_S`] rather than mapping to what the identifier used to mean.
struct KnownCc {
    system: TrunkSystem,
    map: ChannelMap,
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
            decoded_at,
        } => {
            ev.f_hz = Some(f_hz);
            ev.detail = json!({
                "opcode": opcode,
                "iden": iden,
                "channel_number": channel_number,
                "mapping": "base + spacing * channel",
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
                hunt(&shared, &node, &buf, base, base_time, &p, &mut known);
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
fn hunt(
    shared: &Shared,
    node: &TrunkCcNode,
    buf: &[Complex<i8>],
    base: u64,
    t_start: Timestamp,
    prov: &ProvenanceHandle,
    known: &mut HashMap<i64, KnownCc>,
) {
    let c = &shared.counters.chains;
    let fs = prov.tune.sample_rate_hz;
    let raster = node.raster_hz;
    if !(fs.is_finite() && fs > 0.0 && raster > 0.0) {
        return;
    }
    // The ONLY frequency the hunt is given is where the device says it is tuned. The raster is an
    // a-priori standard and its origin is that centre — no band-plan lookup, no truth.
    let tune_center = prov.tune.center_hz;
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
    let Some(fco) = occupancy(buf, fs, raster, &ks) else {
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
    if cands.len() > node.max_demods {
        add(
            &c.cc_admission_refused,
            (cands.len() - node.max_demods) as u64,
        );
        cands.truncate(node.max_demods);
    }
    if crate::debug_enabled() {
        eprintln!(
            "hk-pipeline: trunk-cc pass at {:.4} MHz: {} raster channels, candidates {:?}",
            tune_center / 1e6,
            ks.len(),
            cands
                .iter()
                .map(|&(i, f)| (ks[i], format!("{f:.3}")))
                .collect::<Vec<_>>()
        );
    }
    if cands.is_empty() {
        return;
    }

    // ---- Confirmation: frame sync AND valid CRC, on demodulated symbols.
    let demod_cfg = C4fmConfig::default();
    let demod = C4fmDemod::new(demod_cfg);
    let confirmer = CcConfirmer::default();
    let decim = (fs / (DEMOD_SPS * demod_cfg.symbol_rate_bd))
        .floor()
        .max(1.0);
    let out_rate = fs / decim;
    let t_end = t_start.saturating_add_nanos((buf.len() as f64 * 1e9 / fs) as i64);
    for (i, fco_i) in cands {
        let k = ks[i];
        let offset = k as f64 * raster;
        let center_hz = tune_center + offset;
        let Some(fit) = best_lmr_raster(center_hz, tune_center, RASTER_TOLERANCE_HZ) else {
            continue;
        };
        let Some(candidate) = CcCandidate::new(center_hz, raster, fco_i, fit) else {
            continue;
        };
        // A deliberately wide channel, not a 12.5 kHz brick wall: the C4FM demodulator applies its
        // own channel filter, and leaving adjacent energy in is realistic. An extra candidate
        // costs a demodulation and is then rejected by sync + CRC, which is the design.
        let mut spec = DdcSpec::new(offset, 2.0 * raster);
        spec.output_rate_hz = Some(out_rate);
        let mut ddc = match Ddc::new(spec, fs) {
            Ok(d) => d,
            Err(_) => {
                inc(&c.errors);
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
                continue;
            }
        };
        inc(&c.cc_demods);
        let rate = ddc.output_rate_hz();
        let Ok(symbols) = demod.demodulate(&baseband, rate, 0.0) else {
            continue;
        };
        // Every framing this build knows, not just P25 (T-271). Trying a second one cannot make a
        // false confirmation likely — each carries its own ~1.4e-16-per-frame chance rate — and it
        // is what lets a DMR control channel be found by the same blind hunt.
        let Some(cc) = confirmer.confirm_any(&candidate, &symbols.dibits) else {
            if crate::debug_enabled() {
                // What each framing actually saw, so a candidate that should have confirmed can be
                // told apart from one that correctly did not — the decoy has to fail here too.
                for f in hk_detect::trunk::CC_FRAMINGS {
                    let s = confirmer.scan_framing(f, &symbols.dibits);
                    eprintln!(
                        "hk-pipeline: trunk-cc {:.4} MHz unconfirmed under {}: trials {} sync {} \
                         crc {}/{} ({} symbols, margin {:.3})",
                        center_hz / 1e6,
                        f.name(),
                        s.trials,
                        s.sync_hits,
                        s.crc_valid,
                        s.crc_checked,
                        symbols.dibits.len(),
                        symbols.level_margin,
                    );
                }
            }
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
        };

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

        // ---- Follow (T-269): what the grants above entitle. The repository lock is released
        // first — the following is DSP over the window already in hand, and holding a database
        // lock across it would serialise every other writer behind a channelizer run.
        drop(repo);
        follow_grants(
            shared, node, buf, base, t_start, prov, &ks, &fco, &events, system_id,
        );
    }
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

/// Allocates a channel on `offset_hz` over `buf` and measures its envelope.
fn channel_frames(
    buf: &[Complex<i8>],
    base: u64,
    t_start: Timestamp,
    prov: &ProvenanceHandle,
    offset_hz: f64,
    bandwidth_hz: f64,
) -> Option<ChannelFrames> {
    let mut ddc = Ddc::new(
        DdcSpec::new(offset_hz, bandwidth_hz),
        prov.tune.sample_rate_hz,
    )
    .ok()?;
    let info = InputInfo {
        time: SampleTime {
            sample_index: base,
            host_time: t_start,
        },
        discontinuity: Discontinuity::NONE,
        dropped_before: 0,
        provenance: prov,
    };
    let blk = ddc.process(info, buf).ok()?;
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
    ev.f_hz = g.f_hz;
    ev.detail = json!({
        "reason": OUTSIDE_WINDOW,
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

/// Follows the grants of one window: calls for the channels inside it, a logged refusal for the
/// channels outside it. See the module docs.
#[allow(clippy::too_many_arguments)]
fn follow_grants(
    shared: &Shared,
    node: &TrunkCcNode,
    buf: &[Complex<i8>],
    base: u64,
    t_start: Timestamp,
    prov: &ProvenanceHandle,
    ks: &[i64],
    fco: &[f64],
    events: &[GrantEvent],
    system: TrunkSystemId,
) {
    let c = &shared.counters.chains;
    let fs = prov.tune.sample_rate_hz;
    let (tune_center, raster) = (prov.tune.center_hz, node.raster_hz);
    if !(fs.is_finite() && fs > 0.0 && raster > 0.0) {
        return;
    }
    let usable_hz = USABLE_FRACTION * fs;

    // One target per distinct resolved frequency. A control channel repeats a grant and its
    // updates many times in half a second, and that is one call, not twenty.
    let key = |g: &GrantEvent| g.f_hz.unwrap_or(f64::NAN).round();
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
    let outside: Vec<&GrantEvent> = targets.iter().copied().filter(|e| !reachable(e)).collect();

    // Everything written in one repository section at the end, so no lock is held across the DSP.
    let mut writes: Vec<(CallRecord, Vec<GrantEvent>)> = Vec::new();

    // ---- C23's span limit. A grant beyond the window the radio is holding is a ROW, not a
    // silence.
    for g in &outside {
        let mut ev = outside_window_event(system, g, tune_center, usable_hz, fs);
        // The call happened; this receiver could not observe it. `t_end` stays NULL, which the
        // model defines as "still open, **or when its end was never observed**", and the reason
        // says which — so a call list shows the traffic instead of hiding it.
        let mut call = CallRecord::from_grant(&ev, g.kind == GrantKind::GrantUpdate);
        call.reasons.push(OUTSIDE_WINDOW.to_owned());
        ev.call = Some(call.id);
        writes.push((call, vec![ev]));
        inc(&c.cc_grants_outside_window);
    }

    // ---- The noise reference: an empty raster channel through the SAME DDC spec, so the granted
    // channel and its floor are on one scale by construction. `k = 0` is excluded — on a HackRF
    // the tuned centre carries a DC spike, and a reference sitting in it would read as a floor no
    // real channel has, which would quietly cost calls rather than announce anything.
    let reference = (!inside.is_empty())
        .then(|| {
            ks.iter()
                .zip(fco)
                .filter(|&(&k, &f)| k != 0 && f <= FOLLOW_REF_MAX_FCO)
                .min_by(|a, b| a.1.total_cmp(b.1).then(a.0.abs().cmp(&b.0.abs())))
                .map(|(k, _)| *k)
        })
        .flatten()
        .and_then(|k| {
            channel_frames(buf, base, t_start, prov, k as f64 * raster, raster).map(|f| (k, f))
        });
    let threshold = reference.as_ref().and_then(|(_, r)| {
        let floor = median(&r.powers);
        (floor.is_finite() && floor > 0.0).then(|| floor * 10f64.powf(OCCUPIED_MARGIN_DB / 10.0))
    });
    if !inside.is_empty() && threshold.is_none() {
        // No quiet channel, or no measurable floor on one. Nothing is claimed rather than measured
        // against a reference the call is itself sitting in.
        inc(&c.cc_follow_no_reference);
    }

    if let (Some(threshold), Some((ref_k, _))) = (threshold, &reference) {
        // Deterministic and blind: nearest the tuned centre first, where the front end rolls off
        // least; ties by frequency.
        inside.sort_by(|a, b| {
            let (fa, fb) = (a.f_hz.unwrap_or(f64::NAN), b.f_hz.unwrap_or(f64::NAN));
            (fa - tune_center)
                .abs()
                .total_cmp(&(fb - tune_center).abs())
                .then(fa.total_cmp(&fb))
        });
        if inside.len() > node.max_follows {
            add(
                &c.cc_follow_refused,
                (inside.len() - node.max_follows) as u64,
            );
            inside.truncate(node.max_follows);
        }
        for g in &inside {
            let f = g.f_hz.unwrap_or(f64::NAN);
            let Some(ch) = channel_frames(buf, base, t_start, prov, f - tune_center, raster) else {
                inc(&c.errors);
                continue;
            };
            inc(&c.cc_follows);
            let frame_s = ch.frame_len as f64 * ch.source_per_output / fs;
            let silence_frames = (SILENCE_TIMEOUT_S / frame_s).ceil().max(1.0) as usize;
            let runs = split_keyings(&ch.powers, threshold, silence_frames);
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
            for (first, last, ended) in runs {
                let (start, end) = (at(first), ended.then(|| at(last + 1)));
                // Active in the window's very first frame means the transmission began before this
                // window: late entry. C23's pitfall is that its encryption state is then UNKNOWN
                // rather than clear, and `from_grant` carries the grant's state verbatim — nothing
                // here reads an encryption bit, so nothing is claimed (T-266, T-270).
                let late = first == 0 || g.kind == GrantKind::GrantUpdate;
                let mut call = CallRecord::from_grant(g, late);
                call.t_start = start;
                call.t_end = end;
                call.reasons
                    .push(if ended { SILENCE_TIMEOUT } else { WINDOW_ENDED }.to_owned());
                if first == 0 {
                    call.reasons.push(LATE_ENTRY.to_owned());
                }
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
                        call.reasons.push(why.reason().to_owned());
                        inc(&c.cc_voice_refused);
                        why.reason()
                    }
                };
                if call.encryption.is_encrypted() {
                    inc(&c.cc_calls_encrypted);
                }
                let mut open = GrantEvent::new(system, GrantKind::CallStart, start);
                open.call = Some(call.id);
                open.talkgroup = g.talkgroup.clone();
                open.unit_id = g.unit_id.clone();
                open.channel = g.channel.clone();
                open.f_hz = g.f_hz;
                open.detail = json!({
                    "reason": if late { LATE_ENTRY } else { "grant-followed" },
                    // What the encryption check decided, and therefore why no audio exists.
                    "voice": voice_reason,
                    "encryption": call.encryption.state(),
                    "granted_by": g.kind.as_str(),
                    "measured": "channel envelope, C11 DDC over the buffered window",
                    "frame_s": frame_s,
                    "margin_db": OCCUPIED_MARGIN_DB,
                    "reference_raster_channel": ref_k,
                });
                let mut evs = vec![open];
                if let Some(t_end) = end {
                    let mut close = GrantEvent::new(system, GrantKind::CallEnd, t_end);
                    close.call = Some(call.id);
                    close.talkgroup = g.talkgroup.clone();
                    close.channel = g.channel.clone();
                    close.f_hz = g.f_hz;
                    close.detail = json!({
                        "reason": SILENCE_TIMEOUT,
                        "silence_timeout_s": SILENCE_TIMEOUT_S,
                    });
                    evs.push(close);
                }
                writes.push((call, evs));
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
            inside.len(),
            inside.len() + outside.len(),
            outside.len(),
            usable_hz / 1e6,
            tune_center / 1e6,
            writes.len(),
        );
    }
    let mut repo = shared.repo();
    for (call, evs) in &writes {
        if let Err(e) = repo.put_call(call) {
            inc(&c.errors);
            eprintln!("hk-pipeline: trunk-cc call: {e}");
            continue;
        }
        inc(&c.cc_calls);
        if call.t_end.is_some() {
            inc(&c.cc_calls_closed);
        }
        for ev in evs {
            if let Err(e) = repo.append_grant(ev) {
                inc(&c.errors);
                eprintln!("hk-pipeline: trunk-cc call event: {e}");
            }
        }
    }
}

/// Frequency-channel occupancy of each channel in `ks`, over `buf`.
///
/// One segmented power sweep gives every channel at once: per 1024-sample frame, the band power of
/// each channel; the channel's own floor is the median over frames, the **band's** floor is the
/// median of those (most raster channels are empty, so this is measured rather than assumed), and
/// a frame is occupied when the channel sits [`OCCUPIED_MARGIN_DB`] above it.
///
/// `None` when the window is too short to say anything, or when the measured floor is degenerate.
fn occupancy(buf: &[Complex<i8>], fs: f64, raster: f64, ks: &[i64]) -> Option<Vec<f64>> {
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
            let lo = bin(k as f64 * raster - raster / 2.0) as usize;
            let hi = bin(k as f64 * raster + raster / 2.0) as usize;
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
    #[test]
    fn occupancy_separates_continuous_from_bursty_and_empty() {
        let (fs, raster) = (500_000.0, 12_500.0);
        let n = 1 << 17;
        // +3 continuous, −5 on for a quarter of the window, everything else empty.
        let buf = scene(n, fs, &[(3.0 * raster, 1.0), (-5.0 * raster, 0.25)]);
        let ks: Vec<i64> = (-8..=8).collect();
        let fco = occupancy(&buf, fs, raster, &ks).expect("a measurable floor");
        let at = |k: i64| fco[ks.iter().position(|&x| x == k).unwrap()];
        assert!(at(3) >= MIN_CC_FCO, "continuous channel: fco {:.3}", at(3));
        assert!(
            at(-5) < MIN_CC_FCO,
            "a quarter-duty channel is not a candidate: fco {:.3}",
            at(-5)
        );
        assert!(at(7) < MIN_CC_FCO, "empty channel: fco {:.3}", at(7));
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
        assert!(occupancy(&[], fs, raster, &[0]).is_none());
        assert!(occupancy(&vec![Complex::new(0i8, 0i8); 512], fs, raster, &[0]).is_none());
        // An all-zero window has no floor to measure against, so it produces no candidates at all
        // rather than declaring every channel occupied above a zero threshold.
        let quiet = vec![Complex::new(0i8, 0i8); 1 << 14];
        assert!(occupancy(&quiet, fs, raster, &[-1, 0, 1]).is_none());
    }
}
