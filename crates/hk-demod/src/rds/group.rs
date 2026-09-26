//! RDS group assembly and parsing (EN 50067 §3.1): bits → blocks → groups → PI, PS, PTY, TP/TA,
//! with block and group error statistics.
//!
//! - **PI** comes from block A, and also from block C' in version-B groups. A group whose two PI
//!   copies disagree, or whose block-3 offset (C vs C') contradicts the version bit, is rejected
//!   as a bad synchronisation. The station PI is a vote over CRC-valid PI blocks and is only
//!   reported with at least `pi_min_votes` votes and a `pi_min_share` majority.
//! - **A reported PI is not yet a committed identity (T-962).** Reporting and *committing* are
//!   two bars, and [`PiDecision::provisional`] is the gap between them: below
//!   `pi_commit_votes` agreeing CRC-valid PI blocks **within `hk_model::RDS_PI_COMMIT_WINDOW_NS`
//!   (5 s) of stream time** the PI is reported **provisionally** — good
//!   enough to show ("PI 1704, 3 groups, provisional"), not good enough to write as a
//!   transmitter identity or to rest a lifecycle change on. See [`GroupConfig::pi_commit_votes`]
//!   for the bound and why it is a vote count rather than ADR-0022's bits budget.
//! - **PS** is sent in 4 two-character segments by groups 0A/0B. A frame completes when
//!   segments 0, 1, 2, 3 arrive in order (repeats allowed), under one PI, each within
//!   `ps_max_gap_s` of the previous. Stations scroll PS (song/artist text), so PS is the list of
//!   complete frames, never one static string; the label is the most frequent frame.
//! - **Error rates:** blocks evaluated at their lattice position while synchronised; a group is
//!   in error when any of its 4 blocks is.
//!
//! **The accumulated field view (T-971).** A followed station is decoded for minutes, not for one
//! window, so the report is the station's fields as they stand, not a packet list:
//!
//! - **PS** is stable until changed: [`RdsReport::ps_current`] is the latest complete frame, and a
//!   dynamic (scrolling) PS is kept as [`RdsReport::ps_sequence`], the order its distinct frames
//!   were sent in.
//! - **RadioText** (groups 2A/2B, EN 50067 §3.1.5.3) is assembled per character address under one
//!   PI and one A/B flag: a toggled flag, or a character that differs from the one held at its
//!   address, starts a new message. A message is complete when every address before its carriage
//!   return (0x0D), or all 64 (2A) / 32 (2B), has arrived CRC-valid.
//! - **AF** (group 0A block C, method A codes 1–204) is the set of alternative frequencies heard.
//! - **CT** (group 4A) is the latest clock time and date, with the local offset.
//!
//! Everything a follow can grow is bounded ([`FRAME_LOG_CAP`], [`PS_DISTINCT_CAP`],
//! [`PS_SEQUENCE_CAP`], [`RT_LOG_CAP`], [`AF_CAP`]), so hours of one station cost the same memory
//! as a minute.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use hk_model::{RDS_PI_COMMIT_WINDOW_NS, VoteWindow};
use serde::{Deserialize, Serialize};

use super::block::{BlockEvent, BlockSync, Offset, SyncConfig};

/// RDS bit rate, Bd (57 kHz / 48 = pilot / 16).
pub const RDS_BITRATE_BD: f64 = 1187.5;

/// T-971: most complete PS frames kept in [`RdsReport::frame_log`] (the newest). A 4 s window
/// holds a handful; a followed station sends one every ~0.35 s for as long as it is followed.
pub const FRAME_LOG_CAP: usize = 256;
/// T-971: most distinct PS texts counted in [`RdsReport::ps_frames`]. A scrolling PS sends many;
/// past the cap the least-sent (oldest among ties) is forgotten.
pub const PS_DISTINCT_CAP: usize = 64;
/// T-971: most entries kept in [`RdsReport::ps_sequence`] (the newest).
pub const PS_SEQUENCE_CAP: usize = 32;
/// T-971: most complete RadioText messages kept in [`RdsReport::rt_messages`] (the newest).
pub const RT_LOG_CAP: usize = 32;
/// T-971: most alternative frequencies kept (EN 50067 method A lists at most 25).
pub const AF_CAP: usize = 25;

/// Group-level decoder settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GroupConfig {
    /// Block synchronisation.
    pub sync: SyncConfig,
    /// CRC-valid PI blocks needed before a PI is reported.
    pub pi_min_votes: u32,
    /// Share of all PI votes the reported PI must hold.
    pub pi_min_share: f64,
    /// **T-962: agreeing CRC-valid PI blocks needed before the PI is a *committed* identity**
    /// rather than a provisional reading. Default [`hk_model::RDS_PI_COMMIT_VOTES`] (10) — the
    /// one bar every RDS producer shares, the `rds` recipe's `messages` outputs included. The
    /// record writer (`crate::record`) also refuses an identity below that constant, so a
    /// config set lower here cannot weaken it.
    ///
    /// **Which bound applies, and the citation.** ADR-0022 §6's `analytic_holdout_bits` budget
    /// governs `ConfirmPolicy.synthesized` — the confirm route for a *synthesized* pipeline whose
    /// check stage was discovered by a search, where the engine can price the look-elsewhere it
    /// spent (§5.1–§5.2). RDS takes none of that path: it is a shipped, template-fixed decoder
    /// confirming through `ConfirmPolicy`'s route A (decoded identity), and no `analytic_holdout_
    /// bits` is computed for it. So the bound here is the ticket's other option — **N agreeing
    /// CRC-valid groups** — and ADR-0022 §1.3 is why it exists at all: a confirm is a lifecycle
    /// change and no rule demotes, so the confirm gate binds, always.
    ///
    /// **Why a count and not a bits sum.** The naive arithmetic says three agreeing blocks are
    /// overwhelming — each block passes a 10-bit check (`L_check = 0`, the generator is in the
    /// standard), and each block after the first must also agree on a 16-bit PI, so three
    /// independent blocks are 3 × 10 + 2 × 16 = 62 bits against ADR-0022's 24. That sum assumes
    /// the draws are **independent**, and a mis-synchronised block lattice is exactly where they
    /// are not: one wrong lock re-reads correlated bits, so the same wrong PI can repeat without
    /// costing 16 bits a time. ADR-0022 §5.3 answers that case empirically, with a shuffled-null
    /// control this decoder does not run. Absent the control, the honest guard is the rate a real
    /// station transmits at.
    ///
    /// **Why 10.** A synchronised RDS stream carries one PI-bearing block per group at
    /// 1187.5 Bd / 104 bits = **11.4 groups/s**, so 10 agreeing votes is **0.9 s of genuine
    /// lock** — and still under two seconds at a 50 % block error rate. The observed false commit
    /// (T-962: 98.085 MHz, PI 1704, an independent oracle finding no RDS at all on the same clip)
    /// reached ~3 votes in **45 s**, two orders of magnitude off that rate. The bound is set by
    /// what a chance lock cannot reach, not by what today's caller happens to supply.
    ///
    /// **Measured on both sides, not assumed.** The bound is chosen to sit in a gap that was
    /// measured rather than argued:
    ///
    /// | Scene | Groups | Votes | Block errors |
    /// |---|---|---|---|
    /// | Real 101.3 MHz HackRF capture, 4.8 s (`hk-demod::signal_062_real`) | 55 | **52** | 13/222 |
    /// | Dense synthetic scene, 3 stations 400 kHz apart (`signal_062_dense_fm_pipeline`) | 3 | **3** | 0/15 |
    /// | The T-962 false commit: 98.085 MHz, PI 1704, 45 s, oracle saw no RDS | — | **~3** | — |
    ///
    /// A real station on real air clears 10 by a factor of five, so the bound costs the case it
    /// must not break nothing at all. The two 3-vote cases fall below it — and the important
    /// point is that **the true one and the false one are indistinguishable by count**, which is
    /// exactly why the answer is a provisional state rather than a cleverer threshold: the dense
    /// scene's PI is still decoded, recorded and shown with its vote, it is simply not yet an
    /// identity. An FM station whose RDS stays provisional still confirms on `ConfirmPolicy`'s
    /// route C (a verified emission — the pilot lock), which needs no RDS at all, so no
    /// confirmation is lost anywhere; only the identity claim waits for evidence.
    ///
    /// Raising it is safe; lowering it is the one-way door. Below the bound nothing is lost — the
    /// PI is still reported, still shown, still in the decode row's metadata with its vote count.
    pub pi_commit_votes: u32,
    /// Longest gap between consecutive PS segments of one frame, s.
    pub ps_max_gap_s: f64,
}

impl Default for GroupConfig {
    fn default() -> Self {
        Self {
            sync: SyncConfig::default(),
            pi_min_votes: 3,
            pi_min_share: 0.6,
            pi_commit_votes: hk_model::RDS_PI_COMMIT_VOTES,
            ps_max_gap_s: 2.0,
        }
    }
}

/// One parsed group.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RdsGroup {
    /// Stream position of the group's first bit (the caller's time unit, e.g. MPX samples).
    pub position: f64,
    /// PI, if a PI block was CRC-valid.
    pub pi: Option<u16>,
    /// Group type 0–15 and version (`true` = B), if block B was valid.
    pub group_type: Option<(u8, bool)>,
    /// Programme type.
    pub pty: Option<u8>,
    /// Traffic programme flag.
    pub tp: Option<bool>,
    /// Traffic announcement flag (0A/0B).
    pub ta: Option<bool>,
    /// PS segment address and its two characters (0A/0B with a valid block D).
    pub ps_segment: Option<(u8, [u8; 2])>,
    /// Syndrome check per block.
    pub blocks_ok: [bool; 4],
}

/// One complete PS frame.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PsFrame {
    /// Stream position of segment 0's first bit.
    pub position: f64,
    /// PI the four segments were sent under.
    pub pi: u16,
    /// The 8 characters (EN 50067 Annex E codes 0x20–0x7E map to ASCII; others to Latin-1).
    pub text: String,
}

/// One complete RadioText message (T-971).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RtMessage {
    /// Stream position of the group that completed it.
    pub position: f64,
    /// PI it was sent under.
    pub pi: u16,
    /// The text A/B flag it was sent with.
    pub ab: bool,
    /// The text, up to its carriage return, trailing spaces removed.
    pub text: String,
}

/// Clock time and date from a 4A group (T-971).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RdsClockTime {
    /// Stream position of the group.
    pub position: f64,
    /// Modified Julian Day (UTC).
    pub mjd: u32,
    /// UTC hour.
    pub hour: u8,
    /// UTC minute.
    pub minute: u8,
    /// Local time offset from UTC, in half hours.
    pub offset_half_hours: i8,
    /// The UTC instant as Unix seconds.
    pub utc_unix_s: i64,
}

/// The reported station PI, its vote, and whether that vote has cleared the commit bound.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PiDecision {
    /// Programme identification code.
    pub pi: u16,
    /// CRC-valid PI blocks carrying it.
    pub votes: u32,
    /// All CRC-valid PI blocks.
    pub total_votes: u32,
    /// `votes / total_votes`.
    pub share: f64,
    /// **T-962 (round 2):** the most of those votes that fell within one
    /// [`hk_model::RDS_PI_COMMIT_WINDOW_NS`] span of stream time, counted up to
    /// `pi_commit_votes` ([`hk_model::VoteWindow`]). The bar is a rate: a PI commits only when
    /// this reaches `pi_commit_votes`, so a long session of sparse chance agreements stays
    /// provisional however many votes it accumulates.
    #[serde(default)]
    pub window_votes: u32,
    /// **T-962: the vote has not reached [`GroupConfig::pi_commit_votes`] within one
    /// [`hk_model::RDS_PI_COMMIT_WINDOW_NS`] span of stream time** ([`Self::window_votes`]).
    ///
    /// A provisional PI is a reading, not an identity: show it with its vote count, do not write
    /// it as a transmitter identity and do not rest a lifecycle change on it. `hk_demod::record`
    /// is the enforcement point for the first two; `ConfirmPolicy`'s route A never sees a
    /// provisional PI because no identity is written for one.
    pub provisional: bool,
}

impl PiDecision {
    /// Uppercase 4-digit hex, e.g. `C0DE`.
    pub fn hex(&self) -> String {
        format!("{:04X}", self.pi)
    }

    /// The vote cleared [`GroupConfig::pi_commit_votes`]: this PI may be written as an identity.
    pub fn committed(&self) -> bool {
        !self.provisional
    }
}

/// Why no PI is reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PiAbstain {
    /// No CRC-valid PI block.
    NoValidBlocks,
    /// Fewer than `pi_min_votes` agreeing blocks.
    TooFewVotes,
    /// The leading PI does not hold `pi_min_share`.
    NoMajority,
}

/// Decoder statistics and results.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RdsReport {
    /// Accepted PI.
    pub pi: Option<PiDecision>,
    /// Why no PI, when `pi` is `None`.
    pub pi_abstain: Option<PiAbstain>,
    /// All PI votes, hex → count.
    pub pi_votes: BTreeMap<String, u32>,
    /// Complete PS frames, most frequent first (ties: first seen first); at most
    /// [`PS_DISTINCT_CAP`] texts.
    pub ps_frames: Vec<(String, u32)>,
    /// Complete PS frames in order: every one of a window, the newest [`FRAME_LOG_CAP`] of a
    /// follow.
    pub frame_log: Vec<PsFrame>,
    /// T-971: the latest complete PS frame — the station's PS as it stands, stable until a
    /// different complete frame arrives.
    #[serde(default)]
    pub ps_current: Option<String>,
    /// T-971: distinct consecutive PS frames in the order sent (a dynamic PS's sequence), the
    /// newest [`PS_SEQUENCE_CAP`].
    #[serde(default)]
    pub ps_sequence: Vec<String>,
    /// T-971: more than one distinct PS text has been sent (a scrolling / dynamic PS).
    #[serde(default)]
    pub ps_dynamic: bool,
    /// T-971: the latest complete RadioText message.
    #[serde(default)]
    pub rt: Option<String>,
    /// T-971: the A/B flag of [`Self::rt`].
    #[serde(default)]
    pub rt_ab: Option<bool>,
    /// T-971: distinct consecutive complete RadioText messages, the newest [`RT_LOG_CAP`].
    #[serde(default)]
    pub rt_messages: Vec<RtMessage>,
    /// T-971: alternative frequencies heard (0A, method A), MHz, ascending.
    #[serde(default)]
    pub af_mhz: Vec<f64>,
    /// T-971: the latest clock time (4A).
    #[serde(default)]
    pub ct: Option<RdsClockTime>,
    /// Most frequent PTY among valid B blocks.
    pub pty: Option<u8>,
    /// Most frequent TP flag.
    pub tp: Option<bool>,
    /// Most frequent TA flag (0A/0B).
    pub ta: Option<bool>,
    /// Group type counts, e.g. `0A` → 11.
    pub group_types: BTreeMap<String, u32>,
    /// Bits received.
    pub bits: u64,
    /// Blocks evaluated while synchronised.
    pub blocks_total: u64,
    /// Of which CRC-valid.
    pub blocks_ok: u64,
    /// `1 − blocks_ok / blocks_total`.
    pub block_error_rate: Option<f64>,
    /// Complete groups (all 4 slots evaluated).
    pub groups_total: u64,
    /// Of which all 4 blocks valid.
    pub groups_ok: u64,
    /// `1 − groups_ok / groups_total`.
    pub group_error_rate: Option<f64>,
    /// Groups rejected as inconsistent (C/C' vs version, PI copies disagree).
    pub groups_rejected: u64,
    /// Synchronisation acquisitions.
    pub sync_acquisitions: u32,
    /// Synchronisation losses.
    pub sync_losses: u32,
    /// One-bit slips absorbed.
    pub bit_slips: u32,
}

impl RdsReport {
    /// The most frequent complete PS frame.
    pub fn ps(&self) -> Option<&str> {
        self.ps_frames.first().map(|(s, _)| s.as_str())
    }
}

#[derive(Clone, Debug, Default)]
struct PsAssembler {
    buf: Vec<[u8; 2]>,
    start: f64,
    pi: u16,
    last_seg: u8,
    last_pos: f64,
}

/// RadioText characters by address under one PI, one A/B flag and one group version (T-971).
#[derive(Clone, Debug)]
struct RtAssembler {
    pi: u16,
    ab: bool,
    version_b: bool,
    chars: [Option<u8>; 64],
}

impl RtAssembler {
    fn new(pi: u16, ab: bool, version_b: bool) -> Self {
        Self {
            pi,
            ab,
            version_b,
            chars: [None; 64],
        }
    }

    /// Addresses a message of this version can hold.
    fn capacity(&self) -> usize {
        if self.version_b { 32 } else { 64 }
    }

    /// The complete message: every address before the first carriage return (or all of them)
    /// has arrived. `None` while one is missing.
    fn complete(&self) -> Option<String> {
        let mut text = String::new();
        for c in &self.chars[..self.capacity()] {
            match *c {
                None => return None,
                Some(0x0D) => break,
                Some(c) => text.push(rds_char(c)),
            }
        }
        Some(text.trim_end().to_owned())
    }
}

/// An RDS character as text: EN 50067 Annex E codes 0x20–0x7E are ASCII, others are read as
/// Latin-1 (as PS frames always were).
fn rds_char(c: u8) -> char {
    char::from(c)
}

/// Distinct PS texts and how often each was sent, bounded (T-971).
#[derive(Clone, Debug, Default)]
struct PsCounts {
    /// `(text, count, first-seen sequence number)`.
    entries: Vec<(String, u32, u64)>,
    seq: u64,
}

impl PsCounts {
    fn add(&mut self, text: &str) {
        self.seq += 1;
        if let Some(e) = self.entries.iter_mut().find(|e| e.0 == text) {
            e.1 += 1;
            return;
        }
        if self.entries.len() >= PS_DISTINCT_CAP
            && let Some(i) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| (e.1, e.2))
                .map(|(i, _)| i)
        {
            self.entries.swap_remove(i);
        }
        self.entries.push((text.to_owned(), 1, self.seq));
    }

    /// Most frequent first, ties first seen first.
    fn ranked(&self) -> Vec<(String, u32)> {
        let mut v = self.entries.clone();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));
        v.into_iter().map(|(t, n, _)| (t, n)).collect()
    }
}

/// Streaming RDS group decoder.
#[derive(Clone, Debug)]
pub struct RdsDecoder {
    config: GroupConfig,
    bits_per_unit: f64,
    sync: BlockSync,
    slots: [Option<BlockEvent>; 4],
    positions: VecDeque<f64>,
    group_start: Option<f64>,
    groups: Vec<RdsGroup>,
    ps: PsAssembler,
    /// Complete PS frames, the newest [`FRAME_LOG_CAP`].
    frames: VecDeque<PsFrame>,
    ps_counts: PsCounts,
    ps_sequence: VecDeque<String>,
    rt: Option<RtAssembler>,
    rt_log: VecDeque<RtMessage>,
    af: BTreeSet<u8>,
    ct: Option<RdsClockTime>,
    pi_votes: BTreeMap<u16, u32>,
    /// T-962: each PI's votes over stream time, the windowed commit rule.
    pi_windows: BTreeMap<u16, VoteWindow>,
    pty: BTreeMap<u8, u32>,
    tp: [u32; 2],
    ta: [u32; 2],
    group_types: BTreeMap<String, u32>,
    blocks_total: u64,
    blocks_ok: u64,
    groups_total: u64,
    groups_ok: u64,
    groups_rejected: u64,
    acquisitions: u32,
    losses: u32,
    slips: u32,
}

impl RdsDecoder {
    /// A decoder whose bit positions are in a unit with `units_per_second` (e.g. the MPX rate)
    /// so `ps_max_gap_s` can be applied.
    pub fn new(config: GroupConfig, units_per_second: f64) -> Self {
        Self {
            config,
            bits_per_unit: RDS_BITRATE_BD / units_per_second,
            sync: BlockSync::new(config.sync),
            slots: [None; 4],
            positions: VecDeque::with_capacity(64),
            group_start: None,
            groups: Vec::new(),
            ps: PsAssembler::default(),
            frames: VecDeque::new(),
            ps_counts: PsCounts::default(),
            ps_sequence: VecDeque::new(),
            rt: None,
            rt_log: VecDeque::new(),
            af: BTreeSet::new(),
            ct: None,
            pi_votes: BTreeMap::new(),
            pi_windows: BTreeMap::new(),
            pty: BTreeMap::new(),
            tp: [0; 2],
            ta: [0; 2],
            group_types: BTreeMap::new(),
            blocks_total: 0,
            blocks_ok: 0,
            groups_total: 0,
            groups_ok: 0,
            groups_rejected: 0,
            acquisitions: 0,
            losses: 0,
            slips: 0,
        }
    }

    /// Pushes one decoded bit whose symbol started at `position`.
    pub fn push_bit(&mut self, bit: u8, position: f64) {
        if self.positions.len() == 64 {
            self.positions.pop_front();
        }
        self.positions.push_back(position);
        let Some(ev) = self.sync.push(bit) else {
            return;
        };
        if ev.acquired {
            self.acquisitions += 1;
            self.slots = [None; 4];
            self.group_start = None;
        }
        self.blocks_total += 1;
        if ev.ok() {
            self.blocks_ok += 1;
        }
        if ev.slip != 0 {
            self.slips += 1;
        }
        if ev.slot == 0 {
            self.slots = [None; 4];
            self.group_start = Some(self.position_of(ev.last_bit, 25));
        }
        self.slots[ev.slot] = Some(ev);
        if ev.slot == 3 {
            self.finish_group();
        }
        if ev.lost {
            self.losses += 1;
            self.slots = [None; 4];
            self.group_start = None;
        }
    }

    /// T-971: groups with all four blocks CRC-valid so far.
    pub fn groups_ok(&self) -> u64 {
        self.groups_ok
    }

    /// Groups parsed so far (drains the buffer).
    pub fn take_groups(&mut self) -> Vec<RdsGroup> {
        std::mem::take(&mut self.groups)
    }

    fn position_of(&self, last_bit: u64, back: u64) -> f64 {
        let newest = self.sync.bits() - 1;
        let age = (newest - last_bit + back) as usize;
        let n = self.positions.len();
        if age < n {
            self.positions[n - 1 - age]
        } else {
            // Older than the window: extrapolate from the oldest kept position.
            let oldest = self.positions.front().copied().unwrap_or(0.0);
            oldest - (age + 1 - n) as f64 / self.bits_per_unit
        }
    }

    fn finish_group(&mut self) {
        let slots = std::mem::take(&mut self.slots);
        let Some(position) = self.group_start.take() else {
            return;
        };
        if slots.iter().any(Option::is_none) {
            return;
        }
        let s = slots.map(|e| e.expect("checked"));
        self.groups_total += 1;
        let blocks_ok = s.map(|e| e.ok());
        if blocks_ok.iter().all(|&ok| ok) {
            self.groups_ok += 1;
        }
        let valid = |i: usize| s[i].offset.map(|_| s[i].info);
        let b = valid(1);
        let version_b = b.map(|b| (b >> 11) & 1 == 1);
        let block3_prime = s[2].offset.map(|o| o == Offset::CPrime);
        if let (Some(vb), Some(cp)) = (version_b, block3_prime)
            && vb != cp
        {
            self.groups_rejected += 1;
            return;
        }
        let pi_a = valid(0);
        let pi_c = (block3_prime == Some(true)).then(|| s[2].info);
        let pi = match (pi_a, pi_c) {
            (Some(a), Some(c)) if a != c => {
                self.groups_rejected += 1;
                return;
            }
            (Some(a), _) => Some(a),
            (None, c) => c,
        };
        let mut group = RdsGroup {
            position,
            pi,
            group_type: None,
            pty: None,
            tp: None,
            ta: None,
            ps_segment: None,
            blocks_ok,
        };
        // Stream time of the group, ns (positions are in units of `RDS_BITRATE_BD /
        // bits_per_unit` per second): the vote's capture time, never the wall clock.
        let t_ns = (position * self.bits_per_unit / RDS_BITRATE_BD * 1e9) as i64;
        for p in [pi_a, pi_c].into_iter().flatten() {
            *self.pi_votes.entry(p).or_default() += 1;
            self.pi_windows.entry(p).or_default().vote(
                t_ns,
                self.config.pi_commit_votes,
                RDS_PI_COMMIT_WINDOW_NS,
            );
        }
        if let Some(b) = b {
            let gtype = (b >> 12) as u8;
            let vb = (b >> 11) & 1 == 1;
            group.group_type = Some((gtype, vb));
            group.tp = Some((b >> 10) & 1 == 1);
            group.pty = Some(((b >> 5) & 0x1F) as u8);
            *self
                .group_types
                .entry(format!("{gtype}{}", if vb { 'B' } else { 'A' }))
                .or_default() += 1;
            *self.pty.entry(((b >> 5) & 0x1F) as u8).or_default() += 1;
            self.tp[usize::from((b >> 10) & 1 == 1)] += 1;
            if gtype == 0 {
                let ta = (b >> 4) & 1 == 1;
                group.ta = Some(ta);
                self.ta[usize::from(ta)] += 1;
                if let (Some(d), Some(pi)) = (valid(3), pi) {
                    let seg = (b & 3) as u8;
                    let chars = [(d >> 8) as u8, d as u8];
                    group.ps_segment = Some((seg, chars));
                    self.push_ps(seg, chars, pi, position);
                }
                // T-971: 0A block C carries two alternative-frequency codes (method A).
                if let (false, Some(c)) = (vb, valid(2)) {
                    self.push_af(c);
                }
            }
            if let (2, Some(pi)) = (gtype, pi) {
                // T-971: RadioText. 2A: four characters at 4·addr (blocks C and D); 2B: two at
                // 2·addr (block D; block C' is the PI).
                let addr = usize::from(b & 0xF);
                let ab = (b >> 4) & 1 == 1;
                let mut chars: Vec<(usize, u8)> = Vec::with_capacity(4);
                if !vb && let Some(c) = valid(2) {
                    chars.extend([(4 * addr, (c >> 8) as u8), (4 * addr + 1, c as u8)]);
                }
                if let Some(d) = valid(3) {
                    let at = if vb { 2 * addr } else { 4 * addr + 2 };
                    chars.extend([(at, (d >> 8) as u8), (at + 1, d as u8)]);
                }
                self.push_rt(pi, ab, vb, &chars, position);
            }
            if let (4, false, Some(c), Some(d)) = (gtype, vb, valid(2), valid(3)) {
                self.push_ct(b, c, d, position);
            }
        }
        self.groups.push(group);
    }

    /// T-971: an AF code pair from 0A block C. Codes 1–204 are 87.6–107.9 MHz; the count
    /// indicators (224–249), the filler (205) and the LF/MF follower (250) are not frequencies.
    fn push_af(&mut self, c: u16) {
        for code in [(c >> 8) as u8, c as u8] {
            if (1..=204).contains(&code) && (self.af.len() < AF_CAP || self.af.contains(&code)) {
                self.af.insert(code);
            }
        }
    }

    /// T-971: RadioText characters (address, code) from one group. See the module docs.
    fn push_rt(&mut self, pi: u16, ab: bool, version_b: bool, chars: &[(usize, u8)], pos: f64) {
        let rt = self
            .rt
            .get_or_insert_with(|| RtAssembler::new(pi, ab, version_b));
        if rt.pi != pi || rt.ab != ab || rt.version_b != version_b {
            *rt = RtAssembler::new(pi, ab, version_b);
        }
        let cap = rt.capacity();
        for &(at, ch) in chars.iter().filter(|(at, _)| *at < cap) {
            // A character that differs from the one held at its address is a new message the
            // station sent without toggling A/B.
            if rt.chars[at].is_some_and(|held| held != ch) {
                rt.chars = [None; 64];
            }
            rt.chars[at] = Some(ch);
        }
        let Some(text) = rt.complete() else {
            return;
        };
        let repeat = self
            .rt_log
            .back()
            .is_some_and(|m| m.pi == pi && m.ab == ab && m.text == text);
        if !repeat {
            if self.rt_log.len() == RT_LOG_CAP {
                self.rt_log.pop_front();
            }
            self.rt_log.push_back(RtMessage {
                position: pos,
                pi,
                ab,
                text,
            });
        }
    }

    /// T-971: a 4A clock-time group (EN 50067 §3.1.5.6). An out-of-range field is a bad group
    /// that passed its checks, and is dropped.
    fn push_ct(&mut self, b: u16, c: u16, d: u16, position: f64) {
        let mjd = (u32::from(b & 3) << 15) | u32::from(c >> 1);
        let hour = (((c & 1) << 4) | (d >> 12)) as u8;
        let minute = ((d >> 6) & 0x3F) as u8;
        let half_hours = (d & 0x1F) as i8;
        // MJD 40587 is 1970-01-01.
        if hour > 23 || minute > 59 || half_hours > 28 || mjd < 40_587 {
            return;
        }
        let offset_half_hours = if (d >> 5) & 1 == 1 {
            -half_hours
        } else {
            half_hours
        };
        self.ct = Some(RdsClockTime {
            position,
            mjd,
            hour,
            minute,
            offset_half_hours,
            utc_unix_s: (i64::from(mjd) - 40_587) * 86_400
                + i64::from(hour) * 3600
                + i64::from(minute) * 60,
        });
    }

    /// A complete PS frame: logged (bounded), counted, and sequenced (T-971).
    fn push_frame(&mut self, frame: PsFrame) {
        self.ps_counts.add(&frame.text);
        if self.ps_sequence.back() != Some(&frame.text) {
            if self.ps_sequence.len() == PS_SEQUENCE_CAP {
                self.ps_sequence.pop_front();
            }
            self.ps_sequence.push_back(frame.text.clone());
        }
        if self.frames.len() == FRAME_LOG_CAP {
            self.frames.pop_front();
        }
        self.frames.push_back(frame);
    }

    fn push_ps(&mut self, seg: u8, chars: [u8; 2], pi: u16, position: f64) {
        let max_gap = self.config.ps_max_gap_s / self.bits_per_unit * RDS_BITRATE_BD;
        let ps = &mut self.ps;
        let continues = !ps.buf.is_empty() && pi == ps.pi && position - ps.last_pos <= max_gap;
        if seg == 0 && !(continues && ps.last_seg == 0 && ps.buf[0] == chars) {
            *ps = PsAssembler {
                buf: vec![chars],
                start: position,
                pi,
                last_seg: 0,
                last_pos: position,
            };
            return;
        }
        if continues && seg == ps.last_seg && ps.buf[seg as usize] == chars {
            ps.last_pos = position; // repeated segment
            return;
        }
        if continues && seg == ps.last_seg + 1 {
            ps.buf.push(chars);
            ps.last_seg = seg;
            ps.last_pos = position;
            if seg == 3 {
                let text: String = ps.buf.iter().flatten().map(|&c| rds_char(c)).collect();
                let start = ps.start;
                *ps = PsAssembler::default();
                self.push_frame(PsFrame {
                    position: start,
                    pi,
                    text,
                });
            }
            return;
        }
        *ps = PsAssembler::default();
    }

    /// Current results and statistics.
    pub fn report(&self) -> RdsReport {
        let total_votes: u32 = self.pi_votes.values().sum();
        let lead = self
            .pi_votes
            .iter()
            .max_by_key(|(pi, v)| (**v, std::cmp::Reverse(**pi)))
            .map(|(p, v)| (*p, *v));
        let (pi, pi_abstain) = match lead {
            None => (None, Some(PiAbstain::NoValidBlocks)),
            Some((_, v)) if v < self.config.pi_min_votes => (None, Some(PiAbstain::TooFewVotes)),
            Some((_, v)) if f64::from(v) < self.config.pi_min_share * f64::from(total_votes) => {
                (None, Some(PiAbstain::NoMajority))
            }
            Some((p, v)) => {
                let window = self.pi_windows.get(&p);
                let window_votes = window.map_or(0, VoteWindow::window_votes);
                let committed = window.is_some_and(|w| w.committed(self.config.pi_commit_votes));
                (
                    Some(PiDecision {
                        pi: p,
                        votes: v,
                        total_votes,
                        share: f64::from(v) / f64::from(total_votes),
                        window_votes,
                        // T-962: reported from `pi_min_votes`, committed only once
                        // `pi_commit_votes` of them fell within `RDS_PI_COMMIT_WINDOW_NS` of
                        // stream time. The gap is the provisional state.
                        provisional: !committed,
                    }),
                    None,
                )
            }
        };
        let flag = |c: [u32; 2]| (c[0] + c[1] > 0).then_some(c[1] > c[0]);
        let rate = |ok: u64, total: u64| (total > 0).then(|| 1.0 - ok as f64 / total as f64);
        RdsReport {
            pi,
            pi_abstain,
            pi_votes: self
                .pi_votes
                .iter()
                .map(|(p, v)| (format!("{p:04X}"), *v))
                .collect(),
            ps_frames: self.ps_counts.ranked(),
            frame_log: self.frames.iter().cloned().collect(),
            ps_current: self.frames.back().map(|f| f.text.clone()),
            ps_sequence: self.ps_sequence.iter().cloned().collect(),
            ps_dynamic: self.ps_counts.entries.len() > 1,
            rt: self.rt_log.back().map(|m| m.text.clone()),
            rt_ab: self.rt_log.back().map(|m| m.ab),
            rt_messages: self.rt_log.iter().cloned().collect(),
            af_mhz: self
                .af
                .iter()
                .map(|&code| (875.0 + f64::from(code)) / 10.0)
                .collect(),
            ct: self.ct,
            pty: self.pty.iter().max_by_key(|(_, v)| **v).map(|(p, _)| *p),
            tp: flag(self.tp),
            ta: flag(self.ta),
            group_types: self.group_types.clone(),
            bits: self.sync.bits(),
            blocks_total: self.blocks_total,
            blocks_ok: self.blocks_ok,
            block_error_rate: rate(self.blocks_ok, self.blocks_total),
            groups_total: self.groups_total,
            groups_ok: self.groups_ok,
            group_error_rate: rate(self.groups_ok, self.groups_total),
            groups_rejected: self.groups_rejected,
            sync_acquisitions: self.acquisitions,
            sync_losses: self.losses,
            bit_slips: self.slips,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::block::encode_block;
    use super::*;
    use hk_dsp::synth::Rng;

    /// Bits of a 0A group carrying PS segment `seg` of `ps`.
    pub(crate) fn group_0a_bits(pi: u16, ps: &[u8; 8], seg: usize, pty: u16, tp: bool) -> Vec<u8> {
        let b2 = (u16::from(tp) << 10) | (pty << 5) | seg as u16;
        let b4 = (u16::from(ps[2 * seg]) << 8) | u16::from(ps[2 * seg + 1]);
        let blocks = [
            encode_block(pi, Offset::A),
            encode_block(b2, Offset::B),
            encode_block(0xE0CD, Offset::C),
            encode_block(b4, Offset::D),
        ];
        blocks
            .iter()
            .flat_map(|b| (0..26).map(move |i| ((b >> (25 - i)) & 1) as u8))
            .collect()
    }

    fn feed(dec: &mut RdsDecoder, bits: &[u8]) {
        let base = dec.sync.bits();
        for (i, &b) in bits.iter().enumerate() {
            dec.push_bit(b, (base + i as u64) as f64);
        }
    }

    fn stream(frames: &[&[u8; 8]], repeats: usize) -> Vec<u8> {
        let mut bits = Vec::new();
        for _ in 0..repeats {
            for ps in frames {
                for seg in 0..4 {
                    bits.extend(group_0a_bits(0xC0DE, ps, seg, 10, true));
                }
            }
        }
        bits
    }

    #[test]
    fn clean_stream_gives_pi_ps_pty_tp() {
        // Positions in bits: 1187.5 units per second.
        let mut dec = RdsDecoder::new(GroupConfig::default(), RDS_BITRATE_BD);
        let mut bits = vec![1, 0, 1, 1, 0];
        bits.extend(stream(&[b"HACKRIFF"], 4));
        // Sync acquires on the second block (the first group is partial) and the slip search
        // evaluates a block one bit late, so pad the tail by a bit.
        bits.push(0);
        feed(&mut dec, &bits);
        let r = dec.report();
        assert_eq!(r.pi.unwrap().hex(), "C0DE");
        assert_eq!(r.ps(), Some("HACKRIFF"));
        assert_eq!(r.frame_log.len(), 3, "{:?}", r.frame_log);
        assert_eq!(r.pty, Some(10));
        assert_eq!(r.tp, Some(true));
        assert_eq!(r.block_error_rate, Some(0.0));
        assert_eq!(r.group_types["0A"], r.groups_total as u32);
    }

    #[test]
    fn scrolling_ps_gives_several_frames() {
        let mut dec = RdsDecoder::new(GroupConfig::default(), RDS_BITRATE_BD);
        feed(
            &mut dec,
            &stream(&[b"Unstoppa", b"ppable -", b"Sia     "], 3),
        );
        let r = dec.report();
        let texts: Vec<&str> = r.ps_frames.iter().map(|(t, _)| t.as_str()).collect();
        for want in ["Unstoppa", "ppable -", "Sia     "] {
            assert!(texts.contains(&want), "{texts:?}");
        }
    }

    #[test]
    fn bit_errors_are_rejected_and_slips_absorbed() {
        let mut rng = Rng::new(3);
        let mut bits = stream(&[b"HACKRIFF"], 12);
        // A 40-bit burst of garbage and a few isolated flips.
        for b in &mut bits[1000..1040] {
            *b = (rng.next_u64() & 1) as u8;
        }
        for k in [2000, 2600, 3100] {
            bits[k] ^= 1;
        }
        // A dropped bit and an inserted bit.
        bits.remove(3500);
        bits.insert(4200, 1);
        let mut dec = RdsDecoder::new(GroupConfig::default(), RDS_BITRATE_BD);
        feed(&mut dec, &bits);
        let r = dec.report();
        assert_eq!(r.pi.unwrap().hex(), "C0DE");
        assert_eq!(r.pi_votes.len(), 1, "{:?}", r.pi_votes);
        assert!(r.frame_log.iter().all(|f| f.text == "HACKRIFF"));
        assert!(r.blocks_total > r.blocks_ok, "errors counted");
        assert!(r.bit_slips >= 2, "slips {}", r.bit_slips);
    }

    /// T-962: four groups (the first partial: sync acquires on the second block) give three
    /// agreeing CRC-valid PI blocks — the exact evidence 98.085 MHz committed PI 1704 on, while
    /// an independent oracle found no RDS at all on the same clip. The PI is **reported**, so the
    /// UI can show "PI C0DE (3 groups, provisional)", and it is **not committed**, so nothing
    /// identity-bearing may be written from it.
    #[test]
    fn t962_three_agreeing_groups_are_provisional_not_committed() {
        let mut dec = RdsDecoder::new(GroupConfig::default(), RDS_BITRATE_BD);
        // Five groups: block sync acquires on three consecutive offset-consistent blocks, so the
        // first two groups pay for the lock and three CRC-valid PI blocks are left.
        let bits = stream(&[b"HACKRIFF"], 2);
        feed(&mut dec, &bits[..5 * 104]);
        let r = dec.report();
        let pi = r.pi.expect("the PI is still reported, with its vote");
        assert_eq!(
            pi.votes, 3,
            "[T-962] the scene is three agreeing groups: {r:?}"
        );
        assert_eq!(pi.hex(), "C0DE");
        assert!(
            (3..GroupConfig::default().pi_commit_votes).contains(&pi.votes),
            "[T-962] this scene is meant to sit in the reported-but-not-committed band; \
             votes {} of a {}-vote bound",
            pi.votes,
            GroupConfig::default().pi_commit_votes
        );
        assert!(
            pi.provisional && !pi.committed(),
            "[T-962] {} agreeing CRC-valid groups is not a committed identity: RDS's block check \
             is 10 bits, and a mis-synchronised lattice re-reads correlated bits, so a handful of \
             agreeing blocks can be one wrong lock rather than one station. {pi:?}",
            pi.votes
        );
        assert_eq!(r.pi_abstain, None, "abstaining would hide the reading");
    }

    /// T-962: a station transmitting for a second clears the bound — 11.4 groups/s is the rate a
    /// real RDS stream runs at, so the bound costs a real station under a second of lock.
    #[test]
    fn t962_a_second_of_a_real_station_commits_the_pi() {
        let mut dec = RdsDecoder::new(GroupConfig::default(), RDS_BITRATE_BD);
        // 12 groups ~ 1.05 s of air.
        feed(&mut dec, &stream(&[b"HACKRIFF"], 3));
        let r = dec.report();
        let pi = r.pi.expect("PI");
        assert_eq!(pi.hex(), "C0DE");
        assert!(
            pi.votes >= GroupConfig::default().pi_commit_votes,
            "[T-962] a second of clean air should clear the bound; votes {}",
            pi.votes
        );
        assert!(pi.committed() && !pi.provisional, "[T-962] {pi:?}");
    }

    /// T-962 round 2: the commit bar is a **rate** over stream time, so a long session cannot
    /// accumulate its way to an identity. The same clean groups, once spaced one per 15 s of
    /// stream time (a chance lock's rate, 98.085 MHz), stay provisional however many agree; back
    /// to back (a real station's 11.4 groups/s) they commit.
    #[test]
    fn t962_sparse_votes_over_a_long_session_stay_provisional() {
        let bits = stream(&[b"HACKRIFF"], 6); // 24 groups
        let spaced = |gap_s: f64| {
            let mut dec = RdsDecoder::new(GroupConfig::default(), RDS_BITRATE_BD);
            let extra = gap_s * RDS_BITRATE_BD - 104.0;
            for (i, &b) in bits.iter().enumerate() {
                let group = (i / 104) as f64;
                dec.push_bit(b, i as f64 + group * extra);
            }
            dec.report().pi.expect("the PI is reported")
        };
        let sparse = spaced(15.0);
        let n = GroupConfig::default().pi_commit_votes;
        assert!(
            sparse.votes >= 2 * n,
            "the scene has twice the bar in lifetime votes: {sparse:?}"
        );
        assert!(
            sparse.provisional && !sparse.committed(),
            "[T-962] {} agreeing groups one per 15 s ({} s of stream) committed the PI: the bar \
             is a rate ({n} within {} s), not a lifetime count. {sparse:?}",
            sparse.votes,
            sparse.votes * 15,
            hk_model::RDS_PI_COMMIT_WINDOW_NS / 1_000_000_000
        );
        assert_eq!(sparse.window_votes, 1, "{sparse:?}");
        let dense = spaced(104.0 / RDS_BITRATE_BD);
        assert!(
            dense.committed() && dense.window_votes == n,
            "[T-962] {dense:?}"
        );
    }

    fn block_bits(blocks: [u32; 4]) -> Vec<u8> {
        blocks
            .iter()
            .flat_map(|b| (0..26).map(move |i| ((b >> (25 - i)) & 1) as u8))
            .collect()
    }

    /// A 2A group: RadioText characters `4·addr..4·addr+4` of `text` (0x0D-terminated, space
    /// padded to 64).
    fn group_2a_bits(pi: u16, ab: bool, addr: usize, text: &[u8]) -> Vec<u8> {
        let at = |i: usize| u16::from(*text.get(i).unwrap_or(&b' '));
        let b2 = (2u16 << 12) | (10u16 << 5) | (u16::from(ab) << 4) | addr as u16;
        block_bits([
            encode_block(pi, Offset::A),
            encode_block(b2, Offset::B),
            encode_block((at(4 * addr) << 8) | at(4 * addr + 1), Offset::C),
            encode_block((at(4 * addr + 2) << 8) | at(4 * addr + 3), Offset::D),
        ])
    }

    /// A 2B group: RadioText characters `2·addr..2·addr+2`.
    fn group_2b_bits(pi: u16, ab: bool, addr: usize, text: &[u8]) -> Vec<u8> {
        let at = |i: usize| u16::from(*text.get(i).unwrap_or(&b' '));
        let b2 = (2u16 << 12) | (1 << 11) | (10u16 << 5) | (u16::from(ab) << 4) | addr as u16;
        block_bits([
            encode_block(pi, Offset::A),
            encode_block(b2, Offset::B),
            encode_block(pi, Offset::CPrime),
            encode_block((at(2 * addr) << 8) | at(2 * addr + 1), Offset::D),
        ])
    }

    /// A whole 2A message (every address the text reaches, terminator included).
    fn rt_2a(pi: u16, ab: bool, text: &str) -> Vec<u8> {
        let mut t = text.as_bytes().to_vec();
        if t.len() < 64 {
            t.push(0x0D);
        }
        (0..t.len().div_ceil(4))
            .flat_map(|a| group_2a_bits(pi, ab, a, &t))
            .collect()
    }

    /// A 0A group whose block C carries AF codes `a`, `b`.
    fn group_0a_af_bits(pi: u16, seg: usize, af: (u8, u8)) -> Vec<u8> {
        let ps = b"HACKRIFF";
        let b2 = (10u16 << 5) | seg as u16;
        block_bits([
            encode_block(pi, Offset::A),
            encode_block(b2, Offset::B),
            encode_block((u16::from(af.0) << 8) | u16::from(af.1), Offset::C),
            encode_block(
                (u16::from(ps[2 * seg]) << 8) | u16::from(ps[2 * seg + 1]),
                Offset::D,
            ),
        ])
    }

    /// A 4A clock-time group.
    fn group_4a_bits(pi: u16, mjd: u32, hour: u16, minute: u16, offset: i8) -> Vec<u8> {
        let b2 = (4u16 << 12) | (10u16 << 5) | ((mjd >> 15) & 3) as u16;
        let c = (((mjd & 0x7FFF) as u16) << 1) | (hour >> 4);
        let d = ((hour & 0xF) << 12)
            | (minute << 6)
            | (u16::from(offset < 0) << 5)
            | u16::from(offset.unsigned_abs());
        block_bits([
            encode_block(pi, Offset::A),
            encode_block(b2, Offset::B),
            encode_block(c, Offset::C),
            encode_block(d, Offset::D),
        ])
    }

    /// Two groups of lead-in so block sync holds from the first group that matters.
    fn lead_in() -> Vec<u8> {
        stream(&[b"HACKRIFF"], 1)
    }

    /// T-971: a 2A RadioText message assembles to its carriage return, and a toggled A/B flag
    /// starts the next one.
    #[test]
    fn t971_radiotext_2a_assembles_and_ab_toggles() {
        let mut dec = RdsDecoder::new(GroupConfig::default(), RDS_BITRATE_BD);
        let mut bits = lead_in();
        bits.extend(rt_2a(
            0xC0DE,
            false,
            "Star 101.3 - Olivia Dean - Man I Need",
        ));
        bits.extend(rt_2a(
            0xC0DE,
            false,
            "Star 101.3 - Olivia Dean - Man I Need",
        ));
        bits.extend(rt_2a(
            0xC0DE,
            true,
            "Star 101.3 - Glass Animals - Heat Waves",
        ));
        bits.push(0);
        feed(&mut dec, &bits);
        let r = dec.report();
        let texts: Vec<(&str, bool)> = r
            .rt_messages
            .iter()
            .map(|m| (m.text.as_str(), m.ab))
            .collect();
        assert_eq!(
            texts,
            [
                ("Star 101.3 - Olivia Dean - Man I Need", false),
                ("Star 101.3 - Glass Animals - Heat Waves", true),
            ],
            "[T-971] a repeat is one message; a toggle is the next: {:?}",
            r.rt_messages
        );
        assert_eq!(
            r.rt.as_deref(),
            Some("Star 101.3 - Glass Animals - Heat Waves")
        );
        assert_eq!(r.rt_ab, Some(true));
    }

    /// T-971: a station that changes RadioText without toggling A/B starts a new message at the
    /// first character that differs, rather than splicing two texts.
    #[test]
    fn t971_radiotext_changed_without_toggle_is_a_new_message() {
        let mut dec = RdsDecoder::new(GroupConfig::default(), RDS_BITRATE_BD);
        let mut bits = lead_in();
        bits.extend(rt_2a(0xC0DE, false, "FIRST SONG"));
        bits.extend(rt_2a(0xC0DE, false, "OTHER TUNE"));
        bits.push(0);
        feed(&mut dec, &bits);
        let r = dec.report();
        let texts: Vec<&str> = r.rt_messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["FIRST SONG", "OTHER TUNE"], "{:?}", r.rt_messages);
    }

    /// T-971: an incomplete message is never reported (a missing address is not a space).
    #[test]
    fn t971_radiotext_incomplete_is_not_reported() {
        let mut dec = RdsDecoder::new(GroupConfig::default(), RDS_BITRATE_BD);
        let text = b"Star 101.3 - Olivia Dean\r";
        let mut bits = lead_in();
        for a in [0usize, 1, 3, 4, 5, 6] {
            bits.extend(group_2a_bits(0xC0DE, false, a, text));
        }
        bits.push(0);
        feed(&mut dec, &bits);
        let r = dec.report();
        assert_eq!(r.rt, None, "address 2 never arrived: {:?}", r.rt_messages);
    }

    /// T-971: 2B carries two characters per address, a 32-character message.
    #[test]
    fn t971_radiotext_2b() {
        let mut dec = RdsDecoder::new(GroupConfig::default(), RDS_BITRATE_BD);
        let mut t = b"KQED News".to_vec();
        t.push(0x0D);
        let mut bits = lead_in();
        for a in 0..t.len().div_ceil(2) {
            bits.extend(group_2b_bits(0xC0DE, false, a, &t));
        }
        bits.push(0);
        feed(&mut dec, &bits);
        assert_eq!(dec.report().rt.as_deref(), Some("KQED News"));
    }

    /// T-971: AF codes from 0A block C (fillers and count indicators are not frequencies), and
    /// CT from 4A.
    #[test]
    fn t971_af_and_ct() {
        let mut dec = RdsDecoder::new(GroupConfig::default(), RDS_BITRATE_BD);
        let mut bits = lead_in();
        // 225 = "one AF follows", 205 = filler; 138 = 101.3 MHz, 114 = 98.9 MHz.
        bits.extend(group_0a_af_bits(0xC0DE, 0, (225, 138)));
        bits.extend(group_0a_af_bits(0xC0DE, 1, (114, 205)));
        // MJD 61308 = 2026-09-25; 11:59 UTC, local −7 h (−14 half hours).
        bits.extend(group_4a_bits(0xC0DE, 61_308, 11, 59, -14));
        bits.extend(group_0a_af_bits(0xC0DE, 2, (138, 205)));
        bits.push(0);
        feed(&mut dec, &bits);
        let r = dec.report();
        assert_eq!(r.af_mhz, [98.9, 101.3], "{r:?}");
        let ct = r.ct.expect("CT");
        assert_eq!(
            (ct.mjd, ct.hour, ct.minute, ct.offset_half_hours),
            (61_308, 11, 59, -14)
        );
        assert_eq!(ct.utc_unix_s, 1_790_337_540, "2026-09-25T11:59:00Z");
    }

    /// T-971: a dynamic PS is its sequence, and the current PS is the latest complete frame;
    /// hours of scrolling PS stay bounded.
    #[test]
    fn t971_ps_sequence_current_and_bounds() {
        let mut dec = RdsDecoder::new(GroupConfig::default(), RDS_BITRATE_BD);
        // The slip search evaluates a block one bit late: pad each feed's tail by a bit.
        let padded = |mut bits: Vec<u8>| {
            bits.push(0);
            bits
        };
        feed(
            &mut dec,
            &padded(stream(&[b"101.3 - ", b"Animals ", b"Heat    "], 2)),
        );
        let r = dec.report();
        assert_eq!(r.ps_current.as_deref(), Some("Heat    "));
        assert!(r.ps_dynamic);
        assert_eq!(
            r.ps_sequence[r.ps_sequence.len() - 3..],
            ["101.3 - ", "Animals ", "Heat    "],
            "{:?}",
            r.ps_sequence
        );
        // Many distinct frames: every accumulator stays at its cap.
        let texts: Vec<[u8; 8]> = (0..(PS_DISTINCT_CAP + 40))
            .map(|i| {
                let mut t = *b"TEXT    ";
                t[5..8].copy_from_slice(format!("{i:03}").as_bytes());
                t
            })
            .collect();
        let refs: Vec<&[u8; 8]> = texts.iter().collect();
        feed(&mut dec, &padded(stream(&refs, 3)));
        let r = dec.report();
        assert!(
            r.ps_frames.len() <= PS_DISTINCT_CAP,
            "{}",
            r.ps_frames.len()
        );
        assert!(r.frame_log.len() <= FRAME_LOG_CAP, "{}", r.frame_log.len());
        assert!(r.ps_sequence.len() <= PS_SEQUENCE_CAP);
        assert_eq!(r.ps_current.as_deref(), Some("TEXT 103"));
    }

    #[test]
    fn random_bits_give_no_pi() {
        let mut rng = Rng::new(11);
        let bits: Vec<u8> = (0..60_000).map(|_| (rng.next_u64() & 1) as u8).collect();
        let mut dec = RdsDecoder::new(GroupConfig::default(), RDS_BITRATE_BD);
        feed(&mut dec, &bits);
        let r = dec.report();
        assert!(r.pi.is_none(), "{r:?}");
        assert!(r.frame_log.is_empty());
    }
}
