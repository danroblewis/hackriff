//! Sample blocks: the unit a [`Source`](crate::source::Source) produces and the
//! [ring buffer](crate::ring) stores.
//!
//! A block is a run of contiguous complex samples plus a [`BlockHeader`]:
//!
//! - [`SampleTime`] of the first sample. Its `sample_index` is the stream's monotonic sample
//!   counter, so a jump between the end of one block and the start of the next is a detectable
//!   gap (dropped samples), never silently spliced.
//! - A [`ProvenanceHandle`]: a cheap, shared, immutable reference to the Provenance in force for
//!   every sample of the block (docs/07 §2.6).
//! - [`Discontinuity`] flags (stream start, retune, rate change, gain change, provenance change,
//!   gap) and the number of samples dropped immediately before the block.
//!
//! Blocks never span a change of state: a retune or gain change starts a new block.

use std::fmt;
use std::ops::{BitOr, BitOrAssign, Deref};
use std::sync::Arc;

use hk_model::{Provenance, ProvenanceId, SampleTime};
use num_complex::Complex32;

/// A shared, immutable Provenance record with its identity.
///
/// Cloning is an `Arc` clone (no allocation), so every block and every ring-buffer read can carry
/// one. Two handles are equal when they have the same [`ProvenanceId`]: records are deduplicated
/// by identity, and sources mint a new handle only when the state actually changes.
#[derive(Clone)]
pub struct ProvenanceHandle {
    id: ProvenanceId,
    value: Arc<Provenance>,
}

impl ProvenanceHandle {
    /// Wraps a new record with a fresh UUIDv7 id.
    pub fn new(provenance: Provenance) -> Self {
        Self::with_id(ProvenanceId::new(), provenance)
    }

    /// Wraps a record under an existing id (e.g. one read back from storage).
    pub fn with_id(id: ProvenanceId, provenance: Provenance) -> Self {
        Self {
            id,
            value: Arc::new(provenance),
        }
    }

    /// The record's identity.
    pub fn id(&self) -> ProvenanceId {
        self.id
    }

    /// The record.
    pub fn get(&self) -> &Provenance {
        &self.value
    }
}

impl Deref for ProvenanceHandle {
    type Target = Provenance;

    fn deref(&self) -> &Provenance {
        &self.value
    }
}

impl PartialEq for ProvenanceHandle {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for ProvenanceHandle {}

impl fmt::Debug for ProvenanceHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProvenanceHandle")
            .field("id", &self.id)
            .field("device_id", &self.value.device_id)
            .field("center_hz", &self.value.tune.center_hz)
            .field("sample_rate_hz", &self.value.tune.sample_rate_hz)
            .finish()
    }
}

/// Discontinuity flags on a block. Set only on the first block after the change.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Discontinuity(u8);

impl Discontinuity {
    /// No discontinuity: the block continues the previous one.
    pub const NONE: Self = Self(0);
    /// First block of a stream.
    pub const STREAM_START: Self = Self(1 << 0);
    /// Centre frequency changed.
    pub const RETUNE: Self = Self(1 << 1);
    /// Sample rate changed.
    pub const RATE_CHANGE: Self = Self(1 << 2);
    /// Gain or amplifier state changed.
    pub const GAIN_CHANGE: Self = Self(1 << 3);
    /// Samples are missing before this block; see [`BlockHeader::dropped_before`].
    pub const GAP: Self = Self(1 << 4);
    /// The Provenance record changed (any field, including the ones above).
    pub const PROVENANCE_CHANGE: Self = Self(1 << 5);

    const NAMES: [(Self, &'static str); 6] = [
        (Self::STREAM_START, "STREAM_START"),
        (Self::RETUNE, "RETUNE"),
        (Self::RATE_CHANGE, "RATE_CHANGE"),
        (Self::GAIN_CHANGE, "GAIN_CHANGE"),
        (Self::GAP, "GAP"),
        (Self::PROVENANCE_CHANGE, "PROVENANCE_CHANGE"),
    ];

    /// The raw bits.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Builds flags from raw bits, dropping unknown bits.
    pub const fn from_bits_truncate(bits: u8) -> Self {
        Self(bits & 0b11_1111)
    }

    /// No flag is set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Every flag in `other` is set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Sets the flags in `other`.
    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    /// Flags describing a change from `prev` to `next`: RETUNE, RATE_CHANGE and GAIN_CHANGE
    /// for those fields, plus PROVENANCE_CHANGE if the records differ at all.
    pub fn between(prev: &Provenance, next: &Provenance) -> Self {
        let mut flags = Self::NONE;
        if prev.tune.center_hz != next.tune.center_hz {
            flags |= Self::RETUNE;
        }
        if prev.tune.sample_rate_hz != next.tune.sample_rate_hz {
            flags |= Self::RATE_CHANGE;
        }
        if prev.tune.lna_db != next.tune.lna_db
            || prev.tune.vga_db != next.tune.vga_db
            || prev.tune.amp_on != next.tune.amp_on
        {
            flags |= Self::GAIN_CHANGE;
        }
        if prev != next {
            flags |= Self::PROVENANCE_CHANGE;
        }
        flags
    }
}

impl BitOr for Discontinuity {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for Discontinuity {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl fmt::Debug for Discontinuity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("NONE");
        }
        let mut first = true;
        for (flag, name) in Self::NAMES {
            if self.contains(flag) {
                if !first {
                    f.write_str(" | ")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        Ok(())
    }
}

/// Everything about a block except its samples.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockHeader {
    /// Time anchor of the first sample. `time.sample_index` is the stream's monotonic sample
    /// counter.
    pub time: SampleTime,
    /// Provenance in force for every sample of the block.
    pub provenance: ProvenanceHandle,
    /// Discontinuity flags, set on the first block after a change.
    pub discontinuity: Discontinuity,
    /// Samples missing between the previous block's end and this block's start (non-zero only
    /// with [`Discontinuity::GAP`]).
    pub dropped_before: u64,
}

impl BlockHeader {
    /// Stream sample index of the first sample.
    pub fn first_sample(&self) -> u64 {
        self.time.sample_index
    }

    /// Sample rate in force, Hz (from the provenance).
    pub fn sample_rate_hz(&self) -> f64 {
        self.provenance.tune.sample_rate_hz
    }

    /// Centre frequency in force, Hz (from the provenance).
    pub fn center_hz(&self) -> f64 {
        self.provenance.tune.center_hz
    }
}

/// An owned block: header plus samples, normalised to complex float in [-1, 1).
///
/// The hot path does not use this type; it reads into a reused `Vec` through
/// [`Source::read_block`](crate::source::Source::read_block).
#[derive(Clone, Debug, PartialEq)]
pub struct SampleBlock {
    /// Block metadata.
    pub header: BlockHeader,
    /// Samples.
    pub samples: Vec<Complex32>,
}

impl SampleBlock {
    /// Stream sample index one past the last sample.
    pub fn end_sample(&self) -> u64 {
        self.header.first_sample() + self.samples.len() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_compose_and_print() {
        let mut d = Discontinuity::RETUNE | Discontinuity::GAP;
        assert!(d.contains(Discontinuity::GAP));
        assert!(!d.contains(Discontinuity::STREAM_START));
        d.insert(Discontinuity::STREAM_START);
        assert_eq!(format!("{d:?}"), "STREAM_START | RETUNE | GAP");
        assert_eq!(format!("{:?}", Discontinuity::NONE), "NONE");
        assert_eq!(Discontinuity::from_bits_truncate(0xff).bits(), 0b11_1111);
    }
}
