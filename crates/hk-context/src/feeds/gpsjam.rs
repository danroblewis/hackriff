//! gpsjam.org adapter (C29; AWARE-001, AWARE-006).
//!
//! # Source (checked 2026-09-13; see the ADR-0010 ledger)
//!
//! gpsjam.org publishes one CSV per UTC day at `https://gpsjam.org/data/<YYYY-MM-DD>-h3_4.csv`
//! (a day index is at `/data/manifest.csv`, columns `date,suspect,num_bad_aircraft_hexes,source`).
//! Each row is an H3 resolution-4 cell: `hex,count_good_aircraft,count_bad_aircraft`, counted
//! from ADS-B navigation-accuracy (NACp) reports of aircraft that flew through the cell that day
//! (data from ADS-B Exchange and airplanes.live). The map colours a cell by
//! `percent_bad = 100·(bad − 1)/(good + bad)` (the −1 suppresses low-traffic false positives):
//! green < 2 %, yellow 2–10 %, red > 10 % (gpsjam.org/faq). The site states **no licence or
//! terms**, so only a hand-made synthetic extract is committed.
//!
//! # Mapping to [`ExternalEvent`]
//!
//! - `source` `gpsjam`, `event_type` `gnss-interference-cell`, `native_id` `<date>/<hex>`; the
//!   local id is derived from the natural key (UUIDv8 from SHA-256), so a frozen cache produces
//!   the same ids on every run and device.
//! - `time`: the whole UTC day (a daily product; never join it as a timestamp).
//! - `geo`: [`Geo::BoundingBox`] of the cell's boundary vertices. The box contains the hexagon, so
//!   containment is slightly generous near the corners (a res-4 edge is ≈ 22 km); boxes across the
//!   antimeridian have `west > east`, and a cell containing a pole spans all longitudes.
//! - `freq`: [`GNSS_BANDS`] (upper and lower L-band RNSS).
//! - `payload`: date, cell, resolution, counts, `percent_bad`, `level`, centroid.
//! - Only cells at or above [`GpsjamConfig::min_level`] (default yellow) become events; green
//!   cells are counted in [`Parsed::skipped`] and still count towards the day's coverage.
//! - `valid_until`: `fetched_at` + [`GpsjamConfig::max_age_s`] (default 24 h, the feed's cadence).

use std::collections::BTreeSet;

use h3o::{CellIndex, LatLng};
use hk_model::{ExternalEvent, ExternalEventId, FreqRange, Geo, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::{FeedAdapter, ParseError, Parsed};
use crate::utc;

/// Feed id.
pub const SOURCE: &str = "gpsjam";
/// Event type.
pub const EVENT_TYPE: &str = "gnss-interference-cell";
/// Expected CSV header.
pub const HEADER: &str = "hex,count_good_aircraft,count_bad_aircraft";
/// Parser version.
pub const PARSER_VERSION: &str = "gpsjam-h3-csv@1";

/// GNSS bands a gpsjam cell is relevant to: the RNSS allocations of 47 CFR 2.106 / ITU RR.
/// - `L5`: 1164–1215 MHz (GPS L5 1176.45, Galileo E5a/E5b, BeiDou B2).
/// - `L2`: 1215–1254 MHz (GPS L2 1227.60, GLONASS G2 ≈ 1246).
/// - `L1`: 1559–1610 MHz (GPS L1 1575.42, Galileo E1, BeiDou B1, GLONASS G1 ≈ 1602).
pub const GNSS_BANDS: [(&str, FreqRange); 3] = [
    ("L5", FreqRange::new(1164e6, 1215e6)),
    ("L2", FreqRange::new(1215e6, 1254e6)),
    ("L1", FreqRange::new(1559e6, 1610e6)),
];

/// gpsjam's colour level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Level {
    /// Green: < 2 % of aircraft reported low accuracy.
    Low,
    /// Yellow: 2–10 %.
    Medium,
    /// Red: > 10 %.
    High,
}

impl Level {
    /// Level of a percentage.
    pub fn of(percent_bad: f64) -> Self {
        if percent_bad > 10.0 {
            Level::High
        } else if percent_bad >= 2.0 {
            Level::Medium
        } else {
            Level::Low
        }
    }

    /// Payload string.
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Low => "low",
            Level::Medium => "medium",
            Level::High => "high",
        }
    }
}

/// gpsjam's `percent_bad` (clamped at 0; 0 for an empty cell).
pub fn percent_bad(good: u64, bad: u64) -> f64 {
    let total = good + bad;
    if total == 0 || bad == 0 {
        return 0.0;
    }
    100.0 * (bad - 1) as f64 / total as f64
}

/// Adapter settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GpsjamConfig {
    /// Lowest level cached as an event.
    pub min_level: Level,
    /// Validity of a fetched snapshot, s.
    pub max_age_s: f64,
}

impl Default for GpsjamConfig {
    fn default() -> Self {
        Self {
            min_level: Level::Medium,
            max_age_s: 86_400.0,
        }
    }
}

/// The gpsjam daily-cell adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct GpsjamAdapter {
    /// Settings.
    pub config: GpsjamConfig,
}

/// Deterministic event id from the natural key: the first 16 bytes of
/// SHA-256(`source` NUL `native_id`) with UUIDv8 version/variant bits.
pub fn event_id(source: &str, native_id: &str) -> ExternalEventId {
    let digest = Sha256::new()
        .chain_update(source.as_bytes())
        .chain_update([0u8])
        .chain_update(native_id.as_bytes())
        .finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    ExternalEventId::from_uuid(uuid::Builder::from_custom_bytes(bytes).into_uuid())
}

/// Bounding box `(south, west, north, east)` of a cell.
pub fn cell_bbox(cell: CellIndex) -> (f64, f64, f64, f64) {
    let boundary = cell.boundary();
    let (mut south, mut north) = (90.0f64, -90.0f64);
    let mut lons: Vec<f64> = Vec::with_capacity(boundary.len());
    for v in boundary.iter() {
        south = south.min(v.lat());
        north = north.max(v.lat());
        lons.push(v.lng());
    }
    for (pole, lat) in [(90.0, 90.0), (-90.0, -90.0)] {
        if LatLng::new(pole, 0.0).is_ok_and(|p| p.to_cell(cell.resolution()) == cell) {
            return (south.min(lat), -180.0, north.max(lat), 180.0);
        }
    }
    let (min, max) = lons
        .iter()
        .fold((180.0f64, -180.0f64), |(lo, hi), &l| (lo.min(l), hi.max(l)));
    if max - min > 180.0 {
        // Crosses the antimeridian: west edge is the smallest positive longitude, east edge the
        // largest negative one.
        let west = lons
            .iter()
            .copied()
            .filter(|l| *l >= 0.0)
            .fold(180.0, f64::min);
        let east = lons
            .iter()
            .copied()
            .filter(|l| *l < 0.0)
            .fold(-180.0, f64::max);
        (south, west, north, east)
    } else {
        (south, min, north, max)
    }
}

impl FeedAdapter for GpsjamAdapter {
    fn source(&self) -> &str {
        SOURCE
    }

    fn parser_version(&self) -> &str {
        PARSER_VERSION
    }

    fn file_name(&self, key: &str) -> String {
        format!("{key}-h3_4.csv")
    }

    fn parse(&self, key: &str, body: &str, fetched_at: Timestamp) -> Result<Parsed, ParseError> {
        let whole = |message: String| ParseError { line: 0, message };
        let coverage = utc::day_range(key)
            .ok_or_else(|| whole(format!("key {key:?} is not a YYYY-MM-DD date")))?;
        let valid_until = if self.config.max_age_s.is_finite() && self.config.max_age_s >= 0.0 {
            Some(fetched_at.saturating_add_nanos((self.config.max_age_s * 1e9) as i64))
        } else {
            return Err(whole(format!(
                "max_age_s {} is invalid",
                self.config.max_age_s
            )));
        };
        let mut header_seen = false;
        let mut seen = BTreeSet::new();
        let mut rows = Vec::new();
        let mut skipped = 0;
        for (index, raw) in body.lines().enumerate() {
            let line_no = index + 1;
            let line = raw.trim_end_matches('\r').trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let err = |message: String| ParseError {
                line: line_no,
                message,
            };
            if !header_seen {
                if line != HEADER {
                    return Err(err(format!("header {line:?}, expected {HEADER:?}")));
                }
                header_seen = true;
                continue;
            }
            let fields: Vec<&str> = line.split(',').collect();
            let [hex, good, bad] = fields[..] else {
                return Err(err(format!("{} fields, expected 3", fields.len())));
            };
            let raw_index = u64::from_str_radix(hex, 16)
                .map_err(|_| err(format!("hex {hex:?} is not hexadecimal")))?;
            let cell = CellIndex::try_from(raw_index)
                .map_err(|e| err(format!("hex {hex:?} is not an H3 cell: {e}")))?;
            let good: u64 = good
                .parse()
                .map_err(|_| err(format!("count_good_aircraft {good:?} is not a count")))?;
            let bad: u64 = bad
                .parse()
                .map_err(|_| err(format!("count_bad_aircraft {bad:?} is not a count")))?;
            if !seen.insert(cell) {
                return Err(err(format!("hex {hex} appears twice")));
            }
            let pct = percent_bad(good, bad);
            let level = Level::of(pct);
            if level < self.config.min_level {
                skipped += 1;
                continue;
            }
            rows.push((cell, good, bad, pct, level));
        }
        if !header_seen {
            return Err(whole("no header (empty snapshot)".into()));
        }
        rows.sort_by_key(|r| u64::from(r.0));
        let events = rows
            .into_iter()
            .map(|(cell, good, bad, pct, level)| {
                let native_id = format!("{key}/{cell}");
                let (south, west, north, east) = cell_bbox(cell);
                let centroid = LatLng::from(cell);
                ExternalEvent {
                    id: event_id(SOURCE, &native_id),
                    source: SOURCE.into(),
                    native_id,
                    event_type: EVENT_TYPE.into(),
                    time: coverage,
                    geo: Geo::BoundingBox {
                        south_deg: south,
                        west_deg: west,
                        north_deg: north,
                        east_deg: east,
                    },
                    freq: GNSS_BANDS.iter().map(|(_, r)| *r).collect(),
                    payload: json!({
                        "date": key,
                        "h3_cell": cell.to_string(),
                        "h3_resolution": u8::from(cell.resolution()),
                        "count_good_aircraft": good,
                        "count_bad_aircraft": bad,
                        "percent_bad": pct,
                        "level": level.as_str(),
                        "centroid_lat_deg": centroid.lat(),
                        "centroid_lon_deg": centroid.lng(),
                    }),
                    fetched_at,
                    valid_until,
                }
            })
            .collect();
        Ok(Parsed {
            events,
            coverage,
            skipped,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fetched() -> Timestamp {
        utc::parse_utc("2026-09-14T01:00:00Z").unwrap()
    }

    const BODY: &str = "hex,count_good_aircraft,count_bad_aircraft\n\
                        84194edffffffff,40,12\n\
                        8419453ffffffff,30,2\n\
                        840d9edffffffff,20,0\n";

    #[test]
    fn aware_006_parses_cells_levels_and_extent() {
        let parsed = GpsjamAdapter::default()
            .parse("2026-09-13", BODY, fetched())
            .unwrap();
        assert_eq!(parsed.skipped, 1, "green cell is not cached");
        assert_eq!(parsed.events.len(), 2);
        let device = parsed
            .events
            .iter()
            .find(|e| e.native_id == "2026-09-13/84194edffffffff")
            .unwrap();
        assert_eq!(device.payload["level"], "high");
        assert!((device.payload["percent_bad"].as_f64().unwrap() - 1100.0 / 52.0).abs() < 1e-9);
        assert_eq!(device.time, utc::day_range("2026-09-13").unwrap());
        assert_eq!(device.freq.len(), 3);
        assert!(
            device
                .freq
                .iter()
                .any(|f| f.lo_hz <= 1575.42e6 && f.hi_hz >= 1575.42e6)
        );
        let Geo::BoundingBox {
            south_deg,
            west_deg,
            north_deg,
            east_deg,
        } = device.geo
        else {
            panic!("bbox");
        };
        assert!(south_deg < 52.2 && north_deg > 52.2 && west_deg < 0.12 && east_deg > 0.12);
        assert_eq!(
            device.id,
            event_id(SOURCE, &device.native_id),
            "id from natural key"
        );
        assert_eq!(
            device.valid_until.unwrap().as_unix_nanos() - fetched().as_unix_nanos(),
            86_400_000_000_000
        );
        let again = GpsjamAdapter::default()
            .parse("2026-09-13", BODY, fetched())
            .unwrap();
        assert_eq!(parsed, again, "deterministic");
    }

    #[test]
    fn schema_changes_and_bad_rows_fail_loudly() {
        let a = GpsjamAdapter::default();
        let e = a
            .parse(
                "2026-09-13",
                "hex,good,bad\n84194edffffffff,1,0\n",
                fetched(),
            )
            .unwrap_err();
        assert_eq!(e.line, 1);
        for (body, line) in [
            (
                "hex,count_good_aircraft,count_bad_aircraft\n84194edffffffff,1\n",
                2,
            ),
            ("hex,count_good_aircraft,count_bad_aircraft\nzz,1,0\n", 2),
            (
                "hex,count_good_aircraft,count_bad_aircraft\n8400000000000000,1,0\n",
                2,
            ),
            (
                "hex,count_good_aircraft,count_bad_aircraft\n84194edffffffff,x,0\n",
                2,
            ),
            (
                "hex,count_good_aircraft,count_bad_aircraft\n84194edffffffff,1,0\n84194edffffffff,1,0\n",
                3,
            ),
            ("", 0),
        ] {
            assert_eq!(
                a.parse("2026-09-13", body, fetched()).unwrap_err().line,
                line,
                "{body:?}"
            );
        }
        assert_eq!(a.parse("13-09-2026", BODY, fetched()).unwrap_err().line, 0);
    }

    #[test]
    fn percent_and_levels_follow_the_gpsjam_faq() {
        assert_eq!(percent_bad(0, 0), 0.0);
        assert_eq!(percent_bad(10, 1), 0.0, "one bad aircraft is discounted");
        assert_eq!(Level::of(1.99), Level::Low);
        assert_eq!(Level::of(2.0), Level::Medium);
        assert_eq!(Level::of(10.0), Level::Medium);
        assert_eq!(Level::of(10.01), Level::High);
    }

    #[test]
    fn antimeridian_cell_bbox_wraps() {
        let cell = LatLng::new(65.0, 179.95)
            .unwrap()
            .to_cell(h3o::Resolution::Four);
        let (s, w, n, e) = cell_bbox(cell);
        assert!(w > e, "west {w} > east {e} across the antimeridian");
        assert!(s < 65.0 && n > 65.0);
        let site = crate::geo::Site::new(65.0, 179.95);
        assert_eq!(crate::geo::distance_to_bbox_km(&site, s, w, n, e), 0.0);
    }
}
