//! The runtime error type.

use crate::process::{CancelCause, CommandFailure};
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
    pub command: Option<String>,
}

/// An error's exit status: a bare code, or the process failure behind one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Code(i32),
    Process(CommandFailure),
    /// ral's own cancellation, minted at a poll point; `Process(CommandFailure::Cancelled)`
    /// is the same fact reported by a child ral tore down.
    Cancelled(CancelCause),
}

impl Error {
    pub fn new(message: impl Into<String>, status: i32) -> Self {
        Self {
            message: message.into(),
            status: Status::Code(status),
            span: None,
            hint: None,
            command: None,
        }
    }

    /// A poll point is any site that reads `CancelScope::cause()` (or
    /// `is_cancelled()`) and answers `Some` by ending the evaluation with an
    /// error; every such site mints that error through `Error::cancelled(cause)`.
    pub fn cancelled(cause: CancelCause) -> Self {
        Self {
            message: cause.message().into(),
            status: Status::Cancelled(cause),
            span: None,
            hint: None,
            command: None,
        }
    }

    /// The cancellation this error reports, from either door: minted here by
    /// `Error::cancelled`, or reported by a child ral tore down.
    pub fn cancelled_by(&self) -> Option<CancelCause> {
        match self.status {
            Status::Cancelled(cause) | Status::Process(CommandFailure::Cancelled { cause, .. }) => {
                Some(cause)
            }
            _ => None,
        }
    }

    /// A bare exit code takes its hint from the session's `exit_hints` table.
    pub fn from_command_failure(
        cmd: &str,
        failure: CommandFailure,
        shell: &crate::types::Shell,
    ) -> Self {
        let hint = failure.default_hint(cmd).or_else(|| match &failure {
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

    pub fn at_span(mut self, span: Span) -> Self {
        self.span = Some(span);
        self
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Numeric exit code for process exit and `$status`.
    pub fn exit_code(&self) -> i32 {
        match &self.status {
            Status::Code(code) => *code,
            Status::Process(failure) => failure.to_user_exit_code(),
            Status::Cancelled(cause) => cause.exit_code(),
        }
    }

    /// `None` for a process failure, whose message already names its status.
    pub fn status_code_for_display(&self) -> Option<i32> {
        match &self.status {
            Status::Code(0) | Status::Process(_) => None,
            Status::Code(code) => Some(*code),
            Status::Cancelled(cause) => Some(cause.exit_code()),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Error {}
