//! The inventory seam (C27): where track and decode results go to the signal inventory.
//!
//! The pipeline persists Tracks (hk-detect `TrackBatch`) and the rows the chains' record writers
//! create (`hk_demod::write_session`, `write_framed_bursts`, plugin `Ingest`, which resolve
//! decoded identities through the repository). Clustering tracks into emitters is T-018's
//! `record_track_event` → `Repository::record_sighting`; [`TrackInventory`] (the default) calls
//! it for closed tracks and hop sets, with the bundled 47 CFR 2.106 band table as the
//! known-status prior. Another policy plugs in through [`Inventory`] without touching the
//! composition. Reads go through `Repository::query_inventory` only (identities gated).
//!
//! **Capture name (T-034 replay dedup).** The detection writer passes the run's stable capture
//! name ([`Inventory::capture_name`]) before the first event. [`TrackInventory`] then offers
//! track and hop-set sightings as re-measurements (`record_sighting_measured` with producer
//! [`TRACK_PRODUCER`] and that capture), so replaying the same IQ again does not count its
//! tracks twice, while two different captures with the same timestamps (synthetic scenes all
//! start at the same instant) stay separate.

use hk_context::{BandTable, Region as BandRegion, match_known_status};
use hk_detect::TrackEvent;
use hk_detect::track::inventory::{hop_set_sighting, record_track_event, track_sighting};
use hk_model::{
    EmitterId, KnownStatusPrior, MeasurementKey, PriorVerdict, RepoError, Repository, TrackId,
};

/// Producer name of track and hop-set sightings in a [`MeasurementKey`].
pub const TRACK_PRODUCER: &str = "hk-track";

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
}

/// Leaves the inventory to the chains' record writers (no track clustering).
#[derive(Debug, Default)]
pub struct NullInventory;

impl Inventory for NullInventory {}

/// T-018 clustering for closed tracks and hop sets, with band-plan priors.
pub struct TrackInventory {
    table: Option<BandTable>,
    capture: Option<String>,
    /// Sightings recorded.
    pub sightings: u64,
    /// Emitters created by sightings.
    pub created: u64,
}

impl Default for TrackInventory {
    fn default() -> Self {
        Self {
            table: BandTable::bundled(BandRegion::Us).ok(),
            capture: None,
            sightings: 0,
            created: 0,
        }
    }
}

impl TrackInventory {
    /// One sighting: a re-measurement keyed by the capture name when one was given, else the
    /// plain T-018 path.
    fn record(
        &self,
        repo: &mut Repository,
        event: &TrackEvent,
        priors: Option<&dyn KnownStatusPrior>,
    ) -> Result<Option<hk_model::Resolution>, RepoError> {
        let Some(capture) = &self.capture else {
            return record_track_event(repo, event, priors);
        };
        let sighting = match event {
            TrackEvent::Closed(summary) => track_sighting(summary),
            TrackEvent::HopSetFormed(h) | TrackEvent::HopSetClosed(h) => Some(hop_set_sighting(h)),
            _ => None,
        };
        let key = MeasurementKey {
            producer: TRACK_PRODUCER.into(),
            capture: Some(capture.clone()),
        };
        sighting
            .map(|s| repo.record_sighting_measured(&s, &key, priors))
            .transpose()
    }
}

impl Inventory for TrackInventory {
    fn capture_name(&mut self, name: &str) {
        self.capture = Some(name.to_owned());
    }

    fn track_event(&mut self, repo: &mut Repository, event: &TrackEvent) -> Result<(), RepoError> {
        let resolution = match &self.table {
            Some(table) => {
                let prior = |family: &str, f: f64, bw: f64| {
                    let m = match_known_status(table, family, f, bw);
                    PriorVerdict {
                        status: m.status,
                        prior_ref: m.prior_ref,
                        reason: m.reason,
                    }
                };
                self.record(repo, event, Some(&prior))?
            }
            None => self.record(repo, event, None)?,
        };
        if let Some(r) = resolution {
            self.sightings += 1;
            self.created += u64::from(r.created);
        }
        Ok(())
    }
}
