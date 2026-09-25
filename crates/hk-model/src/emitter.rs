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

/// Name of the blind framer's structural identity scheme (`other:hk-framing`): a framing signature,
/// not a transmitter ([`IdentityScheme::is_structural`]).
pub const FRAMING_IDENTITY_SCHEME: &str = "hk-framing";

/// **T-962: agreeing CRC-valid frames an RDS PI needs before any decoder may write it as an
/// identity** — [`IdentityScheme::commit_votes`] for [`IdentityScheme::RdsPi`].
///
/// One bar for every producer: `hk-demod`'s always-on RDS decoder (`GroupConfig::pi_commit_votes`
/// defaults to this) and every recipe `messages` output naming `rds-pi` (the writer counts
/// agreeing frames per output, `hk_pipeline::recipes::messages`). A vote is one CRC-valid,
/// PI-bearing group; a recipe frame is at least one group, so counting frames never counts more
/// groups than there are. Why 10: a synchronised RDS stream carries 11.4 groups/s (1187.5 Bd /
/// 104 bits), so 10 agreeing votes is under a second of genuine lock, while the observed false
/// commit (98.085 MHz, PI 1704, an independent oracle finding no RDS on the same clip) reached
/// about 3 in 45 s. RDS's block check is 10 bits and a mis-synchronised lattice re-reads
/// correlated bits, so a handful of agreeing groups is not independent evidence. Below the bar the
/// PI is a **provisional** reading — recorded and shown with its vote, never an identity, never a
/// confirm (ADR-0022 §1.3: a confirm is a one-way door). Raising it is safe; lowering it is not.
pub const RDS_PI_COMMIT_VOTES: u32 = 10;

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

    /// Whether this scheme describes **structure, not a transmitter** (T-879): the blind framer's
    /// `other:hk-framing` signature (sync word, CRC, payload length) is what every unit of one
    /// device type sends, so it cannot say *which* unit was heard.
    ///
    /// Entity resolution therefore never uses it as a key: a sighting carrying one resolves by its
    /// ledger or re-measurement, and otherwise starts its own entry (it still [shares a
    /// channel](Self::shares_channel), so neither a context nor a fingerprint folds it into
    /// another), which the pipeline's same-emission link joins to the track it came from. The
    /// identity is stored on that entry when no other emitter already holds it (the identity
    /// index is unique), and a structural claim is never an identity *conflict*. Before
    /// T-879 three units of one sensor type on three channels, 1.5 MHz apart, all resolved to the
    /// first unit's emitter by this signature — one emitter over three appearances.
    pub fn is_structural(&self) -> bool {
        matches!(self, IdentityScheme::Other(name) if name == FRAMING_IDENTITY_SCHEME)
    }

    /// Agreeing CRC-valid frames (votes) a decoder must have seen before it may attach this
    /// identity to a Decode row (T-962). `1` — a single CRC-valid frame — for every scheme whose
    /// check is strong enough to carry an identity alone (ADS-B's 24-bit parity, …);
    /// [`RDS_PI_COMMIT_VOTES`] for an RDS PI, whose 10-bit block check is not. Below it the
    /// decoder writes the row **without** an identity and marks it provisional, so neither entity
    /// resolution nor the confirm gate's decoded-identity route can rest on it.
    pub fn commit_votes(&self) -> u32 {
        match self {
            IdentityScheme::RdsPi => RDS_PI_COMMIT_VOTES,
            _ => 1,
        }
    }

    /// The canonical value of an identity read from an unsigned field of `bits` bits (0 when the
    /// width is unknown), rendered in hex or decimal: the schemes with a documented form get it
    /// (`adsb-icao` 6 lower-case hex digits, `rds-pi` 4 upper-case, `ais-mmsi` 9 digits);
    /// others are lower-case hex zero-padded to the field width, or plain decimal (T-111).
    pub fn canonical_uint(&self, value: u64, bits: u32, hex: bool) -> String {
        match self {
            IdentityScheme::AdsbIcao => format!("{value:06x}"),
            IdentityScheme::RdsPi => format!("{value:04X}"),
            IdentityScheme::AisMmsi => format!("{value:09}"),
            _ if hex => format!("{value:0w$x}", w = bits.div_ceil(4) as usize),
            _ => value.to_string(),
        }
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

/// Inventory lifecycle of an emitter (T-078, docs/07 §2.11).
///
/// Every emitter starts as a `Candidate`. An auto rule (strong, unambiguous evidence) or a user
/// promotes it to `Confirmed`; a user deletes either. `Deleted` is final for that row: it leaves
/// the inventory and entity resolution, and a later sighting of the same signal creates a new
/// candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LifecycleState {
    /// Seen, not (yet) confirmed: intermittent, weak or not yet enough evidence.
    Candidate,
    /// Confirmed by an auto rule or a user.
    Confirmed,
    /// Removed from the inventory by a user; row, links and history are kept.
    Deleted,
}

/// Who changed an emitter's lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LifecycleAuthor {
    /// An automatic confirmation rule; `actor` names the rule and version.
    Auto,
    /// A person; `actor` names the credential (e.g. the API token fingerprint).
    User,
}

/// One entry of an emitter's append-only lifecycle history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LifecycleChange {
    /// Emitter.
    pub emitter_id: EmitterId,
    /// New state.
    pub state: LifecycleState,
    /// State before.
    pub previous: LifecycleState,
    /// Who.
    pub author: LifecycleAuthor,
    /// Rule id (`Auto`) or credential fingerprint (`User`).
    pub actor: String,
    /// Why, in words (the rule's evidence, or the user's note).
    pub reason: String,
    /// When: data time for auto rules, wall time for users.
    pub t: Timestamp,
}

/// One appearance of an emitter: a source observation counted into it (a track, or a decoder
/// sighting when it has no tracks).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Appearance {
    /// Time span.
    pub time: TimeRange,
    /// Sightings (bursts) it added.
    pub count: u64,
    /// Duty cycle within the span, when the tracker measured one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duty_cycle: Option<f64>,
}

/// Recurrence statistics of an emitter (T-078), computed from its observation ledger.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Recurrence {
    /// Sightings summed (the emitter's `count`: bursts for tracks).
    pub occurrences: u64,
    /// Appearances (track observations; decoder sightings when there are no tracks).
    pub appearances: u64,
    /// First to last appearance, s.
    pub span_s: f64,
    /// Time on air: Σ appearance span × duty cycle, s. An appearance without a measured duty
    /// cycle adds nothing (unknown duty is not evidence of continuity).
    pub on_air_s: f64,
    /// `on_air_s / span_s` (≤ 1), `None` for a zero span.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duty_cycle: Option<f64>,
    /// The latest appearances, newest first.
    pub recent: Vec<Appearance>,
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
    /// First sighting — the **lower end of the hull**, never the start of a duration. See
    /// [`Self::last_seen`].
    pub first_seen: Timestamp,
    /// Latest sighting. With [`Self::first_seen`] this is the **hull** of the presence track
    /// (docs/07 §2.11, ADR-0017 §1.1), **never a time extent and never to be displayed as a
    /// duration**: an emitter that fired once at 09:00 and once at 17:00 has an eight-hour hull
    /// and may have been on the air for twenty seconds.
    ///
    /// The time extent lives on the presence interval ([`crate::presence`]). Ask
    /// `Repository::presence` for "how long was this on air" and "is it on air now"; a window
    /// filter must test **interval overlap**, not hull overlap, or it matches every window
    /// between two distant sightings.
    pub last_seen: Timestamp,
    /// Sightings (detections) summarised: a **lifetime total, valid only in History**.
    ///
    /// **Excluded from every liveness decision and from live-list ranking** (docs/07 §2.11,
    /// ADR-0017 §5). It was the only column that could hold "this is still here", which is why it
    /// was observed climbing to 582,500 per hour with nothing ageing out — the counter was not
    /// wrong, it was homeless. What accumulates instead is the open presence interval's `t_end`;
    /// what ranks a live list is in-window on-air time ([`crate::presence::Presence::on_air_s`]).
    /// Its existing uses in `recurrence.occurrences` and `ConfirmPolicy` are unchanged.
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

    /// First-to-last-seen span: the **hull** of the presence track, **never a duration**.
    ///
    /// This is a bounding box in time, not time on air, and it is mostly silence for anything
    /// intermittent. Use it to bound a scan, never to say how long a signal transmitted or
    /// whether it is transmitting now — `Repository::presence` answers both from the intervals
    /// ([`crate::presence`], docs/07 §2.27).
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
