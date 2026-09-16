//! DMR Tier III CSBK decode, and the frequency it refuses to invent (C23, T-271).
//!
//! T-268 did this for P25 Phase 1: read the blocks a confirmed control channel already CRC-checked,
//! and turn what they said into a band plan and grants. This is the same shape for DMR's trunked
//! tier, and it ends differently in one important place — see *The frequency this module will not
//! produce* below.
//!
//! # What is verified, and by whom
//!
//! Every number here was checked against an independent reference **before** it was written, and
//! where a published worked example or an arithmetic identity could settle it, that is what settles
//! it rather than a recited table (T-268's rule, and the reason it exists: references repeat each
//! other's mistakes, which is how the wrong P25 CRC variant is in half of them).
//!
//! - **Frame sync is 48 bits, and a base-station *data* burst — which is what a Tier III control
//!   channel transmits — carries `0xDFF57D75DF5D`.** *Verified* two ways beyond being stated by two
//!   independent sources. First, **all 24 dibits are outer symbols**: under the standard 4FSK dibit
//!   map (`01`→+3, `00`→+1, `10`→−1, `11`→−3) the pattern decodes to ±3 at every position and never
//!   to an inner level, which is the documented property of DMR's sync words. Second, it is the
//!   **exact dibit complement** of the base-station *voice* sync `0x755FD7DF75F7`: dibit 1 ↔ 3
//!   everywhere, i.e. the MSB of every dibit inverted. Two published hex constants reproducing both
//!   of those relations by arithmetic is not something a copied typo does. Both checks are unit
//!   tests below.
//! - **A CSBK is 96 bits, and the fields sum to exactly 96**: last-block (1), protect flag (1),
//!   CSBK opcode (6), feature-set id (8), 64 payload bits, 16-bit CRC. *Verified* — and the sum is
//!   itself the check, because a layout that is one field wrong does not add up.
//! - **The CSBK CRC mask is `0xA5A5`**: the CRC-CCITT over the block's first 80 bits is XORed with
//!   `0xA5A5` before being stored. *Verified* against an independent DMR implementation citing the
//!   air-interface specification. (The CRC *initial value* could **not** be corroborated; see the
//!   deliberate-deviations list below, where it does not affect any claim.)
//! - **Grant opcodes.** `0x30` P_GRANT, `0x32` BTV_GRANT (broadcast voice), `0x33` PD_GRANT
//!   (packet data), `0x34` TD_GRANT (data). *Verified*: a reference decoder's grant-opcode table
//!   names `0x32`/`0x33`/`0x34` individually, and two sources place the channel grants in the
//!   `0x30`–`0x38` opcode block. `0x19` C_ALOHA (the control channel's own system announcement) and
//!   `0x2E` P_CLEAR (call termination) are *verified* by the same table.
//! - **A grant's 64 payload bits lead with a 12-bit Logical Physical Channel Number (LPCN), then a
//!   timeslot bit, then the target and source addresses, with the source 24 bits wide.** *Verified*
//!   — and, again, the widths are pinned by arithmetic: 12 + 1 + 24 (target) + 24 (source) = 61,
//!   which leaves **exactly three bits** between the timeslot bit and the target address. That
//!   three-bit residue is the one thing here that is *not* corroborated, and it is handled below.
//!
//! # UNVERIFIED — and therefore decoded, recorded, and mapped through nothing
//!
//! - **The three flag bits** between the timeslot bit and the target address. Their meanings
//!   (emergency, late entry, and a third) could not be corroborated field-by-field against an
//!   independent reference. They are therefore carried **verbatim** in [`DmrGrant::flags`] and
//!   **nothing is decided by them** — in particular, no encryption state is claimed from them, which
//!   is why [`DmrGrant::encryption`] returns [`hk_model::Encryption::Unknown`] unconditionally. If
//!   one of them is a privacy bit, a later task with a corroborated layout can raise a grant to
//!   `Encrypted` through the very same seam; what it may never do is read a *clear* bit out of them,
//!   for the reason T-270 gives.
//! - **Everything the LPCN would need to become a frequency.** See below.
//!
//! # The frequency this module will not produce
//!
//! P25 announces its band plan on the air: an `IDEN_UP` carries a base frequency and a channel
//! spacing, and `f = base + spacing × channel` follows from messages the receiver actually heard.
//! **No DMR Tier III channel-parameter announcement could be corroborated here**, so this decoder
//! has no on-air statement to resolve an LPCN through.
//!
//! The tempting move is to assume a spacing — 12.5 kHz is *usually* right — and a base from wherever
//! the radio happens to be tuned. That produces a frequency for every grant, and it is exactly C23's
//! named pitfall ("stale IDEN tables mapping to wrong frequencies") committed on purpose: a
//! plausible-looking number that no message supports. So:
//!
//! **Every DMR grant resolves to [`DmrResolved::NoChannelParameters`], carrying no frequency and
//! saying why.** The grant itself is not in doubt — its talkgroup, source, channel number and
//! timeslot are all decoded and recorded — only the frequency is, and the row says which. This is
//! T-268's refusal shape exactly, one protocol along.
//!
//! # Metadata only, and nothing decrypts
//!
//! Nothing here touches voice frames, audio or message payloads, and there is no cipher in this
//! crate. Reading a flag is not decryption.
//!
//! # NOT a standards-compliant DMR receiver
//!
//! Framing here is simplified in the same deliberate way T-267 simplified P25, and for the same
//! reason (no standards-compliant off-air recording exists in this repo, and building a compliant
//! encoder to feed a third-party decoder is more work than the decoder while making a GPL binary a
//! dependency of a CI job that must stay green with no hardware). The deviations, stated so nobody
//! mistakes this for a DMR stack:
//!
//! 1. **No BPTC(196,96)** — a real CSBK is block-product-turbo coded.
//! 2. **No interleaving.**
//! 3. **The burst is flattened.** A real DMR burst is 264 bits with the 48-bit sync in the *middle*,
//!    between two 98-bit payload halves; here the sync precedes a contiguous 12-byte block, as
//!    T-267's P25 frames do.
//! 4. **No TDMA burst timing** — two 30 ms slots, slot alignment, and the CACH are not modelled.
//! 5. **The CRC initial value is this repo's choice** (0x0000), because it could not be
//!    corroborated. The *mask* is the verified part. The initial value is invisible to every claim
//!    this module makes: it changes only which 16-bit value a block must carry, so the gate's
//!    false-alarm arithmetic (2⁻¹⁶ for random data) is identical either way, and no field is read
//!    from the CRC.
//!
//! Anyone feeding this a real off-air capture must fix 1–4, and check 5 against the specification.

use hk_estimate::framing::crc::BitCrc;
use hk_model::{Encryption, TrunkProtocol};

// ---------------------------------------------------------------------------------------------
// Framing (verified)
// ---------------------------------------------------------------------------------------------

/// DMR base-station **data** frame sync, `0xDFF57D75DF5D`, as 24 dibits (48 bits, MSB first).
///
/// This is the sync a Tier III control channel transmits, because a CSBK is a data burst. *Verified*
/// — see the module docs, and [`tests::the_dmr_syncs_are_all_outer_symbols_and_exact_complements`],
/// which reproduces both published relations by arithmetic.
pub const DMR_BS_DATA_SYNC_DIBITS: [u8; 24] = [
    3, 1, 3, 3, 3, 3, 1, 1, 1, 3, 3, 1, 1, 3, 1, 1, 3, 1, 3, 3, 1, 1, 3, 1,
];

/// DMR base-station **voice** frame sync, `0x755FD7DF75F7`, as 24 dibits.
///
/// Not used to confirm a control channel — a control channel does not transmit voice bursts — but
/// kept because it is half of the arithmetic that verifies the data sync: the two are exact dibit
/// complements.
pub const DMR_BS_VOICE_SYNC_DIBITS: [u8; 24] = [
    1, 3, 1, 1, 1, 1, 3, 3, 3, 1, 1, 3, 3, 1, 3, 3, 1, 3, 1, 1, 3, 3, 1, 3,
];

/// Bytes in a CSBK, including its CRC. 96 bits: 1 + 1 + 6 + 8 + 64 + 16.
pub const CSBK_BYTES: usize = 12;

/// The mask XORed into a CSBK's CRC-CCITT before it is transmitted. *Verified.*
pub const CSBK_CRC_MASK: u16 = 0xA5A5;

// ---------------------------------------------------------------------------------------------
// Opcodes (verified)
// ---------------------------------------------------------------------------------------------

/// `C_ALOHA` — the control channel's own system announcement.
pub const CSBKO_C_ALOHA: u8 = 0x19;
/// `P_CLEAR` — a call was released.
pub const CSBKO_P_CLEAR: u8 = 0x2E;
/// `P_GRANT` — a private voice channel grant.
pub const CSBKO_P_GRANT: u8 = 0x30;
/// `BTV_GRANT` — a broadcast/talkgroup voice channel grant.
pub const CSBKO_BTV_GRANT: u8 = 0x32;
/// `PD_GRANT` — a packet-data channel grant.
pub const CSBKO_PD_GRANT: u8 = 0x33;
/// `TD_GRANT` — a data channel grant.
pub const CSBKO_TD_GRANT: u8 = 0x34;

/// The grant opcodes this decoder reads.
pub const CSBKO_GRANTS: [u8; 4] = [
    CSBKO_P_GRANT,
    CSBKO_BTV_GRANT,
    CSBKO_PD_GRANT,
    CSBKO_TD_GRANT,
];

/// The Tier III trunking opcodes whose presence is evidence that a control channel is a **trunked**
/// one rather than a conventional DMR repeater emitting the same sync.
///
/// Deliberately the corroborated ones only. An opcode this decoder cannot name is counted as
/// unhandled and contributes to nothing.
pub const CSBKO_TIER3: [u8; 6] = [
    CSBKO_C_ALOHA,
    CSBKO_P_CLEAR,
    CSBKO_P_GRANT,
    CSBKO_BTV_GRANT,
    CSBKO_PD_GRANT,
    CSBKO_TD_GRANT,
];

/// Whether an opcode names a voice grant — the only kind a follower would ever open a voice path on.
///
/// Data grants (`PD_GRANT`, `TD_GRANT`) are decoded and recorded as grants, because a data call is
/// traffic and dropping it would be the same silence this task exists to prevent, but they are not
/// voice and are marked as such.
pub const fn is_voice_grant(csbko: u8) -> bool {
    matches!(csbko, CSBKO_P_GRANT | CSBKO_BTV_GRANT)
}

/// A short name for an opcode, for a row a person reads. `None` for one this decoder does not name —
/// which is a statement, not a gap to fill with a guess.
pub fn csbko_name(csbko: u8) -> Option<&'static str> {
    Some(match csbko {
        CSBKO_C_ALOHA => "c-aloha",
        CSBKO_P_CLEAR => "p-clear",
        CSBKO_P_GRANT => "p-grant",
        CSBKO_BTV_GRANT => "btv-grant",
        CSBKO_PD_GRANT => "pd-grant",
        CSBKO_TD_GRANT => "td-grant",
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------------
// A-priori thresholds.
//
// Fixed, with the arithmetic below, BEFORE the decoder was run on anything. Never edit one of these
// to make a run pass (ADR-0016 §7; the rule `confirm.rs` and `tsbk.rs` state for their own floors).
// ---------------------------------------------------------------------------------------------

/// CRC-valid CSBKs carrying a **named Tier III opcode** that must be seen before this build will
/// name a system `dmr-tier3`.
///
/// A priori, in the same style as [`super::MIN_IDEN_AGREEMENTS`], and with the arithmetic stated so
/// it cannot drift to whatever makes a scene pass:
///
/// - Random data clears the 16-bit CRC with probability 2⁻¹⁶ = 1.53e-5.
/// - It carries the standard feature-set id (`FID = 0`) with probability 2⁻⁸ = 3.91e-3.
/// - Its 6-bit opcode is one of the [`CSBKO_TIER3`] six with probability 6/64 = 9.38e-2.
///
/// So one random block looks like a named Tier III CSBK with probability
/// 1.53e-5 × 3.91e-3 × 9.38e-2 = **5.6e-9**. Requiring `N` such blocks squares it for N = 2:
/// **3.1e-17** per pair, which sits alongside T-267's 1.4e-16 per confirmable frame and T-268's
/// 5.4e-20 per corroborated identifier — i.e. never, over any run this device will ever make.
///
/// **Error direction.** Raising this can only delay naming a real system by one more control-channel
/// message (a Tier III TSCC emits many per second, so the cost is milliseconds). Lowering it toward
/// 1 admits a 5.6e-9-per-block chance of naming a protocol from luck — and a *named* protocol is a
/// claim about what the emitter is, which is precisely the thing T-268 refused to make from a single
/// unrepeated message. The direction that costs a moment is the one this threshold takes.
pub const MIN_DMR_CSBKS: u32 = 2;

/// Most CSBKs decoded from one window, so the cost of a pass stays bounded a priori.
///
/// Matches [`super::MAX_TSBK_PER_WINDOW`]: at 4800 Bd a 96-bit block plus its 48-bit sync is 72
/// symbols, so a second of control channel holds ~66 frames, and 4096 is two orders of magnitude of
/// headroom over any window the chain collects.
pub const MAX_CSBK_PER_WINDOW: usize = 4096;

// ---------------------------------------------------------------------------------------------
// The block
// ---------------------------------------------------------------------------------------------

/// The CSBK CRC: CRC-CCITT over the first 80 bits, XORed with [`CSBK_CRC_MASK`].
///
/// See the module docs for why the initial value is this repo's choice and why that choice is
/// invisible to every claim made from a block.
pub fn csbk_crc() -> BitCrc {
    BitCrc::new(16, 0x1021, 0x0000, false, false, u32::from(CSBK_CRC_MASK))
        .expect("CRC-CCITT with the DMR CSBK mask is a valid BitCrc")
}

/// Whether a 12-byte block's stored CRC matches the one its first ten bytes imply.
///
/// The `BitCrc` is built once and shared: it carries a 256-entry table, and this is called once per
/// frame-sync hit.
pub fn csbk_crc_ok(block: &[u8]) -> bool {
    static CRC: std::sync::OnceLock<BitCrc> = std::sync::OnceLock::new();
    if block.len() < CSBK_BYTES {
        return false;
    }
    let want = (u16::from(block[10]) << 8) | u16::from(block[11]);
    CRC.get_or_init(csbk_crc).compute(&block[..10], 0, 80) as u16 == want
}

/// One decoded control signalling block.
///
/// Constructing one asserts only that 12 bytes parsed — **not** that they were a real message. The
/// CRC is checked by whatever produced the block, exactly as [`super::Tsbk`] inherits its CRC check
/// from the confirmer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Csbk {
    /// Last block of a multi-block message.
    pub last_block: bool,
    /// Protect flag. Recorded, never acted on here.
    pub protect: bool,
    /// CSBK opcode, 6 bits.
    pub csbko: u8,
    /// Feature-set id (0x00 = standard).
    pub fid: u8,
    /// The 64 payload bits.
    pub payload: [u8; 8],
}

impl Csbk {
    /// Parses the 12-byte block. The trailing two CRC bytes are not re-checked here.
    pub fn parse(block: &[u8]) -> Option<Self> {
        if block.len() < CSBK_BYTES {
            return None;
        }
        let mut payload = [0u8; 8];
        payload.copy_from_slice(&block[2..10]);
        Some(Self {
            last_block: block[0] & 0x80 != 0,
            protect: block[0] & 0x40 != 0,
            csbko: block[0] & 0x3F,
            fid: block[1],
            payload,
        })
    }

    /// Reads `n` bits from the payload starting at bit `at` (MSB-first).
    fn bits(&self, at: u32, n: u32) -> u64 {
        debug_assert!(n <= 64 && at + n <= 64);
        let mut v = 0u64;
        for i in 0..n {
            let bit = at + i;
            let byte = self.payload[(bit / 8) as usize];
            v = (v << 1) | u64::from((byte >> (7 - bit % 8)) & 1);
        }
        v
    }

    /// Whether this block carries a **named** Tier III trunking opcode with the standard feature
    /// set — the evidence [`MIN_DMR_CSBKS`] counts.
    pub fn is_tier3(&self) -> bool {
        self.fid == 0 && CSBKO_TIER3.contains(&self.csbko)
    }

    /// The channel grant this block carries, if it is one.
    ///
    /// Layout of the 64 payload bits, which sum to exactly 64: LPCN (12), timeslot (1), **three
    /// uncorroborated flag bits**, target address (24), source address (24). See the module docs:
    /// the flags are recorded and decide nothing.
    pub fn grant(&self) -> Option<DmrGrant> {
        if self.fid != 0 || !CSBKO_GRANTS.contains(&self.csbko) {
            return None;
        }
        Some(DmrGrant {
            csbko: self.csbko,
            lpcn: self.bits(0, 12) as u16,
            timeslot: self.bits(12, 1) as u8,
            flags: self.bits(13, 3) as u8,
            target: self.bits(16, 24) as u32,
            source: self.bits(40, 24) as u32,
        })
    }
}

/// A DMR Tier III channel grant.
///
/// It carries a **logical** channel number, which is the whole difficulty: see
/// [`DmrGrant::resolve`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmrGrant {
    /// The opcode that granted it, so a row can say which kind of grant this was.
    pub csbko: u8,
    /// Logical Physical Channel Number, 12 bits. *Not* a frequency, and not resolvable to one here.
    pub lpcn: u16,
    /// Timeslot, 1 bit — DMR is two-slot TDMA, and a call without its slot is under-attributed
    /// (C23's slot mix-up pitfall).
    pub timeslot: u8,
    /// The three payload bits between the timeslot and the target address. **UNVERIFIED**: carried
    /// verbatim so a later task can revisit them, and mapped through to nothing at all.
    pub flags: u8,
    /// Target address (talkgroup for a broadcast grant, radio for a private one), 24 bits.
    pub target: u32,
    /// Source radio address, 24 bits.
    pub source: u32,
}

impl DmrGrant {
    /// Whether this grant is for voice (as opposed to data).
    pub const fn is_voice(&self) -> bool {
        is_voice_grant(self.csbko)
    }

    /// The opcode's name, when this decoder names it.
    pub fn opcode_name(&self) -> Option<&'static str> {
        csbko_name(self.csbko)
    }

    /// What this grant is entitled to say about encryption: **nothing**.
    ///
    /// The DMR privacy indication a call carries lives in a PI (privacy indicator) header on the
    /// *traffic* channel, which nothing in this milestone demodulates — structurally the same
    /// position P25's ALGID is in (T-270). And the three flag bits this grant does carry are
    /// uncorroborated, so no claim may be built on them in either direction.
    ///
    /// So every DMR call reaches [`super::VoicePermit::open`] as
    /// [`hk_model::Encryption::Unknown`] and is refused a voice path, exactly as hard as an
    /// encrypted one. There is no second gate: the state this returns is the state that permit
    /// consumes. See [`dmr_pi_encryption`] for the one thing that could ever raise it.
    pub const fn encryption(&self) -> Encryption {
        Encryption::Unknown
    }

    /// The frequency this grant's channel number resolves to: **none, and why**.
    ///
    /// Kept as a method returning a refusal rather than simply having no method, so the refusal is
    /// something a caller receives and records rather than something it has to remember to say.
    pub const fn resolve(&self) -> DmrResolved {
        DmrResolved::NoChannelParameters
    }
}

/// What a DMR logical channel number resolved to.
///
/// One variant today, and the type exists precisely so that adding a `Mapped` variant later is a
/// change a compiler forces every caller to consider, rather than a frequency quietly appearing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmrResolved {
    /// No channel-parameter announcement was decoded, so there is no base and no step to resolve
    /// through, and **no frequency is produced**.
    NoChannelParameters,
}

impl DmrResolved {
    /// A short machine reason for a `grant_event` detail.
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::NoChannelParameters => "no-channel-parameters",
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The privacy indicator (T-270's seam, one protocol along)
// ---------------------------------------------------------------------------------------------

/// Reads a DMR privacy indication into an encryption state.
///
/// **Asymmetric, in the direction T-270 fixed and for a reason DMR makes sharper than P25.** A DMR
/// PI header is transmitted *because* a call is protected — an unprotected call simply has no PI
/// header at all. So:
///
/// - a PI header **present** says `Encrypted`, with [`hk_model::EncryptionEvidence::DmrPi`] naming
///   what said so, and carries the algorithm and key identifiers it stated;
/// - a PI header **absent** says nothing whatsoever, and yields `Unknown`. It is *not* evidence of
///   clear traffic: it is also exactly what a call joined in progress, a missed header, or a broken
///   decode looks like.
///
/// There is no input to this function that produces [`hk_model::Encryption::Clear`], and that is
/// deliberate: nothing in M4 may say "clear" about DMR, because saying it needs a statement no
/// message this build reads can make.
///
/// The result goes to [`super::VoicePermit::open`] like every other encryption state — the **same**
/// gate T-270 built, not a DMR-specific one. `Unknown` fails closed there exactly as hard as
/// `Encrypted` does.
///
/// Nothing here decrypts. Reading a privacy *indicator* is not decryption: no key is derived, no
/// keystream is produced, and this crate contains no cipher.
///
/// # Reachability in M4, stated rather than implied
///
/// A PI header rides on the **traffic** channel, and this milestone demodulates no traffic-channel
/// voice frames, so nothing in a normal run calls this with `Some`. That is the same position P25's
/// ALGID is in (T-270) and it is the honest one: the function exists so the seam is in place and
/// unit-tested before the thing that feeds it arrives, not so a run can pretend to have read one.
pub fn dmr_pi_encryption(pi: Option<DmrPrivacyHeader>) -> Encryption {
    let Some(h) = pi else {
        // The whole point. `hk_model::Encryption::from_dmr_pi(false)` exists and returns `Clear`,
        // and this is the one place in the workspace that could reach it — so it deliberately does
        // not. An absent header is not a header saying "no", it is no header.
        return Encryption::Unknown;
    };
    match Encryption::from_dmr_pi(true) {
        Encryption::Encrypted { evidence, .. } => Encryption::Encrypted {
            evidence,
            algid: Some(h.algorithm),
            key_id: h.key_id,
        },
        // Unreachable by construction: `from_dmr_pi(true)` is `Encrypted`. Falling through to
        // `Unknown` rather than panicking keeps the fail-closed direction even if that ever changes.
        _ => Encryption::Unknown,
    }
}

/// A DMR privacy indicator header, as read from a traffic channel.
///
/// **UNVERIFIED field layout**: this carries what a PI header *states* — an algorithm identifier and
/// a key identifier — without asserting where in the header those octets sit, because no independent
/// reference could be corroborated for the bit positions. Nothing in this build parses one; the type
/// is the shape the seam takes, and a later task that verifies the layout fills it in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmrPrivacyHeader {
    /// The algorithm the header named.
    pub algorithm: u8,
    /// The key identifier the header named, when it carried one.
    pub key_id: Option<u16>,
}

// ---------------------------------------------------------------------------------------------
// Reading a window
// ---------------------------------------------------------------------------------------------

/// What one window of CRC-valid CSBKs contained.
#[derive(Clone, Debug, Default)]
pub struct CsbkScan {
    /// Blocks that parsed.
    pub blocks: usize,
    /// Blocks carrying a named Tier III opcode with the standard feature set.
    pub tier3: usize,
    /// Grants seen, in order.
    pub grants: Vec<DmrGrant>,
    /// Blocks whose opcode this decoder does not name.
    pub unhandled: usize,
}

/// Decodes CRC-valid CSBKs into grants and Tier III evidence.
///
/// Input is blocks a confirmer already validated, so nothing here re-decides whether a control
/// channel is present: [`super::CcConfirmer::confirm_framing`] remains the only source of a
/// [`super::ConfirmedCc`], and this function cannot produce one.
pub fn scan_csbks<'a>(blocks: impl IntoIterator<Item = &'a [u8; CSBK_BYTES]>) -> CsbkScan {
    let mut out = CsbkScan::default();
    for block in blocks.into_iter().take(MAX_CSBK_PER_WINDOW) {
        let Some(c) = Csbk::parse(block) else {
            continue;
        };
        out.blocks += 1;
        if c.is_tier3() {
            out.tier3 += 1;
        } else {
            out.unhandled += 1;
        }
        if let Some(g) = c.grant() {
            out.grants.push(g);
        }
    }
    out
}

/// The protocol a decoded window names, or `Unknown`.
///
/// Naming is gated on [`MIN_DMR_CSBKS`] CRC-valid blocks carrying a named Tier III opcode (3.1e-17
/// from random data; see that constant). A DMR *sync* alone is deliberately not enough: a
/// conventional Tier II repeater transmits the same base-station bursts, so sync says "DMR air
/// interface", and only trunking messages say "trunked".
pub fn dmr_protocol_of(scan: &CsbkScan) -> TrunkProtocol {
    if scan.tier3 >= MIN_DMR_CSBKS as usize {
        TrunkProtocol::DmrTier3
    } else {
        TrunkProtocol::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_model::EncryptionEvidence;

    /// The 4FSK dibit→level map both DMR and P25 use: `01`→+3, `00`→+1, `10`→−1, `11`→−3.
    fn level(dibit: u8) -> i8 {
        match dibit {
            0b01 => 3,
            0b00 => 1,
            0b10 => -1,
            0b11 => -3,
            _ => unreachable!(),
        }
    }

    fn hex_to_dibits(hex: &str) -> Vec<u8> {
        let bytes: Vec<u8> = (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex"))
            .collect();
        bytes
            .iter()
            .flat_map(|&b| [(b >> 6) & 3, (b >> 4) & 3, (b >> 2) & 3, b & 3])
            .collect()
    }

    /// The verification of the sync constants, done by arithmetic rather than by recitation.
    ///
    /// Two published hex words have to reproduce two independent documented properties of DMR's
    /// sync patterns: every symbol is an **outer** one, and the base-station data and voice syncs
    /// are exact **dibit complements** of each other. A transcription error survives neither.
    #[test]
    fn the_dmr_syncs_are_all_outer_symbols_and_exact_complements() {
        let data = hex_to_dibits("dff57d75df5d");
        let voice = hex_to_dibits("755fd7df75f7");
        assert_eq!(data.len(), 24, "48 bits is 24 dibits");
        assert_eq!(voice.len(), 24);

        // The constants in this module are those hex words, and nothing else.
        assert_eq!(data, DMR_BS_DATA_SYNC_DIBITS, "0xDFF57D75DF5D");
        assert_eq!(voice, DMR_BS_VOICE_SYNC_DIBITS, "0x755FD7DF75F7");

        // Property 1: outer symbols only. A DMR sync never uses ±1.
        for (i, &d) in data.iter().chain(voice.iter()).enumerate() {
            assert_eq!(
                level(d).abs(),
                3,
                "dibit {i} ({d}) is an inner symbol, which a DMR sync never uses"
            );
        }
        // Property 2: the two are exact complements — the MSB of every dibit inverted, i.e. 1 <-> 3.
        for (k, (&a, &b)) in data.iter().zip(&voice).enumerate() {
            assert_eq!(
                level(a),
                -level(b),
                "dibit {k}: the data and voice syncs are not complements"
            );
        }
    }

    /// The layout claim, as arithmetic: a CSBK's fields sum to exactly 96 bits, and a grant's to
    /// exactly 64. A layout that is one field wrong does not add up, which is the only check
    /// available without the specification in hand.
    #[test]
    fn the_csbk_and_grant_layouts_sum_to_their_block_sizes() {
        // last-block(1) + protect(1) + opcode(6) + FID(8) + payload(64) + CRC(16)
        assert_eq!(1 + 1 + 6 + 8 + 64 + 16, CSBK_BYTES * 8);
        // LPCN(12) + timeslot(1) + flags(3) + target(24) + source(24)
        assert_eq!(12 + 1 + 3 + 24 + 24, 64);
    }

    fn csbk(csbko: u8, payload: [u8; 8]) -> [u8; CSBK_BYTES] {
        let mut b = [0u8; CSBK_BYTES];
        b[0] = csbko & 0x3F;
        b[1] = 0;
        b[2..10].copy_from_slice(&payload);
        let crc = csbk_crc().compute(&b[..10], 0, 80) as u16;
        b[10] = (crc >> 8) as u8;
        b[11] = crc as u8;
        b
    }

    fn grant_payload(lpcn: u16, slot: u8, flags: u8, target: u32, source: u32) -> [u8; 8] {
        let v: u64 = (u64::from(lpcn & 0x0FFF) << 52)
            | (u64::from(slot & 1) << 51)
            | (u64::from(flags & 7) << 48)
            | (u64::from(target & 0x00FF_FFFF) << 24)
            | u64::from(source & 0x00FF_FFFF);
        v.to_be_bytes()
    }

    #[test]
    fn a_grant_carries_a_logical_channel_a_slot_and_two_addresses() {
        let b = csbk(
            CSBKO_BTV_GRANT,
            grant_payload(0x0ABC, 1, 0b101, 0x00_1234, 0x00_5678),
        );
        assert!(csbk_crc_ok(&b), "the block this test built must check out");
        let c = Csbk::parse(&b).expect("12 bytes");
        assert_eq!(c.csbko, CSBKO_BTV_GRANT);
        assert_eq!(c.fid, 0);
        assert!(c.is_tier3());
        let g = c.grant().expect("a grant");
        assert_eq!(g.lpcn, 0x0ABC);
        assert_eq!(g.timeslot, 1);
        assert_eq!(g.flags, 0b101, "the uncorroborated bits are kept verbatim");
        assert_eq!(g.target, 0x00_1234);
        assert_eq!(g.source, 0x00_5678);
        assert!(g.is_voice());
        assert_eq!(g.opcode_name(), Some("btv-grant"));

        // A data grant is still a grant, and still traffic, but is not voice.
        let d = Csbk::parse(&csbk(CSBKO_PD_GRANT, grant_payload(5, 0, 0, 1, 2)))
            .unwrap()
            .grant()
            .expect("a data grant");
        assert!(!d.is_voice());
        assert_eq!(d.opcode_name(), Some("pd-grant"));
    }

    /// The assertion this module's whole design turns on: a DMR grant produces **no frequency**,
    /// and says why, rather than being resolved through a spacing nobody announced.
    #[test]
    fn a_dmr_grant_never_produces_a_frequency_and_says_why() {
        let g = Csbk::parse(&csbk(
            CSBKO_P_GRANT,
            grant_payload(0x0123, 0, 0, 0x00_4567, 0x00_89AB),
        ))
        .unwrap()
        .grant()
        .unwrap();
        // The tempting wrong answer, computed here only so the test can say what must NOT appear:
        // a 12.5 kHz spacing off some assumed base is a perfectly plausible-looking number.
        let plausible_but_unsupported_hz = 450e6 + 12_500.0 * f64::from(g.lpcn);
        assert_eq!(
            g.resolve(),
            DmrResolved::NoChannelParameters,
            "a DMR grant resolved to a frequency; the nearest plausible guess would have been \
             {plausible_but_unsupported_hz} Hz, which no message supports"
        );
        assert_eq!(g.resolve().reason(), "no-channel-parameters");
        // The grant itself is not in doubt: everything that WAS said is decoded.
        assert_eq!(g.lpcn, 0x0123);
        assert_eq!(g.target, 0x00_4567);
        assert_eq!(g.source, 0x00_89AB);
    }

    /// The uncorroborated flag bits decide nothing — in particular, no encryption state.
    #[test]
    fn the_uncorroborated_flag_bits_never_claim_an_encryption_state() {
        for flags in 0..8u8 {
            let g = Csbk::parse(&csbk(CSBKO_BTV_GRANT, grant_payload(1, 0, flags, 100, 200)))
                .unwrap()
                .grant()
                .unwrap();
            assert_eq!(g.flags, flags, "the octet must be recorded");
            assert_eq!(
                g.encryption(),
                Encryption::Unknown,
                "flag bits {flags:#05b} claimed an encryption state from an unverified layout"
            );
            assert!(!g.encryption().is_clear(), "unknown is never clear");
            // And the refusal is what T-270's gate sees — the same gate, not a DMR-specific one.
            assert!(super::super::VoicePermit::open(g.encryption()).is_err());
        }
    }

    /// The privacy indicator, at the seam T-270 built. Present says `Encrypted`; absent says
    /// nothing. No input says `clear`.
    #[test]
    fn a_privacy_header_may_say_encrypted_and_its_absence_says_nothing() {
        // Absent: not a claim of clear traffic. This is the case a "no PI means fine" bug fails.
        assert_eq!(dmr_pi_encryption(None), Encryption::Unknown);
        assert!(!dmr_pi_encryption(None).is_clear());
        assert!(super::super::VoicePermit::open(dmr_pi_encryption(None)).is_err());

        // Present: encrypted, naming the evidence and carrying the identifiers it stated.
        for (algorithm, key_id) in [(0x21u8, Some(0x1234u16)), (0x02, None), (0xFF, Some(1))] {
            let enc = dmr_pi_encryption(Some(DmrPrivacyHeader { algorithm, key_id }));
            assert!(enc.is_encrypted(), "algorithm {algorithm:#04x}");
            assert_eq!(enc.evidence(), Some(EncryptionEvidence::DmrPi));
            assert_eq!(enc.algid(), Some(algorithm));
            assert_eq!(enc.key_id(), key_id);
            assert!(!enc.is_clear());
            assert!(super::super::VoicePermit::open(enc).is_err());
        }

        // Exhaustively: nothing in the whole 8-bit algorithm space reaches `clear`.
        let clears = (0u16..=255)
            .filter(|&a| {
                dmr_pi_encryption(Some(DmrPrivacyHeader {
                    algorithm: a as u8,
                    key_id: None,
                }))
                .is_clear()
            })
            .count();
        assert_eq!(clears, 0, "a DMR privacy header reached `clear`");
    }

    /// The naming gate, and the negative control that matters: random CRC-valid-looking blocks must
    /// never name a protocol. This is the DMR twin of T-268's random-block test.
    #[test]
    fn random_blocks_name_no_protocol() {
        let mut state = 0x243F_6A88_85A3_08D3u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut valid = 0usize;
        let mut tier3 = 0usize;
        let mut grants = 0usize;
        for _ in 0..500_000 {
            let mut b = [0u8; CSBK_BYTES];
            for chunk in b.chunks_mut(8) {
                let v = next().to_be_bytes();
                chunk.copy_from_slice(&v[..chunk.len()]);
            }
            // Only blocks that would actually have reached the decoder: the confirmer gates on CRC.
            if !csbk_crc_ok(&b) {
                continue;
            }
            valid += 1;
            let scan = scan_csbks(std::iter::once(&b));
            tier3 += scan.tier3;
            grants += scan.grants.len();
        }
        eprintln!(
            "[T-271] 500,000 random blocks: {valid} CRC-valid, {tier3} Tier III-shaped, \
             {grants} grant-shaped"
        );
        // A single stray block must not be enough, which is what MIN_DMR_CSBKS is for.
        let scan = CsbkScan {
            tier3: 1,
            ..CsbkScan::default()
        };
        assert_eq!(
            dmr_protocol_of(&scan),
            TrunkProtocol::Unknown,
            "one Tier III-shaped block named a protocol"
        );
        let scan = CsbkScan {
            tier3: MIN_DMR_CSBKS as usize,
            ..CsbkScan::default()
        };
        assert_eq!(dmr_protocol_of(&scan), TrunkProtocol::DmrTier3);
    }

    /// The CRC gate is the thing that keeps the block stream honest, so the mask has to be applied:
    /// a block whose CRC was computed **without** the DMR mask must be rejected.
    #[test]
    fn the_csbk_crc_mask_is_applied_so_an_unmasked_block_is_rejected() {
        let mut b = csbk(CSBKO_C_ALOHA, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(csbk_crc_ok(&b));
        // The same block with the mask undone: still a valid CRC-CCITT, and still wrong for DMR.
        let unmasked = ((u16::from(b[10]) << 8) | u16::from(b[11])) ^ CSBK_CRC_MASK;
        b[10] = (unmasked >> 8) as u8;
        b[11] = unmasked as u8;
        assert!(
            !csbk_crc_ok(&b),
            "a block carrying an unmasked CRC-CCITT passed the DMR CSBK check"
        );
        assert!(!csbk_crc_ok(&b[..11]), "a short block never checks out");
    }

    #[test]
    fn a_window_separates_grants_from_the_opcodes_this_decoder_does_not_name() {
        let blocks = [
            csbk(CSBKO_C_ALOHA, [0; 8]),
            csbk(CSBKO_BTV_GRANT, grant_payload(7, 1, 0, 42, 43)),
            csbk(0x07, [0; 8]),
        ];
        let scan = scan_csbks(blocks.iter());
        assert_eq!(scan.blocks, 3);
        assert_eq!(scan.tier3, 2, "aloha and the grant");
        assert_eq!(scan.grants.len(), 1);
        assert_eq!(scan.unhandled, 1);
        assert_eq!(dmr_protocol_of(&scan), TrunkProtocol::DmrTier3);
    }
}
