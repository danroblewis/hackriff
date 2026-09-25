//! RDS group assembly and parsing (EN 50067 §3.1): bits → blocks → groups → PI, PS, PTY, TP/TA,
//! with block and group error statistics.
//!
//! - **PI** comes from block A, and also from block C' in version-B groups. A group whose two PI
//!   copies disagree, or whose block-3 offset (C vs C') contradicts the version bit, is rejected
//!   as a bad synchronisation. The station PI is a vote over CRC-valid PI blocks and is only
//!   reported with at least `pi_min_votes` votes and a `pi_min_share` majority.
//! - **A reported PI is not yet a committed identity (T-962).** Reporting and *committing* are
//!   two bars, and [`PiDecision::provisional`] is the gap between them: below
//!   `pi_commit_votes` agreeing CRC-valid PI blocks the PI is reported **provisionally** — good
//!   enough to show ("PI 1704, 3 groups, provisional"), not good enough to write as a
//!   transmitter identity or to rest a lifecycle change on. See [`GroupConfig::pi_commit_votes`]
//!   for the bound and why it is a vote count rather than ADR-0022's bits budget.
//! - **PS** is sent in 4 two-character segments by groups 0A/0B. A frame completes when
//!   segments 0, 1, 2, 3 arrive in order (repeats allowed), under one PI, each within
//!   `ps_max_gap_s` of the previous. Stations scroll PS (song/artist text), so PS is the list of
//!   complete frames, never one static string; the label is the most frequent frame.
//! - **Error rates:** blocks evaluated at their lattice position while synchronised; a group is
//!   in error when any of its 4 blocks is.

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

use super::block::{BlockEvent, BlockSync, Offset, SyncConfig};

/// RDS bit rate, Bd (57 kHz / 48 = pilot / 16).
pub const RDS_BITRATE_BD: f64 = 1187.5;

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
    /// **T-962: the vote has not reached [`GroupConfig::pi_commit_votes`].**
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
    /// Complete PS frames, most frequent first (ties: first seen first).
    pub ps_frames: Vec<(String, u32)>,
    /// Every complete PS frame in order.
    pub frame_log: Vec<PsFrame>,
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
    frames: Vec<PsFrame>,
    pi_votes: BTreeMap<u16, u32>,
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
            frames: Vec::new(),
            pi_votes: BTreeMap::new(),
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
        for p in [pi_a, pi_c].into_iter().flatten() {
            *self.pi_votes.entry(p).or_default() += 1;
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
            }
        }
        self.groups.push(group);
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
                let text: String = ps.buf.iter().flatten().map(|&c| char::from(c)).collect();
                self.frames.push(PsFrame {
                    position: ps.start,
                    pi,
                    text,
                });
                *ps = PsAssembler::default();
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
            Some((p, v)) => (
                Some(PiDecision {
                    pi: p,
                    votes: v,
                    total_votes,
                    share: f64::from(v) / f64::from(total_votes),
                    // T-962: reported from `pi_min_votes`, committed only from
                    // `pi_commit_votes`. The gap is the provisional state.
                    provisional: v < self.config.pi_commit_votes,
                }),
                None,
            ),
        };
        let mut counts: Vec<(String, u32, usize)> = Vec::new();
        for (i, f) in self.frames.iter().enumerate() {
            match counts.iter_mut().find(|(t, _, _)| *t == f.text) {
                Some(c) => c.1 += 1,
                None => counts.push((f.text.clone(), 1, i)),
            }
        }
        counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));
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
            ps_frames: counts.into_iter().map(|(t, n, _)| (t, n)).collect(),
            frame_log: self.frames.clone(),
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
