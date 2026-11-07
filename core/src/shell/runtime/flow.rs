//! The evaluator's control-flow currencies: `Escape` and `Break` for exits.

use super::Shell;
use super::error::{Error, Prov};
use super::value::Value;
use crate::fact::{Done, Ending, ExitCode, FailureCode};
use crate::process::{CancelCause, Cancellation, Scope, ScopeId};
use crate::source::{SourceDb, Span};
use std::num::NonZeroU8;

/// A stop: consumed by its owner, caught by no `try`.
#[derive(Debug, Clone)]
pub struct Stop {
    pub cause: CancelCause,
    /// The outermost node whose own flag attains the join; `None` for a
    /// stop no node minted (the closed-stdout stop), the top's.
    pub owner: Option<ScopeId>,
    /// Where it unwound through, for the caret.
    pub at: Option<Prov>,
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
            Self::Error(e) => e.fmt(f),
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
    /// The one slot a failure's provenance lives in; an `exit` has none.
    fn prov_slot(&mut self) -> Option<&mut Option<Prov>> {
        match self {
            Self::Error(e) => Some(&mut e.prov),
            Self::Escape(Escape::Stopped(stop)) => Some(&mut stop.at),
            Self::Escape(Escape::Exit(_)) => None,
        }
    }

    /// Claims an unset provenance for `prov`.
    pub(crate) fn claimed_by(mut self, prov: impl FnOnce() -> Prov) -> Self {
        if let Some(slot) = self.prov_slot() {
            slot.get_or_insert_with(prov);
        }
        self
    }

    /// Claims an unset provenance for the frame `name` at `span`; a span the
    /// session cannot resolve claims nothing.
    pub(crate) fn locate(self, name: &str, span: Option<Span>, shell: &Shell) -> Self {
        match span {
            Some(span) if shell.session.sources.get(span.file).is_some() => {
                self.claimed_by(|| Prov::Live {
                    name: name.to_owned().into(),
                    span,
                })
            }
            _ => self,
        }
    }

    /// The one minting site for a stop: every poll point, every write a
    /// sink stopped, every child's teardown.
    pub(crate) fn stopped(cause: CancelCause, owner: Option<ScopeId>) -> Self {
        Self::Escape(Escape::Stopped(Stop {
            cause,
            owner,
            at: None,
        }))
    }

    /// A child cancelled under `scope`, the current scope it was waited
    /// under: a stop carrying the join on it.  The join is `None` when the
    /// cause came from a collector's own `sent` alone (an anchor's death,
    /// written on an external stage): the stop is then unnamed, nobody's but
    /// the host's.
    pub(crate) fn cancelled(cause: CancelCause, scope: &Scope) -> Self {
        match scope.in_force() {
            Some(join) => Self::from(join),
            None => Self::stopped(cause, None),
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
        (Some(join), Err(Break::Escape(Escape::Stopped(stop))))
            if stop.cause > join.cause
                || (stop.cause == join.cause && stop.owner.as_ref() == Some(&join.owner)) =>
        {
            Err(Break::Escape(Escape::Stopped(stop)))
        }
        (Some(join), _) => Err(join.into()),
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

    /// The ending leaves the shell `sources` belongs to: every provenance
    /// into a file the parent registry lacks is resolved first, so no span
    /// crosses to a registry that cannot read it.
    pub(crate) fn leaving(mut self, sources: &SourceDb) -> Self {
        let slot = match &mut self {
            Self::Err(error) => &mut error.prov,
            Self::Stopped(stop) => &mut stop.at,
            Self::Ok(_) | Self::Exited(_) | Self::Panic(_) => return self,
        };
        *slot = slot.take().map(|prov| prov.leaving(sources));
        self
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
    use crate::fact::Reason;
    use crate::process::{CancelCause as C, DurableRoot, RootCause, ScopeNode};
    use crate::source::FileId;

    fn nz(n: u8) -> NonZeroU8 {
        NonZeroU8::new(n).unwrap()
    }

    #[test]
    fn cancellation_becomes_a_noncatchable_stop_naming_its_owner() {
        let root = DurableRoot::default();
        let child = root.child();
        root.cancel(RootCause::Terminated);
        child.cancel(C::Terminated);

        let stop = Break::from(child.poll().expect_err("cancelled"));
        assert!(matches!(
            stop,
            Break::Escape(Escape::Stopped(Stop {
                cause: C::Terminated,
                owner: Some(owner),
                at: None,
            })) if owner == root.id()
        ));
    }

    #[test]
    fn every_end_has_its_code() {
        let error = |reason| End::Err(Error::from_reason(reason));
        assert_eq!(End::Ok(Value::Unit).code(), 0);
        assert_eq!(error(Reason::Raised("x".into())).code(), 1);
        assert_eq!(
            error(Reason::process("tool", crate::fact::Failure::Exited(3))).code(),
            3
        );
        assert_eq!(error(Reason::TimedOut).code(), 124);
        assert_eq!(error(Reason::Cancelled).code(), 143);
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

    fn stopped(cause: C, owner: Option<ScopeId>, at: Option<Prov>) -> Settled<Value> {
        Err(Break::Escape(Escape::Stopped(Stop { cause, owner, at })))
    }

    fn node_under(cause: C) -> ScopeNode {
        let node = DurableRoot::default().worker();
        node.cancel(cause);
        node
    }

    #[test]
    fn final_poll_prefers_the_join_and_keeps_a_located_equal() {
        let sp = || Prov::Live {
            name: "sleep".into(),
            span: Span::new(FileId(1), 0, 4),
        };
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
                stopped(C::Interrupted, Some(node.id()), Some(sp())),
                &node
            )),
            (C::Interrupted, Some(node.id()), Some(sp()))
        );
        let other = crate::process::DurableRoot::default().worker().id();
        assert_eq!(
            cause(final_poll(
                stopped(C::Interrupted, Some(other), Some(sp())),
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
            final_poll(Err(Break::Error(Error::raised("x"))), &calm),
            Err(Break::Error(e)) if e.failure_code() == nz(1)
        ));
    }

    /// A forked registry keeps its inherited ids live and resolves the rest:
    /// both an error's and a stop's provenance, as a stage thread's ending
    /// carries either out of its shell.
    #[test]
    fn an_ending_resolves_provenance_past_the_forked_prefix() {
        use crate::source::{Source, SourceDb, SourceOrigin};
        let source = |name: &str| Source::from_text(SourceOrigin::named(name), "a\nb\n");
        let mut parent = SourceDb::default();
        let inherited = parent.register(source("parent"));
        let mut child = parent.fork();
        let own = child.register(source("own"));
        let live = |file| Prov::Live {
            name: "fail".into(),
            span: Span::new(file, 2, 3),
        };

        let kept = End::Err(Error {
            prov: Some(live(inherited)),
            ..Error::raised("x")
        })
        .leaving(&child);
        assert!(matches!(
            kept,
            End::Err(Error {
                prov: Some(Prov::Live { .. }),
                ..
            })
        ));

        let End::Stopped(stop) = (End::Stopped(Stop {
            cause: C::Cancelled,
            owner: None,
            at: Some(live(own)),
        }))
        .leaving(&child) else {
            unreachable!()
        };
        let Some(Prov::Resolved(pair)) = stop.at else {
            panic!("a stop into the child's own file resolves: {:?}", stop.at)
        };
        assert_eq!(
            (pair.0.as_str(), &*pair.1.script, pair.1.line),
            ("fail", "own", 2)
        );
    }
}
