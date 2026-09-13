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
//! - [`query`]: `/api/history` (T-017 region-over-time) and `/api/floor` (T-021 floor vs time).
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
pub mod query;

pub use hk_stream as stream;

pub use auth::{Token, default_token_path};
pub use bridge::{StreamInfo, StreamRegistry};
pub use control::{AuditLog, DisplayState, DisplayUpdate, RecordingState, RunControl, RunState};
pub use http::{ApiState, ROUTES, Server, ServerConfig};
pub use live_control::{
    LiveControl, LiveControlError, LiveTuning, SourceLiveControl, WindowPolicy, WindowRetuner,
    validate_gains,
};

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_the_model() {
        let _ = hk_model::BitstreamId::new();
    }
}
