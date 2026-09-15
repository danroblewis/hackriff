//! Block factories and the registry (ADR-0011 §1.2).

use std::collections::BTreeMap;
use std::sync::Arc;

use hk_recipe::{BlockDescriptor, Catalogue, FieldMap, ParamSchema, Params, PortType};

use crate::block::{Block, BlockError};

/// What a factory may look at when building a node.
pub struct BuildCtx<'a> {
    /// The recipe's field maps (for `fields`).
    pub field_maps: &'a BTreeMap<String, FieldMap>,
    /// Types arriving on the node's inputs (descriptor order), for polymorphic blocks.
    pub input_types: &'a [PortType],
}

impl BuildCtx<'_> {
    /// Field map `id`.
    pub fn field_map(&self, id: &str) -> Option<&FieldMap> {
        self.field_maps.get(id)
    }
}

/// Builds instances of one block kind.
pub trait BlockFactory: Send + Sync {
    /// The kind's descriptor.
    fn descriptor(&self) -> &BlockDescriptor;

    /// A new instance from schema-validated `params`. May allocate; runs off the real-time
    /// thread (at pipeline start and when a hot edit rebuilds the node).
    fn build(&self, params: &Params, ctx: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError>;
}

/// Block kinds by name.
#[derive(Clone, Default)]
pub struct Registry {
    factories: BTreeMap<String, Arc<dyn BlockFactory>>,
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every implemented block (each group registers its own, `crate::blocks`).
    pub fn builtin() -> Self {
        let mut r = Self::new();
        crate::blocks::register_all(&mut r);
        r
    }

    /// Adds a factory. Refuses a duplicate name.
    pub fn register(&mut self, factory: Arc<dyn BlockFactory>) -> Result<(), String> {
        let name = factory.descriptor().name.clone();
        if self.factories.contains_key(&name) {
            return Err(format!("block {name} registered twice"));
        }
        self.factories.insert(name, factory);
        Ok(())
    }

    /// The factory for `name`.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn BlockFactory>> {
        self.factories.get(name)
    }

    /// Descriptors of every registered block (the planned `GET /api/blocks`).
    pub fn descriptors(&self) -> impl Iterator<Item = &BlockDescriptor> {
        self.factories.values().map(|f| f.descriptor())
    }

    /// Validates `params` against the kind's schema, then builds.
    pub fn build(
        &self,
        name: &str,
        params: &Params,
        ctx: &BuildCtx<'_>,
    ) -> Result<Box<dyn Block>, BlockError> {
        let f = self
            .get(name)
            .ok_or_else(|| BlockError::Params(format!("unknown block {name}")))?;
        let maps: Vec<&str> = ctx.field_maps.keys().map(String::as_str).collect();
        let errors = ParamSchema::validate_all(&f.descriptor().params, params, &maps);
        if let Some(e) = errors.first() {
            return Err(BlockError::Params(e.to_string()));
        }
        f.build(params, ctx)
    }
}

impl Catalogue for Registry {
    fn descriptor(&self, name: &str) -> Option<&BlockDescriptor> {
        self.factories.get(name).map(|f| f.descriptor())
    }
}
