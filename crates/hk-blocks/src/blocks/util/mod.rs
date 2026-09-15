//! Utility blocks (T-085): the contract example.

use std::sync::Arc;

use crate::Registry;

pub mod identity;

/// Registers this group's implemented blocks.
pub fn register(r: &mut Registry) {
    r.register(Arc::new(identity::IdentityFactory::new()))
        .expect("identity registered once");
}
