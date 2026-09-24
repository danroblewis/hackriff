//! Decoder workbench data contracts (ADR-0011, T-085). **Core interface**: changes are reviewed
//! before merge.
//!
//! A decoder is **data, not code**: a [`Recipe`] chains blocks from the block library
//! (`hk-blocks`) with parameters, declares parser [`FieldMap`]s and names its output streams. It
//! runs live in the backend and is hot-edited without stopping capture (ADR-0001).
//!
//! - [`port`]: the typed sample kinds that flow between blocks ([`PortType`]).
//! - [`param`]: block parameter schemas ([`ParamSchema`]) and their validation, plus the
//!   [`BlockDescriptor`] every block publishes and the [`Catalogue`] a recipe validates against.
//! - [`recipe`]: the recipe schema ([`Recipe`]) and its validation.
//! - [`fields`]: the declarative parser's field-map schema ([`FieldMap`]) and its static
//!   validation. Evaluation against frames (T-089) produces
//!   [`hk_stream::inspector::LayerTree`], the inspector's layer tree with bit/byte ranges.
//! - [`edit`]: hot-edit planning ([`EditPlan`]): which nodes keep their state across an edit.
//! - [`matching`]: ranking recipes against a signal's *measured* parameters ([`rank`], T-164), with
//!   per-field reasons. Measurement decides the order; a band-plan hint may only break ties.
//!
//! This crate links no DSP, so the API, storage and authoring assist can validate, store and
//! diff recipes without the block library.
//!
//! ```
//! use hk_recipe::{Recipe, RECIPE_SCHEMA};
//!
//! let json = r#"{
//!   "schema": "hackriff.recipe", "schema_version": 2,
//!   "id": "passthrough", "version": 1, "name": "Pass-through",
//!   "input": {"port": "bits"},
//!   "nodes": [{"id": "copy", "block": "identity"}],
//!   "outputs": [{"id": "bits", "kind": "stage", "from": "copy"}],
//!   "output_policy": {"content_class": "unrestricted"}
//! }"#;
//! let recipe: Recipe = serde_json::from_str(json).unwrap();
//! assert_eq!(recipe.schema, RECIPE_SCHEMA);
//! recipe.validate_structure().unwrap();
//! ```

pub mod edit;
pub mod fields;
pub mod matching;
pub mod param;
pub mod port;
pub mod recipe;

pub use edit::{EditPlan, NodeChange};
pub use fields::{
    AllOf, AnyOf, BitOrder, Charset, Compare, Condition, Display, Endianness, Field, FieldMap,
    FieldMapError, FieldType, Flag, Length, LengthFrom, LengthKeyword, NotOf, Parity, Unit,
};
pub use matching::{
    Candidate, Entry, MatchReason, MeasuredSignal, Outcome, Ranked, RuledOut, Verdict, rank,
};
pub use param::{
    BlockDescriptor, Catalogue, ParamError, ParamSchema, ParamType, Params, PortSpec, parse_hex,
};
pub use port::PortType;
pub use recipe::{
    ChannelsSpec, DecodeMapping, Edge, Endpoint, EvidenceTarget, FOLLOW_HOPS_BLOCK,
    IdentityMapping, InputSpec, MatchHints, NodeSpec, ObjectiveForm, OutputKind, OutputPolicy,
    OutputSpec, PortRef, Recipe, RecipeError, RefineGoal, RefineObjective, RefineSpec, Resolved,
    StageView, parse_param_path,
};

/// `schema` value of every recipe document.
pub const RECIPE_SCHEMA: &str = "hackriff.recipe";
/// The current recipe format version: what this crate writes for a new document, and the newest
/// it reads.
///
/// - **2** (T-085 review, before any release): variable-length framing params, field-map
///   `char_bits: 4`/`pocsag-bcd`/`parity`/`skip_bits`/`scale`/`add`/`value_unit`. Version 1 was
///   never released and is not read.
/// - **3** (T-858 = MAUTO M-7, ADR-0015 §2.3, ADR-0011 §8.6): `refine.objective.evidence`, and
///   node-parameter paths in `refine.tune` under it. ADR-0011 §8.6 puts the audio amendment's
///   `audio` output kind, `input.liveness` and `refine.objective.builtin` in the same version
///   ("one bump, three keys"); they join 3 when they land, as T-111 joined 2, since no version-3
///   document has been released.
///
/// Version 2 documents stay valid and are read unchanged ([`is_supported_schema_version`]); a
/// schema-3 key in a version-2 document is a validation error.
pub const RECIPE_SCHEMA_VERSION: u32 = 3;

/// The oldest recipe format version still read.
pub const RECIPE_SCHEMA_VERSION_MIN: u32 = 2;

/// Whether `v` is a format version this crate reads (2 or 3).
pub const fn is_supported_schema_version(v: u32) -> bool {
    v >= RECIPE_SCHEMA_VERSION_MIN && v <= RECIPE_SCHEMA_VERSION
}

/// A short identifier: `[a-z0-9_-]{1,64}` starting with a letter or digit (recipe, node, output
/// and field-map ids; they appear in stream ids and API paths).
pub fn is_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}
