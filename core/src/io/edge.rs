//! The fate of one interior pipeline edge, shared by its writer and the
//! collector that holds the read end.

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

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }

    pub fn mark_dead(&self) {
        self.dead.store(true, Ordering::SeqCst);
    }
}

/// The io error a write to a dead edge returns; `Shell::write_sink` reads it
/// back as `Error::cancelled(ReaderGone)`.
#[derive(Debug)]
pub struct DeadEdge;

impl std::fmt::Display for DeadEdge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(crate::process::CancelCause::ReaderGone.message())
    }
}

impl std::error::Error for DeadEdge {}
