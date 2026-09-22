//! C23 trunking metadata (T-266; docs/07 §2.28–§2.30; `docs/capabilities/C23-trunking-follow.md`
//! §Interface): the system model a control-channel decoder builds, the call records it produces,
//! and the grant stream underneath them.
//!
//! # Metadata only
//!
//! There is no `CallAudio` here and no column anywhere that could hold one (the migration says so
//! too). M4 follows grants to learn *that* a call happened — who, where, when, on what channel,
//! and whether it was encrypted — and nothing else. Keeping audio out is what stops the roadmap's
//! vocoder-IP question from gating any of this work: no vocoder is reachable from these types.
//!
//! # Encryption is three-state, and `unknown` is not `clear`
//!
//! The C23 card names the pitfall: *"late entry without a header: encryption status should default
//! to 'unknown', not 'clear'."* A system that reads "unknown" as "clear" presents an encrypted call
//! as listenable — wrong, and the kind of wrong that looks fine in a demo.
//!
//! [`Encryption`] makes the mistake **unconstructible** rather than merely discouraged:
//!
//! - It has **no `Default`**. There is no value you get by not deciding.
//! - [`Encryption::Clear`] and [`Encryption::Encrypted`] each *require* an
//!   [`EncryptionEvidence`] field — you cannot say "clear" without naming what said so.
//! - [`Encryption::Unknown`] carries no evidence field, so "nothing was seen" cannot be dressed up
//!   as a measurement.
//! - [`Encryption::is_clear`] matches `Clear` alone. `Unknown` answers `false`, so any future gate
//!   written as `if enc.is_clear()` fails closed for a late entry.
//!
//! The schema repeats the same rule as CHECK constraints, and the repository's read path refuses a
//! contradictory row instead of coercing it (`repo/trunking.rs`). This is the same discipline as
//! the rest of the model: `unknown` means *not measured*, never a value (T-164's `duty_cycle: 0.0`
//! read as "bursty" when it meant unmeasured; T-207's "Not yet classified").

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{CallRecordId, EmitterId, TrunkSystemId};
use crate::time::Timestamp;

/// Longest identifier text (system id, site id, talkgroup, unit id, channel) kept on a row.
pub const TRUNK_TEXT_MAX: usize = 64;

/// Longest talkgroup label.
pub const TRUNK_LABEL_MAX: usize = 128;

/// Most reasons kept on one [`CallRecord`].
pub const CALL_REASONS_MAX: usize = 16;

/// The P25 ALGID that means no encryption. Every other ALGID is an algorithm (0x81 DES-OFB,
/// 0x84 AES-256, 0xAA ADP/RC4; docs/04 §8.3), so anything but this reads as encrypted.
pub const P25_ALGID_CLEAR: u8 = 0x80;

/// A trunking object that breaks its own contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidTrunking(pub String);

impl fmt::Display for InvalidTrunking {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InvalidTrunking {}

fn bad(msg: impl Into<String>) -> InvalidTrunking {
    InvalidTrunking(msg.into())
}

fn check_text(what: &str, value: Option<&str>, max: usize) -> Result<(), InvalidTrunking> {
    match value {
        None => Ok(()),
        Some(v) if v.trim().is_empty() => Err(bad(format!("{what} is present but blank"))),
        Some(v) if v.chars().count() > max => {
            Err(bad(format!("{what} is longer than {max} characters")))
        }
        Some(_) => Ok(()),
    }
}

fn check_freq(what: &str, hz: Option<f64>) -> Result<(), InvalidTrunking> {
    match hz {
        None => Ok(()),
        Some(f) if !f.is_finite() || f <= 0.0 => {
            Err(bad(format!("{what} must be a positive, finite frequency")))
        }
        Some(_) => Ok(()),
    }
}

// ---------------------------------------------------------------------------------------------
// Protocol
// ---------------------------------------------------------------------------------------------

/// The trunking control protocol a system was found to speak.
///
/// [`TrunkProtocol::Unknown`] is a first-class outcome, not a placeholder to be filled in later by
/// guesswork: the control-channel hunter can confirm *a* continuous control channel by frame sync
/// and CRC (T-267) before any protocol decoder claims it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrunkProtocol {
    /// P25 Phase 1 (FDMA, C4FM/CQPSK, TSBK at 9600 bps).
    P25Phase1,
    /// P25 Phase 2 (TDMA, H-DQPSK, two slots).
    P25Phase2,
    /// DMR Tier III / Capacity Max.
    DmrTier3,
    /// Motorola SmartNet/SmartZone (3600 bps control).
    SmartNet,
    /// EDACS (9600 bps control).
    Edacs,
    /// NXDN Type-C (dedicated control channel).
    NxdnTypeC,
    /// MPT1327.
    Mpt1327,
    /// A confirmed control channel whose protocol has not been decoded yet.
    Unknown,
}

impl TrunkProtocol {
    /// The stored text.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::P25Phase1 => "p25-phase1",
            Self::P25Phase2 => "p25-phase2",
            Self::DmrTier3 => "dmr-tier3",
            Self::SmartNet => "smart-net",
            Self::Edacs => "edacs",
            Self::NxdnTypeC => "nxdn-type-c",
            Self::Mpt1327 => "mpt1327",
            Self::Unknown => "unknown",
        }
    }

    /// Reads the stored text back. An unrecognised string is an error, never `Unknown`: a protocol
    /// this build cannot name is not the same statement as "not decoded yet".
    pub fn parse(text: &str) -> Result<Self, InvalidTrunking> {
        match text {
            "p25-phase1" => Ok(Self::P25Phase1),
            "p25-phase2" => Ok(Self::P25Phase2),
            "dmr-tier3" => Ok(Self::DmrTier3),
            "smart-net" => Ok(Self::SmartNet),
            "edacs" => Ok(Self::Edacs),
            "nxdn-type-c" => Ok(Self::NxdnTypeC),
            "mpt1327" => Ok(Self::Mpt1327),
            "unknown" => Ok(Self::Unknown),
            other => Err(bad(format!("{other} is not a trunking protocol"))),
        }
    }

    /// Whether this protocol carries TDMA slots, so a call without a slot is under-attributed
    /// rather than simply FDMA (the C23 slot mix-up pitfall).
    pub const fn is_tdma(self) -> bool {
        matches!(self, Self::P25Phase2 | Self::DmrTier3)
    }
}

// ---------------------------------------------------------------------------------------------
// Encryption: the invariant this module exists for
// ---------------------------------------------------------------------------------------------

/// What said whether a call was encrypted. Required by [`Encryption::Clear`] and
/// [`Encryption::Encrypted`]: a state without a source is `Unknown`, not a claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EncryptionEvidence {
    /// A P25 ALGID octet in a header or voice frame (docs/04 §8.3).
    Algid,
    /// The grant's service-options encryption bit.
    ServiceOptions,
    /// A DMR privacy indicator in an LC or PI header.
    DmrPi,
    /// A link-control / voice header that stated the state without an ALGID.
    LcHeader,
    /// A person said so.
    User,
}

impl EncryptionEvidence {
    /// The stored text.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Algid => "algid",
            Self::ServiceOptions => "service-options",
            Self::DmrPi => "dmr-pi",
            Self::LcHeader => "lc-header",
            Self::User => "user",
        }
    }

    /// Reads the stored text back.
    pub fn parse(text: &str) -> Result<Self, InvalidTrunking> {
        match text {
            "algid" => Ok(Self::Algid),
            "service-options" => Ok(Self::ServiceOptions),
            "dmr-pi" => Ok(Self::DmrPi),
            "lc-header" => Ok(Self::LcHeader),
            "user" => Ok(Self::User),
            other => Err(bad(format!("{other} is not encryption evidence"))),
        }
    }
}

/// The encryption state of a call or grant: **clear, encrypted, or unknown**.
///
/// Deliberately **not** a `bool` and deliberately **without a `Default`**. The two states that make
/// a claim carry the evidence for it; the third carries nothing, because nothing was seen. There is
/// no constructor, conversion or serde path that turns an absent header into `Clear` — see the
/// module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum Encryption {
    /// Something said the traffic was in the clear, and `evidence` says what.
    Clear {
        /// What said so.
        evidence: EncryptionEvidence,
        /// The P25 ALGID, when one was read ([`P25_ALGID_CLEAR`] for a clear call).
        algid: Option<u8>,
        /// The key id, when one was read.
        key_id: Option<u16>,
    },
    /// Something said the traffic was encrypted, and `evidence` says what.
    Encrypted {
        /// What said so.
        evidence: EncryptionEvidence,
        /// The P25 ALGID, when one was read (0x81 DES-OFB, 0x84 AES-256, 0xAA ADP…).
        algid: Option<u8>,
        /// The key id, when one was read.
        key_id: Option<u16>,
    },
    /// **Nothing said.** Late entry without a header, a control message that does not carry the
    /// bit, or a decode too broken to trust. Never a synonym for `Clear`.
    Unknown,
}

impl Encryption {
    /// The stored text: `clear` / `encrypted` / `unknown`.
    pub const fn state(self) -> &'static str {
        match self {
            Self::Clear { .. } => "clear",
            Self::Encrypted { .. } => "encrypted",
            Self::Unknown => "unknown",
        }
    }

    /// What decided the state, or `None` when nothing did.
    pub const fn evidence(self) -> Option<EncryptionEvidence> {
        match self {
            Self::Clear { evidence, .. } | Self::Encrypted { evidence, .. } => Some(evidence),
            Self::Unknown => None,
        }
    }

    /// The ALGID, when one was read.
    pub const fn algid(self) -> Option<u8> {
        match self {
            Self::Clear { algid, .. } | Self::Encrypted { algid, .. } => algid,
            Self::Unknown => None,
        }
    }

    /// The key id, when one was read.
    pub const fn key_id(self) -> Option<u16> {
        match self {
            Self::Clear { key_id, .. } | Self::Encrypted { key_id, .. } => key_id,
            Self::Unknown => None,
        }
    }

    /// **True only when something said the call was in the clear.** `Unknown` answers `false`, so a
    /// gate written as `if enc.is_clear()` fails closed on a late entry — the C23 pitfall, made
    /// structural.
    pub const fn is_clear(self) -> bool {
        matches!(self, Self::Clear { .. })
    }

    /// True when something said the call was encrypted.
    pub const fn is_encrypted(self) -> bool {
        matches!(self, Self::Encrypted { .. })
    }

    /// Whether the state was measured at all.
    pub const fn is_known(self) -> bool {
        !matches!(self, Self::Unknown)
    }

    /// Reads a P25 ALGID: [`P25_ALGID_CLEAR`] is clear, every other value is an algorithm.
    pub const fn from_algid(algid: u8) -> Self {
        if algid == P25_ALGID_CLEAR {
            Self::Clear {
                evidence: EncryptionEvidence::Algid,
                algid: Some(algid),
                key_id: None,
            }
        } else {
            Self::Encrypted {
                evidence: EncryptionEvidence::Algid,
                algid: Some(algid),
                key_id: None,
            }
        }
    }

    /// Reads a grant's service-options encryption bit.
    pub const fn from_service_options(encrypted: bool) -> Self {
        if encrypted {
            Self::Encrypted {
                evidence: EncryptionEvidence::ServiceOptions,
                algid: None,
                key_id: None,
            }
        } else {
            Self::Clear {
                evidence: EncryptionEvidence::ServiceOptions,
                algid: None,
                key_id: None,
            }
        }
    }

    /// Reads a DMR privacy indicator.
    pub const fn from_dmr_pi(private: bool) -> Self {
        if private {
            Self::Encrypted {
                evidence: EncryptionEvidence::DmrPi,
                algid: None,
                key_id: None,
            }
        } else {
            Self::Clear {
                evidence: EncryptionEvidence::DmrPi,
                algid: None,
                key_id: None,
            }
        }
    }

    /// Later evidence folded into an existing state, keeping the **safer** answer: `Unknown` gives
    /// way to anything measured, and a call once seen encrypted never becomes clear (a key change
    /// mid-call must not read as "listenable after all"). A clear call may be escalated by a later
    /// encrypted header.
    pub fn refine(self, later: Self) -> Self {
        match (self, later) {
            (_, Self::Unknown) => self,
            (Self::Encrypted { .. }, Self::Clear { .. }) => self,
            _ => later,
        }
    }

    /// Checks the internal contract: a stated ALGID must agree with the state it is stored under.
    pub fn validate(self) -> Result<(), InvalidTrunking> {
        match self {
            Self::Clear {
                algid: Some(a),
                evidence,
                ..
            } if a != P25_ALGID_CLEAR => Err(bad(format!(
                "a call recorded clear on {} evidence cannot carry ALGID {a:#04x}",
                evidence.as_str()
            ))),
            Self::Encrypted {
                algid: Some(a),
                evidence,
                ..
            } if a == P25_ALGID_CLEAR => Err(bad(format!(
                "a call recorded encrypted on {} evidence cannot carry the clear ALGID",
                evidence.as_str()
            ))),
            _ => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// TrunkSystem and its parts
// ---------------------------------------------------------------------------------------------

/// One entry of a system's channel table: `f = base + spacing × (channel / slots)`, with the TX
/// offset for the uplink side (P25 `IDEN_UP` and `IDEN_UP_TDMA`; docs/04 §8.1).
///
/// Entries are kept with the time they were decoded and appended rather than overwritten, so a
/// **stale** table is detectable instead of silently mapping a grant to the wrong frequency (a C23
/// pitfall, asserted by T-268).
///
/// # `slots` is part of the entry, not a detail of the decoder (T-272)
///
/// An FDMA entry has `slots == 1` and the division is the identity. A **TDMA** entry (P25 Phase 2,
/// announced by `IDEN_UP_TDMA`) has two or four slots sharing one carrier, so consecutive channel
/// numbers are the *same frequency* on different slots. An entry stored without its slot count is
/// ambiguous between those two readings, and a reader re-deriving a frequency from it lands half a
/// channel out on every other channel — C23's TDMA slot mix-up pitfall, preserved in the database.
/// [`Self::downlink_hz`] and [`Self::slot_of`] are therefore the only sanctioned readings of a
/// channel number, and both go through `slots`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChannelPlanEntry {
    /// The protocol's table identifier (P25 IDEN is 4 bits; others index their own table).
    pub iden: u8,
    /// Base frequency, Hz.
    pub base_hz: f64,
    /// Channel spacing, Hz.
    pub spacing_hz: f64,
    /// Uplink offset, Hz (signed).
    pub tx_offset_hz: f64,
    /// Channel bandwidth, Hz, when the message carried it.
    pub bandwidth_hz: Option<f64>,
    /// TDMA slots sharing one carrier: 1 for FDMA, 2 or 4 for a P25 Phase 2 channel type.
    pub slots: u8,
    /// When this entry was decoded.
    pub t: Timestamp,
}

impl ChannelPlanEntry {
    /// Checks the entry.
    pub fn validate(&self) -> Result<(), InvalidTrunking> {
        check_freq("channel plan base", Some(self.base_hz))?;
        check_freq("channel plan spacing", Some(self.spacing_hz))?;
        check_freq("channel plan bandwidth", self.bandwidth_hz)?;
        if !self.tx_offset_hz.is_finite() {
            return Err(bad("channel plan tx offset must be finite"));
        }
        if !(1..=MAX_TDMA_SLOTS).contains(&self.slots) {
            return Err(bad(format!(
                "channel plan slots must be 1..={MAX_TDMA_SLOTS}"
            )));
        }
        Ok(())
    }

    /// Downlink frequency of a channel number under this entry, Hz.
    ///
    /// The channel number is divided by [`Self::slots`] first, so a two-slot TDMA entry puts
    /// channels `2n` and `2n+1` on the **same** frequency. For an FDMA entry (`slots == 1`) this
    /// is `base + spacing × channel`, unchanged.
    pub fn downlink_hz(&self, channel: u32) -> f64 {
        self.base_hz + self.spacing_hz * f64::from(channel / u32::from(self.slots.max(1)))
    }

    /// The TDMA slot a channel number names, or `None` on an FDMA entry — where there is no slot
    /// to attribute and claiming slot 0 would be an invented measurement.
    pub fn slot_of(&self, channel: u32) -> Option<u8> {
        (self.slots > 1).then(|| (channel % u32::from(self.slots)) as u8)
    }
}

/// Most TDMA slots one carrier may be divided into (P25 Phase 2 channel types name 1, 2 or 4).
pub const MAX_TDMA_SLOTS: u8 = 4;

/// A neighbour site a control channel advertised. At least one of the two fields is present:
/// an announcement with neither says nothing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NeighbourSite {
    /// The neighbour's site identifier, as decoded.
    pub site_id: Option<String>,
    /// Its control-channel frequency, Hz, when the announcement carried one.
    pub cc_freq_hz: Option<f64>,
    /// When it was announced.
    pub t: Timestamp,
}

impl NeighbourSite {
    /// Checks the announcement.
    pub fn validate(&self) -> Result<(), InvalidTrunking> {
        check_text("neighbour site id", self.site_id.as_deref(), TRUNK_TEXT_MAX)?;
        check_freq("neighbour control-channel frequency", self.cc_freq_hz)?;
        if self.site_id.is_none() && self.cc_freq_hz.is_none() {
            return Err(bad("a neighbour announcement names a site or a frequency"));
        }
        Ok(())
    }
}

/// Where a talkgroup's human label came from. A label is a **suggestion** — a prior (e.g.
/// RadioReference, C17) or a person — and never evidence about what was measured.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LabelSource {
    /// A person typed it.
    User,
    /// A catalogue prior supplied it.
    Prior,
}

impl LabelSource {
    /// The stored text.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Prior => "prior",
        }
    }

    /// Reads the stored text back.
    pub fn parse(text: &str) -> Result<Self, InvalidTrunking> {
        match text {
            "user" => Ok(Self::User),
            "prior" => Ok(Self::Prior),
            other => Err(bad(format!("{other} is not a label source"))),
        }
    }
}

/// A talkgroup seen on a system: the decoded id, an optional suggested label, and activity
/// counters. Aggregate — rebuildable from the call records.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Talkgroup {
    /// The decoded talkgroup id, as text (protocols number them differently).
    pub id: String,
    /// A suggested human label, never truth.
    pub label: Option<String>,
    /// Where the label came from; `None` exactly when there is no label.
    pub label_source: Option<LabelSource>,
    /// First and last time it was heard.
    pub first_seen: Timestamp,
    /// Last time it was heard.
    pub last_seen: Timestamp,
    /// Calls counted for it.
    pub calls: u64,
}

impl Talkgroup {
    /// Checks the row.
    pub fn validate(&self) -> Result<(), InvalidTrunking> {
        check_text("talkgroup id", Some(&self.id), TRUNK_TEXT_MAX)?;
        check_text("talkgroup label", self.label.as_deref(), TRUNK_LABEL_MAX)?;
        if self.label.is_some() != self.label_source.is_some() {
            return Err(bad("a talkgroup label names its source"));
        }
        if self.last_seen < self.first_seen {
            return Err(bad("a talkgroup's last_seen precedes its first_seen"));
        }
        Ok(())
    }
}

/// A trunked system as measured: the protocol, its identifiers, where its control channel is, and
/// the tables decoded off that control channel.
///
/// The identifiers are `Option` because a control channel is **found before it is identified**
/// (T-267 confirms a CC by frame sync plus CRC; T-268 decodes who it belongs to). A system whose
/// ids have not been decoded stays its own row rather than being merged with another undecoded
/// one — two things that have not been shown to be the same are two things.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrunkSystem {
    /// Row id.
    pub id: TrunkSystemId,
    /// The control protocol, possibly [`TrunkProtocol::Unknown`].
    pub protocol: TrunkProtocol,
    /// The system identifier as decoded (e.g. P25 `WACN:SYSID`), or `None` before it was read.
    pub system_id: Option<String>,
    /// The site identifier as decoded (e.g. P25 `RFSS:SITE`), or `None`.
    pub site_id: Option<String>,
    /// The control-channel frequency, Hz. `None` for a system with **no dedicated control
    /// channel** (Capacity Plus rest channel, NXDN Type-D, LTR) — never `0`.
    pub cc_freq_hz: Option<f64>,
    /// Current channel table, newest entry per `iden` (history in `trunk_channel_plan`).
    pub channel_plan: Vec<ChannelPlanEntry>,
    /// Neighbour sites announced.
    pub neighbours: Vec<NeighbourSite>,
    /// Talkgroups seen.
    pub talkgroups: Vec<Talkgroup>,
    /// First and last time the system was heard.
    pub first_seen: Timestamp,
    /// Last time the system was heard.
    pub last_seen: Timestamp,
    /// Row creation and update times.
    pub created_at: Timestamp,
    /// Row update time.
    pub updated_at: Timestamp,
}

impl TrunkSystem {
    /// A freshly discovered system: a protocol (possibly unknown), the control channel it was found
    /// on, and the time it was found. Everything else fills in as the control channel decodes.
    pub fn new(protocol: TrunkProtocol, cc_freq_hz: Option<f64>, t: Timestamp) -> Self {
        Self {
            id: TrunkSystemId::new(),
            protocol,
            system_id: None,
            site_id: None,
            cc_freq_hz,
            channel_plan: Vec::new(),
            neighbours: Vec::new(),
            talkgroups: Vec::new(),
            first_seen: t,
            last_seen: t,
            created_at: t,
            updated_at: t,
        }
    }

    /// Checks the system and everything hanging off it.
    pub fn validate(&self) -> Result<(), InvalidTrunking> {
        check_text("system id", self.system_id.as_deref(), TRUNK_TEXT_MAX)?;
        check_text("site id", self.site_id.as_deref(), TRUNK_TEXT_MAX)?;
        check_freq("control-channel frequency", self.cc_freq_hz)?;
        if self.last_seen < self.first_seen {
            return Err(bad("a system's last_seen precedes its first_seen"));
        }
        if self.updated_at < self.created_at {
            return Err(bad("a system's updated_at precedes its created_at"));
        }
        for e in &self.channel_plan {
            e.validate()?;
        }
        for n in &self.neighbours {
            n.validate()?;
        }
        for tg in &self.talkgroups {
            tg.validate()?;
        }
        Ok(())
    }

    /// The newest channel-plan entry for an `iden`, if the table has one.
    pub fn iden(&self, iden: u8) -> Option<&ChannelPlanEntry> {
        self.channel_plan
            .iter()
            .filter(|e| e.iden == iden)
            .max_by_key(|e| e.t.as_unix_nanos())
    }
}

// ---------------------------------------------------------------------------------------------
// GrantEvent
// ---------------------------------------------------------------------------------------------

/// What a control-channel message said. The append-only stream underneath the call records, and
/// the input of the metadata-only load index (AWARE-067, T-273).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GrantKind {
    /// A voice or data channel was granted.
    Grant,
    /// An update repeating or amending a grant (used for late entry).
    GrantUpdate,
    /// The follower started a call on a granted channel.
    CallStart,
    /// The call ended (silence timeout or an explicit release).
    CallEnd,
    /// The system denied or queued the request.
    Denied,
    /// The grant's frequency fell outside the dwell window, so it could not be followed. Logged,
    /// never silently dropped (C23's ≤20 MHz span limit; T-269).
    OutsideWindow,
    /// The grant's channel number could not be mapped to a frequency (no IDEN, or a stale one).
    UnmappedChannel,
}

impl GrantKind {
    /// The stored text.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Grant => "grant",
            Self::GrantUpdate => "grant-update",
            Self::CallStart => "call-start",
            Self::CallEnd => "call-end",
            Self::Denied => "denied",
            Self::OutsideWindow => "outside-window",
            Self::UnmappedChannel => "unmapped-channel",
        }
    }

    /// Reads the stored text back.
    pub fn parse(text: &str) -> Result<Self, InvalidTrunking> {
        match text {
            "grant" => Ok(Self::Grant),
            "grant-update" => Ok(Self::GrantUpdate),
            "call-start" => Ok(Self::CallStart),
            "call-end" => Ok(Self::CallEnd),
            "denied" => Ok(Self::Denied),
            "outside-window" => Ok(Self::OutsideWindow),
            "unmapped-channel" => Ok(Self::UnmappedChannel),
            other => Err(bad(format!("{other} is not a grant event kind"))),
        }
    }
}

/// One control-channel event, with the encryption state **as that message stated it** — which for
/// a bare grant update is usually [`Encryption::Unknown`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrantEvent {
    /// The system whose control channel said it.
    pub system: TrunkSystemId,
    /// The call it belongs to, once one was opened.
    pub call: Option<CallRecordId>,
    /// What happened.
    pub kind: GrantKind,
    /// When.
    pub t: Timestamp,
    /// Talkgroup, when the message carried one.
    pub talkgroup: Option<String>,
    /// Unit (radio) id, when the message carried one.
    pub unit_id: Option<String>,
    /// Channel number or LCN as text, when the message carried one.
    pub channel: Option<String>,
    /// TDMA slot, when the message carried one.
    pub slot: Option<u8>,
    /// Resolved frequency, Hz. `None` when the channel could not be mapped.
    pub f_hz: Option<f64>,
    /// What this message said about encryption.
    pub encryption: Encryption,
    /// Machine detail (the raw opcode, the mapping arithmetic, why a channel was unmapped).
    pub detail: Value,
}

impl GrantEvent {
    /// A grant event that states nothing about encryption — the ordinary case, and the one the
    /// late-entry pitfall is about.
    pub fn new(system: TrunkSystemId, kind: GrantKind, t: Timestamp) -> Self {
        Self {
            system,
            call: None,
            kind,
            t,
            talkgroup: None,
            unit_id: None,
            channel: None,
            slot: None,
            f_hz: None,
            encryption: Encryption::Unknown,
            detail: Value::Null,
        }
    }

    /// Checks the event.
    pub fn validate(&self) -> Result<(), InvalidTrunking> {
        check_text("talkgroup", self.talkgroup.as_deref(), TRUNK_TEXT_MAX)?;
        check_text("unit id", self.unit_id.as_deref(), TRUNK_TEXT_MAX)?;
        check_text("channel", self.channel.as_deref(), TRUNK_TEXT_MAX)?;
        check_freq("grant frequency", self.f_hz)?;
        if self.slot.is_some_and(|s| s > MAX_SLOT) {
            return Err(bad(format!("slot must be 0..={MAX_SLOT}")));
        }
        self.encryption.validate()
    }
}

/// Highest TDMA slot index stored (P25 Phase 2 and DMR both use two; the range leaves room).
pub const MAX_SLOT: u8 = 3;

// ---------------------------------------------------------------------------------------------
// CallRecord
// ---------------------------------------------------------------------------------------------

/// One call as followed: **metadata only**. No audio, no vocoder, no content — see the module docs.
///
/// A call is an aggregate: it opens when a grant is followed and its `t_end` fills in when it ends,
/// and later evidence may sharpen `encryption` through [`Encryption::refine`] (never back towards
/// clear once encrypted; the repository and the schema both refuse that).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CallRecord {
    /// Row id.
    pub id: CallRecordId,
    /// The system it was heard on.
    pub system: TrunkSystemId,
    /// When the call started.
    pub t_start: Timestamp,
    /// When it ended. `None` while it is still open, or when its end was never observed.
    pub t_end: Option<Timestamp>,
    /// Talkgroup, when decoded.
    pub talkgroup: Option<String>,
    /// Unit (radio) id, when decoded.
    pub unit_id: Option<String>,
    /// Channel number or LCN as text, when known.
    pub channel: Option<String>,
    /// TDMA slot, when attributed.
    pub slot: Option<u8>,
    /// Voice-channel frequency, Hz, when the channel could be mapped.
    pub f_hz: Option<f64>,
    /// Encryption state: clear, encrypted, or **unknown**. Never defaulted.
    pub encryption: Encryption,
    /// Whether the call was joined **without seeing its header** (late entry). A late-entry call
    /// whose `encryption` is `Unknown` is the exact C23 pitfall case, and it stays unknown.
    pub late_entry: bool,
    /// The emission this call rode on, once the inventory has one for it.
    pub emitter_id: Option<EmitterId>,
    /// Machine reasons (`grant-outside-window`, `stale-iden`, `silence-timeout`…).
    pub reasons: Vec<String>,
}

impl CallRecord {
    /// Opens a call from a grant, **carrying the grant's encryption state verbatim**.
    ///
    /// This is the constructor the late-entry case goes through: a grant update with no header
    /// carries [`Encryption::Unknown`], and there is no branch here that turns that into `Clear`.
    pub fn from_grant(grant: &GrantEvent, late_entry: bool) -> Self {
        Self {
            id: CallRecordId::new(),
            system: grant.system,
            t_start: grant.t,
            t_end: None,
            talkgroup: grant.talkgroup.clone(),
            unit_id: grant.unit_id.clone(),
            channel: grant.channel.clone(),
            slot: grant.slot,
            f_hz: grant.f_hz,
            encryption: grant.encryption,
            late_entry,
            emitter_id: None,
            reasons: Vec::new(),
        }
    }

    /// Folds later evidence into the call's encryption state ([`Encryption::refine`]).
    pub fn refine_encryption(&mut self, later: Encryption) {
        self.encryption = self.encryption.refine(later);
    }

    /// Call duration, when the call has ended.
    pub fn duration_ns(&self) -> Option<i64> {
        self.t_end
            .map(|e| e.as_unix_nanos() - self.t_start.as_unix_nanos())
    }

    /// Checks the record.
    pub fn validate(&self) -> Result<(), InvalidTrunking> {
        check_text("talkgroup", self.talkgroup.as_deref(), TRUNK_TEXT_MAX)?;
        check_text("unit id", self.unit_id.as_deref(), TRUNK_TEXT_MAX)?;
        check_text("channel", self.channel.as_deref(), TRUNK_TEXT_MAX)?;
        check_freq("call frequency", self.f_hz)?;
        if self.slot.is_some_and(|s| s > MAX_SLOT) {
            return Err(bad(format!("slot must be 0..={MAX_SLOT}")));
        }
        if self
            .t_end
            .is_some_and(|e| e.as_unix_nanos() < self.t_start.as_unix_nanos())
        {
            return Err(bad("a call's end precedes its start"));
        }
        if self.reasons.len() > CALL_REASONS_MAX {
            return Err(bad(format!(
                "a call keeps at most {CALL_REASONS_MAX} reasons"
            )));
        }
        for r in &self.reasons {
            check_text("call reason", Some(r), TRUNK_LABEL_MAX)?;
        }
        self.encryption.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(sec: i64) -> Timestamp {
        Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + sec * 1_000_000_000)
    }

    #[test]
    fn a_late_entry_grant_produces_a_call_that_is_unknown_not_clear() {
        let system = TrunkSystemId::new();
        // A grant update seen mid-call: no header, so nothing said anything about encryption.
        let mut grant = GrantEvent::new(system, GrantKind::GrantUpdate, t(0));
        grant.talkgroup = Some("4242".into());
        grant.f_hz = Some(851.0125e6);
        assert_eq!(grant.encryption, Encryption::Unknown);

        let call = CallRecord::from_grant(&grant, true);
        assert!(call.late_entry);
        assert_eq!(call.encryption, Encryption::Unknown);
        assert_eq!(call.encryption.state(), "unknown");
        assert!(!call.encryption.is_clear(), "unknown is never clear");
        assert!(!call.encryption.is_known());
        assert_eq!(call.encryption.evidence(), None);
        call.validate().unwrap();
    }

    #[test]
    fn a_state_that_makes_a_claim_carries_its_evidence() {
        let clear = Encryption::from_algid(P25_ALGID_CLEAR);
        assert!(clear.is_clear());
        assert_eq!(clear.evidence(), Some(EncryptionEvidence::Algid));
        assert_eq!(clear.algid(), Some(0x80));

        for algid in [0x81u8, 0x84, 0xAA] {
            let enc = Encryption::from_algid(algid);
            assert!(enc.is_encrypted(), "{algid:#04x}");
            assert!(!enc.is_clear());
            assert_eq!(enc.algid(), Some(algid));
        }

        assert!(Encryption::from_service_options(true).is_encrypted());
        assert!(Encryption::from_service_options(false).is_clear());
        assert!(Encryption::from_dmr_pi(true).is_encrypted());
    }

    #[test]
    fn refining_never_walks_back_towards_clear() {
        let unknown = Encryption::Unknown;
        let clear = Encryption::from_algid(P25_ALGID_CLEAR);
        let encrypted = Encryption::from_algid(0x84);

        // Unknown gives way to anything measured...
        assert_eq!(unknown.refine(clear), clear);
        assert_eq!(unknown.refine(encrypted), encrypted);
        // ...but nothing gives way to Unknown, and encrypted never becomes clear.
        assert_eq!(clear.refine(unknown), clear);
        assert_eq!(encrypted.refine(unknown), encrypted);
        assert_eq!(encrypted.refine(clear), encrypted);
        // A clear call may still be escalated by a later encrypted header.
        assert_eq!(clear.refine(encrypted), encrypted);

        let mut call = CallRecord::from_grant(
            &GrantEvent::new(TrunkSystemId::new(), GrantKind::Grant, t(0)),
            true,
        );
        call.refine_encryption(encrypted);
        assert!(call.encryption.is_encrypted());
        call.refine_encryption(clear);
        assert!(call.encryption.is_encrypted(), "no downgrade to clear");
    }

    #[test]
    fn encryption_serde_round_trips_and_keeps_its_three_states_distinct() {
        for enc in [
            Encryption::Unknown,
            Encryption::from_algid(P25_ALGID_CLEAR),
            Encryption::Encrypted {
                evidence: EncryptionEvidence::DmrPi,
                algid: None,
                key_id: Some(0x1234),
            },
        ] {
            let json = serde_json::to_string(&enc).unwrap();
            assert_eq!(serde_json::from_str::<Encryption>(&json).unwrap(), enc);
        }
        let json = serde_json::to_value(Encryption::Unknown).unwrap();
        assert_eq!(json, serde_json::json!({"state": "unknown"}));
        assert!(
            serde_json::from_str::<Encryption>(r#"{"state":"clear"}"#).is_err(),
            "clear without evidence is not a representable value"
        );
    }

    #[test]
    fn a_contradictory_algid_is_refused() {
        let wrong = Encryption::Clear {
            evidence: EncryptionEvidence::Algid,
            algid: Some(0x84),
            key_id: None,
        };
        assert!(wrong.validate().is_err());
        let also_wrong = Encryption::Encrypted {
            evidence: EncryptionEvidence::Algid,
            algid: Some(P25_ALGID_CLEAR),
            key_id: None,
        };
        assert!(also_wrong.validate().is_err());
    }

    #[test]
    fn a_system_checks_itself_and_finds_the_newest_iden() {
        let mut s = TrunkSystem::new(TrunkProtocol::P25Phase1, Some(851.0125e6), t(0));
        s.validate().unwrap();
        assert_eq!(s.protocol.as_str(), "p25-phase1");
        assert!(!s.protocol.is_tdma());
        assert!(TrunkProtocol::P25Phase2.is_tdma());

        let old = ChannelPlanEntry {
            iden: 1,
            base_hz: 851.0e6,
            spacing_hz: 6250.0,
            tx_offset_hz: -45.0e6,
            bandwidth_hz: Some(12_500.0),
            slots: 1,
            t: t(1),
        };
        let new = ChannelPlanEntry {
            base_hz: 851.00625e6,
            t: t(100),
            ..old
        };
        s.channel_plan = vec![old, new];
        s.validate().unwrap();
        assert_eq!(s.iden(1), Some(&new));
        assert_eq!(s.iden(2), None);
        assert!((old.downlink_hz(4) - (851.0e6 + 25_000.0)).abs() < 1e-6);

        s.cc_freq_hz = Some(0.0);
        assert!(s.validate().is_err(), "0 Hz is not a control channel");
        s.cc_freq_hz = None; // a rest-channel system has no dedicated CC at all
        s.validate().unwrap();

        s.talkgroups = vec![Talkgroup {
            id: "4242".into(),
            label: Some("Fire dispatch".into()),
            label_source: None,
            first_seen: t(1),
            last_seen: t(2),
            calls: 3,
        }];
        assert!(s.validate().is_err(), "a label names its source");
    }

    #[test]
    fn a_neighbour_that_names_nothing_is_refused() {
        assert!(
            NeighbourSite {
                site_id: None,
                cc_freq_hz: None,
                t: t(0),
            }
            .validate()
            .is_err()
        );
        NeighbourSite {
            site_id: Some("2:7".into()),
            cc_freq_hz: None,
            t: t(0),
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn protocol_text_round_trips_and_an_unknown_string_is_an_error() {
        for p in [
            TrunkProtocol::P25Phase1,
            TrunkProtocol::P25Phase2,
            TrunkProtocol::DmrTier3,
            TrunkProtocol::SmartNet,
            TrunkProtocol::Edacs,
            TrunkProtocol::NxdnTypeC,
            TrunkProtocol::Mpt1327,
            TrunkProtocol::Unknown,
        ] {
            assert_eq!(TrunkProtocol::parse(p.as_str()).unwrap(), p);
        }
        assert!(TrunkProtocol::parse("tetra").is_err());
        assert!(GrantKind::parse("nonsense").is_err());
        assert!(EncryptionEvidence::parse("vibes").is_err());
        assert!(LabelSource::parse("guess").is_err());
    }

    #[test]
    fn a_call_checks_its_own_shape() {
        let g = GrantEvent::new(TrunkSystemId::new(), GrantKind::Grant, t(10));
        let mut c = CallRecord::from_grant(&g, false);
        c.validate().unwrap();
        assert_eq!(c.duration_ns(), None);

        c.t_end = Some(t(5));
        assert!(c.validate().is_err(), "an end before the start");
        c.t_end = Some(t(13));
        c.validate().unwrap();
        assert_eq!(c.duration_ns(), Some(3_000_000_000));

        c.slot = Some(9);
        assert!(c.validate().is_err());
        c.slot = Some(1);
        c.talkgroup = Some("   ".into());
        assert!(c.validate().is_err(), "present but blank");
    }
}
