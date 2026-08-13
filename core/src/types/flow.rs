//! The evaluator's control-flow currencies: `Escape` and `Break` for exits,
//! `Tail` and `TailCall` for the trampoline, `Control` for their union.
//!
//! `Tail` is not machine state: an eliminator is handed its own tail-ness and
//! may grant it to one final sub-computation, so failing to thread it costs a
//! frame rather than letting a tail call escape a live one.  That a tail call
//! never crosses a public boundary is enforced by visibility alone —
//! `TailCall`, `Control` and `Raw` are `pub(crate)`, and `absorb_tail` in
//! `core/src/evaluator.rs` is the seam that lands one.

use super::error::Error;
use super::value::Value;

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

/// The tail-position property of an evaluation context: whether the redex sits
/// under a trivial continuation.  Threaded as a parameter of `eval_comp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tail {
    Yes,
    No,
}

/// The tail-call signal, emitted by the application and `case` eliminators
/// only when handed [`Tail::Yes`].
#[derive(Debug)]
pub(crate) struct TailCall {
    pub callee: Value,
    pub args: Vec<Value>,
}

/// Evaluator-internal union: an absorbable tail call, or a [`Break`] untouched.
#[derive(Debug)]
pub(crate) enum Control {
    Break(Break),
    Tail(TailCall),
}

/// Result whose error is a [`Break`]: tail calls have been absorbed.
pub type Settled<T> = Result<T, Break>;

/// Evaluator return that may still carry a tail call.
pub(crate) type Raw<T> = Result<T, Control>;

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

impl From<Break> for Control {
    fn from(b: Break) -> Self {
        Self::Break(b)
    }
}

impl From<Error> for Control {
    fn from(e: Error) -> Self {
        Self::Break(Break::Error(e))
    }
}

impl From<Escape> for Control {
    fn from(e: Escape) -> Self {
        Self::Break(Break::Escape(e))
    }
}

impl From<TailCall> for Control {
    fn from(t: TailCall) -> Self {
        Self::Tail(t)
    }
}
