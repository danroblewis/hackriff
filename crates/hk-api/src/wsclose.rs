//! One shared WebSocket close path for every bridged consumer (T-954, hardened by T-1010).
//!
//! A session that ends by shutting the TCP connection down reads as close code `1006`
//! ("abnormal closure") in every browser — indistinguishable from a real fault, however clean
//! the actual reason was. So **every** ending this server decides sends a real close frame first:
//! `/ws/open/<name>` ([`crate::ondemand`]) and `/ws/<stream_id>` ([`crate::bridge`]) both go
//! through the [`Conn`] here, whose mutex is also the socket writer's, so the frame can never
//! land inside a record.
//!
//! **No close path may block.** Three of the five [`hk_stream::CloseReason`]s are raised on a
//! thread that must not wait on a browser:
//!
//! - `SlowConsumer` fires straight out of `Publisher::publish_binary`/`publish_status`, on the
//!   **producer's own real-time thread** ("the producer never waits on a browser",
//!   [`crate::bridge`]'s module docs), and `DrainTimeout` from the watchdog thread shared by every
//!   draining consumer. They take the lock only if it is free **right now** and write the frame
//!   with the socket in **non-blocking** mode, so the promised `1008` reaches a peer that is
//!   reading (T-1010) and a peer that is not costs the producer nothing — the raw shutdown that
//!   follows is what actually ends it either way. Before T-1010 these two sent no frame at all,
//!   which made `docs/api.md`'s promise of `1008 (too slow to keep up)` / `1008 (did not drain in
//!   time)` false: a slow consumer that *was* reading (over the tunnel, say) saw `1006`.
//! - `PeerGone` fires after the writer's own write already failed, and `Detached` is the
//!   connection's own close after it has made its own attempt: both try the lock once, without
//!   waiting.
//! - `PublisherFinished` fires from the writer thread once its last write returned, so the lock is
//!   normally free; it may briefly wait ([`CLOSE_LOCK_WAIT`]) behind a ping or the connection's
//!   own close attempt, so a normal end is not left to read as `1006` over that race.
//!
//! Every close-frame write runs with the short [`CLOSE_WRITE_TIMEOUT`], never the peer timeout.

use std::io::{self, Write};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::{Duration, Instant};

use hk_stream::CloseReason;
use tungstenite::protocol::frame::coding::CloseCode;

use crate::bridge::WsSink;

/// Longest close reason (the WebSocket limit is 123 bytes).
pub(crate) const MAX_CLOSE_REASON: usize = 120;

/// Write timeout for every *blocking* close-frame attempt (T-954, review attempt 3). A close frame
/// is a few bytes: to a peer that is reading it goes out at once, and a peer whose send buffer is
/// full is not reading, so the full peer timeout the socket otherwise carries would only hold the
/// session (its guard, producer chain and budget slot) that much longer. The socket is shut down
/// right after every close attempt, so shortening its timeout here cannot fail a later record
/// write.
pub(crate) const CLOSE_WRITE_TIMEOUT: Duration = Duration::from_millis(200);

/// Longest a closer waits for a [`Conn`]'s lock before giving up on the close frame (T-954, review
/// attempt 3). A writer mid-write to a live peer releases it within milliseconds; one that still
/// holds it after this is stuck on a peer that is not reading, which would not read the frame
/// either — and only the raw shutdown that follows can unblock it.
pub(crate) const CLOSE_LOCK_WAIT: Duration = Duration::from_millis(200);

/// How often [`lock_within`] retries. 100 µs rather than a millisecond so a waiting closer can
/// take the micro-gap between two of a busy writer's writes instead of missing every one of them
/// for the whole wait (T-1010, review item 6); it only ever runs on a close path.
const LOCK_POLL: Duration = Duration::from_micros(100);

/// The consumer's socket writer, shared with the connection's own reader so pings and close
/// frames never land inside a message.
pub(crate) struct Conn {
    sink: WsSink,
    /// The `101` response and the first messages went out: pings and a close frame may follow.
    /// Before that the upgrade is incomplete on the wire and a close frame would be nonsense, so
    /// such a session is left as the bare hang-up it always was.
    started: bool,
    /// A close frame was already written: whichever side gets there first wins, and no record may
    /// follow it.
    close_sent: bool,
}

impl Conn {
    pub(crate) fn new(sink: WsSink) -> Self {
        Self {
            sink,
            started: false,
            close_sent: false,
        }
    }

    /// For tests and for a connection whose handshake is known to have gone out already.
    #[cfg(test)]
    pub(crate) fn started(sink: WsSink) -> Self {
        Self {
            sink,
            started: true,
            close_sent: false,
        }
    }

    /// True once the `101` response and the first bytes reached the peer.
    pub(crate) fn is_started(&self) -> bool {
        self.started
    }

    /// Puts this connection's socket in (or out of) non-blocking mode for one short write the
    /// caller makes itself (the liveness ping, [`crate::ondemand::watch`]). Holding the [`Conn`]
    /// is what makes it safe: the writer thread shares the same file description.
    pub(crate) fn set_nonblocking(&mut self, on: bool) {
        self.sink.set_nonblocking(on);
    }

    /// Sends the close frame at most once, with the short [`CLOSE_WRITE_TIMEOUT`].
    pub(crate) fn send_close(&mut self, code: CloseCode, reason: &str) {
        if self.started && !self.close_sent {
            self.sink.set_write_timeout(CLOSE_WRITE_TIMEOUT);
            self.sink.close(code, reason);
            self.close_sent = true;
        }
    }

    /// Sends the close frame **without ever blocking** (T-1010): the socket goes non-blocking for
    /// the attempt, so a full send buffer costs a `WouldBlock` the sink discards rather than a
    /// wait on the producer's real-time thread. A partial frame cannot confuse the peer, because
    /// no record may follow a close ([`ConnSink::write`] refuses) and the caller shuts the socket
    /// down immediately afterwards.
    pub(crate) fn try_send_close(&mut self, code: CloseCode, reason: &str) {
        if self.started && !self.close_sent {
            self.sink.set_nonblocking(true);
            self.sink.close(code, reason);
            self.sink.set_nonblocking(false);
            self.close_sent = true;
        }
    }

    /// Writes `buf` as this consumer's egress, unless the session is already closed.
    fn write_record(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.close_sent {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the session was closed",
            ));
        }
        let n = self.sink.write(buf)?;
        self.started = true;
        Ok(n)
    }
}

/// A [`Conn`]'s lock if it can be had within `wait` ([`Duration::ZERO`]: only if free right now).
///
/// **Never `lock()` on a close path (T-954, review attempt 3):** [`ConnSink::write`] holds that
/// lock for a whole socket write, whose timeout is the full peer timeout, so a closer that blocks
/// on it waits out a write stuck on a vanished peer — the one the close is meant to cut short.
pub(crate) fn lock_within(conn: &Mutex<Conn>, wait: Duration) -> Option<MutexGuard<'_, Conn>> {
    let deadline = Instant::now() + wait;
    loop {
        match conn.try_lock() {
            Ok(g) => return Some(g),
            Err(TryLockError::Poisoned(p)) => return Some(p.into_inner()),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(LOCK_POLL);
            }
            Err(TryLockError::WouldBlock) => return None,
        }
    }
}

/// Sends the close frame if `conn` can be had within `wait` (see [`lock_within`]); otherwise skips
/// it — a writer that has held the lock that long is stuck on a peer that is not reading.
pub(crate) fn send_close_within(conn: &Mutex<Conn>, wait: Duration, code: CloseCode, reason: &str) {
    if let Some(mut c) = lock_within(conn, wait) {
        c.send_close(code, reason);
    }
}

/// How each producer-side [`CloseReason`] closes the WebSocket: the producer can drop this
/// consumer from its own thread — finished, too slow, not drained in time — with the connection's
/// own reader not yet aware anything happened, so the subscribe closer is the only place that can
/// send the frame before the raw shutdown that same closer performs.
pub(crate) fn close_frame_for_reason(reason: CloseReason) -> (CloseCode, &'static str) {
    match reason {
        CloseReason::PublisherFinished => (CloseCode::Normal, "producer finished"),
        CloseReason::SlowConsumer => (CloseCode::Policy, "too slow to keep up"),
        CloseReason::DrainTimeout => (CloseCode::Policy, "did not drain in time"),
        CloseReason::PeerGone | CloseReason::Detached => (CloseCode::Normal, "closed"),
    }
}

/// The subscribe closer's close-frame half, with the per-reason lock discipline of the module
/// docs. The caller does its own shutdown afterwards (`/ws/<stream_id>` keeps the connection alive
/// across a `PublisherFinished`, T-417, so it does not call this for that reason at all).
pub(crate) fn send_close_for_reason(conn: &Mutex<Conn>, reason: CloseReason) {
    let (code, msg) = close_frame_for_reason(reason);
    match reason {
        // The producer's own real-time thread and the drain watchdog: one try, never a wait,
        // never a blocking write.
        CloseReason::SlowConsumer | CloseReason::DrainTimeout => {
            if let Some(mut c) = lock_within(conn, Duration::ZERO) {
                c.try_send_close(code, msg);
            }
        }
        CloseReason::PeerGone | CloseReason::Detached => {
            send_close_within(conn, Duration::ZERO, code, msg);
        }
        CloseReason::PublisherFinished => send_close_within(conn, CLOSE_LOCK_WAIT, code, msg),
    }
}

/// The egress writer handed to `hk-stream`: every record write takes the [`Conn`]'s lock, which is
/// what lets a ping or a close frame be interleaved between messages and never inside one.
pub(crate) struct ConnSink(pub(crate) Arc<Mutex<Conn>>);

impl Write for ConnSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .write_record(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .sink
            .flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;

    fn loopback_conn() -> (Arc<Mutex<Conn>>, TcpStream) {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let conn = Arc::new(Mutex::new(Conn::started(WsSink::new(
            server,
            hk_stream::StreamKind::Bits,
            Vec::new(),
        ))));
        (conn, client)
    }

    /// Once a close frame has gone out, the session is over: a record written afterwards would
    /// follow a close on the wire, which is a protocol error (and, after a non-blocking partial
    /// close write, garbage). The writer is told so instead.
    #[test]
    fn no_record_is_written_after_the_close_frame() {
        let (conn, _client) = loopback_conn();
        let mut sink = ConnSink(Arc::clone(&conn));
        // A valid length-prefixed egress frame: the stream header, which goes out as the first
        // message. What matters here is only that the sink accepts it before the close.
        let header = hk_stream::StreamHeader::new(
            "test",
            hk_stream::StreamKind::Bits,
            hk_model::ContentClass::Unrestricted,
            "test",
        )
        .to_json_bytes()
        .unwrap();
        let mut frame = u32::try_from(header.len()).unwrap().to_le_bytes().to_vec();
        frame.extend_from_slice(&header);
        assert!(sink.write(&frame).is_ok(), "records flow before a close");
        conn.lock()
            .unwrap()
            .send_close(CloseCode::Policy, "too slow to keep up");
        assert_eq!(
            sink.write(&frame).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe,
            "a record after the close frame must be refused, not written behind it"
        );
    }

    /// A session whose handshake never reached the peer has nothing valid to close on the wire.
    #[test]
    fn a_session_that_never_started_sends_no_frame() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let mut c = Conn::new(WsSink::new(
            server,
            hk_stream::StreamKind::Audio,
            Vec::new(),
        ));
        assert!(!c.is_started());
        c.send_close(CloseCode::Normal, "closed");
        assert!(!c.close_sent, "no frame before the handshake went out");
        c.try_send_close(CloseCode::Normal, "closed");
        assert!(!c.close_sent);
    }
}
