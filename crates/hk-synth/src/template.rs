//! Templates: `hackriff.template/1` (ADR-0015 §4.1, with §15's fact provenance and validation).
//!
//! A template is **data about a protocol**: a recipe reference or a skeleton, free-parameter
//! ranges, measured-parameter priors and plausibility checks. It is **never truth**:
//!
//! - priors order the search; they never rank a result and never confirm one (§1.3, §4.1);
//! - `bands_hz` only raises rank for an emitter **already detected** there — nothing tunes to it,
//!   nothing is created from it (CLAUDE.md, workflow #4);
//! - a template never starves unknowns (the open-search floor, §4.2);
//! - validation is quality control on the library, never a bit source (§15.6).
//!
//! The loader, the `consistency` checks (§15.6 level 1), the built-ins and seeding are M-5's.
//! This module fixes the document shape so that work, `save-as-template` (M-10) and the flex
//! importer have one schema to write.

use std::collections::BTreeMap;

use hk_model::ContentClass;
use hk_model::signature::RecipeRef;
use hk_recipe::OutputPolicy;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::candidate::FreeParam;
use crate::result::PipelineResult;
use crate::skeleton::{Skeleton, SkeletonSlots};
use crate::stage::Stage;

/// `schema` of every template document.
pub const TEMPLATE_SCHEMA: &str = "hackriff.template";

/// Template format version. Stays 1 because nothing was implemented before §15's fields (§15.8).
pub const TEMPLATE_SCHEMA_VERSION: u32 = 1;

/// Who authored the template (§4.1). Independent of where each fact came from ([`Fact`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorKind {
    /// Shipped read-only in `templates/`.
    Builtin,
    /// Written by the user.
    User,
    /// Saved from an analyze result (§4.3).
    Discovered,
}

/// A template's provenance: its author, and per-field fact sources (§4.1, §15.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateProvenance {
    /// Author kind.
    pub kind: AuthorKind,
    /// `discovered` only: the analyze job it came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    /// `discovered` only: the emitter it was solved on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emitter_id: Option<String>,
    /// `discovered` only: when.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t: Option<String>,
    /// Where each fact-bearing field came from. Every such field of a **builtin** must be covered
    /// (the loader refuses `fact_unsourced`); a user template's uncovered fields default to
    /// `{kind: user}`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<Fact>,
}

/// What kind of claim a fact is (§15.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactBasis {
    /// A protocol constant from a specification.
    Spec,
    /// A tolerance someone chose (the awkward middle): orders the search, never claims.
    Tolerance,
    /// Measured from a capture.
    Measured,
}

/// One sourced group of template fields (§15.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fact {
    /// Field paths this source covers (`priors.symbol_rate_bd`, `free[clock].domain`).
    pub fields: Vec<String>,
    /// What kind of claim it is.
    pub basis: FactBasis,
    /// Where it came from.
    pub source: FactSource,
}

/// Which artefact of a decoder's repository a fact was read from (§15.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Artefact {
    /// Source code.
    Code,
    /// Tests.
    Tests,
    /// Documentation.
    Docs,
    /// Configuration (e.g. an rtl_433 `conf/*.conf` flex file).
    Conf,
}

/// Where a fact came from (§15.2's table). Bookkeeping in ADR-0010's ledger sense, never a gate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum FactSource {
    /// A standards body's document.
    Standard {
        /// Document reference (`ITU-R M.584-2`).
        #[serde(rename = "ref")]
        reference: String,
        /// Section within it.
        locator: String,
        /// When it was read.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        accessed: Option<String>,
    },
    /// A vendor or alliance specification, application note or datasheet.
    Specification {
        /// Document reference.
        #[serde(rename = "ref")]
        reference: String,
        /// Section within it.
        locator: String,
        /// When it was read.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        accessed: Option<String>,
    },
    /// A paper (DOI or arXiv id).
    Paper {
        /// DOI or arXiv id.
        #[serde(rename = "ref")]
        reference: String,
    },
    /// A published reverse-engineering write-up.
    ReverseEngineering {
        /// URL.
        #[serde(rename = "ref")]
        reference: String,
    },
    /// A community wiki entry.
    Wiki {
        /// URL.
        #[serde(rename = "ref")]
        reference: String,
        /// The page's content licence.
        licence: String,
    },
    /// Read from a decoder's own repository. Recorded, not refused. `licence` is read from the
    /// file, never a forge API (`none-stated` when there is none).
    DecoderSource {
        /// Project name.
        project: String,
        /// Which artefact.
        artefact: Artefact,
        /// Path in the repository.
        path: String,
        /// Commit read.
        commit: String,
        /// Licence as stated in the repository.
        licence: String,
        /// The file the licence was read from.
        licence_read_from: String,
    },
    /// Derived by this system from a capture.
    Measured {
        /// The analyze job, for a discovered template.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        job_id: Option<String>,
        /// Fixture path and content hash, for a fixture fit.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fixture: Option<String>,
    },
    /// The user stated it.
    User,
}

/// The transmitter's timing class, which sets `hk-synth`'s default tolerance widening when a spec
/// states none (§15.3; the widenings themselves are unverified guesses and M-5's).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimingClass {
    /// Crystal-controlled.
    Crystal,
    /// RC oscillator.
    Rc,
    /// Unknown.
    Unknown,
}

/// Measured-parameter priors: a superset of recipe `match`, read the same way — against
/// **measured** parameters (§4.1).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplatePriors {
    /// Family weights (`{"fsk": 1.0}`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub families: BTreeMap<String, f64>,
    /// Symbol-rate ranges, Bd.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub symbol_rate_bd: Vec<[f64; 2]>,
    /// Occupied-bandwidth range, Hz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_hz: Option<[f64; 2]>,
    /// Bursty emission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bursty: Option<bool>,
    /// Bands, Hz. **Rank only**, for an emitter already detected there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bands_hz: Vec<[f64; 2]>,
}

/// What a template expects a stage to show (§4.1 `evidence_targets`). Contributes S6 ranking
/// evidence only when met by measured frames.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceTarget {
    /// S4: sync length in bits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_bits: Option<u32>,
    /// S5: check kind (`crc`, `bch`, `parity`, `checksum`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// S5: check width in bits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
}

/// A field's plausible range, from the message format (§4.1, §15.3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plausibility {
    /// Field name in the recipe's field map.
    pub field: String,
    /// Inclusive range.
    pub range: [f64; 2],
}

/// One level of template validation (§15.6). Reports what each level proves; none is a bit source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "level", rename_all = "snake_case")]
pub enum Validation {
    /// Internally coherent (checked on every load).
    Consistency,
    /// Expressible by the block catalogue. Circular about the facts; never evidence they are true.
    Synthetic {
        /// Generator, `hkpy.synth:<name>`.
        generator: String,
        /// Commit.
        commit: String,
    },
    /// Run blind through the mock SDR against a real capture: the real test.
    Fixture {
        /// Fixture path.
        fixture: String,
        /// Its content hash.
        sha256: String,
        /// The use case it is keyed by.
        use_case: String,
        /// What the run reached.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
    },
    /// A confirmed emitter whose winning pipeline came from this template. Accrued, never asserted.
    Field {
        /// The job.
        job_id: String,
        /// The emitter.
        emitter_id: String,
    },
}

/// A template document (§4.1, §15).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Template {
    /// [`TEMPLATE_SCHEMA`].
    pub schema: String,
    /// [`TEMPLATE_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Stable id.
    pub id: String,
    /// Version; immutable once saved.
    pub version: u32,
    /// Display name, written fresh (no prose crosses from a source, §15.2).
    pub name: String,
    /// Description, written fresh.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Author and fact sources.
    pub provenance: TemplateProvenance,
    /// The recipe this template parameterises. Exactly one of `recipe` / `skeleton`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<RecipeRef>,
    /// The skeleton this (generic) template offers. Exactly one of `recipe` / `skeleton`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skeleton: Option<SkeletonSlots>,
    /// Free parameters (only `protocol`-class params once ADR-0011's `ParamSchema.class` lands).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub free: Vec<FreeParam>,
    /// Measured-parameter priors.
    #[serde(default)]
    pub priors: TemplatePriors,
    /// Transmitter timing class, for the tolerance rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<TimingClass>,
    /// Per-stage evidence targets.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub evidence_targets: BTreeMap<Stage, EvidenceTarget>,
    /// Field plausibility ranges.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plausibility: Vec<Plausibility>,
    /// Content ceiling of what a result may emit: the recipe `output_policy` shape and rules
    /// (a template can restrict, never upgrade; §4.3 clamps it to the job's source class).
    pub output_policy: OutputPolicy,
    /// Validation record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validation: Vec<Validation>,
}

/// What a template's structure is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TemplateStructure<'a> {
    /// A shipped or saved recipe, with free parameters over it.
    Recipe(&'a RecipeRef),
    /// A generic skeleton.
    Skeleton(&'a SkeletonSlots),
}

/// A template's structure is not exactly one of `recipe` / `skeleton`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AmbiguousStructure;

impl std::fmt::Display for AmbiguousStructure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "a template names exactly one of `recipe` or `skeleton`")
    }
}

impl std::error::Error for AmbiguousStructure {}

impl Template {
    /// `id@version`.
    pub fn key(&self) -> String {
        format!("{}@{}", self.id, self.version)
    }

    /// The template's structure, or an error unless exactly one of `recipe` / `skeleton` is set.
    pub fn structure(&self) -> Result<TemplateStructure<'_>, AmbiguousStructure> {
        match (&self.recipe, &self.skeleton) {
            (Some(r), None) => Ok(TemplateStructure::Recipe(r)),
            (None, Some(s)) => Ok(TemplateStructure::Skeleton(s)),
            _ => Err(AmbiguousStructure),
        }
    }

    /// The skeleton this template offers, for a generic template.
    pub fn as_skeleton(&self) -> Option<Skeleton> {
        self.skeleton
            .clone()
            .filter(|_| self.recipe.is_none())
            .map(|body| Skeleton::from_body(self.id.clone(), self.version, body))
    }
}

/// A recipe can restrict its output ceiling, never upgrade it (`OutputPolicy`'s own doc comment):
/// [`save_as_template`] applies the same rule to a saved template, so a discovered template can
/// never promise more than the job that discovered it was allowed to see.
fn clamp_output_policy(mut policy: OutputPolicy, source_class: ContentClass) -> OutputPolicy {
    if source_class != ContentClass::Unrestricted
        && policy.content_class == ContentClass::Unrestricted
    {
        policy.content_class = source_class;
    }
    policy
}

/// ADR-0015 §4.3: `POST /api/analyze/{id}/results/{rank}/template` writes a user template with
/// `provenance.discovered` from one of a finished job's [`PipelineResult`]s.
///
/// - **Solved stages become fixed parameters.** [`PipelineResult::recipe`] is already concrete —
///   "every free parameter bound" (its own doc comment) — so a discovered template simply carries
///   that recipe with an **empty** `free` list: every node's value is what the search found,
///   never narrowed to a range. (§4.3 also widens *unsolved* stages to "the job's ranges" at twice
///   their measured uncertainty; `PipelineResult` does not carry the original search-space domains
///   for stages past [`PipelineResult::stage_reached`], so a discovered template today is always
///   fully bound rather than partly free — a narrower, but still runnable and rankable, document.
///   Widening it is follow-up work once a result exposes those domains.)
/// - **`output_policy` is clamped** to `source_class`, the acquired IQ's content class, per
///   [`clamp_output_policy`].
/// - **Discovered templates rank like any other and confirm nothing** (§4.3): nothing here writes
///   `Validation::Field`, which only accrues once a *later* job's winning pipeline actually comes
///   from this template (§15.6's fourth, accrued level).
///
/// [`save_as_template`]'s request: the id/name/description a caller (the API route) supplies plus
/// the discovery provenance §4.3 requires. Bundled so the function stays under clippy's argument
/// cap and a caller can name each field instead of ordering positionals.
#[derive(Clone, Debug)]
pub struct SaveAsTemplate {
    /// Stable id for the new template.
    pub id: String,
    /// Display name, written fresh.
    pub name: String,
    /// Description, written fresh.
    pub description: String,
    /// The analyze job the result came from.
    pub job_id: String,
    /// The emitter the job attached to, if any.
    pub emitter_id: Option<String>,
    /// When the template was saved.
    pub t: String,
    /// The acquired IQ's content class, which [`clamp_output_policy`] never lets the saved
    /// template's ceiling exceed.
    pub source_class: ContentClass,
}

/// Callers validate the id (`hk_model::signature::is_signature_id`-shaped) and uniqueness against
/// the library themselves; this only builds the document.
pub fn save_as_template(result: &PipelineResult, req: SaveAsTemplate) -> Template {
    Template {
        schema: TEMPLATE_SCHEMA.into(),
        schema_version: TEMPLATE_SCHEMA_VERSION,
        id: req.id,
        version: 1,
        name: req.name,
        description: req.description,
        provenance: TemplateProvenance {
            kind: AuthorKind::Discovered,
            job_id: Some(req.job_id),
            emitter_id: req.emitter_id,
            t: Some(req.t),
            facts: Vec::new(),
        },
        recipe: Some(RecipeRef {
            id: result.recipe.id.clone(),
            version: result.recipe.version,
        }),
        skeleton: None,
        free: Vec::new(),
        priors: TemplatePriors::default(),
        timing: None,
        evidence_targets: BTreeMap::new(),
        plausibility: Vec::new(),
        output_policy: clamp_output_policy(result.recipe.output_policy.clone(), req.source_class),
        validation: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0015 §4.1's POCSAG example, with §15.2's fact provenance. `metadata_keys` is written in
    /// the recipe §9.3 map shape (the ADR's list form is shorthand; ADR-0015 §16 records it).
    const POCSAG: &str = r#"{
      "schema": "hackriff.template", "schema_version": 1,
      "id": "pocsag", "version": 1, "name": "POCSAG paging", "description": "Pager batches.",
      "provenance": { "kind": "builtin", "facts": [
        { "fields": ["priors.families", "priors.symbol_rate_bd", "evidence_targets.S4"],
          "basis": "spec",
          "source": { "kind": "standard", "ref": "ITU-R M.584-2", "locator": "Annex 1 §4", "accessed": "2026-09-22" } },
        { "fields": ["free[clock].domain"], "basis": "tolerance",
          "source": { "kind": "decoder-source", "project": "rtl_433", "artefact": "code",
                      "path": "src/devices/x.c", "commit": "abc", "licence": "GPL-2.0-or-later",
                      "licence_read_from": "COPYING" } } ] },
      "recipe": { "id": "pocsag", "version": 3 },
      "free": [ { "path": "nodes[clock].params.symbol_rate_bd",
                  "domain": { "enum": { "values": [512, 1200, 2400] } } } ],
      "priors": { "families": { "fsk": 1.0 }, "symbol_rate_bd": [[512, 2400]], "bandwidth_hz": [8e3, 25e3],
                  "bursty": true, "bands_hz": [[137e6, 174e6], [420e6, 470e6]] },
      "timing": "crystal",
      "evidence_targets": { "S4": { "sync_bits": 32 }, "S5": { "kind": "bch", "width": 10 } },
      "plausibility": [ { "field": "ric", "range": [0, 2097151] } ],
      "output_policy": { "content_class": "restricted-paging",
                         "metadata_keys": { "capcode": { "type": "digits", "max_len": 8 } } },
      "validation": [ { "level": "consistency" } ] }"#;

    #[test]
    fn the_adr_example_parses_and_round_trips() {
        let t: Template = serde_json::from_str(POCSAG).unwrap();
        assert_eq!(t.key(), "pocsag@1");
        assert_eq!(t.provenance.kind, AuthorKind::Builtin);
        assert!(matches!(
            t.provenance.facts[1].source,
            FactSource::DecoderSource {
                artefact: Artefact::Code,
                ..
            }
        ));
        assert_eq!(t.evidence_targets[&Stage::S4].sync_bits, Some(32));
        assert!(matches!(t.structure(), Ok(TemplateStructure::Recipe(_))));
        assert!(t.as_skeleton().is_none());
        let back: Template = serde_json::from_value(serde_json::to_value(&t).unwrap()).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn a_template_names_exactly_one_structure() {
        let mut t: Template = serde_json::from_str(POCSAG).unwrap();
        t.skeleton = Some(SkeletonSlots::default());
        assert_eq!(t.structure(), Err(AmbiguousStructure));
        t.recipe = None;
        assert!(matches!(t.structure(), Ok(TemplateStructure::Skeleton(_))));
        assert_eq!(t.as_skeleton().unwrap().key(), "pocsag@1");
        t.skeleton = None;
        assert_eq!(t.structure(), Err(AmbiguousStructure));
    }

    #[test]
    fn an_unknown_key_is_refused_not_ignored() {
        let bad = POCSAG.replacen("\"timing\"", "\"tune_to_hz\": 1.0, \"timing\"", 1);
        assert!(serde_json::from_str::<Template>(&bad).is_err());
    }

    /// §4.3's `save_as_template` (M-10, T-861).
    mod save_as_template_tests {
        use hk_recipe::Recipe;

        use super::*;
        use crate::result::{PipelineResult, Verdict};
        use crate::stage::Stage as SynthStage;

        fn recipe() -> Recipe {
            serde_json::from_str(include_str!("../../../recipes/adsb.recipe.json")).unwrap()
        }

        fn solved() -> PipelineResult {
            PipelineResult {
                rank: 1,
                verdict: Verdict::Solved,
                summary: "s".into(),
                recipe: recipe(),
                template: None,
                stage_reached: SynthStage::S5,
                stages: Vec::new(),
                evidence_bits: 40.0,
                prior_bits: 0.0,
                analytic_holdout_bits: Some(30.0),
                check: None,
                frames_preview: Vec::new(),
                characterisation: None,
                holdout: None,
            }
        }

        #[test]
        fn writes_a_discovered_template_with_the_recipe_fully_bound() {
            let r = solved();
            let t = save_as_template(
                &r,
                SaveAsTemplate {
                    id: "my-adsb".into(),
                    name: "My ADS-B".into(),
                    description: "Saved from a run.".into(),
                    job_id: "a3".into(),
                    emitter_id: Some("e7".to_owned()),
                    t: "2026-09-25T00:00:00Z".into(),
                    source_class: ContentClass::Unrestricted,
                },
            );
            assert_eq!(t.schema, TEMPLATE_SCHEMA);
            assert_eq!(t.schema_version, TEMPLATE_SCHEMA_VERSION);
            assert_eq!(t.id, "my-adsb");
            assert_eq!(t.version, 1);
            assert_eq!(t.provenance.kind, AuthorKind::Discovered);
            assert_eq!(t.provenance.job_id.as_deref(), Some("a3"));
            assert_eq!(t.provenance.emitter_id.as_deref(), Some("e7"));
            assert!(t.provenance.t.is_some());
            assert_eq!(
                t.recipe,
                Some(RecipeRef {
                    id: r.recipe.id.clone(),
                    version: r.recipe.version
                })
            );
            assert!(t.skeleton.is_none());
            assert!(t.free.is_empty(), "the result's recipe is already bound");
            assert_eq!(t.output_policy, r.recipe.output_policy);
            // Never confirms anything and accrues no field validation on save.
            assert!(t.validation.is_empty());
            // A discovered template still parses back through the shared schema.
            let back: Template = serde_json::from_value(serde_json::to_value(&t).unwrap()).unwrap();
            assert_eq!(back, t);
        }

        #[test]
        fn output_policy_is_clamped_to_the_source_class_never_upgraded() {
            let r = solved();
            assert_eq!(
                r.recipe.output_policy.content_class,
                ContentClass::Unrestricted,
                "the fixture recipe is unrestricted, so the clamp is exercised below"
            );
            fn req(source_class: ContentClass) -> SaveAsTemplate {
                SaveAsTemplate {
                    id: "id".into(),
                    name: "n".into(),
                    description: "d".into(),
                    job_id: "a1".into(),
                    emitter_id: None,
                    t: "t".into(),
                    source_class,
                }
            }
            let t = save_as_template(&r, req(ContentClass::MetadataOnly));
            assert_eq!(t.output_policy.content_class, ContentClass::MetadataOnly);

            // A source class at least as permissive never downgrades the recipe's own ceiling.
            let t2 = save_as_template(&r, req(ContentClass::Unrestricted));
            assert_eq!(t2.output_policy.content_class, ContentClass::Unrestricted);
        }
    }
}
