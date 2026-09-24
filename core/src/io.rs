//! Unified stream plumbing: [`Io`], the per-`Shell` bundle of byte streams
//! and terminal state.
//!
//! It sits over the [`edge`] / [`source`] / [`sink`] / [`terminal`]
//! submodules, whose public items are re-exported here as `crate::io::*`.

mod edge;
mod sink;
mod source;
mod terminal;

pub(crate) use edge::{DeadEdge, Edge};
pub use sink::{ByteBuffer, CapturedBytes, Sink};
pub(crate) use sink::{
    SINK_BUFFER_CAP, buffer_overflowed, new_buffer, peek_buffer, str_strip_one_terminator,
    strip_trailing_newline, take_buffer, tee_into, tee_with_buffer,
};
pub use source::{Source, SourceReader};
pub use terminal::{InteractiveMode, TerminalState};
#[cfg(windows)]
pub(crate) use terminal::{STD_ERROR_HANDLE, is_console};
#[cfg(windows)]
pub use terminal::{
    console_mode_snapshot, enable_virtual_terminal_processing, restore_console_mode,
};

/// Process-group role of a shell context: top-level orchestrator, or
/// pipeline-local child.
///
/// It decides pgid placement — a top-level standalone external may lead its own
/// group, so a watchdog cancel can `kill(-pgid, …)` the whole subtree — and
/// names the reader a child's exit status is read against
/// ([`Reader`](crate::process::Reader)).  Being top-level is also one conjunct
/// of the foreground gate in `runtime/command/foreground.rs`; holding the
/// session's [`TerminalLease`](crate::process::TerminalLease) is another.
#[derive(Clone, Debug, Default)]
pub(crate) enum LaunchRole {
    /// A top-level eval or single-command exec.
    #[default]
    TopLevel,
    /// Joins the pipeline's pgid; never leads a group of its own.
    PipelineStage(crate::process::Membership),
}

impl LaunchRole {
    pub(crate) fn is_top_level(&self) -> bool {
        matches!(self, Self::TopLevel)
    }

    pub(crate) fn membership(&self) -> Option<&crate::process::Membership> {
        match self {
            Self::TopLevel => None,
            Self::PipelineStage(m) => Some(m),
        }
    }
}

/// All pipeline-stage IO state for a single Shell.
pub(crate) struct Io {
    pub stdin: Source,
    /// Where the running computation's own payload goes.
    pub stdout: Sink,
    /// The nearest enclosing *visible* stream: where a discarded statement
    /// writes.  Never a capture buffer, so however deep the brackets nest
    /// there is no rule about which one wins.
    pub(crate) ambient: Sink,
    /// `spawn` installs a buffer sink here, so a worker's errors are held in
    /// its handle and drained on `await`, never interleaved with the parent's.
    pub stderr: Sink,
    /// Running as an interactive REPL — not merely attached to a tty.
    pub(crate) interactive: bool,
    /// Probed once at startup; nothing re-queries the OS mid-session.
    pub terminal: TerminalState,
    pub(crate) launch_role: LaunchRole,
}

impl Io {
    /// Swap `stdout` for the ambient sink, returning what `stdout` was.
    /// `with_ambient_stdout` is a bracket over this; the `Bind` rule of a
    /// binder's RHS is the other caller — the RHS's bytes are effect, so
    /// they go where a discarded statement's do, and the frame that pushed
    /// this swap restores it from the value handed back.
    pub(crate) fn swap_ambient_stdout(&mut self) -> Sink {
        std::mem::replace(&mut self.stdout, self.ambient.clone())
    }
}

impl Default for Io {
    fn default() -> Self {
        Self {
            stdin: Source::Terminal,
            stdout: Sink::Terminal,
            ambient: Sink::Terminal,
            stderr: Sink::Stderr,
            interactive: false,
            terminal: TerminalState::default(),
            launch_role: LaunchRole::default(),
        }
    }
}
