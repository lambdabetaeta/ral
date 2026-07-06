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
    /// Protocol descriptors are capabilities: `F_DUPFD_CLOEXEC` keeps the
    /// duplicate close-on-exec, exactly as `std`'s own `UnixStream::pair`
    /// and `try_clone` do — a raw `dup` would hand the duplicate to every
    /// child a turn spawns.
    pub fn try_clone(&self) -> io::Result<Self> {
        use std::os::unix::io::AsRawFd;
        use std::os::unix::io::FromRawFd;
        let fd = self.stream.as_raw_fd();
        let dup_fd = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if dup_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            stream: unsafe { UnixStream::from_raw_fd(dup_fd) },
        })
    }

    /// Expose the raw fd for passing to a child process via `pre_exec`.
    pub fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.stream.as_raw_fd()
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
