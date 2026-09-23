//! hackriff end-to-end IQ-replay harness (T-023; [docs/10](../../../docs/10-test-strategy.md) tiers
//! T3 recorded replay and T4 synthetic scenarios).
//!
//! A test gets a fixture, runs a [`Pipeline`] of capability stages over its samples, and asserts
//! the outputs against the fixture's `hackriff:truth` annotations, naming its use-case IDs:
//!
//! ```no_run
//! use hk_e2e::{synth_or_skip, BoxTolerance, Pipeline, SynthRequest};
//!
//! let out = synth_or_skip!(SynthRequest::new("fsk_burst_train").seed(1).param("snr_db", 15));
//! let fixture = out.fixture(0).unwrap();
//! let outputs = Pipeline::new().run(&fixture).unwrap(); // capability stages plug in here
//! let bursts = fixture.of_kind("fsk-burst");
//! let report = hk_e2e::match_detections(&outputs.detections, &bursts, &fixture.artefacts(),
//!     BoxTolerance { time_s: 2e-3, freq_hz: 5e3 });
//! report.assert_all_found(&["AWARE-036"], &bursts);
//! ```
//!
//! - [`synth`]: runs the Python generator (`py/hkpy/synth`) into a content-hashed cache under
//!   `target/synth-cache/`, and skips cleanly when `uv` is missing ([`synth_or_skip!`]).
//! - [`fixture`]: SigMF metadata via `hk_model::sigmf` plus typed access to truth annotations.
//! - [`samples`]: **seam** — a minimal ci8/cu8/cf32 reader, to be replaced by hk-core's replay
//!   source when T-003 merges.
//! - [`pipeline`]: the [`Stage`] trait and provisional output records (to be replaced by the
//!   T-002 data-model objects).
//! - [`assertions`]: truth-box matching with time/frequency tolerance, false-alarm counting and
//!   parameter tolerances, with use-case IDs in every failure message.

pub mod assertions;
pub mod blind;
pub mod checks;
pub mod fixture;
pub mod paths;
pub mod pipeline;
pub mod samples;
pub mod scene;
pub mod synth;

pub use assertions::{
    BoxTolerance, MatchReport, Tolerance, assert_param, check_param, match_detections,
};
pub use checks::Checks;
pub use fixture::{Fixture, FixtureError, Role, TruthItem};
pub use pipeline::{
    DecodedMessage, DetectionBox, FloorEstimate, ParameterEstimate, Pipeline, PipelineError,
    PipelineOutputs, Stage, StageError, StageInput,
};
pub use samples::{Cf32, SampleError};
pub use synth::{SynthError, SynthOutput, SynthRequest};
