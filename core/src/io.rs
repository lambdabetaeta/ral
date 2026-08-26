//! Unified stream plumbing: [`Io`], the per-`Shell` bundle of byte streams
//! and terminal state.
//!
//! It sits over the [`source`] / [`sink`] / [`terminal`] submodules, whose
//! public items are re-exported here as `crate::io::*`.

mod sink;
mod source;
mod terminal;

pub use sink::{ByteBuffer, CapturedBytes, ChildStdioPlan, ExternalWrite, Sink};
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LaunchRole {
    /// A top-level eval or single-command exec.
    #[default]
    TopLevel,
    /// Joins the pipeline's pgid; never leads a group of its own.
    PipelineStage,
}

impl LaunchRole {
    pub fn is_top_level(self) -> bool {
        matches!(self, Self::TopLevel)
    }
}

/// All pipeline-stage IO state for a single Shell.
pub struct Io {
    pub stdin: Source,
    /// Where the running computation's own payload goes.
    pub stdout: Sink,
    /// The nearest enclosing *visible* stream: where a discarded statement
    /// writes.  Never a capture buffer, so however deep the brackets nest
    /// there is no rule about which one wins.
    pub ambient: Sink,
    /// `spawn` installs a buffer sink here, so a worker's errors are held in
    /// its handle and drained on `await`, never interleaved with the parent's.
    pub stderr: Sink,
    /// Running as an interactive REPL — not merely attached to a tty.
    pub interactive: bool,
    /// Probed once at startup; nothing re-queries the OS mid-session.
    pub terminal: TerminalState,
    pub launch_role: LaunchRole,
}

impl Io {
    /// Install `parent`'s IO into a cross-process pipeline-stage child — via
    /// `Shell::child_of`, over the throwaway parent `child_eval` rebuilds in the
    /// helper process.  Sinks are cloned; stdin is *moved*, since only one of
    /// the two may consume a read-once source.
    pub fn inherit_from(&mut self, parent: &mut Self) {
        self.stdout = parent.stdout.clone();
        self.ambient = parent.ambient.clone();
        self.stderr = parent.stderr.clone();
        self.terminal = parent.terminal;
        self.interactive = parent.interactive;
        self.launch_role = parent.launch_role;
        // The whole source moves, markers included: a child of an `Empty` stdin
        // must also see no fall-through to fd 0, not revert to `Terminal`.
        self.stdin = std::mem::replace(&mut parent.stdin, Source::Terminal);
    }

    /// Hand the read-once stdin back to `parent`, so a later sibling still sees
    /// the unconsumed pipe.
    pub fn return_to(&mut self, parent: &mut Self) {
        parent.stdin = std::mem::replace(&mut self.stdin, Source::Terminal);
    }

    /// Swap `stdout` for the ambient sink, returning what `stdout` was.
    /// `with_ambient_stdout` is a bracket over this; the `Bind` rule of a
    /// binder's RHS is the other caller — the RHS's bytes are effect, so
    /// they go where a discarded statement's do, and the frame that pushed
    /// this swap restores it from the value handed back.
    pub(crate) fn to_ambient(&mut self) -> Sink {
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
