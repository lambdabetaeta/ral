//! The runtime error type.

use crate::process::{CancelCause, ChildEnd, CommandFailure, SpawnFailure};
use crate::source::Span;
use std::fmt;

#[derive(Debug, Clone)]
pub struct Error {
    pub message: String,
    pub status: Status,
    /// `None` until `evaluator::machine`'s `stamp` sets the innermost
    /// enclosing node's span.
    pub span: Option<Span>,
    pub hint: Option<String>,
    /// The shown name of the command whose failure this is; `None` until
    /// `evaluator::audit`'s `frame_call` stamps the innermost dispatch.
    pub(crate) command: Option<String>,
}

/// An error's exit status: one constructor per fact, whichever door reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Raised(i32),
    /// ral's own cancellation, whether a poll point or a child's teardown saw it.
    Cancelled(CancelCause),
    Process(CommandFailure),
}

impl Status {
    /// The numeric status: the only home of `128 + n` and of the cause table.
    pub fn code(&self) -> i32 {
        match self {
            Self::Raised(code) | Self::Process(CommandFailure::ExitCode(code)) => *code,
            Self::Process(CommandFailure::Signal(sig)) => 128 + sig.number(),
            Self::Process(CommandFailure::Spawn(SpawnFailure::NotFound)) => 127,
            Self::Process(CommandFailure::Spawn(_)) => 126,
            // A signal-born cause reports 128 + its signal; a deadline, timeout(1)'s 124.
            Self::Cancelled(cause) => match cause {
                CancelCause::ReaderGone => 141,
                CancelCause::Interrupt => 130,
                CancelCause::Explicit | CancelCause::Terminate => 143,
                CancelCause::Deadline => 124,
                CancelCause::RootAbort => 131,
            },
        }
    }

    /// The code an external command chose to exit with, if that is what this is.
    pub fn exited(&self) -> Option<i32> {
        match self {
            Self::Process(CommandFailure::ExitCode(code)) => Some(*code),
            _ => None,
        }
    }
}

impl Error {
    pub fn new(message: impl Into<String>, status: i32) -> Self {
        Self {
            message: message.into(),
            status: Status::Raised(status),
            span: None,
            hint: None,
            command: None,
        }
    }

    /// A poll point is any site that reads `CancelScope::cause()` (or
    /// `is_cancelled()`) and answers `Some` by ending the evaluation with an
    /// error; every such site mints that error through `Error::cancelled(cause)`.
    pub(crate) fn cancelled(cause: CancelCause) -> Self {
        Self {
            message: cause.message().into(),
            status: Status::Cancelled(cause),
            span: None,
            hint: None,
            command: None,
        }
    }

    pub(crate) fn cancelled_by(&self) -> Option<CancelCause> {
        match self.status {
            Status::Cancelled(cause) => Some(cause),
            _ => None,
        }
    }

    /// A child's end as the error `cmd` fails with; a cancelled one's is the
    /// very error a poll point mints for the same cause.
    pub(crate) fn of_child(cmd: &str, end: ChildEnd, shell: &crate::types::Shell) -> Self {
        match end {
            ChildEnd::Failed(failure) => Self::from_command_failure(cmd, failure, shell),
            ChildEnd::Cancelled(cause) => Self::cancelled(cause),
        }
    }

    /// A command that never became a process, whether the pre-spawn probe or
    /// the spawn itself found out.
    pub(crate) fn spawn_failure(cmd: &str, failure: SpawnFailure) -> Self {
        let failure = CommandFailure::Spawn(failure);
        Self {
            message: failure.message(cmd),
            status: Status::Process(failure),
            span: None,
            hint: None,
            command: None,
        }
    }

    /// A bare exit code takes its hint from the session's `exit_hints` table.
    pub(crate) fn from_command_failure(
        cmd: &str,
        failure: CommandFailure,
        shell: &crate::types::Shell,
    ) -> Self {
        let hint = failure.default_hint().or_else(|| match &failure {
            CommandFailure::ExitCode(code) => shell.session.exit_hints.lookup(cmd, *code),
            _ => None,
        });
        Self {
            message: failure.message(cmd),
            status: Status::Process(failure),
            span: None,
            hint,
            command: None,
        }
    }

    pub(crate) fn at_span(mut self, span: Span) -> Self {
        self.span = Some(span);
        self
    }

    pub(crate) fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Prefixes `message` with `ctx`, preserving span, hint, status and command.
    pub fn context(mut self, ctx: impl std::fmt::Display) -> Self {
        self.message = format!("{ctx}: {}", self.message);
        self
    }

    /// Numeric exit code for process exit and `$status`.
    pub fn exit_code(&self) -> i32 {
        self.status.code()
    }

    /// `None` for a process failure, whose message already names its status.
    pub(crate) fn status_code_for_display(&self) -> Option<i32> {
        match &self.status {
            Status::Raised(0) | Status::Process(_) => None,
            status => Some(status.code()),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Error {}
