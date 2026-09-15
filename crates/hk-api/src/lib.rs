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
//!   named gains, bias tee, display (FFT size, averaging, row rate), pause/resume, manual
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

pub mod auth;
pub mod bridge;
pub mod control;
pub mod http;
pub mod live_control;
pub mod ondemand;
pub mod outputs;
pub mod query;
pub mod selections;
pub mod tcp;

pub use hk_stream as stream;
pub use tcp::{StreamServer, StreamServerConfig, StreamServerStats};

pub use auth::{Token, default_token_path};
pub use bridge::{StreamInfo, StreamRegistry};
pub use control::{AuditLog, DisplayState, DisplayUpdate, RecordingState, RunControl, RunState};
pub use http::{ApiState, ROUTES, Server, ServerConfig};
pub use live_control::{
    LiveControl, LiveControlError, LiveTuning, SourceLiveControl, WindowPolicy, WindowRetuner,
    validate_gains,
};
pub use outputs::{OutputControl, OutputFailure, OutputStart, OutputTarget};

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_the_model() {
        let _ = hk_model::BitstreamId::new();
    }
}
