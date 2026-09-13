//! Unix-domain-socket and TCP listeners (docs/stream-contract.md §2). Each accepted connection
//! becomes a consumer of one publisher. Listeners are unauthenticated: bind TCP to loopback
//! unless the network is trusted (C24 pitfall; authentication is an open issue).
//!
//! The accept thread blocks in `poll(2)` on the listening socket and a private wake socket, so
//! shutdown never depends on being able to connect to the listener (e.g. after its socket file
//! was removed) and an idle listener costs nothing.

use std::fs;
use std::io;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::publisher::PublisherHandle;

/// Where a listener is bound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListenAddr {
    /// Unix-domain socket path.
    Unix(PathBuf),
    /// TCP address (with the real port when bound to port 0).
    Tcp(SocketAddr),
}

/// An accept loop feeding one publisher. Dropping it stops accepting (existing consumers stay).
pub struct Listener {
    addr: ListenAddr,
    wake: Option<UnixStream>,
    thread: Option<JoinHandle<()>>,
}

/// What `poll` reported.
enum Ready {
    Accept,
    Stop,
    Retry,
}

fn wait_ready(listener: RawFd, wake: RawFd) -> Ready {
    let mut fds = [
        libc::pollfd {
            fd: listener,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // SAFETY: `fds` is a valid array of two initialised pollfd structs for the whole call.
    let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
    if rc < 0 {
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            thread::sleep(Duration::from_millis(10));
        }
        return Ready::Retry;
    }
    if fds[1].revents != 0 {
        Ready::Stop
    } else if fds[0].revents != 0 {
        Ready::Accept
    } else {
        Ready::Retry
    }
}

impl Listener {
    /// Binds a Unix-domain socket (a stale socket file at `path` is replaced).
    pub fn bind_uds(path: impl AsRef<Path>, handle: PublisherHandle) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Ok(meta) = fs::symlink_metadata(&path) {
            if meta.file_type().is_socket() {
                fs::remove_file(&path)?;
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a socket", path.display()),
                ));
            }
        }
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        let label = format!("uds:{}", path.display());
        Self::spawn(
            ListenAddr::Unix(path),
            "hk-stream-accept-uds",
            move |wake| {
                loop {
                    match wait_ready(listener.as_raw_fd(), wake) {
                        Ready::Stop => break,
                        Ready::Retry => {}
                        Ready::Accept => match listener.accept() {
                            Ok((s, _)) => attach_uds(&handle, &label, s),
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                            Err(_) => thread::sleep(Duration::from_millis(10)),
                        },
                    }
                }
            },
        )
    }

    /// Binds a TCP listener. Use port 0 for an ephemeral port ([`Listener::addr`] has the real one).
    pub fn bind_tcp(addr: impl ToSocketAddrs, handle: PublisherHandle) -> io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let local = listener.local_addr()?;
        Self::spawn(
            ListenAddr::Tcp(local),
            "hk-stream-accept-tcp",
            move |wake| {
                loop {
                    match wait_ready(listener.as_raw_fd(), wake) {
                        Ready::Stop => break,
                        Ready::Retry => {}
                        Ready::Accept => match listener.accept() {
                            Ok((s, _)) => attach_tcp(&handle, s),
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                            Err(_) => thread::sleep(Duration::from_millis(10)),
                        },
                    }
                }
            },
        )
    }

    fn spawn(
        addr: ListenAddr,
        name: &str,
        run: impl FnOnce(RawFd) + Send + 'static,
    ) -> io::Result<Self> {
        let (wake_tx, wake_rx) = UnixStream::pair()?;
        let thread = thread::Builder::new().name(name.into()).spawn(move || {
            run(wake_rx.as_raw_fd());
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

fn attach_uds(handle: &PublisherHandle, label: &str, s: UnixStream) {
    // Accepted sockets may inherit the listener's non-blocking flag (BSD); writers block.
    if s.set_nonblocking(false).is_err() {
        return;
    }
    let Ok(clone) = s.try_clone() else { return };
    // An error (e.g. the stream finished) drops the socket, which closes the connection.
    let _ = handle.subscribe(
        label,
        Box::new(s),
        Box::new(move |_| {
            let _ = clone.shutdown(Shutdown::Both);
        }),
    );
}

fn attach_tcp(handle: &PublisherHandle, s: TcpStream) {
    if s.set_nonblocking(false).is_err() {
        return;
    }
    let _ = s.set_nodelay(true);
    let label = s
        .peer_addr()
        .map_or_else(|_| "tcp".to_owned(), |a| format!("tcp:{a}"));
    let Ok(clone) = s.try_clone() else { return };
    let _ = handle.subscribe(
        label,
        Box::new(s),
        Box::new(move |_| {
            let _ = clone.shutdown(Shutdown::Both);
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{Publisher, PublisherConfig, StreamHeader, StreamKind};
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
