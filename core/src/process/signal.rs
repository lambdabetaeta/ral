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
//!     `is_cancelled` walk lets a cancelled scope unwind every worker that
//!     inherited it at its next poll point — the foreground turn, a
//!     detached `spawn`/`watch` worker, or a `RunningChild` whose wait loop
//!     polls the scope and tears the child down by cause.
//!   * **Process-group placement.**  [`PgidPolicy`] is the type-level
//!     spelling of "stay in the parent's group / become a leader of a
//!     fresh group / join an existing group as a non-leader" applied via
//!     `pre_exec` before `execve` by the platform-specific
//!     [`spawn_with_pgid`].
//!
//! Platform-specific machinery lives in [`unix`] and [`windows`]; this
//! file re-exports the items each platform provides and keeps the
//! portable data types and the unwind-poll path here.

use std::sync::atomic::{AtomicPtr, AtomicU8, Ordering};

#[cfg(unix)]
use super::outcome::Signal;
use super::outcome::WaitOutcome;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{
    ForegroundGuard, PipelineRelay, install_handlers, interrupt_foreground_child, quit_handler,
    relay_handler, reset_child_signals, spawn_with_pgid, spawn_with_pgid_after, term_handler,
    termios_snapshot, waitpid_eintr,
};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{
    ForegroundGuard, PipelineRelay, ReapStatus, apply_group_active_process_limit,
    disown_pipeline_group, install_handlers, is_known_group, kill_pipeline_group,
    release_win_group, reset_child_signals, spawn_with_pgid, try_reap_leader, wait_leader_blocking,
};
#[cfg(windows)]
pub(crate) use windows::{
    PreparedGroup, RawChild, close_prepared_group, prepare_group, prepared_job,
    register_prepared_group,
};

// ── Child handle ───────────────────────────────────────────────────────────
//
// Bare `std::process::Child::wait` / `try_wait` call `waitpid` without
// `WUNTRACED`: a SIGSTOP'd child is invisible to `try_wait` (it reports
// "still running"), and `wait` blocks indefinitely.  Every external
// child ral tracks therefore goes through `ChildHandle`, whose wait
// methods pass `WUNTRACED` on Unix and classify the result.  Direct
// `Child::wait` / `try_wait` calls outside this file are blocked by
// `clippy::disallowed_methods` (see `clippy.toml`); the few legitimate
// exceptions are tagged with `#[allow]` at the call site.

/// Wrapper around a spawned child that hides the raw `wait` / `try_wait`
/// and only exposes [`Self::wait_handling_stop`] /
/// [`Self::try_wait_handling_stop`] — the WUNTRACED-aware peers that
/// can't misclassify a stopped child as "still running".
pub struct ChildHandle(ChildRepr);

enum ChildRepr {
    Std(std::process::Child),
    #[cfg(windows)]
    RawWindows(windows::RawChild),
}

impl ChildHandle {
    /// Wrap a freshly-spawned child.  After this point the std handle
    /// is no longer reachable by name; the only paths to wait are the
    /// `*_handling_stop` methods.
    pub fn from_std(child: std::process::Child) -> Self {
        Self(ChildRepr::Std(child))
    }

    #[cfg(windows)]
    pub(crate) fn from_windows_raw(child: windows::RawChild) -> Self {
        Self(ChildRepr::RawWindows(child))
    }

    pub fn id(&self) -> u32 {
        match &self.0 {
            ChildRepr::Std(child) => child.id(),
            #[cfg(windows)]
            ChildRepr::RawWindows(child) => child.id(),
        }
    }

    pub fn kill(&mut self) -> std::io::Result<()> {
        match &mut self.0 {
            ChildRepr::Std(child) => child.kill(),
            #[cfg(windows)]
            ChildRepr::RawWindows(child) => child.kill(),
        }
    }

    pub fn take_stdout(&mut self) -> Option<Box<dyn std::io::Read + Send>> {
        match &mut self.0 {
            ChildRepr::Std(child) => child
                .stdout
                .take()
                .map(|stdout| Box::new(stdout) as Box<dyn std::io::Read + Send>),
            #[cfg(windows)]
            ChildRepr::RawWindows(child) => child.take_stdout(),
        }
    }

    pub fn take_stderr(&mut self) -> Option<Box<dyn std::io::Read + Send>> {
        match &mut self.0 {
            ChildRepr::Std(child) => child
                .stderr
                .take()
                .map(|stderr| Box::new(stderr) as Box<dyn std::io::Read + Send>),
            #[cfg(windows)]
            ChildRepr::RawWindows(child) => child.take_stderr(),
        }
    }

    #[cfg(windows)]
    pub(crate) fn raw_process_handle(&self) -> windows_sys::Win32::Foundation::HANDLE {
        match &self.0 {
            ChildRepr::Std(child) => {
                use std::os::windows::io::AsRawHandle;
                child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE
            }
            ChildRepr::RawWindows(child) => child.raw_process_handle(),
        }
    }

    /// Blocking wait that observes `WIFSTOPPED` on Unix.  See the
    /// platform `wait_handling_stop` for the park / kill-and-reap
    /// dispatch on stop.
    pub fn wait_handling_stop(
        &mut self,
        pgid: Option<Pgid>,
        park_on_stop: bool,
    ) -> std::io::Result<WaitOutcome> {
        #[cfg(unix)]
        {
            let ChildRepr::Std(child) = &mut self.0;
            unix::wait_handling_stop(child, pgid, park_on_stop)
        }
        #[cfg(windows)]
        {
            match &mut self.0 {
                ChildRepr::Std(child) => windows::wait_handling_stop(child, pgid, park_on_stop),
                ChildRepr::RawWindows(child) => child.wait_handling_stop(),
            }
        }
    }

    /// Non-blocking peek matching the contract of
    /// [`Self::wait_handling_stop`]: returns `Ok(None)` when nothing is
    /// pending, otherwise classifies and (on no-park stop) kills and
    /// reaps inline.
    pub fn try_wait_handling_stop(
        &mut self,
        pgid: Option<Pgid>,
        park_on_stop: bool,
    ) -> std::io::Result<Option<WaitOutcome>> {
        #[cfg(unix)]
        {
            let ChildRepr::Std(child) = &mut self.0;
            unix::try_wait_handling_stop(child, pgid, park_on_stop)
        }
        #[cfg(windows)]
        {
            match &mut self.0 {
                ChildRepr::Std(child) => windows::try_wait_handling_stop(child, pgid, park_on_stop),
                ChildRepr::RawWindows(child) => child.try_wait_handling_stop(),
            }
        }
    }

    /// Blocking reap used only after a confirmed SIGKILL on the abort
    /// path: SIGKILL terminates even stopped processes, so the full
    /// stop-aware ceremony of `wait_handling_stop` is pure overhead
    /// here.  This is the one place inside `ChildHandle` that calls
    /// the raw `Child::wait`.
    #[allow(clippy::disallowed_methods)]
    pub fn reap(&mut self) -> std::io::Result<std::process::ExitStatus> {
        match &mut self.0 {
            ChildRepr::Std(child) => child.wait(),
            #[cfg(windows)]
            ChildRepr::RawWindows(child) => child.reap(),
        }
    }
}

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
///     any of its ancestors) has been cancelled, e.g. by a turn deadline
///     ([`reaper`](crate::process::reaper)), an explicit `cancel <handle>`,
///     or a Ctrl-\ root abort.
///
/// Both unwind via the same `Break::Error` so callers don't need to
/// distinguish them.  A pending signal reads "interrupted"; a cancelled
/// scope derives its message from the strongest [`CancelCause`] in
/// force: "interrupted" for an [`Interrupt`](CancelCause::Interrupt),
/// "cancelled" for an [`Explicit`](CancelCause::Explicit), "timed out"
/// for a [`Deadline`](CancelCause::Deadline), "aborted" for a
/// [`RootAbort`](CancelCause::RootAbort).
///
/// Signals are interrupts: they surface as either a recoverable error
/// (`Break::Error`) or a non-catchable escape (`Break::Escape`). They
/// never carry a tail call, hence the `Break` return type rather than
/// the richer `Control`.
pub fn check(shell: &crate::types::Shell) -> Result<(), crate::types::Break> {
    if SIGNAL_COUNT.load(Ordering::Relaxed) >= 1 {
        return Err(crate::types::Break::Error(
            crate::types::Error::new("interrupted", 130).at_loc(shell.turn.loc.source_loc(0)),
        ));
    }
    if let Some(cause) = shell.turn.cancel.cause() {
        let msg = match cause {
            CancelCause::Interrupt => "interrupted",
            CancelCause::Explicit => "cancelled",
            CancelCause::Deadline => "timed out",
            CancelCause::RootAbort => "aborted",
        };
        return Err(crate::types::Break::Error(
            crate::types::Error::new(msg, 130).at_loc(shell.turn.loc.source_loc(0)),
        ));
    }
    Ok(())
}

/// Clear the signal flag (e.g., after handling in interactive mode).
pub fn clear() {
    SIGNAL_COUNT.store(0, Ordering::Relaxed);
}

/// Request a cooperative unwind without escalating toward force-exit.
///
/// [`check`] treats any count `>= 1` as interrupted, so a single store
/// unwinds at the next poll.  Unlike a real signal — which the platform
/// handler `fetch_add`s, forcing `_exit` on the third — this is
/// idempotent: a frontend that drives its own task-level cancel can ask
/// the evaluator to unwind without ever feeding the shell's third-signal
/// force-exit.
pub fn interrupt() {
    SIGNAL_COUNT.store(1, Ordering::Relaxed);
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
pub fn spawn_with_pgid_after<F>(
    cmd: &mut std::process::Command,
    pgid: PgidPolicy,
    _after: F,
) -> std::io::Result<(std::process::Child, Option<Pgid>)>
where
    F: Fn() -> std::io::Result<()> + Send + Sync + 'static,
{
    spawn_with_pgid(cmd, pgid)
}

#[cfg(not(any(unix, windows)))]
pub fn wait_handling_stop(
    child: &mut std::process::Child,
    _pgid: Option<Pgid>,
    _park_on_stop: bool,
) -> std::io::Result<WaitOutcome> {
    child.wait().map(WaitOutcome::from_exit_status)
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
    pub fn try_acquire(_target: i32, _lease: &crate::process::TerminalLease) -> Option<Self> {
        None
    }
}

// ── Cooperative cancellation ───────────────────────────────────────────────
//
// A worker checks `is_cancelled` at well-defined poll points (the same
// places `signal::check` is already called); cancelling an outer scope
// propagates to every inner scope, so a top-level Ctrl-\ root abort — or a
// turn deadline — unwinds every thread that inherited the scope at its next
// poll point.
//
// The chain is walked, not flattened, so subscopes can carry their own
// flag (cancelling only their subtree) while still observing parent
// cancellation.  No mutex, no allocation in the hot path — just an
// `AtomicU8::load` per ancestor.

/// Why a [`CancelScope`] was cancelled.
///
/// The causes form an escalation order
/// `Interrupt < Explicit < Deadline < RootAbort`.  A scope records the
/// highest cause ever applied to it and never downgrades: a later,
/// weaker cancellation cannot mask a stronger one already in force.  The
/// numeric values double as the on-flag encoding read back by
/// [`from_u8`](CancelCause::from_u8); `0` on the flag means uncancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CancelCause {
    /// The user asked the foreground to stop (Ctrl-C / Esc).
    Interrupt = 1,
    /// `cancel <handle>` or a `race` loser was deliberately reaped — a
    /// targeted worker teardown, neither a user interrupt nor a timeout.
    Explicit = 2,
    /// A wall-clock or lifetime ceiling expired.
    Deadline = 3,
    /// The session root is being reaped (Ctrl-\).
    RootAbort = 4,
}

impl CancelCause {
    /// Decode a flag byte back into a cause.  `0` (uncancelled) and any
    /// value outside the escalation order yield `None`.
    fn from_u8(flag: u8) -> Option<Self> {
        match flag {
            1 => Some(CancelCause::Interrupt),
            2 => Some(CancelCause::Explicit),
            3 => Some(CancelCause::Deadline),
            4 => Some(CancelCause::RootAbort),
            _ => None,
        }
    }
}

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
/// Cancellation is one-way and monotone in the [`CancelCause`]
/// escalation order: once cancelled a scope stays cancelled, and the
/// recorded cause only ever rises.
#[derive(Debug, Clone)]
pub struct CancelScope(std::sync::Arc<ScopeNode>);

impl CancelScope {
    /// A fresh root scope.  Constructed only at session init (and the
    /// `Default` impl below); spawned workers take a [`child`](Self::child)
    /// of their parent's scope so cancellation reaches them.
    pub(crate) fn root() -> Self {
        Self(std::sync::Arc::new(ScopeNode {
            flag: AtomicU8::new(0),
            parent: None,
        }))
    }

    /// A new scope nested under `self`.  Cancelling any ancestor (or
    /// `self`) cancels the returned child.  A turn mints one as its
    /// foreground scope and a detached worker mints one off the durable
    /// root, so cancelling a worker (or the foreground) doesn't reach the
    /// parent shell.
    pub fn child(&self) -> Self {
        Self(std::sync::Arc::new(ScopeNode {
            flag: AtomicU8::new(0),
            parent: Some(self.0.clone()),
        }))
    }

    /// Record `cause` on this scope's flag, raising it to the maximum of
    /// the existing and new cause.  Visible to every share / child of
    /// this scope at the next [`is_cancelled`](Self::is_cancelled) /
    /// [`cause`](Self::cause) poll.
    pub fn cancel(&self, cause: CancelCause) {
        self.0.flag.fetch_max(cause as u8, Ordering::Release);
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

    /// The strongest [`CancelCause`] in force along this scope's chain to
    /// the root, or `None` if nothing on the chain is cancelled.  Walks
    /// every ancestor and takes the maximum flag seen, so an ancestor
    /// [`RootAbort`](CancelCause::RootAbort) dominates a child
    /// [`Interrupt`](CancelCause::Interrupt).
    pub fn cause(&self) -> Option<CancelCause> {
        let mut node: &std::sync::Arc<ScopeNode> = &self.0;
        let mut max = 0u8;
        loop {
            max = max.max(node.flag.load(Ordering::Acquire));
            match &node.parent {
                Some(p) => node = p,
                None => break,
            }
        }
        CancelCause::from_u8(max)
    }

    /// A raw pointer to this scope's own flag, for publishing into a
    /// signal-reachable slot. The flag lives in the scope's `Arc`; the
    /// caller must keep the scope alive for the slot's tenure.
    pub(crate) fn flag_ptr(&self) -> *const AtomicU8 {
        &self.0.flag
    }
}

// ── Signal-reachable cancel slots ──────────────────────────────────────────
//
// A context with no `CancelScope` in hand — a signal handler, a TUI input
// thread — cannot reach the foreground turn's scope or the session's
// durable root directly.  As exarch's `cancel.rs` does for its per-turn
// `Token`, we bridge the gap with process-global `AtomicPtr` slots holding
// a borrowed pointer into each scope's flag.  A scope's flag is an
// `AtomicU8` and `cancel` is a `fetch_max`, so the translation is itself
// async-signal-safe: the signal/input path loads the slot and `fetch_max`es
// the cause onto the flag, exactly the store `cancel` performs.

/// Pointer to the current turn's foreground-scope flag, published for the
/// signal/frontend translation path. Null between turns; restored to its
/// predecessor (not nulled) on guard drop, so nested turns stack correctly.
static FOREGROUND_SCOPE: AtomicPtr<AtomicU8> = AtomicPtr::new(std::ptr::null_mut());

/// Pointer to the session's durable-root-scope flag. Published per turn by
/// the turn doors, alongside the foreground, for the turn's extent.
static DURABLE_ROOT_SCOPE: AtomicPtr<AtomicU8> = AtomicPtr::new(std::ptr::null_mut());

/// Publishes a scope's flag into [`FOREGROUND_SCOPE`] for its lifetime,
/// restoring the previous publication on drop. Held by the turn doors for the
/// turn's extent; the scope it points at (the turn's `shell.turn.cancel`)
/// must outlive the guard, which it does — the guard is dropped before the
/// frame restores `turn.cancel`.
pub struct ForegroundCancelSlot {
    prev: *mut AtomicU8,
}

/// Publish `scope`'s flag as the foreground-cancel target for the returned
/// guard's lifetime. The prior publication is saved and restored on drop —
/// a swap, not a clear — so a re-entrant turn nests its scope above the
/// outer turn's and reveals it again when the inner guard drops.
pub fn publish_foreground(scope: &CancelScope) -> ForegroundCancelSlot {
    let prev = FOREGROUND_SCOPE.swap(scope.flag_ptr() as *mut AtomicU8, Ordering::Release);
    ForegroundCancelSlot { prev }
}

impl Drop for ForegroundCancelSlot {
    fn drop(&mut self) {
        FOREGROUND_SCOPE.store(self.prev, Ordering::Release);
    }
}

/// Publishes a scope's flag into [`DURABLE_ROOT_SCOPE`] for its lifetime,
/// restoring the previous publication on drop. Held by the session for its
/// whole life; the durable root scope must outlive the guard.
pub struct RootCancelSlot {
    prev: *mut AtomicU8,
}

/// Publish `scope`'s flag as the durable-root-cancel target for the returned
/// guard's lifetime, saving and restoring the prior publication on drop.
pub fn publish_durable_root(scope: &CancelScope) -> RootCancelSlot {
    let prev = DURABLE_ROOT_SCOPE.swap(scope.flag_ptr() as *mut AtomicU8, Ordering::Release);
    RootCancelSlot { prev }
}

impl Drop for RootCancelSlot {
    fn drop(&mut self) {
        DURABLE_ROOT_SCOPE.store(self.prev, Ordering::Release);
    }
}

/// Deliver `cause` to the current turn's foreground scope. Signal-safe: a
/// lock-free slot load and an atomic `fetch_max`. A no-op between turns
/// (null slot). The foreground scope is the only thing affected — detached
/// workers poll their own scopes, not this one, so they are spared.
pub fn request_foreground_cancel(cause: CancelCause) {
    let p = FOREGROUND_SCOPE.load(Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: a non-null slot points into the live foreground scope's
        // `Arc` (the publishing guard restores the prior pointer before that
        // scope can drop), so the flag is valid for this RMW.
        unsafe {
            (*p).fetch_max(cause as u8, Ordering::Release);
        }
    }
}

/// Deliver `cause` to the session's durable root, reaching the foreground
/// turn and every detached worker (all parented under it). Signal-safe: a
/// lock-free slot load and an atomic `fetch_max`. A no-op when no root is
/// published.
pub fn request_root_cancel(cause: CancelCause) {
    let p = DURABLE_ROOT_SCOPE.load(Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: a non-null slot points into the live durable-root scope's
        // `Arc` (the publishing guard restores the prior pointer before that
        // scope can drop), so the flag is valid for this RMW.
        unsafe {
            (*p).fetch_max(cause as u8, Ordering::Release);
        }
    }
}

impl Default for CancelScope {
    fn default() -> Self {
        Self::root()
    }
}

// ── Typed root / foreground relation ───────────────────────────────────────
//
// Two newtypes name the one structural invariant the cancel tree must keep: a
// turn's foreground scope is always a descendant of the session's durable
// root.  A [`ForegroundScope`] can only be minted from a [`DurableRoot`] (or
// by nesting another foreground), so an unrelated root can never be installed
// as a turn's foreground by accident.  Both wrap a private [`CancelScope`];
// `as_scope` borrows it for the few places that still need the bare handle
// (signal-slot publication, a worker's returned cancel token).

/// The session's durable cancel root.  Detached workers (`spawn`, `watch`,
/// `par`) parent under it, and every foreground scope is one of its
/// descendants, so a foreground cancel never reaches a detached worker.  Only
/// a [`RootAbort`](CancelCause::RootAbort) on the root (Ctrl-\), or a cancel
/// on a worker's own scope, stops such a worker.
#[derive(Debug, Clone)]
pub struct DurableRoot(CancelScope);

impl DurableRoot {
    /// Mint a fresh session root.  One per [`Shell`](crate::types::Shell),
    /// constructed at session init.
    pub(crate) fn new() -> Self {
        Self(CancelScope::root())
    }

    /// Mint a [`ForegroundScope`] descending from this root — a turn's
    /// foreground or a detached worker's scope.  The only entry into
    /// foreground construction, so the descendant invariant holds by type.
    pub fn child(&self) -> ForegroundScope {
        ForegroundScope(self.0.child())
    }

    /// Record `cause` on the root, reaching the foreground turn and every
    /// detached worker at once.
    pub fn cancel(&self, cause: CancelCause) {
        self.0.cancel(cause);
    }

    /// Borrow the underlying scope for signal-slot publication.
    pub fn as_scope(&self) -> &CancelScope {
        &self.0
    }
}

impl Default for DurableRoot {
    fn default() -> Self {
        Self::new()
    }
}

/// A cancel scope installed as a turn's foreground work scope.  Constructed
/// only from a [`DurableRoot`] or by nesting another `ForegroundScope`, so it
/// is always a descendant of the session root.  A foreground cancel — a turn
/// timeout, a Ctrl-C — reaches it and its same-thread descendants.
#[derive(Debug, Clone)]
pub struct ForegroundScope(CancelScope);

impl ForegroundScope {
    /// Nest a child foreground scope: a deadline window over a turn, or the
    /// shared scope a same-thread body inherits.  Still root-descended.
    pub fn child(&self) -> ForegroundScope {
        ForegroundScope(self.0.child())
    }

    /// Record `cause` on this scope.
    pub fn cancel(&self, cause: CancelCause) {
        self.0.cancel(cause);
    }

    /// True if this scope or any ancestor is cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }

    /// The strongest [`CancelCause`] in force along this scope's chain, or
    /// `None` if nothing on the chain is cancelled.
    pub fn cause(&self) -> Option<CancelCause> {
        self.0.cause()
    }

    /// Borrow the underlying scope for publication, or to hand a bare
    /// [`CancelScope`] to a worker handle / running pipeline that tracks
    /// cancellation without minting children.
    pub fn as_scope(&self) -> &CancelScope {
        &self.0
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

#[cfg(unix)]
impl Pgid {
    /// Send `signal` to every process in this group via `kill(-pgid, sig)`.
    ///
    /// Async-signal-safe: no allocation, no locking, just one libc call.
    /// Failures (e.g. `ESRCH` on a group whose last member has already
    /// exited) are silently ignored — pipeline abort and Ctrl-Z paths
    /// don't have a meaningful recovery for those, and the kernel has
    /// already done the right thing if the group is gone.
    pub fn signal_group(self, signal: Signal) {
        unsafe {
            libc::kill(-self.0, signal.number());
        }
    }
}

/// Process-group placement decision applied via `pre_exec` before `execve`.
///
/// Foreground job leaders and pipeline first stages use `NewLeader`;
/// subsequent pipeline stages use `Join(leader_pgid)`; an interactive
/// background child that must share the terminal's signal reach uses
/// `Inherit`; a *detached* background worker (a `spawn`/non-interactive
/// top-level child that never takes the foreground) uses `NewSession`.
#[derive(Clone, Copy, Debug)]
pub enum PgidPolicy {
    /// Inherit the parent's pgid — no `setpgid` call.
    Inherit,
    /// Become the leader of a fresh process group (`setpgid(0, 0)`),
    /// keeping the parent's session and controlling terminal.
    NewLeader,
    /// Become the leader of a fresh *session* (`setsid`): a new process
    /// group with **no controlling terminal**.  A detached background
    /// worker has no business holding the terminal, and severing the
    /// session is what stops it from signalling — via the shared tty or
    /// `tcgetpgrp` — whatever owns the controlling terminal (the REPL, or
    /// a supervising frontend).  The new session's pgid equals the child's
    /// pid, so the watchdog's `kill(-pgid, …)` still reaps the subtree.
    NewSession,
    /// Join an existing pgid as a non-leader (`setpgid(0, leader)`).
    Join(Pgid),
}

#[cfg(not(unix))]
impl PgidPolicy {
    /// No-op on platforms without POSIX process groups.
    pub fn apply(self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Serializes every publisher and asserter of the process-global cancel
/// slots ([`FOREGROUND_SCOPE`], [`DURABLE_ROOT_SCOPE`]) across the ral-core
/// test binary. The turn tests publish the slots too (every turn door
/// does), so the signal slot tests and the turn tests must share one lock to
/// keep their publications and requests from interleaving.
#[cfg(test)]
pub(crate) static SLOT_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    /// [`interrupt`] requests a cooperative unwind without escalating: no
    /// matter how often it fires, the counter sits at exactly 1, so it can
    /// never reach the handler's third-signal `_exit`.
    #[test]
    fn interrupt_is_idempotent() {
        clear();
        for _ in 0..5 {
            interrupt();
        }
        assert!(is_interrupted(), "interrupt should mark the evaluator");
        assert_eq!(
            SIGNAL_COUNT.load(Ordering::Relaxed),
            1,
            "interrupt must never accumulate past 1"
        );
        clear();
    }

    /// Cancelling a parent reaches a child: the child's `is_cancelled`
    /// walk climbs to the shared parent node and reads its set flag.
    /// This is the propagation a spawned worker relies on for a
    /// top-level Ctrl-C to unwind it.
    #[test]
    fn parent_cancel_reaches_child() {
        let parent = CancelScope::root();
        let child = parent.child();
        assert!(!child.is_cancelled(), "a fresh child starts uncancelled");
        parent.cancel(CancelCause::Interrupt);
        assert!(
            child.is_cancelled(),
            "cancelling the parent must cancel the child"
        );
    }

    /// Cancelling a child leaves the parent and sibling untouched: a
    /// per-worker `cancel` stops only that worker, not the spawning
    /// shell or its other workers.
    #[test]
    fn child_cancel_isolates_from_parent_and_siblings() {
        let parent = CancelScope::root();
        let one = parent.child();
        let two = parent.child();
        one.cancel(CancelCause::Interrupt);
        assert!(one.is_cancelled(), "the cancelled child observes its flag");
        assert!(
            !parent.is_cancelled(),
            "cancelling a child must not cancel the parent"
        );
        assert!(
            !two.is_cancelled(),
            "cancelling one child must not cancel a sibling"
        );
    }

    /// A published foreground scope receives a requested cancel; after the
    /// guard drops the slot reverts to null, so a later request is a no-op
    /// and the scope keeps the cause it already recorded.
    #[test]
    fn foreground_request_reaches_published_scope_then_reverts() {
        let _g = SLOT_SERIAL.lock().unwrap();
        let scope = CancelScope::root();
        {
            let _slot = publish_foreground(&scope);
            request_foreground_cancel(CancelCause::Interrupt);
            assert_eq!(
                scope.cause(),
                Some(CancelCause::Interrupt),
                "a foreground request must reach the published scope"
            );
        }
        // The slot is null again; a request between turns touches nothing.
        request_foreground_cancel(CancelCause::RootAbort);
        assert_eq!(
            scope.cause(),
            Some(CancelCause::Interrupt),
            "a post-drop request must not reach the formerly-published scope"
        );
    }

    /// A request raises the recorded cause monotonically: a stronger cause
    /// after a weaker one wins (`fetch_max`).
    #[test]
    fn foreground_request_escalates_monotonically() {
        let _g = SLOT_SERIAL.lock().unwrap();
        let scope = CancelScope::root();
        let _slot = publish_foreground(&scope);
        request_foreground_cancel(CancelCause::Interrupt);
        request_foreground_cancel(CancelCause::Deadline);
        assert_eq!(
            scope.cause(),
            Some(CancelCause::Deadline),
            "a stronger later cause must win"
        );
    }

    /// Publishing nests: an inner publication shadows the outer, and the
    /// inner guard's drop restores the outer pointer (a swap, not a clear),
    /// so a request after the inner drop reaches the outer scope again.
    #[test]
    fn foreground_publications_nest() {
        let _g = SLOT_SERIAL.lock().unwrap();
        let outer = CancelScope::root();
        let inner = CancelScope::root();
        let _outer_slot = publish_foreground(&outer);
        {
            let _inner_slot = publish_foreground(&inner);
            request_foreground_cancel(CancelCause::Interrupt);
            assert_eq!(
                inner.cause(),
                Some(CancelCause::Interrupt),
                "the inner publication must shadow the outer"
            );
            assert_eq!(
                outer.cause(),
                None,
                "the outer scope is untouched while shadowed"
            );
        }
        request_foreground_cancel(CancelCause::Explicit);
        assert_eq!(
            outer.cause(),
            Some(CancelCause::Explicit),
            "the inner drop must restore the outer pointer, not null the slot"
        );
    }

    /// The root slot behaves like the foreground slot, and the two are
    /// independent: a foreground request must not touch a published root.
    #[test]
    fn root_request_is_independent_of_foreground() {
        let _g = SLOT_SERIAL.lock().unwrap();
        let foreground = CancelScope::root();
        let root = CancelScope::root();
        let _fg = publish_foreground(&foreground);
        let _rt = publish_durable_root(&root);

        request_foreground_cancel(CancelCause::Interrupt);
        assert_eq!(
            foreground.cause(),
            Some(CancelCause::Interrupt),
            "a foreground request reaches the foreground scope"
        );
        assert_eq!(
            root.cause(),
            None,
            "a foreground request must not touch the published root"
        );

        request_root_cancel(CancelCause::RootAbort);
        assert_eq!(
            root.cause(),
            Some(CancelCause::RootAbort),
            "a root request reaches the published root"
        );
        assert_eq!(
            foreground.cause(),
            Some(CancelCause::Interrupt),
            "a root request must not downgrade or alter the foreground scope"
        );
    }
}
