//! Unix-domain-socket and TCP listeners (docs/stream-contract.md §2). Each accepted connection
//! becomes a consumer of one publisher.
//!
//! - **Local-only classes.** `own-key-decrypted` streams are refused on TCP
//!   ([`crate::gate::remote_transport_permitted`]); serve them on a Unix socket. `bind_tcp`
//!   refuses up front, and every accepted connection is subscribed as its concrete socket type
//!   (`UnixStream` is [`Locality::Local`](crate::Locality), `TcpStream` is `Remote`), so
//!   the publisher enforces the rule again per consumer.
//! - **Unix sockets are created mode 0600** without a window: the socket is bound inside a fresh
//!   0700 directory, chmodded, then renamed into place. A path held by a *live* listener is
//!   refused (probe-connect); a stale socket file is replaced.
//! - **TCP is unauthenticated.** Bind to loopback unless the network is trusted (C24 pitfall;
//!   authentication is an open issue).
//! - **Hang-ups are reaped while idle.** The accept thread also polls every connection it
//!   accepted: consumers never send, so readability (EOF or unexpected bytes) closes the consumer
//!   even when nothing is being published. The publisher's `max_consumers` caps open connections.
//!
//! The accept thread blocks in `poll(2)` on the listening socket, a private wake socket and the
//! accepted connections, so shutdown never depends on connecting to the listener and an idle
//! listener costs nothing.

use std::fs;
use std::io;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::gate;
use super::publisher::{ConsumerId, PublisherHandle};

/// Where a listener is bound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListenAddr {
    /// Unix-domain socket path.
    Unix(PathBuf),
    /// TCP address (with the real port when bound to port 0).
    Tcp(SocketAddr),
}

/// An accept loop feeding one publisher. Dropping it stops accepting and stops watching for
/// hang-ups (existing consumers stay until their next write fails or the stream finishes).
pub struct Listener {
    addr: ListenAddr,
    wake: Option<UnixStream>,
    thread: Option<JoinHandle<()>>,
}

/// Accepts one pending connection: `Ok(Some)` attached, `Ok(None)` refused (dropped),
/// `Err(WouldBlock)` none pending.
type AcceptFn = Box<dyn FnMut() -> io::Result<Option<(ConsumerId, OwnedFd)>> + Send>;

fn serve(listener: RawFd, wake: RawFd, handle: &PublisherHandle, mut accept: AcceptFn) {
    let mut conns: Vec<(ConsumerId, OwnedFd)> = Vec::new();
    loop {
        let mut fds: Vec<libc::pollfd> = Vec::with_capacity(2 + conns.len());
        for fd in [listener, wake]
            .into_iter()
            .chain(conns.iter().map(|(_, fd)| fd.as_raw_fd()))
        {
            fds.push(libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            });
        }
        // SAFETY: `fds` is a valid, initialised pollfd array of the given length.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if rc < 0 {
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                thread::sleep(Duration::from_millis(10));
            }
            continue;
        }
        if fds[1].revents != 0 {
            break;
        }
        // Consumers never send: readable means hung up (EOF) or misbehaving. Either way, close.
        let mut i = 2;
        conns.retain(|(id, _)| {
            let ready = fds[i].revents != 0;
            i += 1;
            if ready {
                handle.peer_gone(*id);
            }
            !ready
        });
        if fds[0].revents != 0 {
            loop {
                match accept() {
                    Ok(Some(conn)) => conns.push(conn),
                    Ok(None) => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => {
                        thread::sleep(Duration::from_millis(10));
                        break;
                    }
                }
            }
        }
    }
}

static BIND_SEQ: AtomicU64 = AtomicU64::new(0);

/// Binds `path` as a mode-0600 Unix socket with no window where it is more permissive.
fn bind_private_uds(path: &Path) -> io::Result<UnixListener> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if !meta.file_type().is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ));
        }
        if UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("{} is served by a live listener", path.display()),
            ));
        }
        // Stale: replaced by the rename below.
    }
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let staging = parent.join(format!(
        ".hks{}.{}",
        std::process::id(),
        BIND_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    fs::DirBuilder::new().mode(0o700).create(&staging)?;
    let result = (|| {
        let tmp = staging.join("s");
        let listener = UnixListener::bind(&tmp)?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        fs::rename(&tmp, path)?;
        Ok(listener)
    })();
    let _ = fs::remove_dir_all(&staging);
    result
}

impl Listener {
    /// Binds a Unix-domain socket (mode 0600). Refuses a path served by a live listener; a
    /// stale socket file is replaced.
    pub fn bind_uds(path: impl AsRef<Path>, handle: PublisherHandle) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let listener = bind_private_uds(&path)?;
        listener.set_nonblocking(true)?;
        let label = format!("uds:{}", path.display());
        let listen_fd = listener.as_raw_fd();
        let h = handle.clone();
        let accept: AcceptFn = Box::new(move || {
            let (s, _) = listener.accept()?;
            Ok(attach_uds(&h, &label, s))
        });
        Self::spawn(
            ListenAddr::Unix(path),
            "hk-stream-accept-uds",
            listen_fd,
            handle,
            accept,
        )
    }

    /// Binds a TCP listener. Use port 0 for an ephemeral port ([`Listener::addr`] has the real
    /// one). Refused for `own-key-decrypted` streams, which are local-only.
    pub fn bind_tcp(addr: impl ToSocketAddrs, handle: PublisherHandle) -> io::Result<Self> {
        if !gate::remote_transport_permitted(handle.content_class()) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "own-key-decrypted streams are local-only: serve them on a Unix socket",
            ));
        }
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let local = listener.local_addr()?;
        let listen_fd = listener.as_raw_fd();
        let h = handle.clone();
        let accept: AcceptFn = Box::new(move || {
            let (s, _) = listener.accept()?;
            Ok(attach_tcp(&h, s))
        });
        Self::spawn(
            ListenAddr::Tcp(local),
            "hk-stream-accept-tcp",
            listen_fd,
            handle,
            accept,
        )
    }

    /// Starts the accept thread. `listen_fd` belongs to the listener owned by `accept`, which
    /// lives inside the thread for as long as `serve` runs.
    fn spawn(
        addr: ListenAddr,
        name: &str,
        listen_fd: RawFd,
        handle: PublisherHandle,
        accept: AcceptFn,
    ) -> io::Result<Self> {
        let (wake_tx, wake_rx) = UnixStream::pair()?;
        let thread = thread::Builder::new().name(name.into()).spawn(move || {
            serve(listen_fd, wake_rx.as_raw_fd(), &handle, accept);
            drop(wake_rx);
        })?;
        Ok(Self {
            addr,
            wake: Some(wake_tx),
            thread: Some(thread),
        })
    }

    /// The bound address.
    pub fn addr(&self) -> &ListenAddr {
        &self.addr
    }

    /// Stops accepting and joins the accept thread; removes the socket file.
    pub fn shutdown(&mut self) {
        // Closing our end makes the accept thread's wake socket readable (EOF).
        drop(self.wake.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
            if let ListenAddr::Unix(path) = &self.addr {
                let _ = fs::remove_file(path);
            }
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn attach_uds(
    handle: &PublisherHandle,
    label: &str,
    s: UnixStream,
) -> Option<(ConsumerId, OwnedFd)> {
    // Accepted sockets may inherit the listener's non-blocking flag (BSD); writers block.
    s.set_nonblocking(false).ok()?;
    let closer = s.try_clone().ok()?;
    let watch = s.try_clone().ok()?;
    // An error (stream finished, consumer cap) drops the socket, which closes the connection.
    let id = handle
        .subscribe(
            label,
            s,
            Box::new(move |_| {
                let _ = closer.shutdown(Shutdown::Both);
            }),
        )
        .ok()?;
    Some((id, OwnedFd::from(watch)))
}

fn attach_tcp(handle: &PublisherHandle, s: TcpStream) -> Option<(ConsumerId, OwnedFd)> {
    s.set_nonblocking(false).ok()?;
    let _ = s.set_nodelay(true);
    let label = s
        .peer_addr()
        .map_or_else(|_| "tcp".to_owned(), |a| format!("tcp:{a}"));
    let closer = s.try_clone().ok()?;
    let watch = s.try_clone().ok()?;
    // A TcpStream is `Locality::Remote`: refused for own-key streams even if `bind_tcp` was
    // bypassed.
    let id = handle
        .subscribe(
            label,
            s,
            Box::new(move |_| {
                let _ = closer.shutdown(Shutdown::Both);
            }),
        )
        .ok()?;
    Some((id, OwnedFd::from(watch)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Publisher, PublisherConfig, StreamHeader, StreamKind};
    use hk_model::ContentClass;

    #[test]
    fn shutdown_does_not_need_the_socket_file() {
        let dir = std::env::temp_dir().join(format!("hkls{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let publisher = Publisher::new(
            StreamHeader::new("m", StreamKind::Messages, ContentClass::Unrestricted, "t"),
            PublisherConfig::default(),
        )
        .unwrap();
        let mut uds = Listener::bind_uds(dir.join("s.sock"), publisher.handle()).unwrap();
        let mut tcp = Listener::bind_tcp("127.0.0.1:0", publisher.handle()).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        uds.shutdown();
        tcp.shutdown();
        // Refusing a non-socket path.
        let file = std::env::temp_dir().join(format!("hklf{}", std::process::id()));
        fs::write(&file, b"x").unwrap();
        assert!(Listener::bind_uds(&file, publisher.handle()).is_err());
        let _ = fs::remove_file(file);
    }
}
