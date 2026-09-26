//! **A suggestion explains a result; it never becomes one** (ADR-0021 §9, T-569).
//!
//! The known-signal database has exactly two attach points (§9.1). One is *into* the search, as
//! `prior_bits` in `hk_synth::seed`, where it orders hypotheses and neither ranks nor confirms
//! one. The other is this module: *onto* the result, as `resolution.explanations[]`, computed
//! **after** the [`Resolution`] is sealed, from **measured** parameters only.
//!
//! The failure this exists to prevent is narrower than "the database is never truth": it is **a
//! suggestion turning an `unknown` into a label**. So:
//!
//! - [`explain_resolution`] takes `&Resolution` — a shared reference. It cannot write `kind`,
//!   `reason` or `summary`, and it returns only a `Vec<Explanation>` for the caller to attach
//!   through [`Resolution::attach_explanations`]. An `unknown` with three high-scoring
//!   explanations is still `unknown`, and a `tied` result is still `tied`: naming one of the tied
//!   candidates from a band plan is exactly the coin flip blind-first exists to refuse.
//! - It never sees the emitter's row, so it cannot move a measured centre. **The mismatch stays a
//!   flag**: an emission 150 kHz off the FM raster gets [`ExplanationStatus::Unexpected`] and
//!   keeps its measured centre. That is the interesting case (CLAUDE.md), not an error to
//!   correct.
//! - It opens **no new database read** (§9.3): its only reference is a [`BandTable`] the caller
//!   already holds, and it performs no I/O — no repository, no feed, no network, no file.
//! - The direction of the crate graph is the real enforcement: `hk-synth` does not depend on
//!   `hk-context`, so the search cannot name an [`Explanation`] source even by accident. That is
//!   asserted by `crates/hk-synth/tests/explanation_boundary.rs`, in the spirit of ADR-0018's
//!   GNSS guard — the confinement is a test, not a promise.

use hk_model::repo::synthesis::{
    Explanation, ExplanationSource, ExplanationStatus, Resolution, ResolutionKind,
};

use crate::band_table::{AllocationRow, BandTable};
use crate::known_status::match_known_status;
use crate::occupancy::channels::raster_hint;

/// Most suggestions offered for one resolution. A ranked shortlist is an explanation; a hundred
/// rows is a database dump.
pub const MAX_EXPLANATIONS: usize = 4;

/// Largest distance from the nearest raster channel still read as *on* that channel, as a
/// fraction of the raster step. The analogue chains' `ON_RASTER_FRACTION` is the same 10 %.
const ON_RASTER_FRACTION: f64 = 0.1;

/// What was **measured** about the emission — never what a table expected of it.
#[derive(Clone, Debug, PartialEq)]
pub struct MeasuredEmission {
    /// Measured centre, Hz.
    pub center_hz: f64,
    /// Measured occupied bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// The service family measured evidence placed it in (`hk_pipeline::family`'s vocabulary), or
    /// `None` when nothing classified it. `None` is not `unknown`-the-label: it means the
    /// suggestion is made on frequency alone.
    pub family: Option<String>,
}

/// The reference data the caller already has loaded. Nothing here opens a new one.
#[derive(Clone, Copy, Debug)]
pub struct References<'a> {
    /// The allocation table.
    pub table: &'a BandTable,
    /// Age of that table, days, when the caller knows it. Served so the user can weigh a
    /// suggestion made from stale reference data.
    pub data_age_days: Option<u32>,
}

/// A channel raster a band plan defines, as spacing and origin.
struct Raster {
    spacing_hz: f64,
    origin_hz: f64,
    /// The plan named in the reasoning.
    plan: &'static str,
}

/// The channel raster for an allocation row, when its band plan defines one.
///
/// Deliberately short: a raster is asserted only where it is a published, checkable convention.
/// A row with no entry here yields a band-level suggestion with no `distance_hz`, which is honest
/// — "this band hosts that service" — rather than an invented channel.
fn raster_for(row: &AllocationRow) -> Option<Raster> {
    if row.has_tag("fm-broadcast") {
        // US FM: 200 kHz channels centred on 88.1 MHz (channel 201).
        return Some(Raster {
            spacing_hz: 200e3,
            origin_hz: 88.1e6,
            plan: "the US FM broadcast 200 kHz channel raster",
        });
    }
    if row.has_tag("aviation") {
        // VHF civil air band: 25 kHz channels from 118.000 MHz.
        return Some(Raster {
            spacing_hz: 25e3,
            origin_hz: 118.0e6,
            plan: "the VHF airband 25 kHz channel raster",
        });
    }
    if row.has_tag("noaa-wx") {
        // NOAA weather radio: seven channels 25 kHz apart from 162.400 MHz.
        return Some(Raster {
            spacing_hz: 25e3,
            origin_hz: 162.4e6,
            plan: "the NOAA weather radio 25 kHz channel set",
        });
    }
    None
}

/// Ranked suggestions about what a **sealed** resolution might be, from measured parameters and
/// already-loaded reference data.
///
/// Returns an empty list for [`ResolutionKind::NotSearched`]: there is no result to explain, and
/// a suggestion beside an un-looked-at emitter would read as a finding (ADR-0021 §7A.4).
///
/// Never mutates, and — by its signature — cannot: the resolution is shared, and the result is a
/// value the caller attaches.
#[must_use]
pub fn explain_resolution(
    resolution: &Resolution,
    measured: &MeasuredEmission,
    refs: &References<'_>,
) -> Vec<Explanation> {
    if resolution.kind == ResolutionKind::NotSearched {
        return Vec::new();
    }
    if !measured.center_hz.is_finite() || !measured.bandwidth_hz.is_finite() {
        return Vec::new();
    }
    let rows = refs
        .table
        .overlapping_center(measured.center_hz, measured.bandwidth_hz);
    if rows.is_empty() {
        // Fail safe, as `known_status` does: "nothing covers this" is its own answer and must
        // never be served as "nothing is expected here, so this is fine".
        return vec![Explanation {
            source: ExplanationSource::Alloc,
            identity: "no allocation data".to_owned(),
            score: 0.0,
            distance_hz: None,
            status: ExplanationStatus::NoReferenceData,
            data_age_days: refs.data_age_days,
            reasoning: format!(
                "no row of the loaded allocation table covers {:.4} MHz: there is no reference \
                 data here, which is not the same as nothing being expected here",
                measured.center_hz / 1e6
            ),
        }];
    }

    let family = measured.family.as_deref().unwrap_or("");
    let mut out: Vec<Explanation> = rows
        .iter()
        .map(|row| explain_row(row, family, measured, refs))
        .collect();
    // Rank: best fit first, then the nearer channel, then a stable name.
    out.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| {
                a.distance_hz
                    .map_or(f64::INFINITY, f64::abs)
                    .total_cmp(&b.distance_hz.map_or(f64::INFINITY, f64::abs))
            })
            .then_with(|| a.identity.cmp(&b.identity))
    });
    out.truncate(MAX_EXPLANATIONS);
    out
}

/// One allocation row's suggestion about this measurement.
fn explain_row(
    row: &AllocationRow,
    family: &str,
    measured: &MeasuredEmission,
    refs: &References<'_>,
) -> Explanation {
    let raster = raster_for(row);
    let distance_hz = raster.as_ref().map(|r| {
        raster_hint(
            measured.center_hz,
            r.spacing_hz,
            r.origin_hz,
            "hk-context/synth-explain",
        )
        .offset_hz
    });
    let off_raster = match (&raster, distance_hz) {
        (Some(r), Some(d)) => d.abs() > ON_RASTER_FRACTION * r.spacing_hz,
        _ => false,
    };
    // The family question, answered by the C17 matcher rather than a second copy of its table.
    let family_match = (!family.is_empty()).then(|| {
        let m = match_known_status(
            refs.table,
            family,
            measured.center_hz,
            measured.bandwidth_hz,
        );
        (
            m.status,
            m.prior_ref.as_deref() == Some(row.prior_ref().as_str()),
        )
    });
    let expects_family = matches!(family_match, Some((hk_model::KnownStatus::Known, true)));
    let misplaced_family = matches!(
        family_match,
        Some((hk_model::KnownStatus::UnexpectedHere, _))
    );

    let mut score = 0.5_f64;
    if expects_family {
        score += 0.3;
    }
    if misplaced_family {
        score -= 0.2;
    }
    if raster.is_some() {
        score += if off_raster { -0.25 } else { 0.2 };
    }
    let score = score.clamp(0.0, 1.0);

    // A mismatch is a FLAG, not a correction: the measured centre is untouched and stays the
    // subject of the sentence.
    let status = if off_raster || misplaced_family {
        ExplanationStatus::Unexpected
    } else {
        ExplanationStatus::Expected
    };

    let mut reasoning = format!(
        "{} allocation {:.4}-{:.4} MHz",
        row.id,
        row.freq.lo_hz / 1e6,
        row.freq.hi_hz / 1e6
    );
    if let (Some(r), Some(d)) = (&raster, distance_hz) {
        if off_raster {
            reasoning.push_str(&format!(
                "; measured centre is {:.1} kHz {} the nearest channel of {} (kept as measured)",
                d.abs() / 1e3,
                if d < 0.0 { "below" } else { "above" },
                r.plan
            ));
        } else {
            reasoning.push_str(&format!(
                "; measured centre is on {} to within {:.1} kHz",
                r.plan,
                d.abs() / 1e3
            ));
        }
    }
    if expects_family {
        reasoning.push_str(&format!("; measured family {family} is expected here"));
    } else if misplaced_family {
        reasoning.push_str(&format!(
            "; measured family {family} is not an expected service of this allocation"
        ));
    } else if family.is_empty() {
        reasoning.push_str("; offered on frequency alone, nothing classified the emission");
    }

    Explanation {
        source: if raster.is_some() {
            ExplanationSource::BandPlan
        } else {
            ExplanationSource::Alloc
        },
        identity: row.id.clone(),
        score,
        distance_hz,
        status,
        data_age_days: refs.data_age_days,
        reasoning,
    }
}
