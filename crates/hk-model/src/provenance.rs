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
///
/// `Ord` is declaration order (`Unknown` < `Off` < `On`) and carries **no meaning**: it exists so
/// the state can sit in a total ordering key — [`crate::attention::baseline::BaselineKey`], a
/// `BTreeMap` key and an on-disk path (T-333). Comparisons of state are equality only.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
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

/// A periodic artefact of the **capture chain** carried by a stream, not by any emission (T-373).
///
/// A capture path that modulates the samples periodically — a gain step at a buffer boundary, a
/// clock leaking into the gain structure — puts a comb of cyclic lines into *everything* the
/// stream holds, including bands that contain no emission at all. Read naively, a comb like that
/// is frame structure: the 8192-sample gain step T-317 found in
/// `fixtures/hackrf/capture-2026-09-15-fm-band` is 0.43 dB deep and puts ≥ 27 harmonics of
/// 292.969 Hz into the amplitude of every channel, and it reads as a 3.41 ms TDMA frame that is
/// not there. It cost an expert analyst a wrong answer before a control on the receiver's own CW
/// lines exposed it.
///
/// Recording it here is what lets analysis exclude it **without a magic constant**: the fundamental
/// is derived from this record and the stream's own sample rate ([`CaptureArtefact::fundamental_hz`]),
/// so a capture at a different rate excludes a different comb.
///
/// This describes the capture *state*, like the gain and the clock source, so it deduplicates with
/// the rest of the record and does not vary frame to frame.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptureArtefact {
    /// What it is, e.g. `periodic-gain-step`. Free text on purpose: the set of ways a capture
    /// chain can stamp a period into a stream is open, and nothing keys behaviour off this — the
    /// exclusion is driven by the **measured period** below, never by the label. A closed enum
    /// would make a future fixture's unknown kind fail to parse the whole provenance record.
    pub kind: String,
    /// Period in samples of the stream this was measured in, when the artefact is locked to the
    /// sample clock. The cyclic fundamental is then `sample_rate_hz / period_samples`, so it
    /// **moves with the sample rate** — which is exactly why no analysis may hardcode a frequency.
    ///
    /// A replayed recording is the one case where it does not move: the artefact is frozen into
    /// the stored samples, so resampling a recording scales `period_samples` and leaves the
    /// fundamental where it was (see `MockSource`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_samples: Option<f64>,
    /// Period in seconds, when known independently of the sample rate. Used when
    /// [`CaptureArtefact::period_samples`] is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_s: Option<f64>,
    /// Depth of the modulation, dB, when it is an amplitude artefact. Informational.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_db: Option<f64>,
    /// Measured fractional wander of the fundamental over the record, ppm (T-382).
    ///
    /// **A free-running artefact is a band, not a line, and the band widens with harmonic
    /// number.** A source locked to the samples or to a crystal does not move: the 8192-sample
    /// gain step holds `fs/8192` to 175 ppm and the host 8 kHz comb holds 8000 Hz to 1.1 ppm,
    /// both at the measurement floor. The ~655 Hz modulation in
    /// `fixtures/hackrf/capture-2026-09-15-fm-band` wanders **2300 ppm** over 45 s, which is why
    /// a fixed notch cannot cover it: harmonic *n* of a fundamental with frequency noise carries
    /// *n* times that noise (the physics T-317 used to pin an oscillator family by
    /// `width / n = const`), so a guard that does not scale with *n* misses the second harmonic
    /// and beyond — which is exactly the member that dominates here.
    ///
    /// Recorded in ppm rather than Hz for the same reason `period_samples` is in samples: it is
    /// the property of the source, and it stays true when the fundamental is re-derived for a
    /// different sample rate. Absent means "does not measurably move", which is the default and
    /// leaves the consumer's own guard the only width.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drift_ppm: Option<f64>,
    /// Highest comb member measured above the floor, when the comb **ends**.
    ///
    /// Usually absent, and absent is the honest default: a periodic artefact of period `T` puts
    /// energy at every multiple of `1/T`, falling as `1/n` but never stopping, so a count is a
    /// detection floor rather than a physical limit (the gain step's ≥ 27 harmonics, the host
    /// comb's members right across the span).
    ///
    /// It is recorded only when the *shape* says the comb is short: the ~655 Hz modulation is a
    /// near-sinusoid with one dominant harmonic (16.4 dB at h1, 23.5 dB at h2, 10.0 dB at h3 and
    /// nothing above the floor after that), not a pulse train. Excluding every multiple of it
    /// would be excluding members that were measured absent — and with [`Self::drift_ppm`]
    /// widening each guard by `n`, an unbounded comb would notch a quarter of the search band by
    /// harmonic 50.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harmonics: Option<u32>,
    /// How it was measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured_by: Option<String>,
    /// What it is, what it is not, and where else it was looked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl CaptureArtefact {
    /// The cyclic fundamental this artefact puts into a stream sampled at `sample_rate_hz`, Hz.
    ///
    /// Prefers the sample-locked form `sample_rate_hz / period_samples`, because that is the
    /// physics: the artefact is generated once per buffer of samples, so the rate carries it.
    /// Falls back to `1 / period_s` for an artefact tied to wall time instead. `None` when neither
    /// is recorded or the numbers are not usable.
    pub fn fundamental_hz(&self, sample_rate_hz: f64) -> Option<f64> {
        if let Some(p) = self.period_samples
            && p.is_finite()
            && p > 0.0
            && sample_rate_hz.is_finite()
            && sample_rate_hz > 0.0
        {
            return Some(sample_rate_hz / p);
        }
        self.period_s
            .filter(|p| p.is_finite() && *p > 0.0)
            .map(|p| 1.0 / p)
    }
}

/// One capture-chain comb, as the line search needs it: where it starts, how far it wanders, and
/// where it stops (T-382).
///
/// Every field is *derived* — the fundamental from the recorded period against the stream's own
/// sample rate, the tolerance from the recorded drift — so nothing downstream carries a frequency.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CyclicComb {
    /// Fundamental in this stream, Hz.
    pub fundamental_hz: f64,
    /// Measured fractional wander, as a fraction (not ppm). `0.0` when the artefact does not move.
    /// Harmonic `n` wanders by `n · fundamental_hz · drift`.
    pub drift: f64,
    /// Highest member measured present, or `None` for a comb with no measured end.
    pub harmonics: Option<u32>,
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
    /// Periodic artefacts the capture chain stamps into the samples (T-373).
    ///
    /// Omitted from the JSON form when empty, so provenance written before this field existed
    /// reads back as "none recorded" and its canonical JSON — and therefore its SHA-256 dedup key
    /// — is unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capture_artefacts: Vec<CaptureArtefact>,
}

impl Provenance {
    /// Cyclic fundamentals the capture chain contributes to this stream, Hz.
    ///
    /// Derived from [`Provenance::capture_artefacts`] and this record's **own** sample rate, so
    /// the same artefact excludes 292.969 Hz in a 2.4 Msps capture and 1220.703 Hz in a 10 Msps
    /// one. Nothing downstream may hardcode either number.
    ///
    /// A harmonic count is reported only where one was *measured* (see
    /// [`CaptureArtefact::harmonics`]); the usual answer is `None`, because a periodic artefact of
    /// period T puts energy at *every* multiple of 1/T, falling as 1/n but never stopping, so a
    /// count is normally a detection floor and not a physical limit. A consumer then excludes
    /// every multiple its own analysis band holds.
    pub fn cyclic_combs(&self) -> Vec<CyclicComb> {
        self.capture_artefacts
            .iter()
            .filter_map(|a| {
                Some(CyclicComb {
                    fundamental_hz: a.fundamental_hz(self.tune.sample_rate_hz)?,
                    drift: a
                        .drift_ppm
                        .filter(|d| d.is_finite() && *d > 0.0)
                        .map_or(0.0, |d| d * 1e-6),
                    harmonics: a.harmonics.filter(|h| *h > 0),
                })
            })
            .collect()
    }

    /// The fundamentals of [`Provenance::cyclic_combs`], Hz — the reportable form.
    pub fn cyclic_artefacts(&self) -> Vec<f64> {
        self.cyclic_combs()
            .into_iter()
            .map(|c| c.fundamental_hz)
            .collect()
    }
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
            capture_artefacts: Vec::new(),
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

    /// T-373: the cyclic comb a capture artefact contributes is a function of the **capture's own
    /// sample rate**, so the same recorded 8192-sample period excludes 292.969 Hz in a 2.4 Msps
    /// capture and 1220.703 Hz in a 10 Msps one. Nothing downstream may carry either constant.
    fn gain_step() -> CaptureArtefact {
        CaptureArtefact {
            kind: "periodic-gain-step".into(),
            period_samples: Some(8192.0),
            period_s: Some(8192.0 / 2.4e6),
            step_db: Some(-0.431),
            drift_ppm: None,
            harmonics: None,
            measured_by: Some("T-317".into()),
            note: None,
        }
    }

    /// T-382: the ~655 Hz modulation of the same fixture — wall-time, free-running, and short.
    fn drifting_modulation() -> CaptureArtefact {
        CaptureArtefact {
            kind: "drifting-noise-floor-modulation".into(),
            period_samples: None,
            period_s: Some(1.0 / 655.198),
            step_db: None,
            drift_ppm: Some(2300.0),
            harmonics: Some(3),
            measured_by: Some("T-382".into()),
            note: None,
        }
    }

    #[test]
    fn a_capture_artefact_fundamental_moves_with_the_sample_rate() {
        let a = gain_step();
        assert!((a.fundamental_hz(2.4e6).unwrap() - 292.968_75).abs() < 1e-9);
        assert!((a.fundamental_hz(10e6).unwrap() - 1_220.703_125).abs() < 1e-9);
        assert!((a.fundamental_hz(20e6).unwrap() - 2_441.406_25).abs() < 1e-9);

        // Wall-time artefacts fall back to the recorded period; a record with neither says nothing.
        let wall = CaptureArtefact {
            period_samples: None,
            period_s: Some(0.01),
            ..gain_step()
        };
        assert!((wall.fundamental_hz(2.4e6).unwrap() - 100.0).abs() < 1e-9);
        let silent = CaptureArtefact {
            period_samples: None,
            period_s: None,
            ..gain_step()
        };
        assert_eq!(silent.fundamental_hz(2.4e6), None);
        // Nonsense never becomes a notch.
        let bad = CaptureArtefact {
            period_samples: Some(0.0),
            period_s: None,
            ..gain_step()
        };
        assert_eq!(bad.fundamental_hz(2.4e6), None);
    }

    #[test]
    fn cyclic_artefacts_use_this_records_own_rate() {
        let mut p = sample();
        p.capture_artefacts = vec![gain_step()];
        p.tune.sample_rate_hz = 2.4e6;
        assert_eq!(p.cyclic_artefacts(), vec![292.968_75]);
        p.tune.sample_rate_hz = 10e6;
        assert_eq!(p.cyclic_artefacts(), vec![1_220.703_125_f64]);
    }

    /// T-382: a sample-locked artefact and a free-running one are different objects, and the
    /// difference has to survive to the consumer — the first is a line at one frequency, the
    /// second a band that widens with harmonic number and then stops.
    #[test]
    fn cyclic_combs_carry_the_measured_drift_and_the_measured_end() {
        let mut p = sample();
        p.tune.sample_rate_hz = 2.4e6;
        p.capture_artefacts = vec![gain_step(), drifting_modulation()];
        let combs = p.cyclic_combs();
        assert_eq!(combs.len(), 2);

        // Sample-locked: no drift recorded means no extra width, and no measured end.
        assert!((combs[0].fundamental_hz - 292.968_75).abs() < 1e-9);
        assert_eq!(combs[0].drift, 0.0);
        assert_eq!(combs[0].harmonics, None);

        // Free-running and wall-timed: the fundamental does *not* move with the sample rate, and
        // the drift arrives as a fraction so harmonic n is `n · f₀ · drift` wide.
        assert!((combs[1].fundamental_hz - 655.198).abs() < 1e-6);
        assert!((combs[1].drift - 2.3e-3).abs() < 1e-12);
        assert_eq!(combs[1].harmonics, Some(3));
        p.tune.sample_rate_hz = 10e6;
        let at_10 = p.cyclic_combs();
        assert!(
            (at_10[0].fundamental_hz - 1_220.703_125).abs() < 1e-9,
            "sample-locked moves"
        );
        assert!(
            (at_10[1].fundamental_hz - 655.198).abs() < 1e-6,
            "wall-timed does not"
        );

        // Nonsense never becomes a width or an end.
        p.capture_artefacts = vec![CaptureArtefact {
            drift_ppm: Some(f64::NAN),
            harmonics: Some(0),
            ..drifting_modulation()
        }];
        assert_eq!(p.cyclic_combs()[0].drift, 0.0);
        assert_eq!(p.cyclic_combs()[0].harmonics, None);
    }

    /// T-382 migration: the two fields are absent from every row written before they existed, and
    /// adding them must not move a stored row's canonical JSON or its SHA-256 dedup key.
    #[test]
    fn drift_and_harmonics_are_omitted_when_absent() {
        let mut p = sample();
        p.capture_artefacts = vec![gain_step()];
        let json = serde_json::to_value(&p).unwrap();
        let a = &json["capture_artefacts"][0];
        assert!(a.get("drift_ppm").is_none(), "{a}");
        assert!(a.get("harmonics").is_none(), "{a}");
        assert_eq!(serde_json::from_value::<Provenance>(json).unwrap(), p);
    }

    /// T-373 migration: every provenance row written before capture artefacts existed has no
    /// `capture_artefacts` key, must read back as "none recorded", and must re-serialise
    /// byte-identically so its SHA-256 dedup key does not move.
    #[test]
    fn provenance_without_capture_artefacts_round_trips_unchanged() {
        let p = sample();
        assert!(p.capture_artefacts.is_empty());
        let json = serde_json::to_value(&p).unwrap();
        assert!(
            json.get("capture_artefacts").is_none(),
            "an empty list is omitted, not written as [] — {json}"
        );
        let read: Provenance = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(read, p);
        assert_eq!(serde_json::to_value(&read).unwrap(), json);
    }

    /// The fixture's recorded form parses, including keys this struct does not model.
    #[test]
    fn a_recorded_capture_artefact_parses_with_unmodelled_keys() {
        let stored = serde_json::json!({
            "kind": "periodic-gain-step",
            "period_samples": 8192,
            "period_s": 0.0034133333333333333,
            "low_window_samples": 896,
            "step_db": -0.431,
            "measured_by": "T-317",
            "note": "stream-wide",
        });
        let a: CaptureArtefact = serde_json::from_value(stored).unwrap();
        assert_eq!(a.kind, "periodic-gain-step");
        assert_eq!(a.period_samples, Some(8192.0));
        assert!((a.fundamental_hz(2.4e6).unwrap() - 292.968_75).abs() < 1e-9);
    }

    #[test]
    fn legacy_clip_count_key_is_ignored_on_read() {
        let p = sample();
        let mut json = serde_json::to_value(&p).unwrap();
        json["clip_count"] = serde_json::json!(12);
        assert_eq!(serde_json::from_value::<Provenance>(json).unwrap(), p);
    }
}
