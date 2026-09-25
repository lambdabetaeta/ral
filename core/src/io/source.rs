//! Byte input for a pipeline stage — the mirror of `sink` on the output side.

use std::io::{self, Read};
use std::sync::Arc;

use crate::process::Wake;

/// Where a stage's byte input comes from.
///
/// A `<file` redirect is parked here by `install_stdin_redirect` rather than
/// `dup2`'d onto fd 0, which keeps the cached `startup_stdin_tty` honest:
/// consumers consult it only when this is `Terminal`.  `Empty` denies byte
/// input without denying foreground authority — immediate EOF for readers,
/// `/dev/null` for a child, never `Terminal`'s fall-through to fd 0.
pub enum Source {
    Terminal,
    Empty,
    Reader(SourceReader),
}

/// A pipe or file, and for a stage thread's stdin the wake that ends a
/// blocked read as EOF.  The wake is never handed to a child.
pub struct SourceReader {
    fd: Fd,
    wake: Option<Arc<Wake>>,
}

enum Fd {
    Pipe(os_pipe::PipeReader),
    File(std::fs::File),
}

impl Source {
    /// A duplicate of the pipe or file behind this source; `None` for the
    /// fd-less markers.  Consumers never take a source: a stage's stdin
    /// outlives every command that reads it, as a shared fd 0 would.
    ///
    /// # Errors
    /// Returns `Err` if duplicating the underlying pipe or file fails.
    pub(crate) fn reader(&self) -> io::Result<Option<SourceReader>> {
        match self {
            Self::Terminal | Self::Empty => Ok(None),
            Self::Reader(r) => r.try_clone().map(Some),
        }
    }
}

impl SourceReader {
    pub(crate) fn pipe(r: os_pipe::PipeReader) -> Self {
        Self {
            fd: Fd::Pipe(r),
            wake: None,
        }
    }

    pub(crate) fn file(f: std::fs::File) -> Self {
        Self {
            fd: Fd::File(f),
            wake: None,
        }
    }

    /// This reader as a stage thread's stdin, ended by `wake`; a wake it
    /// already carried belonged to an enclosing stage and is replaced.
    pub(crate) fn interruptible(self, wake: Arc<Wake>) -> Self {
        Self {
            fd: self.fd,
            wake: Some(wake),
        }
    }

    /// # Errors
    /// Returns `Err` if duplicating the pipe or file fails.
    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        let fd = match &self.fd {
            Fd::Pipe(r) => Fd::Pipe(r.try_clone()?),
            Fd::File(f) => Fd::File(f.try_clone()?),
        };
        Ok(Self {
            fd,
            wake: self.wake.clone(),
        })
    }
}

impl Fd {
    fn inner(&mut self) -> &mut dyn Read {
        match self {
            Self::Pipe(r) => r,
            Self::File(f) => f,
        }
    }
}

/// Forwards the bulk reads too: `File`'s own preallocate from its length,
/// where the default doubles the buffer and peaks at twice the payload.
impl Read for Fd {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner().read(buf)
    }

    fn read_to_end(&mut self, buf: &mut Vec<u8>) -> io::Result<usize> {
        self.inner().read_to_end(buf)
    }

    fn read_to_string(&mut self, buf: &mut String) -> io::Result<usize> {
        self.inner().read_to_string(buf)
    }
}

/// The fd read beside its wake.
struct Woken<'a> {
    fd: &'a mut Fd,
    wake: &'a Wake,
}

impl Read for Woken<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        read_interruptible(self.fd, self.wake, buf)
    }
}

impl SourceReader {
    fn with<T>(&mut self, f: impl FnOnce(&mut dyn Read) -> T) -> T {
        match &self.wake {
            Some(wake) => f(&mut Woken {
                fd: &mut self.fd,
                wake,
            }),
            None => f(&mut self.fd),
        }
    }
}

impl Read for SourceReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.with(|r| r.read(buf))
    }

    fn read_to_end(&mut self, buf: &mut Vec<u8>) -> io::Result<usize> {
        self.with(|r| r.read_to_end(buf))
    }

    fn read_to_string(&mut self, buf: &mut String) -> io::Result<usize> {
        self.with(|r| r.read_to_string(buf))
    }
}

/// A fired wake always reads as EOF, never as data.
#[cfg(unix)]
fn read_interruptible(fd: &mut Fd, wake: &Wake, buf: &mut [u8]) -> io::Result<usize> {
    use crate::process::wake::Readiness;
    use std::os::fd::AsRawFd;
    match wake.poll_beside(fd.as_raw_fd(), libc::POLLIN)? {
        Readiness::Fired => Ok(0),
        Readiness::Ready(_) => fd.read(buf),
    }
}

#[cfg(windows)]
fn read_interruptible(fd: &mut Fd, wake: &Wake, buf: &mut [u8]) -> io::Result<usize> {
    use windows_sys::Win32::Foundation::ERROR_OPERATION_ABORTED;
    if wake.is_fired() {
        wake.acknowledge();
        return Ok(0);
    }
    match fd.read(buf) {
        // `CancelSynchronousIo` (from `ThreadStage::interrupt`) aborts
        // whatever `ReadFile` was in flight; only a wake-caused abort is EOF.
        Err(e)
            if e.raw_os_error() == Some(ERROR_OPERATION_ABORTED.cast_signed())
                && wake.is_fired() =>
        {
            wake.acknowledge();
            Ok(0)
        }
        r => r,
    }
}

impl From<SourceReader> for crate::process::StdioSpec {
    fn from(r: SourceReader) -> Self {
        match r.fd {
            Fd::Pipe(r) => Self::from_pipe_reader(r),
            Fd::File(f) => Self::from_file(f),
        }
    }
}

#[cfg(unix)]
impl std::os::unix::io::AsRawFd for Fd {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        match self {
            Self::Pipe(r) => r.as_raw_fd(),
            Self::File(f) => f.as_raw_fd(),
        }
    }
}

#[cfg(windows)]
impl std::os::windows::io::AsRawHandle for Fd {
    fn as_raw_handle(&self) -> std::os::windows::io::RawHandle {
        match self {
            Self::Pipe(r) => r.as_raw_handle(),
            Self::File(f) => f.as_raw_handle(),
        }
    }
}

#[cfg(all(test, unix))]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn a_blocked_read_returns_eof_within_100ms_of_fire() {
        let (r, _w) = crate::process::cloexec_pipe().expect("data pipe");
        let wake = Wake::new().expect("wake");
        let mut reader = SourceReader::pipe(r).interruptible(wake.clone());

        let joiner = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            wake.fire();
        });

        let start = Instant::now();
        let mut buf = [0u8; 8];
        let n = reader.read(&mut buf).expect("read");
        assert_eq!(n, 0);
        assert!(start.elapsed() < Duration::from_millis(100));
        joiner.join().expect("wake thread");
    }

    #[test]
    fn data_written_before_the_wake_is_still_delivered() {
        let (r, mut w) = crate::process::cloexec_pipe().expect("data pipe");
        let wake = Wake::new().expect("wake");
        let mut reader = SourceReader::pipe(r).interruptible(wake.clone());

        use std::io::Write;
        w.write_all(b"hi").expect("write");

        let mut buf = [0u8; 8];
        let n = reader.read(&mut buf).expect("read");
        assert_eq!(&buf[..n], b"hi");

        // The write end is still open, so a second read would block here
        // without the wake — this is what proves the wake, not a closed
        // pipe, produced the EOF below.
        let joiner = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            wake.fire();
        });
        let n = reader.read(&mut buf).expect("read after wake");
        assert_eq!(n, 0);
        joiner.join().expect("wake thread");
        drop(w);
    }

    #[test]
    fn a_file_reads_to_end_into_an_exact_buffer() {
        use std::io::{Seek, Write};
        let mut f = tempfile::tempfile().expect("tempfile");
        f.write_all(&vec![b'x'; 100_000]).expect("write");
        f.rewind().expect("rewind");
        let mut buf = Vec::new();
        SourceReader::file(f)
            .read_to_end(&mut buf)
            .expect("read_to_end");
        assert_eq!((buf.len(), buf.capacity()), (100_000, 100_000));
    }

    #[test]
    fn reader_borrows_rather_than_takes_a_pipe_source() {
        use std::io::Write;
        let (r, mut w) = crate::process::cloexec_pipe().expect("data pipe");
        let source = Source::Reader(SourceReader::pipe(r));
        let mut first = source.reader().expect("reader").expect("some reader");
        let mut second = source.reader().expect("reader").expect("some reader");
        w.write_all(b"ab").expect("write");

        let mut buf = [0u8; 1];
        first
            .read_exact(&mut buf)
            .expect("read from first duplicate");
        assert_eq!(&buf, b"a");
        second
            .read_exact(&mut buf)
            .expect("read from second duplicate");
        assert_eq!(&buf, b"b");
    }
}
