//! The shared cells behind [`Value::Handle`](super::Value): what a spawned
//! worker writes and its observers read.
//!
//! **Module invariant.** [`HandleInner::new`] mints the sole
//! [`CompletionPermit`] for each handle, and terminal publication (`publish` /
//! `publish_abandoned_if_unfinished`) is private to this module, so a handle's
//! terminal transition is written at most once.

use super::flow::ThreadEnd;
use super::value::Value;
use crate::fact::{Io, WorkerId};
use crate::io::ByteBuffer;
use crate::process::{CancelCause, ScopeNode};
use crate::sync::LockExt as _;
use std::ops::ControlFlow;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

pub(crate) mod brood;
pub(crate) mod compacting;
pub(crate) mod surface;

/// One instant on both clocks: the monotonic one durations are measured on,
/// the wall one a reader is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// Whether `v` structurally reaches a live (`running` or `stopping`) handle —
/// the binding-lease reaper's pin check, so a name still holding live work is
/// never pruned.  Closure captures are never descended, the same refusal
/// [`Value::shallow_size`] makes: the worker registry retains the handle
/// regardless, so nothing is stranded either way.
pub(crate) fn pins_live_work(v: &Value) -> bool {
    match v {
        Value::Handle(h) => h.sample().is_live(),
        _ => v
            .try_for_each_child(&mut |c| {
                if pins_live_work(c) {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })
            .is_break(),
    }
}

/// A worker's lifecycle and result cell, under one mutex.  An ended worker's
/// result is `Some` while `ready` and taken (`None`) once `consumed`.
#[derive(Debug)]
enum HandleState {
    Running,
    Stopping,
    Ended {
        ended: Stamp,
        result: Option<CompletedHandle>,
    },
}

impl HandleState {
    fn ended(&self) -> Option<Stamp> {
        match self {
            Self::Running | Self::Stopping => None,
            Self::Ended { ended, .. } => Some(*ended),
        }
    }
}

/// What an observer finds in a handle: a running worker's projection as `L`,
/// its ready result lent or moved as `R`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sample<R, L = ()> {
    Running(L),
    Stopping,
    Ready(R),
    Consumed,
}

impl<R, L> Sample<R, L> {
    pub fn is_live(&self) -> bool {
        matches!(self, Self::Running(_) | Self::Stopping)
    }

    pub fn has_ended(&self) -> bool {
        !self.is_live()
    }

    pub fn is_claimed(&self) -> bool {
        matches!(self, Self::Stopping | Self::Consumed)
    }
}

/// A finished block's outcome: the bytes it wrote and its ending.
#[derive(Debug, Clone)]
pub struct CompletedHandle {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub(crate) end: ThreadEnd,
}

/// One locked lifecycle/time projection of a [`HandleInner`].
pub(crate) struct Snapshot<R, L = ()> {
    pub(crate) state: Sample<R, L>,
    pub(crate) elapsed: Duration,
    pub(crate) ended: Option<Stamp>,
}

/// Shared handle to a spawned computation.
#[derive(Debug, Clone)]
pub struct HandleInner {
    /// Lock order: registry or brood membership -> handle state -> stdout/stderr buffers.
    state: Arc<Mutex<HandleState>>,
    /// Drained when the worker publishes its terminal state.  Empty for a
    /// watched handle, whose lines surface live through `Sink::Watch`.
    stdout_buf: ByteBuffer,
    stderr_buf: ByteBuffer,
    pub id: WorkerId,
    /// What the work is, for people; nothing looks a worker up by it.
    pub title: String,
    pub started: Stamp,
    /// The worker's own node: a child of its cradle's `under`, the session
    /// root for a session birth, the gather's entry scope (a worker's own
    /// node, for its body) for a gather's child.  `cancel`, a halting parent
    /// and `race`'s losers fire it, and the worker stops at its next poll.
    pub cancel: ScopeNode,
}

impl PartialEq for HandleInner {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }
}

impl HandleInner {
    /// A running handle, born now over the buffers its worker writes, and the
    /// sole [`CompletionPermit`] that publishes its terminal state.
    pub(crate) fn new(
        id: WorkerId,
        title: impl Into<String>,
        cancel: ScopeNode,
        stdout_buf: ByteBuffer,
        stderr_buf: ByteBuffer,
    ) -> (Self, CompletionPermit) {
        let handle = Self {
            state: Arc::new(Mutex::new(HandleState::Running)),
            stdout_buf,
            stderr_buf,
            id,
            title: title.into(),
            started: Stamp::now(),
            cancel,
        };
        let permit = CompletionPermit {
            handle: handle.clone(),
        };
        (handle, permit)
    }

    /// Sample lifecycle, elapsed duration and optional end stamp under one
    /// state lock, projecting a `Running` worker through `running` and a
    /// `Ready` result through `ready`.  Both run under the handle's state lock
    /// and must not re-lock the handle or registry; `running` may lock the
    /// stdout/stderr buffers, which follow the state in the lock order.
    fn project<R, L>(
        &self,
        running: impl FnOnce() -> L,
        ready: impl FnOnce(&CompletedHandle) -> R,
    ) -> Snapshot<R, L> {
        let state = self.state.lock_ignore_poison();
        let ended = state.ended();
        let elapsed = ended
            .unwrap_or_else(Stamp::now)
            .mono
            .saturating_duration_since(self.started.mono);
        let sample = match &*state {
            HandleState::Running => Sample::Running(running()),
            HandleState::Stopping => Sample::Stopping,
            HandleState::Ended {
                result: Some(completed),
                ..
            } => Sample::Ready(ready(completed)),
            HandleState::Ended { result: None, .. } => Sample::Consumed,
        };
        drop(state);
        Snapshot {
            state: sample,
            elapsed,
            ended,
        }
    }

    /// [`Self::project`], lending a `Ready` result to `f`.
    pub(crate) fn snapshot<R>(&self, f: impl FnOnce(&CompletedHandle) -> R) -> Snapshot<R> {
        self.project(|| (), f)
    }

    pub fn sample(&self) -> Sample<()> {
        self.observe(|_| ())
    }

    /// Observe without claiming.  `f` runs under the handle's lock and must
    /// not touch the handle.
    pub(crate) fn observe<R>(&self, f: impl FnOnce(&CompletedHandle) -> R) -> Sample<R> {
        self.snapshot(f).state
    }

    /// A live worker's age so far, an ended worker's frozen run time.
    pub fn elapsed(&self) -> Duration {
        self.snapshot(|_| ()).elapsed
    }

    /// When the worker published its terminal state, on the wall clock.
    pub fn settled_at(&self) -> Option<SystemTime> {
        self.snapshot(|_| ()).ended.map(|e| e.wall)
    }

    /// Whether the worker has published its terminal state (`Ready` or `Consumed`).
    pub(crate) fn has_ended(&self) -> bool {
        self.sample().has_ended()
    }

    /// Whether the handle's result has been claimed (`Stopping` or `Consumed`).
    #[cfg(test)]
    pub(crate) fn is_claimed(&self) -> bool {
        self.sample().is_claimed()
    }

    /// Coherent snapshot for `poll`: locks state first, peeks stdout/stderr
    /// buffers only while `Running` (respecting `state -> buffers`), clones
    /// `Ready` once, and copies no bytes for `Stopping` or `Consumed`.
    pub(crate) fn snapshot_io(&self) -> Snapshot<CompletedHandle, Io> {
        self.project(
            || Io::of(self.stdout_buf.peek(), self.stderr_buf.peek()),
            CompletedHandle::clone,
        )
    }

    /// Claim a ready result, moving it out into `Consumed` without cloning.
    pub(crate) fn claim_ready(&self) -> Sample<CompletedHandle> {
        let mut state = self.state.lock_ignore_poison();
        match &mut *state {
            HandleState::Running => Sample::Running(()),
            HandleState::Stopping => Sample::Stopping,
            HandleState::Ended { result, .. } => {
                result.take().map_or(Sample::Consumed, Sample::Ready)
            }
        }
    }

    /// Claim whatever the handle holds: if `Running`, signals `Cancelled`
    /// before unlocking and transitions to `Stopping`; if `Ready`, moves the
    /// completion out to `Consumed` and drops it outside the lock; if already
    /// `Stopping` or `Consumed`, returns `false`.
    pub(crate) fn claim_and_stop(&self) -> bool {
        let mut state = self.state.lock_ignore_poison();
        let discarded = match &mut *state {
            HandleState::Running => {
                self.cancel.cancel(CancelCause::Cancelled);
                *state = HandleState::Stopping;
                return true;
            }
            HandleState::Stopping => return false,
            HandleState::Ended { result, .. } => match result.take() {
                Some(completed) => completed,
                None => return false,
            },
        };
        drop(state);
        drop(discarded);
        true
    }

    /// Publish the worker's terminal outcome once.
    fn publish(&self, end: ThreadEnd) -> Stamp {
        let state = self.state.lock_ignore_poison();
        if let Some(ended) = state.ended() {
            drop(state);
            drop(end);
            return ended;
        }
        self.install_terminal(state, end)
    }

    /// Fallback for an unfinished [`CompletionPermit`] dropped during unwind.
    fn publish_abandoned_if_unfinished(&self) {
        let state = self.state.lock_ignore_poison();
        if state.ended().is_some() {
            return;
        }
        let end = ThreadEnd::Panicked(format!("{}: spawned thread panicked", self.title));
        self.install_terminal(state, end);
    }

    fn install_terminal(&self, mut state: MutexGuard<'_, HandleState>, end: ThreadEnd) -> Stamp {
        let stdout = self.stdout_buf.take();
        let stderr = self.stderr_buf.take();
        let ended = Stamp::now();
        let completed = CompletedHandle {
            stdout,
            stderr,
            end,
        };
        let (result, discarded) = match &*state {
            HandleState::Running => (Some(completed), None),
            HandleState::Stopping => (None, Some(completed)),
            HandleState::Ended { .. } => {
                unreachable!("terminal state checked under the same lock")
            }
        };
        *state = HandleState::Ended { ended, result };
        drop(state);
        drop(discarded);
        ended
    }
}

/// The worker's sole completion capability, minted by [`HandleInner::new`].
///
/// Consuming it with [`Self::finish`] publishes the worker's [`ThreadEnd`] and
/// returns the terminal [`Stamp`].  Dropping it unfinished publishes an
/// abandoned-thread panic without invoking any host sink.
pub(crate) struct CompletionPermit {
    handle: HandleInner,
}

impl CompletionPermit {
    pub(crate) fn finish(self, end: ThreadEnd) -> Stamp {
        self.handle.publish(end)
    }
}

impl Drop for CompletionPermit {
    fn drop(&mut self) {
        self.handle.publish_abandoned_if_unfinished();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::DurableRoot;
    use std::sync::Barrier;

    fn make_handle() -> (HandleInner, CompletionPermit, ByteBuffer, ByteBuffer) {
        let out = ByteBuffer::default();
        let err = ByteBuffer::default();
        let (handle, permit) = HandleInner::new(
            WorkerId::mint(),
            "<handle-test>",
            DurableRoot::default().worker(),
            out.clone(),
            err.clone(),
        );
        (handle, permit, out, err)
    }

    #[test]
    fn running_to_ready_to_consumed_moves_buffers_without_cloning_and_freezes_stamp() {
        let (handle, permit, out, err) = make_handle();
        out.append(b"stdout-bytes");
        err.append(b"stderr-bytes");

        assert_eq!(handle.sample(), Sample::Running(()));
        assert!(handle.sample().is_live());
        assert!(!handle.sample().has_ended());
        assert!(!handle.sample().is_claimed());
        assert_eq!(handle.settled_at(), None);
        assert!(matches!(handle.claim_ready(), Sample::Running(())));

        // While Running, snapshot_io peeks buffers without draining them.
        let Sample::Running(io) = handle.snapshot_io().state else {
            panic!("expected Running");
        };
        assert_eq!(io.stdout.as_slice(), b"stdout-bytes");
        assert_eq!(io.stderr.as_slice(), b"stderr-bytes");

        let published_stamp = permit.finish(ThreadEnd::Settled(Ok(Value::Int(99))));
        assert_eq!(handle.sample(), Sample::Ready(()));
        assert!(!handle.sample().is_live());
        assert!(handle.sample().has_ended());
        assert!(!handle.sample().is_claimed());
        assert_eq!(handle.settled_at(), Some(published_stamp.wall));
        assert!(out.take().is_empty(), "stdout buffer drained on publish");
        assert!(err.take().is_empty(), "stderr buffer drained on publish");

        // Record the raw buffer pointers inside Ready before claiming to prove
        // claim_ready moves the Vecs out without cloning.
        let (ready_out_ptr, ready_err_ptr, ready_elapsed) = {
            let snap = handle.snapshot(|c| (c.stdout.as_ptr(), c.stderr.as_ptr()));
            let Sample::Ready((out_ptr, err_ptr)) = snap.state else {
                panic!("expected Ready");
            };
            assert_eq!(snap.ended, Some(published_stamp));
            (out_ptr, err_ptr, snap.elapsed)
        };

        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(
            handle.elapsed(),
            ready_elapsed,
            "elapsed is frozen at the terminal stamp"
        );

        let Sample::Ready(claimed) = handle.claim_ready() else {
            panic!("first claim_ready on Ready must succeed");
        };
        assert_eq!(claimed.stdout.as_ptr(), ready_out_ptr);
        assert_eq!(claimed.stderr.as_ptr(), ready_err_ptr);

        // Subsequent observations and claims see Consumed with the same frozen stamp.
        assert_eq!(handle.sample(), Sample::Consumed);
        assert!(!handle.sample().is_live());
        assert!(handle.sample().has_ended());
        assert!(handle.sample().is_claimed());
        assert!(matches!(handle.claim_ready(), Sample::Consumed));
        assert!(!handle.claim_and_stop());
        assert_eq!(handle.settled_at(), Some(published_stamp.wall));
        assert_eq!(handle.elapsed(), ready_elapsed);
        let snap = handle.snapshot_io();
        assert!(matches!(snap.state, Sample::Consumed));
        assert_eq!(snap.elapsed, ready_elapsed);
    }

    #[test]
    fn running_to_stopping_to_consumed_signals_under_lock_and_drains_buffers() {
        let (handle, permit, out, err) = make_handle();
        out.append(b"before-cancel");

        assert!(handle.claim_and_stop());
        assert_eq!(handle.sample(), Sample::Stopping);
        assert!(handle.cancel.is_cancelled());
        assert!(handle.sample().is_live());
        assert!(!handle.sample().has_ended());
        assert!(handle.sample().is_claimed());
        assert!(matches!(handle.claim_ready(), Sample::Stopping));
        // Second cancel in Stopping is a no-op.
        assert!(!handle.claim_and_stop());

        // Bytes written during cancellation cleanup while Stopping:
        out.append(b"-after-cancel");
        err.append(b"cleanup-stderr");
        // A stopping snapshot copies no bytes.
        assert!(matches!(handle.snapshot_io().state, Sample::Stopping));

        let stamp = permit.finish(ThreadEnd::Settled(Ok(Value::Unit)));
        assert_eq!(handle.sample(), Sample::Consumed);
        assert!(handle.has_ended());
        assert!(handle.sample().is_claimed());
        assert_eq!(handle.settled_at(), Some(stamp.wall));
        assert!(
            out.take().is_empty(),
            "Stopping -> Consumed must drain stdout_buf"
        );
        assert!(
            err.take().is_empty(),
            "Stopping -> Consumed must drain stderr_buf"
        );
        assert!(!handle.claim_and_stop());
        assert!(matches!(handle.claim_ready(), Sample::Consumed));
    }

    #[test]
    fn claim_and_stop_on_ready_discards_result_into_consumed_preserving_stamp() {
        let (handle, permit, out, _err) = make_handle();
        out.append(b"discard-me");
        let stamp = permit.finish(ThreadEnd::Settled(Ok(Value::Int(1))));
        let frozen_elapsed = handle.elapsed();

        assert!(handle.claim_and_stop());
        assert_eq!(handle.sample(), Sample::Consumed);
        assert_eq!(handle.settled_at(), Some(stamp.wall));
        assert_eq!(handle.elapsed(), frozen_elapsed);
        assert!(!handle.claim_and_stop());
        assert!(matches!(handle.claim_ready(), Sample::Consumed));
    }

    #[test]
    fn competing_claimers_yield_exactly_one_winner() {
        let (handle, permit, out, _err) = make_handle();
        out.append(b"once");
        permit.finish(ThreadEnd::Settled(Ok(Value::Int(7))));

        let barrier = Arc::new(Barrier::new(2));
        let threads = std::array::from_fn::<_, 2, _>(|_| {
            let h = handle.clone();
            let b = barrier.clone();
            std::thread::spawn(move || {
                b.wait();
                matches!(h.claim_ready(), Sample::Ready(_))
            })
        });

        let winners = threads
            .into_iter()
            .filter_map(|t| t.join().expect("claimer thread").then_some(()))
            .count();
        assert_eq!(winners, 1, "exactly one claimer must win Ready");
        assert_eq!(handle.sample(), Sample::Consumed);
    }

    #[test]
    fn observer_seeing_stopping_always_sees_cancel_signalled() {
        let (handle, _permit, _out, _err) = make_handle();
        let barrier = Arc::new(Barrier::new(2));
        let canceller_handle = handle.clone();
        let canceller_barrier = barrier.clone();
        let canceller = std::thread::spawn(move || {
            canceller_barrier.wait();
            assert!(canceller_handle.claim_and_stop());
        });

        barrier.wait();
        loop {
            match handle.sample() {
                Sample::Running(()) => std::thread::yield_now(),
                Sample::Stopping => {
                    assert!(
                        handle.cancel.is_cancelled(),
                        "cancel must be signalled before state becomes Stopping"
                    );
                    break;
                }
                other => panic!("unexpected state {other:?}"),
            }
        }
        canceller.join().unwrap();
    }

    #[test]
    fn permit_abandonment_publishes_panic_when_running_and_consumed_when_stopping() {
        // 1. Abandoned while Running -> Ready(Panicked) and buffers drained.
        let (running, running_permit, out1, err1) = make_handle();
        out1.append(b"partial-out");
        err1.append(b"partial-err");
        drop(running_permit);
        assert_eq!(out1.take(), b"");
        assert_eq!(err1.take(), b"");
        match running.claim_ready() {
            Sample::Ready(c) => {
                assert_eq!(c.stdout, b"partial-out");
                assert_eq!(c.stderr, b"partial-err");
                assert!(matches!(c.end, ThreadEnd::Panicked(ref s) if s.contains("<handle-test>")));
            }
            other => panic!("expected Ready from abandoned Running permit, got {other:?}"),
        }

        // 2. Abandoned while Stopping -> Consumed and buffers drained.
        let (stopping, stopping_permit, out2, err2) = make_handle();
        out2.append(b"stopping-out");
        err2.append(b"stopping-err");
        assert!(stopping.claim_and_stop());
        drop(stopping_permit);
        assert_eq!(stopping.sample(), Sample::Consumed);
        assert_eq!(out2.take(), b"");
        assert_eq!(err2.take(), b"");
    }

    #[test]
    fn ancestor_cancellation_leaves_handle_running_until_published() {
        let root = DurableRoot::default();
        let parent = root.worker();
        let child_node = parent.child();
        let (handle, permit) = HandleInner::new(
            WorkerId::mint(),
            "<child>",
            child_node,
            ByteBuffer::default(),
            ByteBuffer::default(),
        );
        parent.cancel(CancelCause::Cancelled);
        assert!(handle.cancel.is_cancelled());
        // Not directly claimed: still Running, not Stopping.
        assert_eq!(handle.sample(), Sample::Running(()));
        permit.finish(ThreadEnd::Settled(Ok(Value::Unit)));
        assert_eq!(handle.sample(), Sample::Ready(()));
    }
}
