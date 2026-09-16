//! Provenance: the trust record attached to every SweepFrame, SpectrumFrame, Detection and
//! Recording (docs/07 §2.6).
//!
//! An 8-bit front end without a preselector produces intermodulation that looks like signals.
//! A measurement is only interpretable together with its gain state, overload flags,
//! calibration and spur mask.
//!
//! Provenance describes **state**, not per-span measurements, so that the repository can
//! deduplicate it by value (many frames and detections share one row while nothing changes).
//! Per-span clipped-sample counts therefore live on `Detection::clip_count` and on the SigMF
//! capture key `hackriff:clip_count`, not here. Its JSON form is the `hackriff:provenance` SigMF
//! extension value (docs/sigmf-extension.md).

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

/// Antenna-port bias-tee state under this provenance (T-325).
///
/// **Three states, and `Unknown` is never `Off`.** A bias tee puts DC on the antenna port: into an
/// active antenna that expects it, fine; into a **passive antenna or a DC-shorted port** it is a
/// fault condition (C36 pitfall). It also changes the measurement — an active antenna's LNA moves
/// the noise floor and the gain structure, so a capture taken with the bias tee on is not
/// comparable to one taken with it off.
///
/// Both uses need to tell "the device says the DC is off" apart from "nothing said". A device that
/// cannot report its bias tee, a replayed recording whose file carries no such field, and a device
/// reporting off are three different facts; collapsing the first two into `Off` would claim the
/// port was safe and the measurement comparable on no evidence at all. This is deliberately **not
/// a `bool`** for the same reason [`crate::trunking::Encryption`] is not: "unknown" read as the
/// benign value is the defect this project guards against.
///
/// There is therefore no `bool` conversion. [`BiasTee::powered`] returns `Option<bool>` so the
/// unknown case has to be handled at every site that cares.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BiasTee {
    /// The source cannot say: a replayed recording or synthetic data (the file records no
    /// bias-tee state), or a driver that does not track it. **Never read this as off** — the DC
    /// may well have been on. This is the default, so provenance written before T-325 reads as
    /// unknown rather than silently claiming off.
    #[default]
    Unknown,
    /// The device reports its bias tee off: no DC on the antenna port.
    Off,
    /// The device reports its bias tee on: DC is on the antenna port. The port must be an active
    /// antenna or LNA that expects power, and the measurement is not comparable with bias-tee-off
    /// captures.
    On,
}

impl BiasTee {
    /// The state as stored text (`unknown`, `off`, `on`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Off => "off",
            Self::On => "on",
        }
    }

    /// `true` when nothing is known about the bias tee. Used to omit the field from the SigMF and
    /// canonical JSON forms (docs/sigmf-extension.md: omit an unknown field, never write `null`).
    pub const fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }

    /// Whether DC is on the antenna port, or `None` when the source cannot say.
    ///
    /// `None` means **unknown, not off**: do not `unwrap_or(false)` it into a claim that the port
    /// was passive-safe or that the measurement is comparable with a bias-tee-off capture.
    pub const fn powered(self) -> Option<bool> {
        match self {
            Self::Unknown => None,
            Self::Off => Some(false),
            Self::On => Some(true),
        }
    }
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

/// The trust record for a measurement (docs/07 §2.6). Immutable once written; deduplicated by
/// value in storage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    /// Source device identity, e.g. `hackrf:<serial>` or `synthetic:<generator>`.
    pub device_id: String,
    /// Tuning and gain state.
    pub tune: Tune,
    /// Sticky tune-state flag: the front end was judged overloaded under this tune/gain state.
    /// It changes only by minting a new Provenance (e.g. after a gain step). Every Detection under
    /// an overloaded provenance must set `flags.clipped` (suspect IMD); the repository enforces it.
    pub overload: bool,
    /// The noise floor under this gain state is within 3 dB of the ADC quantisation floor, so weak
    /// signals are limited by the 8-bit ADC rather than by thermal noise (spike S4). Stable per
    /// gain state, so it does not defeat deduplication.
    pub quantisation_limited: bool,
    /// Board temperature, °C, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature_c: Option<f64>,
    /// Active antenna/filter port (e.g. an Opera Cake port name), if switched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub antenna_port: Option<String>,
    /// Antenna-port bias-tee state (T-325): device-local context like gain state, so it is read
    /// from the device and recorded here (T-259: device-local physics must read the device).
    ///
    /// A bias tee left on into a passive or DC-shorted port is both a hardware hazard and a
    /// measurement confound, and neither is visible without this field. Omitted from the JSON
    /// form when [`BiasTee::Unknown`], so provenance written before T-325 reads back as unknown
    /// and its canonical JSON — and therefore its dedup hash — is unchanged.
    #[serde(default, skip_serializing_if = "BiasTee::is_unknown")]
    pub bias_tee: BiasTee,
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
            overload: false,
            quantisation_limited: false,
            temperature_c: None,
            antenna_port: Some("A1".into()),
            bias_tee: BiasTee::On,
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
        assert!(json.get("clip_count").is_none());
        assert_eq!(json["clock_source"], "internal");
        assert_eq!(json["timestamp_method"], "host-arrival");
        assert_eq!(serde_json::from_value::<Provenance>(json).unwrap(), p);
    }

    /// T-325: the three bias-tee states are distinct on the wire, and `unknown` is not `off`.
    #[test]
    fn bias_tee_states_are_three_distinct_values() {
        let mut p = sample();
        for (state, text) in [(BiasTee::On, "on"), (BiasTee::Off, "off")] {
            p.bias_tee = state;
            let json = serde_json::to_value(&p).unwrap();
            assert_eq!(json["bias_tee"], text);
            assert_eq!(serde_json::from_value::<Provenance>(json).unwrap(), p);
        }
        // Unknown is omitted, never written as `off` or `null`.
        p.bias_tee = BiasTee::Unknown;
        let json = serde_json::to_value(&p).unwrap();
        assert!(json.get("bias_tee").is_none(), "{json}");
        assert_eq!(serde_json::from_value::<Provenance>(json).unwrap(), p);

        assert_ne!(BiasTee::Unknown, BiasTee::Off);
        assert_eq!(BiasTee::Unknown.powered(), None, "unknown is not off");
        assert_eq!(BiasTee::Off.powered(), Some(false));
        assert_eq!(BiasTee::On.powered(), Some(true));
    }

    /// T-325 migration: a provenance row stored before this field existed has no `bias_tee` key.
    /// It must read back as `Unknown` — not `Off` — and its canonical JSON must be byte-identical
    /// to what it was, so the SHA-256 dedup key of every stored row is unchanged.
    #[test]
    fn provenance_stored_before_the_field_existed_reads_as_unknown() {
        let p = sample();
        let mut stored = serde_json::to_value(&p).unwrap();
        // Exactly what a pre-T-325 writer produced: the same record with no bias-tee key at all.
        stored.as_object_mut().unwrap().remove("bias_tee");
        let read: Provenance = serde_json::from_value(stored.clone()).unwrap();
        assert_eq!(read.bias_tee, BiasTee::Unknown, "a missing key is unknown");
        assert_ne!(read.bias_tee, BiasTee::Off, "'nothing said' is never 'off'");

        // Re-serialising that row reproduces the stored bytes, so its dedup hash does not move.
        assert_eq!(serde_json::to_value(&read).unwrap(), stored);
    }

    #[test]
    fn legacy_clip_count_key_is_ignored_on_read() {
        let p = sample();
        let mut json = serde_json::to_value(&p).unwrap();
        json["clip_count"] = serde_json::json!(12);
        assert_eq!(serde_json::from_value::<Provenance>(json).unwrap(), p);
    }
}
