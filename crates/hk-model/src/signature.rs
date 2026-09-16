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
use crate::cluster::Fingerprint;
use crate::ids::EmitterId;
use crate::time::Timestamp;

/// Schema version of [`Signature`] and [`SignatureMatch`].
pub const SIGNATURE_SCHEMA: u16 = 1;

// T-202 (ADR-0016 §5): clusters of unknown emissions — "the same thing I saw before".
pub mod cluster;

/// Default relative tolerance on a symbol rate (ADR-0016 §5: ±1 %).
pub const DEFAULT_SYMBOL_RATE_TOLERANCE: f64 = 0.01;

/// Default number of required fields that must be present and agree before a match can be `full`
/// (ADR-0016 §5: rate + deviation + sync, say).
pub const DEFAULT_MIN_DISCRIMINATING: u32 = 3;

/// Normalised distance (`z = |Δ| / tolerance`) above which a field **conflicts**: it is evidence
/// against the signature, not merely a miss.
pub const Z_CONFLICT: f64 = 3.0;

/// Default **relative** tolerance for a numeric field whose [`FieldSpec`] names none.
///
/// One table, used by both comparisons that exist: the catalogue matcher (T-201) and the
/// clustering distance (T-202). They mirror `crate::cluster::Tolerances::default` where the two
/// overlap, so matching, clustering and entity resolution never disagree about how close counts
/// as close.
pub fn default_tolerance(name: &str) -> f64 {
    match name {
        field::SYMBOL_RATE_HZ => DEFAULT_SYMBOL_RATE_TOLERANCE,
        field::DEVIATION_HZ => 0.10,
        field::PERIOD_S | field::TDMA_PERIOD_S | field::PRI_S | field::SCAN_PERIOD_S => 0.05,
        field::DUTY_CYCLE | field::BURST_LENGTH_S => 0.25,
        field::OBW_HZ => 0.20,
        field::F_CENTER_HZ | field::HOP_RASTER_HZ | field::COMB_SPACING_HZ => 0.02,
        _ => 0.10,
    }
}

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
    /// Normalised distance `|Δ| / tolerance`, so `z ≤ 1` is agreement whatever the field's units.
    /// A bit field normalises its Hamming distance by the pattern's own `max_errors`
    /// (`z = errors / max(max_errors, 1)`), which is what keeps [`FieldAgreement::ok`] and
    /// [`Z_CONFLICT`] meaning the same thing for bits as for numbers (T-201; T-218's "distance in
    /// bits" wording predated the matcher and would have broken the `ok == (z <= 1)` invariant
    /// this type validates).
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
                // A partial that is *missing* required fields is expected to score low: the score
                // divides by every required field's weight (ADR-0016 §5), so a measurement with
                // one or two of them lands here by construction. Holding it to the 0.4 floor
                // would force it to `none` and throw the ranked candidates away — the opposite of
                // the C18 rule that too few fields yields a partial with candidates and never an
                // identity (ADR-0016 §5, C18 card §Unknown handling). The floor still applies to a
                // partial whose required fields were all measured and merely agree poorly.
                MatchOutcome::Partial if !top.missing.is_empty() => 0.0,
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

// ---------------------------------------------------------------------------------------------
// EmissionFeatures (T-201): the measured side of the comparison.
// ---------------------------------------------------------------------------------------------

/// Version of the [`EmissionFeatures`] field set (ADR-0016 §5).
pub const EMISSION_FEATURES_VERSION: u32 = 1;

/// Running-mean weight cap when folding observations, matching [`crate::cluster::Fingerprint`]:
/// past the cap the aggregate follows a slowly drifting emission instead of averaging it away.
pub const FEATURE_FOLD_WEIGHT_CAP: u32 = 16;

/// Canonical [`EmissionFeatures`] field names (C18 card §Methods). Producers and the matcher key
/// on these strings, so they are spelled once here rather than in each caller.
pub mod field {
    /// Lower edge of the occupied band, Hz.
    pub const F_LO_HZ: &str = "f_lo_hz";
    /// Upper edge of the occupied band, Hz.
    pub const F_HI_HZ: &str = "f_hi_hz";
    /// Centre frequency, Hz.
    pub const F_CENTER_HZ: &str = "f_center_hz";
    /// Offset from the nearest channel raster point, Hz.
    pub const RASTER_OFFSET_HZ: &str = "raster_offset_hz";
    /// Occupied bandwidth (OBW99), Hz.
    pub const OBW_HZ: &str = "obw_hz";
    /// `hk-mod@1` family label.
    pub const FAMILY: &str = "family";
    /// Within-family class label.
    pub const CLASS: &str = "class";
    /// Symbol rate, Bd.
    pub const SYMBOL_RATE_HZ: &str = "symbol_rate_hz";
    /// FSK deviation, Hz.
    pub const DEVIATION_HZ: &str = "deviation_hz";
    /// Modulation levels / constellation order.
    pub const LEVELS: &str = "levels";
    /// Line code (`nrz`, `manchester`, …).
    pub const LINE_CODE: &str = "line_code";
    /// Preamble bits.
    pub const PREAMBLE: &str = "preamble";
    /// Sync word bits.
    pub const SYNC_WORD: &str = "sync_word";
    /// Packet length, bits.
    pub const PACKET_LENGTH_BITS: &str = "packet_length_bits";
    /// CRC polynomial name or value.
    pub const CRC_POLY: &str = "crc_poly";
    /// Burst repetition period, s.
    pub const PERIOD_S: &str = "period_s";
    /// Duty cycle, 0–1.
    pub const DUTY_CYCLE: &str = "duty_cycle";
    /// Median burst length, s.
    pub const BURST_LENGTH_S: &str = "burst_length_s";
    /// TDMA frame period, s.
    pub const TDMA_PERIOD_S: &str = "tdma_period_s";
    /// Hop raster, Hz.
    pub const HOP_RASTER_HZ: &str = "hop_raster_hz";
    /// Hop-set size (channels seen).
    pub const HOP_COUNT: &str = "hop_count";
    /// Spectral flatness, 0–1.
    pub const FLATNESS: &str = "flatness";
    /// Spectral symmetry, −1..1.
    pub const SYMMETRY: &str = "symmetry";
    /// Carrier-line prominence, dB.
    pub const CARRIER_LINE_DB: &str = "carrier_line_db";
    /// RFI comb spacing, Hz (AWARE-029/030).
    pub const COMB_SPACING_HZ: &str = "comb_spacing_hz";
    /// RFI comb tooth count.
    pub const COMB_COUNT: &str = "comb_count";
    /// Radar pulse repetition interval, s.
    pub const PRI_S: &str = "pri_s";
    /// Radar scan period, s.
    pub const SCAN_PERIOD_S: &str = "scan_period_s";
    /// Carrier frequency offset, Hz (oscillator, AWARE-051).
    pub const CFO_OFFSET_HZ: &str = "cfo_offset_hz";
    /// In-band SNR, dB.
    pub const SNR_DB: &str = "snr_db";
}

/// One measured value: a number, a bit pattern or a label.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum FeatValue {
    /// A numeric measurement in the field's units.
    Num {
        /// The value.
        value: f64,
    },
    /// A bit pattern (`0`/`1`), e.g. a sync word or preamble.
    Bits {
        /// The bits.
        bits: String,
    },
    /// A label (a line code, a CRC polynomial name, a family).
    Text {
        /// The text.
        text: String,
    },
}

impl FeatValue {
    /// The number, when this is a numeric measurement.
    pub fn num(&self) -> Option<f64> {
        match self {
            FeatValue::Num { value } => Some(*value),
            _ => None,
        }
    }

    /// The bits, when this is a bit pattern.
    pub fn bits(&self) -> Option<&str> {
        match self {
            FeatValue::Bits { bits } => Some(bits),
            _ => None,
        }
    }

    /// The text, when this is a label.
    pub fn text(&self) -> Option<&str> {
        match self {
            FeatValue::Text { text } => Some(text),
            _ => None,
        }
    }

    /// Whether two values are the same measurement (exact for bits and text).
    fn same(&self, other: &FeatValue) -> bool {
        match (self, other) {
            (FeatValue::Num { value: a }, FeatValue::Num { value: b }) => a == b,
            (FeatValue::Bits { bits: a }, FeatValue::Bits { bits: b }) => a == b,
            (FeatValue::Text { text: a }, FeatValue::Text { text: b }) => a == b,
            _ => false,
        }
    }

    /// Whether the two are the same *kind* of measurement (a fold never mixes kinds).
    fn same_kind(&self, other: &FeatValue) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }

    fn is_finite(&self) -> bool {
        match self {
            FeatValue::Num { value } => value.is_finite(),
            FeatValue::Bits { bits } => {
                !bits.is_empty() && bits.bytes().all(|b| matches!(b, b'0' | b'1'))
            }
            FeatValue::Text { text } => !text.trim().is_empty(),
        }
    }
}

/// One aggregated field of an [`EmissionFeatures`]: what was measured, how well, and from how
/// many observations.
///
/// # How the uncertainty is carried
///
/// [`Feat::sigma`] is deliberately **not** a standard error of the mean. It is
/// `max(spread, sigma_meas)`: the aggregate is never claimed to be better known than either
///
/// - `spread` — how much the folded observations actually disagreed with each other, or
/// - `sigma_meas` — how well any single one of them was measured.
///
/// Shrinking as `1/√n` would be wrong here and actively harmful. A drifting oscillator, a
/// temperature-dependent FSK deviation or a symbol rate re-estimated from short bursts does not
/// become more certain with more looks, and an over-tight `sigma` is exactly what turns a real
/// emission into a **conflict** against the catalogue entry it genuinely matches — the failure
/// mode the exploration-first rule cares most about (a mismatch must be interesting, not
/// manufactured). Matching widens each tolerance by this `sigma`, so a poorly measured field can
/// never conflict on its own.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Feat {
    /// The aggregated value.
    pub value: FeatValue,
    /// Reported uncertainty: `max(spread, sigma_meas)`, in the value's units (0 for a label).
    pub sigma: f64,
    /// Running sample standard deviation across the folded observations (0 while `n` is 1).
    pub spread: f64,
    /// Running mean of the per-observation measurement sigmas.
    pub sigma_meas: f64,
    /// A **lower bound** on the fraction of folded observations agreeing with `value`. Numeric
    /// fields report 1.0 and carry their disagreement in `spread` instead; label and bit fields
    /// vote (see [`EmissionFeatures::fold`]).
    pub agreement: f64,
    /// Observations folded in.
    pub n: u32,
    /// How it was measured (`c14-cyclic`, `chain-label`, `track-timing`, …).
    pub method: String,
    /// Plurality-vote counter for label and bit fields (Boyer–Moore); unused for numbers.
    #[serde(default)]
    pub votes: u32,
}

impl Feat {
    /// A first observation of a numeric field measured to `sigma` (0 if unknown).
    pub fn num(value: f64, sigma: f64, method: impl Into<String>) -> Self {
        Self::of(FeatValue::Num { value }, sigma, method)
    }

    /// A first observation of a bit pattern.
    pub fn bits(bits: impl Into<String>, method: impl Into<String>) -> Self {
        Self::of(FeatValue::Bits { bits: bits.into() }, 0.0, method)
    }

    /// A first observation of a label.
    pub fn text(text: impl Into<String>, method: impl Into<String>) -> Self {
        Self::of(FeatValue::Text { text: text.into() }, 0.0, method)
    }

    fn of(value: FeatValue, sigma: f64, method: impl Into<String>) -> Self {
        let sigma = if sigma.is_finite() && sigma > 0.0 {
            sigma
        } else {
            0.0
        };
        Self {
            value,
            sigma,
            spread: 0.0,
            sigma_meas: sigma,
            agreement: 1.0,
            n: 1,
            method: method.into(),
            votes: 1,
        }
    }

    /// The numeric value, when this field is numeric.
    pub fn as_num(&self) -> Option<f64> {
        self.value.num()
    }

    fn recompute_sigma(&mut self) {
        self.sigma = self.spread.max(self.sigma_meas);
    }
}

/// Folds one observation of one field into a map of aggregates.
///
/// This is the arithmetic behind [`EmissionFeatures::fold`], written once so that the cluster
/// centroids of [`cluster::ClusterCentroid`] (T-202) fold members exactly the way an emitter folds
/// sightings — one uncertainty rule, at both levels:
///
/// - **Numeric.** A capped running mean ([`FEATURE_FOLD_WEIGHT_CAP`]) with a Welford update for
///   `spread`, and a running mean of the observations' own sigmas. The reported [`Feat::sigma`] is
///   the larger of the two (see [`Feat`] for why it never shrinks as `1/√n`).
/// - **Label / bits.** A streaming plurality vote (Boyer–Moore): a matching observation raises the
///   counter, a differing one lowers it, and the candidate is replaced when the counter reaches
///   zero. `agreement` is `votes / n`, a lower bound on the true share — so a sync word seen once
///   among many disagreeing reads is visibly weak evidence rather than silently authoritative.
/// - **Kind change.** An observation of a different kind (a number where a label stands) replaces
///   the field and restarts its statistics: the two are not averageable.
pub fn fold_field(fields: &mut BTreeMap<String, Feat>, name: &str, obs: Feat) {
    match fields.get_mut(name) {
        None => {
            fields.insert(name.to_owned(), obs);
        }
        Some(cur) if !cur.value.same_kind(&obs.value) => {
            *cur = obs;
        }
        Some(cur) => {
            let n = cur.n.saturating_add(1);
            let w = f64::from(cur.n.clamp(1, FEATURE_FOLD_WEIGHT_CAP));
            match (&cur.value, &obs.value) {
                (FeatValue::Num { value: old }, FeatValue::Num { value: new }) => {
                    let (old, new) = (*old, *new);
                    let mean = (old * w + new) / (w + 1.0);
                    // Welford in its weighted form: the spread follows the observations even
                    // once the mean's weight is capped.
                    let var =
                        (cur.spread * cur.spread * w + (new - old) * (new - mean)) / (w + 1.0);
                    cur.value = FeatValue::Num { value: mean };
                    cur.spread = if var.is_finite() && var > 0.0 {
                        var.sqrt()
                    } else {
                        0.0
                    };
                    cur.sigma_meas = (cur.sigma_meas * w + obs.sigma_meas) / (w + 1.0);
                    cur.agreement = 1.0;
                }
                _ => {
                    // Streaming plurality vote over labels and bit patterns.
                    if cur.value.same(&obs.value) {
                        cur.votes = cur.votes.saturating_add(1);
                    } else if cur.votes <= 1 {
                        cur.value = obs.value.clone();
                        cur.method = obs.method.clone();
                        cur.votes = 1;
                    } else {
                        cur.votes -= 1;
                    }
                    cur.agreement = f64::from(cur.votes) / f64::from(n.max(1));
                }
            }
            cur.n = n;
            cur.recompute_sigma();
        }
    }
}

/// The aggregated, uncertainty-carrying measurement of one emitter (ADR-0016 §5), from which a
/// [`SignatureMatch`] is computed.
///
/// It is an **append-only snapshot**: a new row is written when a field moves by more than its
/// sigma or enough new observations have been folded in. [`crate::cluster::Fingerprint`] v1 is a
/// projection of it ([`EmissionFeatures::fingerprint`]), so entity resolution is unchanged.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmissionFeatures {
    /// Schema version, [`SIGNATURE_SCHEMA`].
    pub schema: u16,
    /// Field-set version, [`EMISSION_FEATURES_VERSION`].
    pub version: u32,
    /// Snapshot id; this is what a [`SignatureMatch::features_ref`] names.
    pub id: String,
    /// The emitter it describes.
    pub emitter_id: EmitterId,
    /// When the snapshot was taken.
    pub t: Timestamp,
    /// Aggregated fields, by [`field`] name.
    pub fields: BTreeMap<String, Feat>,
    /// Observations folded into this snapshot.
    pub observations: u32,
    /// Fraction of those observations carrying a suspect flag (clipped, IMD, image, spur). A
    /// signature is never minted from an all-suspect emitter (C18 card, ADR-0016 §5).
    pub suspect_fraction: f64,
    /// Machine reason codes for what could not be measured.
    #[serde(default)]
    pub reasons: Vec<String>,
}

impl EmissionFeatures {
    /// An empty snapshot for `emitter_id`.
    pub fn new(id: impl Into<String>, emitter_id: EmitterId, t: Timestamp) -> Self {
        Self {
            schema: SIGNATURE_SCHEMA,
            version: EMISSION_FEATURES_VERSION,
            id: id.into(),
            emitter_id,
            t,
            fields: BTreeMap::new(),
            observations: 0,
            suspect_fraction: 0.0,
            reasons: Vec::new(),
        }
    }

    /// A field, if it was measured.
    pub fn get(&self, name: &str) -> Option<&Feat> {
        self.fields.get(name)
    }

    /// A numeric field's value, if it was measured as a number.
    pub fn num(&self, name: &str) -> Option<f64> {
        self.fields.get(name)?.value.num()
    }

    /// How many fields were measured.
    pub fn present(&self) -> usize {
        self.fields.len()
    }

    /// Folds one observation of one field into the aggregate.
    ///
    /// - **Numeric.** A capped running mean ([`FEATURE_FOLD_WEIGHT_CAP`]) with a Welford update
    ///   for `spread`, and a running mean of the observations' own sigmas. The reported
    ///   [`Feat::sigma`] is the larger of the two (see [`Feat`] for why it never shrinks as
    ///   `1/√n`).
    /// - **Label / bits.** A streaming plurality vote (Boyer–Moore): a matching observation
    ///   raises the counter, a differing one lowers it, and the candidate is replaced when the
    ///   counter reaches zero. `agreement` is `votes / n`, a lower bound on the true share — so a
    ///   sync word seen once among many disagreeing reads is visibly weak evidence rather than
    ///   silently authoritative.
    /// - **Kind change.** An observation of a different kind (a number where a label stands)
    ///   replaces the field and restarts its statistics: the two are not averageable.
    pub fn fold(&mut self, name: &str, obs: Feat) {
        fold_field(&mut self.fields, name, obs);
    }

    /// Folds a whole observation (several fields measured at once) and counts it.
    pub fn observe(&mut self, fields: impl IntoIterator<Item = (String, Feat)>, suspect: bool) {
        let before = f64::from(self.observations);
        for (name, feat) in fields {
            self.fold(&name, feat);
        }
        self.observations = self.observations.saturating_add(1);
        let suspect_count = self.suspect_fraction * before + f64::from(u8::from(suspect));
        self.suspect_fraction = suspect_count / f64::from(self.observations.max(1));
    }

    /// The [`crate::cluster::Fingerprint`] v1 projection of this snapshot, so entity resolution
    /// keeps working off the same measurement.
    pub fn fingerprint(&self) -> Fingerprint {
        let f_center = self.num(field::F_CENTER_HZ).unwrap_or(0.0);
        let bandwidth = self.num(field::OBW_HZ).unwrap_or(0.0);
        Fingerprint {
            family: self
                .fields
                .get(field::FAMILY)
                .and_then(|f| f.value.text())
                .map(str::to_owned),
            symbol_rate_hz: self.num(field::SYMBOL_RATE_HZ),
            deviation_hz: self.num(field::DEVIATION_HZ),
            period_s: self.num(field::PERIOD_S),
            duty_cycle: self.num(field::DUTY_CYCLE),
            burst_length_s: self.num(field::BURST_LENGTH_S),
            hop_raster_hz: self.num(field::HOP_RASTER_HZ),
            observations: u64::from(self.observations.max(1)),
            ..Fingerprint::new(f_center, bandwidth)
        }
    }

    /// Checks every invariant a stored snapshot must hold.
    pub fn validate(&self) -> Result<(), InvalidSignature> {
        if self.schema != SIGNATURE_SCHEMA {
            return bad(format!("schema {} is not {SIGNATURE_SCHEMA}", self.schema));
        }
        if self.version != EMISSION_FEATURES_VERSION {
            return bad(format!(
                "features version {} is not {EMISSION_FEATURES_VERSION}",
                self.version
            ));
        }
        if self.id.trim().is_empty() {
            return bad("a features snapshot needs an id");
        }
        if !(0.0..=1.0).contains(&self.suspect_fraction) || !self.suspect_fraction.is_finite() {
            return bad("suspect_fraction must be in [0, 1]");
        }
        for (name, f) in &self.fields {
            if name.trim().is_empty() {
                return bad("empty feature name");
            }
            if !f.value.is_finite() {
                return bad(format!("field {name}: value is not a usable measurement"));
            }
            for (what, v) in [
                ("sigma", f.sigma),
                ("spread", f.spread),
                ("sigma_meas", f.sigma_meas),
            ] {
                if !v.is_finite() || v < 0.0 {
                    return bad(format!("field {name}: {what} must be finite and ≥ 0"));
                }
            }
            if (f.sigma - f.spread.max(f.sigma_meas)).abs() > 1e-9 {
                return bad(format!(
                    "field {name}: sigma must be max(spread, sigma_meas)"
                ));
            }
            if !f.agreement.is_finite() || !(0.0..=1.0).contains(&f.agreement) {
                return bad(format!("field {name}: agreement must be in [0, 1]"));
            }
            if f.n == 0 {
                return bad(format!("field {name}: a measured field has n ≥ 1"));
            }
            if f.method.trim().is_empty() {
                return bad(format!("field {name}: a measurement names its method"));
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

    fn features() -> EmissionFeatures {
        EmissionFeatures::new(
            "features:test",
            EmitterId::new(),
            Timestamp::from_unix_nanos(1_789_300_800_000_000_000),
        )
    }

    /// The aggregate never claims to be better known than the observations disagree: repeated
    /// looks at a drifting rate widen `sigma`, they do not shrink it as `1/√n`.
    #[test]
    fn folding_numeric_observations_keeps_the_spread_they_actually_show() {
        let mut f = features();
        for rate in [1200.0, 1206.0, 1194.0, 1203.0] {
            f.fold(field::SYMBOL_RATE_HZ, Feat::num(rate, 0.5, "c14-cyclic"));
        }
        let rate = f.get(field::SYMBOL_RATE_HZ).unwrap();
        assert_eq!(rate.n, 4);
        let mean = rate.value.num().unwrap();
        assert!((mean - 1200.0).abs() < 4.0, "mean {mean}");
        // The observations disagree by several Bd, so the reported sigma is the spread, not the
        // half-Bd each measurement claimed for itself, and not a shrinking standard error.
        assert!(rate.spread > 1.0, "spread {}", rate.spread);
        assert_eq!(rate.sigma, rate.spread.max(rate.sigma_meas));
        assert!(rate.sigma > rate.sigma_meas, "{rate:?}");
        assert_eq!(rate.agreement, 1.0, "numbers carry disagreement as spread");

        // A field measured identically every time keeps its measurement sigma and no spread.
        let mut steady = features();
        for _ in 0..5 {
            steady.fold(field::DEVIATION_HZ, Feat::num(4500.0, 20.0, "chain"));
        }
        let dev = steady.get(field::DEVIATION_HZ).unwrap();
        assert_eq!(dev.spread, 0.0);
        assert!((dev.sigma - 20.0).abs() < 1e-9, "{dev:?}");
        f.validate().unwrap();
    }

    /// Labels and bit patterns vote; a value seen once among many disagreeing reads is visibly
    /// weak evidence rather than silently authoritative.
    #[test]
    fn folding_labels_and_bits_is_a_plurality_vote_with_an_agreement_bound() {
        let mut f = features();
        for label in ["fsk", "fsk", "ook", "fsk"] {
            f.fold(field::FAMILY, Feat::text(label, "classifier"));
        }
        let fam = f.get(field::FAMILY).unwrap();
        assert_eq!(fam.value.text(), Some("fsk"));
        assert_eq!(fam.n, 4);
        assert!(fam.agreement < 1.0 && fam.agreement > 0.0, "{fam:?}");

        // A genuine change of the measured value wins once it outvotes the old one.
        let mut sync = features();
        sync.fold(field::SYNC_WORD, Feat::bits("01111100", "framer"));
        for _ in 0..3 {
            sync.fold(field::SYNC_WORD, Feat::bits("10101010", "framer"));
        }
        assert_eq!(
            sync.get(field::SYNC_WORD).unwrap().value.bits(),
            Some("10101010")
        );

        // A different *kind* of measurement replaces the field: the two are not averageable.
        let mut mixed = features();
        mixed.fold(field::LEVELS, Feat::num(2.0, 0.0, "estimator"));
        mixed.fold(field::LEVELS, Feat::text("two", "chain"));
        let levels = mixed.get(field::LEVELS).unwrap();
        assert_eq!(levels.value.text(), Some("two"));
        assert_eq!(levels.n, 1, "the statistics restart");
        mixed.validate().unwrap();
    }

    #[test]
    fn an_observation_counts_suspect_fraction_and_projects_to_a_fingerprint() {
        let mut f = features();
        f.observe(
            [
                (
                    field::F_CENTER_HZ.to_owned(),
                    Feat::num(148.5e6, 100.0, "detector"),
                ),
                (field::OBW_HZ.to_owned(), Feat::num(12.5e3, 200.0, "c13")),
                (
                    field::SYMBOL_RATE_HZ.to_owned(),
                    Feat::num(1200.0, 2.0, "c14-cyclic"),
                ),
                (field::FAMILY.to_owned(), Feat::text("fsk", "classifier")),
            ],
            false,
        );
        f.observe([], true);
        assert_eq!(f.observations, 2);
        assert!((f.suspect_fraction - 0.5).abs() < 1e-9, "{f:?}");

        let fp = f.fingerprint();
        assert_eq!(fp.version, crate::cluster::FEATURE_SET_VERSION);
        assert_eq!(fp.f_center_hz, 148.5e6);
        assert_eq!(fp.bandwidth_hz, 12.5e3);
        assert_eq!(fp.family.as_deref(), Some("fsk"));
        assert_eq!(fp.symbol_rate_hz, Some(1200.0));
        f.validate().unwrap();
    }

    #[test]
    fn features_validation_catches_each_broken_invariant() {
        type Break = fn(&mut EmissionFeatures);
        let cases: &[(&str, Break)] = &[
            ("schema", |f| f.schema = 2),
            ("version", |f| f.version = 0),
            ("id", |f| f.id = " ".into()),
            ("suspect fraction", |f| f.suspect_fraction = 1.5),
            ("non-finite value", |f| {
                f.fields.get_mut(field::OBW_HZ).unwrap().value = FeatValue::Num { value: f64::NAN }
            }),
            ("negative sigma", |f| {
                f.fields.get_mut(field::OBW_HZ).unwrap().sigma = -1.0
            }),
            ("sigma not the max", |f| {
                f.fields.get_mut(field::OBW_HZ).unwrap().sigma = 1e9
            }),
            ("agreement", |f| {
                f.fields.get_mut(field::OBW_HZ).unwrap().agreement = 2.0
            }),
            ("n", |f| f.fields.get_mut(field::OBW_HZ).unwrap().n = 0),
            ("method", |f| {
                f.fields.get_mut(field::OBW_HZ).unwrap().method = String::new()
            }),
            ("empty bits", |f| {
                f.fields.get_mut(field::OBW_HZ).unwrap().value = FeatValue::Bits {
                    bits: "0102".into(),
                }
            }),
            ("empty reason", |f| f.reasons = vec![String::new()]),
        ];
        for (name, mutate) in cases {
            let mut f = features();
            f.fold(field::OBW_HZ, Feat::num(12.5e3, 100.0, "c13"));
            f.validate().unwrap();
            mutate(&mut f);
            assert!(f.validate().is_err(), "{name} should be refused");
        }

        // Round trip.
        let mut f = features();
        f.fold(field::SYNC_WORD, Feat::bits("0111110011", "framer"));
        f.fold(field::PERIOD_S, Feat::num(0.12, 0.005, "track-timing"));
        let json = serde_json::to_string(&f).unwrap();
        assert_eq!(serde_json::from_str::<EmissionFeatures>(&json).unwrap(), f);
    }

    /// A partial that is missing required fields keeps its ranked candidates: the "too few
    /// fields" case must never be forced to `none` (and never to an identity).
    #[test]
    fn a_partial_missing_required_fields_is_valid_below_the_partial_score_floor() {
        let mut m = a_match(MatchOutcome::Partial);
        m.candidates[0].score = 0.21;
        m.candidates[0].missing = vec!["sync_word".into(), "deviation_hz".into()];
        m.validate().unwrap();

        // But a partial whose required fields were all measured still has to clear the floor.
        m.candidates[0].missing.clear();
        assert!(m.validate().is_err());
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
