//! Output records of the detector: [`DetectionRecord`] (a docs/07 §2.9 `Detection` plus the
//! stream-side context T-007 needs) and the [`DetectorEvent`] stream.

use std::ops::Range;

use hk_core::ProvenanceHandle;
use hk_model::{Detection, DetectionId};

use crate::integrated::IntegratedEvaluation;

/// Why a box was emitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    /// The component ended and the gap-merge window passed.
    Ended,
    /// Still open after `max_duration_s`: emitted, and the same component continues as a new box.
    MaxDuration,
    /// A retune, gain/provenance change, rate change, gap or floor-segment reset closed it.
    Transition,
    /// [`Detector::finish`](crate::Detector::finish).
    EndOfStream,
}

/// Emitter-candidate status at emission (S4 §5 "Emitter confirmation"). Later confirmations of an
/// already-emitted detection arrive as [`DetectorEvent::Confirmed`]; nothing is filtered.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Candidate {
    /// An isolated box: not (yet) an emitter candidate.
    Unconfirmed,
    /// Repeats an earlier box at a consistent frequency.
    Repeat {
        /// The earlier detection.
        with: DetectionId,
    },
    /// Overlaps an emitter in the ≥ 1 s integrated spectrum (seed +6 dB, extend +3 dB).
    Integrated {
        /// Integrated emitter extent, Hz.
        f_lo_hz: f64,
        /// See `f_lo_hz`.
        f_hi_hz: f64,
    },
}

impl Candidate {
    /// Confirmed by either rule.
    pub fn is_confirmed(&self) -> bool {
        !matches!(self, Candidate::Unconfirmed)
    }
}

/// Why an emitted detection became an emitter candidate after the fact.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ConfirmReason {
    /// A later box repeated it.
    Repeat {
        /// The later detection.
        with: DetectionId,
    },
    /// A ≥ 1 s integrated evaluation found an emitter over it.
    Integrated {
        /// Integrated emitter extent, Hz.
        f_lo_hz: f64,
        /// See `f_lo_hz`.
        f_hi_hz: f64,
    },
}

/// A detection emitted earlier is now an emitter candidate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Confirmation {
    /// The detection.
    pub detection: DetectionId,
    /// Why.
    pub reason: ConfirmReason,
}

/// Evidence behind `image_candidate` (rule 5).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImageEvidence {
    /// Mirror frequency `2fc − f`, Hz.
    pub source_hz: f64,
    /// How much stronger the mirror is, dB (peak mean excess ratio).
    pub rejection_db: f64,
    /// Mirrored-shape correlation of the dB ratio profiles, when there are enough bins.
    pub shape_correlation: Option<f64>,
}

/// One emitted detection.
#[derive(Clone, Debug, PartialEq)]
pub struct DetectionRecord {
    /// The immutable docs/07 row. `provenance_ref` is the handle's id; a writer that interns
    /// provenance by value substitutes the stored id ([`crate::DetectionWriter`]).
    pub detection: Detection,
    /// Provenance in force for every frame of the box.
    pub provenance: ProvenanceHandle,
    /// Detector segment (between transitions).
    pub segment: u64,
    /// Bin extent `[lo, hi)`.
    pub bins: Range<usize>,
    /// Box lower edge, Hz.
    pub f_lo_hz: f64,
    /// Box upper edge, Hz.
    pub f_hi_hz: f64,
    /// Detector frame indices `[first, last + 1)`.
    pub frames: Range<u64>,
    /// Stream sample indices spanned `[first frame start, last frame end)`.
    pub samples: Range<u64>,
    /// Detected cells.
    pub pixels: u64,
    /// Why it was emitted.
    pub close: CloseReason,
    /// Emitted at `max_duration_s` and still continuing.
    pub continues: bool,
    /// Emitter-candidate status at emission.
    pub candidate: Candidate,
    /// Image evidence when `image_candidate`.
    pub image: Option<ImageEvidence>,
    /// The reference harmonic for `spur_reason = ref-harmonic`, Hz.
    pub spur_harmonic_hz: Option<f64>,
    /// Raw components merged into this record (gap merges; members of an impulsive event).
    pub merged_boxes: u32,
    /// A trust test was inconclusive (invalid floor frames, image test impossible).
    pub inconclusive: bool,
}

impl DetectionRecord {
    /// Start, seconds from stream sample 0 at `sample_rate_hz`.
    pub fn t_start_s(&self, sample_rate_hz: f64) -> f64 {
        self.samples.start as f64 / sample_rate_hz
    }

    /// End, seconds from stream sample 0.
    pub fn t_end_s(&self, sample_rate_hz: f64) -> f64 {
        self.samples.end as f64 / sample_rate_hz
    }
}

/// Everything the detector emits, in order.
///
/// Events are handed to the callback by value and never stored by the detector, so the large
/// `Detection` variant is not boxed (a box would be a second allocation per detection).
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum DetectorEvent<'a> {
    /// A detection.
    Detection(DetectionRecord),
    /// An earlier detection became an emitter candidate.
    Confirmed(Confirmation),
    /// An integrated-spectrum evaluation (every block, and at segment end).
    Integrated(&'a IntegratedEvaluation),
}
