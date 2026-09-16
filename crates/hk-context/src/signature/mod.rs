//! C18 fingerprint signatures (T-201, ADR-0016 §5): aggregating what an emitter measures like,
//! and comparing that against an editable catalogue.
//!
//! - [`features`] folds repeated sightings into an [`EmissionFeatures`] that carries its
//!   uncertainty.
//! - [`matcher`] compares one against the catalogue and produces a ranked [`SignatureMatch`].
//!
//! # The rule this module exists to keep
//!
//! **A signature is a suggestion source, never truth** — the same standing as a band plan
//! (CLAUDE.md). Blind detection and measurement come first and are never overridden; the catalogue
//! only ever adds a ranked, reasoned explanation on top. So a match sets no identity, no
//! `known_status`, no lifecycle state and no family, and an emission whose parameters sit beside a
//! known protocol's without fitting it stays an interesting `partial` with the conflicting fields
//! named, rather than being snapped to the nearest entry. Clustering of unknowns (T-202) and the
//! rtl_433 importer (T-214) build on these two pieces.

pub mod cluster;
pub mod features;
pub mod matcher;

pub use cluster::{
    CLUSTER_EPSILON, CLUSTER_MIN_SHARED_FIELDS, Closeness, REPAIR_MIN_POINTS, RepairReport,
    Separated, assign_emitter, compare, promote, repair,
};
pub use features::{FeatureObservation, aggregate, fold_observation};
pub use matcher::{MATCH_MIN_DISCRIMINATING, default_tolerance, match_signatures, missing_fields};

use hk_model::signature::{EmissionFeatures, MatchOutcome, SignatureMatch};
use hk_model::time::Timestamp;
use hk_model::{EmitterId, RepoError, Repository};

/// Matches the emitter's latest features snapshot against the stored catalogue and appends the
/// result when it differs from the current one.
///
/// Returns `None` when the emitter has no features snapshot yet: nothing measured means nothing to
/// explain, which is not the same as "no match" and must not be recorded as one.
///
/// The append is deliberately conditional on a *change* of outcome or top candidate: the match log
/// is append-only history of what the catalogue said about this emitter, and re-running the
/// matcher on an unchanged measurement should not fill it with identical rows.
pub fn match_emitter(
    repo: &mut Repository,
    emitter_id: EmitterId,
    t: Timestamp,
) -> Result<Option<SignatureMatch>, RepoError> {
    let Some(features) = repo.emitter_features(emitter_id)? else {
        return Ok(None);
    };
    let catalogue = repo.signatures()?;
    let rev = repo.signatures_rev()?;
    let m = match_signatures(&features, &catalogue, rev, t);
    let current = repo.current_signature_match(emitter_id)?;
    if !same_verdict(current.as_ref(), &m) {
        repo.append_signature_match(&m)?;
    }
    Ok(Some(m))
}

/// Whether two matches say the same thing (same outcome and same top candidate version).
fn same_verdict(current: Option<&SignatureMatch>, next: &SignatureMatch) -> bool {
    let Some(current) = current else {
        return false;
    };
    current.outcome == next.outcome
        && current.top().map(|c| &c.signature) == next.top().map(|c| &c.signature)
}

/// Whether a features snapshot may be used to *mint* a signature: never from an emitter whose
/// every observation was suspect (clipped, IMD, image or spur), because the 8-bit, preselector-less
/// front end produces ghosts with real-looking parameters (C18 card, ADR-0016 §5).
pub fn may_mint_from(features: &EmissionFeatures) -> bool {
    features.suspect_fraction < 1.0 && features.observations > 0
}

/// Whether a match is strong enough to offer a decoder recipe as the fast path (ADR-0016 §8).
pub fn offers_recipe(m: &SignatureMatch) -> bool {
    m.outcome != MatchOutcome::None && m.top().is_some_and(|c| c.recipe.is_some())
}

#[cfg(test)]
mod cluster_tests; // T-202
#[cfg(test)]
mod tests;
