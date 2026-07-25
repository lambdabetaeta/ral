//! Cooperative structured-concurrency cancellation.
//!
//! A worker checks `is_cancelled` at well-defined poll points (the same
//! places [`check`](crate::process::check) is called); cancelling an outer
//! scope propagates to every inner scope, so a top-level Ctrl-\ root abort — or
//! a run deadline — unwinds every thread that inherited the scope at its next
//! poll point.
//!
//! The chain is walked, not flattened, so subscopes can carry their own flag
//! (cancelling only their subtree) while still observing parent cancellation.
//! No mutex, no allocation in the hot path — just an `AtomicU8::load` per
//! ancestor.
//!
//! [`CancelScope`] is the raw handle; [`DurableRoot`] and [`ForegroundScope`]
//! name the one structural invariant the tree must keep — a run's foreground
//! scope is always a descendant of the session's durable root. A context with
//! no scope in hand (a signal handler, a TUI input thread) reaches the live
//! scopes through the process-global [`CancelSlot`] publications.

use std::sync::atomic::{AtomicPtr, AtomicU8, Ordering};

/// Why a [`CancelScope`] was cancelled.
///
/// The causes form an escalation order
/// `Interrupt < Explicit < Deadline < Terminate < RootAbort`.  A scope
/// records the highest cause ever applied to it and never downgrades: a
/// later, weaker cancellation cannot mask a stronger one already in
/// force.  The numeric values double as the on-flag encoding read back
/// by [`from_u8`](CancelCause::from_u8); `0` on the flag means
/// uncancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CancelCause {
    /// The user asked the foreground to stop (Ctrl-C / Esc).
    Interrupt = 1,
    /// `cancel <handle>` or a `race` loser was deliberately reaped — a
    /// targeted worker teardown, neither a user interrupt nor a timeout.
    Explicit = 2,
    /// A wall-clock or lifetime ceiling expired.
    Deadline = 3,
    /// The process was asked to shut down (SIGTERM / SIGHUP).  Lands on
    /// the durable root — a termination request reaches detached workers,
    /// not just the foreground run — and tears externals down
    /// SIGTERM-first, so a well-behaved tree gets the signal it expects.
    Terminate = 4,
    /// The session root is being reaped (Ctrl-\).
    RootAbort = 5,
}

impl CancelCause {
    /// Decode a flag byte back into a cause.  `0` (uncancelled) and any
    /// value outside the escalation order yield `None`.
    fn from_u8(flag: u8) -> Option<Self> {
        match flag {
            1 => Some(Self::Interrupt),
            2 => Some(Self::Explicit),
            3 => Some(Self::Deadline),
            4 => Some(Self::Terminate),
            5 => Some(Self::RootAbort),
            _ => None,
        }
    }

    /// The user-facing word for an evaluation this cause unwound.  The
    /// single vocabulary shared by every poll point that surfaces a
    /// cancellation as an error — [`check`](crate::process::check) and the
    /// hosts' own parked waits — so the wording cannot drift between them.
    pub fn message(self) -> &'static str {
        match self {
            Self::Interrupt => "interrupted",
            Self::Explicit => "cancelled",
            Self::Deadline => "timed out",
            Self::Terminate => "terminated",
            Self::RootAbort => "aborted",
        }
    }

    /// The exit status paired with [`message`](Self::message): 130
    /// (`128 + SIGINT`) for every interactive-shaped cancellation, 143
    /// (`128 + SIGTERM`) for a termination request — what a supervisor
    /// that `SIGTERMed` the process expects to read back.
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Terminate => 143,
            _ => 130,
        }
    }
}

/// Internal node of the cancel-scope tree.  A scope is cancelled if its
/// own flag is set OR any ancestor's flag is set.
#[derive(Debug)]
struct ScopeNode {
    flag: AtomicU8,
    parent: Option<std::sync::Arc<Self>>,
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
    /// `self`) cancels the returned child.  A run mints one as its
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
    fn flag_ptr(&self) -> *const AtomicU8 {
        &raw const self.0.flag
    }
}

impl Default for CancelScope {
    fn default() -> Self {
        Self::root()
    }
}

// ── Signal-reachable cancel slots ──────────────────────────────────────────
//
// A context with no `CancelScope` in hand — a signal handler, a TUI input
// thread — cannot reach the foreground run's scope or the session's
// durable root directly.  As exarch's `cancel.rs` does for its per-exchange
// `Token`, we bridge the gap with process-global `AtomicPtr` slots holding
// a borrowed pointer into each scope's flag.  A scope's flag is an
// `AtomicU8` and `cancel` is a `fetch_max`, so the translation is itself
// async-signal-safe: the signal/input path loads the slot and `fetch_max`es
// the cause onto the flag, exactly the store `cancel` performs.

/// Pointer to the current run's foreground-scope flag, published for the
/// signal/frontend translation path. Null between runs; restored to its
/// predecessor (not nulled) on guard drop, so nested runs stack correctly.
static FOREGROUND_SCOPE: AtomicPtr<AtomicU8> = AtomicPtr::new(std::ptr::null_mut());

/// Pointer to the session's durable-root-scope flag. Published per run by
/// the run doors, alongside the foreground, for the run's extent.
static DURABLE_ROOT_SCOPE: AtomicPtr<AtomicU8> = AtomicPtr::new(std::ptr::null_mut());

/// RAII publication of a scope's flag into a process-global cancel slot,
/// restoring the previous publication on drop.
///
/// Minted by [`publish_foreground`] / [`publish_durable_root`]; the prior
/// publication is saved and restored on drop — a swap, not a clear — so a
/// re-entrant run nests its scope above the outer run's and reveals it again
/// when the inner guard drops. That save/restore discipline (and the safety of
/// [`request_foreground_cancel`] reading the slot) requires publications to
/// nest LIFO on a single thread: at most one session per process publishes —
/// the signal-facing one, marked by `SessionState::publishes_signal_slots`. A
/// forked session's runs never publish; hosts cancel them through their own
/// scope handles. The scope the slot points at must outlive the guard.
pub struct CancelSlot {
    slot: &'static AtomicPtr<AtomicU8>,
    prev: *mut AtomicU8,
}

impl Drop for CancelSlot {
    fn drop(&mut self) {
        self.slot.store(self.prev, Ordering::Release);
    }
}

/// Publish `scope`'s flag into `slot`, saving the prior pointer for the
/// returned guard to restore on drop.
fn publish(slot: &'static AtomicPtr<AtomicU8>, scope: &CancelScope) -> CancelSlot {
    let prev = slot.swap(scope.flag_ptr().cast_mut(), Ordering::Release);
    CancelSlot { slot, prev }
}

/// Deliver `cause` onto the scope currently published in `slot`.
///
/// Signal-safe: a lock-free slot load and an atomic `fetch_max`. A no-op when
/// the slot is null.
fn request(slot: &AtomicPtr<AtomicU8>, cause: CancelCause) {
    let p = slot.load(Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: a non-null slot points into the live scope's `Arc` (the
        // publishing guard restores the prior pointer before that scope can
        // drop), so the flag is valid for this RMW.
        unsafe {
            (*p).fetch_max(cause as u8, Ordering::Release);
        }
    }
}

/// Publish `scope`'s flag as the foreground-cancel target for the returned
/// guard's lifetime, saving and restoring the prior publication on drop.
pub fn publish_foreground(scope: &CancelScope) -> CancelSlot {
    publish(&FOREGROUND_SCOPE, scope)
}

/// Publish `scope`'s flag as the durable-root-cancel target for the returned
/// guard's lifetime, saving and restoring the prior publication on drop.
pub fn publish_durable_root(scope: &CancelScope) -> CancelSlot {
    publish(&DURABLE_ROOT_SCOPE, scope)
}

/// Deliver `cause` to the current run's foreground scope; a no-op between
/// runs.
///
/// The foreground scope is the only thing affected — detached workers
/// poll their own scopes, not this one, so they are spared.
pub fn request_foreground_cancel(cause: CancelCause) {
    request(&FOREGROUND_SCOPE, cause);
}

/// Deliver `cause` to the session's durable root, reaching the foreground run
/// and every detached worker (all parented under it); a no-op when no root is
/// published.
pub fn request_root_cancel(cause: CancelCause) {
    request(&DURABLE_ROOT_SCOPE, cause);
}

/// Read [`FOREGROUND_SCOPE`] without a [`CancelScope`] in hand — the read-side
/// dual of [`request_foreground_cancel`], for a context parked outside the
/// evaluator's own `shell.run.cancel`.
///
/// The wire engine's enquiry desk, parked on a rendezvous, has no `&Shell` to
/// poll [`check`](crate::process::check) with, but runs on the same thread that
/// published this run's foreground scope, so the slot it reads is exactly that
/// run's. `None` between runs (null slot) or when the published scope isn't
/// cancelled.
pub fn foreground_cancel_cause() -> Option<CancelCause> {
    let p = FOREGROUND_SCOPE.load(Ordering::Acquire);
    if p.is_null() {
        return None;
    }
    // SAFETY: as in `request` — a non-null slot points into the live
    // foreground scope's flag for the reader's whole call.
    let flag = unsafe { (*p).load(Ordering::Acquire) };
    CancelCause::from_u8(flag)
}

// ── Typed root / foreground relation ───────────────────────────────────────
//
// Two newtypes name the one structural invariant the cancel tree must keep: a
// run's foreground scope is always a descendant of the session's durable
// root.  A [`ForegroundScope`] can only be minted from a [`DurableRoot`] (or
// by nesting another foreground), so an unrelated root can never be installed
// as a run's foreground by accident.  Both wrap a private [`CancelScope`];
// `as_scope` borrows it for the few places that still need the bare handle
// (signal-slot publication, a worker's returned cancel token).

/// The session's durable cancel root.
///
/// Detached workers (`spawn`, `watch`,
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

    /// Mint a [`ForegroundScope`] descending from this root — a run's
    /// foreground or a detached worker's scope.  The only entry into
    /// foreground construction, so the descendant invariant holds by type.
    pub fn child(&self) -> ForegroundScope {
        ForegroundScope(self.0.child())
    }

    /// Record `cause` on the root, reaching the foreground run and every
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

/// A cancel scope installed as a run's foreground work scope.
///
/// Constructed
/// only from a [`DurableRoot`] or by nesting another `ForegroundScope`, so it
/// is always a descendant of the session root.  A foreground cancel — a run
/// timeout, a Ctrl-C — reaches it and its same-thread descendants.
#[derive(Debug, Clone)]
pub struct ForegroundScope(CancelScope);

impl ForegroundScope {
    /// Nest a child foreground scope: a deadline window over a run, or the
    /// shared scope a same-thread body inherits.  Still root-descended.
    pub fn child(&self) -> Self {
        Self(self.0.child())
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

/// Serializes every publisher and asserter of the process-global cancel
/// slots ([`FOREGROUND_SCOPE`], [`DURABLE_ROOT_SCOPE`]) across the ral-core
/// test binary. The run tests publish the slots too (every run door
/// does), so the signal slot tests and the run tests must share one lock to
/// keep their publications and requests from interleaving.
#[cfg(test)]
pub(crate) static SLOT_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    /// The flag byte round-trips through [`CancelCause::from_u8`] for
    /// every cause, and the escalation order is total: a stronger cause's
    /// encoding always compares above a weaker one's, so `fetch_max` on
    /// the flag byte implements "never downgrade" correctly.
    #[test]
    fn cause_encoding_roundtrips_and_orders() {
        let causes = [
            CancelCause::Interrupt,
            CancelCause::Explicit,
            CancelCause::Deadline,
            CancelCause::Terminate,
            CancelCause::RootAbort,
        ];
        for pair in causes.windows(2) {
            assert!(
                pair[0] < pair[1],
                "{:?} must rank below {:?}",
                pair[0],
                pair[1]
            );
        }
        for cause in causes {
            assert_eq!(
                CancelCause::from_u8(cause as u8),
                Some(cause),
                "{cause:?} must round-trip through its flag encoding"
            );
        }
        assert_eq!(CancelCause::from_u8(0), None, "0 means uncancelled");
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
        // The slot is null again; a request between runs touches nothing.
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
