//! Input continuity tracking shared by the PFB and the DDC: which inputs reset filter state,
//! and which flags/losses are owed to the next output block.

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_model::{SampleTime, Timestamp};

use crate::stft::InputInfo;

/// Flags that reset channelizer and DDC filter state by default: stream start, retune, rate
/// change and gap. Gain and other provenance changes pass through as flags without a reset.
/// A gap (index jump or `dropped_before > 0`) always resets, whatever the setting, because the
/// output time map cannot splice across missing samples.
pub const DEFAULT_CHANNEL_RESET_ON: Discontinuity = Discontinuity::from_bits_truncate(
    Discontinuity::STREAM_START.bits()
        | Discontinuity::RETUNE.bits()
        | Discontinuity::RATE_CHANGE.bits()
        | Discontinuity::GAP.bits(),
);

/// What [`StreamTracker::begin`] decided about an input.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Begin {
    /// Filter state must be cleared and restarted at this input's first sample.
    pub reset: bool,
    /// The input sample rate differs from the previous input's (or this is the first input).
    pub rate_changed: bool,
}

pub(crate) struct StreamTracker {
    reset_on: Discontinuity,
    next_index: Option<u64>,
    provenance: Option<ProvenanceHandle>,
    anchor: SampleTime,
    pending_flags: Discontinuity,
    pending_dropped: u64,
}

impl StreamTracker {
    pub(crate) fn new(reset_on: Discontinuity) -> Self {
        Self {
            reset_on,
            next_index: None,
            provenance: None,
            anchor: SampleTime {
                sample_index: 0,
                host_time: Timestamp::UNIX_EPOCH,
            },
            pending_flags: Discontinuity::NONE,
            pending_dropped: 0,
        }
    }

    pub(crate) fn set_reset_on(&mut self, flags: Discontinuity) {
        self.reset_on = flags;
    }

    pub(crate) fn reset_on(&self) -> Discontinuity {
        self.reset_on
    }

    /// Forgets the stream (the next input is a stream start); pending flags are dropped.
    pub(crate) fn clear(&mut self) {
        self.next_index = None;
        self.provenance = None;
        self.pending_flags = Discontinuity::NONE;
        self.pending_dropped = 0;
    }

    /// Registers an input of `len` samples.
    pub(crate) fn begin(&mut self, info: &InputInfo<'_>, len: usize) -> Begin {
        let mut flags = info.discontinuity;
        let first = self.next_index.is_none();
        let dropped = match self.next_index {
            None => {
                flags |= Discontinuity::STREAM_START;
                info.dropped_before
            }
            Some(expected) => {
                let idx = info.time.sample_index;
                if idx != expected {
                    flags |= Discontinuity::GAP;
                    idx.saturating_sub(expected).max(info.dropped_before)
                } else {
                    info.dropped_before
                }
            }
        };
        if dropped > 0 {
            flags |= Discontinuity::GAP;
        }
        let prov = info.provenance;
        let rate_changed = match &self.provenance {
            None => true,
            Some(old) if old != prov => {
                let d = Discontinuity::between(old, prov);
                flags |= d;
                d.contains(Discontinuity::RATE_CHANGE)
            }
            Some(_) => false,
        };
        if self.provenance.as_ref() != Some(prov) {
            self.provenance = Some(prov.clone());
        }
        self.anchor = info.time;
        self.next_index = Some(info.time.sample_index + len as u64);
        self.pending_flags |= flags;
        self.pending_dropped += dropped;
        let reset =
            first || flags.contains(Discontinuity::GAP) || flags.bits() & self.reset_on.bits() != 0;
        Begin {
            reset,
            rate_changed,
        }
    }

    /// Flags and losses owed to the next non-empty output block; clears them.
    pub(crate) fn take_pending(&mut self) -> (Discontinuity, u64) {
        let out = (self.pending_flags, self.pending_dropped);
        self.pending_flags = Discontinuity::NONE;
        self.pending_dropped = 0;
        out
    }

    /// Provenance of the latest input. Panics before the first input.
    pub(crate) fn provenance(&self) -> &ProvenanceHandle {
        self.provenance.as_ref().expect("input seen")
    }

    /// Time anchor of the latest input.
    pub(crate) fn anchor(&self) -> SampleTime {
        self.anchor
    }
}
