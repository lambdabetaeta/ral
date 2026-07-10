//! Unified stream plumbing.
//!
//! Three submodules carry the building blocks.  [`terminal`] caches the
//! startup isatty / ANSI / NO_COLOR / mode bits.  [`source`] is a stage's
//! byte input.  [`sink`] is its byte output, together with the child-process
//! routing plan and the in-memory buffer primitives.  Public items from each
//! are re-exported below so callers spell them `crate::io::Sink`,
//! `crate::io::TerminalState`, etc.
//!
//! This file holds [`Io`] — the per-Shell IO bundle (stdin / stdout /
//! stderr / interactive / terminal / launch_role / capture_outer /
//! capture_depth) — and
//! [`LaunchRole`], the process-group role that distinguishes the top-level
//! orchestrator from a pipeline-local child. Terminal-foreground authority is
//! no longer carried here — that is the session's
//! [`TerminalLease`](crate::process::TerminalLease); this type only governs
//! process-group placement.

mod sink;
mod source;
mod terminal;

pub use sink::{ByteBuffer, ChildStdioPlan, ExternalWrite, SINK_BUFFER_CAP, Sink};
pub(crate) use sink::{
    new_buffer, peek_buffer, str_strip_one_terminator, strip_trailing_newline, take_buffer,
    tee_with_buffer,
};
pub use source::{Source, SourceReader};
#[cfg(windows)]
pub use terminal::enable_virtual_terminal_processing;
pub use terminal::{InteractiveMode, TerminalState};
#[cfg(windows)]
pub(crate) use terminal::{STD_ERROR_HANDLE, is_console};

use std::io;

/// The process-group role of the current shell context: the top-level
/// orchestrator, or a pipeline-local child.
///
/// This is the residue of the former `JobControl` once terminal-foreground
/// authority moved to the session's
/// [`TerminalLease`](crate::process::TerminalLease). It still decides
/// process-group *placement* — a top-level standalone external may lead its
/// own group (so a watchdog cancel can `kill(-pgid, …)` the whole subtree),
/// while a pipeline stage must join the pipeline's pgid and never become a
/// new-leader orchestrator on its own — and it forgives SIGPIPE on pipeline
/// children. It no longer says anything about who may foreground.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LaunchRole {
    /// The orchestrator: a top-level eval or single-command exec.
    #[default]
    TopLevel,
    /// A pipeline-local child. Joins the pipeline's pgid; never leads its own
    /// group independently.
    PipelineStage,
}

impl LaunchRole {
    /// Whether this is the top-level orchestrator (not a pipeline stage).
    pub fn is_top_level(self) -> bool {
        matches!(self, Self::TopLevel)
    }
}

/// All pipeline-stage IO state for a single Shell.
pub struct Io {
    /// Byte source for this stage.
    pub stdin: Source,
    /// Byte sink for this stage.
    pub stdout: Sink,
    /// Byte sink for this stage's stderr.  Defaults to `Sink::Stderr`.
    /// Spawned handles install a `Sink::Buffer` here so errors are buffered
    /// in the handle and replayed on `await` (§13.3 replay rule).
    pub stderr: Sink,
    /// True when the shell is running as an interactive REPL.
    pub interactive: bool,
    /// Cached isatty results from shell startup.
    pub terminal: TerminalState,
    /// This shell context's process-group role. `TopLevel` for orchestrator
    /// paths; `PipelineStage` inside helper-owned pipeline code. Governs pgid
    /// placement and SIGPIPE forgiveness, not terminal foreground — that is the
    /// session [`TerminalLease`](crate::process::TerminalLease).
    pub launch_role: LaunchRole,
    /// The stdout that was active before the current `with_capture` installed
    /// its buffer.  `Comp::Seq` flushes non-final commands' bytes here so
    /// side-effects remain visible rather than being silently discarded.
    /// `None` when not inside a capture context.
    pub capture_outer: Option<Sink>,
    /// Depth of nested `with_capture` scopes.  `> 0` means the current
    /// stdout is a capture buffer (or tee chain leading to one), so
    /// pipeline planning must avoid foregrounding: a captured pipeline
    /// must not steal the controlling terminal away from the parent.
    /// `capture_outer` alone is not enough — a `try_clone` of the outer
    /// sink can fail and leave it `None` even inside a capture.
    pub capture_depth: usize,
}

impl Io {
    /// Clone the Io state for a child thread.
    ///
    /// `stdin` is not propagated: it is a read-once resource consumed by the
    /// child that spawns it.  The caller must set `child.io.stdin` explicitly.
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
            capture_depth: self.capture_depth,
        })
    }

    /// Install Io state from `parent` into `self` for a same-thread child shell
    /// (thunk body, `try`, `audit`, …).  Bytes sinks are `try_clone`d; the
    /// pipe stdin is *moved* out of the parent so the child consumes it once.
    /// `try_clone` failure collapses to the default terminal sink: by then
    /// the parent's FDs are already gone, and `Sink::Terminal` routes
    /// through the sink machinery to the inherited fd 1, which this
    /// same-thread child shares with the parent.
    pub fn inherit_from(&mut self, parent: &mut Self) {
        self.stdout = parent.stdout.try_clone().unwrap_or(Sink::Terminal);
        self.stderr = parent.stderr.try_clone().unwrap_or(Sink::Stderr);
        self.capture_outer = parent
            .capture_outer
            .as_ref()
            .and_then(|s| s.try_clone().ok());
        self.capture_depth = parent.capture_depth;
        self.terminal = parent.terminal;
        self.interactive = parent.interactive;
        self.launch_role = parent.launch_role;
        // Move the whole source so every marker (`Empty` as well as a `Pipe` /
        // `File`) reaches the child: a child of an `Empty`-stdin turn must also
        // see no fall-through to fd 0, not silently revert to `Terminal`.
        self.stdin = std::mem::replace(&mut parent.stdin, Source::Terminal);
    }

    /// STT-out: return the read-once stdin to `parent` so subsequent
    /// sibling calls see the unconsumed pipe.  Mirror of `inherit_from`.
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
            capture_depth: 0,
        }
    }
}
