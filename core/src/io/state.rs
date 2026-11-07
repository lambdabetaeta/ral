//! Per-shell pipeline I/O state.

use super::{Sink, Source};

/// A shell's three streams.
pub(crate) struct Io {
    pub stdin: Source,
    /// Where the running computation writes.
    pub stdout: Sink,
    /// `spawn` installs a buffer sink here, so a worker's errors are held in
    /// its handle and replayed by `await`, never interleaved with the parent's.
    pub stderr: Sink,
}

impl Default for Io {
    /// No input; output to the inherited fds.
    fn default() -> Self {
        Self {
            stdin: Source::Empty,
            stdout: Sink::Terminal,
            stderr: Sink::Stderr,
        }
    }
}
