//! Emitter, the inventory entry (docs/07 §2.11; C27).
//!
//! Split between the parts that change and the parts that must keep history:
//!
//! - **Aggregate** (mutable, updated by upsert): current frequency and bandwidth, first/last
//!   seen, count, displayed identity, tags.
//! - **Append-only interpretation:** the [`Classification`] history and the known-status history
//!   ([`KnownStatusChange`]). Nothing is overwritten; the latest row of each is current.
//! - **Links** ([`EmitterLink`]) to tracks, detections, recordings, demodulations, decodes,
//!   anomalies, explanations and annotations are append-only rows, loaded separately because
//!   they can be numerous.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{
    AnnotationId, AnomalyId, DecodeId, DemodulationId, DetectionId, EmitterId, ExplanationId,
    RecordingId, TrackId,
};
use crate::region::{FreqRange, TimeRange};
use crate::time::Timestamp;

/// The namespace of a decoded identity.
///
/// Serialised as a string: the kebab-case variant name, or `other:<name>` for [`Self::Other`].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum IdentityScheme {
    /// ADS-B / Mode S 24-bit ICAO address, lowercase hex, e.g. `a1b2c3`.
    AdsbIcao,
    /// RDS Programme Identification code, uppercase hex, e.g. `C0DE`.
    RdsPi,
    /// AIS Maritime Mobile Service Identity, 9 digits.
    AisMmsi,
    /// Trunked-radio talkgroup, `<system>:<talkgroup>`.
    Talkgroup,
    /// rtl_433-style sensor id, `<model>:<id>`.
    SensorId,
    /// Any other scheme, named by its decoder.
    Other(String),
}

impl IdentityScheme {
    /// String form.
    pub fn as_string(&self) -> String {
        match self {
            IdentityScheme::AdsbIcao => "adsb-icao".into(),
            IdentityScheme::RdsPi => "rds-pi".into(),
            IdentityScheme::AisMmsi => "ais-mmsi".into(),
            IdentityScheme::Talkgroup => "talkgroup".into(),
            IdentityScheme::SensorId => "sensor-id".into(),
            IdentityScheme::Other(name) => format!("other:{name}"),
        }
    }

    /// Whether many instances of this scheme normally share one channel (aircraft on 1090 MHz,
    /// ships on AIS, sensors on 433.92 MHz, talkgroups on a trunk). Entity resolution (T-018)
    /// never folds an anonymous fingerprint match or a channel-level emitter context into an
    /// emitter identified by such a scheme: the fingerprint cannot tell the instances apart.
    /// Unknown schemes are treated as shared (the conservative choice, avoiding over-merging).
    pub fn shares_channel(&self) -> bool {
        !matches!(self, IdentityScheme::RdsPi)
    }
}

impl fmt::Display for IdentityScheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_string())
    }
}

impl From<IdentityScheme> for String {
    fn from(s: IdentityScheme) -> String {
        s.as_string()
    }
}

impl FromStr for IdentityScheme {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "adsb-icao" => IdentityScheme::AdsbIcao,
            "rds-pi" => IdentityScheme::RdsPi,
            "ais-mmsi" => IdentityScheme::AisMmsi,
            "talkgroup" => IdentityScheme::Talkgroup,
            "sensor-id" => IdentityScheme::SensorId,
            other => match other.strip_prefix("other:") {
                Some(name) if !name.is_empty() => IdentityScheme::Other(name.to_owned()),
                _ => return Err(format!("unknown identity scheme {other:?}")),
            },
        })
    }
}

impl TryFrom<String> for IdentityScheme {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

/// A decoded identifier naming a transmitter.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DecodedIdentity {
    /// Namespace.
    pub scheme: IdentityScheme,
    /// Value in the scheme's canonical form.
    pub value: String,
}

/// An emitter's identity: decoded, or not (yet) known.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Identity {
    /// No decoded identifier.
    Unknown,
    /// Named by a decoder (or a user) in a scheme.
    Decoded(DecodedIdentity),
}

/// Status against priors (band plans, licences, own history; C17).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KnownStatus {
    /// Matches an expected emitter here.
    Known,
    /// Recognised, but not expected at this place/frequency/time (e.g. out-of-allocation).
    UnexpectedHere,
    /// No match.
    Unknown,
}

/// Who decided a known-status change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusAuthor {
    /// Known-signal priors (C17): band plan, licence extract, reference database, own history.
    Prior,
    /// A decoder identity (C22).
    Decoder,
    /// A classifier (C15).
    Classifier,
    /// A person.
    User,
    /// The repository itself: the initial status written when an emitter is created.
    System,
    /// Emitter clustering (C18, T-018): the initial status of an emitter it created.
    Clusterer,
}

/// One entry of an emitter's append-only known-status history. The latest entry is the emitter's
/// current [`Emitter::known_status`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KnownStatusChange {
    /// Emitter.
    pub emitter_id: EmitterId,
    /// New status.
    pub status: KnownStatus,
    /// The prior record that decided it, e.g. `bandplan:us-fcc-2026#118-137MHz` or
    /// `fmlist:station/12345`, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_ref: Option<String>,
    /// Why, in words.
    pub reason: String,
    /// When.
    pub t: Timestamp,
    /// Who decided.
    pub author: StatusAuthor,
}

/// One classifier result, appended to an emitter's history (never overwritten).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Classification {
    /// When it was produced.
    pub t: Timestamp,
    /// Family/modulation/protocol label, e.g. `fsk2`, `adsb`, `unknown`.
    pub family: String,
    /// Confidence in `family`, 0–1.
    pub confidence: f64,
    /// Open-set score, 0–1: how far the input is from every known class. High = likely unknown.
    pub open_set_score: f64,
    /// Model or rule set and version, e.g. `amc-cnn@0.3.1`.
    pub model_version: String,
}

/// Target of an emitter link.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind", content = "id")]
pub enum LinkTarget {
    /// A Track.
    Track(TrackId),
    /// A Detection.
    Detection(DetectionId),
    /// A Recording.
    Recording(RecordingId),
    /// A Demodulation.
    Demodulation(DemodulationId),
    /// A Decode.
    Decode(DecodeId),
    /// An Anomaly.
    Anomaly(AnomalyId),
    /// An Explanation.
    Explanation(ExplanationId),
    /// An Annotation.
    Annotation(AnnotationId),
}

/// An append-only link from an emitter to a related object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EmitterLink {
    /// Emitter.
    pub emitter_id: EmitterId,
    /// Related object.
    pub target: LinkTarget,
    /// When the link was made.
    pub linked_at: Timestamp,
}

/// The persistent "thing seen on the air" (docs/07 §2.11).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Emitter {
    /// Stable for the life of the cluster.
    pub id: EmitterId,
    /// Current centre frequency, Hz.
    pub f_center_hz: f64,
    /// Current bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// First sighting.
    pub first_seen: Timestamp,
    /// Latest sighting.
    pub last_seen: Timestamp,
    /// Sightings (detections) summarised.
    pub count: u64,
    /// Fingerprint features (C18; shape not yet pinned).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub fingerprint: Value,
    /// Displayed identity, derived from decoder/user claims (C27 precedence).
    pub identity: Identity,
    /// Current status against priors: the latest [`KnownStatusChange`]. When inserting a new
    /// emitter it becomes the first history entry; afterwards change it only by appending.
    pub known_status: KnownStatus,
    /// Classification history, oldest first. Append-only.
    #[serde(default)]
    pub classifications: Vec<Classification>,
    /// User and system tags.
    #[serde(default)]
    pub tags: BTreeSet<String>,
}

impl Emitter {
    /// Current frequency extent.
    pub fn freq(&self) -> FreqRange {
        FreqRange::centered(self.f_center_hz, self.bandwidth_hz)
    }

    /// First-to-last-seen span.
    pub fn seen(&self) -> TimeRange {
        TimeRange::new(self.first_seen, self.last_seen)
    }

    /// Latest classification, if any.
    pub fn current_classification(&self) -> Option<&Classification> {
        self.classifications.last()
    }
}

/// A batch of sightings of one emitter, merged into the inventory by
/// `Repository::upsert_emitter_observation`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EmitterObservation {
    /// Emitter the caller resolved the sightings to (entity resolution is C18/T-018's job).
    pub emitter_id: EmitterId,
    /// Time span of the sightings.
    pub seen: TimeRange,
    /// Number of sightings; added to the emitter's count.
    pub count: u64,
    /// Observed centre frequency, Hz (becomes current if this observation is the latest).
    pub f_center_hz: f64,
    /// Observed bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// Identity decoded during these sightings, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<DecodedIdentity>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_scheme_string_forms() {
        for s in [
            IdentityScheme::AdsbIcao,
            IdentityScheme::RdsPi,
            IdentityScheme::AisMmsi,
            IdentityScheme::Talkgroup,
            IdentityScheme::SensorId,
            IdentityScheme::Other("dmr-radio-id".into()),
        ] {
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(serde_json::from_str::<IdentityScheme>(&json).unwrap(), s);
        }
        assert_eq!(
            serde_json::to_string(&IdentityScheme::Other("x".into())).unwrap(),
            "\"other:x\""
        );
        assert!("other:".parse::<IdentityScheme>().is_err());
        assert!("bogus".parse::<IdentityScheme>().is_err());
    }
}
