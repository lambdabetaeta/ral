//! Byte input for a pipeline stage.
//!
//! [`Source`] is the per-stage input slot ("upstream pipe", "redirected
//! file", "fall through to fd 0", or "no input at all").  [`SourceReader`]
//! is the owning handle returned once a *resource* source (pipe / file) has
//! been claimed: callers then use it as a `Read`, an `AsRawFd`, or convert
//! into a `process::Stdio` for spawn.  The two fd-less markers — `Terminal`
//! and `Empty` — yield no reader; a consumer that must tell them apart reads
//! the `Source` directly.

use std::io::{self, Read};

/// Where a stage's byte input comes from.
///
/// `Pipe` is the upstream pipeline writer; `File` is a `<file` redirect that
/// has been opened upstream and parked here so consumers see all redirected
/// input through one channel rather than racing the cached `stdin_tty` against
/// a `dup2`'d fd 0.  `Terminal` means "no redirect — fall through to whatever
/// fd 0 of the shell process is".  `Empty` means "no input at all" — an
/// explicit empty source that reads as immediate EOF and wires a child's stdin
/// to `/dev/null`, with *no* fall-through to fd 0.  It is what an exarch tool
/// turn installs so a tool command can never steal the TUI's controlling
/// terminal; it is distinct from `Terminal` precisely so denial of foreground
/// authority and denial of byte input stay separate effects.
pub enum Source {
    Terminal,
    Empty,
    Pipe(os_pipe::PipeReader),
    File(std::fs::File),
}

/// Owning handle to a non-terminal input source, returned by
/// [`Source::take_reader`].
///
/// Read-trait consumers (codecs, `lines`) call `Read` directly; fd consumers
/// (sandbox spawn, externals) reach for `AsRawFd` / `Into<Stdio>` without
/// caring whether the underlying handle was a pipe or a file.
pub enum SourceReader {
    Pipe(os_pipe::PipeReader),
    File(std::fs::File),
}

impl Source {
    /// Claim a *resource* source (pipe / file), leaving `Terminal` in place.
    ///
    /// Returns `None` for both fd-less markers: `Terminal` (fall through to
    /// fd 0) and `Empty` (no input). `Empty` is restored in place rather than
    /// collapsing to `Terminal`, so a second read in the same turn still sees
    /// no fd-0 fall-through; a consumer that must distinguish the two markers
    /// inspects the `Source` itself.
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
            SourceReader::Pipe(r) => r.as_raw_handle(),
            SourceReader::File(f) => f.as_raw_handle(),
        }
    }
}
