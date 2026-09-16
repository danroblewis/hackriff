//! C18 signature contracts (ADR-0016 §5). **Core interface.** Types and storage only.
//!
//! A [`Signature`] is a catalogue entry: "an emission with *these* measured parameters, within
//! *these* tolerances, is consistent with *this* protocol or device type". A [`SignatureMatch`] is
//! the append-only record of comparing one emitter's measured features against the catalogue.
//!
//! # What a match is not
//!
//! **A match is never an identity, a status or a lifecycle change** (ADR-0016 §5, and the
//! exploration-first rule in CLAUDE.md): blind detection and measurement come first, and the
//! catalogue only ever adds a *ranked, reasoned* explanation on top of what was measured. A
//! mismatch — an emission whose parameters sit next to a known protocol's but outside its
//! tolerances — is interesting and is kept as `partial` with the conflicting fields named, never
//! snapped to the nearest entry. Only a CRC-valid decode confirms a signal (docs/15 §1).
//!
//! # Split of work
//!
//! This module holds the shapes, their validation and the SQLite storage (migration 0009). The
//! **matching engine** (per-field z scores, the `full` / `partial` / `none` rule, minting a
//! `recipe-confirmed` signature from a CRC-valid decode) is T-201 in `hk-context`, and
//! **clustering** unknowns is T-202. `EmissionFeatures` — the measured side of the comparison —
//! is T-201's too; until it lands, a match references a features snapshot by opaque id.
//!
//! The thresholds below are the ADR's, a priori, and are *unverified* against real populations:
//! T-206's exit gate measures full-match precision and recall.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::classify::TaxonomyRef;
use crate::ids::EmitterId;
use crate::time::Timestamp;

/// Schema version of [`Signature`] and [`SignatureMatch`].
pub const SIGNATURE_SCHEMA: u16 = 1;

/// Default relative tolerance on a symbol rate (ADR-0016 §5: ±1 %).
pub const DEFAULT_SYMBOL_RATE_TOLERANCE: f64 = 0.01;

/// Default number of required fields that must be present and agree before a match can be `full`
/// (ADR-0016 §5: rate + deviation + sync, say).
pub const DEFAULT_MIN_DISCRIMINATING: u32 = 3;

/// Normalised distance (`z = |Δ| / tolerance`) above which a field **conflicts**: it is evidence
/// against the signature, not merely a miss.
pub const Z_CONFLICT: f64 = 3.0;

/// Smallest score a `full` match may carry.
pub const FULL_MATCH_MIN_SCORE: f64 = 0.8;

/// Smallest score a `partial` match may carry.
pub const PARTIAL_MATCH_MIN_SCORE: f64 = 0.4;

/// Most ranked candidates a [`SignatureMatch`] carries.
pub const MATCH_CANDIDATES_MAX: usize = 5;

/// What kind of thing a signature describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SignatureKind {
    /// A protocol (POCSAG, ADS-B, LoRa …).
    Protocol,
    /// A device type (a make/model of sensor, key fob, telemetry unit …).
    DeviceType,
    /// Interference: a comb, a switching supply, a noise source (AWARE-029/030).
    Rfi,
    /// A radar (PRI, scan period).
    Radar,
    /// Learned on this device from a promoted cluster.
    Learned,
}

impl SignatureKind {
    /// The serde/column string.
    pub const fn as_str(self) -> &'static str {
        match self {
            SignatureKind::Protocol => "protocol",
            SignatureKind::DeviceType => "device-type",
            SignatureKind::Rfi => "rfi",
            SignatureKind::Radar => "radar",
            SignatureKind::Learned => "learned",
        }
    }
}

/// Where a signature came from. It bounds how far it may be trusted: an import is untrusted input
/// and is validated on the way in; `recipe-confirmed` is the only provenance a decode can mint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SignatureProvenance {
    /// Shipped read-only with the device (`signatures/*.signature.json`).
    Builtin,
    /// Written by the user.
    User,
    /// Minted from a recipe whose decode was CRC-valid on ≥ 3 frames from ≥ 2 bursts (T-201).
    RecipeConfirmed,
    /// Imported from an rtl_433 flex spec (untrusted; T-214).
    Rtl433Import,
    /// Promoted from a cluster of unknowns (T-202).
    ClusterPromoted,
}

impl SignatureProvenance {
    /// The serde/column string.
    pub const fn as_str(self) -> &'static str {
        match self {
            SignatureProvenance::Builtin => "builtin",
            SignatureProvenance::User => "user",
            SignatureProvenance::RecipeConfirmed => "recipe-confirmed",
            SignatureProvenance::Rtl433Import => "rtl433-import",
            SignatureProvenance::ClusterPromoted => "cluster-promoted",
        }
    }

    /// Whether this provenance is trusted input (an import never is).
    pub const fn trusted(self) -> bool {
        !matches!(self, SignatureProvenance::Rtl433Import)
    }
}

/// A `<id>@<version>` reference to an immutable signature version.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignatureRef {
    /// Signature id.
    pub id: String,
    /// Version (≥ 1).
    pub version: u32,
}

impl fmt::Display for SignatureRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.id, self.version)
    }
}

impl FromStr for SignatureRef {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (id, version) = s
            .split_once('@')
            .ok_or_else(|| format!("signature ref {s:?} is not <id>@<version>"))?;
        let version: u32 = version
            .parse()
            .ok()
            .filter(|v| *v >= 1 && !version.starts_with('0'))
            .ok_or_else(|| format!("signature ref {s:?} has an invalid version"))?;
        if !is_signature_id(id) {
            return Err(format!("signature ref {s:?} has an invalid id"));
        }
        Ok(Self {
            id: id.to_owned(),
            version,
        })
    }
}

/// Whether `id` is a valid signature id: 1–64 lower-case ASCII letters, digits, `-`, `_` or `.`,
/// starting with a letter or a digit (the recipe-id rule, so ids stay file-name safe).
pub fn is_signature_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && id.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
        })
}

/// A reference to a decoder recipe (ADR-0011) a signature can hand the search.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeRef {
    /// Recipe id.
    pub id: String,
    /// Recipe version.
    pub version: u32,
}

/// What a signature expects of one measured field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum FieldExpect {
    /// One value (with the field's tolerance around it).
    Value {
        /// The expected value.
        value: f64,
    },
    /// A closed interval; a value inside it is `z = 0`.
    Range {
        /// Lower bound, inclusive.
        lo: f64,
        /// Upper bound, inclusive.
        hi: f64,
    },
    /// Any one of these values (a channel raster, a level set).
    Set {
        /// The accepted values (non-empty).
        values: Vec<f64>,
    },
    /// A bit pattern: `0`, `1` or `x` (don't care). Matched in both polarities and every PSK
    /// rotation, within `max_errors` bit errors (T-201 owns that comparison).
    Bits {
        /// The pattern.
        bits: String,
        /// Bit errors tolerated.
        max_errors: u8,
    },
    /// An exact text value (a line code, a CRC polynomial name).
    Text {
        /// The expected text.
        text: String,
    },
}

/// One field of a signature: what is expected, how close counts, and how much it is worth.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldSpec {
    /// The expectation.
    pub expect: FieldExpect,
    /// Relative tolerance for a numeric field (e.g. 0.01 = ±1 %), or `None` for the matcher's
    /// default for that field. Ignored by bit and text expectations.
    pub tolerance: Option<f64>,
    /// Whether the field must be present and agree for a `full` match.
    pub required: bool,
    /// Weight in the score (> 0).
    pub weight: f64,
}

impl FieldSpec {
    /// A required field with `weight` 1 and the default tolerance.
    pub fn required(expect: FieldExpect) -> Self {
        Self {
            expect,
            tolerance: None,
            required: true,
            weight: 1.0,
        }
    }
}

/// An immutable catalogue entry (ADR-0016 §5). A new version is a new row; a version never
/// changes once written.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signature {
    /// Schema version, [`SIGNATURE_SCHEMA`].
    pub schema: u16,
    /// Id, stable across versions.
    pub id: String,
    /// Version (≥ 1).
    pub version: u32,
    /// Human name, e.g. "POCSAG 1200".
    pub name: String,
    /// What kind of thing it describes.
    pub kind: SignatureKind,
    /// Taxonomy the `family`/`class` labels belong to, if it names any.
    pub taxonomy: Option<TaxonomyRef>,
    /// Expected `hk-mod@1` family, if the entry is modulation-specific. **Rank-only compatibility:
    /// an `unknown` classification gates nothing** (ADR-0016 §5 step 1).
    pub family: Option<String>,
    /// Expected within-family class, if narrower still.
    pub class: Option<String>,
    /// Expected measured fields, by field name (`symbol_rate_hz`, `deviation_hz`, `sync_word`,
    /// `preamble`, `period_s`, `duty_cycle`, `comb_spacing_hz` …).
    pub fields: BTreeMap<String, FieldSpec>,
    /// Required fields that must be present before a match can be `full`
    /// ([`DEFAULT_MIN_DISCRIMINATING`]).
    pub min_discriminating: u32,
    /// The decoder recipe this signature hands the search, if any (ADR-0016 §8: templates first).
    pub recipe: Option<RecipeRef>,
    /// Where the entry came from.
    pub provenance: SignatureProvenance,
    /// Who wrote it (a user token fingerprint, a recipe id, an importer id).
    pub author: String,
    /// When this version was created.
    pub created_at: Timestamp,
    /// The version this one supersedes, if any.
    pub supersedes: Option<u32>,
    /// Bands the entry is usually seen in, Hz, as `[lo, hi]` pairs. **Rank-only: a band never
    /// gates a match** — an emission in the "wrong" band is the interesting case.
    #[serde(default)]
    pub bands_hz: Vec<[f64; 2]>,
    /// Free-text notes (never rendered as evidence).
    #[serde(default)]
    pub notes: Option<String>,
}

/// A [`Signature`] or [`SignatureMatch`] broke the contract.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid signature: {0}")]
pub struct InvalidSignature(pub String);

fn bad<T>(msg: impl Into<String>) -> Result<T, InvalidSignature> {
    Err(InvalidSignature(msg.into()))
}

impl Signature {
    /// Its `<id>@<version>` reference.
    pub fn reference(&self) -> SignatureRef {
        SignatureRef {
            id: self.id.clone(),
            version: self.version,
        }
    }

    /// The fields that must be present and agree for a `full` match.
    pub fn required_fields(&self) -> impl Iterator<Item = (&str, &FieldSpec)> {
        self.fields
            .iter()
            .filter(|(_, s)| s.required)
            .map(|(n, s)| (n.as_str(), s))
    }

    /// Checks every invariant a stored signature must hold.
    pub fn validate(&self) -> Result<(), InvalidSignature> {
        if self.schema != SIGNATURE_SCHEMA {
            return bad(format!("schema {} is not {SIGNATURE_SCHEMA}", self.schema));
        }
        if !is_signature_id(&self.id) {
            return bad(format!("id {:?} is not a valid signature id", self.id));
        }
        if self.version < 1 {
            return bad("version must be ≥ 1");
        }
        if let Some(s) = self.supersedes
            && s >= self.version
        {
            return bad("supersedes must name an earlier version");
        }
        if self.name.trim().is_empty() || self.author.trim().is_empty() {
            return bad("name and author are required");
        }
        if let Some(tax) = &self.taxonomy {
            let resolved = tax
                .resolve()
                .ok_or_else(|| InvalidSignature(format!("taxonomy {tax} is not released")))?;
            if let Some(f) = &self.family
                && !resolved.is_family(f)
            {
                return bad(format!("family {f:?} is not in {tax}"));
            }
            if let Some(c) = &self.class {
                let Some(f) = &self.family else {
                    return bad("a class needs its family");
                };
                let in_family = resolved
                    .family(f)
                    .is_some_and(|fd| fd.classes.contains(&c.as_str()));
                if !in_family {
                    return bad(format!("class {c:?} is not in family {f}"));
                }
            }
        } else if self.family.is_some() || self.class.is_some() {
            return bad("a family or class needs a taxonomy reference");
        }
        if self.fields.is_empty() {
            return bad("a signature with no fields matches everything");
        }
        for (name, spec) in &self.fields {
            if name.trim().is_empty() {
                return bad("empty field name");
            }
            if !spec.weight.is_finite() || spec.weight <= 0.0 {
                return bad(format!("field {name}: weight must be > 0"));
            }
            if let Some(t) = spec.tolerance
                && (!t.is_finite() || t <= 0.0)
            {
                return bad(format!("field {name}: tolerance must be > 0"));
            }
            match &spec.expect {
                FieldExpect::Value { value } => {
                    if !value.is_finite() {
                        return bad(format!("field {name}: value must be finite"));
                    }
                }
                FieldExpect::Range { lo, hi } => {
                    if !lo.is_finite() || !hi.is_finite() || lo > hi {
                        return bad(format!("field {name}: range must be finite and ordered"));
                    }
                }
                FieldExpect::Set { values } => {
                    if values.is_empty() || values.iter().any(|v| !v.is_finite()) {
                        return bad(format!("field {name}: set must be non-empty and finite"));
                    }
                }
                FieldExpect::Bits { bits, .. } => {
                    if bits.is_empty() || !bits.bytes().all(|b| matches!(b, b'0' | b'1' | b'x')) {
                        return bad(format!("field {name}: bits must be non-empty 0/1/x"));
                    }
                }
                FieldExpect::Text { text } => {
                    if text.trim().is_empty() {
                        return bad(format!("field {name}: text must be non-empty"));
                    }
                }
            }
        }
        if self.min_discriminating == 0 {
            return bad("min_discriminating must be ≥ 1: nothing matches everything");
        }
        let required = self.required_fields().count() as u32;
        if required < self.min_discriminating {
            return bad(format!(
                "min_discriminating {} exceeds the {required} required fields",
                self.min_discriminating
            ));
        }
        for b in &self.bands_hz {
            if !b[0].is_finite() || !b[1].is_finite() || b[0] >= b[1] || b[0] < 0.0 {
                return bad("a band must be a finite, ordered, non-negative [lo, hi]");
            }
        }
        if let Some(r) = &self.recipe
            && (r.id.trim().is_empty() || r.version < 1)
        {
            return bad("a recipe reference needs an id and a version ≥ 1");
        }
        Ok(())
    }
}

/// The outcome of comparing an emitter's features against the catalogue (ADR-0016 §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MatchOutcome {
    /// Every required field present with `z ≤ 1`, at least `min_discriminating` of them, and
    /// `score ≥ 0.8`.
    Full,
    /// No required field conflicting, but some missing, or a score in [0.4, 0.8).
    Partial,
    /// Otherwise. **Not** evidence that the emission is unknown — only that the catalogue has
    /// nothing to say about it.
    None,
}

impl MatchOutcome {
    /// The serde/column string.
    pub const fn as_str(self) -> &'static str {
        match self {
            MatchOutcome::Full => "full",
            MatchOutcome::Partial => "partial",
            MatchOutcome::None => "none",
        }
    }
}

/// How one field of one candidate agreed with the measurement. Disclosed so the reasoning is
/// visible, never summarised into a single number alone.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldAgreement {
    /// Field name.
    pub field: String,
    /// The measured value, as measured (a number, a bit string, text).
    pub measured: serde_json::Value,
    /// What the signature expected.
    pub expected: serde_json::Value,
    /// Normalised distance `|Δ| / tolerance` (bit distance in bits for a bit field).
    pub z: f64,
    /// Whether it agreed (`z ≤ 1`).
    pub ok: bool,
}

/// One ranked candidate of a [`SignatureMatch`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignatureCandidate {
    /// Which signature version.
    pub signature: SignatureRef,
    /// Its name, so a reader needs no second lookup.
    pub name: String,
    /// Score, 0–1: `Σ w·exp(−z²/2) / Σ w_required`.
    pub score: f64,
    /// Per-field agreement, for the fields that were compared.
    pub agreement: Vec<FieldAgreement>,
    /// Required fields the measurement does not have yet. These are what a MAUTO search must go
    /// and estimate (ADR-0016 §8).
    pub missing: Vec<String>,
    /// Fields that actively disagree (`z > `[`Z_CONFLICT`]).
    pub conflicting: Vec<String>,
    /// The recipe this candidate hands the search, if it has one.
    pub recipe: Option<RecipeRef>,
}

/// The append-only record of one emitter's comparison against the catalogue.
///
/// **It sets no identity, no `known_status` and no lifecycle state.** It adds ranked explanation
/// evidence of kind `signature`, feeds `decoder_available` (ADR-0012) when the top candidate has a
/// recipe, and seeds MAUTO (ADR-0016 §8).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignatureMatch {
    /// Schema version, [`SIGNATURE_SCHEMA`].
    pub schema: u16,
    /// The emitter it is about.
    pub emitter_id: EmitterId,
    /// When it was computed.
    pub t: Timestamp,
    /// The outcome.
    pub outcome: MatchOutcome,
    /// The `EmissionFeatures` snapshot it was computed from (opaque until T-201).
    pub features_ref: Option<String>,
    /// Revision of the signature store, so a match can be re-derived exactly.
    pub signatures_rev: u64,
    /// Ranked candidates, best first, at most [`MATCH_CANDIDATES_MAX`]. Empty on
    /// [`MatchOutcome::None`].
    pub candidates: Vec<SignatureCandidate>,
    /// Machine reason codes (`too_few_fields`, `all_suspect`, `no_candidate_in_family` …).
    #[serde(default)]
    pub reasons: Vec<String>,
}

impl SignatureMatch {
    /// The best candidate, if any.
    pub fn top(&self) -> Option<&SignatureCandidate> {
        self.candidates.first()
    }

    /// Checks the structural invariants (the matching *rule* is T-201's; what is checked here is
    /// what any stored row must hold whoever wrote it).
    pub fn validate(&self) -> Result<(), InvalidSignature> {
        if self.schema != SIGNATURE_SCHEMA {
            return bad(format!("schema {} is not {SIGNATURE_SCHEMA}", self.schema));
        }
        if self.candidates.len() > MATCH_CANDIDATES_MAX {
            return bad(format!(
                "{} candidates exceeds {MATCH_CANDIDATES_MAX}",
                self.candidates.len()
            ));
        }
        if (self.outcome == MatchOutcome::None) != self.candidates.is_empty() {
            return bad("a none outcome carries no candidates, and any other carries at least one");
        }
        let mut previous = f64::INFINITY;
        for c in &self.candidates {
            if !c.score.is_finite() || !(0.0..=1.0).contains(&c.score) {
                return bad(format!("score {} is not in [0, 1]", c.score));
            }
            if c.score > previous {
                return bad("candidates must be ranked, best first");
            }
            previous = c.score;
            if !is_signature_id(&c.signature.id) || c.signature.version < 1 {
                return bad(format!("candidate {} is not a signature ref", c.signature));
            }
            if c.name.trim().is_empty() {
                return bad("a candidate carries its signature's name");
            }
            for a in &c.agreement {
                if a.field.trim().is_empty() {
                    return bad("empty agreement field name");
                }
                if !a.z.is_finite() || a.z < 0.0 {
                    return bad(format!("field {}: z {} is not a distance", a.field, a.z));
                }
                if a.ok != (a.z <= 1.0) {
                    return bad(format!("field {}: ok disagrees with z", a.field));
                }
                if c.conflicting.contains(&a.field) != (a.z > Z_CONFLICT) {
                    return bad(format!(
                        "field {}: conflicting disagrees with z > {Z_CONFLICT}",
                        a.field
                    ));
                }
            }
            if c.missing.iter().any(|m| m.trim().is_empty()) {
                return bad("empty missing-field name");
            }
        }
        if let Some(top) = self.top() {
            let floor = match self.outcome {
                MatchOutcome::Full => FULL_MATCH_MIN_SCORE,
                MatchOutcome::Partial => PARTIAL_MATCH_MIN_SCORE,
                MatchOutcome::None => 0.0,
            };
            if top.score + 1e-12 < floor {
                return bad(format!(
                    "a {} match needs a top score of at least {floor}, not {}",
                    self.outcome.as_str(),
                    top.score
                ));
            }
            if self.outcome == MatchOutcome::Full
                && (!top.missing.is_empty() || !top.conflicting.is_empty())
            {
                return bad("a full match has no missing or conflicting required fields");
            }
        }
        if self.reasons.iter().any(|r| r.trim().is_empty()) {
            return bad("empty reason code");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signature() -> Signature {
        let mut fields = BTreeMap::new();
        fields.insert(
            "symbol_rate_hz".to_owned(),
            FieldSpec {
                expect: FieldExpect::Value { value: 1200.0 },
                tolerance: Some(DEFAULT_SYMBOL_RATE_TOLERANCE),
                required: true,
                weight: 2.0,
            },
        );
        fields.insert(
            "deviation_hz".to_owned(),
            FieldSpec::required(FieldExpect::Range {
                lo: 4000.0,
                hi: 4800.0,
            }),
        );
        fields.insert(
            "sync_word".to_owned(),
            FieldSpec::required(FieldExpect::Bits {
                bits: "01111100110100100001010111011000".into(),
                max_errors: 2,
            }),
        );
        fields.insert(
            "line_code".to_owned(),
            FieldSpec {
                expect: FieldExpect::Text { text: "nrz".into() },
                tolerance: None,
                required: false,
                weight: 0.5,
            },
        );
        Signature {
            schema: SIGNATURE_SCHEMA,
            id: "pocsag-1200".into(),
            version: 1,
            name: "POCSAG 1200".into(),
            kind: SignatureKind::Protocol,
            taxonomy: Some(TaxonomyRef::current()),
            family: Some("fsk".into()),
            class: Some("2fsk".into()),
            fields,
            min_discriminating: DEFAULT_MIN_DISCRIMINATING,
            recipe: Some(RecipeRef {
                id: "pocsag".into(),
                version: 3,
            }),
            provenance: SignatureProvenance::Builtin,
            author: "hackriff".into(),
            created_at: Timestamp::from_unix_nanos(1_789_300_820_000_000_000),
            supersedes: None,
            bands_hz: vec![[137e6, 175e6]],
            notes: None,
        }
    }

    fn a_match(outcome: MatchOutcome) -> SignatureMatch {
        let candidate = SignatureCandidate {
            signature: signature().reference(),
            name: "POCSAG 1200".into(),
            score: 0.91,
            agreement: vec![FieldAgreement {
                field: "symbol_rate_hz".into(),
                measured: serde_json::json!(1201.0),
                expected: serde_json::json!(1200.0),
                z: 0.08,
                ok: true,
            }],
            missing: Vec::new(),
            conflicting: Vec::new(),
            recipe: Some(RecipeRef {
                id: "pocsag".into(),
                version: 3,
            }),
        };
        SignatureMatch {
            schema: SIGNATURE_SCHEMA,
            emitter_id: EmitterId::new(),
            t: Timestamp::from_unix_nanos(1_789_300_830_000_000_000),
            outcome,
            features_ref: Some("features:0199".into()),
            signatures_rev: 7,
            candidates: match outcome {
                MatchOutcome::None => Vec::new(),
                _ => vec![candidate],
            },
            reasons: Vec::new(),
        }
    }

    #[test]
    fn a_signature_validates_and_round_trips_through_serde() {
        let s = signature();
        s.validate().unwrap();
        let json = serde_json::to_string(&s).unwrap();
        let back: Signature = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        assert_eq!(s.reference().to_string(), "pocsag-1200@1");
        assert_eq!(
            "pocsag-1200@1".parse::<SignatureRef>().unwrap(),
            s.reference()
        );
        assert_eq!(s.required_fields().count(), 3);
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["kind"], "protocol");
        assert_eq!(v["provenance"], "builtin");
        assert_eq!(v["fields"]["deviation_hz"]["expect"]["kind"], "range");
    }

    #[test]
    fn signature_validation_catches_each_broken_invariant() {
        type Break = fn(&mut Signature);
        let cases: &[(&str, Break)] = &[
            ("schema", |s| s.schema = 2),
            ("id", |s| s.id = "Not An Id".into()),
            ("name", |s| s.name = " ".into()),
            ("no fields", |s| s.fields.clear()),
            ("class outside family", |s| s.class = Some("bpsk".into())),
            ("family outside taxonomy", |s| {
                s.family = Some("adsb".into());
                s.class = None;
            }),
            ("family without taxonomy", |s| s.taxonomy = None),
            ("min_discriminating 0", |s| s.min_discriminating = 0),
            ("min_discriminating above required", |s| {
                s.min_discriminating = 9
            }),
            ("weight", |s| {
                s.fields.get_mut("deviation_hz").unwrap().weight = 0.0
            }),
            ("tolerance", |s| {
                s.fields.get_mut("symbol_rate_hz").unwrap().tolerance = Some(0.0)
            }),
            ("bits", |s| {
                s.fields.get_mut("sync_word").unwrap().expect = FieldExpect::Bits {
                    bits: "0102".into(),
                    max_errors: 0,
                }
            }),
            ("range order", |s| {
                s.fields.get_mut("deviation_hz").unwrap().expect =
                    FieldExpect::Range { lo: 5.0, hi: 1.0 }
            }),
            ("band", |s| s.bands_hz = vec![[175e6, 137e6]]),
            ("supersedes", |s| s.supersedes = Some(1)),
            ("recipe", |s| {
                s.recipe = Some(RecipeRef {
                    id: "x".into(),
                    version: 0,
                })
            }),
        ];
        for (name, f) in cases {
            let mut s = signature();
            f(&mut s);
            assert!(s.validate().is_err(), "{name} should be refused");
        }
        for bad in [
            "pocsag-1200",
            "pocsag-1200@0",
            "pocsag-1200@01",
            "@1",
            "A@1",
        ] {
            assert!(bad.parse::<SignatureRef>().is_err(), "{bad}");
        }
    }

    #[test]
    fn a_match_validates_round_trips_and_is_never_an_identity() {
        for outcome in [
            MatchOutcome::Full,
            MatchOutcome::Partial,
            MatchOutcome::None,
        ] {
            let m = a_match(outcome);
            m.validate().unwrap();
            let json = serde_json::to_string(&m).unwrap();
            let back: SignatureMatch = serde_json::from_str(&json).unwrap();
            assert_eq!(back, m);
        }
        // The type carries no identity, status or lifecycle field at all: the guarantee is
        // structural, not a runtime check.
        let v = serde_json::to_value(a_match(MatchOutcome::Full)).unwrap();
        for forbidden in ["identity", "known_status", "lifecycle", "state"] {
            assert!(v.get(forbidden).is_none(), "{forbidden}");
        }
        assert_eq!(a_match(MatchOutcome::None).top(), None);
    }

    #[test]
    fn match_validation_catches_each_broken_invariant() {
        type Break = fn(&mut SignatureMatch);
        let cases: &[(&str, Break)] = &[
            ("schema", |m| m.schema = 0),
            ("none with candidates", |m| m.outcome = MatchOutcome::None),
            ("score range", |m| m.candidates[0].score = 1.5),
            ("full below its floor", |m| m.candidates[0].score = 0.5),
            ("unranked", |m| {
                let mut second = m.candidates[0].clone();
                second.score = 0.99;
                m.candidates.push(second);
            }),
            ("full with a missing field", |m| {
                m.candidates[0].missing = vec!["sync_word".into()]
            }),
            ("ok disagrees with z", |m| {
                m.candidates[0].agreement[0].z = 2.0
            }),
            ("conflict not listed", |m| {
                m.candidates[0].agreement[0].z = 4.0;
                m.candidates[0].agreement[0].ok = false;
            }),
            ("empty reason", |m| m.reasons = vec!["".into()]),
            ("too many candidates", |m| {
                let c = m.candidates[0].clone();
                m.candidates = vec![c; MATCH_CANDIDATES_MAX + 1];
            }),
        ];
        for (name, f) in cases {
            let mut m = a_match(MatchOutcome::Full);
            f(&mut m);
            assert!(m.validate().is_err(), "{name} should be refused");
        }
    }

    #[test]
    fn an_import_is_untrusted_and_every_other_provenance_is_not() {
        assert!(!SignatureProvenance::Rtl433Import.trusted());
        for p in [
            SignatureProvenance::Builtin,
            SignatureProvenance::User,
            SignatureProvenance::RecipeConfirmed,
            SignatureProvenance::ClusterPromoted,
        ] {
            assert!(p.trusted(), "{p:?}");
        }
    }
}
