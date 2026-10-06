//! The shared cells behind [`Value::Handle`](super::Value): what a spawned
//! worker writes and its observers read.

use super::flow::Settled;
use super::value::Value;
use crate::io::ByteBuffer;
use crate::sync::LockExt as _;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub(crate) mod surface;

/// A once-only claim, contested across threads: the first [`Self::claim`] wins.
#[derive(Debug, Clone, Default)]
pub struct Latch(Arc<AtomicBool>);

impl Latch {
    pub(crate) fn claim(&self) -> bool {
        !self.0.swap(true, Ordering::AcqRel)
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
    /// Drained once from [`HandleInner::surface_buf`], and replayed through
    /// the awaiting run's surface by `await`/`race` — never by `poll`.
    pub surface: Vec<crate::first_order::FOValue>,
    pub(crate) outcome: super::flow::Settled<Value>,
}

/// Bounded buffer of the structured events a *detached* worker defers rather
/// than emitting live, its spawning run having possibly ended.
///
/// The bound lives in [`surface::DeferredSurface`], so a runaway emitter
/// cannot grow this without limit.
pub type SurfaceBuffer = Arc<Mutex<Vec<crate::first_order::FOValue>>>;

/// Shared handle to a spawned computation.
#[derive(Debug, Clone)]
#[allow(clippy::type_complexity)]
pub struct HandleInner {
    /// Result channel.  The worker's body is its own closed machine run
    /// (`machine::evaluate`), already settled before it reaches the
    /// channel — nothing to absorb crossing the thread boundary.
    pub result: Arc<Mutex<Option<std::sync::mpsc::Receiver<super::flow::Settled<Value>>>>>,
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
    /// Where a *detached* worker's `surface` events land, the spawning run's
    /// live sink being possibly gone.  Drained once into
    /// [`CompletedHandle::surface`].
    pub surface_buf: SurfaceBuffer,
    /// Deliver-once latch, contested by the two renderers of the deferred
    /// batch: an eliminator's replay and the host's boundary delivery.  The
    /// loser skips, so a batch never renders twice and a never-awaited worker
    /// still delivers exactly once.
    pub joined: Latch,
    /// When an eliminator last named this handle: renewed by `poll` and by
    /// every `await`/`race` sweep, read by the idle lease chain in
    /// `builtins::concurrency`.  Cancelling or listing never renews it.
    pub last_observed: Observed,
    pub cmd: std::string::String,
    /// Where an observer checks the value the worker settles with, set by
    /// `service-handle`: what the worker returns was decided by another unit,
    /// so the unit that reacquires its handle admits it against the type it
    /// uses it at.  Each `service-handle` call is a site of its own.
    pub site: Option<Arc<crate::ty::Site>>,
    /// The worker's own scope — a `DurableRoot::worker()` child of the
    /// *session* root, not of the spawning run, so a foreground interrupt
    /// cannot collaterally kill it.  `cancel` and `race`'s losers fire it, and
    /// the worker stops at its next poll rather than running on detached.
    pub cancel: crate::process::CancelScope,
}

/// A handle's lifecycle, read and transitioned only through [`HandleInner`]'s
/// methods: a guard born in a caller's expression lives to the end of that
/// expression — long enough to re-enter this lock, or to take the registry's
/// against the documented order — so none is ever handed out.
#[derive(Debug, Clone)]
pub struct StateCell(Arc<Mutex<HandleState>>);

/// When a handle was last named by an eliminator; sealed as [`StateCell`] is.
#[derive(Debug, Clone)]
pub struct Observed(Arc<Mutex<Instant>>);

impl PartialEq for HandleInner {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.result, &other.result)
    }
}

impl HandleInner {
    /// A running handle, observed now, whose worker (if any) settles through
    /// `result`; its buffers start empty.
    pub fn new(
        cmd: impl Into<std::string::String>,
        cancel: crate::process::CancelScope,
        result: Option<Receiver<Settled<Value>>>,
    ) -> Self {
        Self {
            result: Arc::new(Mutex::new(result)),
            cached: Arc::new(Mutex::new(None)),
            state: StateCell(Arc::new(Mutex::new(HandleState::Running))),
            stdout_buf: ByteBuffer::default(),
            stderr_buf: ByteBuffer::default(),
            surface_buf: Arc::new(Mutex::new(Vec::new())),
            joined: Latch::default(),
            last_observed: Observed(Arc::new(Mutex::new(Instant::now()))),
            cmd: cmd.into(),
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

    pub fn last_observed(&self) -> Instant {
        *self.last_observed.0.lock_ignore_poison()
    }

    /// An eliminator named this handle.
    pub(crate) fn renew(&self) {
        *self.last_observed.0.lock_ignore_poison() = Instant::now();
    }

    /// The worker's exit mark.  Guarded: an eliminator may have won the
    /// transition, and a `cancel`'s `Cancelled` must not be undone.
    pub(crate) fn complete(&self) {
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
        let outcome = match rx.try_recv() {
            Ok(result) => result,
            // The worker names itself: the eliminator that found the corpse is
            // not what panicked, and `await`, `poll` and `race` all arrive here.
            Err(TryRecvError::Disconnected) => Err(super::coerce::sig(format!(
                "{}: spawned thread panicked",
                self.cmd
            ))),
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
            surface: std::mem::take(&mut *self.surface_buf.lock_ignore_poison()),
            outcome,
        };
        *self.cached.lock_ignore_poison() = Some(completed.clone());
        Some(completed)
    }
}

/// A running handle with no worker behind it.
#[cfg(test)]
pub(crate) fn idle_handle() -> Value {
    Value::Handle(Box::new(HandleInner::new(
        "<test>",
        crate::process::CancelScope::default(),
        None,
    )))
}
