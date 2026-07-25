//! Concurrency substrate backing [`Value::Handle`](super::Value).
//!
//! [`HandleInner`] is the shared state for a spawned computation; a
//! [`Value::Handle`](super::Value) carries one.  [`CompletedHandle`] is
//! the outcome cached on completion, and [`SurfaceBuffer`] holds the
//! structured events a detached worker defers.

use super::value::Value;
use crate::io::ByteBuffer;
use std::sync::{Arc, Mutex};

/// Whether `v` structurally reaches a handle whose state is
/// [`HandleState::Running`] — the binding-lease reaper's pin check
/// (`decisions/260629_agent-binding-reaping`): a name whose value still
/// reaches a running worker is never pruned. Recurses through `List`,
/// `Map`, and `Variant` payloads; a `Lambda` or `Block`'s captured
/// `Arc<Env>` is deliberately **never** descended — the same graph chase
/// [`Value::shallow_size`] refuses, and a handle reachable only through a
/// closure capture is not "the name of live work": the worker registry
/// retains the handle itself regardless of whether any top-level name
/// still reaches it, so nothing can be stranded either way.
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

/// Shared handle to a spawned computation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleState {
    Running,
    Completed,
    Cancelled,
}

/// The observable outcome of a finished concurrent block: the bytes it
/// wrote, drained exactly once from the handle's buffers, paired with its
/// raw result.
///
/// Every eliminator (`await`, `race`, `poll`) projects this
/// one cached value rather than re-reading the live buffers, so a failed
/// block's bytes are captured on completion and repeated observations stay
/// consistent.  `outcome` is `Ok(value)` for a block that returned and
/// `Err(break)` for one that raised or whose worker panicked.
#[derive(Debug, Clone)]
pub struct CompletedHandle {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Structured-event values a *detached* worker deferred, drained once
    /// from [`HandleInner::surface_buf`].  Replayed through the awaiting
    /// run's surface by `await`/`race` (once), never by `poll`.
    pub surface: Vec<crate::serial::FOValue>,
    pub outcome: super::flow::Settled<Value>,
}

/// Bounded buffer of structured-event values a *detached* worker defers
/// instead of emitting live.
///
/// The run that spawned the worker may already
/// have ended, so its `surface` events are buffered here and replayed through
/// the awaiting run's surface on the first `await`/`race`.  Bounded so a
/// runaway detached emitter cannot grow it without limit (see
/// `builtins::concurrency`'s `DeferredSurface`).
pub type SurfaceBuffer = Arc<Mutex<Vec<crate::serial::FOValue>>>;

/// Shared handle to a spawned computation.
#[derive(Debug, Clone)]
#[allow(clippy::type_complexity)]
pub struct HandleInner {
    /// Result channel: worker sends [`Settled<Value>`](super::flow::Settled)
    /// (the worker's trampoline absorbs any terminal tail call before the
    /// value reaches the channel — `Tail` cannot cross a thread boundary).
    pub result: Arc<Mutex<Option<std::sync::mpsc::Receiver<super::flow::Settled<Value>>>>>,
    /// Cached outcome after first observation (§13.3: a second await
    /// returns the cached bytes + result).  `None` until the block
    /// completes, then the buffers are drained into it exactly once.
    pub cached: Arc<Mutex<Option<CompletedHandle>>>,
    /// Lifecycle state for handle-level APIs.
    pub state: Arc<Mutex<HandleState>>,
    /// Buffered stdout from the spawned block (§13.3 replay rule).  Bytes
    /// accumulate here during execution and are drained on `await`.  Always
    /// empty for watched handles — bytes flow live through `Sink::LineFramed`.
    pub stdout_buf: ByteBuffer,
    /// Buffered stderr from the spawned block (§13.3 replay rule).  Always
    /// empty for watched handles.
    pub stderr_buf: ByteBuffer,
    /// Bounded deferred surface events from the worker: a *detached*
    /// worker's `surface` builtin lands here rather than on the
    /// (possibly-ended) spawning run's live sink.  Drained once on
    /// completion into [`CompletedHandle::surface`].
    pub surface_buf: SurfaceBuffer,
    /// The deliver-once test-and-set latch for the worker's deferred surface,
    /// shared between the two renderers and set by whichever renders the batch
    /// first — an eliminator's replay (`await`/`race`) or the host's boundary
    /// delivery.  Whoever wins renders; the other sees it set and skips, so a
    /// batch never renders twice and a never-awaited worker still delivers
    /// exactly once at the boundary.
    pub joined: Arc<Mutex<bool>>,
    /// The moment an eliminator last named this handle: written at
    /// construction and renewed by `poll` and by each `await`/`race` wait
    /// sweep, read by the idle-observation lease chain
    /// (`builtins::concurrency`).  `cancel` and listing never touch it —
    /// only an observation renews the lease.
    pub last_observed: Arc<Mutex<std::time::Instant>>,
    pub cmd: std::string::String,
    /// The worker's cancel scope, a child of the spawning shell's scope.
    /// `cancel` and `race`-of-losers fire it so the worker stops at its
    /// next poll rather than running to completion detached.
    pub cancel: crate::process::CancelScope,
}

impl PartialEq for HandleInner {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.result, &other.result)
    }
}
