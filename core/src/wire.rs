//! Duplex framed channel: one connection is one session, carrying
//! length-prefixed JSON frames (`subprocess_codec`) in either direction.
//!
//! [`WireStream`] is `UnixStream` on Unix and `TcpStream` on Windows, yet
//! `vm-manager` hands back `AF_VSOCK` and `AF_HYPERV` sockets: the types are
//! borrowed only as std's owner of a *connected stream socket*, every operation
//! used here being one syscall in any family.  Nothing here or above asks such
//! a stream for an address — the one question that would expose the pretence.

use crate::transport::Frame;
use std::io;

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
    #[cfg(unix)]
    pub fn pair() -> io::Result<(Self, Self)> {
        let (a, b) = WireStream::pair()?;
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
    /// the virtual-socket connection into a guest VM — a backend's own
    /// `OwnedFd` or `OwnedSocket` converts straight in.
    pub fn from_stream(stream: impl Into<WireStream>) -> Self {
        Self {
            stream: stream.into(),
        }
    }

    /// Read one frame.
    ///
    /// # Errors
    /// Returns the read or decode error.  `Ok(None)` is a clean EOF: the peer
    /// closed between frames.
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

    /// Duplicate the socket so one channel reads while another writes the same
    /// connection.  The duplicate is uninheritable on both platforms, so no
    /// process a run spawns receives it.
    ///
    /// # Errors
    /// Returns the duplication error.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            stream: self.stream.try_clone()?,
        })
    }

    /// The raw fd, for `pre_exec` to place on fd 3 in the engine child
    /// ([`WireTransport::new`](crate::transport::WireTransport::new)).  Unix
    /// only: a Windows engine is a guest, adopted through [`Self::from_stream`].
    #[cfg(unix)]
    pub fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        use std::os::unix::io::AsRawFd;
        self.stream.as_raw_fd()
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
    pub fn poll_readable(&self, timeout: Option<std::time::Duration>) -> io::Result<bool> {
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

    /// Shut the socket down both ways, waking any thread parked in
    /// `read_frame` or [`Self::poll_readable`] — clones included, since they
    /// share the one underlying connection.
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
