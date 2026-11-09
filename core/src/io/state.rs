//! A shell's three streams.

use super::{Sink, Source};
use std::mem::replace;

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

/// Some of an [`Io`]'s streams, `None` for each one left alone.
#[derive(Default)]
pub(crate) struct IoPatch {
    pub stdin: Option<Source>,
    pub stdout: Option<Sink>,
    pub stderr: Option<Sink>,
}

impl Io {
    /// Install each stream `patch` holds, and return the streams it
    /// displaced: the patch that undoes this one.
    #[must_use = "the displaced streams are the only way back"]
    pub(crate) fn swap(&mut self, patch: IoPatch) -> IoPatch {
        IoPatch {
            stdin: patch.stdin.map(|s| replace(&mut self.stdin, s)),
            stdout: patch.stdout.map(|s| replace(&mut self.stdout, s)),
            stderr: patch.stderr.map(|s| replace(&mut self.stderr, s)),
        }
    }

    /// Swap `prior` back, dropping the streams it displaces.
    pub(crate) fn restore(&mut self, prior: IoPatch) {
        drop(self.swap(prior));
    }
}
