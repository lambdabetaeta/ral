//! The evaluator's control-flow currencies: `Escape` and `Break` for exits.

use super::Shell;
use super::error::Error;
use super::value::Value;
use crate::fact::{Done, Ending, ExitCode, FailureCode};
use crate::process::{CancelCause, Cancellation, ChildEnd, Scope, ScopeId};
use crate::source::Span;
use std::num::NonZeroU8;

/// A stop: consumed by its owner, caught by no `try`.
#[derive(Debug, Clone)]
pub struct Stop {
    pub cause: CancelCause,
    /// The outermost node whose own flag attains the join; `None` for a
    /// stop no node minted (the closed-stdout stop), the top's.
    pub owner: Option<ScopeId>,
    /// Where it unwound through, for the caret.
    pub at: Option<Span>,
}

impl FailureCode for Stop {
    fn failure_code(&self) -> NonZeroU8 {
        self.cause.failure_code()
    }
}

/// Non-catchable exits from a delimited scope.
#[derive(Debug, Clone)]
pub enum Escape {
    /// `exit n`: the host's alone.
    Exit(u8),
    /// A stop.
    Stopped(Stop),
}

impl std::fmt::Display for Escape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exit(code) => write!(f, "exit {code}"),
            Self::Stopped(Stop { cause, .. }) => f.write_str(cause.message()),
        }
    }
}

/// What `try` decides about: `Error` is catchable; `Escape` propagates.
#[derive(Debug, Clone)]
pub enum Break {
    Error(Error),
    Escape(Escape),
}

impl std::fmt::Display for Break {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Error(e) => f.write_str(&e.message),
            Self::Escape(e) => e.fmt(f),
        }
    }
}

impl ExitCode for Break {
    fn code(&self) -> u8 {
        match self {
            Self::Error(error) => error.failure_code().get(),
            Self::Escape(Escape::Exit(code)) => *code,
            Self::Escape(Escape::Stopped(stop)) => stop.failure_code().get(),
        }
    }
}

impl Break {
    /// The one minting site for a stop: every poll point, every write a
    /// sink stopped, every child's teardown.
    pub(crate) fn stopped(cause: CancelCause, owner: Option<ScopeId>) -> Self {
        Self::Escape(Escape::Stopped(Stop {
            cause,
            owner,
            at: None,
        }))
    }

    /// A child's end: `Failed` is the command's error; `Cancelled` is
    /// [`Self::cancelled`].
    pub(crate) fn of_child(
        cmd: &str,
        end: ChildEnd,
        scope: &Scope,
        shell: &crate::shell::runtime::Shell,
    ) -> Self {
        match end {
            ChildEnd::Failed(failure) => {
                Self::Error(Error::from_command_failure(cmd, failure, shell))
            }
            ChildEnd::Cancelled(cause) => Self::cancelled(cause, scope),
        }
    }

    /// A child cancelled under `scope`, the current scope it was waited
    /// under: a stop carrying the join on it.  The join is `None` when the
    /// cause came from a collector's own `sent` alone (an anchor's death,
    /// written on an external stage): the stop is then unnamed, nobody's but
    /// the host's.
    pub(crate) fn cancelled(cause: CancelCause, scope: &Scope) -> Self {
        match scope.in_force() {
            Some((join, owner)) => Self::stopped(join, Some(owner)),
            None => Self::stopped(cause, None),
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

impl From<Cancellation> for Break {
    fn from(cancellation: Cancellation) -> Self {
        Self::stopped(cancellation.cause, Some(cancellation.owner))
    }
}

/// A base frame's closing poll, the first half of its return clause: the
/// owner's node is read once more after the body settled, so a stop that
/// landed after the body's last poll is not lost.  A stop in force outranks
/// a value, an error and a weaker stop; a located stop is kept, for its
/// caret, when it outranks the join or equals it *naming the join's owner*:
/// an equal-cause stop naming someone else is superseded, or a base's own
/// switch would convert the wrong owner's stop.  An `exit` has already left.
pub(crate) fn final_poll(settled: Settled<Value>, node: &Scope) -> Settled<Value> {
    match (node.in_force(), settled) {
        (_, exit @ Err(Break::Escape(Escape::Exit(_)))) => exit,
        (Some((join, owner)), Err(Break::Escape(Escape::Stopped(stop))))
            if stop.cause > join || (stop.cause == join && stop.owner.as_ref() == Some(&owner)) =>
        {
            Err(Break::Escape(Escape::Stopped(stop)))
        }
        (Some((join, owner)), _) => Err(Break::stopped(join, Some(owner))),
        (None, settled) => settled,
    }
}

/// How a thread's body, or a run, ended.
pub type End = Ending<Value, Error, Stop>;

impl End {
    /// The body under `catch_unwind`: a base frame's first half.
    pub(crate) fn of(body: impl FnOnce() -> Settled<Value>) -> Self {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
            Ok(settled) => Self::settled(settled),
            Err(payload) => Self::Panic(panic_text(&*payload)),
        }
    }

    pub(crate) fn settled(settled: Settled<Value>) -> Self {
        match settled {
            Ok(value) => Self::Ok(value),
            Err(Break::Error(error)) => Self::Err(error),
            Err(Break::Escape(Escape::Exit(code))) => Self::Exited(code),
            Err(Break::Escape(Escape::Stopped(stop))) => Self::Stopped(stop),
        }
    }

    /// Back into the machine's currency, a panic resumed.
    ///
    /// # Errors
    /// A failure, exit or stop becomes the corresponding [`Break`].
    pub fn into_settled(self) -> Settled<Value> {
        match self {
            Self::Ok(value) => Ok(value),
            Self::Err(error) => Err(error.into()),
            Self::Exited(code) => Err(Escape::Exit(code).into()),
            Self::Stopped(stop) => Err(Escape::Stopped(stop).into()),
            Self::Panic(text) => std::panic::resume_unwind(Box::new(text)),
        }
    }

    /// The record minted against `shell`, the stop's cause.
    pub(crate) fn done(&self, shell: &Shell) -> Done {
        match self {
            Self::Ok(_) => Done::Ok(()),
            Self::Err(error) => Done::Err(error.record(shell)),
            Self::Exited(code) => Done::Exited(*code),
            Self::Stopped(stop) => Done::Stopped(stop.cause),
            Self::Panic(text) => Done::Panic(text.clone()),
        }
    }
}

/// The message of a recovered panic payload, for either string shape.
pub(crate) fn panic_text(payload: &dyn std::any::Any) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&'static str>().map(|s| (*s).into()))
        .unwrap_or_else(|| "non-string payload".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{CancelCause as C, DurableRoot, ScopeNode, WorkerStop};
    use crate::shell::runtime::Status;
    use crate::source::FileId;

    fn nz(n: u8) -> NonZeroU8 {
        NonZeroU8::new(n).unwrap()
    }

    #[test]
    fn cancellation_becomes_a_noncatchable_stop_naming_its_owner() {
        let root = DurableRoot::default();
        let child = root.child();
        root.cancel(C::Interrupted);
        child.cancel(C::Interrupted);

        let stop = Break::from(child.poll().expect_err("cancelled"));
        assert!(matches!(
            stop,
            Break::Escape(Escape::Stopped(Stop {
                cause: C::Interrupted,
                owner: Some(owner),
                at: None,
            })) if owner == root.id()
        ));
    }

    #[test]
    fn every_end_has_its_code() {
        let error = |status| End::Err(Error::with_status("x", status));
        assert_eq!(End::Ok(Value::Unit).code(), 0);
        assert_eq!(error(Status::Raised(nz(7))).code(), 7);
        assert_eq!(
            error(Status::Process(crate::process::CommandFailure::ExitCode(3))).code(),
            3
        );
        assert_eq!(
            error(Status::WorkerStopped(WorkerStop::TimedOut)).code(),
            124
        );
        assert_eq!(
            error(Status::WorkerStopped(WorkerStop::Cancelled)).code(),
            143
        );
        assert_eq!(End::Exited(5).code(), 5);
        for cause in [C::ReaderGone, C::Interrupted, C::TimedOut, C::Aborted] {
            assert_eq!(
                End::Stopped(Stop {
                    cause,
                    owner: None,
                    at: None
                })
                .code(),
                cause.failure_code().get()
            );
        }
        assert_eq!(End::Panic("x".into()).code(), crate::fact::PANIC_CODE);
    }

    fn stopped(cause: C, owner: Option<ScopeId>, at: Option<Span>) -> Settled<Value> {
        Err(Break::Escape(Escape::Stopped(Stop { cause, owner, at })))
    }

    fn node_under(cause: C) -> ScopeNode {
        let node = DurableRoot::default().worker();
        node.cancel(cause);
        node
    }

    #[test]
    fn final_poll_prefers_the_join_and_keeps_a_located_equal() {
        let sp = Span::new(FileId(1), 0, 4);
        let node = node_under(C::Interrupted);
        let cause = |r: Settled<Value>| match r {
            Err(Break::Escape(Escape::Stopped(Stop { cause, owner, at }))) => (cause, owner, at),
            other => panic!("not a stop: {other:?}"),
        };

        assert_eq!(
            cause(final_poll(Ok(Value::Unit), &node)),
            (C::Interrupted, Some(node.id()), None)
        );
        assert_eq!(
            cause(final_poll(
                stopped(C::Interrupted, Some(node.id()), Some(sp)),
                &node
            )),
            (C::Interrupted, Some(node.id()), Some(sp))
        );
        let other = crate::process::DurableRoot::default().worker().id();
        assert_eq!(
            cause(final_poll(
                stopped(C::Interrupted, Some(other), Some(sp)),
                &node
            )),
            (C::Interrupted, Some(node.id()), None)
        );
        let aborted = node_under(C::Aborted);
        assert_eq!(
            cause(final_poll(stopped(C::Interrupted, None, None), &aborted)).0,
            C::Aborted
        );
        assert!(matches!(
            final_poll(Err(Break::Escape(Escape::Exit(3))), &aborted),
            Err(Break::Escape(Escape::Exit(3)))
        ));
        let calm = DurableRoot::default().worker();
        assert!(matches!(
            final_poll(Err(Break::Error(Error::raised("x", nz(7)))), &calm),
            Err(Break::Error(e)) if e.failure_code() == nz(7)
        ));
    }
}
