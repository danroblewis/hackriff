//! The inventory seam (C27): where track and decode results go to the signal inventory.
//!
//! The pipeline persists Tracks (hk-detect `TrackBatch`) and the rows the chains' record writers
//! create (`hk_demod::write_session`, `write_framed_bursts`, plugin `Ingest`, which resolve
//! decoded identities through the repository). Clustering tracks into emitters is T-018's
//! `record_track_event` → `Repository::record_sighting`; [`TrackInventory`] (the default) calls
//! it for closed tracks and hop sets, with the bundled 47 CFR 2.106 band table as the
//! known-status prior. Another policy plugs in through [`Inventory`] without touching the
//! composition. Reads go through `Repository::query_inventory` only (identities gated).

use hk_context::{BandTable, Region as BandRegion, match_known_status};
use hk_detect::TrackEvent;
use hk_detect::track::inventory::record_track_event;
use hk_model::{EmitterId, PriorVerdict, RepoError, Repository, TrackId};

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

/// T-018 clustering for closed tracks and hop sets, with band-plan priors.
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
                record_track_event(repo, event, Some(&prior))?
            }
            None => record_track_event(repo, event, None)?,
        };
        if let Some(r) = resolution {
            self.sightings += 1;
            self.created += u64::from(r.created);
        }
        Ok(())
    }
}
