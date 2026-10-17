//! Byte input for a pipeline stage — the mirror of `sink` on the output side.

use std::io::{self, Read};

use crate::process::Scope;

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

/// A pipe or file, and for a stage thread's stdin the scope whose cause ends
/// a blocked read as EOF.  The scope is never handed to a child.
pub struct SourceReader {
    fd: Fd,
    scope: Option<Scope>,
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
            scope: None,
        }
    }

    pub(crate) fn file(f: std::fs::File) -> Self {
        Self {
            fd: Fd::File(f),
            scope: None,
        }
    }

    /// This reader as one that ends as EOF once `scope` carries a cause; a
    /// scope it already carried belonged to an enclosing computation and is
    /// replaced.
    pub(crate) fn interruptible(self, scope: Scope) -> Self {
        Self {
            fd: self.fd,
            scope: Some(scope),
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
            scope: self.scope.clone(),
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

/// The fd read under its scope.
struct Scoped<'a> {
    fd: &'a mut Fd,
    scope: &'a Scope,
}

impl Read for Scoped<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        read_interruptible(self.fd, self.scope, buf)
    }
}

impl SourceReader {
    fn with<T>(&mut self, f: impl FnOnce(&mut dyn Read) -> T) -> T {
        match &self.scope {
            Some(scope) => f(&mut Scoped {
                fd: &mut self.fd,
                scope,
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

/// A scope carrying a cause reads as EOF, never as data.  The scope is read
/// first, so a stream that is always ready still polls once per read.
#[cfg(unix)]
fn read_interruptible(fd: &mut Fd, scope: &Scope, buf: &mut [u8]) -> io::Result<usize> {
    use super::poll_slice;
    use rustix::event::PollFlags;
    loop {
        if scope.is_cancelled() {
            return Ok(0);
        }
        if poll_slice(&*fd, PollFlags::IN)?.is_some() {
            return fd.read(buf);
        }
    }
}

#[cfg(windows)]
fn read_interruptible(fd: &mut Fd, scope: &Scope, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        if scope.is_cancelled() {
            return Ok(0);
        }
        // An `interrupt` aborts the `ReadFile` in flight; the scope is then re-read.
        match fd.read(buf) {
            Err(e) if super::aborted(&e) => {}
            r => return r,
        }
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
impl std::os::fd::AsFd for Fd {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        match self {
            Self::Pipe(r) => r.as_fd(),
            Self::File(f) => f.as_fd(),
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

    use crate::process::{CancelCause, DurableRoot, ScopeNode};

    fn cancel_after(scope: &ScopeNode, ms: u64) -> std::thread::JoinHandle<()> {
        let scope = scope.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(ms));
            scope.cancel(CancelCause::Interrupted);
        })
    }

    #[test]
    fn a_blocked_read_returns_eof_within_a_slice_of_the_cause() {
        let (r, _w) = crate::process::cloexec_pipe().expect("data pipe");
        let scope = DurableRoot::default().worker();
        let mut reader = SourceReader::pipe(r).interruptible(Scope::clone(&scope));
        let canceller = cancel_after(&scope, 20);

        let start = Instant::now();
        let mut buf = [0u8; 8];
        let n = reader.read(&mut buf).expect("read");
        assert_eq!(n, 0);
        assert!(start.elapsed() < Duration::from_millis(100));
        canceller.join().expect("cancel thread");
    }

    #[test]
    fn data_written_before_the_cause_is_still_delivered() {
        let (r, mut w) = crate::process::cloexec_pipe().expect("data pipe");
        let scope = DurableRoot::default().worker();
        let mut reader = SourceReader::pipe(r).interruptible(Scope::clone(&scope));

        use std::io::Write;
        w.write_all(b"hi").expect("write");

        let mut buf = [0u8; 8];
        let n = reader.read(&mut buf).expect("read");
        assert_eq!(&buf[..n], b"hi");

        // The write end is still open, so a second read would block here
        // without the cause: it is the scope, not a closed pipe, that
        // produces the EOF below.
        let canceller = cancel_after(&scope, 20);
        let n = reader.read(&mut buf).expect("read after the cause");
        assert_eq!(n, 0);
        canceller.join().expect("cancel thread");
        drop(w);
    }

    #[test]
    fn a_ready_read_under_a_cause_is_eof() {
        use std::io::Write;
        let (r, mut w) = crate::process::cloexec_pipe().expect("data pipe");
        let scope = DurableRoot::default().worker();
        let mut reader = SourceReader::pipe(r).interruptible(Scope::clone(&scope));
        w.write_all(b"pending").expect("write");
        scope.cancel(CancelCause::Interrupted);
        assert_eq!(reader.read(&mut [0u8; 8]).expect("read"), 0);
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
