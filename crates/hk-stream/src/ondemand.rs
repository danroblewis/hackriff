//! On-demand streams (T-043; reused by T-060): a stream a consumer asks for, opened for that
//! consumer and closed when it goes away, e.g. listening to one emitter. Transport-agnostic: a
//! front end (the hk-api WebSocket route `/ws/open/<name>`, later TCP) parses the request into
//! an [`OpenRequest`], calls a [`StreamOpener`], subscribes its connection to the returned
//! [`OpenedStream::handle`] exactly like any other stream, and drops [`OpenedStream::session`]
//! when the connection ends.
//!
//! # Contract
//! - **Gating happens in the opener, before anything is attached.** An opener refuses with an
//!   [`OpenRefusal`] (an HTTP-style status and a reason, never content) and starts nothing. What
//!   it does start publishes through a normal [`crate::Publisher`], so the egress gate
//!   ([`crate::gate`]), sequence numbers, drop markers and the per-consumer drop-not-block queue
//!   all apply unchanged: a slow consumer never blocks the producer.
//! - **Lifetime.** The producer stops when the session guard is dropped (consumer gone or
//!   stopped), and may stop on its own (idle timeout, source gone), which finishes the publisher
//!   and so closes the consumer.
//! - **Transport locality** is the front end's: a remote transport subscribes with
//!   [`crate::Declared::remote`], so `own-key-decrypted` streams stay local-only.

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, RwLock};

use hk_model::ContentClass;
use serde_json::{Value, json};

use crate::header::StreamHeader;
use crate::publisher::PublisherHandle;

/// Largest number of request parameters accepted.
pub const MAX_OPEN_PARAMS: usize = 16;

/// What a consumer asked for: the transport's query parameters (the token already checked and
/// removed) and a label for logs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenRequest {
    /// `(name, value)` pairs in request order.
    pub params: Vec<(String, String)>,
    /// Who asked (e.g. `ws:127.0.0.1:50000`), for consumer labels only.
    pub peer: String,
}

impl OpenRequest {
    /// A request from query parameters; `token` is dropped so it never reaches an opener.
    pub fn from_query(query: &[(String, String)], peer: impl Into<String>) -> Self {
        Self {
            params: query
                .iter()
                .filter(|(k, _)| k != "token")
                .take(MAX_OPEN_PARAMS)
                .cloned()
                .collect(),
            peer: peer.into(),
        }
    }

    /// The first value of `name`.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// How the **transport** saw an on-demand session end (T-633).
///
/// The session guard is a bare `drop`, so a producer that counted every dropped guard as "the
/// client went away" reported a server-side reap and a transport fault under the same label an
/// operator reads to blame their own client. These are the cases the transport can actually
/// distinguish; a producer maps them onto its own counters, and [`SessionEnd::Unattributed`]
/// stays its own answer rather than collapsing into the convenient one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum SessionEnd {
    /// The guard was dropped without the transport saying why. **Not** a client end.
    #[default]
    Unattributed = 0,
    /// An affirmative end from the client: a close frame, a hang-up, a data message, or Stop.
    Client = 1,
    /// The server stopped hearing the peer (no pong for the peer timeout) and reaped it. Nobody
    /// said the client went away; the server gave up on it.
    Unresponsive = 2,
    /// A reset, or a read/write error on the connection.
    Transport = 3,
}

impl SessionEnd {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Client,
            2 => Self::Unresponsive,
            3 => Self::Transport,
            _ => Self::Unattributed,
        }
    }
}

/// The slot a transport sets **before** dropping [`OpenedStream::session`], so the producer can
/// count the end it actually had (T-633). Shared with the producer; `Unattributed` until set.
#[derive(Clone, Debug, Default)]
pub struct SessionEndSlot(Arc<AtomicU8>);

impl SessionEndSlot {
    /// Records how the transport saw the session end. The **first** attribution wins, so a
    /// later, less informed drop cannot overwrite it.
    pub fn set(&self, end: SessionEnd) {
        let _ = self.0.compare_exchange(
            SessionEnd::Unattributed as u8,
            end as u8,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }

    /// What the transport reported, or [`SessionEnd::Unattributed`].
    #[must_use]
    pub fn get(&self) -> SessionEnd {
        SessionEnd::from_u8(self.0.load(Ordering::SeqCst))
    }
}

/// A stream opened for one consumer.
pub struct OpenedStream {
    /// The stream header (also the first frame every consumer receives).
    pub header: StreamHeader,
    /// Subscribe the requesting connection here.
    pub handle: PublisherHandle,
    /// Dropping it stops the producer (detaches its chain).
    pub session: Box<dyn Any + Send>,
    /// How the transport saw the session end (T-633), set before `session` is dropped.
    pub end: SessionEndSlot,
}

/// Why a stream was not opened. Carries no content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenRefusal {
    /// HTTP-style status: 400 bad request, 403 legal gate, 404 unknown target, 409 conflict
    /// (e.g. outside the tuned window), 410 source gone, 422 nothing to open (e.g. no analog
    /// modulation recognised), 503 at capacity, 504 timed out.
    pub status: u16,
    /// A short machine token, e.g. `restricted-class`, `busy`.
    pub code: String,
    /// Human-readable reason.
    pub reason: String,
    /// The content class that caused a legal refusal, if one did.
    pub content_class: Option<ContentClass>,
}

impl OpenRefusal {
    /// A refusal.
    pub fn new(status: u16, code: &str, reason: impl Into<String>) -> Self {
        Self {
            status,
            code: code.to_owned(),
            reason: reason.into(),
            content_class: None,
        }
    }

    /// A legal-gate refusal (403) naming the class.
    pub fn gated(class: ContentClass, reason: impl Into<String>) -> Self {
        Self {
            content_class: Some(class),
            ..Self::new(403, "restricted-class", reason)
        }
    }

    /// WebSocket close code for this refusal: `4000 + status` (private-use range).
    pub fn close_code(&self) -> u16 {
        4000 + self.status.min(999)
    }

    /// The refusal as JSON (`{"type":"refused", ...}`), what front ends send before closing.
    pub fn to_json(&self) -> Value {
        json!({
            "type": "refused",
            "status": self.status,
            "code": self.code,
            "reason": self.reason,
            "content_class": self.content_class,
        })
    }
}

impl std::fmt::Display for OpenRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}: {}", self.status, self.code, self.reason)
    }
}

/// Opens streams on request. Implementations gate first and attach nothing when they refuse.
pub trait StreamOpener: Send + Sync {
    /// Opens a stream for `request`.
    fn open(&self, request: &OpenRequest) -> Result<OpenedStream, OpenRefusal>;

    /// What discovery reports about this opener (T-060): a JSON object such as
    /// `{"kind": "bits", "datatype": "ru8", "params": [...]}`. Metadata only; the default is
    /// empty.
    fn describe(&self) -> Value {
        json!({})
    }
}

/// Openers by name (the `<name>` of `/ws/open/<name>`).
#[derive(Clone, Default)]
pub struct OpenerRegistry {
    inner: Arc<RwLock<BTreeMap<String, Arc<dyn StreamOpener>>>>,
}

impl OpenerRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `opener` under `name` (replacing any previous one) and returns the registry.
    #[must_use]
    pub fn with(self, name: &str, opener: Arc<dyn StreamOpener>) -> Self {
        self.register(name, opener);
        self
    }

    /// Registers `opener` under `name`.
    pub fn register(&self, name: &str, opener: Arc<dyn StreamOpener>) {
        let mut m = self.inner.write().unwrap_or_else(|p| p.into_inner());
        m.insert(name.to_owned(), opener);
    }

    /// The opener named `name`.
    pub fn get(&self, name: &str) -> Option<Arc<dyn StreamOpener>> {
        let m = self.inner.read().unwrap_or_else(|p| p.into_inner());
        m.get(name).cloned()
    }

    /// Registered names.
    pub fn names(&self) -> Vec<String> {
        let m = self.inner.read().unwrap_or_else(|p| p.into_inner());
        m.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_never_reaches_openers_and_refusals_map_to_close_codes() {
        let q = vec![
            ("token".to_owned(), "secret".to_owned()),
            ("emitter".to_owned(), "abc".to_owned()),
        ];
        let r = OpenRequest::from_query(&q, "ws:x");
        assert_eq!(r.param("token"), None);
        assert_eq!(r.param("emitter"), Some("abc"));
        let g = OpenRefusal::gated(ContentClass::RestrictedPaging, "paging band");
        assert_eq!(g.close_code(), 4403);
        assert_eq!(g.to_json()["content_class"], "restricted-paging");
        assert_eq!(OpenRefusal::new(503, "busy", "x").close_code(), 4503);
    }
}
