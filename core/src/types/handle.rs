//! The shared cells behind [`Value::Handle`](super::Value): what a spawned
//! worker writes and its observers read.

use super::value::Value;
use crate::io::ByteBuffer;
use std::sync::{Arc, Mutex};

/// Whether `v` structurally reaches a running handle — the binding-lease
/// reaper's pin check, so a name still holding live work is never pruned.
/// Closure captures are never descended, the same refusal
/// [`Value::shallow_size`] makes: the worker registry retains the handle
/// regardless, so nothing is stranded either way.
pub(crate) fn pins_running_work(v: &Value) -> bool {
    match v {
        Value::Handle(h) => *h.state.lock().unwrap() == HandleState::Running,
        Value::List(items) => items.iter().any(pins_running_work),
        Value::Map(pairs) => pairs.iter().any(|(_, v)| pins_running_work(v)),
        Value::Variant { payload, .. } => payload.as_deref().is_some_and(pins_running_work),
        Value::Unit
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::String(_)
        | Value::Bytes(_)
        | Value::Lambda { .. }
        | Value::Block { .. } => false,
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
    pub surface: Vec<crate::serial::FOValue>,
    pub outcome: super::flow::Settled<Value>,
}

/// Bounded buffer of the structured events a *detached* worker defers rather
/// than emitting live, its spawning run having possibly ended.
///
/// The bound lives in `builtins::concurrency`'s `DeferredSurface`, so a
/// runaway emitter cannot grow this without limit.
pub type SurfaceBuffer = Arc<Mutex<Vec<crate::serial::FOValue>>>;

/// Shared handle to a spawned computation.
#[derive(Debug, Clone)]
#[allow(clippy::type_complexity)]
pub struct HandleInner {
    /// Result channel.  The worker's trampoline absorbs any terminal tail
    /// call first: `Tail` cannot cross a thread boundary.
    pub result: Arc<Mutex<Option<std::sync::mpsc::Receiver<super::flow::Settled<Value>>>>>,
    /// Filled once, when the block completes and the buffers drain into it;
    /// every later observation reads it instead of the channel.
    pub cached: Arc<Mutex<Option<CompletedHandle>>>,
    pub state: Arc<Mutex<HandleState>>,
    /// Buffered stdout, drained into `cached` on completion.  Empty for a
    /// watched handle, whose bytes flow live through `Sink::LineFramed`.
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
    pub joined: Arc<Mutex<bool>>,
    /// When an eliminator last named this handle: renewed by `poll` and by
    /// every `await`/`race` sweep, read by the idle lease chain in
    /// `builtins::concurrency`.  Cancelling or listing never renews it.
    pub last_observed: Arc<Mutex<std::time::Instant>>,
    pub cmd: std::string::String,
    /// The worker's own scope — a `DurableRoot::worker()` child of the
    /// *session* root, not of the spawning run, so a foreground interrupt
    /// cannot collaterally kill it.  `cancel` and `race`'s losers fire it, and
    /// the worker stops at its next poll rather than running on detached.
    pub cancel: crate::process::CancelScope,
}

impl PartialEq for HandleInner {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.result, &other.result)
    }
}
