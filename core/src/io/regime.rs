//! The regime a run asks of its streams: intent, which the run doors turn
//! into resources.

use serde::{Deserialize, Serialize};

/// The IO regime of a run — intent, which the run doors turn into resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunIo {
    /// Clone the run's byte sinks from the ambient `shell.io`: the REPL and
    /// batch.
    Inherit,
    /// Mint fresh stdout/stderr buffers, returned in [`Report::Ran`]'s
    /// `captured`: exarch's tool runs.
    Capture,
}

/// The byte source a run's stdin reads from — orthogonal to [`RunIo`] (the
/// *output* regime) and to [`RequestedTerminalAccess`](crate::process::RequestedTerminalAccess) (foreground
/// authority).
///
/// A piped `ral -c` is `Denied` yet still reads its inherited pipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStdin {
    /// The inherited fd 0 — a terminal, a pipe, or a redirected file.
    Inherit,
    /// Immediate EOF, a child's stdin wired to `/dev/null`, no fall-through to
    /// fd 0.
    Empty,
}
