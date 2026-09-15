//! C12 occupancy baseline (ADR-0012 §2–§4, §7): occupancy engine, learned channels, thresholds,
//! baselines, novelty, the interestingness score and novelty alarms.
//!
//! File ownership (ADR-0012 §11): `engine`, `channels`, `threshold` → T-118; `baseline`,
//! `novelty`, `score`, `site` → T-119; `alarm` → T-122. This file is final: owners add items only
//! inside their own files and consumers use full paths (no re-exports here).

pub mod alarm;
pub mod baseline;
pub mod channels;
pub mod engine;
pub mod novelty;
pub mod score;
pub mod site;
pub mod threshold;
