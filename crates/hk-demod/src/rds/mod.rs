//! RDS/RBDS (EN 50067 / NRSC-4): owned by C19 because it rides the FM MPX (docs/06 §5).
//!
//! - [`demod`]: MPX + pilot phase → bits (57 kHz = 3 × pilot, biphase matched filter, timing
//!   recovery, differential decoding).
//! - [`block`]: block code, syndromes, offset words, block synchronisation.
//! - [`group`]: groups → PI, PS frames (scrolling-aware), PTY, TP/TA, error rates.
//!
//! Broadcast RDS is public station identity: decodes are [`hk_model::ContentClass::Unrestricted`].
//! RDS on a mono station without a pilot is not handled yet (the subcarrier is recovered from the
//! pilot); T-010's x² line estimate is the route if it is needed.

pub mod block;
pub mod demod;
pub mod group;

use serde::{Deserialize, Serialize};

pub use block::{BlockEvent, BlockSync, Offset, SyncConfig, encode_block, syndrome};
pub use demod::{RdsBit, RdsDemod, RdsDemodConfig};
pub use group::{
    GroupConfig, PiAbstain, PiDecision, PsFrame, RDS_BITRATE_BD, RdsDecoder, RdsGroup, RdsReport,
};

/// RDS settings: physical layer and group decoding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RdsConfig {
    /// Physical layer.
    pub demod: RdsDemodConfig,
    /// Groups, PI vote, PS frames.
    pub groups: GroupConfig,
}
