//! Time model: UTC timestamps and sample-count timing.
//!
//! HackRF One has no hardware 1PPS. A stream's time comes from the host arrival time of a
//! buffer plus a running sample count (docs/07 §2.6 note), optionally tagged with GNSS time.
//! [`SampleTime`] records one such (sample index, host time) anchor. Times of other samples in the
//! same stream are derived from the anchor and the sample rate.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// A UTC instant as nanoseconds since the Unix epoch (range ±292 years).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(i64);

impl Timestamp {
    /// 1970-01-01T00:00:00Z.
    pub const UNIX_EPOCH: Timestamp = Timestamp(0);

    /// Builds a timestamp from nanoseconds since the Unix epoch.
    pub const fn from_unix_nanos(nanos: i64) -> Self {
        Self(nanos)
    }

    /// Nanoseconds since the Unix epoch.
    pub const fn as_unix_nanos(self) -> i64 {
        self.0
    }

    /// The host's current wall-clock time (saturating outside the i64 range).
    pub fn now() -> Self {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(after) => Self(i64::try_from(after.as_nanos()).unwrap_or(i64::MAX)),
            Err(before) => {
                Self(i64::try_from(before.duration().as_nanos()).map_or(i64::MIN, |n| -n))
            }
        }
    }

    /// Adds a signed number of nanoseconds, saturating at the range limits.
    pub const fn saturating_add_nanos(self, nanos: i64) -> Self {
        Self(self.0.saturating_add(nanos))
    }
}

/// A timing anchor: sample `sample_index` of a stream arrived at host time `host_time`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SampleTime {
    /// Running count of samples since the stream started (0-based).
    pub sample_index: u64,
    /// Host time assigned to that sample, according to the stream's [`TimestampMethod`].
    pub host_time: Timestamp,
}

impl SampleTime {
    /// Estimated time of another sample in the same stream, assuming a constant sample rate.
    ///
    /// Uses f64 arithmetic. Rounding stays below 1 ns for offsets up to about 10^16 ns
    /// (~100 days), which is well inside the timing error budget.
    pub fn time_of(&self, sample_index: u64, sample_rate_hz: f64) -> Timestamp {
        let delta_samples = sample_index as i128 - self.sample_index as i128;
        let delta_nanos = (delta_samples as f64 * 1e9 / sample_rate_hz).round();
        // `as` saturates for out-of-range floats.
        self.host_time.saturating_add_nanos(delta_nanos as i64)
    }
}

/// How timestamps were obtained. Pair it with an error budget
/// ([`crate::Provenance::timestamp_error_budget_ns`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TimestampMethod {
    /// Host USB arrival time of a buffer plus a running sample count. This is the HackRF One
    /// default. Buffering and USB jitter bound its accuracy; the budget comes from a spike.
    HostArrival,
    /// Host-arrival timing disciplined to GNSS time on the host (e.g. gpsd with PPS on the
    /// compute board).
    GnssTagged,
    /// Sample clock locked to an external reference with a known epoch (e.g. a GPSDO into
    /// CLKIN). Needed for sub-µs timing.
    ExternalReference,
    /// Generated data. Times are exact by construction.
    Synthetic,
    /// Not recorded, e.g. third-party SigMF without `hackriff:provenance`.
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_of_derives_from_anchor_and_rate() {
        let anchor = SampleTime {
            sample_index: 2_000_000,
            host_time: Timestamp::from_unix_nanos(1_000_000_000),
        };
        // 2 Msps: one second later is 2M samples later.
        assert_eq!(
            anchor.time_of(4_000_000, 2e6).as_unix_nanos(),
            2_000_000_000
        );
        // Earlier samples go backwards.
        assert_eq!(anchor.time_of(0, 2e6).as_unix_nanos(), 0);
        assert_eq!(
            anchor.time_of(2_000_001, 2e6).as_unix_nanos(),
            1_000_000_500
        );
    }

    #[test]
    fn serde_forms() {
        assert_eq!(
            serde_json::to_string(&Timestamp::from_unix_nanos(42)).unwrap(),
            "42"
        );
        assert_eq!(
            serde_json::to_string(&TimestampMethod::HostArrival).unwrap(),
            "\"host-arrival\""
        );
        assert!(Timestamp::now() > Timestamp::UNIX_EPOCH);
    }
}
