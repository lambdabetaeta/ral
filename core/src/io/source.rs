//! Byte input for a pipeline stage — the mirror of `sink` on the output side.

use std::fs::File;
use std::io::{self, Read};

use crate::process::{Scope, StdioSpec};
#[cfg(unix)]
use std::os::fd::OwnedFd as Owned;
#[cfg(windows)]
use std::os::windows::io::OwnedHandle as Owned;

/// Where a stage's byte input comes from.
///
/// A `<` or `<<` redirect is installed here rather than `dup2`'d onto fd 0, and a session's non-tty fd 0 is a `Reader` from the
/// start: `Terminal` always means a terminal on the inherited fd 0.  `Empty`
/// denies byte input without denying foreground authority: immediate EOF for
/// readers, `/dev/null` for a child, never `Terminal`'s fall-through to fd 0.
pub enum Source {
    Terminal,
    Empty,
    Reader(SourceReader),
}

/// A pipe or file, and for a stage thread's stdin the scope whose cause ends
/// a blocked read as EOF.  The scope is never handed to a child.
pub struct SourceReader {
    file: File,
    scope: Option<Scope>,
}

impl Source {
    /// Configure a child's stdin. Readers lend a duplicate of their descriptor;
    /// the source and its cancellation scope remain with the parent.
    ///
    /// # Errors
    /// Returns `Err` if duplicating the descriptor fails.
    pub(crate) fn child_stdio(&self) -> io::Result<StdioSpec> {
        match self {
            Self::Empty => Ok(StdioSpec::Null),
            Self::Terminal => Ok(StdioSpec::Inherit),
            Self::Reader(reader) => reader.file.try_clone().map(StdioSpec::from_owned),
        }
    }

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
        Self::file(File::from(Owned::from(r)))
    }

    pub(crate) fn file(file: File) -> Self {
        Self { file, scope: None }
    }

    /// A reader of `bytes`, written into a pipe by a thread of its own so
    /// that more than the pipe buffer holds cannot deadlock.  If the reader
    /// stops early, the thread gets a broken pipe and exits.
    ///
    /// # Errors
    /// Returns `Err` if the pipe or the thread cannot be created.
    pub(crate) fn from_bytes(bytes: Vec<u8>) -> io::Result<Self> {
        use std::io::Write as _;
        let (reader, mut writer) = crate::process::cloexec_pipe()?;
        std::thread::Builder::new()
            .name("ral-bytes-source".into())
            .spawn(move || {
                // A broken pipe means the reader stopped early, which is fine.
                let _ = writer.write_all(&bytes);
            })?;
        Ok(Self::pipe(reader))
    }

    /// This reader as one that ends as EOF once `scope` carries a cause; a
    /// scope it already carried belonged to an enclosing computation and is
    /// replaced.
    pub(crate) fn interruptible(self, scope: Scope) -> Self {
        Self {
            scope: Some(scope),
            ..self
        }
    }

    /// # Errors
    /// Returns `Err` if duplicating the pipe or file fails.
    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
            scope: self.scope.clone(),
        })
    }
}

impl Read for SourceReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &self.scope {
            Some(scope) => read_interruptible(&mut self.file, scope, buf),
            None => self.file.read(buf),
        }
    }

    /// An unscoped file keeps `File`'s exact-capacity read; a scoped one
    /// takes the default loop over [`Self::read`].
    fn read_to_end(&mut self, buf: &mut Vec<u8>) -> io::Result<usize> {
        match self.scope {
            None => self.file.read_to_end(buf),
            Some(_) => self.by_ref().take(u64::MAX).read_to_end(buf),
        }
    }
}

/// A scope carrying a cause reads as EOF, never as data.  The scope is read
/// first, so a stream that is always ready still polls once per read.
fn read_interruptible(fd: &mut File, scope: &Scope, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        if scope.is_cancelled() {
            return Ok(0);
        }
        if let Some(n) = try_read(fd, buf)? {
            return Ok(n);
        }
    }
}

/// Read once the fd is ready within a slice; `None` if the slice ends first,
/// or a signal cuts the read.
#[cfg(unix)]
fn try_read(fd: &mut File, buf: &mut [u8]) -> io::Result<Option<usize>> {
    use rustix::event::PollFlags;
    if super::poll_slice(&*fd, PollFlags::IN)?.is_none() {
        return Ok(None);
    }
    match fd.read(buf) {
        Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(None),
        r => r.map(Some),
    }
}

/// Read; `None` if an `interrupt` aborted the `ReadFile`, which took nothing.
#[cfg(windows)]
fn try_read(fd: &mut File, buf: &mut [u8]) -> io::Result<Option<usize>> {
    match fd.read(buf) {
        Err(e) if super::aborted(&e) => Ok(None),
        r => r.map(Some),
    }
}

impl From<SourceReader> for crate::process::StdioSpec {
    fn from(r: SourceReader) -> Self {
        Self::from_owned(r.file)
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods, reason = "test code")]
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
    fn a_blocked_read_returns_eof_once_the_cause_lands() {
        let (r, _w) = crate::process::cloexec_pipe().expect("data pipe");
        let scope = DurableRoot::default().child();
        let mut reader = SourceReader::pipe(r).interruptible(Scope::clone(&scope));
        let canceller = cancel_after(&scope, 20);

        let start = Instant::now();
        let mut buf = [0u8; 8];
        let n = reader.read(&mut buf).expect("read");
        assert_eq!(n, 0);
        assert!(start.elapsed() < crate::test_helper::NEVER_RETURNS);
        canceller.join().expect("cancel thread");
    }

    #[test]
    fn data_written_before_the_cause_is_still_delivered() {
        let (r, mut w) = crate::process::cloexec_pipe().expect("data pipe");
        let scope = DurableRoot::default().child();
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
        let scope = DurableRoot::default().child();
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
    fn a_scoped_read_to_end_stops_at_the_cause_with_what_arrived() {
        let (r, mut w) = crate::process::cloexec_pipe().expect("data pipe");
        let scope = DurableRoot::default().child();
        let mut reader = SourceReader::pipe(r).interruptible(Scope::clone(&scope));
        let canceller = cancel_after(&scope, 20);

        use std::io::Write;
        w.write_all(b"hi").expect("write");
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).expect("read_to_end");
        assert_eq!(buf, b"hi");
        canceller.join().expect("cancel thread");
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
