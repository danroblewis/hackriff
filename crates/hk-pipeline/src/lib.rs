//! hackriff pipeline assembly (T-027): the composition `hackriffd` and `hk replay` run.
//!
//! ```text
//!                 raised priority                       always-on ring readers (own threads)
//! Source ──► capture thread ──► RAM ring (ci8) ──┬─► 1 detect:   STFT (Hann, 0 % overlap) → NoiseFloorTracker
//! (replay;         │ lossless gate               │                → Detector (clip counts) → Tracker
//!  HackRF stub)    │ (unpaced replay only)       │                → batched SQLite: Detections, Tracks, links,
//!                  │                              │                  inventory (T-018), Anomalies (+ correlator)
//!                  │                              ├─► 2 history:  STFT → NoiseFloorTracker → FloorProduct
//!                  │                              │                (C26 pyramid tiles + C33 floor vs time)
//!                  │                              ├─► 3 spectrum: STFT → gated spectrum Publisher (C24)
//!                  │                              └─► 4 survey:   one window per capture state →
//!                  │                                               receiver-line survey (T-399),
//!                  │                                               reused by classification
//!                  │
//!   SourceControl ◄┴── control thread ◄── events (confirmed tracks, members, closes, captures)
//!   (StepApplier;          │  scheduler on stream time (hackriffd), POIs, verification → trust
//!    replay guard)         └─► runtime chains = ring readers attached/detached at runtime with a
//!                              data-built node list (ADR-0001 S1):
//!                              [record] analog-auto (WFM + RDS) | [record] fsk-bursts (+ framing)
//!                              | [ddc] plugin (readsb …, subprocess)
//! ```
//!
//! - **Crate placement.** The composition needs hk-dsp, hk-detect, hk-estimate, hk-demod,
//!   hk-store, hk-context, hk-stream and hk-plugins, all of which depend on hk-core, so it cannot
//!   live in hk-core without cycles; hk-cli stays a thin binary crate over this one. hk-api is not
//!   a dependency: streams are offered through [`StreamSink`] and status through
//!   [`PipelineHandle::counters`].
//! - **Threads.** Capture (raised priority, `hk_core::rt::spawn_capture_thread`), four always-on
//!   readers (detect, history, spectrum and the T-399 receiver-line [`survey`]), one control
//!   thread, one thread per runtime chain or recording. Readers never block
//!   the capture thread; slow consumers are lapped and their loss counted, except in lossless
//!   replay where [`gate::FlowGate`] holds the capture thread back instead.
//! - **Choices** (resolution, overlap and n_eff, batching): [`config`].
//! - **Legal guardrail:** [`class`]; content chains and recordings refuse classes that forbid
//!   content; FSK content fails closed unless classified; plugin output is capped by the input
//!   channel class; spectrum is gated by the publisher.
//! - **Observability:** [`stats::Counters`], the [`RunSummary`] and `/api/status`.

pub mod chains;
pub mod characterise; // T-242
pub mod class;
pub mod classify; // T-199
pub mod compute;
pub mod config;
pub mod control;
pub mod events;
pub mod family;
pub mod gate;
pub mod inventory;
pub mod recipes;
pub mod refine;
pub mod stats;
pub mod survey; // T-399

// ADR-0012 §11 attention + memory wiring (pre-added by T-113; the owners fill them in).
pub mod alarms; // T-122
pub mod attention; // T-119
pub mod candidates; // T-128
pub mod observe; // T-115
pub mod occupancy; // T-118
pub mod presence; // T-388
pub mod reports; // T-121

mod capture;
mod dc_twin; // T-174
mod detect;
mod history;
pub mod iqbuffer; // T-157
mod recorder;
mod run;
mod spectrum;
mod verify;

pub use chains::iq::IqTapOpener;
pub use chains::listen::{ListenConfig, ListenManager, listen_class};
pub use chains::outputs::{
    OUTPUTS_DIR, OutputError, OutputFileStatus, OutputKind, OutputLimits, OutputRecorders,
    OutputRequest, OutputStatus, OutputTarget,
};
pub use chains::spec::{
    ChainShape, ChainSpec, FmRegion, NodeSpec, Trigger, builtin_chains, builtin_chains_for,
};
pub use chains::taps::{BurstHub, BurstTapOpener, TapKind};
pub use characterise::{Characterised, characterise};
pub use class::{ClassRule, classify_emitter, source_class};
pub use config::{
    DISPLAY_AVERAGING_MAX, DISPLAY_FFT_MAX, DISPLAY_FFT_MIN, DISPLAY_ROWS_MAX, DISPLAY_ROWS_MIN,
    DisplayPatch, DisplaySettings, ListenSettings, PipelineConfig, PipelineSettings, StreamSink,
    StreamUnsink, detection_resolution, load_calibrations, replay_plan,
};
pub use control::SwitchableControl;
pub use events::Candidate;
pub use family::{Explanation, FamilyPrior, explain_emitter, explanations};
pub use inventory::{CONFIRM_RULE, ConfirmPolicy, Inventory, TrackInventory};
pub use presence::{
    MAX_EXTENSIONS_PER_TICK, PRESENCE_EXTENSION_KIND, PRESENCE_MESSAGE_SCHEMA, PRESENCE_PUSH_NS,
    PRESENCE_STREAM_ID, PresenceExtension, PresenceStream,
};
pub use recorder::{
    RECORDING_DEFAULT_S, RECORDING_LABEL_MAX, RECORDING_MAX_BYTES, RECORDING_MAX_S, RecordingStatus,
};
pub use refine::RefineSettings;
pub use run::{
    ControlFailure, ControlStats, ControlStatus, DeviceReplay, Pipeline, PipelineController,
    PipelineHandle, REPLUMB_TIMEOUT, Replay, ResolutionSummary, RetuneOutcome, RunSummary,
    SourceFactory, SourceInfo, Stopper, open_mock_replay, open_replay, replay_block_len,
    replay_once,
};
pub use stats::Counters;
pub use survey::{ReceiverSurvey, SurveyCadence, SurveyCounts};

/// `HK_PIPELINE_DEBUG` is set: chain attach/detach and chain results are logged to stderr.
pub(crate) fn debug_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("HK_PIPELINE_DEBUG").is_some())
}
