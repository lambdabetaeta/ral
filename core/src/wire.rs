//! Duplex framed channel over the host's own local stream socket.
//!
//! One `WireChannel` is one connection, and the connection is the session:
//! length-prefixed JSON frames ([`crate::subprocess_codec`]) in either
//! direction, over a socket this module never has to know the provenance of.
//!
//! # The stream type is a wrapper, not a claim about the address family
//!
//! [`WireStream`] is `UnixStream` on Unix and `TcpStream` on Windows, and
//! neither name says what the socket underneath actually is.  A `UnixStream`
//! built from an `AF_VSOCK` descriptor — what `vm-manager`'s
//! Virtualization.framework backend hands back from a booted guest — is not a
//! Unix-domain socket, and a `TcpStream` built from an `AF_HYPERV` socket —
//! what its Hyper-V backend hands back — is not TCP.  Both types are used
//! here purely as std's ready-made owner of a *connected stream socket*: read,
//! write, duplicate, shut down.  Each of those is the same syscall whatever
//! the family, which is what makes the borrowing sound rather than merely
//! convenient.
//!
//! The one operation that would expose the pretence is asking a stream for its
//! own or its peer's address, and nothing here or upstream of here ever does.
//! The only place a family-specific option is set is [`WireChannel::pair`]'s
//! loopback socket, which really is TCP.

use crate::transport::Frame;
use std::io;

/// The connected stream socket a [`WireChannel`] frames over.
///
/// See the module docs: this is std's owner type for a stream socket, not a
/// statement about the address family — under a real guest it is an
/// `AF_VSOCK` connection wearing this type.
#[cfg(unix)]
pub type WireStream = std::os::unix::net::UnixStream;

/// The connected stream socket a [`WireChannel`] frames over.
///
/// See the module docs: this is std's owner type for a stream socket, not a
/// statement about the address family — under a real guest it is the
/// `AF_HYPERV` control-plane socket, adopted through
/// `From<OwnedSocket>`, and never a TCP connection.
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
    #[cfg(unix)]
    pub fn pair() -> io::Result<(Self, Self)> {
        let (a, b) = WireStream::pair()?;
        Ok((Self { stream: a }, Self { stream: b }))
    }

    /// Two ends of one connection, for a front-end and an engine that share a
    /// process tree.
    ///
    /// Windows has no `socketpair(2)`, so the pair is a loopback connection
    /// made and accepted here.  Nagle is turned off because a frame protocol
    /// wants each frame on the wire now rather than coalesced with a next one
    /// that may never come; this is the one socket in the module that really
    /// is TCP, so it is the one place a TCP-level option means anything.
    ///
    /// # Errors
    /// Returns the socket error if the loopback pair cannot be established.
    #[cfg(windows)]
    pub fn pair() -> io::Result<(Self, Self)> {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        let a = std::net::TcpStream::connect(listener.local_addr()?)?;
        let (b, _peer) = listener.accept()?;
        a.set_nodelay(true)?;
        b.set_nodelay(true)?;
        Ok((Self { stream: a }, Self { stream: b }))
    }

    /// Frame over an already-connected stream: one end of a [`Self::pair`], or
    /// the virtual-socket connection into a guest VM.
    ///
    /// Generic over `Into<WireStream>` so a backend's own owned handle — an
    /// `OwnedFd` on Unix, an `OwnedSocket` on Windows — can be adopted
    /// directly, without the caller naming the platform's stream type.
    pub fn from_stream(stream: impl Into<WireStream>) -> Self {
        Self {
            stream: stream.into(),
        }
    }

    /// Read one frame.
    ///
    /// # Errors
    /// Returns the read or decode error.  `Ok(None)` is a clean EOF: the peer
    /// closed the connection between frames.
    pub fn read_frame(&mut self) -> io::Result<Option<Frame>> {
        crate::subprocess_codec::read_frame(&mut self.stream)
    }

    /// Write one frame.
    ///
    /// # Errors
    /// Returns the encode or write error.
    pub fn write_frame(&mut self, frame: &Frame) -> io::Result<()> {
        crate::subprocess_codec::write_frame(&mut self.stream, frame)
    }

    /// Duplicate the underlying socket so one `WireChannel` can be read from
    /// while another writes to the same connection.
    ///
    /// Both platforms' `try_clone` keeps the duplicate out of a child's
    /// inheritance — close-on-exec on Unix, non-inheritable on Windows — so it
    /// is never handed to a process a run spawns.
    ///
    /// # Errors
    /// Returns the duplication error.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            stream: self.stream.try_clone()?,
        })
    }

    /// Expose the raw fd for passing to a child process via `pre_exec`.
    ///
    /// Unix only, and so is the one thing it serves: handing the engine end of
    /// a socketpair to a same-host child on fd 3
    /// ([`WireTransport::new`](crate::transport::WireTransport::new)).  A
    /// Windows front-end has no such child — its engine is a guest, reached
    /// through [`Self::from_stream`] — so there is no descriptor to inherit.
    #[cfg(unix)]
    pub fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        use std::os::unix::io::AsRawFd;
        self.stream.as_raw_fd()
    }

    /// Wait until the channel has a frame to read, or `timeout` passes.
    /// `None` waits indefinitely.  Returns whether the channel is readable —
    /// which includes a peer hangup, so a `true` answer means `read_frame`
    /// will return promptly, with a frame, `None`, or an error.
    ///
    /// This is the liveness primitive: a reader that must notice a silent
    /// peer parks here on a tick instead of blocking forever inside
    /// `read_frame`.  The deadline governs frame *arrival*; once bytes are
    /// flowing, `read_frame` is entitled to block for the frame's remainder,
    /// because a peer that dies mid-frame tears the connection and errs the
    /// read rather than stalling it.
    ///
    /// # Errors
    /// Returns the `poll(2)` error, with `EINTR` retried against the
    /// caller's own timeout.
    #[cfg(unix)]
    pub fn poll_readable(&self, timeout: Option<std::time::Duration>) -> io::Result<bool> {
        use std::os::unix::io::AsRawFd;
        let deadline = timeout.map(|t| std::time::Instant::now() + t);
        loop {
            let wait_ms: libc::c_int = match deadline {
                None => -1,
                Some(d) => {
                    let left = d.saturating_duration_since(std::time::Instant::now());
                    libc::c_int::try_from(left.as_millis().min(i32::MAX as u128))
                        .expect("clamped to i32::MAX")
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

    /// Wait until the channel has a frame to read, or `timeout` passes.
    /// `None` waits indefinitely.
    ///
    /// The Unix twin above carries the law this upholds.  `WSAPoll` is
    /// Winsock's `poll(2)` and `POLLRDNORM` its `POLLIN`; a hangup surfaces
    /// the same way, as a readable socket whose next read returns zero bytes.
    /// There is no `EINTR` to retry — Winsock has no interrupting signals —
    /// so the loop the Unix side needs collapses to one call here.
    ///
    /// # Errors
    /// Returns the `WSAPoll` error.
    ///
    /// # Panics
    /// Panics if this process's socket handle does not fit a pointer — which is
    /// to say never: Winsock's own `SOCKET` *is* a pointer-sized value, and
    /// `RawSocket` is the widest integer that could hold one on any Windows.
    #[cfg(windows)]
    pub fn poll_readable(&self, timeout: Option<std::time::Duration>) -> io::Result<bool> {
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

    /// Shut the socket down in both directions, waking any thread parked in
    /// `read_frame` or [`Self::poll_readable`] on this socket — including
    /// through clones, which share the one underlying connection.
    pub fn shutdown(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{Control, Winsize};

    #[test]
    fn round_trip_control() {
        let (mut a, mut b) = WireChannel::pair().unwrap();
        let frame = Frame::Control(Control::Resize(Winsize { rows: 24, cols: 80 }));
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

    /// The liveness primitive answers both ways on either platform: silence
    /// is not readable, and a frame in flight is.  The heartbeat ticker's
    /// whole deadline discipline rests on that difference, so it is pinned
    /// here rather than left to the transport's integration tests.
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
