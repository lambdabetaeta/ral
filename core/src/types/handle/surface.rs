//! A worker's surface: the events it emits while no run listens, buffered
//! until the worker settles and then handed once to the session's deferred sink.

use crate::first_order::FOValue;
use crate::sync::LockExt as _;
use crate::types::{DeferredSink, Done, DoneEvent, EventSink, WorkerId};
use std::sync::{Arc, Mutex};

/// Cap on a worker's deferred surface: past it one `surface-overflow`
/// marker is recorded and further events drop.
const DEFERRED_SURFACE_CAP: usize = 4096;

/// A worker's surface: it may outlive the run that spawned it, so it
/// buffers into a bounded vector rather than hold that run's live sink.  The
/// buffer leaves once, at completion, through [`Self::flush`] to the
/// [`DeferredSink`].
pub(crate) struct DeferredSurface {
    buf: Mutex<Vec<FOValue>>,
    deferred: Arc<dyn DeferredSink>,
}

impl DeferredSurface {
    pub(crate) fn new(deferred: Arc<dyn DeferredSink>) -> Self {
        Self {
            buf: Mutex::default(),
            deferred,
        }
    }

    /// Deliver the buffer plus a final [`DoneEvent`] as one batch.
    fn flush(&self, id: WorkerId, cmd: &str, outcome: Done) {
        let mut batch = std::mem::take(&mut *self.buf.lock_ignore_poison());
        let cmd = cmd.into();
        batch.push(DoneEvent { id, cmd, outcome }.to_surface());
        self.deferred.deliver(batch);
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
    id: WorkerId,
    cmd: String,
}

/// Flushes the worker's deferred surface on *every* exit path.  The clean path
/// disarms it through [`Self::settle`]; an unwinding panic leaves it armed, so
/// `drop` flushes a `` `panic `` outcome and the unwind carries on, dropping
/// the result channel's sender unsent, which `HandleInner::try_settle` still
/// settles as a panic.  Unarmed when the worker has no deferred sink.
pub(crate) struct FlushGuard(Option<Pending>);

impl FlushGuard {
    pub(crate) fn new(surface: Option<Arc<DeferredSurface>>, id: WorkerId, cmd: String) -> Self {
        Self(surface.map(|surface| Pending { surface, id, cmd }))
    }

    /// Disarm and flush `outcome`.
    pub(crate) fn settle(mut self, outcome: Done) {
        self.flush(outcome);
    }

    fn flush(&mut self, outcome: Done) {
        if let Some(Pending { surface, id, cmd }) = self.0.take() {
            surface.flush(id, &cmd, outcome);
        }
    }
}

impl Drop for FlushGuard {
    fn drop(&mut self) {
        self.flush(Done::Panic("spawned thread panicked".into()));
    }
}
