//! A worker's surface: the events it emits while no run listens, buffered
//! until the worker settles and then handed once to the session's deferred sink.

use crate::data::FOValue;
use crate::shell::runtime::{DeferredSink, DoneEvent, EventSink};
use crate::sync::LockExt as _;
use std::sync::{Arc, Mutex};

/// Cap on a worker's deferred surface: past it one `surface-overflow`
/// marker is recorded and further events drop.
const DEFERRED_SURFACE_CAP: usize = 4096;

/// A worker's surface: it may outlive the run that spawned it, so it
/// buffers into a bounded vector rather than hold that run's live sink.  The
/// buffer leaves once, after the worker's [`CompletionPermit`](super::CompletionPermit)
/// publishes its terminal state, through [`Self::flush`] to the [`DeferredSink`].
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
    pub(crate) fn flush(&self, done: DoneEvent) {
        let mut batch = std::mem::take(&mut *self.buf.lock_ignore_poison());
        batch.push(done.to_surface());
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
