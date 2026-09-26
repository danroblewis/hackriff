//! Tracks into the signal inventory (C27, T-018): builds the [`Fingerprint`] and [`Sighting`] of a
//! finished track or hop set and hands it to `Repository::record_sighting`, which clusters it
//! (rules in [`hk_model::cluster`]).
//!
//! - A closed channel track is one sighting keyed by its track id; `count` is its burst count
//!   (tone lobes and split continuations count once), so re-offering it never double-counts.
//! - A channel track that belongs to a hop set is not offered on its own: the hop set (keyed by
//!   the hop-set aggregate id, re-offered as it grows) is the emitter.
//! - A track merged into another is skipped; its bursts belong to the survivor.
//! - Features: centre/bandwidth, duty cycle, period (only with fold confidence ≥
//!   [`MIN_PERIOD_CONFIDENCE`]), median burst length; hop raster and channel set for hop sets.
//!   Callers add C13–C15 estimates (family, symbol rate, deviation) before recording.
//! - Tracks whose members are mostly suspect (spur, image, IMD, clipping) get the
//!   [`SUSPECT_ARTIFACT_TAG`] so the inventory can filter ghosts.

use hk_model::detection::SpurReason;
use hk_model::{
    Fingerprint, KnownStatusPrior, LinkTarget, RepoError, Repository, Resolution, Sighting,
    TrackState,
};

use super::events::{HopSetSummary, TrackEvent, TrackSummary};

/// Tag for clusters built from mostly suspect detections.
pub const SUSPECT_ARTIFACT_TAG: &str = "suspect-artifact";

/// Smallest periodicity-fold confidence whose period enters the fingerprint.
pub const MIN_PERIOD_CONFIDENCE: f64 = 0.5;

/// Share of suspect members above which a track is tagged [`SUSPECT_ARTIFACT_TAG`].
pub const SUSPECT_FRACTION: f64 = 0.5;

/// T-948: widest a track may measure, in analysis bins, and still be refused as a receiver line.
///
/// The same number and the same reasoning as the confirmation policy's
/// `ConfirmPolicy::min_live_bandwidth_bins` (T-403), read in the other direction: a
/// receiver-generated line is CW, so the only width it can measure is the analysis window's own —
/// 2 to 4 bins, measured flat across 28 dB of level, because the OBW99 of a windowed tone is a
/// property of the window and not of the tone. Eight bins is twice the widest a tone can measure,
/// and a modulated emission is well past it (the WFM scene measures 14 bins).
///
/// Its job is to stop this rule from ever refusing a *modulated* emission that merely overlaps a
/// spur: an emission wider than this is admitted however its members were flagged, and is judged
/// by the suspect-artifact tag and the confirmation policy exactly as before.
pub const ARTIFACT_MAX_BINS: f64 = 8.0;

/// T-948: **the receiver's own line, refused admission to the inventory.** `Some(reason)` when
/// every member detection of the track was attributed to the receiver
/// ([`TrackSummary::artifact_detections`]) *and* the track is CW-narrow
/// ([`ARTIFACT_MAX_BINS`]) — the DC/LO-leakage spike at the tuned centre, a reference harmonic, a
/// clock harmonic, a comb tooth or a listed spur-map entry.
///
/// **Why at admission, and not by ranking afterwards.** The explorer (T-948) found the DC point
/// listed as a candidate at the exact tuned centre on *every* tune, and fixed spurs listed as
/// candidates carrying band-plan explanations — a receiver artefact dressed as an emission, with a
/// suggestion attached. The DC notch is declared *excluded from analysis* in the coverage map
/// (`hk_store::coverage`, T-595): inside it the receiver has no measurement to offer, so it must
/// not offer a signal either. The same holds for a spur: the arithmetic that named it is a
/// statement that this receiver made the line.
///
/// **Nothing is dropped.** The detections and their reasons are stored exactly as measured; this
/// refuses only the *inventory entry*, and the reason comes back with the refusal so the line can
/// be explained ("the receiver's own DC spike") rather than vanishing silently.
///
/// **What it cannot refuse:** anything wider than [`ARTIFACT_MAX_BINS`]; any track with even one
/// member the rules did not attribute to the receiver; and a DC flag a clean twin from another
/// tuning refuted (T-174), which the tracker has already subtracted.
pub fn receiver_artifact(summary: &TrackSummary) -> Option<SpurReason> {
    let n = summary.track.detection_count;
    if n == 0 || summary.artifact_detections < n {
        return None;
    }
    let (bw, bin) = (summary.track.bandwidth_hz, summary.bin_hz);
    if !(bin.is_finite() && bin > 0.0 && bw.is_finite()) {
        // A missing resolution cannot establish that the width is a window's: refuse to refuse.
        return None;
    }
    (bw <= ARTIFACT_MAX_BINS * bin).then(|| summary.artifact_reason.unwrap_or(SpurReason::Dc))
}

/// T-948: a centre frequency an emitter could actually have been transmitting on.
///
/// The explorer found a candidate at **−0.598 MHz**. It is not a sign error in one step's mapping:
/// a tuning near the bottom of the device's range puts the lower half of the baseband *below
/// 0 Hz* (1.0 MHz centre at 2.4 Msps reaches −0.2 MHz), the edge rule bounds the span by the
/// filter and the bin count but not by DC, and whatever energy aliases in there gets a frequency
/// from the same linear bin→Hz map as everything else. No emitter transmits there, so nothing that
/// claims to is an emitter: it is refused at admission, where the impossible value is cheap to see
/// and has not yet been clustered, explained against a band plan or offered to a chain.
fn plausible_centre(f_center_hz: f64, bandwidth_hz: f64) -> bool {
    f_center_hz.is_finite() && f_center_hz > 0.0 && bandwidth_hz.is_finite() && bandwidth_hz >= 0.0
}

/// Fingerprint of a channel track.
pub fn track_fingerprint(summary: &TrackSummary) -> Fingerprint {
    let mut fp = Fingerprint::from_track(&summary.track);
    fp.period_s = summary
        .period
        .as_ref()
        .filter(|p| p.confidence >= MIN_PERIOD_CONFIDENCE)
        .map(|p| p.period_s);
    fp.burst_length_s = summary.burst_length.as_ref().map(|d| d.p50_s);
    fp.hop_set_hz.clear();
    fp
}

/// Sighting of a finished channel track; `None` for hop-set members, merged tracks, the
/// receiver's own lines ([`receiver_artifact`]) and impossible centres ([`plausible_centre`]) —
/// both T-948.
pub fn track_sighting(summary: &TrackSummary) -> Option<Sighting> {
    if summary.hop_set.is_some()
        || summary.inband_fragment
        || matches!(summary.track.state, TrackState::MergedInto(_))
        || receiver_artifact(summary).is_some()
        || !plausible_centre(summary.track.f_center_hz, summary.track.bandwidth_hz)
    {
        return None;
    }
    let mut s = Sighting::track(&summary.track, track_fingerprint(summary));
    s.count = summary.burst_count;
    if summary.suspect_fraction > SUSPECT_FRACTION {
        s.tags.push(SUSPECT_ARTIFACT_TAG.into());
    }
    Some(s)
}

/// Sighting of a hop set: the channel span as its band, `count` = hops linked.
pub fn hop_set_sighting(h: &HopSetSummary) -> Sighting {
    let lo = h.channels_hz.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = h
        .channels_hz
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);
    let (center, width) = if lo.is_finite() && hi.is_finite() {
        ((lo + hi) / 2.0, hi - lo + h.raster_hz.unwrap_or(0.0))
    } else {
        (0.0, 0.0)
    };
    let fp = Fingerprint {
        hop_raster_hz: h.raster_hz,
        hop_set_hz: h.channels_hz.clone(),
        burst_length_s: h.dwell_s,
        ..Fingerprint::new(center, width)
    };
    Sighting {
        source: LinkTarget::Track(h.id),
        seen: h.time,
        count: h.hops,
        f_center_hz: center,
        bandwidth_hz: width,
        fingerprint: Some(fp),
        identity: None,
        context: None,
        classification: None,
        tags: Vec::new(),
    }
}

/// Records the inventory sighting a tracker event carries (closed tracks and hop sets), if any.
pub fn record_track_event(
    repo: &mut Repository,
    event: &TrackEvent,
    priors: Option<&dyn KnownStatusPrior>,
) -> Result<Option<Resolution>, RepoError> {
    let sighting = match event {
        TrackEvent::Closed(summary) => track_sighting(summary),
        TrackEvent::HopSetFormed(h) | TrackEvent::HopSetClosed(h) => Some(hop_set_sighting(h)),
        _ => None,
    }
    // T-948: the same admission rule for a hop set, whose centre is derived from its channels
    // (and is 0.0 when they are not all finite) rather than measured.
    .filter(|s| plausible_centre(s.f_center_hz, s.bandwidth_hz));
    sighting
        .map(|s| repo.record_sighting(&s, priors))
        .transpose()
}
