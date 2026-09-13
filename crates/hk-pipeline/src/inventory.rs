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
//! Another policy plugs in through [`Inventory`] without touching the composition. Reads go
//! through `Repository::query_inventory` only (identities gated).

use hk_context::{BandTable, Region as BandRegion};
use hk_detect::TrackEvent;
use hk_detect::track::inventory::{hop_set_sighting, track_sighting};
use hk_model::{EmitterId, RepoError, Repository, TrackId};

use crate::family::{explain_emitter, track_family};

/// Receives inventory-relevant results, under the repository lock.
pub trait Inventory: Send {
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

/// T-018 clustering for closed tracks and hop sets, with T-039 family explanations and priors.
pub struct TrackInventory {
    table: Option<BandTable>,
    /// Sightings recorded.
    pub sightings: u64,
    /// Emitters created by sightings.
    pub created: u64,
}

impl Default for TrackInventory {
    fn default() -> Self {
        Self {
            table: BandTable::bundled(BandRegion::Us).ok(),
            sightings: 0,
            created: 0,
        }
    }
}

impl Inventory for TrackInventory {
    fn track_event(&mut self, repo: &mut Repository, event: &TrackEvent) -> Result<(), RepoError> {
        let sighting = match event {
            TrackEvent::Closed(summary) => track_sighting(summary).map(|mut s| {
                s.classification = track_family(summary).classification(s.seen.end);
                s
            }),
            TrackEvent::HopSetFormed(h) | TrackEvent::HopSetClosed(h) => Some(hop_set_sighting(h)),
            _ => None,
        };
        let Some(sighting) = sighting else {
            return Ok(());
        };
        let r = repo.record_sighting(&sighting, None)?;
        self.sightings += 1;
        self.created += u64::from(r.created);
        if let Some(table) = &self.table {
            explain_emitter(repo, table, r.emitter_id)?;
        }
        Ok(())
    }

    fn chain_emitter(
        &mut self,
        repo: &mut Repository,
        _track: Option<TrackId>,
        emitter: EmitterId,
    ) -> Result<(), RepoError> {
        if let Some(table) = &self.table {
            explain_emitter(repo, table, emitter)?;
        }
        Ok(())
    }
}
