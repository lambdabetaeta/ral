//! Signal handling and process-group placement — facade.
//!
//! Three concerns sit behind the same module name:
//!
//!   * **Termination flag.**  SIGINT / SIGTERM / SIGHUP set a single
//!     atomic counter that the evaluator polls between statements; the
//!     first signal unwinds, a second is deferred until cleanup completes,
//!     a third forces process exit.  Polled via [`check`].
//!   * **Cooperative cancellation.**  [`CancelScope`] is the structured-
//!     concurrency primitive: a tree of Arc-shared flags whose
//!     `is_cancelled` walk lets a `RunningPipeline::Drop` unwind every
//!     thread that inherited the scope.
//!   * **Process-group placement.**  [`PgidPolicy`] is the type-level
//!     spelling of "stay in the parent's group / become a leader of a
//!     fresh group / join an existing group as a non-leader" applied via
//!     `pre_exec` before `execve` by the platform-specific
//!     [`spawn_with_pgid`].
//!
//! Platform-specific machinery lives in [`unix`] and [`windows`]; this
//! file re-exports the items each platform provides and keeps the
//! portable data types and the unwind-poll path here.

use std::sync::atomic::{AtomicU8, Ordering};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{
    ForegroundGuard, PipelineRelay, install_handlers, relay_handler, reset_child_signals,
    spawn_with_pgid, term_handler, wait_handling_stop,
};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{
    ForegroundGuard, PipelineRelay, install_handlers, release_win_group, reset_child_signals,
    spawn_with_pgid, wait_handling_stop,
};

// ── Termination flag ───────────────────────────────────────────────────────

/// 0 = normal, 1 = interrupted, 2 = second signal, >=3 = force exit.
///
/// The platform handlers (`unix::handler` and `windows::install_handlers`)
/// fetch_add into this counter; [`check`] reads it.
pub(crate) static SIGNAL_COUNT: AtomicU8 = AtomicU8::new(0);

/// Check whether the current evaluation should unwind.
///
/// Two reasons can fire:
///
///   * a process-level signal (SIGINT / SIGTERM / SIGHUP) — incremented
///     by the platform handler;
///   * a structured-concurrency cancel — the shell's [`CancelScope`] (or
///     any of its ancestors) has been cancelled, e.g. by
///     `RunningPipeline::Drop` on the abort path.
///
/// Both unwind via the same `EvalSignal::Error` so callers don't need to
/// distinguish them — the message is "interrupted" vs "cancelled".
pub fn check(shell: &crate::types::Shell) -> Result<(), crate::types::EvalSignal> {
    if SIGNAL_COUNT.load(Ordering::Relaxed) >= 1 {
        return Err(crate::types::EvalSignal::Error(
            crate::types::Error::new("interrupted", 130)
                .at(shell.location.line, shell.location.col),
        ));
    }
    if shell.cancel.is_cancelled() {
        return Err(crate::types::EvalSignal::Error(
            crate::types::Error::new("cancelled", 130)
                .at(shell.location.line, shell.location.col),
        ));
    }
    Ok(())
}

/// Clear the signal flag (e.g., after handling in interactive mode).
pub fn clear() {
    SIGNAL_COUNT.store(0, Ordering::Relaxed);
}

/// Returns true if a signal is pending.
pub fn is_interrupted() -> bool {
    SIGNAL_COUNT.load(Ordering::Relaxed) >= 1
}

// ── Fallback platform stubs ────────────────────────────────────────────────
//
// `cfg(not(any(unix, windows)))` (wasm, etc.) — keep the compile path open
// without trying to emulate process-group semantics.

#[cfg(not(any(unix, windows)))]
pub fn install_handlers() {}

#[cfg(not(any(unix, windows)))]
pub fn reset_child_signals() {}

#[cfg(not(any(unix, windows)))]
pub fn spawn_with_pgid(
    cmd: &mut std::process::Command,
    _pgid: PgidPolicy,
) -> std::io::Result<(std::process::Child, Option<Pgid>)> {
    let child = cmd.spawn()?;
    Ok((child, None))
}

#[cfg(not(any(unix, windows)))]
pub fn wait_handling_stop(
    child: &mut std::process::Child,
    _pgid: Option<Pgid>,
) -> std::io::Result<std::process::ExitStatus> {
    child.wait()
}

#[cfg(not(any(unix, windows)))]
pub struct PipelineRelay;

#[cfg(not(any(unix, windows)))]
impl PipelineRelay {
    pub fn install(_pgid: i32) -> Option<Self> {
        None
    }
}

#[cfg(not(any(unix, windows)))]
pub struct ForegroundGuard;

#[cfg(not(any(unix, windows)))]
impl ForegroundGuard {
    pub fn try_acquire(_target: i32, _shell: &crate::types::Shell) -> Option<Self> {
        None
    }
}

// ── Cooperative cancellation ───────────────────────────────────────────────
//
// A worker checks `is_cancelled` at well-defined poll points (the same
// places `signal::check` is already called); cancelling an outer scope
// propagates to every inner scope, so a top-level Ctrl-C — or a
// `RunningPipeline::Drop` on the abort path — unwinds every thread that
// inherited the scope at its next poll point.
//
// The chain is walked, not flattened, so subscopes can carry their own
// flag (cancelling only their subtree) while still observing parent
// cancellation.  No mutex, no allocation in the hot path — just an
// `AtomicU8::load` per ancestor.

/// Internal node of the cancel-scope tree.  A scope is cancelled if its
/// own flag is set OR any ancestor's flag is set.
#[derive(Debug)]
struct ScopeNode {
    flag: AtomicU8,
    parent: Option<std::sync::Arc<ScopeNode>>,
}

/// A handle into the cancel-scope tree.  Cheap to clone (one `Arc` bump);
/// cheap to check (chain of atomic loads).
///
/// Construction:
///   * [`CancelScope::root`] — a fresh top-level scope with no parent.
///   * [`CancelScope::child`] — a new scope nested under `self`;
///     cancelling `self` (or any of its ancestors) cancels the child too.
///
/// Cancellation is one-way: once cancelled, a scope stays cancelled.
#[derive(Debug, Clone)]
pub struct CancelScope(std::sync::Arc<ScopeNode>);

impl CancelScope {
    /// A fresh root scope.  Used by the default `Shell` and by tests
    /// that don't need cancellation.
    pub fn root() -> Self {
        Self(std::sync::Arc::new(ScopeNode {
            flag: AtomicU8::new(0),
            parent: None,
        }))
    }

    /// A new scope nested under `self`.  Cancelling any ancestor (or
    /// `self`) cancels the returned child.  `RunningPipeline` creates
    /// one of these per pipeline so cancelling the pipeline doesn't
    /// reach the parent shell.
    pub fn child(&self) -> Self {
        Self(std::sync::Arc::new(ScopeNode {
            flag: AtomicU8::new(0),
            parent: Some(self.0.clone()),
        }))
    }

    /// Set this scope's flag.  Idempotent.  Visible to every share /
    /// child of this scope at the next [`is_cancelled`](Self::is_cancelled)
    /// poll.
    pub fn cancel(&self) {
        self.0.flag.store(1, Ordering::Release);
    }

    /// Walk the parent chain, returning true if any node's flag is set.
    pub fn is_cancelled(&self) -> bool {
        let mut node: &std::sync::Arc<ScopeNode> = &self.0;
        loop {
            if node.flag.load(Ordering::Acquire) != 0 {
                return true;
            }
            match &node.parent {
                Some(p) => node = p,
                None => return false,
            }
        }
    }
}

impl Default for CancelScope {
    fn default() -> Self {
        Self::root()
    }
}

// ── Process-group placement (data) ─────────────────────────────────────────

/// Newtype wrapping a process-group identifier.
///
/// On Unix: a POSIX pgid (== leader's pid).  Addressable via
/// `kill(-pgid, sig)` to fan a signal out across every member.
///
/// On Windows: the leader's pid.  Windows console process groups can't
/// be joined post-spawn — every external pipeline stage is its own
/// group leader (spawned with `CREATE_NEW_PROCESS_GROUP`).  The `Pgid`
/// here is the *first* stage's pid; the `PipelineGroup` keeps the full
/// member list and a Job Object that ties them together for abort-time
/// `TerminateJobObject`.
///
/// Constructed only from a real pid; no `0` sentinel encodes "no pgid"
/// — the `Option<Pgid>` does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pgid(pub i32);

/// Process-group placement decision applied via `pre_exec` before `execve`.
///
/// Single-command foreground jobs and pipeline first stages use
/// `NewLeader`; subsequent pipeline stages use `Join(leader_pgid)`;
/// non-pipeline non-foreground children use `Inherit`.
#[derive(Clone, Copy, Debug)]
pub enum PgidPolicy {
    /// Inherit the parent's pgid — no `setpgid` call.
    Inherit,
    /// Become the leader of a fresh process group (`setpgid(0, 0)`).
    NewLeader,
    /// Join an existing pgid as a non-leader (`setpgid(0, leader)`).
    Join(Pgid),
}

#[cfg(not(unix))]
impl PgidPolicy {
    /// No-op on platforms without POSIX process groups.
    pub fn apply(self) {}
}
