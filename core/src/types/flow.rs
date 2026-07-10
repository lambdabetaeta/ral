//! Control-flow types: `Escape` / `Break` / `Tail` / `TailCall` / `Control`.
//!
//! The orthogonal layers:
//!
//! - [`Escape`] — non-catchable exits from a delimited scope (process
//!   exit, stopped job).
//! - [`Break`] — what `try` decides about: `Error` (catchable) vs
//!   `Escape` (propagates).
//! - [`Tail`] — the tail-position property of an evaluation context:
//!   whether the redex sits under a trivial continuation.  Threaded as
//!   a parameter of computation evaluation and *granted* by an
//!   eliminator to a chosen sub-computation, never read from machine
//!   state.
//! - [`TailCall`] — the signal an eliminator emits when handed
//!   [`Tail::Yes`]: a callee and its arguments, absorbed by the
//!   trampoline before reaching any boundary.
//! - [`Control`] — evaluator-private union of [`Break`] and [`TailCall`];
//!   the carrier for raw evaluator returns.
//!
//! Type aliases:
//! - [`Settled<T>`] = `Result<T, Break>` — what callers outside the
//!   evaluator see after tail calls have been landed.
//! - [`Raw<T>`] = `Result<T, Control>` — evaluator-internal; may carry
//!   a tail call.
//!
//! Privacy: `TailCall`, `Control`, and `Raw` are `pub(crate)`, so the
//! type system rejects any path that would let a tail call cross a
//! public boundary.  No runtime guard is needed — the invariant is
//! enforced by Rust visibility.

use super::error::Error;
use super::value::Value;

/// Non-catchable exits from a delimited scope.
#[derive(Debug, Clone)]
pub enum Escape {
    Exit(i32),
    #[cfg(unix)]
    Stopped {
        pgid: crate::process::Pgid,
        signal: crate::process::Signal,
        cmd: String,
    },
}

/// What `try` decides about: `Error` is catchable; `Escape` propagates.
#[derive(Debug, Clone)]
pub enum Break {
    Error(Error),
    Escape(Escape),
}

/// The tail-position property of an evaluation context: whether the
/// redex sits under a trivial continuation.
///
/// Threaded as a parameter of [`eval_comp`](crate::evaluator::comp::eval_comp).
/// The default at every recursive call is [`Tail::No`]; an eliminator
/// must *choose* to grant [`Tail::Yes`], and only ever by forwarding its
/// own tail-ness to a single final sub-computation. Because tail-ness is
/// granted rather than ambient, forgetting to thread it is safe (the
/// sub-computation simply runs under a non-trivial continuation) rather
/// than wrong (a tail call escaping a live frame).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tail {
    Yes,
    No,
}

/// Evaluator-private tail-call signal. Emitted by the application and
/// `case` eliminators only when handed [`Tail::Yes`]; absorbed by the
/// trampoline before any boundary sees it.
#[derive(Debug)]
pub(crate) struct TailCall {
    pub callee: Value,
    pub args: Vec<Value>,
}

/// Evaluator-internal control-flow union. Carries either an absorbable
/// tail call or a [`Break`] propagated unchanged.
#[derive(Debug)]
pub(crate) enum Control {
    Break(Break),
    Tail(TailCall),
}

/// Result with [`Break`] error: tail calls have been absorbed.
pub type Settled<T> = Result<T, Break>;

/// Raw evaluator return: may carry an absorbable tail call.
pub(crate) type Raw<T> = Result<T, Control>;

// ── From impls ───────────────────────────────────────────────────────

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
