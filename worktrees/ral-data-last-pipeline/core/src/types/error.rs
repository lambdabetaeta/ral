//! Runtime error types and eval signals.
//!
//! [`EvalSignal`] is the non-local control flow type threaded through all
//! evaluator operations.  [`Error`] is a located runtime error with an
//! optional hint and an [`ErrorKind`] tag used by `_try-apply` to
//! distinguish pattern-mismatch failures from other errors.

use std::fmt;
use super::value::Value;

/// Classification of runtime errors.  Used by `_try-apply` (SPEC §16.4) to
/// catch only pattern-mismatch failures while letting other errors propagate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ErrorKind {
    #[default]
    Other,
    /// Destructuring a function parameter failed (wrong shape, missing key,
    /// wrong length).  Any other failure is `Other`.
    PatternMismatch,
}

#[derive(Debug, Clone)]
pub struct Error {
    pub message: String,
    pub status: i32,
    pub loc: Option<crate::diagnostic::SourceLoc>,
    pub hint: Option<String>,
    pub kind: ErrorKind,
}

impl Error {
    pub fn new(message: impl Into<String>, status: i32) -> Self {
        Error {
            message: message.into(),
            status,
            loc: None,
            hint: None,
            kind: ErrorKind::Other,
        }
    }

    pub fn with_kind(mut self, kind: ErrorKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn at(mut self, line: usize, col: usize) -> Self {
        self.loc = Some(crate::diagnostic::SourceLoc {
            file: String::new(),
            line,
            col,
            len: 0,
        });
        self
    }

    pub fn at_loc(mut self, loc: crate::diagnostic::SourceLoc) -> Self {
        self.loc = Some(loc);
        self
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Error {}

/// Non-local control-flow signal returned by every evaluator operation.
#[derive(Debug, Clone)]
pub enum EvalSignal {
    /// Runtime error or fail.
    Error(Error),
    /// exit N — clean process exit with a status code.
    Exit(i32),
    /// Tail call — propagates up to the nearest trampoline.
    /// Carries the full callee and all args so curried recursion works.
    TailCall { callee: Value, args: Vec<Value> },
}

impl From<Error> for EvalSignal {
    fn from(e: Error) -> Self {
        EvalSignal::Error(e)
    }
}

impl fmt::Display for EvalSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EvalSignal::Error(e) => write!(f, "{e}"),
            EvalSignal::Exit(code) => write!(f, "exit {code}"),
            EvalSignal::TailCall { .. } => write!(f, "<tail call>"),
        }
    }
}
