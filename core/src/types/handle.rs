//! The shared cells behind [`Value::Handle`](super::Value): what a spawned
//! worker writes and its observers read.

use super::flow::ThreadEnd;
use super::value::Value;
use crate::fact::WorkerId;
use crate::io::ByteBuffer;
use crate::process::ScopeNode;
use crate::sync::LockExt as _;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

pub(crate) mod brood;
pub(crate) mod surface;

/// One instant on both clocks: the monotonic one durations are measured on,
/// the wall one a reader is shown.
#[derive(Clone, Copy, Debug)]
pub struct Stamp {
    pub mono: Instant,
    pub wall: SystemTime,
}

impl Stamp {
    pub fn now() -> Self {
        Self {
            mono: Instant::now(),
            wall: SystemTime::now(),
        }
    }
}

/// Whether `v` structurally reaches a running handle — the binding-lease
/// reaper's pin check, so a name still holding live work is never pruned.
/// Closure captures are never descended, the same refusal
/// [`Value::shallow_size`] makes: the worker registry retains the handle
/// regardless, so nothing is stranded either way.
pub(crate) fn pins_running_work(v: &Value) -> bool {
    match v {
        Value::Handle(h) => h.is_running(),
        _ => v
            .try_for_each_child(&mut |c| {
                if pins_running_work(c) {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })
            .is_break(),
    }
}

/// Lifecycle of a spawned computation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleState {
    Running,
    Completed,
    Cancelled,
}

/// A finished block's outcome, cached on completion: bytes drained from the
/// handle's buffers exactly once, paired with its result.
///
/// Every eliminator (`await`, `race`, `poll`) projects this rather than
/// re-reading the live buffers, so repeat observations agree and a failed
/// block's bytes survive.
#[derive(Debug, Clone)]
pub struct CompletedHandle {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub(crate) end: ThreadEnd,
}

/// Shared handle to a spawned computation.
#[derive(Debug, Clone)]
#[allow(clippy::type_complexity)]
pub struct HandleInner {
    /// Result channel.  The worker's body is its own closed machine run
    /// (`machine::evaluate`), already settled before it reaches the
    /// channel — nothing to absorb crossing the thread boundary.
    pub result: Arc<Mutex<Option<Receiver<ThreadEnd>>>>,
    /// Filled once, when the block completes and the buffers drain into it;
    /// every later observation reads it instead of the channel.
    pub cached: Arc<Mutex<Option<CompletedHandle>>>,
    /// Lock order: the worker registry lock may take this lock (a brief read
    /// in `WorkerRegistry::reserve` and `WorkerRegistry::sweep_retention`),
    /// never the reverse.
    pub state: StateCell,
    /// Buffered stdout, drained into `cached` on completion.  Empty for a
    /// watched handle, whose lines surface live through `Sink::Watch`.
    pub stdout_buf: ByteBuffer,
    pub stderr_buf: ByteBuffer,
    pub id: WorkerId,
    /// What the work is, for people; nothing looks a worker up by it.
    pub title: String,
    pub started: Stamp,
    /// Set once, when the worker thread ends.
    pub(crate) ended: Arc<OnceLock<Stamp>>,
    /// An eliminator settled this handle; a brood or litter answers for the
    /// complement.
    pub(crate) claimed: Arc<AtomicBool>,
    /// Where an observer checks the value the worker settles with, set by
    /// `worker`: what the worker returns was decided by another unit, so the
    /// unit that reacquires its handle admits it against the type it uses it
    /// at.  Each `worker` call is a site of its own.
    pub site: Option<Arc<crate::ty::Site>>,
    /// The worker's own node: a child of its cradle's for a loose birth (the
    /// session root in a run, the parent worker's node inside one), of the
    /// call's scope for a combinator's child.  `cancel`, a halting parent and
    /// `race`'s losers fire it, and the worker stops at its next poll.
    pub cancel: ScopeNode,
}

/// A handle's lifecycle, read and transitioned only through [`HandleInner`]'s
/// methods: a guard born in a caller's expression lives to the end of that
/// expression — long enough to re-enter this lock, or to take the registry's
/// against the documented order — so none is ever handed out.
#[derive(Debug, Clone)]
pub struct StateCell(Arc<Mutex<HandleState>>);

impl PartialEq for HandleInner {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.result, &other.result)
    }
}

impl HandleInner {
    /// A running handle, born now, whose worker (if any) settles through
    /// `result`; its buffers start empty.
    pub fn new(
        id: WorkerId,
        title: impl Into<String>,
        cancel: ScopeNode,
        result: Option<Receiver<ThreadEnd>>,
    ) -> Self {
        Self {
            result: Arc::new(Mutex::new(result)),
            cached: Arc::new(Mutex::new(None)),
            state: StateCell(Arc::new(Mutex::new(HandleState::Running))),
            stdout_buf: ByteBuffer::default(),
            stderr_buf: ByteBuffer::default(),
            id,
            title: title.into(),
            started: Stamp::now(),
            ended: Arc::default(),
            claimed: Arc::default(),
            site: None,
            cancel,
        }
    }

    pub fn state(&self) -> HandleState {
        *self.state.0.lock_ignore_poison()
    }

    pub(crate) fn is_running(&self) -> bool {
        self.state() == HandleState::Running
    }

    /// A running worker's age so far, a settled worker's run time.
    pub fn elapsed(&self) -> Duration {
        self.ended
            .get()
            .map_or_else(Instant::now, |e| e.mono)
            .saturating_duration_since(self.started.mono)
    }

    /// When the worker thread ended, on the wall clock.
    pub fn settled_at(&self) -> Option<SystemTime> {
        self.ended.get().map(|e| e.wall)
    }

    pub(crate) fn claim(&self) {
        self.claimed.store(true, Ordering::Release);
    }

    pub(crate) fn is_claimed(&self) -> bool {
        self.claimed.load(Ordering::Acquire)
    }

    /// Whether the worker thread has ended, whatever any eliminator saw.
    pub(crate) fn has_ended(&self) -> bool {
        self.ended.get().is_some()
    }

    /// Stop the worker: the policy `cancel`, a halting parent and `race`'s
    /// losers share.  An already-completed handle keeps its cached outcome,
    /// so a finished worker's value is never destroyed by a stop that lost the
    /// toss; [`Self::detach`]'s guard spans the test and the transition.
    pub(crate) fn stop(&self) {
        if self.detach() {
            self.cancel.cancel(crate::process::CancelCause::Cancelled);
        }
    }

    /// The worker's exit mark.  Guarded: an eliminator may have won the
    /// transition, and a `cancel`'s `Cancelled` must not be undone.
    pub(crate) fn complete(&self) {
        let _ = self.ended.set(Stamp::now());
        let mut state = self.state.0.lock_ignore_poison();
        if *state == HandleState::Running {
            *state = HandleState::Completed;
        }
    }

    /// `cancel`'s transition: release the receiver and the cache with the
    /// state, as one step.  `false` if the worker had already completed, whose
    /// outcome is left alone.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the guard's span is the point: dropping it earlier reopens the window in which a worker completes between the test and the transition and loses its outcome"
    )]
    pub(crate) fn detach(&self) -> bool {
        let mut state = self.state.0.lock_ignore_poison();
        if *state == HandleState::Completed {
            return false;
        }
        *state = HandleState::Cancelled;
        self.result.lock_ignore_poison().take();
        *self.cached.lock_ignore_poison() = None;
        true
    }

    /// Non-blocking settle: `Some` if the handle has an outcome to observe now
    /// (the cache, or a just-arrived channel message drained into it), `None`
    /// while the worker runs.  A `Disconnected` receiver means the worker
    /// dropped its `Sender` unsent — it panicked — so it settles as a failure
    /// rather than a `None` that `poll` and `race` would read as still-running.
    ///
    /// Settling is once-only, and a transition: `state` is held across the
    /// cache read, `try_recv` and the cache write, so a second awaiter either
    /// sees the first's cached outcome or blocks, and a concurrent `cancel`
    /// waits its turn.  Draining on the error path too captures a failed
    /// block's bytes.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the guard must span the whole transition, or a second awaiter observes a bare Disconnected"
    )]
    pub(crate) fn try_settle(&self) -> Option<CompletedHandle> {
        let mut state = self.state.0.lock_ignore_poison();
        let cached = self.cached.lock_ignore_poison().clone();
        if let Some(completed) = cached {
            return Some(completed);
        }
        let mut rx_guard = self.result.lock_ignore_poison();
        let rx = rx_guard.as_ref()?;
        let end = match rx.try_recv() {
            Ok(end) => end,
            // The worker names itself: the eliminator that found the corpse is
            // not what panicked, and `await`, `poll` and `race` all arrive here.
            Err(TryRecvError::Disconnected) => {
                ThreadEnd::Panicked(format!("{}: spawned thread panicked", self.title))
            }
            Err(TryRecvError::Empty) => return None,
        };
        rx_guard.take();
        drop(rx_guard);
        if *state == HandleState::Running {
            *state = HandleState::Completed;
        }
        let completed = CompletedHandle {
            stdout: self.stdout_buf.take(),
            stderr: self.stderr_buf.take(),
            end,
        };
        *self.cached.lock_ignore_poison() = Some(completed.clone());
        Some(completed)
    }
}

/// A running handle with no worker behind it.
#[cfg(test)]
pub(crate) fn idle_handle() -> Value {
    Value::Handle(Box::new(HandleInner::new(
        WorkerId::mint(),
        "<test>",
        crate::process::DurableRoot::default().worker(),
        None,
    )))
}
