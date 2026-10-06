//! A detached worker's surface: the events it emits while no run listens,
//! buffered until an eliminator replays them or the completion delivers them.

use super::{Latch, SurfaceBuffer};
use crate::first_order::FOValue;
use crate::sync::LockExt as _;
use crate::types::{DeferredSink, Done, DoneEvent, EventSink};
use std::sync::Arc;

/// Cap on a detached worker's deferred surface: past it one `surface-overflow`
/// marker is recorded and further events drop.
const DEFERRED_SURFACE_CAP: usize = 4096;

/// A detached worker's surface: it may outlive the run that spawned it, so it
/// buffers into a bounded [`SurfaceBuffer`] rather than hold that run's live
/// sink.  The buffer leaves by `await`/`race` replay or by [`Self::flush`] to
/// the [`DeferredSink`], whichever wins the handle's `joined` latch.
pub(crate) struct DeferredSurface {
    buf: SurfaceBuffer,
    deferred: Option<Arc<dyn DeferredSink>>,
}

impl DeferredSurface {
    pub(crate) fn new(buf: SurfaceBuffer, deferred: Option<Arc<dyn DeferredSink>>) -> Self {
        Self { buf, deferred }
    }

    /// Deliver the buffer plus a final [`DoneEvent`] as one batch, at most once
    /// — the claim lives here, at the sink's sole call site, so no
    /// implementation can forget the discipline.  The batch is a fresh clone,
    /// independent of `HandleInner::try_settle`'s later `mem::take` of the same buffer.
    fn flush(&self, joined: &Latch, cmd: &str, outcome: Done) {
        let Some(deferred) = self.deferred.as_ref() else {
            return;
        };
        if !joined.claim() {
            return;
        }
        let mut batch = self.buf.lock_ignore_poison().clone();
        let cmd = cmd.into();
        batch.push(DoneEvent { cmd, outcome }.to_surface());
        deferred.deliver(batch);
    }
}

impl EventSink for DeferredSurface {
    fn emit(&self, ev: &FOValue) {
        let mut buf = self.buf.lock_ignore_poison();
        if buf.len() < DEFERRED_SURFACE_CAP {
            buf.push(ev.clone());
        } else if buf.len() == DEFERRED_SURFACE_CAP {
            buf.push(FOValue::Variant {
                label: "surface-overflow".into(),
                payload: None,
            });
        }
    }
}

/// What a [`FlushGuard`] still owes: the flush of one worker's surface.
struct Pending {
    surface: Arc<DeferredSurface>,
    joined: Latch,
    cmd: String,
}

/// Flushes the worker's deferred surface on *every* exit path.  The clean path
/// disarms it through [`Self::settle`]; an unwinding panic leaves it armed, so
/// `drop` flushes a `` `panic `` outcome and the unwind carries on, dropping
/// the result channel's sender unsent, which `HandleInner::try_settle` still
/// settles as a panic.
pub(crate) struct FlushGuard(Option<Pending>);

impl FlushGuard {
    pub(crate) fn new(surface: Arc<DeferredSurface>, joined: Latch, cmd: String) -> Self {
        Self(Some(Pending {
            surface,
            joined,
            cmd,
        }))
    }

    /// Disarm and flush `outcome`.  The call site runs this before sending the
    /// result, so the boundary's clone predates the eliminators' drain.
    pub(crate) fn settle(mut self, outcome: Done) {
        self.flush(outcome);
    }

    fn flush(&mut self, outcome: Done) {
        if let Some(Pending {
            surface,
            joined,
            cmd,
        }) = self.0.take()
        {
            surface.flush(&joined, &cmd, outcome);
        }
    }
}

impl Drop for FlushGuard {
    fn drop(&mut self) {
        self.flush(Done::Panic("spawned thread panicked".into()));
    }
}
