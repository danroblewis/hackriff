//! **Conventional DMR (Tier II)**: identifying one from any 4FSK 4800 Bd emission, and reading the
//! headers it sends (T-989).
//!
//! # Why this exists beside [`crate::trunk::dmr`]
//!
//! T-271 decodes a DMR **Tier III** trunked control channel — a system with a dedicated outbound
//! signalling channel — and it does so over a deliberately flattened framing (no BPTC, no
//! interleaving, the burst laid out as sync-then-block). Nothing in that path claims anything
//! about **conventional** DMR, which is what a repeater on a land-mobile channel actually
//! transmits: no control channel, no CSBKs to hunt, just two 30 ms TDMA slots carrying voice and
//! data bursts. An explorer window on 2026-09-25 measured exactly that gap — a +24 dB DMR repeater
//! at 464.6125 MHz, 42 base-station data syncs in ten seconds by an independent oracle, and every
//! burst arriving in the inventory with **no classification at all**.
//!
//! So this module asks the one question T-271's path cannot: *is this 4FSK emission DMR?* It
//! answers from the air interface alone — the frame sync, the CACH, the slot type, the BPTC block
//! — and never from a band plan, an allocation or a frequency lookup (the product rule: blind
//! detection first, the database only afterwards, as a ranked suggestion).
//!
//! # The burst, and why the arithmetic is the check
//!
//! A DMR burst is **264 bits** at 4800 symbols/s (132 dibits, 27.5 ms), laid out
//!
//! ```text
//!   [ info 98 ][ slot type 10 ][ SYNC or embedded signalling 48 ][ slot type 10 ][ info 98 ]
//! ```
//!
//! and on a base station's downlink each burst is preceded by a **24-bit CACH** (12 dibits, 2.5
//! ms), so one TDMA slot is 288 bits = 144 dibits = exactly 30 ms, and two slots are the 60 ms
//! TDMA frame. Every one of those identities has to close, and they do: 98+10+48+10+98 = 264,
//! 264+24 = 288, 288/2 = 144 symbols = 30 ms at 4800 Bd. A layout one field wrong does not add up
//! — the same check T-271 used on the CSBK's 96 bits, and the reason a mis-recollection of the
//! structure is not the risk here.
//!
//! A **voice** burst uses those same 216 information bits for vocoder frames and its middle 48
//! bits for either a sync (burst A of a superframe) or embedded signalling; it therefore carries
//! **no slot type**, and this module reads none from one. Embedded link control (the LC fragments
//! spread over bursts B–E, with their own Reed–Solomon) is **not** decoded here and is not
//! claimed: what a voice burst contributes is its sync, which is what identifies the emission.
//!
//! # The ten sync words, and what verifies them
//!
//! [`DMR_SYNCS`] holds all ten 48-bit sync patterns. Two published relations verify them, both as
//! tests ([`tests::every_sync_word_is_all_outer_symbols`],
//! [`tests::voice_and_data_syncs_are_exact_dibit_complements`]):
//!
//! 1. **Every dibit of every sync is an outer symbol** (±3 in 4FSK terms, i.e. every hex digit is
//!    one of 5, 7, D, F). That is the documented property of DMR's sync words, and a typo in any
//!    digit breaks it with probability 3/4.
//! 2. **Voice and data syncs pair as exact dibit complements** — XOR with 0xAAAA_AAAA_AAAA maps
//!    each voice word onto its data word. Two independent constants reproducing that by arithmetic
//!    is not what a copied typo does. The base-station pair is additionally the one T-271 verified
//!    independently, and this module reuses those two constants rather than restating them.
//!
//! # What identification rests on, and its false-alarm arithmetic
//!
//! Identification needs [`MIN_TIER2_SYNCS`] sync hits at [`SYNC_TOLERANCE_DIBITS`] or fewer wrong
//! dibits. For random dibits the chance that one of the ten patterns matches at one position is
//!
//! ```text
//!   10 × (C(24,0) + C(24,1)·3 + C(24,2)·3²) / 4²⁴  =  10 × 2557 / 2.81e14  ≈  9.1e-11
//! ```
//!
//! so over a 0.25 s window (1200 dibit positions) a single false hit runs at ~1e-7 and **two** at
//! ~1e-14. Two is therefore not a tuned number: one hit would already be safe, and the second is
//! there so that a lone hit inside a long noisy stream cannot name a band's emission by itself.
//! The threshold is a priori and is never to be edited to make a run pass (the rule
//! [`crate::trunk::confirm`] and `tsbk.rs` state for their own floors; ADR-0016 §7).
//!
//! **Burst-grid consistency is measured, not required.** Real bursts sit on a 144-dibit grid, and
//! [`Tier2Scan::grid_consistent`] counts the hits that do; a symbol slip mid-window would cost
//! that count without being evidence against DMR, so it is reported and never gates.
//!
//! # What a colour code has to earn
//!
//! The slot type's Golay(20,8) is the one piece of coding whose *specific* generator could not be
//! corroborated offline ([`fec`]), so a decoded colour code is reported **only when a second burst
//! agrees with it** ([`MIN_CC_AGREEMENTS`]). With a wrong generator, agreement does not happen and
//! the verdict says `CC unknown` — a stated gap, not a confident wrong number.
//!
//! # Metadata only
//!
//! Voice payload bits are skipped **by position** and no vocoder exists in this workspace; what is
//! read out of a burst is signalling — colour code, slot, data type, and a header's addresses —
//! which is the same metadata-only class the trunking chain runs under. Nothing here decrypts
//! anything: a privacy-indicator header is reported as *present*, and its key material is not
//! touched.
//!
//! # In process, not a plugin (ADR-0010)
//!
//! ADR-0010 puts GPLv3 code behind the plugin process boundary, and a DMR decoder is exactly the
//! kind of thing that usually arrives under the GPL. **Nothing here is derived from one**: the
//! framing, the codes and the field layouts are written from the air interface's own arithmetic,
//! verified as above, and the only code reused is this workspace's own (`hk-estimate`'s CRC,
//! `hk-demod`'s 4FSK symbol recovery, `trunk::dmr`'s CSBK parser). So the licence reason to spawn
//! a process does not apply, and three reasons not to do:
//!
//! 1. **It runs on every narrowband region, always on.** Identification is a sync search over one
//!    already-demodulated window — a few hundred microseconds — and paying a process spawn, a
//!    pipe and a manifest per region would cost orders of magnitude more than the work.
//! 2. **It needs the burst structure, not a bitstream.** A plugin consumes a stream through a
//!    manifest's ports; what this needs is the dibits a C4FM demodulator already produced inside
//!    the run, and the emitter row it writes evidence to.
//! 3. **DMR already lives in this crate.** `trunk::dmr` is here, its verified sync constants and
//!    its CSBK parser are reused by this module rather than restated, and splitting the two
//!    halves of one air interface across a process boundary would guarantee they drift.
//!
//! A third-party DMR *application* decoder (voice, data reassembly, a vocoder) is a different
//! question and still belongs behind the boundary; this module deliberately stops short of it.
//!
//! # NOT a standards-compliant DMR receiver
//!
//! It does not reassemble data packets, does not decode embedded link control, does not track the
//! two slots' call state, and does no vocoder work. It identifies the air interface and reads the
//! headers that stand alone in one burst. What it refuses to do is guess: an unresolvable BPTC, a
//! failed CRC or a failed RS parity produces a *count*, never a payload.

pub mod fec;

use fec::{BPTC_PAYLOAD_BYTES, BptcStats};
use hk_estimate::framing::crc::BitCrc;

pub use crate::trunk::dmr::{DMR_BS_DATA_SYNC_DIBITS, DMR_BS_VOICE_SYNC_DIBITS};

/// DMR's symbol rate, Bd (9600 bit/s over 4FSK dibits).
pub const DMR_SYMBOL_RATE_BD: f64 = 4800.0;
/// Dibits in a frame sync (48 bits).
pub const SYNC_DIBITS: usize = 24;
/// Dibits in a burst (264 bits).
pub const BURST_DIBITS: usize = 132;
/// Dibits in each of a burst's two information halves (98 bits).
pub const INFO_DIBITS: usize = 49;
/// Dibits in each half of the slot type (10 bits).
pub const SLOT_TYPE_HALF_DIBITS: usize = 5;
/// Dibits in the CACH that precedes a downlink burst (24 bits).
pub const CACH_DIBITS: usize = 12;
/// Dibits in one 30 ms TDMA slot: CACH + burst.
pub const SLOT_GRID_DIBITS: usize = CACH_DIBITS + BURST_DIBITS;
/// Bits in a slot type, both halves together.
pub const SLOT_TYPE_BITS: usize = 20;
/// Bits in a CACH.
pub const CACH_BITS: usize = 24;

/// Wrong dibits a sync match may carry. Same tolerance the P25 confirmer uses, with the
/// false-alarm arithmetic for *these* patterns in the module docs.
pub const SYNC_TOLERANCE_DIBITS: usize = 2;

/// Sync hits before this build will say an emission is DMR. A priori; see the module docs for the
/// ~1e-14 false-alarm arithmetic behind it.
pub const MIN_TIER2_SYNCS: usize = 2;

/// Bursts that must decode the **same** colour code before one is reported.
pub const MIN_CC_AGREEMENTS: usize = 2;

/// Most sync hits examined in one scan, so a long window's cost is bounded.
pub const MAX_BURSTS_PER_SCAN: usize = 512;

/// Who transmits a sync word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncSource {
    /// A base station (repeater) downlink.
    BaseStation,
    /// A mobile station, inbound to a repeater.
    MobileStation,
    /// Direct mode, timeslot 1.
    DirectSlot1,
    /// Direct mode, timeslot 2.
    DirectSlot2,
    /// The reserved pattern: named so a hit on it is reported rather than silently dropped.
    Reserved,
}

/// What follows the sync in the burst.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncPayload {
    /// A voice burst: 216 vocoder bits, no slot type.
    Voice,
    /// A data/control burst: 196 BPTC bits and a slot type.
    Data,
    /// A mobile station's reverse-channel signalling.
    ReverseChannel,
    /// Reserved by the air interface.
    Reserved,
}

/// One of DMR's ten frame syncs.
#[derive(Clone, Copy, Debug)]
pub struct DmrSync {
    /// Short name for a row a person reads.
    pub name: &'static str,
    /// The 48-bit pattern.
    pub hex: u64,
    /// Who sends it.
    pub source: SyncSource,
    /// What follows it.
    pub payload: SyncPayload,
    /// The pattern as 24 dibits, MSB first.
    pub dibits: [u8; SYNC_DIBITS],
}

/// The 24 dibits of a 48-bit sync word, MSB first.
pub const fn sync_dibits(hex: u64) -> [u8; SYNC_DIBITS] {
    let mut out = [0u8; SYNC_DIBITS];
    let mut i = 0;
    while i < SYNC_DIBITS {
        out[i] = ((hex >> (46 - 2 * i)) & 0b11) as u8;
        i += 1;
    }
    out
}

/// The XOR that maps a voice sync onto its data sync: every dibit's most significant bit inverted.
pub const SYNC_COMPLEMENT: u64 = 0xAAAA_AAAA_AAAA;

const fn sync(name: &'static str, hex: u64, source: SyncSource, payload: SyncPayload) -> DmrSync {
    DmrSync {
        name,
        hex,
        source,
        payload,
        dibits: sync_dibits(hex),
    }
}

/// Every DMR frame sync, with who sends it and what follows.
///
/// The base-station pair is T-271's independently verified pair (`0xDFF57D75DF5D` data,
/// `0x755FD7DF75F7` voice); the rest are verified by the two published relations in the module
/// docs, which every one of them satisfies.
pub const DMR_SYNCS: [DmrSync; 10] = [
    sync(
        "bs-voice",
        0x755F_D7DF_75F7,
        SyncSource::BaseStation,
        SyncPayload::Voice,
    ),
    sync(
        "bs-data",
        0xDFF5_7D75_DF5D,
        SyncSource::BaseStation,
        SyncPayload::Data,
    ),
    sync(
        "ms-voice",
        0x7F7D_5DD5_7DFD,
        SyncSource::MobileStation,
        SyncPayload::Voice,
    ),
    sync(
        "ms-data",
        0xD5D7_F77F_D757,
        SyncSource::MobileStation,
        SyncPayload::Data,
    ),
    sync(
        "ms-rc",
        0x77D5_5F7D_FD77,
        SyncSource::MobileStation,
        SyncPayload::ReverseChannel,
    ),
    sync(
        "direct-ts1-voice",
        0x5D57_7F77_57FF,
        SyncSource::DirectSlot1,
        SyncPayload::Voice,
    ),
    sync(
        "direct-ts1-data",
        0xF7FD_D5DD_FD55,
        SyncSource::DirectSlot1,
        SyncPayload::Data,
    ),
    sync(
        "direct-ts2-voice",
        0x7DFF_D5F5_5D5F,
        SyncSource::DirectSlot2,
        SyncPayload::Voice,
    ),
    sync(
        "direct-ts2-data",
        0xD755_7F5F_F7F5,
        SyncSource::DirectSlot2,
        SyncPayload::Data,
    ),
    sync(
        "reserved",
        0xDD7F_F5D7_57DD,
        SyncSource::Reserved,
        SyncPayload::Reserved,
    ),
];

/// One frame sync found in a dibit stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncHit {
    /// Index of the sync's first dibit.
    pub dibit: usize,
    /// Index into [`DMR_SYNCS`].
    pub sync: usize,
    /// Dibits that disagreed.
    pub errors: usize,
}

impl SyncHit {
    /// The sync word this hit matched.
    pub fn word(&self) -> &'static DmrSync {
        &DMR_SYNCS[self.sync]
    }
}

/// Every DMR frame sync in `dibits`, best match first at each position, non-overlapping.
///
/// Cost is `positions × 10 × 24` dibit comparisons with an early exit past `tolerance`, which is
/// ~290 k comparisons over a 0.25 s window — the whole reason identification can be tried on any
/// narrowband emission rather than only where a band plan expects one.
pub fn find_syncs(dibits: &[u8], tolerance: usize) -> Vec<SyncHit> {
    let mut hits: Vec<SyncHit> = Vec::new();
    if dibits.len() < SYNC_DIBITS {
        return hits;
    }
    let mut pos = 0usize;
    while pos + SYNC_DIBITS <= dibits.len() {
        let window = &dibits[pos..pos + SYNC_DIBITS];
        let mut best: Option<SyncHit> = None;
        for (i, s) in DMR_SYNCS.iter().enumerate() {
            let mut errors = 0usize;
            for (a, b) in window.iter().zip(s.dibits.iter()) {
                if a != b {
                    errors += 1;
                    if errors > tolerance {
                        break;
                    }
                }
            }
            if errors <= tolerance && best.is_none_or(|b| errors < b.errors) {
                best = Some(SyncHit {
                    dibit: pos,
                    sync: i,
                    errors,
                });
            }
        }
        match best {
            // A hit consumes its own 24 dibits: two syncs cannot overlap, and the next real one is
            // a burst away.
            Some(h) => {
                hits.push(h);
                pos += SYNC_DIBITS;
            }
            None => pos += 1,
        }
    }
    hits
}

/// What a data burst's slot type says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotType {
    /// Colour code, 0–15: the repeater's own "squelch code".
    pub colour_code: u8,
    /// What the burst's 196 information bits are.
    pub data_type: DataType,
    /// Bits the Golay code corrected.
    pub corrected: u32,
}

/// The kind of block a data burst carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataType {
    /// Privacy-indicator header: the call is encrypted. Reported, never decrypted.
    PiHeader,
    /// Voice LC header: the link control of the call that follows.
    VoiceLcHeader,
    /// Terminator with LC: the link control of the call that just ended.
    TerminatorWithLc,
    /// Control signalling block (the Tier III path's payload, properly coded).
    Csbk,
    /// Data header: the header of a packet-data transfer.
    DataHeader,
    /// Idle burst: the slot is keyed but carries nothing.
    Idle,
    /// A value this build does not name — a statement, not a gap to fill with a guess.
    Unnamed(u8),
}

impl DataType {
    /// The 4-bit code's meaning, as far as this build corroborated one.
    pub fn from_code(code: u8) -> Self {
        match code & 0x0F {
            0 => DataType::PiHeader,
            1 => DataType::VoiceLcHeader,
            2 => DataType::TerminatorWithLc,
            3 => DataType::Csbk,
            6 => DataType::DataHeader,
            9 => DataType::Idle,
            other => DataType::Unnamed(other),
        }
    }

    /// The 4-bit code.
    pub fn code(&self) -> u8 {
        match self {
            DataType::PiHeader => 0,
            DataType::VoiceLcHeader => 1,
            DataType::TerminatorWithLc => 2,
            DataType::Csbk => 3,
            DataType::DataHeader => 6,
            DataType::Idle => 9,
            DataType::Unnamed(v) => *v & 0x0F,
        }
    }

    /// A short name for a row a person reads; `None` for one this build does not name.
    pub fn name(&self) -> Option<&'static str> {
        Some(match self {
            DataType::PiHeader => "pi-header",
            DataType::VoiceLcHeader => "voice-lc-header",
            DataType::TerminatorWithLc => "terminator-with-lc",
            DataType::Csbk => "csbk",
            DataType::DataHeader => "data-header",
            DataType::Idle => "idle",
            DataType::Unnamed(_) => return None,
        })
    }
}

/// The 20 slot-type bits of `colour_code` and `data_type`, Golay(20,8) coded.
pub fn slot_type_encode(colour_code: u8, data_type: DataType) -> [u8; SLOT_TYPE_BITS] {
    let data = ((colour_code & 0x0F) << 4) | data_type.code();
    let cw = fec::golay_20_8_encode(data);
    let mut out = [0u8; SLOT_TYPE_BITS];
    for (i, b) in out.iter_mut().enumerate() {
        *b = (cw >> (SLOT_TYPE_BITS - 1 - i) & 1) as u8;
    }
    out
}

/// Decodes a burst's 20 slot-type bits. `None` when the Golay code refuses them.
pub fn slot_type_decode(bits: &[u8]) -> Option<SlotType> {
    if bits.len() != SLOT_TYPE_BITS {
        return None;
    }
    let cw = bits.iter().fold(0u32, |a, &b| (a << 1) | u32::from(b & 1));
    let (data, corrected) = fec::golay_20_8_decode(cw)?;
    Some(SlotType {
        colour_code: data >> 4,
        data_type: DataType::from_code(data),
        corrected,
    })
}

/// Where each CACH bit sits on the air: deinterleaved bit `a` is transmitted bit
/// `CACH_INTERLEAVE[a]`, the first seven being the TACT bits.
pub const CACH_INTERLEAVE: [usize; CACH_BITS] = [
    0, 4, 8, 12, 14, 18, 22, 1, 2, 3, 5, 6, 7, 9, 10, 11, 13, 15, 16, 17, 19, 20, 21, 23,
];

/// What the CACH of a downlink burst says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cach {
    /// Access type: whether the *next* slot is free for a mobile to use.
    pub access_type: u8,
    /// TDMA channel — the slot number this burst belongs to, 0 or 1.
    pub tdma_channel: u8,
    /// Link-control start/stop, which says where in a short-LC sequence this burst sits.
    pub lcss: u8,
    /// Whether the Hamming(7,4,3) code corrected a bit.
    pub corrected: bool,
    /// The 17 short-LC payload bits, carried verbatim. Nothing here decides anything from them:
    /// a short LC spans four CACHes and its own checksum is not assembled by this build.
    pub short_lc: [u8; 17],
}

/// Decodes a burst's 24 CACH bits. `None` when the TACT's Hamming code refuses them.
pub fn cach_decode(bits: &[u8]) -> Option<Cach> {
    if bits.len() != CACH_BITS {
        return None;
    }
    let mut de = [0u8; CACH_BITS];
    for (a, d) in de.iter_mut().enumerate() {
        *d = bits[CACH_INTERLEAVE[a]] & 1;
    }
    let mut tact = [0u8; 7];
    tact.copy_from_slice(&de[..7]);
    let fix = fec::hamming_correct(&mut tact, 4, &fec::H7_4_EQS);
    if fix == fec::HammingFix::Uncorrectable {
        return None;
    }
    let mut short_lc = [0u8; 17];
    short_lc.copy_from_slice(&de[7..]);
    Some(Cach {
        access_type: tact[0],
        tdma_channel: tact[1],
        lcss: (tact[2] << 1) | tact[3],
        corrected: fix != fec::HammingFix::Clean,
        short_lc,
    })
}

/// The 24 on-air CACH bits for `access_type`, `tdma_channel` and `lcss`, with `short_lc` carried
/// through — the exact inverse of [`cach_decode`].
pub fn cach_encode(
    access_type: u8,
    tdma_channel: u8,
    lcss: u8,
    short_lc: &[u8; 17],
) -> [u8; CACH_BITS] {
    let data = [access_type & 1, tdma_channel & 1, (lcss >> 1) & 1, lcss & 1];
    let mut tact = [0u8; 7];
    fec::hamming_encode(&data, &fec::H7_4_EQS, &mut tact);
    let mut de = [0u8; CACH_BITS];
    de[..7].copy_from_slice(&tact);
    de[7..].copy_from_slice(short_lc);
    let mut out = [0u8; CACH_BITS];
    for (a, d) in de.iter().enumerate() {
        out[CACH_INTERLEAVE[a]] = *d;
    }
    out
}

/// The three-byte RS(12,9) masks that name which header a full LC came from.
///
/// The CSBK's 16-bit mask `0xA5A5` is T-271's independently verified constant, and these are the
/// same table's neighbouring entries. Recorded as recalled; nothing is decided by *which* mask
/// matched, because the data type already said, and a mask that is wrong only costs the header.
pub const VOICE_LC_HEADER_MASK: [u8; 3] = [0x96, 0x96, 0x96];
/// The terminator-with-LC mask.
pub const TERMINATOR_LC_MASK: [u8; 3] = [0x99, 0x99, 0x99];
/// The 16-bit CRC mask of a packet-data header.
pub const DATA_HEADER_CRC_MASK: u16 = 0xCCCC;

/// A full link control: who is talking to whom, as the call itself states it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FullLc {
    /// Protect flag, carried verbatim.
    pub protect: bool,
    /// Full link control opcode: 0 = group voice, 3 = unit to unit.
    pub flco: u8,
    /// Feature-set id (0 = standard).
    pub fid: u8,
    /// Service options, carried verbatim; **no encryption state is read from them**, for the
    /// reason T-270 gives: a *clear* claim needs a corroborated bit, and this build has none.
    pub service_options: u8,
    /// Destination — the talkgroup for a group call, the unit for a private one.
    pub destination: u32,
    /// Source unit address.
    pub source: u32,
}

/// Parses the 12 bytes a voice-LC header or terminator BPTC carried, with the RS(12,9) parity
/// checked after `mask` is removed. `None` when the parity does not check.
///
/// The 72-bit layout is pinned by arithmetic: PF(1) + R(1) + FLCO(6) + FID(8) + service
/// options(8) + destination(24) + source(24) = 72, which is exactly the 9 information bytes the
/// RS(12,9) protects.
pub fn full_lc_parse(payload: &[u8; BPTC_PAYLOAD_BYTES], mask: [u8; 3]) -> Option<FullLc> {
    let mut word = *payload;
    for (i, m) in mask.iter().enumerate() {
        word[9 + i] ^= m;
    }
    if !fec::rs_12_9_check(&word) {
        return None;
    }
    Some(FullLc {
        protect: word[0] & 0x80 != 0,
        flco: word[0] & 0x3F,
        fid: word[1],
        service_options: word[2],
        destination: (u32::from(word[3]) << 16) | (u32::from(word[4]) << 8) | u32::from(word[5]),
        source: (u32::from(word[6]) << 16) | (u32::from(word[7]) << 8) | u32::from(word[8]),
    })
}

/// The 12 bytes of a full-LC header block: the 9 LC bytes, their RS(12,9) parity, and `mask`.
pub fn full_lc_encode(lc: &FullLc, mask: [u8; 3]) -> [u8; BPTC_PAYLOAD_BYTES] {
    let data = [
        (u8::from(lc.protect) << 7) | (lc.flco & 0x3F),
        lc.fid,
        lc.service_options,
        (lc.destination >> 16) as u8,
        (lc.destination >> 8) as u8,
        lc.destination as u8,
        (lc.source >> 16) as u8,
        (lc.source >> 8) as u8,
        lc.source as u8,
    ];
    let parity = fec::rs_12_9_parity(&data);
    let mut out = [0u8; BPTC_PAYLOAD_BYTES];
    out[..9].copy_from_slice(&data);
    for i in 0..3 {
        out[9 + i] = parity[i] ^ mask[i];
    }
    out
}

/// The header of a packet-data transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataHeader {
    /// Group (true) or individual (false) addressing.
    pub group: bool,
    /// The sender asked for a response.
    pub response_requested: bool,
    /// Data packet format.
    pub dpf: u8,
    /// Service access point.
    pub sap: u8,
    /// Destination address.
    pub destination: u32,
    /// Source address.
    pub source: u32,
    /// Data blocks that follow this header.
    pub blocks_to_follow: u8,
}

/// The data header's CRC-CCITT, masked with [`DATA_HEADER_CRC_MASK`].
///
/// The **initial value is this repo's choice** (0x0000), exactly as [`crate::trunk::dmr`] records
/// for the CSBK: it could not be corroborated, it is invisible to the gate's false-alarm
/// arithmetic (2⁻¹⁶ for random data either way), and no field is read from the CRC. The
/// consequence is stated plainly: a **real** off-air data header whose standard initial value
/// differs will fail this check and be counted, never published as a guess.
fn data_header_crc() -> BitCrc {
    BitCrc::new(
        16,
        0x1021,
        0x0000,
        false,
        false,
        u32::from(DATA_HEADER_CRC_MASK),
    )
    .expect("CRC-CCITT with the DMR data-header mask is a valid BitCrc")
}

/// Whether a data header's stored CRC matches the one its first ten bytes imply.
pub fn data_header_crc_ok(payload: &[u8; BPTC_PAYLOAD_BYTES]) -> bool {
    static CRC: std::sync::OnceLock<BitCrc> = std::sync::OnceLock::new();
    let want = (u16::from(payload[10]) << 8) | u16::from(payload[11]);
    CRC.get_or_init(data_header_crc)
        .compute(&payload[..10], 0, 80) as u16
        == want
}

/// Parses the 12 bytes a data-header BPTC carried. `None` when the CRC does not check.
pub fn data_header_parse(payload: &[u8; BPTC_PAYLOAD_BYTES]) -> Option<DataHeader> {
    if !data_header_crc_ok(payload) {
        return None;
    }
    Some(DataHeader {
        group: payload[0] & 0x80 != 0,
        response_requested: payload[0] & 0x40 != 0,
        dpf: payload[0] & 0x0F,
        sap: payload[1] >> 4,
        destination: (u32::from(payload[2]) << 16)
            | (u32::from(payload[3]) << 8)
            | u32::from(payload[4]),
        source: (u32::from(payload[5]) << 16)
            | (u32::from(payload[6]) << 8)
            | u32::from(payload[7]),
        blocks_to_follow: payload[8] & 0x7F,
    })
}

/// The 12 bytes of a data header, CRC included.
pub fn data_header_encode(h: &DataHeader) -> [u8; BPTC_PAYLOAD_BYTES] {
    let mut out = [0u8; BPTC_PAYLOAD_BYTES];
    out[0] = (u8::from(h.group) << 7) | (u8::from(h.response_requested) << 6) | (h.dpf & 0x0F);
    out[1] = (h.sap & 0x0F) << 4;
    out[2] = (h.destination >> 16) as u8;
    out[3] = (h.destination >> 8) as u8;
    out[4] = h.destination as u8;
    out[5] = (h.source >> 16) as u8;
    out[6] = (h.source >> 8) as u8;
    out[7] = h.source as u8;
    out[8] = h.blocks_to_follow & 0x7F;
    let crc = data_header_crc().compute(&out[..10], 0, 80) as u16;
    out[10] = (crc >> 8) as u8;
    out[11] = crc as u8;
    out
}

/// A header a burst carried, once its own check passed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Header {
    /// The link control of a call that is starting.
    VoiceLc(FullLc),
    /// The link control of a call that has ended.
    TerminatorLc(FullLc),
    /// A packet-data header.
    Data(DataHeader),
    /// A control signalling block, parsed by [`crate::trunk::dmr::Csbk`] after its own CRC
    /// checked — the Tier III payload, reached here through *real* DMR framing.
    Csbk(crate::trunk::dmr::Csbk),
    /// A privacy-indicator header: the call that follows is encrypted. Its key material is not
    /// read, and no *clear* claim is ever made from its absence.
    PrivacyIndicator,
}

impl Header {
    /// A short name for a row a person reads.
    pub fn name(&self) -> &'static str {
        match self {
            Header::VoiceLc(_) => "voice-lc-header",
            Header::TerminatorLc(_) => "terminator-with-lc",
            Header::Data(_) => "data-header",
            Header::Csbk(_) => "csbk",
            Header::PrivacyIndicator => "pi-header",
        }
    }
}

/// One burst framed from a sync hit.
#[derive(Clone, Copy, Debug)]
pub struct Tier2Burst {
    /// Index of the burst's first dibit.
    pub dibit: usize,
    /// Index into [`DMR_SYNCS`] of the sync that framed it.
    pub sync: usize,
    /// Dibits the sync match had wrong.
    pub sync_errors: usize,
    /// The CACH before it, where the window held one and its TACT checked.
    pub cach: Option<Cach>,
    /// The slot type, for a data burst whose Golay code resolved.
    pub slot_type: Option<SlotType>,
    /// What the BPTC cost, where one was decoded.
    pub bptc: Option<BptcStats>,
    /// The header it carried, where the header's own check passed.
    pub header: Option<Header>,
}

impl Tier2Burst {
    /// The sync word that framed it.
    pub fn word(&self) -> &'static DmrSync {
        &DMR_SYNCS[self.sync]
    }
}

/// What a scan of one emission's dibits found.
#[derive(Clone, Debug, Default)]
pub struct Tier2Scan {
    /// Dibits scanned.
    pub dibits: usize,
    /// Sync hits, in order.
    pub hits: Vec<SyncHit>,
    /// Bursts framed from them.
    pub bursts: Vec<Tier2Burst>,
    /// Hits whose offset from the first hit is a whole number of 30 ms slots. Evidence, not a
    /// gate: a symbol slip costs this count without being evidence against DMR.
    pub grid_consistent: usize,
    /// The colour code, once [`MIN_CC_AGREEMENTS`] bursts agreed on one.
    pub colour_code: Option<u8>,
    /// Bursts that decoded [`Tier2Scan::colour_code`].
    pub colour_code_agreements: usize,
    /// Bursts that decoded a *different* colour code — a second repeater in the channel, or a
    /// slot type this build mis-decodes. Reported either way.
    pub colour_code_disagreements: usize,
    /// Slots (30 ms) the scanned window spans: the denominator of `sync n/m`.
    pub slots: usize,
    /// Data bursts whose BPTC could not be resolved.
    pub bptc_failed: usize,
    /// Blocks that resolved but whose CRC or RS parity refused them.
    pub check_failed: usize,
}

impl Tier2Scan {
    /// Whether this is DMR: [`MIN_TIER2_SYNCS`] sync hits, and nothing else.
    pub fn identified(&self) -> bool {
        self.hits.len() >= MIN_TIER2_SYNCS
    }

    /// `(syncs found, slots the window spans)` — the `n/m` of the verdict.
    pub fn sync_counts(&self) -> (usize, usize) {
        (self.hits.len(), self.slots)
    }

    /// The headers whose own check passed, in order.
    pub fn headers(&self) -> impl Iterator<Item = &Header> {
        self.bursts.iter().filter_map(|b| b.header.as_ref())
    }

    /// The row's verdict, e.g. `DMR Tier II, CC 1, sync 8/9`. `None` when this is not DMR.
    ///
    /// A colour code that no second burst agreed with reads `CC unknown`: the number is missing,
    /// and saying so is the point.
    pub fn verdict(&self) -> Option<String> {
        if !self.identified() {
            return None;
        }
        let (n, m) = self.sync_counts();
        let cc = match self.colour_code {
            Some(c) => c.to_string(),
            None => "unknown".to_string(),
        };
        Some(format!("DMR Tier II, CC {cc}, sync {n}/{m}"))
    }

    /// The sync words seen, with how many times each, for a row that wants to say *which*.
    pub fn sync_names(&self) -> Vec<(&'static str, usize)> {
        let mut out: Vec<(&'static str, usize)> = Vec::new();
        for h in &self.hits {
            let name = h.word().name;
            match out.iter_mut().find(|(n, _)| *n == name) {
                Some((_, c)) => *c += 1,
                None => out.push((name, 1)),
            }
        }
        out
    }
}

/// Reads `n` bits from `dibits[at..]`, MSB first (two bits per dibit).
fn bits_from(dibits: &[u8], at: usize, n_dibits: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n_dibits * 2);
    for d in &dibits[at..at + n_dibits] {
        out.push(d >> 1 & 1);
        out.push(d & 1);
    }
    out
}

/// Scans a demodulated dibit stream for conventional DMR.
///
/// The stream is whatever a 4FSK demodulator at 4800 Bd produced over one emission's region — no
/// band, no frequency and no prior is consulted, so the same scan identifies DMR wherever it is
/// found. Cost is [`find_syncs`] plus, per hit, one 196-bit BPTC (a few hundred XORs) — bounded
/// at [`MAX_BURSTS_PER_SCAN`] bursts.
pub fn scan(dibits: &[u8]) -> Tier2Scan {
    let mut out = Tier2Scan {
        dibits: dibits.len(),
        slots: dibits.len() / SLOT_GRID_DIBITS,
        ..Default::default()
    };
    out.hits = find_syncs(dibits, SYNC_TOLERANCE_DIBITS);
    if let Some(first) = out.hits.first() {
        out.grid_consistent = out
            .hits
            .iter()
            .filter(|h| (h.dibit - first.dibit) % SLOT_GRID_DIBITS == 0)
            .count();
    }
    let mut cc_counts = [0usize; 16];
    for hit in out.hits.iter().take(MAX_BURSTS_PER_SCAN) {
        // The burst's first dibit: 98 information bits and 10 slot-type bits precede the sync.
        let Some(start) = hit.dibit.checked_sub(INFO_DIBITS + SLOT_TYPE_HALF_DIBITS) else {
            continue;
        };
        if start + BURST_DIBITS > dibits.len() {
            continue;
        }
        let mut burst = Tier2Burst {
            dibit: start,
            sync: hit.sync,
            sync_errors: hit.errors,
            cach: None,
            slot_type: None,
            bptc: None,
            header: None,
        };
        if let Some(cach_at) = start.checked_sub(CACH_DIBITS) {
            burst.cach = cach_decode(&bits_from(dibits, cach_at, CACH_DIBITS));
        }
        if DMR_SYNCS[hit.sync].payload == SyncPayload::Data {
            let mut st = bits_from(dibits, start + INFO_DIBITS, SLOT_TYPE_HALF_DIBITS);
            st.extend(bits_from(
                dibits,
                hit.dibit + SYNC_DIBITS,
                SLOT_TYPE_HALF_DIBITS,
            ));
            burst.slot_type = slot_type_decode(&st);
            if let Some(s) = burst.slot_type {
                cc_counts[usize::from(s.colour_code & 0x0F)] += 1;
            }
            let mut info = bits_from(dibits, start, INFO_DIBITS);
            info.extend(bits_from(
                dibits,
                hit.dibit + SYNC_DIBITS + SLOT_TYPE_HALF_DIBITS,
                INFO_DIBITS,
            ));
            match fec::bptc_196_96_decode(&info) {
                Some((payload, stats)) => {
                    burst.bptc = Some(stats);
                    burst.header = read_header(&payload, burst.slot_type.map(|s| s.data_type));
                    if burst.header.is_none()
                        && matches!(
                            burst.slot_type.map(|s| s.data_type),
                            Some(
                                DataType::VoiceLcHeader
                                    | DataType::TerminatorWithLc
                                    | DataType::Csbk
                                    | DataType::DataHeader
                            )
                        )
                    {
                        out.check_failed += 1;
                    }
                }
                None => out.bptc_failed += 1,
            }
        }
        out.bursts.push(burst);
    }
    let (best, count) = cc_counts
        .iter()
        .enumerate()
        .max_by_key(|&(_, c)| *c)
        .map(|(i, &c)| (i as u8, c))
        .unwrap_or((0, 0));
    if count >= MIN_CC_AGREEMENTS {
        out.colour_code = Some(best);
        out.colour_code_agreements = count;
    }
    out.colour_code_disagreements = cc_counts.iter().sum::<usize>() - count;
    out
}

/// The header a resolved BPTC payload carries, for the data type its slot type named. `None` when
/// the header's own check refused it, or when the data type carries no header this build reads.
fn read_header(payload: &[u8; BPTC_PAYLOAD_BYTES], data_type: Option<DataType>) -> Option<Header> {
    match data_type? {
        DataType::VoiceLcHeader => {
            full_lc_parse(payload, VOICE_LC_HEADER_MASK).map(Header::VoiceLc)
        }
        DataType::TerminatorWithLc => {
            full_lc_parse(payload, TERMINATOR_LC_MASK).map(Header::TerminatorLc)
        }
        DataType::DataHeader => data_header_parse(payload).map(Header::Data),
        DataType::Csbk => crate::trunk::dmr::csbk_crc_ok(payload)
            .then(|| crate::trunk::dmr::Csbk::parse(payload))
            .flatten()
            .map(Header::Csbk),
        // A PI header's payload is key material and an initialisation vector. Its presence is the
        // finding; nothing inside it is read.
        DataType::PiHeader => Some(Header::PrivacyIndicator),
        DataType::Idle | DataType::Unnamed(_) => None,
    }
}

pub mod encode;

#[cfg(test)]
mod tests;
