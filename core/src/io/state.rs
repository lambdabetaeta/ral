//! A shell's three streams.

use super::{Sink, Source};

pub(crate) struct Io {
    /// Where the running computation reads.
    pub stdin: Source,
    /// Where the running computation writes.
    pub stdout: Sink,
    /// Where its diagnostics go; `spawn` gives a worker a buffer sink here,
    /// replayed by `await`, so they never interleave with the parent's.
    pub stderr: Sink,
}

impl Default for Io {
    /// No input; output to the inherited fds.
    fn default() -> Self {
        Self {
            stdin: Source::Empty,
            stdout: Sink::Stdout,
            stderr: Sink::Stderr,
        }
    }
}
