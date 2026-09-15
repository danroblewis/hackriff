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
//!
//! This crate links no DSP, so the API, storage and authoring assist can validate, store and
//! diff recipes without the block library.
//!
//! ```
//! use hk_recipe::{Recipe, RECIPE_SCHEMA};
//!
//! let json = r#"{
//!   "schema": "hackriff.recipe", "schema_version": 1,
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
pub mod param;
pub mod port;
pub mod recipe;

pub use edit::{EditPlan, NodeChange};
pub use fields::{
    AllOf, AnyOf, BitOrder, Charset, Compare, Condition, Display, Endianness, Field, FieldMap,
    FieldMapError, FieldType, Flag, Length, LengthFrom, LengthKeyword, NotOf, Unit,
};
pub use param::{
    BlockDescriptor, Catalogue, ParamError, ParamSchema, ParamType, Params, PortSpec, parse_hex,
};
pub use port::PortType;
pub use recipe::{
    ChannelsSpec, DecodeMapping, Edge, Endpoint, FOLLOW_HOPS_BLOCK, IdentityMapping, InputSpec,
    MatchHints, NodeSpec, OutputKind, OutputPolicy, OutputSpec, PortRef, Recipe, RecipeError,
    RefineGoal, RefineObjective, RefineSpec, Resolved, StageView,
};

/// `schema` value of every recipe document.
pub const RECIPE_SCHEMA: &str = "hackriff.recipe";
/// Recipe format version this crate reads and writes.
pub const RECIPE_SCHEMA_VERSION: u32 = 1;

/// A short identifier: `[a-z0-9_-]{1,64}` starting with a letter or digit (recipe, node, output
/// and field-map ids; they appear in stream ids and API paths).
pub fn is_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}
