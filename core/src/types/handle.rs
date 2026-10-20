//! The shared cells behind [`Value::Handle`](super::Value): what a spawned
//! worker writes and its observers read.

use super::flow::ThreadEnd;
use super::value::Value;
use crate::fact::WorkerId;
use crate::io::ByteBuffer;
use crate::process::ScopeNode;
use crate::sync::LockExt as _;
use crate::types::Done;
use std::ops::ControlFlow;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};
use surface::DeferredSurface;

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

/// A worker's result, claimed once: running until its worker settles it,
/// then held until one caller claims it.
#[derive(Debug)]
enum HandleState {
    Running,
    Settled(CompletedHandle),
    Claimed,
}

/// What an observer finds in a handle, its settled result lent as `R`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sample<R> {
    Pending,
    Settled(R),
    Claimed,
}

/// A finished block's outcome: the bytes it wrote, paired with its result.
#[derive(Debug, Clone)]
pub struct CompletedHandle {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub(crate) end: ThreadEnd,
}

/// Shared handle to a spawned computation.
#[derive(Debug, Clone)]
pub struct HandleInner {
    /// Lock order: the worker registry lock may take this lock, never the
    /// reverse.
    state: Arc<Mutex<HandleState>>,
    /// Drained into the result when the worker settles.  Empty for a watched
    /// handle, whose lines surface live through `Sink::Watch`.
    stdout_buf: ByteBuffer,
    stderr_buf: ByteBuffer,
    pub id: WorkerId,
    /// What the work is, for people; nothing looks a worker up by it.
    pub title: String,
    pub started: Stamp,
    /// Set once, when the worker thread ends, claimed or not.
    ended: Arc<OnceLock<Stamp>>,
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

impl PartialEq for HandleInner {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }
}

impl HandleInner {
    /// A running handle, born now over the buffers its worker writes, and the
    /// one [`Settler`] that worker settles it with.
    pub(crate) fn new(
        id: WorkerId,
        title: impl Into<String>,
        cancel: ScopeNode,
        stdout_buf: ByteBuffer,
        stderr_buf: ByteBuffer,
    ) -> (Self, Settler) {
        let handle = Self {
            state: Arc::new(Mutex::new(HandleState::Running)),
            stdout_buf,
            stderr_buf,
            id,
            title: title.into(),
            started: Stamp::now(),
            ended: Arc::default(),
            site: None,
            cancel,
        };
        let settler = Settler {
            handle: handle.clone(),
            surface: None,
            settled: false,
        };
        (handle, settler)
    }

    pub fn sample(&self) -> Sample<()> {
        self.observe(|_| ())
    }

    pub(crate) fn is_running(&self) -> bool {
        self.sample() == Sample::Pending
    }

    pub(crate) fn is_claimed(&self) -> bool {
        self.sample() == Sample::Claimed
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

    /// Whether the worker thread has ended, whatever any eliminator saw.
    pub(crate) fn has_ended(&self) -> bool {
        self.ended.get().is_some()
    }

    /// What the worker has written so far, without draining it.
    pub(crate) fn written(&self) -> (Vec<u8>, Vec<u8>) {
        (self.stdout_buf.peek(), self.stderr_buf.peek())
    }

    /// Observe without claiming.  `f` runs under the handle's lock and must
    /// not touch the handle.
    pub(crate) fn observe<R>(&self, f: impl FnOnce(&CompletedHandle) -> R) -> Sample<R> {
        match &*self.state.lock_ignore_poison() {
            HandleState::Running => Sample::Pending,
            HandleState::Settled(completed) => Sample::Settled(f(completed)),
            HandleState::Claimed => Sample::Claimed,
        }
    }

    /// Claim a settled result, moving it out.
    pub(crate) fn claim_settled(&self) -> Sample<CompletedHandle> {
        let mut state = self.state.lock_ignore_poison();
        match std::mem::replace(&mut *state, HandleState::Claimed) {
            HandleState::Running => {
                *state = HandleState::Running;
                Sample::Pending
            }
            HandleState::Settled(completed) => Sample::Settled(completed),
            HandleState::Claimed => Sample::Claimed,
        }
    }

    /// Claim whatever the handle holds, stopping a running worker and
    /// discarding a settled result.  `false` if it was already claimed.
    pub(crate) fn claim_and_stop(&self) -> bool {
        let was = std::mem::replace(&mut *self.state.lock_ignore_poison(), HandleState::Claimed);
        match was {
            HandleState::Running => {
                self.cancel.cancel(crate::process::CancelCause::Cancelled);
                true
            }
            HandleState::Settled(_) => true,
            HandleState::Claimed => false,
        }
    }

    /// The worker's end: a claimed handle's result is dropped.
    fn complete(&self, end: ThreadEnd) {
        let _ = self.ended.set(Stamp::now());
        let mut state = self.state.lock_ignore_poison();
        if matches!(*state, HandleState::Running) {
            *state = HandleState::Settled(CompletedHandle {
                stdout: self.stdout_buf.take(),
                stderr: self.stderr_buf.take(),
                end,
            });
        }
    }
}

/// The worker's end of its handle: settles it once, flushing the worker's
/// deferred surface first.  Dropped unsettled, by a panic, it settles the
/// handle as one.
pub(crate) struct Settler {
    handle: HandleInner,
    surface: Option<Arc<DeferredSurface>>,
    settled: bool,
}

impl Settler {
    /// Flush `surface` as the worker settles.
    pub(crate) fn flushing(mut self, surface: Option<Arc<DeferredSurface>>) -> Self {
        self.surface = surface;
        self
    }

    pub(crate) fn settle(mut self, end: ThreadEnd, done: Done) {
        self.finish(end, done);
    }

    fn finish(&mut self, end: ThreadEnd, done: Done) {
        self.settled = true;
        if let Some(surface) = &self.surface {
            surface.flush(&self.handle, done);
        }
        self.handle.complete(end);
    }
}

impl Drop for Settler {
    fn drop(&mut self) {
        if !self.settled {
            let end =
                ThreadEnd::Panicked(format!("{}: spawned thread panicked", self.handle.title));
            self.finish(end, Done::Panic("spawned thread panicked".into()));
        }
    }
}

/// A handle whose worker never ran, settled as a panic: for tests that only
/// carry one.
#[cfg(test)]
pub(crate) fn idle_handle() -> Value {
    let (handle, _) = HandleInner::new(
        WorkerId::mint(),
        "<test>",
        crate::process::DurableRoot::default().worker(),
        ByteBuffer::default(),
        ByteBuffer::default(),
    );
    Value::Handle(Box::new(handle))
}
