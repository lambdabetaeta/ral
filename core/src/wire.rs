//! Duplex framed channel over a Unix socketpair for front-end ↔ engine IPC.

use crate::transport::Frame;
use std::io;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;

pub struct WireChannel {
    stream: UnixStream,
}

impl WireChannel {
    pub fn pair() -> io::Result<(Self, Self)> {
        let (a, b) = UnixStream::pair()?;
        Ok((Self { stream: a }, Self { stream: b }))
    }

    pub fn from_stream(stream: UnixStream) -> Self {
        Self { stream }
    }

    pub fn read_frame(&mut self) -> io::Result<Option<Frame>> {
        crate::subprocess_codec::read_frame(&mut self.stream)
    }

    pub fn write_frame(&mut self, frame: &Frame) -> io::Result<()> {
        crate::subprocess_codec::write_frame(&mut self.stream, frame)
    }

    /// Duplicate the underlying fd so one `WireChannel` can be read
    /// from while another writes to the same socket.
    ///
    /// `UnixStream::try_clone` keeps the duplicate close-on-exec, so it is
    /// never inherited by a child a turn spawns.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            stream: self.stream.try_clone()?,
        })
    }

    /// Expose the raw fd for passing to a child process via `pre_exec`.
    pub fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
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
    pub fn poll_readable(&self, timeout: Option<std::time::Duration>) -> io::Result<bool> {
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

    /// Shut the socket down in both directions, waking any thread parked in
    /// `read_frame` or [`Self::poll_readable`] on this fd — including through
    /// clones, which share the one underlying socket.
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
}
