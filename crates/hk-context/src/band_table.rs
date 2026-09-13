//! Compact allocation table + interval lookup (C17 known-signal priors, T-019).
//!
//! Answers "what is *supposed* to be here?" for a frequency/bandwidth, using a hand-compiled
//! extract of the FCC Table of Frequency Allocations (47 CFR 2.106; a US Government work, public
//! domain) plus the closely related unlicensed/GNSS/cellular band rules — see the header comment
//! of the bundled CSV (`data/us-47cfr2106-compact.csv`) for exact sources and which rows are
//! `unverified`. It is a *prior*, not a legal reference: allocation != assignment != actual use
//! (docs/04 §1.1), so "no matching row" must never be read as "this use is illegal", only as "we
//! have no data" (see [`crate::known_status`]).
//!
//! Bundled offline per ADR-0008: the table ships with the binary (`include_str!`) and needs no
//! network access.

use std::fmt;
use std::str::FromStr;

use hk_model::FreqRange;

/// The bundled compact allocation table (see the file header for sources/edition).
const US_47CFR2106_COMPACT: &str = include_str!("../data/us-47cfr2106-compact.csv");

const EXPECTED_HEADER: &str = "id,f_lo_hz,f_hi_hz,region,federal,primary_services,\
secondary_services,tags,unverified";

/// The allocation-table region a query applies to. Only [`Region::Us`] has a bundled table today;
/// ITU Region 1/3 tables are future work (C17 card, "International"). Callers with no location
/// fix (no GNSS) fall back to [`Region::default`] (docs/adr/0008 "No GNSS fix").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Region {
    /// United States (47 CFR 2.106).
    #[default]
    Us,
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Region::Us => "us",
        })
    }
}

impl FromStr for Region {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "us" => Ok(Region::Us),
            other => Err(format!(
                "unknown region {other:?} (only \"us\" is loaded today)"
            )),
        }
    }
}

/// Simplified federal/non-federal column of 47 CFR 2.106.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FederalStatus {
    /// Allocated to federal (US Government) use.
    Federal,
    /// Allocated to non-federal use.
    NonFederal,
    /// Both columns carry an allocation here.
    Shared,
}

impl FromStr for FederalStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "federal" => Ok(FederalStatus::Federal),
            "non-federal" => Ok(FederalStatus::NonFederal),
            "shared" => Ok(FederalStatus::Shared),
            other => Err(format!(
                "unknown federal status {other:?} (expected federal, non-federal or shared)"
            )),
        }
    }
}

/// One row of the compact allocation table.
#[derive(Clone, Debug, PartialEq)]
pub struct AllocationRow {
    /// Stable slug, e.g. `fm-broadcast`. Used to build a [`Self::prior_ref`].
    pub id: String,
    /// Band edges, Hz (closed interval).
    pub freq: FreqRange,
    /// Which table this row belongs to.
    pub region: Region,
    /// Federal / non-federal / shared.
    pub federal: FederalStatus,
    /// Services allocated primary, kebab-case slugs (docs/04 §1.1 allocation layer).
    pub primary_services: Vec<String>,
    /// Services allocated secondary, kebab-case slugs.
    pub secondary_services: Vec<String>,
    /// Free-form use tags consumed by [`crate::known_status`]'s family matcher.
    pub tags: Vec<String>,
    /// `true` if this row's edges/services were not individually re-checked against a primary
    /// source (see the CSV header comment).
    pub unverified: bool,
}

impl AllocationRow {
    /// This row carries `tag` (exact match).
    pub fn has_tag(&self, tag: &str) -> bool {
        self.tags.iter().any(|t| t == tag)
    }

    /// The value to store as an emitter's `known_status` `prior_ref` when this row decided the
    /// status, e.g. `us-47cfr2106-compact:fm-broadcast`.
    pub fn prior_ref(&self) -> String {
        format!("us-47cfr2106-compact:{}", self.id)
    }
}

/// An error loading the allocation table: which line, and what was wrong with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadError {
    /// 1-based line number in the source text, or `0` for a whole-file problem (e.g. no header).
    pub line: usize,
    /// What was wrong.
    pub message: String,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            write!(f, "allocation table: {}", self.message)
        } else {
            write!(f, "allocation table line {}: {}", self.line, self.message)
        }
    }
}

impl std::error::Error for LoadError {}

fn semicolon_list(field: &str) -> Vec<String> {
    field
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn parse_row(line_no: usize, line: &str) -> Result<AllocationRow, LoadError> {
    let err = |message: String| LoadError {
        line: line_no,
        message,
    };
    let fields: Vec<&str> = line.split(',').collect();
    if fields.len() != 9 {
        return Err(err(format!(
            "expected 9 columns ({EXPECTED_HEADER}), found {}",
            fields.len()
        )));
    }
    let [
        id,
        f_lo,
        f_hi,
        region,
        federal,
        primary,
        secondary,
        tags,
        unverified,
    ] = [
        fields[0].trim(),
        fields[1].trim(),
        fields[2].trim(),
        fields[3].trim(),
        fields[4].trim(),
        fields[5].trim(),
        fields[6].trim(),
        fields[7].trim(),
        fields[8].trim(),
    ];
    if id.is_empty() {
        return Err(err("id column is empty".into()));
    }
    let f_lo_hz: f64 = f_lo
        .parse()
        .map_err(|_| err(format!("f_lo_hz {f_lo:?} is not a number")))?;
    let f_hi_hz: f64 = f_hi
        .parse()
        .map_err(|_| err(format!("f_hi_hz {f_hi:?} is not a number")))?;
    if !(f_lo_hz.is_finite() && f_hi_hz.is_finite()) {
        return Err(err("f_lo_hz/f_hi_hz must be finite".into()));
    }
    if f_lo_hz > f_hi_hz {
        return Err(err(format!(
            "f_lo_hz ({f_lo_hz}) is greater than f_hi_hz ({f_hi_hz})"
        )));
    }
    let region = Region::from_str(region).map_err(err)?;
    let federal = FederalStatus::from_str(federal).map_err(err)?;
    let unverified = match unverified {
        "true" => true,
        "false" => false,
        other => {
            return Err(err(format!(
                "unverified column {other:?} must be \"true\" or \"false\""
            )));
        }
    };
    Ok(AllocationRow {
        id: id.to_owned(),
        freq: FreqRange::new(f_lo_hz, f_hi_hz),
        region,
        federal,
        primary_services: semicolon_list(primary),
        secondary_services: semicolon_list(secondary),
        tags: semicolon_list(tags),
        unverified,
    })
}

/// The loaded compact allocation table.
#[derive(Clone, Debug, Default)]
pub struct BandTable {
    rows: Vec<AllocationRow>,
}

impl BandTable {
    /// Parses a table from CSV text (see the module docs for the schema). Rejects malformed rows
    /// with a [`LoadError`] naming the line and the problem: wrong column count, a non-numeric or
    /// inverted frequency range, an unknown `region`/`federal`/`unverified` value, or a duplicate
    /// `id`.
    pub fn parse(csv: &str) -> Result<Self, LoadError> {
        let mut lines = csv.lines().enumerate().map(|(i, l)| (i + 1, l));
        let header = loop {
            match lines.next() {
                Some((_, l)) if l.trim().is_empty() || l.trim_start().starts_with('#') => continue,
                Some((n, l)) => break Some((n, l)),
                None => break None,
            }
        };
        let Some((header_line, header)) = header else {
            return Err(LoadError {
                line: 0,
                message: "empty table: no header row".into(),
            });
        };
        if header.trim() != EXPECTED_HEADER {
            return Err(LoadError {
                line: header_line,
                message: format!(
                    "unexpected header {:?}, expected {EXPECTED_HEADER:?}",
                    header.trim()
                ),
            });
        }
        let mut rows = Vec::new();
        for (line_no, line) in lines {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let row = parse_row(line_no, line)?;
            if rows.iter().any(|r: &AllocationRow| r.id == row.id) {
                return Err(LoadError {
                    line: line_no,
                    message: format!("duplicate id {:?}", row.id),
                });
            }
            rows.push(row);
        }
        Ok(BandTable { rows })
    }

    /// The bundled table for `region`. Only [`Region::Us`] has a bundled table today; the match is
    /// exhaustive so adding a region's CSV and a match arm here is the only change needed to add
    /// one.
    pub fn bundled(region: Region) -> Result<Self, LoadError> {
        match region {
            Region::Us => Self::parse(US_47CFR2106_COMPACT),
        }
    }

    /// All rows, in file order.
    pub fn rows(&self) -> &[AllocationRow] {
        &self.rows
    }

    /// Rows whose band overlaps the closed interval `[f_lo_hz, f_hi_hz]`.
    pub fn overlapping(&self, f_lo_hz: f64, f_hi_hz: f64) -> Vec<&AllocationRow> {
        let query = FreqRange::new(f_lo_hz, f_hi_hz);
        self.rows
            .iter()
            .filter(|r| r.freq.overlaps(&query))
            .collect()
    }

    /// Rows whose band overlaps a single frequency.
    pub fn overlapping_point(&self, f_hz: f64) -> Vec<&AllocationRow> {
        self.overlapping(f_hz, f_hz)
    }

    /// Rows whose band overlaps a centre frequency ± half of `bandwidth_hz` (an emitter's
    /// occupied band). A non-finite or negative `bandwidth_hz` is treated as `0`.
    pub fn overlapping_center(&self, f_center_hz: f64, bandwidth_hz: f64) -> Vec<&AllocationRow> {
        let half = if bandwidth_hz.is_finite() && bandwidth_hz > 0.0 {
            bandwidth_hz / 2.0
        } else {
            0.0
        };
        self.overlapping(f_center_hz - half, f_center_hz + half)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn us() -> BandTable {
        BandTable::bundled(Region::Us).expect("bundled table parses")
    }

    /// AWARE-053: 98.1 MHz (a typical FM broadcast carrier) resolves to the FM broadcast
    /// allocation.
    #[test]
    fn aware_053_fm_broadcast_lookup() {
        let table = us();
        let rows = table.overlapping_point(98.1e6);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].id, "fm-broadcast");
        assert!(
            rows[0]
                .primary_services
                .contains(&"broadcasting".to_string())
        );
        assert!(rows[0].has_tag("fm-broadcast"));
    }

    /// AWARE-053: 1090 MHz (Mode S/ADS-B downlink) resolves to the aeronautical radionavigation
    /// band and carries the `adsb` tag.
    #[test]
    fn aware_053_adsb_1090_lookup() {
        let table = us();
        let rows = table.overlapping_point(1090e6);
        assert!(
            rows.iter().any(|r| r.id == "aero-radionav-960-1215"),
            "{rows:?}"
        );
        let row = rows
            .iter()
            .find(|r| r.id == "aero-radionav-960-1215")
            .unwrap();
        assert!(
            row.primary_services
                .contains(&"aeronautical-radionavigation".to_string())
        );
        assert!(row.has_tag("adsb"));
    }

    /// AWARE-053: a frequency with no bundled row (a gap between the covered bands) returns no
    /// rows, never a fabricated match.
    #[test]
    fn aware_053_no_data_lookup() {
        let table = us();
        assert!(
            table.overlapping_point(300e6).is_empty(),
            "300 MHz should be outside every bundled row"
        );
    }

    #[test]
    fn every_row_id_is_unique_and_ranges_are_ordered() {
        let table = us();
        let mut ids = std::collections::BTreeSet::new();
        for row in table.rows() {
            assert!(ids.insert(row.id.clone()), "duplicate id {}", row.id);
            assert!(row.freq.lo_hz <= row.freq.hi_hz, "{row:?}");
        }
        assert!(
            table.rows().len() >= 30,
            "expected a few dozen compact rows"
        );
    }

    #[test]
    fn loader_rejects_wrong_column_count() {
        let csv = format!("{EXPECTED_HEADER}\nbad-row,1,2,us,non-federal\n");
        let err = BandTable::parse(&csv).unwrap_err();
        assert_eq!(err.line, 2);
        assert!(err.message.contains("9 columns"), "{err}");
    }

    #[test]
    fn loader_rejects_non_numeric_frequency() {
        let csv = format!("{EXPECTED_HEADER}\nbad-row,not-a-number,2,us,non-federal,,,,false\n");
        let err = BandTable::parse(&csv).unwrap_err();
        assert!(err.message.contains("f_lo_hz"), "{err}");
    }

    #[test]
    fn loader_rejects_inverted_range() {
        let csv = format!("{EXPECTED_HEADER}\nbad-row,200,100,us,non-federal,,,,false\n");
        let err = BandTable::parse(&csv).unwrap_err();
        assert!(err.message.contains("greater than"), "{err}");
    }

    #[test]
    fn loader_rejects_unknown_federal_status() {
        let csv = format!("{EXPECTED_HEADER}\nbad-row,100,200,us,quasi-federal,,,,false\n");
        let err = BandTable::parse(&csv).unwrap_err();
        assert!(err.message.contains("federal status"), "{err}");
    }

    #[test]
    fn loader_rejects_duplicate_id() {
        let csv = format!(
            "{EXPECTED_HEADER}\ndup,100,200,us,non-federal,,,,false\ndup,300,400,us,non-federal,,,,false\n"
        );
        let err = BandTable::parse(&csv).unwrap_err();
        assert_eq!(err.line, 3);
        assert!(err.message.contains("duplicate id"), "{err}");
    }

    #[test]
    fn bundled_table_has_no_load_errors() {
        // Re-parsing the exact bundled text must succeed; this also pins the header contract.
        BandTable::parse(US_47CFR2106_COMPACT).expect("bundled table is well-formed");
    }
}
