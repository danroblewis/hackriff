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
    AUDIO_OUT_BLOCK, AudioChannels, AudioProfile, ChannelsSpec, DEFAULT_LIVE_EDGE_BACKLOG_S,
    DecodeMapping, Edge, Endpoint, EvidenceTarget, FOLLOW_HOPS_BLOCK, IdentityMapping, InputSpec,
    Liveness, LivenessMode, LivenessSpec, MAX_LIVE_EDGE_BACKLOG_S, MatchHints, NodeSpec,
    ObjectiveForm, OutputKind, OutputPolicy, OutputSpec, PortRef, REFINE_BUILTINS, Recipe,
    RecipeError, RefineGoal, RefineObjective, RefineSpec, Resolved, StageView, parse_param_path,
};

/// `schema` value of every recipe document.
pub const RECIPE_SCHEMA: &str = "hackriff.recipe";
/// Recipe format version this crate writes for a new document, and the newest it reads.
///
/// - **2** (T-085 review, before any release): variable-length framing params, field-map
///   `char_bits: 4`/`pocsag-bcd`/`parity`/`skip_bits`/`scale`/`add`/`value_unit`. Version 1 was
///   never released and is not read.
/// - **3** (ADR-0011 §8.6, "one bump, three keys"): the `audio` output kind and `input.liveness`
///   (T-866); `refine.objective.evidence` (T-858 = MAUTO M-7, ADR-0015 §2.3), with node-parameter
///   paths in `refine.tune` under it; and `refine.objective.builtin` (T-870 = LP-6,
///   ADR-0011 §8.7). Schema 3 only adds optional keys, so a
///   version-2 document is still read unchanged; a schema-3 key in a version-2 document is an
///   error, exactly as an unknown field was.
pub const RECIPE_SCHEMA_VERSION: u32 = 3;

/// Every recipe format version this crate reads.
pub const RECIPE_SCHEMA_VERSIONS: [u32; 2] = [2, 3];

/// A short identifier: `[a-z0-9_-]{1,64}` starting with a letter or digit (recipe, node, output
/// and field-map ids; they appear in stream ids and API paths).
pub fn is_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}
