//! Runtime error type and body-result split helper.
//!
//! [`Error`] is a located runtime error with an optional hint.  The
//! type [`Settled<T>`](super::flow::Settled) `= Result<T, Break>` is
//! the public boundary shape for evaluator outcomes; [`split`] is the
//! single helper that decomposes a `Settled<Value>` body into a
//! ([`BodyResult`], [`Escape`]) split for scope helpers.

use super::flow::{Break, Escape};
use super::value::Value;
use crate::process::CommandFailure;
use crate::source::Span;
use std::fmt;

#[derive(Debug, Clone)]
pub struct Error {
    pub message: String,
    pub status: Status,
    /// Byte range of the node the error broke on, resolved against a
    /// [`SourceDb`](crate::source::SourceDb) at render time.  `None` until
    /// the break path stamps the innermost enclosing node's span.
    pub span: Option<Span>,
    pub hint: Option<String>,
}

/// Reduced exit status or structured process failure.
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

    /// Build a runtime error from a structured command failure.
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

    /// Numeric exit code for process exit and `$status`.
    pub fn exit_code(&self) -> i32 {
        match &self.status {
            Status::Code(code) => *code,
            Status::Process(failure) => failure.to_user_exit_code(),
        }
    }

    /// Exit code to append in compact formatting, if any.
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

/// The non-escape outcome of a delimited body: either a value
/// (success), or a recoverable runtime error (catchable by `try`,
/// recorded as a non-zero-status node by `audit`/`grant`/`within`/
/// `guard`).
#[derive(Debug)]
pub enum BodyResult {
    Value(Value),
    Error(Error),
}

/// Decompose a body's `Settled<Value>` outcome into the
/// (`BodyResult`, `Escape`) split that scope helpers consume.
///
/// Total
/// by construction: `Settled<Value> = Result<Value, Break>` cannot
/// encode a tail call — `Tail` lives only in the evaluator-private
/// `Control` enum and is absorbed by the trampoline before any
/// `Settled` value is built — so every arm is reachable.
///
/// # Errors
/// Returns `Err` if `settled` is a non-local escape (`Break::Escape`); a
/// value or a recoverable runtime error (`Break::Error`) becomes `Ok`.
pub fn split(settled: super::flow::Settled<Value>) -> Result<BodyResult, Escape> {
    match settled {
        Ok(v) => Ok(BodyResult::Value(v)),
        Err(Break::Error(e)) => Ok(BodyResult::Error(e)),
        Err(Break::Escape(esc)) => Err(esc),
    }
}
