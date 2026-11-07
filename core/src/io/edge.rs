//! The fate of one interior pipeline edge, shared by its writer and the
//! collector that holds the read end.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// One interior edge's fate: marked dead once its reader stage has ended
/// outside a teardown, which cuts nothing at an edge.
///
/// A stage is cut at its first write to a dead edge, and nowhere else — so
/// whatever it does that the reader was never owed runs to completion.
pub struct Edge {
    dead: AtomicBool,
}

impl Edge {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            dead: AtomicBool::new(false),
        })
    }

    pub(crate) fn is_dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }

    pub(crate) fn mark_dead(&self) {
        self.dead.store(true, Ordering::SeqCst);
    }
}
