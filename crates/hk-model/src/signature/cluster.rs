//! C18 clusters of unknown emissions (T-202, ADR-0016 §5): "the same thing I saw before".
//!
//! # What a cluster is, and what it is not
//!
//! A cluster is a **type above emitters**, which are *instances* (the C18 pitfall: two identical
//! sensors share a cluster and stay two emitters). It is built from measurement alone — the
//! catalogue plays no part in forming one — and it is **evidence, never identity**: nothing here
//! can set an emitter's identity, family, `known_status` or lifecycle, and the type system carries
//! most of that (a [`SignatureCluster`] has nowhere to put an identity). The rest is pinned by
//! tests in `hk_context::signature::cluster`.
//!
//! So the honest reading of a cluster id on an inventory row is "this emission measures like
//! these others", never "this emission *is* X". A cluster only becomes a *hypothesis* — a
//! [`crate::signature::Signature`] with provenance `cluster-promoted` — when a user promotes it or
//! a recipe decodes a member with a valid CRC, and even then the signature is still ranked
//! evidence.
//!
//! # Why a wrong merge is the failure to avoid
//!
//! Two genuinely distinct emitters sharing one cluster id is worse than one emitter's sightings
//! landing in two clusters: the first invents a relationship that is not there and hides one
//! emission behind another, while the second is visible fragmentation that the repair pass can
//! fix. Every rule here is biased that way — the separation guard in
//! `hk_context::signature::cluster` refuses a join on *any* conflicting field however few fields
//! the two share, and abstains outright when too little is comparable.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::{Feat, SIGNATURE_SCHEMA, SignatureRef, field, fold_field};
use crate::ids::EmitterId;
use crate::time::Timestamp;

/// Smallest number of member emitters that makes a cluster visible (ADR-0016 §5).
pub const CLUSTER_MIN_MEMBERS: usize = 3;

/// Smallest number of separated appearances that makes a **single**-member cluster visible: one
/// emitter seen again and again is "the same thing I saw before" just as much as three emitters
/// that measure alike (ADR-0016 §5: "≥ 3 appearances of one emitter across ≥ 2 sessions").
pub const CLUSTER_MIN_APPEARANCES: u32 = 3;

/// Fields a cluster compares, and **only** these.
///
/// Two exclusions carry the whole "type, not instance" rule:
///
/// - **Frequency** (`f_center_hz`, `f_lo_hz`, `f_hi_hz`, `raster_offset_hz`) — two of the same
///   sensor model transmit on different channels, and the same channel is shared by unrelated
///   things. A type is not a frequency, so clustering never looks at where the emission sits;
///   entity resolution ([`crate::cluster`]) is what keeps sightings of one *instance* together.
/// - **Propagation and oscillator** (`snr_db`, `cfo_offset_hz`) — these describe this receiver,
///   this path and this individual transmitter's crystal, not the protocol. Per-transmitter RF
///   fingerprinting (AWARE-047/051) is deliberately *not* done in M3 (ADR-0016 §5, Privacy).
///
/// # Three of these measure the observation, and here they have no silence exclusion (T-281)
///
/// `period_s`, `duty_cycle` and `burst_length_s` are statistics of **how long the producer
/// happened to watch**, not properties of the emission. Entity resolution already knows this:
/// [`crate::cluster::Fingerprint::compare_across_silence`] drops exactly these three when two
/// sightings' presence intervals are disjoint (T-250/T-262), and
/// `crate::relate::distinguishing_evidence` does the same.
///
/// **The clustering distance has no such exclusion.** `hk_context::signature::cluster::compare`
/// compares every shared field below unconditionally, so two emitters of the same *type* watched
/// over different windows are pushed apart on these three — and, because one field at `z > 3`
/// separates a pair however much else agrees, a single disagreeing burst length is enough to
/// refuse the join outright. The three also count toward `CLUSTER_MIN_SHARED_FIELDS`, so they can
/// carry a join that nothing about the emission itself supports.
///
/// Left as-is rather than narrowed here: a cluster has no single presence interval to compare a
/// member against (a centroid folds many members' windows), so the fix is a design question about
/// what a centroid's time extent means, not a local edit. See the T-281 follow-ups.
pub const CLUSTER_FIELDS: &[&str] = &[
    field::OBW_HZ,
    field::FAMILY,
    field::CLASS,
    field::SYMBOL_RATE_HZ,
    field::DEVIATION_HZ,
    field::LEVELS,
    field::LINE_CODE,
    field::PREAMBLE,
    field::SYNC_WORD,
    field::PACKET_LENGTH_BITS,
    field::CRC_POLY,
    field::PERIOD_S,
    field::DUTY_CYCLE,
    field::BURST_LENGTH_S,
    field::TDMA_PERIOD_S,
    field::HOP_RASTER_HZ,
    field::HOP_COUNT,
    field::FLATNESS,
    field::SYMMETRY,
    field::CARRIER_LINE_DB,
    field::COMB_SPACING_HZ,
    field::COMB_COUNT,
    field::PRI_S,
    field::SCAN_PERIOD_S,
];

/// Whether `name` is one of the [`CLUSTER_FIELDS`].
pub fn is_cluster_field(name: &str) -> bool {
    CLUSTER_FIELDS.contains(&name)
}

/// A cluster's lifecycle. It says how much has been *seen*, never what the thing is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClusterState {
    /// Seeded but not yet visible: too few members, too few appearances.
    Pending,
    /// Visible: at least [`CLUSTER_MIN_MEMBERS`] members, or one member seen at least
    /// [`CLUSTER_MIN_APPEARANCES`] times.
    Active,
    /// Folded into another cluster by a repair pass; `merged_into` names the survivor.
    Merged,
    /// Promoted to a [`crate::signature::Signature`] (still evidence, never an identity).
    Promoted,
}

impl ClusterState {
    /// Stored spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Merged => "merged",
            Self::Promoted => "promoted",
        }
    }

    /// Whether a cluster in this state is offered to the API and to inventory rows.
    pub fn visible(self) -> bool {
        matches!(self, Self::Active | Self::Promoted)
    }
}

impl fmt::Display for ClusterState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a cluster has measured like, folded over its members.
///
/// The centroid folds **aggregates**, not raw sightings: each member arrives as its own
/// [`Feat`] carrying the uncertainty T-201 gave it, and that uncertainty is offered as the
/// observation's own `sigma_meas`. So the centroid's `sigma` is
/// `max(spread across members, mean member sigma)` — it never shrinks as `1/√n` at either level,
/// which is the rule that keeps a drifting emission clustering with itself instead of drifting
/// out of its own cluster.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterCentroid {
    /// Folded fields, by [`field`] name. Only [`CLUSTER_FIELDS`] appear.
    #[serde(default)]
    pub fields: BTreeMap<String, Feat>,
    /// Member folds counted in.
    #[serde(default)]
    pub folds: u32,
    /// Largest `suspect_fraction` of any folded member: a cluster is as suspect as its most
    /// suspect member, and nothing is minted from an all-suspect one (C18 card).
    #[serde(default)]
    pub suspect_fraction: f64,
}

impl ClusterCentroid {
    /// An empty centroid.
    pub fn new() -> Self {
        Self::default()
    }

    /// A field, if any member measured it.
    pub fn get(&self, name: &str) -> Option<&Feat> {
        self.fields.get(name)
    }

    /// A numeric field's value.
    pub fn num(&self, name: &str) -> Option<f64> {
        self.fields.get(name)?.value.num()
    }

    /// Folds one member's measured fields in, keeping only [`CLUSTER_FIELDS`].
    ///
    /// The member's own reported `sigma` becomes the observation's `sigma_meas`, so a member
    /// that was measured badly (or whose repeated sightings disagreed) widens the centroid
    /// rather than pulling it tight.
    pub fn fold_member(&mut self, fields: &BTreeMap<String, Feat>, suspect_fraction: f64) {
        for (name, feat) in fields {
            if !is_cluster_field(name) {
                continue;
            }
            let obs = Feat {
                sigma_meas: feat.sigma,
                spread: 0.0,
                n: 1,
                votes: 1,
                ..feat.clone()
            };
            fold_field(&mut self.fields, name, obs);
        }
        self.folds = self.folds.saturating_add(1);
        if suspect_fraction.is_finite() {
            self.suspect_fraction = self.suspect_fraction.max(suspect_fraction.clamp(0.0, 1.0));
        }
    }

    /// The [`CLUSTER_FIELDS`] this centroid has, as a plain map (what the distance compares).
    pub fn comparable(&self) -> &BTreeMap<String, Feat> {
        &self.fields
    }
}

/// One cluster of unknown emissions: a *type* grouping emitter *instances*.
///
/// Membership itself lives in the append-only `emitter_cluster` link log (an emitter has at most
/// one current cluster), so this row never has to be the second source of truth about who belongs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignatureCluster {
    /// Schema version, [`SIGNATURE_SCHEMA`].
    pub schema: u16,
    /// Cluster id, e.g. `cluster:0199…`.
    pub id: String,
    /// Lifecycle (how much has been seen, never what it is).
    pub state: ClusterState,
    /// The cluster this one was folded into, when `state` is [`ClusterState::Merged`].
    pub merged_into: Option<String>,
    /// The signature minted from it, when promoted.
    pub signature: Option<SignatureRef>,
    /// When it was seeded.
    pub created_at: Timestamp,
    /// When it last changed.
    pub updated_at: Timestamp,
    /// What it measures like.
    pub centroid: ClusterCentroid,
    /// Field-set version of the features folded in ([`super::EMISSION_FEATURES_VERSION`]).
    pub feature_set_version: u32,
}

impl SignatureCluster {
    /// A new `pending` cluster with an empty centroid.
    pub fn new(id: impl Into<String>, t: Timestamp) -> Self {
        Self {
            schema: SIGNATURE_SCHEMA,
            id: id.into(),
            state: ClusterState::Pending,
            merged_into: None,
            signature: None,
            created_at: t,
            updated_at: t,
            centroid: ClusterCentroid::new(),
            feature_set_version: super::EMISSION_FEATURES_VERSION,
        }
    }

    /// Whether this cluster may be *offered* as a hypothesis to mint from: never from an
    /// all-suspect group (the 8-bit, preselector-less front end makes ghosts with real-looking
    /// parameters — C18 card).
    pub fn may_mint(&self) -> bool {
        self.centroid.folds > 0 && self.centroid.suspect_fraction < 1.0
    }

    /// Checks every invariant a stored cluster must hold.
    pub fn validate(&self) -> Result<(), super::InvalidSignature> {
        if self.schema != SIGNATURE_SCHEMA {
            return err(format!("schema {} is not {SIGNATURE_SCHEMA}", self.schema));
        }
        if !is_cluster_id(&self.id) {
            return err(format!("{} is not a cluster id", self.id));
        }
        if (self.state == ClusterState::Merged) != self.merged_into.is_some() {
            return err("a merged cluster names its survivor, and only a merged one does");
        }
        if self.merged_into.as_deref() == Some(self.id.as_str()) {
            return err("a cluster cannot be merged into itself");
        }
        if self.state == ClusterState::Promoted && self.signature.is_none() {
            return err("a promoted cluster names its signature");
        }
        if self.updated_at < self.created_at {
            return err("a cluster cannot change before it exists");
        }
        if !(0.0..=1.0).contains(&self.centroid.suspect_fraction) {
            return err("suspect_fraction must be in [0, 1]");
        }
        for (name, f) in &self.centroid.fields {
            if !is_cluster_field(name) {
                return err(format!("{name} is not a clustering field"));
            }
            if !f.value.is_finite() || !f.sigma.is_finite() || f.sigma < 0.0 {
                return err(format!("field {name}: not a usable measurement"));
            }
        }
        Ok(())
    }
}

fn err(m: impl Into<String>) -> Result<(), super::InvalidSignature> {
    Err(super::InvalidSignature(m.into()))
}

/// One emitter's current cluster membership, appended whenever it changes.
///
/// `cluster` is `None` for "currently in no cluster" — an explicit, append-only record that the
/// clusterer *abstained*, which is a different statement from never having looked.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EmitterClusterLink {
    /// The emitter (an instance).
    pub emitter_id: EmitterId,
    /// The cluster it currently belongs to, or `None`.
    pub cluster_id: Option<String>,
    /// When the membership was decided.
    pub t: Timestamp,
    /// Machine reason code (`joined`, `seeded`, `reassigned`, `ambiguous`, `too_few_fields`,
    /// `conflict`, `too_far`, `repair`).
    pub reason: String,
    /// The tolerance-normalised distance that decided it, when one was computed.
    pub distance: Option<f64>,
}

/// What happened to a cluster. Append-only history beside the mutable current row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClusterEventKind {
    /// Seeded.
    Created,
    /// Became visible.
    Activated,
    /// Another cluster was folded into this one.
    Merge,
    /// Part of this cluster left for another.
    Split,
    /// One emitter moved in or out.
    Reassign,
    /// Promoted to a signature.
    Promoted,
}

impl ClusterEventKind {
    /// Stored spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Activated => "activated",
            Self::Merge => "merge",
            Self::Split => "split",
            Self::Reassign => "reassign",
            Self::Promoted => "promoted",
        }
    }
}

/// One append-only cluster history row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClusterEvent {
    /// The cluster it happened to.
    pub cluster_id: String,
    /// What happened.
    pub kind: ClusterEventKind,
    /// The other cluster involved (a merge survivor, a split destination).
    pub other_cluster_id: Option<String>,
    /// The emitter involved, for a reassignment.
    pub emitter_id: Option<EmitterId>,
    /// When.
    pub t: Timestamp,
    /// The arithmetic behind it (distances, member counts), disclosed like every other verdict.
    #[serde(default)]
    pub detail: serde_json::Value,
}

impl ClusterEvent {
    /// A history row with no other cluster and no emitter.
    pub fn new(cluster_id: impl Into<String>, kind: ClusterEventKind, t: Timestamp) -> Self {
        Self {
            cluster_id: cluster_id.into(),
            kind,
            other_cluster_id: None,
            emitter_id: None,
            t,
            detail: serde_json::Value::Null,
        }
    }
}

/// A fresh cluster id, time-ordered like every other id in the store.
pub fn new_cluster_id() -> String {
    format!("cluster:{}", uuid::Uuid::now_v7())
}

/// Whether `id` is a well-formed cluster id (`cluster:` plus an id-safe tail).
pub fn is_cluster_id(id: &str) -> bool {
    let Some(tail) = id.strip_prefix("cluster:") else {
        return false;
    };
    !tail.is_empty()
        && tail.len() <= 120
        && tail
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(sec: i64) -> Timestamp {
        Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + sec * 1_000_000_000)
    }

    #[test]
    fn a_cluster_id_is_recognisable_and_a_signature_id_is_not_one() {
        let id = new_cluster_id();
        assert!(is_cluster_id(&id), "{id}");
        assert!(!is_cluster_id("pocsag-1200"));
        assert!(!is_cluster_id("cluster:"));
        assert!(!is_cluster_id("cluster:with space"));
        assert!(!is_cluster_id(&format!("cluster:{}", "x".repeat(121))));
    }

    #[test]
    fn frequency_and_propagation_are_not_clustering_fields() {
        // The "type, not instance" rule, as data: two of the same sensor on different channels
        // must be able to land in one cluster, so where they sit is never compared.
        for excluded in [
            field::F_CENTER_HZ,
            field::F_LO_HZ,
            field::F_HI_HZ,
            field::RASTER_OFFSET_HZ,
            field::SNR_DB,
            field::CFO_OFFSET_HZ,
        ] {
            assert!(!is_cluster_field(excluded), "{excluded} is compared");
        }
        for included in [field::SYMBOL_RATE_HZ, field::DEVIATION_HZ, field::PERIOD_S] {
            assert!(is_cluster_field(included), "{included} is not compared");
        }
    }

    #[test]
    fn a_centroid_folds_member_uncertainty_and_never_reports_a_tighter_sigma() {
        let mut c = ClusterCentroid::new();
        let mut member = BTreeMap::new();
        // One member measured loosely (sigma 50 Bd), another tightly but at a different value.
        member.insert(
            field::SYMBOL_RATE_HZ.to_owned(),
            Feat::num(4800.0, 50.0, "c14"),
        );
        // A field outside CLUSTER_FIELDS is dropped, not folded.
        member.insert(
            field::F_CENTER_HZ.to_owned(),
            Feat::num(433.92e6, 100.0, "detector"),
        );
        c.fold_member(&member, 0.0);
        assert_eq!(c.fields.len(), 1, "only clustering fields are folded");
        assert!((c.get(field::SYMBOL_RATE_HZ).unwrap().sigma - 50.0).abs() < 1e-9);

        member.insert(
            field::SYMBOL_RATE_HZ.to_owned(),
            Feat::num(4830.0, 1.0, "c14"),
        );
        c.fold_member(&member, 0.5);
        let f = c.get(field::SYMBOL_RATE_HZ).unwrap();
        // Two members: the sigma is max(spread between them, mean member sigma) — bigger than the
        // tight member's own 1 Bd, and never a standard error that shrank with the second look.
        assert!(f.sigma >= 25.0, "sigma collapsed to {}", f.sigma);
        assert_eq!(f.n, 2);
        assert_eq!(c.folds, 2);
        assert!((c.suspect_fraction - 0.5).abs() < 1e-9);
    }

    #[test]
    fn a_cluster_validates_its_own_lifecycle() {
        let mut c = SignatureCluster::new(new_cluster_id(), t(0));
        c.validate().unwrap();
        assert!(!c.may_mint(), "an empty cluster mints nothing");

        c.state = ClusterState::Merged;
        assert!(c.validate().is_err(), "a merged cluster names its survivor");
        c.merged_into = Some(c.id.clone());
        assert!(c.validate().is_err(), "merged into itself");
        c.merged_into = Some(new_cluster_id());
        c.validate().unwrap();

        let mut c = SignatureCluster::new(new_cluster_id(), t(0));
        c.state = ClusterState::Promoted;
        assert!(
            c.validate().is_err(),
            "a promoted cluster names its signature"
        );

        let mut c = SignatureCluster::new(new_cluster_id(), t(0));
        c.centroid
            .fields
            .insert(field::F_CENTER_HZ.to_owned(), Feat::num(1.0, 0.0, "m"));
        assert!(c.validate().is_err(), "frequency is not a clustering field");
    }

    #[test]
    fn a_cluster_has_nowhere_to_put_an_identity() {
        // The type-level half of "evidence, never identity": serialising the richest cluster we
        // can build mentions no identity, status, family verdict or lifecycle of an emitter.
        let mut c = SignatureCluster::new(new_cluster_id(), t(0));
        let mut fields = BTreeMap::new();
        fields.insert(
            field::SYMBOL_RATE_HZ.to_owned(),
            Feat::num(4800.0, 5.0, "c14"),
        );
        c.centroid.fold_member(&fields, 0.0);
        c.state = ClusterState::Active;
        let json = serde_json::to_string(&c).unwrap();
        for forbidden in ["identity", "known_status", "lifecycle", "callsign"] {
            assert!(!json.contains(forbidden), "{forbidden} leaked: {json}");
        }
    }
}
