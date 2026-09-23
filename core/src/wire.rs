//! Duplex framed channel: one connection is one session, carrying
//! length-prefixed JSON frames (`subprocess_codec`) in either direction.
//!
//! [`WireStream`] is `UnixStream` on Unix and `TcpStream` on Windows, yet
//! `vm-manager` hands back `AF_VSOCK` and `AF_HYPERV` sockets: the types are
//! borrowed only as std's owner of a *connected stream socket*, every operation
//! used here being one syscall in any family.  Nothing here or above asks such
//! a stream for an address — the one question that would expose the pretence.

use crate::protocol::Frame;
use std::io;
use std::sync::Mutex;

/// The connected stream socket a [`WireChannel`] frames over: std's owner
/// type, not a statement about the address family.
#[cfg(unix)]
pub type WireStream = std::os::unix::net::UnixStream;

/// The connected stream socket a [`WireChannel`] frames over: std's owner
/// type, not a statement about the address family.
#[cfg(windows)]
pub type WireStream = std::net::TcpStream;

pub struct WireChannel {
    stream: WireStream,
}

impl WireChannel {
    /// Two ends of one connection, for a front-end and an engine that share a
    /// process tree.
    ///
    /// # Errors
    /// Returns the socket error if the pair cannot be made.
    #[cfg(all(unix, test))]
    pub(crate) fn pair() -> io::Result<(Self, Self)> {
        let (a, b) = crate::process::cloexec_socketpair()?;
        Ok((Self { stream: a }, Self { stream: b }))
    }

    /// Two ends of one connection, for a front-end and an engine that share a
    /// process tree.
    ///
    /// Windows has no `socketpair(2)`, so the pair is a loopback connection
    /// made and accepted here — genuinely TCP, hence the one place Nagle is
    /// worth turning off: a frame protocol wants each frame on the wire now.
    ///
    /// # Errors
    /// Returns the socket error if the loopback pair cannot be established.
    #[cfg(all(windows, test))]
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:wire-pair-windows] the Windows twin of `process::cloexec_socketpair` ([silent:cloexec-socketpair]): the same one-connection-for-a-process-tree, spelled as a loopback bind-connect-accept because Windows has no socketpair(2). No outside name is reached — the port is ephemeral and the peer is this process — so it is silent for the same reason its Unix hemisphere is."
    )]
    pub(crate) fn pair() -> io::Result<(Self, Self)> {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        let a = std::net::TcpStream::connect(listener.local_addr()?)?;
        let (b, _peer) = listener.accept()?;
        a.set_nodelay(true)?;
        b.set_nodelay(true)?;
        Ok((Self { stream: a }, Self { stream: b }))
    }

    /// Frame over an already-connected stream: one end of a [`Self::pair`], or
    /// the virtual-socket connection into a guest VM — a backend's own
    /// `OwnedFd` or `OwnedSocket` converts straight in.
    pub(crate) fn from_stream(stream: impl Into<WireStream>) -> Self {
        Self {
            stream: stream.into(),
        }
    }

    /// Read one frame.
    ///
    /// # Errors
    /// Returns the read or decode error.  `Ok(None)` is a clean EOF: the peer
    /// closed between frames.
    pub(crate) fn read_frame(&mut self) -> io::Result<Option<Frame>> {
        crate::subprocess_codec::read_frame(&mut self.stream)
    }

    /// Write one frame.
    ///
    /// # Errors
    /// Returns the encode or write error.
    pub(crate) fn write_frame(&mut self, frame: &Frame) -> io::Result<()> {
        crate::subprocess_codec::write_frame(&mut self.stream, frame)
    }

    /// Duplicate the socket so one channel reads while another writes the same
    /// connection.  The duplicate is uninheritable on both platforms, so no
    /// process a run spawns receives it.
    ///
    /// # Errors
    /// Returns the duplication error.
    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            stream: self.stream.try_clone()?,
        })
    }

    /// Wait until a frame is readable, or `timeout` passes; `None` waits
    /// indefinitely.  A hangup counts as readable, so `true` promises
    /// `read_frame` returns promptly — frame, `None`, or error.  This is how
    /// `engine_session` notices a silent front-end; the deadline governs frame
    /// *arrival*, a peer dying mid-frame erring the read rather than stalling.
    ///
    /// # Errors
    /// Returns the `poll(2)` error, with `EINTR` retried against the
    /// caller's own timeout.
    #[cfg(unix)]
    pub(crate) fn poll_readable(&self, timeout: Option<std::time::Duration>) -> io::Result<bool> {
        use std::os::unix::io::AsRawFd;
        let deadline = timeout.map(|t| std::time::Instant::now() + t);
        loop {
            let wait_ms: libc::c_int = match deadline {
                None => -1,
                Some(d) => {
                    let left = d.saturating_duration_since(std::time::Instant::now());
                    libc::c_int::try_from(left.as_millis()).unwrap_or(libc::c_int::MAX)
                }
            };
            let mut fds = libc::pollfd {
                fd: self.stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: `fds` is one fully initialised pollfd, and `1` is its
            // count; `poll` writes only `revents`.
            let rc = unsafe { libc::poll(&raw mut fds, 1, wait_ms) };
            if rc < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            return Ok(rc > 0);
        }
    }

    /// Wait until a frame is readable, or `timeout` passes; `None` waits
    /// indefinitely — the Unix twin above states the contract.  `WSAPoll` is
    /// Winsock's `poll(2)` and `POLLRDNORM` its `POLLIN`; with no interrupting
    /// signals there is no `EINTR`, so that loop collapses to one call.
    ///
    /// # Errors
    /// Returns the `WSAPoll` error.
    ///
    /// # Panics
    /// Never: Winsock's `SOCKET` *is* a pointer-sized value, and `RawSocket` is
    /// the widest integer that could hold one on any Windows.
    #[cfg(windows)]
    #[allow(
        dead_code,
        reason = "shape, not use: the only caller is `engine`, `cfg(unix)` today"
    )]
    pub(crate) fn poll_readable(&self, timeout: Option<std::time::Duration>) -> io::Result<bool> {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::{
            POLLRDNORM, SOCKET_ERROR, WSAPOLLFD, WSAPoll,
        };

        let wait_ms: i32 = match timeout {
            None => -1,
            Some(t) => i32::try_from(t.as_millis().min(i32::MAX as u128)).expect("clamped to i32"),
        };
        let mut fds = WSAPOLLFD {
            fd: usize::try_from(self.stream.as_raw_socket())
                .expect("a SOCKET is pointer-sized on every Windows"),
            events: POLLRDNORM,
            revents: 0,
        };
        // SAFETY: `fds` is one fully initialised WSAPOLLFD and `1` is its
        // count; `WSAPoll` writes only `revents`.
        let rc = unsafe { WSAPoll(&raw mut fds, 1, wait_ms) };
        if rc == SOCKET_ERROR {
            return Err(io::Error::last_os_error());
        }
        Ok(rc > 0)
    }

    /// Shut the socket down both ways, waking any thread parked in
    /// `read_frame` or [`Self::poll_readable`] — clones included, since they
    /// share the one underlying connection.
    pub(crate) fn shutdown(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    /// Bound how long a single `write_frame` may block on this socket.
    ///
    /// `SO_SNDTIMEO` lives on the shared file description, not the file
    /// descriptor, so every clone of this channel inherits the same deadline —
    /// one call at construction reaches every writer — and it governs writes
    /// alone: `read_frame` and [`Self::poll_readable`] are untouched. A write
    /// that stalls past `d` becomes an `io::Error`, which the framing law at
    /// the call sites above converts into connection death, the same as a
    /// severed pipe. The invariant this buys: no `write_frame` blocks past the
    /// peer's patience deadline.
    ///
    /// # Errors
    /// Returns the socket error if the timeout cannot be set.
    pub(crate) fn set_write_deadline(&self, d: std::time::Duration) -> io::Result<()> {
        self.stream.set_write_timeout(Some(d))
    }
}

/// The one write door under the severance law: write `frame`, and if the
/// write fails, record it through `record` and shut the channel down before
/// the lock is released.
///
/// Both ends of the protocol pass through here — the front-end's
/// `write_through` records a `Severed` cause, the engine's `engine_write`
/// raises its fault flag — so the ordering the law states is enforced once
/// rather than restated at each door. What it buys: nothing can append a
/// fresh frame after a truncated one, and no window exists in which the
/// record still calls an already-shut socket healthy.
///
/// Deliberately outside [`LockExt`](crate::sync::LockExt)'s recover-poison
/// policy: a panic between `write_frame` and `shutdown` can leave a partial
/// frame on the socket that neither `record` nor a shutdown has sealed off,
/// and letting the next writer resume into that torn stream is worse than
/// the panic propagating; a poisoned channel must stay poisoned.
///
/// # Errors
/// Returns the write failure, so a caller that must react further — the
/// heartbeat's `Ping` arm breaking its loop — can, without severing again.
#[allow(
    clippy::disallowed_methods,
    reason = "the wire writer: a panic between write_frame and shutdown leaves a partial frame on the socket that neither the severance record nor shutdown has sealed off, so a recovered guard would append the next frame onto a torn one"
)]
pub(crate) fn write_or_sever(
    ch: &Mutex<WireChannel>,
    frame: &Frame,
    record: impl FnOnce(&io::Error),
) -> io::Result<()> {
    let mut guard = ch.lock().unwrap();
    let outcome = guard.write_frame(frame);
    if let Err(e) = &outcome {
        record(e);
        guard.shutdown();
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Control, DispatchId};

    #[test]
    fn round_trip_control() {
        let (mut a, mut b) = WireChannel::pair().unwrap();
        let frame = Frame::Control(Control::Cancel(DispatchId(7)));
        a.write_frame(&frame).unwrap();
        let got = b.read_frame().unwrap().unwrap();
        assert_eq!(got, frame);
    }

    #[test]
    fn eof_returns_none() {
        let (mut a, b) = WireChannel::pair().unwrap();
        drop(b);
        let got = a.read_frame().unwrap();
        assert!(got.is_none());
    }

    /// Silence is not readable and a frame in flight is — the difference the
    /// engine's silence deadline rests on, pinned here rather than left to the
    /// transport's integration tests.
    #[test]
    fn poll_readable_tells_silence_from_traffic() {
        let (mut a, b) = WireChannel::pair().unwrap();
        assert!(
            !b.poll_readable(Some(std::time::Duration::from_millis(50)))
                .unwrap(),
            "an idle channel is not readable"
        );
        a.write_frame(&Frame::Ping(1)).unwrap();
        assert!(
            b.poll_readable(Some(std::time::Duration::from_secs(5)))
                .unwrap(),
            "a channel carrying a frame is readable"
        );
    }
}
