//! The fate of one interior pipeline edge, shared by its writer and the
//! collector that holds the read end.

use crate::process::{CancelCause, ScopeId};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// One interior edge's fate: dead once its reader stage has ended.
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

/// Why a stage's write did not complete: the stop `Shell::write_sink`
/// re-mints.  A dead edge is the writing stage's own reader-gone stop; a cause
/// in force is the join the writer's scope read between slices.
#[derive(Debug)]
pub(crate) struct WriteStopped {
    pub(crate) cause: CancelCause,
    pub(crate) owner: Option<ScopeId>,
}

impl std::fmt::Display for WriteStopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.cause.message())
    }
}

impl std::error::Error for WriteStopped {}
