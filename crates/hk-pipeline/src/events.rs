//! Events from the detection reader to the control thread (event rate, never sample rate).

use std::ops::Range;

use hk_detect::{CaptureResult, TrackSummary};
use hk_model::{DetectionId, Timestamp, TrackId};

use crate::chains::spec::ChainSpec;

/// One member detection box of a track.
#[derive(Clone, Debug, PartialEq)]
pub struct MemberBox {
    /// Detection.
    pub detection: DetectionId,
    /// Stream sample indices spanned.
    pub samples: Range<u64>,
    /// Lower edge, Hz.
    pub f_lo_hz: f64,
    /// Upper edge, Hz.
    pub f_hi_hz: f64,
    /// Start time.
    pub t_start: Timestamp,
    /// Emitted at the detector's max duration and still continuing (a steady carrier).
    pub continues: bool,
    /// Mean SNR of the detection, dB (T-128 candidate evidence).
    pub snr_db: f64,
    /// The detection is suspect by the §2.6 rule (IMD, spur, confirmed image, clipped, compressed;
    /// T-128).
    pub suspect: bool,
}

/// What a chain is attached for.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    /// Track, for track-triggered chains.
    pub track: Option<TrackId>,
    /// The confirming detection.
    pub detection: Option<DetectionId>,
    /// Lower edge, Hz.
    pub f_lo_hz: f64,
    /// Upper edge, Hz.
    pub f_hi_hz: f64,
    /// First sample of the track (pre-trigger reference).
    pub first_sample: u64,
    /// Trigger sample (end of the confirming box).
    pub trigger_sample: u64,
    /// Burstiness from the member boxes so far.
    pub bursty: Option<bool>,
}

/// Control-thread input.
#[derive(Debug)]
pub enum ControlEvent {
    /// A track got its first trustworthy confirmed member.
    TrackConfirmed(Candidate),
    /// A detection was linked to a track.
    Member {
        /// Track.
        track: TrackId,
        /// Box.
        member: MemberBox,
    },
    /// A track closed.
    TrackClosed {
        /// Track.
        track: TrackId,
        /// Final summary.
        summary: Box<TrackSummary>,
    },
    /// `from` merged into `into`.
    TrackMerged {
        /// Absorbed.
        from: TrackId,
        /// Survivor.
        into: TrackId,
    },
    /// The capture result of a detector segment that just ended (verification captures).
    Capture {
        /// Segment start.
        t_start: Timestamp,
        /// The capture.
        capture: Box<CaptureResult>,
    },
    /// Attach `spec` for `candidate` now (API / tests).
    Manual {
        /// Spec.
        spec: Box<ChainSpec>,
        /// Candidate.
        candidate: Candidate,
    },
    /// Detach every manually attached chain.
    DetachManual,
    /// The detection reader has flushed everything.
    DetectFinished,
}
