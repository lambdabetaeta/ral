//! The evaluator's control-flow currencies: `Escape` and `Break` for exits.

use super::error::Error;

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

/// A capability-policy decode/freeze failure.
///
/// The capability decoder and the sigil freeze pass ([`crate::path::sigil`])
/// answer a malformed grant with a "no", never a process exit; having no
/// `Escape` arm is how the type checker holds them to it.  A `Break` is minted
/// only at the boundary that needs one.
#[derive(Debug, Clone)]
pub struct PolicyError {
    pub message: String,
    pub hint: Option<String>,
}

impl PolicyError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            hint: None,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl From<PolicyError> for Break {
    fn from(e: PolicyError) -> Self {
        let err = Error::new(e.message, 1);
        Self::Error(match e.hint {
            Some(hint) => err.with_hint(hint),
            None => err,
        })
    }
}

/// The `as_map`/`as_list` coercions raise a bare `Error` — a shape mismatch,
/// never an exit — so the decoder absorbs one directly.
impl From<Error> for PolicyError {
    fn from(e: Error) -> Self {
        match e.hint {
            Some(hint) => Self::new(e.message).with_hint(hint),
            None => Self::new(e.message),
        }
    }
}

/// Result whose error is a [`Break`].
pub type Settled<T> = Result<T, Break>;

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
