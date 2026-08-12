//! The runtime error type, and [`split`] — the one decomposition of a body's
//! outcome into the ([`BodyResult`], [`Escape`]) pair `evaluator::audit` consumes.

use super::flow::{Break, Escape};
use super::value::Value;
use crate::process::CommandFailure;
use crate::source::Span;
use std::fmt;

#[derive(Debug, Clone)]
pub struct Error {
    pub message: String,
    pub status: Status,
    /// `None` until `eval_comp` stamps the innermost enclosing node's span.
    pub span: Option<Span>,
    pub hint: Option<String>,
}

/// An error's exit status: a bare code, or the process failure behind one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Code(i32),
    Process(CommandFailure),
}

impl Error {
    pub fn new(message: impl Into<String>, status: i32) -> Self {
        Self {
            message: message.into(),
            status: Status::Code(status),
            span: None,
            hint: None,
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

    /// Message and hint as one line, for the forensic string `audit` records.
    /// A rendered diagnostic prints the hint beneath the message; a report read
    /// as data has one field for both, and dropping the hint there would make
    /// the record say less than the terminal does about the same failure.
    pub fn message_with_hint(&self) -> String {
        match &self.hint {
            Some(hint) => format!("{} — {hint}", self.message),
            None => self.message.clone(),
        }
    }

    /// Numeric exit code for process exit and `$status`.
    pub fn exit_code(&self) -> i32 {
        match &self.status {
            Status::Code(code) => *code,
            Status::Process(failure) => failure.to_user_exit_code(),
        }
    }

    /// `None` for a process failure, whose message already names its status.
    pub fn status_code_for_display(&self) -> Option<i32> {
        match &self.status {
            Status::Code(0) | Status::Process(_) => None,
            Status::Code(code) => Some(*code),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Error {}

/// A delimited body's non-escape outcome: a value, or an error — catchable by
/// `try`, recorded as a non-zero-status record by `audit`.
#[derive(Debug)]
pub enum BodyResult {
    Value(Value),
    Error(Error),
}

/// Sort a body's outcome into its non-escape and escape halves.
///
/// # Errors
/// An escape returns `Err`; an error stays in `Ok`, since `try`/`audit` must
/// still classify or record it.
pub fn split(settled: super::flow::Settled<Value>) -> Result<BodyResult, Escape> {
    match settled {
        Ok(v) => Ok(BodyResult::Value(v)),
        Err(Break::Error(e)) => Ok(BodyResult::Error(e)),
        Err(Break::Escape(esc)) => Err(esc),
    }
}
