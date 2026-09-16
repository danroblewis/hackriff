//! Frequency, time and region extents: the two indexed axes of the data model (docs/07 §4).
//!
//! All intervals are **closed**: `[lo, hi]`. Two extents overlap when each starts at or before
//! the other ends, so a detection that ends exactly where a query starts is included. The SQLite
//! repository uses the same rule, so the in-memory helpers here predict query results.

use serde::{Deserialize, Serialize};

use crate::time::Timestamp;

/// A closed frequency interval `[lo_hz, hi_hz]`, Hz.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FreqRange {
    /// Lower edge, Hz.
    pub lo_hz: f64,
    /// Upper edge, Hz. `hi_hz >= lo_hz`.
    pub hi_hz: f64,
}

impl FreqRange {
    /// Builds a range from its edges.
    pub const fn new(lo_hz: f64, hi_hz: f64) -> Self {
        Self { lo_hz, hi_hz }
    }

    /// The range centred on `center_hz` with total width `width_hz`.
    pub fn centered(center_hz: f64, width_hz: f64) -> Self {
        Self::new(center_hz - width_hz / 2.0, center_hz + width_hz / 2.0)
    }

    /// Centre frequency, Hz.
    pub fn center_hz(&self) -> f64 {
        (self.lo_hz + self.hi_hz) / 2.0
    }

    /// Width, Hz.
    pub fn width_hz(&self) -> f64 {
        self.hi_hz - self.lo_hz
    }

    /// Both ranges share at least one frequency (closed intervals).
    pub fn overlaps(&self, other: &FreqRange) -> bool {
        self.lo_hz <= other.hi_hz && self.hi_hz >= other.lo_hz
    }
}

/// A closed time interval `[start, end]`.
///
/// Serializes as `{"start_ns", "end_ns"}` — integer Unix nanoseconds, the unit named in the field
/// like [`FreqRange`] names Hz (T-349; the unit law is on [`crate::Timestamp`]). The old unitless
/// `start`/`end` are accepted on read so the CRC-checked observation and occupancy line logs
/// written before the rename still parse.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TimeRange {
    /// First instant.
    #[serde(rename = "start_ns", alias = "start")]
    pub start: Timestamp,
    /// Last instant. `end >= start`; equal for a point in time.
    #[serde(rename = "end_ns", alias = "end")]
    pub end: Timestamp,
}

impl TimeRange {
    /// Builds a range from its ends.
    pub const fn new(start: Timestamp, end: Timestamp) -> Self {
        Self { start, end }
    }

    /// A zero-length range at `t`.
    pub const fn instant(t: Timestamp) -> Self {
        Self { start: t, end: t }
    }

    /// Duration, ns.
    pub const fn duration_ns(&self) -> i64 {
        self.end.as_unix_nanos() - self.start.as_unix_nanos()
    }

    /// Both ranges share at least one instant (closed intervals).
    pub fn overlaps(&self, other: &TimeRange) -> bool {
        self.start <= other.end && self.end >= other.start
    }
}

/// A frequency × time box. The shape of "this region over this time" queries and of the
/// `region` on Anomaly (docs/07 §2.18).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Region {
    /// Frequency extent.
    pub freq: FreqRange,
    /// Time extent.
    pub time: TimeRange,
}

impl Region {
    /// Builds a region from its extents.
    pub const fn new(freq: FreqRange, time: TimeRange) -> Self {
        Self { freq, time }
    }

    /// Overlaps `other` in both frequency and time.
    pub fn overlaps(&self, other: &Region) -> bool {
        self.freq.overlaps(&other.freq) && self.time.overlaps(&other.time)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(ns: i64) -> Timestamp {
        Timestamp::from_unix_nanos(ns)
    }

    #[test]
    fn closed_interval_overlap() {
        let a = FreqRange::new(100.0, 200.0);
        assert!(
            a.overlaps(&FreqRange::new(200.0, 300.0)),
            "touching edges overlap"
        );
        assert!(!a.overlaps(&FreqRange::new(200.5, 300.0)));
        assert!(
            a.overlaps(&FreqRange::new(120.0, 130.0)),
            "containment overlaps"
        );
        assert_eq!(
            FreqRange::centered(1090e6, 2e6),
            FreqRange::new(1089e6, 1091e6)
        );

        let r = TimeRange::new(t(10), t(20));
        assert!(r.overlaps(&TimeRange::instant(t(20))));
        assert!(!r.overlaps(&TimeRange::instant(t(21))));
        assert_eq!(r.duration_ns(), 10);
    }
}
