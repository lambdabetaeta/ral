//! Byte input for a pipeline stage — the mirror of `sink` on the output side.

use std::io::{self, Read};

/// Where a stage's byte input comes from.
///
/// A `<file` redirect is parked here as `File` by `install_stdin_redirect`
/// rather than `dup2`'d onto fd 0, which keeps the cached `startup_stdin_tty`
/// honest: consumers consult it only when this is `Terminal`.  `Empty` denies
/// byte input without denying foreground authority — immediate EOF for
/// readers, `/dev/null` for a child, never `Terminal`'s fall-through to fd 0.
pub enum Source {
    Terminal,
    Empty,
    Pipe(os_pipe::PipeReader),
    File(std::fs::File),
}

/// Owning handle to a claimed pipe or file, returned by [`Source::take_reader`];
/// consumers use it as a `Read`, an `AsRawFd`, or a `process::Stdio`.
pub enum SourceReader {
    Pipe(os_pipe::PipeReader),
    File(std::fs::File),
}

impl Source {
    /// Claim a pipe or file source; both fd-less markers yield `None`, and
    /// `Empty` is restored in place rather than collapsing to `Terminal`, so a
    /// second read still sees no fd-0 fall-through.
    pub fn take_reader(&mut self) -> Option<SourceReader> {
        match std::mem::replace(self, Self::Terminal) {
            Self::Terminal => None,
            Self::Empty => {
                *self = Self::Empty;
                None
            }
            Self::Pipe(r) => Some(SourceReader::Pipe(r)),
            Self::File(f) => Some(SourceReader::File(f)),
        }
    }
}

impl Read for SourceReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Pipe(r) => r.read(buf),
            Self::File(f) => f.read(buf),
        }
    }
}

impl From<SourceReader> for std::process::Stdio {
    fn from(r: SourceReader) -> Self {
        match r {
            SourceReader::Pipe(r) => Self::from(r),
            SourceReader::File(f) => Self::from(f),
        }
    }
}

#[cfg(unix)]
impl std::os::unix::io::AsRawFd for SourceReader {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        match self {
            Self::Pipe(r) => r.as_raw_fd(),
            Self::File(f) => f.as_raw_fd(),
        }
    }
}

#[cfg(windows)]
impl std::os::windows::io::AsRawHandle for SourceReader {
    fn as_raw_handle(&self) -> std::os::windows::io::RawHandle {
        match self {
            Self::Pipe(r) => r.as_raw_handle(),
            Self::File(f) => f.as_raw_handle(),
        }
    }
}
