//! Unified stream plumbing: [`Io`], the per-`Shell` bundle of byte streams
//! and terminal state.
//!
//! It sits over the [`edge`] / [`source`] / [`sink`] submodules, whose public
//! items are re-exported here as `crate::io::*`.

mod edge;
mod regime;
mod sink;
mod source;
mod wake;

use crate::terminal::TerminalState;
pub(crate) use edge::{DeadEdge, Edge};
pub use regime::{RunIo, RunStdin};
pub use sink::{ByteBuffer, Captured, CapturedBytes, LineFn, Sink};
pub(crate) use sink::{SINK_BUFFER_CAP, new_buffer, tee_with_buffer, terminator_len};
pub use source::{Source, SourceReader};
#[cfg(unix)]
pub(crate) use wake::Readiness;
pub(crate) use wake::Wake;

/// All pipeline-stage IO state for a single Shell.
pub(crate) struct Io {
    pub stdin: Source,
    /// Where the running computation writes.
    pub stdout: Sink,
    /// `spawn` installs a buffer sink here, so a worker's errors are held in
    /// its handle and drained on `await`, never interleaved with the parent's.
    pub stderr: Sink,
    /// Running as an interactive REPL — not merely attached to a tty.
    pub(crate) interactive: bool,
    /// Probed once at startup; nothing re-queries the OS mid-session.
    pub terminal: TerminalState,
    /// The pipeline group this context's externals join, never leading one of
    /// their own; `None` at the top level.  It decides pgid placement (a
    /// top-level standalone external may lead its own group, so a watchdog
    /// cancel can `kill(-pgid, …)` the whole subtree), and top level is one
    /// conjunct of the foreground gate in `runtime/command/foreground.rs`;
    /// holding the session's [`TerminalLease`](crate::process::TerminalLease)
    /// is another.
    pub(crate) stage: Option<crate::process::Membership>,
}

impl Default for Io {
    fn default() -> Self {
        Self {
            stdin: Source::Terminal,
            stdout: Sink::Terminal,
            stderr: Sink::Stderr,
            interactive: false,
            terminal: TerminalState::default(),
            stage: None,
        }
    }
}
