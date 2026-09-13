//! Provenance: the trust record attached to every SweepFrame, SpectrumFrame, Detection and
//! Recording (docs/07 §2.6).
//!
//! An 8-bit front end without a preselector produces intermodulation that looks like signals.
//! A measurement is only interpretable together with its gain state, overload flags,
//! calibration and spur mask.
//!
//! The struct holds the value only. Whether `provenance_id` is a field or only a repository key
//! (rows are deduplicated) is decided by T-002. Its JSON form is also the `hackriff:provenance`
//! SigMF extension value (docs/sigmf-extension.md).

use serde::{Deserialize, Serialize};

use crate::ids::{CalibrationStateId, SpurMaskId};
use crate::time::TimestampMethod;

/// Front-end tuning and gain state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tune {
    /// RF centre frequency, Hz.
    pub center_hz: f64,
    /// Complex sample rate, Hz.
    pub sample_rate_hz: f64,
    /// LNA (IF) gain, dB. HackRF: 0–40 in 8 dB steps.
    pub lna_db: f64,
    /// VGA (baseband) gain, dB. HackRF: 0–62 in 2 dB steps.
    pub vga_db: f64,
    /// RF amplifier on (HackRF: ~+11 dB).
    pub amp_on: bool,
    /// Baseband anti-alias filter bandwidth, Hz.
    pub bandwidth_hz: f64,
}

/// Sample clock source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClockSource {
    /// The on-board crystal or TCXO.
    Internal,
    /// An external 10 MHz reference into CLKIN, source unspecified.
    External,
    /// A GNSS-disciplined oscillator into CLKIN.
    Gpsdo,
}

/// The trust record for a measurement (docs/07 §2.6). Immutable once written.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    /// Source device identity, e.g. `hackrf:<serial>` or `synthetic:<generator>`.
    pub device_id: String,
    /// Tuning and gain state.
    pub tune: Tune,
    /// Clipped samples observed over the span this record covers.
    pub clip_count: u64,
    /// The front end was judged overloaded. Downstream detections become `suspect-IMD`.
    pub overload: bool,
    /// Board temperature, °C, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature_c: Option<f64>,
    /// Active antenna/filter port (e.g. an Opera Cake port name), if switched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub antenna_port: Option<String>,
    /// Sample clock source.
    pub clock_source: ClockSource,
    /// The clock is locked to its reference (always `true` for `internal`).
    pub clock_locked: bool,
    /// CalibrationState version in force, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration_state_ref: Option<CalibrationStateId>,
    /// SpurMask version in force, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spur_mask_ref: Option<SpurMaskId>,
    /// How timestamps were obtained.
    pub timestamp_method: TimestampMethod,
    /// One-sigma timestamp error budget, ns. `None` until characterised by the timing spike.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp_error_budget_ns: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample() -> Provenance {
        Provenance {
            device_id: "hackrf:0000000000000000a06063c8234e925f".into(),
            tune: Tune {
                center_hz: 433.92e6,
                sample_rate_hz: 2e6,
                lna_db: 32.0,
                vga_db: 20.0,
                amp_on: false,
                bandwidth_hz: 1.75e6,
            },
            clip_count: 3,
            overload: false,
            temperature_c: None,
            antenna_port: Some("A1".into()),
            clock_source: ClockSource::Internal,
            clock_locked: true,
            calibration_state_ref: Some(CalibrationStateId::new()),
            spur_mask_ref: None,
            timestamp_method: TimestampMethod::HostArrival,
            timestamp_error_budget_ns: Some(2_000_000),
        }
    }

    #[test]
    fn json_round_trip_omits_absent_options() {
        let p = sample();
        let json = serde_json::to_value(&p).unwrap();
        assert!(json.get("temperature_c").is_none());
        assert_eq!(json["clock_source"], "internal");
        assert_eq!(json["timestamp_method"], "host-arrival");
        assert_eq!(serde_json::from_value::<Provenance>(json).unwrap(), p);
    }
}
