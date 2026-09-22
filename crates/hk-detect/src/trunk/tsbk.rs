//! P25 Phase 1 TSBK decode and the IDEN channel map (C23, T-268).
//!
//! T-267 proved *that* a control channel is there (frame sync **and** CRC-valid blocks) and
//! deliberately stopped: [`super::confirm::ConfirmedCc::protocol`] reports
//! [`TrunkProtocol::Unknown`]. This module reads the blocks that confirmation already validated
//! and says *what* the system is — and, more usefully, turns the 16-bit channel numbers a grant
//! carries into frequencies.
//!
//! # Build, not wrap
//!
//! C23's open question was "native CC decoders on C11 outputs, or Trunk Recorder/OP25/SDRTrunk
//! subprocesses fed channel IQ?" This is the native answer, and the reason is the fixture, not a
//! preference for writing code:
//!
//! - A wrapped decoder needs a **standards-compliant** P25 control channel to sync on: rate-1/2
//!   trellis coding, block interleaving, status symbols, and the augmented CRC below. The repo has
//!   no such recording, and building an encoder good enough to drive a third-party decoder is
//!   strictly more work than this decoder — with the third-party binary then a hard dependency of
//!   a CI job that must stay green with no hardware and no GPL toolchain installed. The blind
//!   acceptance test would skip in CI, which defeats the point of having one.
//! - The native path composes with what already exists: confirmation hands over dibits, and what
//!   follows is field extraction from a 96-bit block. No DSP is in the loop here, so the decode
//!   can be exercised over millions of synthetic blocks in a unit test.
//!
//! Wrapping keeps its proper role, which is **oracle, not runtime**: C23 §Testing asks for a
//! comparison against Trunk Recorder/SDRTrunk on the same IQ, and that is worth doing behind the
//! C22 process boundary (ADR-0010 keeps GPLv3 stacks out of the core) the day a real off-air
//! capture exists. It is not a dependency of decoding.
//!
//! # No independent oracle has confirmed this decoder (T-299)
//!
//! **Say this plainly, because the fact it guards against is easy to miss:** this decoder and its
//! own test fixtures were written from the same reading of the same references, by the same
//! author, in the same task family. A shared misreading of the spec — a wrong bit position, a
//! wrong unit, a wrong opcode — would pass both the decoder and the synthetic scene that exercises
//! it, because nothing here checks this module's output against a second, independently-written
//! implementation on the *same* IQ. The "verified" markers throughout this file are real (each
//! checks one field against a named third-party source), but that is verification of individual
//! facts, not an end-to-end oracle comparison of decoded output.
//!
//! §5.3 of `docs/19-bart-800mhz-trunked.md` lays out that oracle: run Trunk Recorder, OP25 or
//! SDRTrunk offline over the same captured IQ this decoder sees, behind the C22 plugin boundary,
//! gated on the hardware/off-air test tier so CI never depends on a GPLv3 binary being present —
//! cross-validated against a spectrogram-derived grant/activity check so no single third-party
//! decoder gets to author the answer key by itself. Building that harness is blocked on a real
//! off-air P25 capture existing to feed it both implementations. T-544 went looking for one in the
//! BART 800 MHz system and came back negative (`docs/19` §7, 2026-09-20): no BART signal was
//! receivable at that location, and no trunked control channel was found anywhere in the
//! 850.9–862.1 MHz downlink band it surveyed. **This ticket (T-299) remains blocked on that
//! capture; nothing in this module changes until one exists.**
//!
//! # What is verified, and what is not
//!
//! Every number below was checked against an independent reference before it was written, because
//! a protocol claim taken from memory is exactly the kind of thing that invalidates its own test:
//!
//! - **TSBK is 12 bytes**: 1-bit last-block flag, 1-bit protected flag, 6-bit opcode, 8-bit MFID,
//!   8 argument bytes, 16-bit CRC. *Verified.*
//! - **Opcodes** `0x00` GRP_VCH_GRANT, `0x02` GRP_VCH_GRANT_UPDATE, `0x3D` IDEN_UP. *Verified.*
//! - **A grant carries no frequency** — only a 4-bit identifier and a 12-bit channel number that
//!   the band plan resolves. *Verified*, and it is the whole reason this module exists.
//! - **Base frequency is in units of 5 Hz** and **channel spacing in units of 125 Hz**. *Verified
//!   by arithmetic on a published worked example*: a base field of `0x09157562` is 152 401 250,
//!   and ×5 Hz gives 762 006 250 Hz, the frequency that example reports; channel 1554 at spacing
//!   100 (×125 Hz) then lands on 762 006 250 + 1554 × 50 × 125 = 771 718 750 Hz, which is the
//!   frequency that example resolves. Both units reproduce a third party's numbers exactly.
//! - **UNVERIFIED — transmit offset.** The sign-plus-8-bit-magnitude encoding in units of 250 kHz
//!   below is from the same family of references but no worked example was found to confirm it,
//!   and one reference notes VHF/UHF bands follow a different rule. It is therefore decoded and
//!   recorded, and **nothing in the mapping path uses it**: [`ChannelMap::resolve`] maps downlinks
//!   from base and spacing only. An uplink claim would need this field verified first.
//! - **UNVERIFIED — bandwidth units.** Same treatment: recorded, never used to decide anything.
//!
//! # The CRC this module does *not* change, and a finding for whoever wraps later
//!
//! Real P25 protects a TSBK with the **augmented** CRC-CCITT (init 0, final XOR 0xFFFF) over all
//! 12 bytes — *not* CRC-16/CCITT-FALSE, which many references repeat and which makes essentially
//! every real TSBK fail. T-267's confirmer uses CCITT-FALSE, and this module reads the blocks that
//! confirmer validated, so it inherits that choice rather than splitting the workspace's framing in
//! two. That is honest only because the framing is already simplified in three other ways (no
//! trellis code, no interleaving, no status symbols), so switching one of the four would buy no
//! standards compliance. **Anyone feeding this a real off-air capture must fix all four**, and the
//! CRC variant is the one that will silently look like a demodulator fault instead of a codec bug.
//!
//! T-300 turned that paragraph into assertions rather than acting on it: the tests at the end of
//! [`super::confirm`] pin all four simplifications — the CRC variant, the missing trellis code, the
//! missing deinterleaver and the unstripped status symbols — so none of them can be inherited
//! silently. They also pin the part that misleads: a correctly-formed real TSBK fails this build's
//! check with *certainty*, at the fixed residue `0x99F6`, while the trellis reports a clean metric.
//!
//! # Metadata only
//!
//! Nothing here touches audio, voice frames or message payloads, and **nothing here decrypts
//! anything**. Reading an algorithm *identifier* is not decryption: no key schedule, no keystream,
//! no cipher of any kind appears in this crate, and none may.
//!
//! # Encryption (T-270): what may say "clear", and what may only say "encrypted"
//!
//! T-266 made the mistake unconstructible — [`hk_model::Encryption`] has no `Default`, its two
//! claiming variants each require evidence, and `Unknown` carries none. This module is what finally
//! *reads* an encryption indication, and it does so asymmetrically, which is the whole design:
//!
//! - **A service-options bit may only ever raise to `Encrypted`.** `P = 1` becomes
//!   [`hk_model::Encryption::Encrypted`]; `P = 0` becomes **`Unknown`**, not `Clear`. Two reasons,
//!   and either alone would be enough. First, the bit is a *grant-time announcement about a call
//!   that has not started*; the authoritative per-call statement is the ALGID in the call's own
//!   header, and a call granted clear can be keyed encrypted. Second, it makes the failure mode of
//!   a mis-read bit position harmless: a false `Encrypted` costs audio nobody hears, while a false
//!   `Clear` opens a voice path on protected traffic. The error that is merely annoying is the one
//!   this module is allowed to make.
//! - **Only a verified ALGID may say `clear`.** [`algid_encryption`] is the single path in the
//!   workspace that can produce [`hk_model::Encryption::Clear`] from a decode, and only for the one
//!   octet that means it ([`hk_model::P25_ALGID_CLEAR`]).
//!
//! So `clear` is reachable by evidence and by nothing else, and every path that lacks evidence —
//! a grant update, a P = 0 grant, a block that never parsed, a call joined in progress — stays
//! `Unknown`.
//!
//! # What is verified here, and by whom
//!
//! - **Service options is argument byte 0 of a grant.** *Verified* against op25's own TSBK
//!   handling, which extracts it from the same place: `opts = (tsbk >> 72) & 0xff` over a 96-bit
//!   block is the byte immediately after the 8-bit header and 8-bit MFID — exactly the first
//!   argument byte this module reads.
//! - **The encryption bit is `0x40`.** *Verified* three independent ways: SDRTrunk's
//!   `ServiceOptions.java` names it `ENCRYPTION_FLAG = 0x40` behind `isEncrypted()`; dsd-fme
//!   documents the same test as `svc & 0x40`; and a TIA-102.AABC-B-referenced field description
//!   gives bit 6 as "protected". Three sources, one mask.
//! - **ALGID values.** *Verified*: SDRTrunk's `Encryption.java` enumerates `0x80` UNENCRYPTED,
//!   `0x81` DES_OFB, `0x84` AES_256, `0xAA` MOTOROLA_ADP; a published ALGID table agrees; op25
//!   tests encryption as `algid != 0x80`; and docs/04 §8.3 states the same four. Four sources.
//! - **A grant update carries no service options.** *Verified* from op25's source: the standard
//!   opcode `0x02` argument field is `ch1/ga1/ch2/ga2`, four 16-bit fields filling all 64 bits,
//!   with no service-options octet and no source address. This is why late entry is `Unknown` by
//!   *protocol* rather than by convention — there is no bit to read.
//! - **UNVERIFIED — every other service-options bit.** Emergency (`0x80`), duplex (`0x20`), mode
//!   (`0x10`) and priority (`0x07`) are corroborated less well or not at all, and M4 has nowhere to
//!   put them. They are therefore **not decoded into anything**: the raw octet is recorded verbatim
//!   so a later task can revisit it, and nothing maps through it. T-268's discipline.
//! - **UNVERIFIED — the `0x02` argument layout as this decoder reads it.** T-268 decodes a grant
//!   update with the *grant* layout, which the op25 finding above shows is not the standard one.
//!   Changing it is T-268's simplification to revisit (alongside the trellis, interleaving, status
//!   symbols and the augmented CRC), not this task's; what matters here is that no encryption claim
//!   is made from it, and none is.

use hk_model::{
    ChannelPlanEntry, Encryption, EncryptionEvidence, P25_ALGID_CLEAR, Timestamp, TrunkProtocol,
};

// ---------------------------------------------------------------------------------------------
// Opcodes (verified)
// ---------------------------------------------------------------------------------------------

/// Group voice channel grant.
pub const OP_GRP_VCH_GRANT: u8 = 0x00;
/// Group voice channel grant update (also how a late-entry follower learns a call).
pub const OP_GRP_VCH_GRANT_UPDATE: u8 = 0x02;
/// Identifier update: the band-plan entry a channel number is resolved through.
pub const OP_IDEN_UP: u8 = 0x3D;

/// Bytes in a TSBK, including its CRC.
pub const TSBK_BYTES: usize = 12;

// ---------------------------------------------------------------------------------------------
// A-priori thresholds.
//
// Fixed, with the arithmetic below, BEFORE the decoder was run on anything. Never edit one of
// these to make a run pass (ADR-0016 §7; the same rule confirm.rs states for its own floors).
// ---------------------------------------------------------------------------------------------

/// IDEN_UP messages that must **agree bit for bit** before an identifier enters the channel map.
///
/// A priori: a CRC-valid block whose opcode happens to be IDEN_UP is not rare enough to trust on
/// its own. Random data clears a 16-bit CRC with probability 2^-16 = 1.5e-5 and carries this
/// opcode with probability 2^-6, so a single spurious "identifier update" arrives at 2.4e-7 per
/// block — over a long run, several wrong band-plan entries an hour. Requiring a second message
/// whose **64 argument bits are identical** collapses that to 2^-64 = 5.4e-20 per pair, because
/// two independent random argument fields must agree everywhere.
///
/// The cost to a real system is one extra announcement period before its table becomes usable.
/// That is deliberate: an entry admitted from a single unrepeated message is precisely how a
/// wrong frequency enters a band plan, which is C23's named pitfall.
pub const MIN_IDEN_AGREEMENTS: u32 = 2;

/// How old an admitted identifier may be and still map a channel, seconds.
///
/// A priori, and the direction of its error is the point. An identifier is **not** a global
/// constant: the same 4-bit value legitimately carries a different base frequency on another site
/// of the same system (700 vs 800 MHz sites, rebanded sites), so an old entry is not merely stale,
/// it may be *wrong for what we are listening to now*. Control channels re-announce IDEN_UP
/// periodically and far more often than this, so a system we are genuinely still hearing re-arms
/// its entries many times over inside the window and never trips it; only one we have stopped
/// hearing — a CC dropout, a retune, a different site — ages out.
///
/// 600 s is the smallest round window that spans many announcement periods. Widening it can only
/// admit a stale mapping; narrowing it can only produce an honest [`Unmapped::Stale`]. Neither
/// direction can invent a frequency, which is the property that matters: the failure mode this
/// threshold guards is "resolved to a plausible-looking wrong frequency", and a refusal is never
/// that.
pub const IDEN_MAX_AGE_S: f64 = 600.0;

/// Most TSBKs decoded from one window, so the cost of a pass stays bounded a priori.
///
/// At 4800 Bd a 96-bit block plus its 48-bit sync is 72 symbols, so a second of control channel
/// holds ~66 frames; 4096 is two orders of magnitude of headroom over any window the chain
/// collects, and exists so a pathological stream cannot turn one pass into unbounded work.
pub const MAX_TSBK_PER_WINDOW: usize = 4096;

// ---------------------------------------------------------------------------------------------
// The block
// ---------------------------------------------------------------------------------------------

/// One decoded trunking signalling block.
///
/// Constructing one asserts only that 12 bytes parsed — **not** that they were a real message.
/// The CRC that makes a block worth parsing is checked upstream, by the confirmer that produced
/// it, and the agreement gate below is what stops a lucky block from changing a band plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tsbk {
    /// Last block of a TSDU.
    pub last_block: bool,
    /// Protected (encrypted signalling) flag. Recorded, never acted on here.
    pub protected: bool,
    /// Message type, 6 bits.
    pub opcode: u8,
    /// Manufacturer id (0x00 = standard).
    pub mfid: u8,
    /// The 64 argument bits.
    pub args: [u8; 8],
}

impl Tsbk {
    /// Parses the 12-byte block. The trailing two CRC bytes are not re-checked here.
    pub fn parse(block: &[u8]) -> Option<Self> {
        if block.len() < TSBK_BYTES {
            return None;
        }
        let mut args = [0u8; 8];
        args.copy_from_slice(&block[2..10]);
        Some(Self {
            last_block: block[0] & 0x80 != 0,
            protected: block[0] & 0x40 != 0,
            opcode: block[0] & 0x3F,
            mfid: block[1],
            args,
        })
    }

    /// Reads `n` bits from the argument field starting at bit `at` (MSB-first).
    fn bits(&self, at: u32, n: u32) -> u64 {
        debug_assert!(n <= 64 && at + n <= 64);
        let mut v = 0u64;
        for i in 0..n {
            let bit = at + i;
            let byte = self.args[(bit / 8) as usize];
            v = (v << 1) | u64::from((byte >> (7 - bit % 8)) & 1);
        }
        v
    }

    /// The identifier update this block carries, if it is one.
    ///
    /// Layout of the 64 argument bits, which sum to exactly 64: identifier (4), bandwidth (9),
    /// transmit offset (9: sign + 8 magnitude), channel spacing (10), base frequency (32).
    pub fn iden_up(&self) -> Option<IdenUp> {
        if self.opcode != OP_IDEN_UP || self.mfid != 0 {
            return None;
        }
        let iden = self.bits(0, 4) as u8;
        let bandwidth = self.bits(4, 9);
        let offset_sign = self.bits(13, 1);
        let offset_mag = self.bits(14, 8);
        let spacing = self.bits(22, 10);
        let base = self.bits(32, 32);
        // Verified units: base ×5 Hz, spacing ×125 Hz. Unverified and unused for mapping:
        // bandwidth ×125 Hz, transmit offset ×250 kHz with 1 = positive.
        let sign = if offset_sign == 1 { 1.0 } else { -1.0 };
        Some(IdenUp {
            iden,
            base_hz: base as f64 * 5.0,
            spacing_hz: spacing as f64 * 125.0,
            tx_offset_hz: sign * offset_mag as f64 * 250_000.0,
            bandwidth_hz: (bandwidth > 0).then_some(bandwidth as f64 * 125.0),
            args: self.args,
        })
    }

    /// The voice grant this block carries, if it is one.
    ///
    /// Layout of the 64 argument bits, which sum to exactly 64: service options (8), channel (16),
    /// group address (16), source address (24).
    ///
    /// The service-options octet is read **only for a plain grant**. A grant update carries no such
    /// field — its standard argument layout is four 16-bit channel/group fields (see the module
    /// docs) — so [`Grant::service_options`] is `None` for one, and the encryption state it can
    /// state is therefore `Unknown`. That is the late-entry case, and it is a fact about the
    /// protocol rather than a policy this decoder applies on top of it.
    pub fn grant(&self) -> Option<Grant> {
        let update = match self.opcode {
            OP_GRP_VCH_GRANT => false,
            OP_GRP_VCH_GRANT_UPDATE => true,
            _ => return None,
        };
        if self.mfid != 0 {
            return None;
        }
        Some(Grant {
            update,
            channel: self.bits(8, 16) as u16,
            talkgroup: self.bits(24, 16) as u16,
            source: self.bits(40, 24) as u32,
            // Read from the one opcode that carries it. A grant update has no service-options
            // octet at all, so there is nothing to read and nothing is claimed.
            service_options: (!update).then(|| ServiceOptions(self.bits(0, 8) as u8)),
        })
    }
}

/// An identifier update: one entry of the band plan.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IdenUp {
    /// The 4-bit table identifier a channel number refers to.
    pub iden: u8,
    /// Base frequency, Hz.
    pub base_hz: f64,
    /// Channel spacing, Hz.
    pub spacing_hz: f64,
    /// Uplink offset, Hz. **Unverified encoding**; recorded, never used to map a downlink.
    pub tx_offset_hz: f64,
    /// Channel bandwidth, Hz. **Unverified units**; recorded, never used to decide anything.
    pub bandwidth_hz: Option<f64>,
    /// The raw argument bits, so two announcements can be compared exactly.
    args: [u8; 8],
}

impl IdenUp {
    /// Whether two announcements agree bit for bit — the [`MIN_IDEN_AGREEMENTS`] test.
    pub fn agrees_with(&self, other: &Self) -> bool {
        self.args == other.args
    }

    /// The channel-plan entry this announcement states, decoded at `t`.
    pub fn entry(&self, t: Timestamp) -> ChannelPlanEntry {
        ChannelPlanEntry {
            iden: self.iden,
            base_hz: self.base_hz,
            spacing_hz: self.spacing_hz,
            tx_offset_hz: self.tx_offset_hz,
            bandwidth_hz: self.bandwidth_hz,
            t,
        }
    }

    /// Whether the announcement is usable at all: a band plan needs a real base and a real step.
    ///
    /// This is a validity check, not a plausibility filter — it rejects what cannot be a frequency
    /// (zero, negative, non-finite), and deliberately does not second-guess *where* a system says
    /// its band is. Guessing that would be the known-signal database overriding a measurement.
    pub fn is_usable(&self) -> bool {
        self.base_hz.is_finite()
            && self.base_hz > 0.0
            && self.spacing_hz.is_finite()
            && self.spacing_hz > 0.0
            && self.tx_offset_hz.is_finite()
    }
}

// ---------------------------------------------------------------------------------------------
// Encryption indications (T-270). Nothing here decrypts; these read identifiers and flags.
// ---------------------------------------------------------------------------------------------

/// The encryption ("protected") bit of a grant's service-options octet.
///
/// *Verified* three independent ways — SDRTrunk's `ENCRYPTION_FLAG = 0x40`, dsd-fme's `svc & 0x40`,
/// and a TIA-102.AABC-B-referenced description giving bit 6 as "protected". See the module docs.
pub const SVC_ENCRYPTED: u8 = 0x40;

/// A grant's service-options octet.
///
/// Only the encryption bit is interpreted. The rest of the octet is carried verbatim and mapped
/// through to nothing at all: emergency, duplex, mode and priority are either less well
/// corroborated or have nowhere to go in a metadata-only milestone, and T-268's rule is that an
/// uncorroborated field is recorded rather than acted on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServiceOptions(pub u8);

impl ServiceOptions {
    /// The octet as received, so a row can record what was actually on the air.
    pub const fn raw(self) -> u8 {
        self.0
    }

    /// Whether the **verified** encryption bit is set.
    pub const fn is_encrypted(self) -> bool {
        self.0 & SVC_ENCRYPTED != 0
    }

    /// What this octet is entitled to say about encryption.
    ///
    /// Set, it says `Encrypted`. Clear, it says **nothing** — see the module docs for why a
    /// grant-time "not protected" is not allowed to license audio.
    pub fn encryption(self) -> Encryption {
        if self.is_encrypted() {
            Encryption::from_service_options(true)
        } else {
            Encryption::Unknown
        }
    }
}

/// The four P25 algorithm identifiers this project names, all *verified* (see the module docs).
///
/// The table is deliberately **not** exhaustive and is not a gate: an identifier missing from it is
/// still an algorithm, and [`algid_encryption`] treats it as one. Naming is a courtesy for a row a
/// person will read, never the thing that decides whether traffic is protected.
pub const P25_ALGIDS: [(u8, &str); 4] = [
    (P25_ALGID_CLEAR, "clear"),
    (0x81, "DES-OFB"),
    (0x84, "AES-256"),
    (0xAA, "ADP"),
];

/// The name of a P25 ALGID, when it is one of the four this project names.
pub fn algid_name(algid: u8) -> Option<&'static str> {
    P25_ALGIDS
        .iter()
        .find(|&&(v, _)| v == algid)
        .map(|&(_, n)| n)
}

/// Reads a P25 ALGID octet, and an optional key id, into an encryption state.
///
/// **This is the only path in the workspace that can decode a `Clear` state**, and only for
/// [`P25_ALGID_CLEAR`]. Every other octet — named in [`P25_ALGIDS`] or not — is an algorithm, so an
/// unrecognised value comes back `Encrypted` carrying the byte rather than dropped or defaulted:
/// an algorithm nobody has a name for is still evidence of encryption, and is arguably the more
/// interesting find.
///
/// Reading an identifier is not decryption. Nothing here derives a key or touches a payload.
pub fn algid_encryption(algid: u8, key_id: Option<u16>) -> Encryption {
    match Encryption::from_algid(algid) {
        Encryption::Clear {
            evidence, algid, ..
        } => Encryption::Clear {
            evidence,
            algid,
            key_id,
        },
        Encryption::Encrypted {
            evidence, algid, ..
        } => Encryption::Encrypted {
            evidence,
            algid,
            key_id,
        },
        Encryption::Unknown => Encryption::Unknown,
    }
}

/// Whether an encryption state was decided by an ALGID octet — the authoritative per-call
/// statement, as opposed to a grant-time announcement.
pub fn is_algid_evidence(enc: Encryption) -> bool {
    enc.evidence() == Some(EncryptionEvidence::Algid)
}

/// A voice channel grant, carrying a channel number and no frequency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grant {
    /// True for GRP_VCH_GRANT_UPDATE, which is also how late entry joins a call in progress.
    pub update: bool,
    /// The 16-bit channel number: 4-bit identifier, then a 12-bit channel.
    pub channel: u16,
    /// Group (talkgroup) address.
    pub talkgroup: u16,
    /// Source (unit) address.
    pub source: u32,
    /// The service-options octet, when this opcode carries one. `None` for a grant update, which
    /// has no such field — the protocol reason late entry cannot state an encryption state.
    pub service_options: Option<ServiceOptions>,
}

impl Grant {
    /// The table identifier the channel number refers to (top 4 bits). *Verified split.*
    pub fn iden(&self) -> u8 {
        (self.channel >> 12) as u8
    }
    /// The channel within that table (low 12 bits). *Verified split.*
    pub fn channel_number(&self) -> u16 {
        self.channel & 0x0FFF
    }

    /// What this grant is entitled to say about encryption.
    ///
    /// `Encrypted` when the verified bit is set; **`Unknown` in every other case**, including a
    /// grant update (no field to read) and a plain grant with the bit clear. There is no branch
    /// here that reaches `Clear` — that needs an ALGID, and a grant does not carry one.
    pub fn encryption(&self) -> Encryption {
        self.service_options
            .map_or(Encryption::Unknown, ServiceOptions::encryption)
    }
}

// ---------------------------------------------------------------------------------------------
// The channel map
// ---------------------------------------------------------------------------------------------

/// Why a channel number could not be turned into a frequency.
///
/// Both variants exist so the refusal can say which it was; neither carries a frequency, because
/// the entire point is that **no frequency is produced when the table cannot be trusted**.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Unmapped {
    /// No IDEN_UP for this identifier has ever been admitted.
    NoIden,
    /// An identifier was admitted, but too long ago to trust. Carries its age in seconds and the
    /// limit it exceeded, so the event can record why.
    Stale {
        /// Age of the newest admitted entry at the time of the grant, s.
        age_s: f64,
        /// The limit it exceeded, s ([`IDEN_MAX_AGE_S`]).
        max_age_s: f64,
    },
}

impl Unmapped {
    /// A short machine reason for the `grant_event` detail.
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::NoIden => "no-iden",
            Self::Stale { .. } => "stale-iden",
        }
    }
}

/// A channel number resolved, or explicitly not.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Resolved {
    /// The downlink frequency, Hz, and the entry that produced it.
    Mapped {
        /// Downlink frequency, Hz.
        f_hz: f64,
        /// The identifier used.
        iden: u8,
        /// The channel within that table.
        channel_number: u16,
        /// When the entry used was decoded.
        decoded_at: Timestamp,
    },
    /// No frequency, and why.
    Unmapped(Unmapped),
}

/// One identifier's state: the announcement, how many agreed, and when it was admitted.
#[derive(Clone, Copy, Debug)]
struct Entry {
    iden_up: IdenUp,
    agreements: u32,
    /// When the announcement that **admitted** this entry was decoded.
    admitted_at: Timestamp,
    admitted: bool,
}

/// The band plan as decoded, and the only thing that turns a channel number into a frequency.
///
/// It refuses in two cases and **never** substitutes another identifier's parameters, which is
/// the C23 pitfall in one sentence: an identifier we have not heard, or heard too long ago, must
/// produce nothing, not a plausible-looking wrong frequency.
#[derive(Clone, Debug, Default)]
pub struct ChannelMap {
    entries: Vec<Entry>,
}

impl ChannelMap {
    /// An empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds in an announcement decoded at `t`.
    ///
    /// Returns the entry if **this** announcement admitted the identifier — the moment the
    /// agreement gate was met — so a caller can append it to the append-only channel table exactly
    /// once. A repeat of an already-admitted identifier returns `None` (nothing new was learned);
    /// an announcement that *disagrees* with the pending one replaces it and restarts the count,
    /// because two different claims are not corroboration.
    pub fn observe(&mut self, iden_up: &IdenUp, t: Timestamp) -> Option<ChannelPlanEntry> {
        if !iden_up.is_usable() {
            return None;
        }
        let Some(e) = self
            .entries
            .iter_mut()
            .find(|e| e.iden_up.iden == iden_up.iden)
        else {
            self.entries.push(Entry {
                iden_up: *iden_up,
                agreements: 1,
                admitted_at: t,
                admitted: false,
            });
            return Self::admit(self.entries.last_mut().expect("just pushed"), t);
        };
        if e.iden_up.agrees_with(iden_up) {
            e.agreements = e.agreements.saturating_add(1);
        } else {
            // A different claim for the same identifier. The system may have been rebanded, or we
            // may be hearing a different site: either way the old count is not evidence for the
            // new parameters, so it starts again rather than averaging two band plans together.
            e.iden_up = *iden_up;
            e.agreements = 1;
            e.admitted = false;
        }
        Self::admit(e, t)
    }

    /// Admits `e` if it has now been corroborated, returning the entry the first time only.
    fn admit(e: &mut Entry, t: Timestamp) -> Option<ChannelPlanEntry> {
        if e.admitted || e.agreements < MIN_IDEN_AGREEMENTS {
            return None;
        }
        e.admitted = true;
        e.admitted_at = t;
        Some(e.iden_up.entry(t))
    }

    /// Identifiers admitted so far.
    pub fn admitted(&self) -> usize {
        self.entries.iter().filter(|e| e.admitted).count()
    }

    /// The admitted announcement for `iden`, if there is one.
    pub fn iden(&self, iden: u8) -> Option<&IdenUp> {
        self.entries
            .iter()
            .find(|e| e.admitted && e.iden_up.iden == iden)
            .map(|e| &e.iden_up)
    }

    /// Resolves a grant's 16-bit channel number as of `now`, or refuses and says why.
    ///
    /// `f = base + spacing × channel` on the identifier the channel number names — *verified*
    /// against a published worked example (see the module docs). No other identifier is ever
    /// consulted, and an aged entry refuses rather than maps.
    pub fn resolve(&self, channel: u16, now: Timestamp) -> Resolved {
        let iden = (channel >> 12) as u8;
        let number = channel & 0x0FFF;
        let Some(e) = self
            .entries
            .iter()
            .find(|e| e.admitted && e.iden_up.iden == iden)
        else {
            return Resolved::Unmapped(Unmapped::NoIden);
        };
        let age_s = (now.as_unix_nanos() - e.admitted_at.as_unix_nanos()) as f64 / 1e9;
        if age_s > IDEN_MAX_AGE_S {
            return Resolved::Unmapped(Unmapped::Stale {
                age_s,
                max_age_s: IDEN_MAX_AGE_S,
            });
        }
        Resolved::Mapped {
            f_hz: e.iden_up.base_hz + e.iden_up.spacing_hz * f64::from(number),
            iden,
            channel_number: number,
            decoded_at: e.admitted_at,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Reading a window
// ---------------------------------------------------------------------------------------------

/// What one window of CRC-valid blocks contained.
#[derive(Clone, Debug, Default)]
pub struct TsbkScan {
    /// Blocks that parsed.
    pub blocks: usize,
    /// Identifier updates seen (before the agreement gate).
    pub iden_ups: Vec<IdenUp>,
    /// Grants seen, in order.
    pub grants: Vec<Grant>,
    /// Blocks whose opcode this decoder does not read.
    pub unhandled: usize,
}

/// Decodes CRC-valid blocks into identifier updates and grants.
///
/// Input is the blocks the confirmer already validated, so nothing here re-decides whether a
/// control channel is present: [`super::CcConfirmer::confirm`] remains the only source of a
/// [`super::ConfirmedCc`], and this function cannot produce one.
pub fn scan_blocks<'a>(blocks: impl IntoIterator<Item = &'a [u8; TSBK_BYTES]>) -> TsbkScan {
    let mut out = TsbkScan::default();
    for block in blocks.into_iter().take(MAX_TSBK_PER_WINDOW) {
        let Some(t) = Tsbk::parse(block) else {
            continue;
        };
        out.blocks += 1;
        if let Some(i) = t.iden_up() {
            out.iden_ups.push(i);
        } else if let Some(g) = t.grant() {
            out.grants.push(g);
        } else {
            out.unhandled += 1;
        }
    }
    out
}

/// The protocol a decoded window names, or `Unknown`.
///
/// Naming is gated on the **channel map**, not on having seen a plausible opcode: an identifier
/// enters the map only after [`MIN_IDEN_AGREEMENTS`] announcements agree on all 64 argument bits,
/// so the chance of naming a protocol from random CRC-valid blocks is 2^-64. Opcode-shaped luck
/// is not evidence, and a window that decoded nothing corroborated says `Unknown` rather than
/// guessing — which is what the T-266 column means by NULL.
pub fn protocol_of(map: &ChannelMap) -> TrunkProtocol {
    if map.admitted() > 0 {
        TrunkProtocol::P25Phase1
    } else {
        TrunkProtocol::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: i64) -> Timestamp {
        Timestamp::UNIX_EPOCH.saturating_add_nanos(secs * 1_000_000_000)
    }

    /// Builds a TSBK the way the synthetic generator does, so the decoder is tested against a
    /// layout written down rather than against itself.
    fn tsbk(opcode: u8, args: [u8; 8]) -> [u8; TSBK_BYTES] {
        let mut b = [0u8; TSBK_BYTES];
        b[0] = opcode & 0x3F;
        b[1] = 0;
        b[2..10].copy_from_slice(&args);
        b
    }

    fn iden_up_args(iden: u8, bw: u16, sign: u8, mag: u8, spacing: u16, base: u32) -> [u8; 8] {
        let mut v: u64 = 0;
        v |= u64::from(iden & 0x0F) << 60;
        v |= u64::from(bw & 0x1FF) << 51;
        v |= u64::from(sign & 1) << 50;
        v |= u64::from(mag) << 42;
        v |= u64::from(spacing & 0x3FF) << 32;
        v |= u64::from(base);
        v.to_be_bytes()
    }

    fn grant_args(service: u8, channel: u16, group: u16, source: u32) -> [u8; 8] {
        let mut v: u64 = 0;
        v |= u64::from(service) << 56;
        v |= u64::from(channel) << 40;
        v |= u64::from(group) << 24;
        v |= u64::from(source & 0x00FF_FFFF);
        v.to_be_bytes()
    }

    /// The published worked example, reproduced end to end. This is the test that makes the unit
    /// claims ("base ×5 Hz", "spacing ×125 Hz", "4+12 split") checkable by someone else: a base
    /// field of 0x09157562 and a spacing field of 100 must put channel 1554 on 771.718750 MHz.
    ///
    /// The example is a TDMA system and divides the spacing by its slot count; this decoder is
    /// Phase 1 FDMA, so the equivalent FDMA statement is a spacing field of 50 (× 125 Hz = 6.25
    /// kHz), which must land on the same frequency.
    #[test]
    fn the_published_worked_example_resolves_to_its_published_frequency() {
        let block = tsbk(OP_IDEN_UP, iden_up_args(3, 0, 1, 0, 50, 0x0915_7562));
        let iden = Tsbk::parse(&block).unwrap().iden_up().expect("an IDEN_UP");
        assert_eq!(iden.iden, 3);
        assert_eq!(iden.base_hz, 762_006_250.0, "base field × 5 Hz");
        assert_eq!(iden.spacing_hz, 6_250.0, "spacing field × 125 Hz");

        let mut map = ChannelMap::new();
        assert!(
            map.observe(&iden, t(0)).is_none(),
            "one message is not a plan"
        );
        assert!(map.observe(&iden, t(1)).is_some(), "the second admits it");

        // The 16-bit channel number for identifier 3, channel 1554.
        let channel = (3u16 << 12) | 1554;
        let Resolved::Mapped {
            f_hz,
            iden,
            channel_number,
            ..
        } = map.resolve(channel, t(2))
        else {
            panic!("an admitted identifier must map");
        };
        assert_eq!(iden, 3);
        assert_eq!(channel_number, 1554, "the low 12 bits are the channel");
        assert_eq!(
            f_hz, 771_718_750.0,
            "the published worked example's frequency"
        );
    }

    #[test]
    fn the_block_header_splits_into_flags_opcode_and_mfid() {
        let mut b = tsbk(OP_IDEN_UP, [0; 8]);
        b[0] |= 0x80; // last block
        b[0] |= 0x40; // protected
        let p = Tsbk::parse(&b).unwrap();
        assert!(p.last_block && p.protected);
        assert_eq!(p.opcode, OP_IDEN_UP, "the flags are not part of the opcode");
        assert_eq!(p.mfid, 0);
        assert!(
            Tsbk::parse(&b[..11]).is_none(),
            "a short block parses to nothing"
        );
    }

    #[test]
    fn a_grant_carries_a_channel_number_and_no_frequency() {
        let channel = (1u16 << 12) | 0x0123;
        let block = tsbk(
            OP_GRP_VCH_GRANT,
            grant_args(0x40, channel, 1234, 0x00AB_CDEF),
        );
        let g = Tsbk::parse(&block).unwrap().grant().expect("a grant");
        assert!(!g.update);
        assert_eq!(g.channel, channel);
        assert_eq!(g.iden(), 1);
        assert_eq!(g.channel_number(), 0x0123);
        assert_eq!(g.talkgroup, 1234);
        assert_eq!(g.source, 0x00AB_CDEF);

        let upd = tsbk(OP_GRP_VCH_GRANT_UPDATE, grant_args(0, channel, 7, 0));
        assert!(Tsbk::parse(&upd).unwrap().grant().unwrap().update);
    }

    /// C23's pitfall, as the test that matters: an identifier we never heard must yield NOTHING.
    /// The trap is that another identifier's parameters would happily produce a frequency that
    /// looks entirely plausible, and that frequency must never appear.
    #[test]
    fn an_unknown_identifier_is_refused_rather_than_mapped_through_another_ones_parameters() {
        let iden1 = Tsbk::parse(&tsbk(OP_IDEN_UP, iden_up_args(1, 0, 1, 0, 50, 170_200_000)))
            .unwrap()
            .iden_up()
            .unwrap();
        let mut map = ChannelMap::new();
        map.observe(&iden1, t(0));
        map.observe(&iden1, t(1));
        assert_eq!(map.admitted(), 1);

        let number = 0x0064u16;
        let wrong_hz = iden1.base_hz + iden1.spacing_hz * f64::from(number);
        // The same channel number under an identifier that was never announced.
        let channel = (7u16 << 12) | number;
        match map.resolve(channel, t(2)) {
            Resolved::Unmapped(u) => assert_eq!(u.reason(), "no-iden"),
            Resolved::Mapped { f_hz, .. } => panic!(
                "identifier 7 was never announced but resolved to {f_hz} Hz \
                 (identifier 1 would give {wrong_hz} Hz)"
            ),
        }
        // And identifier 1 itself still maps, so the refusal is about the missing entry, not a
        // map that stopped working.
        assert!(matches!(
            map.resolve((1u16 << 12) | number, t(2)),
            Resolved::Mapped { f_hz, .. } if f_hz == wrong_hz
        ));
    }

    /// The other half of the same pitfall: an entry decoded too long ago refuses, and reports its
    /// age, instead of mapping to the frequency it used to mean.
    #[test]
    fn a_stale_identifier_is_detected_rather_than_used() {
        let iden = Tsbk::parse(&tsbk(OP_IDEN_UP, iden_up_args(2, 0, 1, 0, 50, 170_200_000)))
            .unwrap()
            .iden_up()
            .unwrap();
        let mut map = ChannelMap::new();
        map.observe(&iden, t(0));
        map.observe(&iden, t(0));
        let channel = (2u16 << 12) | 200;
        let would_be = iden.base_hz + iden.spacing_hz * 200.0;

        // Inside the window it maps.
        let inside = t(IDEN_MAX_AGE_S as i64);
        assert!(matches!(
            map.resolve(channel, inside),
            Resolved::Mapped { f_hz, .. } if f_hz == would_be
        ));

        // One second past it, the same table refuses — and names the age, so the event can say why.
        let outside = t(IDEN_MAX_AGE_S as i64 + 1);
        match map.resolve(channel, outside) {
            Resolved::Unmapped(Unmapped::Stale { age_s, max_age_s }) => {
                assert!(age_s > max_age_s);
                assert_eq!(max_age_s, IDEN_MAX_AGE_S);
            }
            other => panic!("a stale table resolved to {other:?}, would-be {would_be} Hz"),
        }
    }

    /// The agreement gate is what makes a lucky block harmless. Two announcements that disagree
    /// are two claims, not corroboration, so neither is admitted.
    #[test]
    fn disagreeing_announcements_do_not_corroborate_each_other() {
        let a = Tsbk::parse(&tsbk(OP_IDEN_UP, iden_up_args(1, 0, 1, 0, 50, 170_200_000)))
            .unwrap()
            .iden_up()
            .unwrap();
        let b = Tsbk::parse(&tsbk(OP_IDEN_UP, iden_up_args(1, 0, 1, 0, 50, 180_000_000)))
            .unwrap()
            .iden_up()
            .unwrap();
        let mut map = ChannelMap::new();
        assert!(map.observe(&a, t(0)).is_none());
        assert!(
            map.observe(&b, t(1)).is_none(),
            "a disagreement restarts the count"
        );
        assert_eq!(map.admitted(), 0);
        assert!(matches!(
            map.resolve(1 << 12, t(2)),
            Resolved::Unmapped(Unmapped::NoIden)
        ));
        // The newer claim, corroborated, wins — the table follows the system, not the first thing
        // it happened to hear.
        assert!(map.observe(&b, t(2)).is_some());
        assert_eq!(map.iden(1).unwrap().base_hz, b.base_hz);
    }

    #[test]
    fn an_announcement_is_admitted_once_so_the_append_only_table_gets_one_row() {
        let iden = Tsbk::parse(&tsbk(OP_IDEN_UP, iden_up_args(1, 8, 1, 4, 50, 170_200_000)))
            .unwrap()
            .iden_up()
            .unwrap();
        let mut map = ChannelMap::new();
        assert!(map.observe(&iden, t(0)).is_none());
        let entry = map.observe(&iden, t(1)).expect("admitted on the second");
        assert_eq!(entry.iden, 1);
        assert_eq!(entry.spacing_hz, 6_250.0);
        assert_eq!(entry.bandwidth_hz, Some(1_000.0));
        assert_eq!(entry.tx_offset_hz, 1_000_000.0);
        entry
            .validate()
            .expect("a decoded entry is a valid model entry");
        for k in 2..8 {
            assert!(
                map.observe(&iden, t(k)).is_none(),
                "no second row for a repeat"
            );
        }
        assert_eq!(map.admitted(), 1);
    }

    #[test]
    fn a_degenerate_announcement_never_enters_the_plan() {
        // A zero base or a zero spacing cannot be a band plan, and the T-266 column would reject
        // it anyway; refusing here means the append never attempts an invalid row.
        for args in [
            iden_up_args(1, 0, 1, 0, 50, 0),
            iden_up_args(1, 0, 1, 0, 0, 170_200_000),
        ] {
            let iden = Tsbk::parse(&tsbk(OP_IDEN_UP, args))
                .unwrap()
                .iden_up()
                .unwrap();
            let mut map = ChannelMap::new();
            map.observe(&iden, t(0));
            map.observe(&iden, t(1));
            assert_eq!(map.admitted(), 0, "{iden:?} entered the plan");
        }
    }

    #[test]
    fn a_window_of_blocks_separates_identifier_updates_from_grants() {
        let blocks = [
            tsbk(OP_IDEN_UP, iden_up_args(1, 0, 1, 0, 50, 170_200_000)),
            tsbk(OP_GRP_VCH_GRANT, grant_args(0, (1 << 12) | 5, 100, 1)),
            tsbk(0x3A, [0; 8]),
        ];
        let scan = scan_blocks(blocks.iter());
        assert_eq!(scan.blocks, 3);
        assert_eq!(scan.iden_ups.len(), 1);
        assert_eq!(scan.grants.len(), 1);
        assert_eq!(
            scan.unhandled, 1,
            "an opcode this decoder does not read is counted"
        );
    }

    /// The encryption bit, at the grant that carries it (T-270).
    ///
    /// The asymmetry is the assertion: a set bit says `Encrypted`, and a clear bit says **nothing**
    /// rather than `Clear`. There is no input to this function that produces `Clear`.
    #[test]
    fn a_grants_service_options_may_say_encrypted_but_never_clear() {
        let channel = (1u16 << 12) | 5;
        let grant_with = |svc: u8| {
            Tsbk::parse(&tsbk(OP_GRP_VCH_GRANT, grant_args(svc, channel, 100, 7)))
                .unwrap()
                .grant()
                .expect("a grant")
        };

        // The verified bit, set: encrypted, and the evidence names what said so.
        let enc = grant_with(SVC_ENCRYPTED).encryption();
        assert!(enc.is_encrypted());
        assert_eq!(enc.evidence(), Some(EncryptionEvidence::ServiceOptions));
        assert_eq!(enc.algid(), None, "a grant carries no ALGID");

        // The same bit clear — and, crucially, with other bits set, so this is not passing merely
        // because the octet was zero. Nothing said, so nothing is claimed.
        for svc in [0x00, 0x80, 0x20, 0x10, 0x07, 0xBF] {
            let g = grant_with(svc);
            assert_eq!(
                g.encryption(),
                Encryption::Unknown,
                "service options {svc:#04x} claimed an encryption state"
            );
            assert!(!g.encryption().is_clear(), "{svc:#04x} reached `clear`");
            // The octet is still recorded verbatim, so nothing is lost by not interpreting it.
            assert_eq!(g.service_options.map(ServiceOptions::raw), Some(svc));
        }
    }

    /// Late entry, as a fact about the protocol: a grant update carries no service-options octet,
    /// so there is nothing to read and the state is `Unknown` — not `clear`, however ordinary the
    /// traffic it announces looks.
    #[test]
    fn a_grant_update_carries_no_service_options_so_late_entry_is_unknown() {
        let channel = (1u16 << 12) | 5;
        // Even with the encryption bit set in the bytes, an update must not read one: the field is
        // not there in the real message, so this decoder must not invent it in either direction.
        let upd = Tsbk::parse(&tsbk(
            OP_GRP_VCH_GRANT_UPDATE,
            grant_args(SVC_ENCRYPTED, channel, 100, 0),
        ))
        .unwrap()
        .grant()
        .expect("a grant update");
        assert!(upd.update);
        assert_eq!(upd.service_options, None, "an update has no such octet");
        assert_eq!(upd.encryption(), Encryption::Unknown);
        assert!(!upd.encryption().is_clear());
    }

    /// The ALGID table: the only path that may say `clear`, and the one octet that earns it.
    #[test]
    fn only_algid_0x80_reads_as_clear_and_every_other_octet_is_an_algorithm() {
        let clear = algid_encryption(P25_ALGID_CLEAR, None);
        assert!(clear.is_clear());
        assert_eq!(clear.evidence(), Some(EncryptionEvidence::Algid));
        assert_eq!(clear.algid(), Some(P25_ALGID_CLEAR));
        assert_eq!(algid_name(P25_ALGID_CLEAR), Some("clear"));

        // The three named algorithms, and the key id carried through.
        for (algid, name) in [(0x81u8, "DES-OFB"), (0x84, "AES-256"), (0xAA, "ADP")] {
            let enc = algid_encryption(algid, Some(0x1234));
            assert!(enc.is_encrypted(), "{name} did not read as encrypted");
            assert_eq!(enc.algid(), Some(algid));
            assert_eq!(enc.key_id(), Some(0x1234));
            assert_eq!(algid_name(algid), Some(name));
        }

        // An algorithm nobody named is still an algorithm: encrypted, carrying the byte, never
        // dropped and never clear. This is the case an "unknown means fine" bug would fail.
        for algid in [0x00u8, 0x01, 0x83, 0x9F, 0xFF] {
            let enc = algid_encryption(algid, None);
            assert!(
                enc.is_encrypted(),
                "unrecognised ALGID {algid:#04x} did not read as encrypted"
            );
            assert_eq!(enc.algid(), Some(algid), "the byte must be recorded");
            assert!(!enc.is_clear());
        }
        // Exactly one octet in the whole 8-bit space reads as clear.
        let clears = (0u16..=255).filter(|&a| algid_encryption(a as u8, None).is_clear());
        assert_eq!(clears.count(), 1, "more than one ALGID reached `clear`");
    }

    /// The naming rule, and the negative control that matters: random CRC-valid blocks — which is
    /// exactly what T-287's fixture carries — must never name a protocol or build a band plan.
    #[test]
    fn random_blocks_name_no_protocol_and_build_no_plan() {
        let mut state = 0x243F_6A88_85A3_08D3u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut map = ChannelMap::new();
        let mut iden_ups = 0usize;
        for _ in 0..200_000 {
            let mut b = [0u8; TSBK_BYTES];
            for chunk in b.chunks_mut(8) {
                let v = next().to_be_bytes();
                chunk.copy_from_slice(&v[..chunk.len()]);
            }
            let scan = scan_blocks(std::iter::once(&b));
            for i in &scan.iden_ups {
                iden_ups += 1;
                map.observe(i, t(0));
            }
        }
        eprintln!(
            "[T-268] 200,000 random blocks: {iden_ups} IDEN_UP-shaped, {} admitted",
            map.admitted()
        );
        assert!(
            iden_ups > 0,
            "the opcode must be reachable, or this proves nothing"
        );
        assert_eq!(
            map.admitted(),
            0,
            "random blocks corroborated a band-plan entry: the agreement gate is not working"
        );
        assert_eq!(protocol_of(&map), TrunkProtocol::Unknown);
    }
}
