//! Stream-output contract v1 (ADR-0004; spec: `docs/stream-contract.md`). One contract serves
//! external consumers (C24) and the plugin data plane (ADR-0003, T-014). Core interface and
//! legal enforcement point: changes are reviewed before merge.
//!
//! Its own crate (re-exported as `hk_api::stream`) so the plugin host can use it without
//! depending on `hk-api` or `hk-core`.
//!
//! - [`frame`]: `u32` LE length-prefixed frames and the incremental [`FrameDecoder`].
//! - [`header`]: the JSON [`StreamHeader`] (first frame) and [`StreamKind`].
//! - [`record`]: NDJSON message records, 32-byte binary record headers, "dropped N" markers.
//! - [`gate`]: egress gating, the single enforcement point for restricted content (class
//!   clamping, own-key streams local-only, gated spectrum row-rate and payload enforcement).
//! - [`policy`]: the typed metadata allowlist ([`MetadataPolicy`]) shared by plugin ingest,
//!   in-process producers and the publisher.
//! - [`publisher`]: the drop-not-block fan-out [`Publisher`] and the ungated [`DecoderFeed`].
//!   Every consumer is subscribed with a [`Locality`] derived from its writer ([`EgressWriter`]).
//! - [`transport`]: Unix-domain-socket (0600) and TCP [`Listener`]s.
//! - [`client`]: the reference [`StreamReader`] (used by `hk stream-tail` and the dummy plugin).
//!
//! # Threads, not tokio
//! Each consumer gets one blocking writer thread fed from a bounded byte ring; each listener has
//! one accept thread. Consumer counts are single digits on a handheld (capped by
//! `max_consumers`); blocked threads cost no CPU when idle (C24: "idle streams must cost
//! nothing"); the producer is a synchronous real-time thread anyway; and std needs no new
//! dependency. A WebSocket bridge for browsers (spike S3) is a follow-up and may bring an async
//! runtime into its own binary.

pub mod audio;
pub mod bursts;
pub mod client;
pub mod frame;
pub mod gate;
pub mod header;
pub mod ondemand;
pub mod policy;
pub mod publisher;
pub mod record;
pub mod transport;

pub use ondemand::{OpenRefusal, OpenRequest, OpenedStream, OpenerRegistry, StreamOpener};

pub use client::{ClientError, StreamReader};
pub use frame::{FrameDecoder, FrameError, HEADER_MAX_LEN, LEN_PREFIX, MAX_FRAME_LEN};
pub use gate::{GATED_SPECTRUM_BURST_ROWS, GATED_SPECTRUM_MAX_ROW_RATE_HZ};
pub use header::{
    DEFAULT_MAX_FRAME_LEN, HeaderError, STREAM_SCHEMA, STREAM_VERSION_MAJOR, STREAM_VERSION_MINOR,
    StreamHeader, StreamKind,
};
pub use policy::{Charset, IdentitySpec, MetadataPolicy, MetadataType};
pub use publisher::{
    CloseReason, ConsumerId, ConsumerState, ConsumerStats, Declared, DecoderFeed, EgressWriter,
    FeedAttacher, FeedFraming, GateStats, Locality, PublishOutcome, Publisher, PublisherConfig,
    PublisherHandle, SpectrumGateReason, StreamError,
};
pub use record::{
    BINARY_RECORD_HEADER_LEN, BinaryData, BinaryRecord, BinaryRecordHeader, BinaryRecordType,
    DropMarker, MessageEnvelope, MessageRecord, Record, RecordFlags,
};
pub use transport::{ListenAddr, Listener};
