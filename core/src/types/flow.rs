//! The evaluator's control-flow currencies: `Escape` and `Break` for exits.

use super::error::Error;
use super::value::Value;

/// Non-catchable exits from a delimited scope.
#[derive(Debug, Clone)]
pub enum Escape {
    Exit(i32),
}

/// What `try` decides about: `Error` is catchable; `Escape` propagates.
#[derive(Debug, Clone)]
pub enum Break {
    Error(Error),
    Escape(Escape),
}

impl Break {
    /// The exit status this break ends a run on.
    pub fn code(&self) -> i32 {
        match self {
            Self::Error(error) => error.code(),
            Self::Escape(Escape::Exit(code)) => *code,
        }
    }
}

/// Result whose error is a [`Break`].
pub type Settled<T> = Result<T, Break>;

/// Stamp `cmd` on an error that has no command yet, so the innermost dispatch
/// wins — the rule `stamp` uses for a span.  Not an observation, and it
/// happens whether or not anyone is listening: `try`'s record needs it with
/// no trail open.  An `_`-prefixed internal name defers to the public wrapper
/// that called it.
pub(crate) fn name_failure(cmd: &str, result: &mut Settled<Value>) {
    if let Err(Break::Error(e)) = result
        && e.command.is_none()
        && !cmd.starts_with('_')
    {
        e.command = Some(cmd.into());
    }
}

impl From<Error> for Break {
    fn from(e: Error) -> Self {
        Self::Error(e)
    }
}

impl From<Escape> for Break {
    fn from(e: Escape) -> Self {
        Self::Escape(e)
    }
}
