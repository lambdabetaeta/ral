//! The evaluator's control-flow currencies: `Escape` and `Break` for exits.

use super::error::Error;
use super::value::Value;
use crate::process::{CancelCause, CancelScope, ChildEnd, ForegroundScope, ScopeId};
use crate::source::Span;

/// Non-catchable exits from a delimited scope.
#[derive(Debug, Clone)]
pub enum Escape {
    /// `exit n`: the host's alone.
    Exit(i32),
    /// A stop: consumed by the owner `owner` names, caught by no `try`.
    /// `owner` is the outermost node whose own flag attains the join;
    /// `None` for a stop no node minted (the closed-stdout stop), the top's.
    /// `at` is where it unwound through, for the caret.
    Stopped {
        cause: CancelCause,
        owner: Option<ScopeId>,
        at: Option<Span>,
    },
}

impl std::fmt::Display for Escape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exit(code) => write!(f, "exit {code}"),
            Self::Stopped { cause, .. } => f.write_str(cause.message()),
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

impl Break {
    /// The one minting site for a stop: every poll point, every write a
    /// sink stopped, every child's teardown.
    pub(crate) fn stopped(cause: CancelCause, owner: Option<ScopeId>) -> Self {
        Self::Escape(Escape::Stopped {
            cause,
            owner,
            at: None,
        })
    }

    /// A child's end: `Failed` is the command's error; `Cancelled` is a stop
    /// carrying the join on `scope`, the current scope the child was waited
    /// under.  The join is `None` when the cause came from a collector's own
    /// `sent` alone (an anchor's death, written on an external stage): the
    /// stop is then unnamed, nobody's but the host's.
    pub(crate) fn of_child(
        cmd: &str,
        end: ChildEnd,
        scope: &CancelScope,
        shell: &crate::types::Shell,
    ) -> Self {
        match end {
            ChildEnd::Failed(failure) => {
                Self::Error(Error::from_command_failure(cmd, failure, shell))
            }
            ChildEnd::Cancelled(cause) => match scope.in_force() {
                Some((join, owner)) => Self::stopped(join, Some(owner)),
                None => Self::stopped(cause, None),
            },
        }
    }

    /// The exit status this break ends a run on.
    pub fn code(&self) -> i32 {
        match self {
            Self::Error(error) => error.code(),
            Self::Escape(Escape::Exit(code)) => *code,
            Self::Escape(Escape::Stopped { cause, .. }) => i32::from(cause.code()),
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

/// A base frame's closing poll, the first half of its return clause: the
/// owner's node is read once more after the body settled, so a stop that
/// landed after the body's last poll is not lost.  A stop in force outranks
/// a value, an error and a weaker stop; a located stop is kept, for its
/// caret, when it outranks the join or equals it *naming the join's owner*:
/// an equal-cause stop naming someone else is superseded, or a base's own
/// switch would convert the wrong owner's stop.  An `exit` has already left.
pub(crate) fn final_poll(settled: Settled<Value>, node: &ForegroundScope) -> Settled<Value> {
    match (node.in_force(), settled) {
        (_, exit @ Err(Break::Escape(Escape::Exit(_)))) => exit,
        (
            Some((join, owner)),
            Err(Break::Escape(Escape::Stopped {
                cause,
                owner: named,
                at,
            })),
        ) if cause > join || (cause == join && named.as_ref() == Some(&owner)) => {
            Err(Break::Escape(Escape::Stopped {
                cause,
                owner: named,
                at,
            }))
        }
        (Some((join, owner)), _) => Err(Break::stopped(join, Some(owner))),
        (None, settled) => settled,
    }
}

/// How a thread's body ended: a worker's channel and a stage's report carry it.
#[derive(Debug, Clone)]
pub enum ThreadEnd {
    Settled(Settled<Value>),
    Panicked(String),
}

impl ThreadEnd {
    /// The body under `catch_unwind`: a thread base's first half.
    pub(crate) fn of(body: impl FnOnce() -> Settled<Value>) -> Self {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
            Ok(settled) => Self::Settled(settled),
            Err(payload) => Self::Panicked(panic_text(&*payload)),
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
    use crate::process::{CancelCause as C, DurableRoot, WorkerStop};
    use crate::source::FileId;
    use crate::types::Status;

    #[test]
    fn every_break_has_its_code() {
        let error = |status| Break::Error(Error::with_status("x", status));
        assert_eq!(error(Status::Raised(7)).code(), 7);
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
        assert_eq!(Break::Escape(Escape::Exit(5)).code(), 5);
        for cause in [C::ReaderGone, C::Interrupted, C::TimedOut, C::Aborted] {
            assert_eq!(Break::stopped(cause, None).code(), i32::from(cause.code()));
        }
    }

    fn stopped(cause: C, owner: Option<ScopeId>, at: Option<Span>) -> Settled<Value> {
        Err(Break::Escape(Escape::Stopped { cause, owner, at }))
    }

    fn node_under(cause: C) -> ForegroundScope {
        let node = DurableRoot::default().worker();
        node.cancel(cause);
        node
    }

    #[test]
    fn final_poll_prefers_the_join_and_keeps_a_located_equal() {
        let sp = Span::new(FileId(1), 0, 4);
        let node = node_under(C::Interrupted);
        let cause = |r: Settled<Value>| match r {
            Err(Break::Escape(Escape::Stopped { cause, owner, at })) => (cause, owner, at),
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
        let other = crate::process::CancelScope::root().id();
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
            final_poll(Err(Break::Error(Error::raised("x", 7))), &calm),
            Err(Break::Error(e)) if e.code() == 7
        ));
    }
}
