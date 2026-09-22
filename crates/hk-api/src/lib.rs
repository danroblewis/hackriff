//! hackriff API. It carries the control plane used by the web UI and `hk` (live view and
//! inspector, C39) and the stream-output contract that delivers bits, symbols and decodes to
//! external programs with framing, backpressure and `content_class` gating (C24, ADR-0004).
//! Core interface: changes are reviewed before merge.
//!
//! - [`stream`]: re-export of the `hk-stream` crate, the versioned stream-output contract
//!   (`docs/stream-contract.md`). It lives in its own crate so `hk-plugins` can use it without
//!   depending on `hk-api`.
//! - [`http`]: the HTTP server (T-022a): JSON endpoints, static UI, and the WebSocket route, all
//!   behind one bearer token ([`auth`]).
//! - [`control`]: the authenticated, audited, receive-only control API (T-050): centre, rate,
//!   named gains, bias tee, display (FFT size, averaging, row rate), manual
//!   recording, bookmarks.
//! - [`live_control`]: the device-generic live control handle behind the device endpoints.
//! - [`bridge`]: the WebSocket bridge, mapping one stream 1:1 to one browser WebSocket as a
//!   `Locality::Remote` consumer (legal-guardrail path).
//! - [`outputs`]: output recordings (T-061): start/stop/list/download recorded bits, symbols, WAV
//!   audio and IQ slices.
//! - [`query`]: `/api/history` (T-017 region-over-time) and `/api/floor` (T-021 floor vs time).
//! - [`tcp`]: the token-authenticated TCP stream server for external programs (T-060): one
//!   handshake line, then the framed stream (or a refusal frame).
//!
//! # Threads, not tokio
//! The server uses std threads and synchronous `tungstenite`, like `hk-stream`: the publisher
//! already owns one blocking writer thread per consumer, so a WebSocket sink plugs in as that
//! thread's writer with no second queue and no runtime; consumer counts are single digits on a
//! handheld; idle connections cost no CPU.

pub mod analyze; // T-190
pub mod assist;
pub mod auth;
pub mod bridge;
pub mod captures;
pub mod classification; // T-247
pub mod clusters; // T-202
pub mod control;
pub mod coverage; // T-368: the coverage map - grey means genuinely unobserved
pub mod datasets; // T-205
pub mod decode; // T-159
pub mod events; // T-264 (ADR-0017 TM-8): the durable catalogue behind the History surface
pub mod http;
pub mod inspector;
pub mod inventory;
pub mod iqbuffer; // T-157
pub mod live_control;
pub mod measurements; // T-818 MAP-18
pub mod navigation; // T-341: the achievable (centre, span) grid and the live-vs-overview claim
pub mod ondemand;
pub mod outputs;
pub mod presence; // T-264 (ADR-0017 TM-8): one emitter's presence track
pub mod query;
pub mod recipes;
pub mod recordings; // T-469: the persisted IQ recordings that extend the audio horizon
pub mod rows; // T-468: rows pushed to a subscription over an ADDRESS RANGE of the tile lattice
pub mod scan; // T-452: the in-app survey sweep, stepping the interactive front end
pub mod selections;
pub mod signatures; // T-201
pub mod taxonomy; // T-218
pub mod tcp;
pub mod tiles; // T-438: one tile of the unified surface, addressed by independent (level_f, level_t)
pub mod timeline; // T-338: the capture window, and the compressed overview drawn on it

// ADR-0012 §8/§11 attention + memory routes (pre-added by T-113; the owners fill them in).
pub mod anomalies; // T-122
pub mod attention; // T-119
pub mod observations; // T-115
pub mod occupancy; // T-118
pub mod reports; // T-121
pub mod schedule; // T-120
pub mod trunking; // T-273

pub use hk_stream as stream;
pub use tcp::{StreamServer, StreamServerConfig, StreamServerStats};

pub use auth::{Token, default_token_path};
pub use bridge::{FINISHED_LINGER, MAX_STREAMS, StreamInfo, StreamRegistry};
pub use control::{
    AuditLog, CaptureStatus, DisplayLimits, DisplayState, DisplayUpdate, RecordingState,
    RunControl, RunState,
};
pub use datasets::{DatasetControl, DatasetFailure};
pub use http::{ApiState, ROUTES, Server, ServerConfig};
pub use iqbuffer::{ClipStart, IqBufferControl, IqBufferFailure, IqBufferQuery};
pub use live_control::{
    DEVICE_GATE_WAIT, DeviceAction, DeviceGate, DeviceGuard, LiveControl, LiveControlError,
    LiveControls, LiveControlsError, LiveTuning, SelectError, SourceLiveControl, WindowPolicy,
    WindowRetuner, validate_gains,
};
pub use outputs::{OutputControl, OutputFailure, OutputStart, OutputTarget};
pub use recordings::{RecordingCatalog, RecordingsFailure};
pub use scan::{Phase as ScanPhase, Prepared as ScanPlan, ScanError, ScanRequest, ScanRunner};

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_the_model() {
        let _ = hk_model::BitstreamId::new();
    }
}
