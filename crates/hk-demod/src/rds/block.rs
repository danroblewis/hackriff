//! RDS block code (EN 50067 §2.3, Annex B) and block synchronisation.
//!
//! A block is 26 bits, MSB first: 16 information bits and a 10-bit checkword from the
//! shortened cyclic code `g(x) = x¹⁰ + x⁸ + x⁷ + x⁵ + x⁴ + x³ + 1`, XORed with the offset word of
//! its position in the group (A, B, C or C', D). The syndrome of a correctly received block
//! equals the syndrome of its offset word, which identifies both "no detected error" and the
//! block's position. Synchronisation (§C.1) looks for two offset-word syndromes whose bit
//! distance and offset order agree, then checks every following block at its expected position.
//! Error *correction* is not attempted: a block whose syndrome does not match is rejected. The
//! code detects every burst of ≤ 10 bits; a random word passes by chance with probability
//! 2⁻¹⁰.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// `g(x) = x¹⁰ + x⁸ + x⁷ + x⁵ + x⁴ + x³ + 1`.
pub const CHECK_POLY: u32 = 0x5B9;

/// Annex B parity-check matrix: row `k` applies to bit `25 − k` of the block (MSB first).
const H: [u16; 26] = [
    0x200, 0x100, 0x080, 0x040, 0x020, 0x010, 0x008, 0x004, 0x002, 0x001, 0x2DC, 0x16E, 0x0B7,
    0x287, 0x39F, 0x313, 0x355, 0x376, 0x1BB, 0x201, 0x3DC, 0x1EE, 0x0F7, 0x2A7, 0x38F, 0x31B,
];

const MASK26: u64 = (1 << 26) - 1;

/// Block position in a group, named by its offset word.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Offset {
    /// Block 1 (PI).
    A,
    /// Block 2 (group type, PTY, TP...).
    B,
    /// Block 3 of a version-A group.
    C,
    /// Block 3 of a version-B group (carries PI).
    CPrime,
    /// Block 4.
    D,
}

impl Offset {
    /// All offsets.
    pub const ALL: [Offset; 5] = [Offset::A, Offset::B, Offset::C, Offset::CPrime, Offset::D];

    /// Position in the group, 0–3.
    pub const fn slot(self) -> usize {
        match self {
            Offset::A => 0,
            Offset::B => 1,
            Offset::C | Offset::CPrime => 2,
            Offset::D => 3,
        }
    }

    /// The 10-bit offset word.
    pub const fn word(self) -> u16 {
        match self {
            Offset::A => 0x0FC,
            Offset::B => 0x198,
            Offset::C => 0x168,
            Offset::CPrime => 0x350,
            Offset::D => 0x1B4,
        }
    }

    /// Syndrome of a correctly received block carrying this offset.
    pub const fn syndrome(self) -> u16 {
        match self {
            Offset::A => 0x3D8,
            Offset::B => 0x3D4,
            Offset::C => 0x25C,
            Offset::CPrime => 0x3CC,
            Offset::D => 0x258,
        }
    }

    /// The offset whose syndrome is `s`, if any.
    pub fn from_syndrome(s: u16) -> Option<Offset> {
        Offset::ALL.into_iter().find(|o| o.syndrome() == s)
    }

    /// Whether this offset may appear at `slot`.
    pub const fn fits_slot(self, slot: usize) -> bool {
        self.slot() == slot
    }
}

/// Syndrome of a 26-bit block (bit 25 first).
pub fn syndrome(block: u32) -> u16 {
    let mut s = 0u16;
    for (k, row) in H.iter().enumerate() {
        if (block >> (25 - k)) & 1 == 1 {
            s ^= row;
        }
    }
    s
}

/// Checkword for `info` at `offset`.
pub fn checkword(info: u16, offset: Offset) -> u16 {
    let mut reg = u32::from(info) << 10;
    for bit in (10..26).rev() {
        if reg & (1 << bit) != 0 {
            reg ^= CHECK_POLY << (bit - 10);
        }
    }
    (reg & 0x3FF) as u16 ^ offset.word()
}

/// The 26-bit block carrying `info` at `offset`.
pub fn encode_block(info: u16, offset: Offset) -> u32 {
    (u32::from(info) << 10) | u32::from(checkword(info, offset))
}

/// One block evaluated at its lattice position while synchronised.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockEvent {
    /// Slot 0–3 in the group.
    pub slot: usize,
    /// Offset whose syndrome matched (`None`: rejected).
    pub offset: Option<Offset>,
    /// The 16 information bits (meaningless when rejected).
    pub info: u16,
    /// Index of the block's last bit in the bit stream.
    pub last_bit: u64,
    /// −1 / +1 when the block was found one bit early / late (a bit slip absorbed).
    pub slip: i8,
    /// This block acquired synchronisation.
    pub acquired: bool,
    /// Synchronisation was lost after this block.
    pub lost: bool,
}

impl BlockEvent {
    /// Syndrome check passed.
    pub fn ok(&self) -> bool {
        self.offset.is_some()
    }
}

/// Block-synchronisation settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SyncConfig {
    /// Two offset hits further apart than this many blocks do not synchronise.
    pub max_hit_gap_blocks: u64,
    /// Consecutive rejected blocks that drop synchronisation.
    pub max_consecutive_bad: u32,
    /// Look for a one-bit slip when a block fails at its expected position.
    pub slip_search: bool,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            max_hit_gap_blocks: 6,
            max_consecutive_bad: 12,
            slip_search: true,
        }
    }
}

/// Streaming block synchroniser: bits in, [`BlockEvent`]s out.
#[derive(Clone, Debug)]
pub struct BlockSync {
    config: SyncConfig,
    reg: u64,
    nbits: u64,
    synced: bool,
    next_slot: usize,
    bits_since: u32,
    hits: VecDeque<(u64, usize)>,
    consecutive_bad: u32,
}

impl BlockSync {
    /// A synchroniser.
    pub fn new(config: SyncConfig) -> Self {
        Self {
            config,
            reg: 0,
            nbits: 0,
            synced: false,
            next_slot: 0,
            bits_since: 0,
            hits: VecDeque::with_capacity(32),
            consecutive_bad: 0,
        }
    }

    /// Currently synchronised.
    pub fn is_synced(&self) -> bool {
        self.synced
    }

    /// Bits received.
    pub fn bits(&self) -> u64 {
        self.nbits
    }

    /// Pushes one bit (0 or 1).
    pub fn push(&mut self, bit: u8) -> Option<BlockEvent> {
        self.reg = (self.reg << 1) | u64::from(bit & 1);
        self.nbits += 1;
        let idx = self.nbits - 1;
        if !self.synced {
            return self.search(idx);
        }
        self.bits_since += 1;
        let wait = if self.config.slip_search { 27 } else { 26 };
        if self.bits_since < wait {
            return None;
        }
        let slot = self.next_slot;
        let fits = |w: u64| {
            Offset::from_syndrome(syndrome((w & MASK26) as u32)).filter(|o| o.fits_slot(slot))
        };
        // (word, bits of the next block already received, slip, last-bit index)
        let on_time = if self.config.slip_search {
            (self.reg >> 1, 1, 0i8, idx.saturating_sub(1))
        } else {
            (self.reg, 0, 0i8, idx)
        };
        let mut choice = (on_time, fits(on_time.0));
        if choice.1.is_none() && self.config.slip_search {
            let early = (self.reg >> 2, 2, -1i8, idx.saturating_sub(2));
            let late = (self.reg, 0, 1i8, idx);
            if let Some(o) = fits(early.0) {
                choice = (early, Some(o));
            } else if let Some(o) = fits(late.0) {
                choice = (late, Some(o));
            }
        }
        let ((word, carry, slip, last_bit), offset) = choice;
        self.bits_since = carry;
        self.next_slot = (slot + 1) % 4;
        let mut ev = BlockEvent {
            slot,
            offset,
            info: ((word & MASK26) >> 10) as u16,
            last_bit,
            slip: if offset.is_some() { slip } else { 0 },
            acquired: false,
            lost: false,
        };
        if offset.is_some() {
            self.consecutive_bad = 0;
        } else {
            self.consecutive_bad += 1;
            if self.consecutive_bad >= self.config.max_consecutive_bad {
                self.synced = false;
                self.hits.clear();
                self.consecutive_bad = 0;
                ev.lost = true;
            }
        }
        Some(ev)
    }

    fn search(&mut self, idx: u64) -> Option<BlockEvent> {
        if self.nbits < 26 {
            return None;
        }
        let word = self.reg & MASK26;
        let offset = Offset::from_syndrome(syndrome(word as u32))?;
        let slot = offset.slot();
        let max_gap = 26 * self.config.max_hit_gap_blocks;
        while self.hits.front().is_some_and(|&(p, _)| idx - p > max_gap) {
            self.hits.pop_front();
        }
        let consistent = self.hits.iter().any(|&(p, s)| {
            let d = idx - p;
            d > 0 && d % 26 == 0 && (s + (d / 26) as usize) % 4 == slot
        });
        self.hits.push_back((idx, slot));
        if !consistent {
            return None;
        }
        self.synced = true;
        self.next_slot = (slot + 1) % 4;
        self.bits_since = 0;
        self.consecutive_bad = 0;
        self.hits.clear();
        Some(BlockEvent {
            slot,
            offset: Some(offset),
            info: (word >> 10) as u16,
            last_bit: idx,
            slip: 0,
            acquired: true,
            lost: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_dsp::synth::Rng;

    #[test]
    fn encoded_blocks_have_their_offset_syndrome() {
        let mut rng = Rng::new(7);
        for _ in 0..200 {
            let info = rng.next_u64() as u16;
            for o in Offset::ALL {
                assert_eq!(syndrome(encode_block(info, o)), o.syndrome(), "{o:?}");
            }
        }
        // Offset words alone (info 0, no check bits) give the same syndromes.
        for o in Offset::ALL {
            assert_eq!(syndrome(u32::from(o.word())), o.syndrome());
        }
    }

    #[test]
    fn every_burst_up_to_ten_bits_is_detected() {
        let block = encode_block(0xC0DE, Offset::A);
        for len in 1..=10u32 {
            for start in 0..=(26 - len) {
                // Bursts start and end with a 1; fill the middle with a fixed pattern.
                let mut e = 1u32 | (1 << (len - 1));
                e |= 0x2AA & ((1 << len) - 1);
                let bad = block ^ (e << start);
                assert_ne!(syndrome(bad), Offset::A.syndrome(), "len {len} at {start}");
            }
        }
    }
}
