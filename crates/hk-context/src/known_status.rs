//! Service-family matcher: turns a [`BandTable`] lookup plus an emitter's classified family into
//! a [`KnownStatus`] (C17 known-signal priors, T-019; docs/07 §2.11).
//!
//! C17 only *supplies* priors — it never classifies (that's C15) and it never vetoes a
//! measurement. So the rule here is deliberately narrow and fails safe:
//!
//! - No allocation row overlaps the emitter's band → [`KnownStatus::Unknown`] ("no allocation
//!   data"), **never** `Known` — "no record" must not be misread as "this is fine" (C17 card,
//!   "Allocation ≠ assignment ≠ use").
//! - The emitter has no (or an unclassified) family → `Unknown`.
//! - The family is expected on a matching row (see [`expected_tags`]) → `Known`.
//! - The family is a recognised family but no matching row carries its expected tag → `UnexpectedHere`.
//! - The family is entirely unrecognised (not in the mapping table) → `Unknown` ("no service
//!   mapping"), not `UnexpectedHere` — we have no basis to call it out of place.
//!
//! One exception is documented explicitly: **Part 15 unlicensed use**. A short-range/ISM-style
//! family (e.g. `fsk-ism` at 433.92 MHz) is `Known` via the `part15` tag on the row that hosts it,
//! not via a licensed allocation — Part 15 devices operate on a non-interference secondary basis
//! and have no allocation-table entry of their own (C17 card, "Pitfalls").

use hk_model::{EmitterId, KnownStatus, KnownStatusChange, StatusAuthor, Timestamp};

use crate::band_table::{AllocationRow, BandTable};

/// Families that are `known` under 47 CFR Part 15 (unlicensed, non-interference, secondary) use
/// whenever they land inside a row tagged `part15`, regardless of that row's licensed family
/// mapping. This is the documented exception used for e.g. a 433.92 MHz ISM-style sensor
/// (AWARE-053): the allocation there is amateur/radiolocation, but Part 15 devices may share it
/// under separate rules (47 CFR 15.231/15.235), so the *use* is expected even though the *family*
/// doesn't match the *allocation*.
pub const PART15_FAMILIES: &[&str] = &["fsk-ism", "ism", "lora", "ook-ism"];

/// Family → allocation tags it is expected to appear under. A family matches a row when the row
/// carries any of its listed tags. Extend this table (and the CSV's `tags` column) together when
/// a new decoder/classifier family is added.
///
/// These are *service* families. Evidence-level names (demodulator modes such as hk-demod's
/// `wfm`, modulation labels such as `2fsk`, decoder ids such as `readsb`) are mapped onto them
/// by the pipeline's family vocabulary (`hk_pipeline::family`, T-039) before this matcher runs.
fn expected_tags(family: &str) -> Option<&'static [&'static str]> {
    match family {
        "fm-broadcast" => Some(&["fm-broadcast"]),
        "adsb" | "mode-s" => Some(&["adsb"]),
        "ais" => Some(&["ais", "maritime"]),
        "noaa-apt" => Some(&["noaa-apt"]),
        "noaa-wx" | "noaa-weather" => Some(&["noaa-wx"]),
        "amateur" | "ham" => Some(&["amateur"]),
        "aviation-voice" | "aviation-am" | "aviation-vhf-comm" => Some(&["aviation"]),
        "gnss" => Some(&["gnss"]),
        "cellular" | "lte" => Some(&["cellular"]),
        "public-safety" | "p25" | "dmr" => Some(&["public-safety"]),
        // T-953: 929-932 MHz paging (Part 90 Subpart P private paging, Part 24 narrowband PCS,
        // Part 22 Subpart E common-carrier paging). `flex` and `pocsag` are the air interfaces
        // the service is carried on, and they name the service; a bare `2fsk` never does.
        "paging" | "flex" | "pocsag" => Some(&["paging"]),
        // T-953: frequency hopping is a measured *behaviour*, and 47 CFR 15.247 is where it is
        // expressly authorised. The tag says "hopping is expected here", never "this hops".
        "fhss" => Some(&["fhss"]),
        _ => None,
    }
}

/// `family` is a service family this matcher can place: it has expected allocation tags or is a
/// Part 15 family. Any other family (including evidence-level names like `wfm`) yields
/// `unknown` ("no service mapping").
pub fn is_service_family(family: &str) -> bool {
    expected_tags(family).is_some() || PART15_FAMILIES.contains(&family)
}

/// The result of matching an emitter's family and band against the [`BandTable`]: what its
/// [`KnownStatus`] should be, which row (if any) decided it, and why.
#[derive(Clone, Debug, PartialEq)]
pub struct PriorMatch {
    /// The status to record.
    pub status: KnownStatus,
    /// `Some(row.prior_ref())` when an allocation row decided the status.
    pub prior_ref: Option<String>,
    /// Human-readable reason, suitable for [`KnownStatusChange::reason`].
    pub reason: String,
}

impl PriorMatch {
    /// Builds the [`KnownStatusChange`] to append to `emitter_id`'s history via
    /// `Repository::append_known_status`, authored by [`StatusAuthor::Prior`].
    pub fn to_status_change(&self, emitter_id: EmitterId, t: Timestamp) -> KnownStatusChange {
        KnownStatusChange {
            emitter_id,
            status: self.status,
            prior_ref: self.prior_ref.clone(),
            reason: self.reason.clone(),
            t,
            author: StatusAuthor::Prior,
        }
    }
}

/// Matches an emitter's classified `family` (e.g. `fm-broadcast`, `adsb`, `unknown`) and occupied
/// band against `table`. See the module docs for the decision rule.
pub fn match_known_status(
    table: &BandTable,
    family: &str,
    f_center_hz: f64,
    bandwidth_hz: f64,
) -> PriorMatch {
    let rows = table.overlapping_center(f_center_hz, bandwidth_hz);
    if rows.is_empty() {
        // Fail safe: never call an emitter `known` just because nothing in the compact table
        // covers its frequency (docs/09 "Ghost alarms"; C17 card "Allocation ≠ assignment ≠ use").
        return PriorMatch {
            status: KnownStatus::Unknown,
            prior_ref: None,
            reason: "no allocation data".into(),
        };
    }
    if family.trim().is_empty() || family.eq_ignore_ascii_case("unknown") {
        return PriorMatch {
            status: KnownStatus::Unknown,
            prior_ref: None,
            reason: "emitter has no classified family".into(),
        };
    }

    if PART15_FAMILIES.contains(&family) {
        if let Some(row) = rows.iter().find(|r| r.has_tag("part15")) {
            return known_via(family, row, "Part 15 unlicensed use of");
        }
    }

    match expected_tags(family) {
        Some(tags) => match rows.iter().find(|r| tags.iter().any(|t| r.has_tag(t))) {
            Some(row) => known_via(family, row, "matches the"),
            None => {
                // A recognised family, but none of the overlapping rows expect it here.
                let row = rows[0];
                PriorMatch {
                    status: KnownStatus::UnexpectedHere,
                    prior_ref: Some(row.prior_ref()),
                    reason: format!(
                        "{family} is not an expected service of the {} allocation ({:.3}-{:.3} MHz)",
                        row.id,
                        row.freq.lo_hz / 1e6,
                        row.freq.hi_hz / 1e6,
                    ),
                }
            }
        },
        None => PriorMatch {
            status: KnownStatus::Unknown,
            prior_ref: None,
            reason: format!("no service mapping for family {family:?}"),
        },
    }
}

fn known_via(family: &str, row: &AllocationRow, verb: &str) -> PriorMatch {
    PriorMatch {
        status: KnownStatus::Known,
        prior_ref: Some(row.prior_ref()),
        reason: format!("{family} {verb} {} allocation", row.id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::band_table::Region;
    use hk_model::{
        EmitterId, EmitterObservation, KnownStatus, Repository, StatusAuthor, TimeRange, Timestamp,
    };

    fn table() -> BandTable {
        BandTable::bundled(Region::Us).unwrap()
    }

    fn t(s: i64) -> Timestamp {
        Timestamp::from_unix_nanos(s * 1_000_000_000)
    }

    /// AWARE-053: an `adsb`-classified emitter at 1090 MHz is `known`.
    #[test]
    fn aware_053_adsb_at_1090_is_known() {
        let m = match_known_status(&table(), "adsb", 1090e6, 2e6);
        assert_eq!(m.status, KnownStatus::Known);
        assert_eq!(
            m.prior_ref.as_deref(),
            Some("us-47cfr2106-compact:aero-radionav-960-1215")
        );
    }

    /// AWARE-053: an `fm-broadcast`-classified carrier at 120 MHz (inside the aviation VHF comm
    /// band) is `unexpected-here`.
    #[test]
    fn aware_053_fm_broadcast_at_120mhz_is_unexpected() {
        let m = match_known_status(&table(), "fm-broadcast", 120e6, 150e3);
        assert_eq!(m.status, KnownStatus::UnexpectedHere);
        assert_eq!(
            m.prior_ref.as_deref(),
            Some("us-47cfr2106-compact:aviation-vhf-comm")
        );
    }

    /// T-039: the service vocabulary accepts service families only; evidence-level names are
    /// the pipeline's to map.
    #[test]
    fn service_families_are_recognised_and_evidence_names_are_not() {
        for f in [
            "fm-broadcast",
            "adsb",
            "ism",
            "fsk-ism",
            "aviation-vhf-comm",
        ] {
            assert!(is_service_family(f), "{f}");
        }
        for f in ["wfm", "2fsk", "readsb", "unknown", ""] {
            assert!(!is_service_family(f), "{f}");
        }
        let m = match_known_status(&table(), "aviation-vhf-comm", 120.5e6, 25e3);
        assert_eq!(m.status, KnownStatus::Known);
    }

    /// AWARE-053: an unclassified ("unknown") emitter is `unknown`, even inside a well-covered
    /// band.
    #[test]
    fn aware_053_unclassified_emitter_is_unknown() {
        let m = match_known_status(&table(), "unknown", 98.1e6, 200e3);
        assert_eq!(m.status, KnownStatus::Unknown);
        assert_eq!(m.prior_ref, None);
    }

    /// AWARE-053: an `fsk-ism` emitter at 433.92 MHz is `known` via the Part 15 rule, not via the
    /// (amateur/radiolocation) allocation.
    #[test]
    fn aware_053_fsk_ism_at_433_92_known_via_part15() {
        let m = match_known_status(&table(), "fsk-ism", 433.92e6, 200e3);
        assert_eq!(m.status, KnownStatus::Known);
        assert_eq!(
            m.prior_ref.as_deref(),
            Some("us-47cfr2106-compact:ism-433-part15")
        );
        assert!(m.reason.contains("Part 15"), "{}", m.reason);
    }

    /// AWARE-053: a frequency with no allocation-table row at all is `unknown`, never `known`.
    /// T-953: the paging allocation exists and places a paging emission at 929.6 MHz. It is a
    /// *suggestion* — the family has to come from somewhere else first — and nothing that is not
    /// paging becomes paging by sitting there.
    #[test]
    fn t953_paging_allocation_places_a_pager_at_929_6_mhz() {
        let t = table();
        let m = match_known_status(&t, "paging", 929.6125e6, 25e3);
        assert_eq!(m.status, KnownStatus::Known, "{m:?}");
        assert_eq!(
            m.prior_ref.as_deref(),
            Some("us-47cfr2106-compact:paging-929")
        );
        // The Part 22 Subpart E common-carrier half, and narrowband PCS between them.
        let m = match_known_status(&t, "paging", 931.4e6, 25e3);
        assert_eq!(
            m.prior_ref.as_deref(),
            Some("us-47cfr2106-compact:paging-931")
        );
        assert_eq!(m.status, KnownStatus::Known);
        assert_eq!(
            match_known_status(&t, "paging", 930.5e6, 25e3).status,
            KnownStatus::Known,
            "narrowband PCS carries paging traffic and is tagged for it"
        );
        // A service that does not belong there is still flagged, not placed.
        assert_eq!(
            match_known_status(&t, "fm-broadcast", 929.6e6, 200e3).status,
            KnownStatus::UnexpectedHere
        );
        // And the band is not a Part 15 band, so the Part 15 pass does not apply here: an `ism`
        // family falls through to "no service mapping", which is `unknown` — never `known`.
        assert_eq!(
            match_known_status(&t, "ism", 929.6e6, 25e3).status,
            KnownStatus::Unknown
        );
        assert!(is_service_family("paging"));
    }

    #[test]
    fn aware_053_no_data_is_unknown() {
        let m = match_known_status(&table(), "unknown-emitter-type", 300e6, 10e3);
        assert_eq!(m.status, KnownStatus::Unknown);
        assert_eq!(m.reason, "no allocation data");
        assert_eq!(m.prior_ref, None);
    }

    /// AWARE-053: the repository's known-status history holds the appended entry with its
    /// `prior_ref`, alongside the `System`-authored entry written when the emitter was created.
    #[test]
    fn aware_053_status_history_records_prior_ref() {
        let mut repo = Repository::open_in_memory().unwrap();
        let emitter_id = repo
            .upsert_emitter_observation(&EmitterObservation {
                emitter_id: EmitterId::new(),
                seen: TimeRange::new(t(0), t(1)),
                count: 1,
                f_center_hz: 1090e6,
                bandwidth_hz: 2e6,
                identity: None,
            })
            .unwrap()
            .emitter_id;

        let m = match_known_status(&table(), "adsb", 1090e6, 2e6);
        repo.append_known_status(&m.to_status_change(emitter_id, t(2)))
            .unwrap();

        let history = repo.known_status_history(emitter_id).unwrap();
        assert_eq!(history.len(), 2, "{history:?}");
        assert_eq!(history[0].author, StatusAuthor::System);
        assert_eq!(history[1].status, KnownStatus::Known);
        assert_eq!(history[1].author, StatusAuthor::Prior);
        assert_eq!(
            history[1].prior_ref.as_deref(),
            Some("us-47cfr2106-compact:aero-radionav-960-1215")
        );
        assert_eq!(
            repo.emitter(emitter_id).unwrap().known_status,
            KnownStatus::Known
        );
    }
}
