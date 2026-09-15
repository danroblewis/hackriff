//! T-024: the M0 vertical-slice acceptance suite (docs/11-roadmap.md §1.1), one module per use
//! case plus the legal-guardrail regression. Run with `just acceptance`; CI runs it in the
//! `acceptance` job with `HK_E2E_REQUIRE_SYNTH=1` and `HK_REQUIRE_FIXTURES=1`, so a missing
//! generator or LFS fixture fails instead of skipping. Only `readsb` (absent in CI) may skip, and
//! only the plugin half of SIGNAL-001.
//!
//! Every test replays IQ through the composed pipeline via `hk_pipeline`'s library entry point
//! (`hk replay`'s path) and asserts on docs/07 objects through the Repository's gated getters,
//! `query_inventory`, the history/floor product and the stream and API outputs.

#[path = "acceptance/common.rs"]
mod common;

#[path = "acceptance/blind.rs"]
mod blind;

#[path = "acceptance/signal_001.rs"]
mod signal_001;

#[path = "acceptance/aware_036.rs"]
mod aware_036;

#[path = "acceptance/signal_062.rs"]
mod signal_062;

#[path = "acceptance/aware_006.rs"]
mod aware_006;

#[path = "acceptance/space_050.rs"]
mod space_050;

#[path = "acceptance/aware_053.rs"]
mod aware_053;

#[path = "acceptance/aware_042.rs"]
mod aware_042;

#[path = "acceptance/legal.rs"]
mod legal;

#[path = "acceptance/listen.rs"]
mod listen;

#[path = "acceptance/device_variants.rs"]
mod device_variants;

#[path = "acceptance/inventory_lifecycle.rs"]
mod inventory_lifecycle;

#[path = "acceptance/hil_hackrf.rs"]
mod hil_hackrf;

#[path = "acceptance/tutorial_rds.rs"]
mod tutorial_rds;

#[path = "acceptance/tutorial_pocsag.rs"]
mod tutorial_pocsag;
#[path = "acceptance/tutorial_acars.rs"]
mod tutorial_acars;
#[path = "acceptance/tutorial_adsb.rs"]
mod tutorial_adsb;
