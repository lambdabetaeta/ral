//! Unified stream plumbing: [`Io`], the per-`Shell` bundle of byte streams
//! and terminal state.
//!
//! It sits over the [`source`] / [`sink`] / [`terminal`] submodules, whose
//! public items are re-exported here as `crate::io::*`.

mod sink;
mod source;
mod terminal;

pub use sink::{ByteBuffer, ChildStdioPlan, ExternalWrite, Sink};
pub(crate) use sink::{
    new_buffer, peek_buffer, str_strip_one_terminator, strip_trailing_newline, take_buffer,
    tee_with_buffer,
};
pub use source::{Source, SourceReader};
pub use terminal::{InteractiveMode, TerminalState};
#[cfg(windows)]
pub(crate) use terminal::{STD_ERROR_HANDLE, is_console};
#[cfg(windows)]
pub use terminal::{
    console_mode_snapshot, enable_virtual_terminal_processing, restore_console_mode,
};

use std::io;

/// Process-group role of a shell context: top-level orchestrator, or
/// pipeline-local child.
///
/// It decides pgid placement — a top-level standalone external may lead its own
/// group, so a watchdog cancel can `kill(-pgid, …)` the whole subtree — and
/// forgives SIGPIPE on pipeline children.  Being top-level is also one conjunct
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
    pub stdout: Sink,
    /// `spawn` installs a buffer sink here, so a worker's errors are held in
    /// its handle and drained on `await`, never interleaved with the parent's.
    pub stderr: Sink,
    /// Running as an interactive REPL — not merely attached to a tty.
    pub interactive: bool,
    /// Probed once at startup; nothing re-queries the OS mid-session.
    pub terminal: TerminalState,
    pub launch_role: LaunchRole,
    /// The stdout displaced by the enclosing `with_capture`.  `Comp::Seq`
    /// flushes non-final commands' bytes here, so their side effects stay
    /// visible instead of vanishing into the captured value.  `None` outside a
    /// capture.
    pub capture_outer: Option<Sink>,
}

impl Io {
    /// Duplicate this bundle, dup'ing each sink's file descriptor.
    ///
    /// `stdin` is not propagated: it is read-once, so the caller installs the
    /// new bundle's source itself.
    ///
    /// # Errors
    /// Returns `Err` if any sink's file descriptor cannot be duplicated.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            stdin: Source::Terminal,
            stdout: self.stdout.try_clone()?,
            stderr: self.stderr.try_clone()?,
            interactive: self.interactive,
            terminal: self.terminal,
            launch_role: self.launch_role,
            capture_outer: self
                .capture_outer
                .as_ref()
                .map(Sink::try_clone)
                .transpose()?,
        })
    }

    /// Install `parent`'s IO into a cross-process pipeline-stage child — via
    /// `Shell::child_of`, over the throwaway parent `child_eval` rebuilds in the
    /// helper process.  Sinks are `try_clone`d; stdin is *moved*, since only one
    /// of the two may consume a read-once source.  A failed `try_clone`
    /// collapses to the terminal sink, which routes to the inherited fd 1 the
    /// child shares with that parent anyway.
    pub fn inherit_from(&mut self, parent: &mut Self) {
        self.stdout = parent.stdout.try_clone().unwrap_or(Sink::Terminal);
        self.stderr = parent.stderr.try_clone().unwrap_or(Sink::Stderr);
        self.capture_outer = parent
            .capture_outer
            .as_ref()
            .and_then(|s| s.try_clone().ok());
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
}

impl Default for Io {
    fn default() -> Self {
        Self {
            stdin: Source::Terminal,
            stdout: Sink::Terminal,
            stderr: Sink::Stderr,
            interactive: false,
            terminal: TerminalState::default(),
            launch_role: LaunchRole::default(),
            capture_outer: None,
        }
    }
}
