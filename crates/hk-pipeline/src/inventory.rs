//! The inventory seam (C27): where track and decode results go to the signal inventory.
//!
//! The pipeline persists Tracks (hk-detect `TrackBatch`) and the rows the chains' record writers
//! create (`hk_demod::write_session`, `write_framed_bursts`, plugin `Ingest`, which resolve
//! decoded identities through the repository). Clustering tracks into emitters is T-018's
//! `Repository::record_sighting`. [`TrackInventory`] (the default) calls it for closed tracks
//! and hop sets.
//!
//! T-039 ([`crate::family`]) adds the family step:
//! - A closed track whose occupancy maps to a service family carries that family as a
//!   Classification.
//! - Every emitter a sighting or a chain ([`Inventory::chain_emitter`]) touches gets its ranked
//!   explanations and its known status from the top one ([`explain_emitter`], bundled
//!   47 CFR 2.106 band table).
//!
//! **Capture name (T-034 replay dedup, T-037b).** The detection writer passes the run's stable
//! capture name ([`Inventory::capture_name`]) before the first event. [`TrackInventory`] then
//! offers track and hop-set sightings as re-measurements (`record_sighting_measured` with producer
//! [`TRACK_PRODUCER`] and that capture), so replaying the same IQ again does not count its tracks
//! twice, while two different captures with the same timestamps (synthetic scenes all start at
//! the same instant) stay separate.
//!
//! **Candidates and confirmed (T-078).** Every emitter enters the inventory as a `candidate`.
//! After each sighting or chain write, [`TrackInventory`] reviews the candidate against its
//! [`ConfirmPolicy`] and confirms it (author `auto`, actor [`CONFIRM_RULE`], the evidence as the
//! reason) only on strong, unambiguous, blind evidence:
//! - **Decoded identity:** the emitter holds a transmitter identity (RDS PI, ADS-B ICAO, …) carried
//!   by at least `min_valid_decodes` CRC-valid decodes. Structural identities
//!   (`structural_schemes`, e.g. the blind framer's `other:hk-framing` signature) are not
//!   transmitter identities and do not count.
//! - **Continuous and trusted:** a closed track with at least `min_on_air_s` on air, duty cycle at
//!   least `min_duty_cycle`, at most `max_suspect_fraction` suspect members (spur, image, IMD,
//!   clipping) and at least `min_confirmed_detections` trust-confirmed detections.
//!
//! Intermittent, weak or suspect emitters stay candidates until a user promotes them. No rule ever
//! deletes; user deletion and the re-detection rule are the repository's (`hk_model` lifecycle).
//!
//! **One entry per physical emitter (T-082).** A chain's decoder output of an emission the tracker
//! also followed (RDS PI on a WFM track, the blind framer's signature on an FSK sensor track) would
//! otherwise be a second entry. After every sighting and chain write, [`TrackInventory`] merges
//! the entry with its same-emission partners (`Repository::same_emission_partners`: overlapping
//! centre or refined centre, overlapping time, hop compatibility; rules in `hk_model::cluster`)
//! when both came from this run (the same capture), before explaining and reviewing the survivor.
//! Entries carrying a channel-sharing transmitter identity (ADS-B ICAO, …: many transmitters per
//! channel) are never linked to a channel entry; structural identities (`structural_schemes`)
//! are.
//!
//! Another policy plugs in through [`Inventory`] without touching the composition. Reads go
//! through `Repository::query_inventory` only (identities gated).

use std::collections::{HashSet, VecDeque};

use hk_context::{BandTable, Region as BandRegion};
use hk_detect::TrackEvent;
use hk_detect::track::TrackSummary;
use hk_detect::track::inventory::{hop_set_sighting, track_sighting};
use hk_model::{
    EmitterId, IdentityScheme, LifecycleAuthor, LifecycleState, MeasurementKey, RepoError,
    Repository, Sighting, Tolerances, TrackId,
};
use serde::{Deserialize, Serialize};

use crate::family::{explain_emitter, track_family};

/// Producer name of track and hop-set sightings in a [`MeasurementKey`].
pub const TRACK_PRODUCER: &str = "hk-track";

/// Actor of automatic confirmations (rule id and version) in the lifecycle history.
pub const CONFIRM_RULE: &str = "hk-pipeline/confirm@1";

/// Reason of same-emission merges (T-082) in the merge record.
pub const SAME_EMISSION_REASON: &str =
    "same emission: track and decoder entries of one emitter (hk-pipeline/link@1)";

/// Entries of the current run remembered for same-emission linking.
const RUN_MEMORY: usize = 8192;

/// Receives inventory-relevant results, under the repository lock.
pub trait Inventory: Send {
    /// The run's stable capture name (content-derived, identical on every replay of the same
    /// IQ), given once before the first event.
    fn capture_name(&mut self, _name: &str) {}

    /// A tracker event whose Track row and links are already stored (closes, hop sets).
    fn track_event(
        &mut self,
        _repo: &mut Repository,
        _event: &TrackEvent,
    ) -> Result<(), RepoError> {
        Ok(())
    }

    /// A chain attached for `track` wrote rows under `emitter`.
    fn chain_emitter(
        &mut self,
        _repo: &mut Repository,
        _track: Option<TrackId>,
        _emitter: EmitterId,
    ) -> Result<(), RepoError> {
        Ok(())
    }

    /// An open, confirmed channel track whose Track row is stored, offered before it closes
    /// (T-109: a channel that never idles out would otherwise enter the inventory only at stop).
    fn live_track(
        &mut self,
        _repo: &mut Repository,
        _summary: &TrackSummary,
    ) -> Result<(), RepoError> {
        Ok(())
    }
}

/// Leaves the inventory to the chains' record writers (no track clustering).
#[derive(Debug, Default)]
pub struct NullInventory;

impl Inventory for NullInventory {}

/// Thresholds of the automatic confirmation rule (module docs). Deserialisable so a run
/// configuration can carry it; missing fields take the defaults.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfirmPolicy {
    /// Run the rule at all (false: every emitter stays a candidate until promoted).
    pub enabled: bool,
    /// Confirm on a decoded transmitter identity.
    pub identity: bool,
    /// CRC-valid decodes carrying the identity needed. Default 1.
    pub min_valid_decodes: u64,
    /// Identity schemes that describe structure, not a transmitter. Default `other:hk-framing`.
    pub structural_schemes: Vec<String>,
    /// Confirm on one continuous, trusted track.
    pub continuous: bool,
    /// Time on air of the track, s. Default 2.
    pub min_on_air_s: f64,
    /// Duty cycle of the track (on air / observed). Default 0.8.
    pub min_duty_cycle: f64,
    /// Largest share of suspect member detections. Default 0.5 (the suspect-artifact tag rule).
    pub max_suspect_fraction: f64,
    /// Trust-confirmed member detections needed. Default 1.
    pub min_confirmed_detections: u64,
}

impl Default for ConfirmPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            identity: true,
            min_valid_decodes: 1,
            structural_schemes: vec!["other:hk-framing".into()],
            continuous: true,
            min_on_air_s: 2.0,
            min_duty_cycle: 0.8,
            max_suspect_fraction: hk_detect::track::inventory::SUSPECT_FRACTION,
            min_confirmed_detections: 1,
        }
    }
}

/// Trust and occupancy of a closed track.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackTrust {
    /// On-air time, s.
    pub on_air_s: f64,
    /// On air / observed.
    pub duty_cycle: Option<f64>,
    /// Share of suspect member detections.
    pub suspect_fraction: f64,
    /// Trust-confirmed member detections.
    pub confirmed_detections: u64,
}

impl TrackTrust {
    /// From a tracker summary.
    pub fn of(summary: &TrackSummary) -> Self {
        Self {
            on_air_s: summary.on_time_s,
            duty_cycle: (summary.observed_s > 0.0)
                .then(|| (summary.on_time_s / summary.observed_s).min(1.0)),
            suspect_fraction: summary.suspect_fraction,
            confirmed_detections: summary.confirmed_detections,
        }
    }
}

/// What the rule looks at for one candidate.
#[derive(Clone, Debug, PartialEq)]
pub struct ConfirmEvidence {
    /// Identity scheme and CRC-valid decodes carrying it (`Repository::identity_decode_evidence`).
    pub identity: Option<(IdentityScheme, u64)>,
    /// The track just closed for this emitter, if the review follows a track close.
    pub track: Option<TrackTrust>,
}

impl ConfirmPolicy {
    /// The confirmation reason, or `None` (stay a candidate).
    pub fn decide(&self, ev: &ConfirmEvidence) -> Option<String> {
        if !self.enabled {
            return None;
        }
        if self.identity
            && let Some((scheme, n)) = &ev.identity
            && !self.structural_schemes.contains(&scheme.as_string())
            && *n >= self.min_valid_decodes.max(1)
        {
            return Some(format!(
                "decoded identity ({scheme}) carried by {n} CRC-valid decode(s)"
            ));
        }
        if self.continuous
            && let Some(tr) = ev.track
            && let Some(duty) = tr.duty_cycle
            && tr.on_air_s >= self.min_on_air_s
            && duty >= self.min_duty_cycle
            && tr.suspect_fraction <= self.max_suspect_fraction
            && tr.confirmed_detections >= self.min_confirmed_detections
        {
            return Some(format!(
                "continuous and trusted: {:.1} s on air, duty cycle {duty:.2}, {:.0} % suspect, \
                 {} trust-confirmed detection(s)",
                tr.on_air_s,
                tr.suspect_fraction * 100.0,
                tr.confirmed_detections
            ));
        }
        None
    }
}

/// T-018 clustering for closed tracks and hop sets, with T-039 family explanations and priors,
/// and T-078 automatic confirmation.
pub struct TrackInventory {
    table: Option<BandTable>,
    capture: Option<String>,
    policy: ConfirmPolicy,
    /// Entries this run's sightings and chains reached, oldest first (bounded).
    run: VecDeque<EmitterId>,
    run_set: HashSet<EmitterId>,
    /// Sightings recorded.
    pub sightings: u64,
    /// Emitters created by sightings.
    pub created: u64,
    /// Candidates confirmed by the rule.
    pub confirmed: u64,
    /// Same-emission merges (T-082).
    pub merged: u64,
}

impl Default for TrackInventory {
    fn default() -> Self {
        Self::with_policy(ConfirmPolicy::default())
    }
}

impl TrackInventory {
    /// The default inventory with another confirmation policy.
    pub fn with_policy(policy: ConfirmPolicy) -> Self {
        Self {
            table: BandTable::bundled(BandRegion::Us).ok(),
            capture: None,
            policy,
            run: VecDeque::new(),
            run_set: HashSet::new(),
            sightings: 0,
            created: 0,
            confirmed: 0,
            merged: 0,
        }
    }

    /// Whether `id` may be linked to another entry of its emission: not when it carries a
    /// channel-sharing transmitter identity (structural identities may).
    fn linkable(&self, repo: &Repository, id: EmitterId) -> Result<bool, RepoError> {
        Ok(match repo.identity_decode_evidence(id)? {
            Some((scheme, _)) => {
                !scheme.shares_channel()
                    || self.policy.structural_schemes.contains(&scheme.as_string())
            }
            None => true,
        })
    }

    fn remember(&mut self, id: EmitterId) {
        if self.run_set.insert(id) {
            self.run.push_back(id);
            if self.run.len() > RUN_MEMORY
                && let Some(old) = self.run.pop_front()
            {
                self.run_set.remove(&old);
            }
        }
    }

    /// T-082: merges `emitter` with this run's same-emission partners; returns the live survivor.
    fn link(&mut self, repo: &mut Repository, emitter: EmitterId) -> Result<EmitterId, RepoError> {
        let mut id = repo.live_emitter_id(emitter)?;
        let tol = Tolerances::default();
        for (partner, _) in repo.same_emission_partners(id, &tol)? {
            if !self.run_set.contains(&partner) {
                continue;
            }
            if !self.linkable(repo, id)? {
                break;
            }
            if !self.linkable(repo, partner)? {
                continue;
            }
            let t = repo
                .emitter(id)?
                .last_seen
                .max(repo.emitter(partner)?.last_seen);
            match repo.merge_same_emission(id, partner, t, SAME_EMISSION_REASON, &tol) {
                Ok(Some(m)) => {
                    id = m.into;
                    self.merged += 1;
                }
                Ok(None) | Err(RepoError::IdentityConflict { .. }) => {}
                Err(e) => return Err(e),
            }
        }
        self.remember(id);
        Ok(id)
    }

    /// The confirmation policy in force.
    pub fn policy(&self) -> &ConfirmPolicy {
        &self.policy
    }

    /// Reviews a candidate against the policy; confirms it when the evidence holds.
    fn review(
        &mut self,
        repo: &mut Repository,
        emitter: EmitterId,
        track: Option<TrackTrust>,
    ) -> Result<(), RepoError> {
        if !self.policy.enabled {
            return Ok(());
        }
        let id = repo.live_emitter_id(emitter)?;
        if repo.emitter_lifecycle_state(id)? != LifecycleState::Candidate {
            return Ok(());
        }
        let evidence = ConfirmEvidence {
            identity: repo.identity_decode_evidence(id)?,
            track,
        };
        let Some(reason) = self.policy.decide(&evidence) else {
            return Ok(());
        };
        let t = repo.emitter(id)?.last_seen;
        if repo
            .change_emitter_lifecycle(
                id,
                LifecycleState::Confirmed,
                LifecycleAuthor::Auto,
                CONFIRM_RULE,
                &reason,
                t,
            )?
            .is_some()
        {
            self.confirmed += 1;
        }
        Ok(())
    }
}

impl Inventory for TrackInventory {
    fn capture_name(&mut self, name: &str) {
        self.capture = Some(name.to_owned());
    }

    fn track_event(&mut self, repo: &mut Repository, event: &TrackEvent) -> Result<(), RepoError> {
        let (sighting, trust) = match event {
            TrackEvent::Closed(summary) => (
                track_sighting(summary).map(|mut s| {
                    s.classification = track_family(summary).classification(s.seen.end);
                    s
                }),
                Some(TrackTrust::of(summary)),
            ),
            TrackEvent::HopSetFormed(h) | TrackEvent::HopSetClosed(h) => {
                (Some(hop_set_sighting(h)), None)
            }
            _ => (None, None),
        };
        let Some(sighting) = sighting else {
            return Ok(());
        };
        self.offer(repo, &sighting, trust)
    }

    fn live_track(
        &mut self,
        repo: &mut Repository,
        summary: &TrackSummary,
    ) -> Result<(), RepoError> {
        if summary.closed.is_some() {
            return Ok(());
        }
        let Some(mut sighting) = track_sighting(summary) else {
            return Ok(());
        };
        sighting.classification = track_family(summary).classification(sighting.seen.end);
        // No auto-confirmation on a partial life: the close re-offers with the full evidence.
        self.offer(repo, &sighting, None)
    }

    fn chain_emitter(
        &mut self,
        repo: &mut Repository,
        _track: Option<TrackId>,
        emitter: EmitterId,
    ) -> Result<(), RepoError> {
        let id = self.link(repo, emitter)?;
        if let Some(table) = &self.table {
            explain_emitter(repo, table, id)?;
        }
        self.review(repo, id, None)
    }
}

impl TrackInventory {
    /// Records a track or hop-set sighting, links it and reviews its lifecycle.
    fn offer(
        &mut self,
        repo: &mut Repository,
        sighting: &Sighting,
        trust: Option<TrackTrust>,
    ) -> Result<(), RepoError> {
        let sighting = sighting.clone();
        let r = match &self.capture {
            Some(capture) => {
                let key = MeasurementKey {
                    producer: TRACK_PRODUCER.into(),
                    capture: Some(capture.clone()),
                };
                repo.record_sighting_measured(&sighting, &key, None)?
            }
            None => repo.record_sighting(&sighting, None)?,
        };
        self.sightings += 1;
        self.created += u64::from(r.created);
        let id = self.link(repo, r.emitter_id)?;
        if let Some(table) = &self.table {
            explain_emitter(repo, table, id)?;
        }
        self.review(repo, id, trust)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_detect::track::CloseCause;
    use hk_model::{InventoryQuery, TimeRange, Timestamp, TimingFeatures, Track, TrackState};

    fn channel_summary(
        id: TrackId,
        end_s: i64,
        bursts: u64,
        closed: Option<CloseCause>,
    ) -> TrackSummary {
        let t0 = Timestamp::from_unix_nanos(1_000_000_000);
        TrackSummary {
            track: Track {
                id,
                state: if closed.is_some() {
                    TrackState::Closed
                } else {
                    TrackState::Open
                },
                split_from: None,
                time: TimeRange::new(t0, Timestamp::from_unix_nanos(end_s * 1_000_000_000)),
                f_center_hz: 152.342e6,
                bandwidth_hz: 12e3,
                detection_count: bursts,
                timing: TimingFeatures::default(),
                updated_at: t0,
            },
            burst_count: bursts,
            on_time_s: 0.9 * bursts as f64,
            observed_s: bursts as f64,
            period: None,
            burst_length: None,
            inter_arrival_cv: None,
            segments: 0,
            hop_set: None,
            inband_fragment: false,
            suspect_fraction: 0.0,
            confirmed_detections: bursts,
            next_burst_eta: None,
            closed,
        }
    }

    #[test]
    fn t109_live_channel_track_enters_the_inventory_before_close_without_double_counting() {
        let mut repo = Repository::open_in_memory().unwrap();
        let mut inv = TrackInventory::default();
        let id = TrackId::new();
        let rows = |repo: &Repository| {
            repo.query_inventory(&InventoryQuery::default())
                .unwrap()
                .entries
                .into_iter()
                .map(|e| (e.emitter.id, e.emitter.count))
                .collect::<Vec<_>>()
        };
        // A repeating channel still open after 5 bursts is catalogued now, not at stop.
        inv.live_track(&mut repo, &channel_summary(id, 6, 5, None))
            .unwrap();
        let first = rows(&repo);
        assert_eq!(first.len(), 1, "{first:?}");
        assert_eq!(first[0].1, 5);
        // Re-offers and the final close of the same track add only the new bursts, to one row.
        inv.live_track(&mut repo, &channel_summary(id, 11, 10, None))
            .unwrap();
        let closed = channel_summary(id, 13, 12, Some(CloseCause::Idle));
        inv.track_event(&mut repo, &TrackEvent::Closed(closed.clone()))
            .unwrap();
        let last = rows(&repo);
        assert_eq!(
            last,
            vec![(first[0].0, 12)],
            "one emitter, counted once per burst"
        );
        // A closed summary is never offered as live.
        let other = channel_summary(TrackId::new(), 13, 12, Some(CloseCause::Idle));
        inv.live_track(&mut repo, &other).unwrap();
        assert_eq!(rows(&repo).len(), 1);
    }

    fn steady() -> TrackTrust {
        TrackTrust {
            on_air_s: 4.9,
            duty_cycle: Some(0.98),
            suspect_fraction: 0.0,
            confirmed_detections: 12,
        }
    }

    #[test]
    fn t078_policy_confirms_only_strong_unambiguous_evidence() {
        let p = ConfirmPolicy::default();
        let none = ConfirmEvidence {
            identity: None,
            track: None,
        };
        assert_eq!(p.decide(&none), None);
        // A transmitter identity with a valid decode confirms; the framer's signature does not.
        let rds = ConfirmEvidence {
            identity: Some((IdentityScheme::RdsPi, 3)),
            track: None,
        };
        assert!(p.decide(&rds).unwrap().contains("rds-pi"));
        let framing = ConfirmEvidence {
            identity: Some((IdentityScheme::Other("hk-framing".into()), 20)),
            track: None,
        };
        assert_eq!(p.decide(&framing), None);
        let no_valid = ConfirmEvidence {
            identity: Some((IdentityScheme::RdsPi, 0)),
            track: None,
        };
        assert_eq!(p.decide(&no_valid), None);
        // Continuous and trusted confirms; intermittent, short, suspect or unverified do not.
        let ev = |track| ConfirmEvidence {
            identity: None,
            track: Some(track),
        };
        assert!(p.decide(&ev(steady())).unwrap().starts_with("continuous"));
        for (what, t) in [
            (
                "intermittent",
                TrackTrust {
                    duty_cycle: Some(0.15),
                    ..steady()
                },
            ),
            (
                "short",
                TrackTrust {
                    on_air_s: 0.5,
                    ..steady()
                },
            ),
            (
                "suspect",
                TrackTrust {
                    suspect_fraction: 0.9,
                    ..steady()
                },
            ),
            (
                "unverified",
                TrackTrust {
                    confirmed_detections: 0,
                    ..steady()
                },
            ),
            (
                "no duty",
                TrackTrust {
                    duty_cycle: None,
                    ..steady()
                },
            ),
        ] {
            assert_eq!(p.decide(&ev(t)), None, "{what}");
        }
        let off = ConfirmPolicy {
            enabled: false,
            ..ConfirmPolicy::default()
        };
        assert_eq!(off.decide(&rds), None);
        let parsed: ConfirmPolicy =
            serde_json::from_value(serde_json::json!({ "min_duty_cycle": 0.5 })).unwrap();
        assert_eq!(parsed.min_duty_cycle, 0.5);
        assert_eq!(parsed.min_on_air_s, 2.0);
    }
}
