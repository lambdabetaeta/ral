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
//! stderr / interactive / terminal / job_control / capture_outer /
//! capture_depth) — and
//! [`JobControl`], the foreground-eligibility token that distinguishes the
//! orchestrator from pipeline-local children.

mod sink;
mod source;
mod terminal;

pub use sink::{ByteBuffer, ChildStdioPlan, ExternalWrite, SINK_BUFFER_CAP, Sink};
pub(crate) use sink::{
    new_buffer, str_strip_one_terminator, strip_trailing_newline, take_buffer, tee_with_buffer,
};
pub use source::{Source, SourceReader};
#[cfg(windows)]
pub use terminal::enable_virtual_terminal_processing;
pub use terminal::{InteractiveMode, TerminalState};
#[cfg(windows)]
pub(crate) use terminal::{STD_ERROR_HANDLE, is_console};

use std::io;

/// Whether the current shell context may hand the controlling terminal
/// to a spawned external child.
///
/// Constructed only via the named methods so the discipline is grep-able:
/// the orchestrator (top-level call, single-command exec) issues
/// `Eligible`; pipeline helpers and their nested children issue
/// `Forbidden`. The common point is simple: only the pipeline launcher may
/// hand off the terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobControl {
    foreground_eligible: bool,
}

impl JobControl {
    /// The orchestrator (top-level eval, single-command exec).  May
    /// foreground a spawned child when other conditions are met
    /// (interactive shell, tty stdin, terminal stdout, no shell pump).
    pub fn top_level() -> Self {
        Self {
            foreground_eligible: true,
        }
    }

    /// Pipeline-local child. Must never take foreground — the pipeline
    /// launcher owns that decision.
    pub fn pipeline_child() -> Self {
        Self {
            foreground_eligible: false,
        }
    }

    pub fn may_foreground(&self) -> bool {
        self.foreground_eligible
    }
}

impl Default for JobControl {
    fn default() -> Self {
        Self::top_level()
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
    /// Whether this shell context may take terminal foreground. `top_level`
    /// for orchestrator paths; `pipeline_child` inside helper-owned pipeline
    /// code. Independent of the `interactive`/`terminal` checks: those
    /// describe the *capability*, this describes whether *this caller* is
    /// permitted to use it.
    pub job_control: JobControl,
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
        Ok(Io {
            stdin: Source::Terminal,
            stdout: self.stdout.try_clone()?,
            stderr: self.stderr.try_clone()?,
            interactive: self.interactive,
            terminal: self.terminal,
            job_control: self.job_control,
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
    pub fn inherit_from(&mut self, parent: &mut Io) {
        self.stdout = parent.stdout.try_clone().unwrap_or(Sink::Terminal);
        self.stderr = parent.stderr.try_clone().unwrap_or(Sink::Stderr);
        self.capture_outer = parent
            .capture_outer
            .as_ref()
            .and_then(|s| s.try_clone().ok());
        self.capture_depth = parent.capture_depth;
        self.terminal = parent.terminal;
        self.interactive = parent.interactive;
        self.job_control = parent.job_control;
        self.stdin = Source::from_reader(parent.stdin.take_reader());
    }

    /// STT-out: return the read-once stdin to `parent` so subsequent
    /// sibling calls see the unconsumed pipe.  Mirror of `inherit_from`.
    pub fn return_to(&mut self, parent: &mut Io) {
        parent.stdin = Source::from_reader(self.stdin.take_reader());
    }
}

impl Default for Io {
    fn default() -> Self {
        Io {
            stdin: Source::Terminal,
            stdout: Sink::Terminal,
            stderr: Sink::Stderr,
            interactive: false,
            terminal: TerminalState::default(),
            job_control: JobControl::default(),
            capture_outer: None,
            capture_depth: 0,
        }
    }
}
