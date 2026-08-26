//! The evaluator's control-flow currencies: `Escape` and `Break` for exits.

use super::error::Error;

/// Non-catchable exits from a delimited scope.
#[derive(Debug, Clone)]
pub enum Escape {
    Exit(i32),
    /// A foreground job frozen by SIGTSTP, on its way to the job table.
    ///
    /// `pending` carries the atomic writes the stopped frames staged but could
    /// not finish: the child is alive with each temp still open, so the writes
    /// belong to the job now, and its end decides between the rename and the
    /// unlink.  They travel as paths precisely so this stays a `Clone` datum —
    /// an escape describes a control transfer, it never owns a resource.
    #[cfg(unix)]
    Stopped {
        pgid: crate::process::Pgid,
        signal: crate::process::Signal,
        cmd: String,
        pending: Vec<crate::runtime::command::PendingWrite>,
    },
}

/// What `try` decides about: `Error` is catchable; `Escape` propagates.
#[derive(Debug, Clone)]
pub enum Break {
    Error(Error),
    Escape(Escape),
}

impl Break {
    /// True for the one break that leaves a live job behind it, so what the
    /// frame staged is unfinished rather than abandoned.
    pub(crate) fn is_stop(&self) -> bool {
        #[cfg(unix)]
        return matches!(self, Self::Escape(Escape::Stopped { .. }));
        #[cfg(not(unix))]
        return false;
    }
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
