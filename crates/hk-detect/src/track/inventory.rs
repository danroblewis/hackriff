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

/// Sighting of a finished channel track; `None` for hop-set members and merged tracks.
pub fn track_sighting(summary: &TrackSummary) -> Option<Sighting> {
    if summary.hop_set.is_some()
        || summary.inband_fragment
        || matches!(summary.track.state, TrackState::MergedInto(_))
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
    };
    sighting
        .map(|s| repo.record_sighting(&s, priors))
        .transpose()
}
