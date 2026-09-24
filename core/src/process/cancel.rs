//! Cooperative structured-concurrency cancellation.
//!
//! A scope's cancellation is the join of its own flag with every ancestor's —
//! walked, not flattened, so a subscope carries its own flag while still
//! observing its parents.  Workers read it wherever
//! [`check`](crate::process::check) is called.
//!
//! A signal handler holds no scope, so it raises one of two ambient causes
//! instead, and no scope folds either: a host hears them through
//! [`forward_ambient`] and delivers each to its engine as `Control`.  A
//! shutdown request is absolute — once raised it holds for every listener
//! forever; an interrupt is aimed at whatever ran when the key was struck, so a
//! listener registered after it never hears it.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::sync::LockExt as _;

/// Why a [`CancelScope`] was cancelled.
///
/// The causes escalate `ReaderGone < Interrupt < Explicit < Deadline <
/// Terminate < RootAbort`; a scope records the highest ever applied to it and
/// never downgrades.  The numeric values are the on-flag encoding, `0`
/// meaning uncancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CancelCause {
    /// A pipeline stage's reader stage has already been observed; the
    /// mildest cause, and the one ending `WaitOutcome::classify` forgives.
    ReaderGone = 1,
    /// Ctrl-C / Esc.
    Interrupt = 2,
    /// A targeted teardown: `cancel <handle>`, or a `race` loser reaped.
    Explicit = 3,
    /// A wall-clock or lifetime ceiling expired.
    Deadline = 4,
    /// SIGTERM / SIGHUP.  Lands on the durable root, so it reaches detached
    /// workers and not just the foreground run.
    Terminate = 5,
    /// Ctrl-\, reaping the session root.
    RootAbort = 6,
}

/// How long ral's teardown waits between its cause signal and the kill that
/// ends the argument.  Short — a cancelled call is already over budget — but
/// enough for a test runner to print its summary and exit.
pub(crate) const TEARDOWN_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

impl CancelCause {
    /// Every cause, mildest first.
    pub const ALL: [Self; 6] = [
        Self::ReaderGone,
        Self::Interrupt,
        Self::Explicit,
        Self::Deadline,
        Self::Terminate,
        Self::RootAbort,
    ];

    fn from_u8(flag: u8) -> Option<Self> {
        match flag {
            1 => Some(Self::ReaderGone),
            2 => Some(Self::Interrupt),
            3 => Some(Self::Explicit),
            4 => Some(Self::Deadline),
            5 => Some(Self::Terminate),
            6 => Some(Self::RootAbort),
            _ => None,
        }
    }

    /// The word every poll point and every host's parked wait reports this
    /// cause with, so the phrasing cannot drift between them.
    ///
    /// For rendering a cancellation that already exists — the `Display`
    /// impl, the process exit byte — never for minting one; minting is
    /// `Error::cancelled(cause)`'s alone.
    pub fn message(self) -> &'static str {
        match self {
            Self::ReaderGone => "its reader ended",
            Self::Interrupt => "interrupted",
            Self::Explicit => "cancelled",
            Self::Deadline => "timed out",
            Self::Terminate => "terminated",
            Self::RootAbort => "aborted",
        }
    }

    /// The tag `$err[reason]` names this cause by, under `` `cancelled ``.
    pub fn label(self) -> &'static str {
        match self {
            Self::ReaderGone => "reader-gone",
            Self::Interrupt => "interrupted",
            Self::Explicit => "cancelled",
            Self::Deadline => "timed-out",
            Self::Terminate => "terminated",
            Self::RootAbort => "aborted",
        }
    }
}

// ── The ambient causes ─────────────────────────────────────────────────────

/// The process-wide shutdown request (SIGTERM / SIGHUP, Ctrl-\\).  Absolute and
/// one-way, so a signal delivered before a host listens still reaches it.
static REQUESTED_ROOT: AtomicU8 = AtomicU8::new(0);

/// Interrupts raised so far.  Only its own modification order is load-bearing:
/// it orders each interrupt against a listener's registration.
static INTERRUPTS: AtomicU64 = AtomicU64::new(0);

/// Raise an interrupt, reaching every [`forward_ambient`] listener registered
/// before this instant.
///
/// Async-signal-safe: one atomic read-modify-write and a reaper `kick` —
/// itself one `write(2)` on Unix — no allocation, no lock.
pub fn request_interrupt() {
    INTERRUPTS.fetch_add(1, Ordering::Release);
    super::reaper::kick();
}

/// Request the session's end with `cause`, reaching every [`forward_ambient`]
/// listener, whenever it registers.  Async-signal-safe, as
/// [`request_interrupt`] is.
pub fn request_root_cancel(cause: CancelCause) {
    REQUESTED_ROOT.fetch_max(cause as u8, Ordering::Release);
    super::reaper::kick();
}

/// Hand the shutdown request back, which no host ever does.  The ral-core test
/// binary is one process hosting many sessions, so a test that raises it clears
/// it again rather than terminating every listener that follows.
#[cfg(test)]
pub(crate) fn clear_root_request() {
    REQUESTED_ROOT.store(0, Ordering::Release);
}

// ── Forwarding the ambient causes ──────────────────────────────────────────

/// One ambient cause, as a host hears it: aimed at the run in flight, or at
/// the session's durable root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ambient {
    Interrupt,
    Root(CancelCause),
}

/// A forwarder's high-water marks: the interrupts and the root request it has
/// already passed on.
struct Listener {
    interrupts_heard: u64,
    root_heard: u8,
    tx: std::sync::mpsc::Sender<Ambient>,
}

static LISTENERS: Mutex<Vec<(u64, Listener)>> = Mutex::new(Vec::new());

/// Hand every ambient cause raised from now on to `forward`, on a thread of
/// its own, until the guard drops.
///
/// A root request already standing is handed on at once, being absolute; an
/// interrupt raised before now is not, having been aimed at a run that was in
/// flight then.
///
/// # Panics
/// If the forwarding thread cannot be spawned.
pub fn forward_ambient(forward: impl Fn(Ambient) + Send + 'static) -> AmbientForward {
    #[cfg(unix)]
    super::reaper::ensure_installed();
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("ral-signals".into())
        .spawn(move || rx.iter().for_each(forward))
        .expect("spawn the signal forwarder");
    let listener = Listener {
        interrupts_heard: INTERRUPTS.load(Ordering::Acquire),
        root_heard: 0,
        tx,
    };
    LISTENERS.lock_ignore_poison().push((id, listener));
    scan_ambient();
    AmbientForward { id }
}

/// Pass each listener what was raised since it last heard; interrupts raised
/// together pass on as one.  Run on every reaper kick, so a signal handler's
/// raise arrives here off signal context.
pub(crate) fn scan_ambient() {
    let interrupts = INTERRUPTS.load(Ordering::Acquire);
    let root = REQUESTED_ROOT.load(Ordering::Acquire);
    for (_, l) in LISTENERS.lock_ignore_poison().iter_mut() {
        if interrupts > l.interrupts_heard {
            l.interrupts_heard = interrupts;
            let _ = l.tx.send(Ambient::Interrupt);
        }
        if root > l.root_heard {
            l.root_heard = root;
            if let Some(cause) = CancelCause::from_u8(root) {
                let _ = l.tx.send(Ambient::Root(cause));
            }
        }
    }
}

/// A registered [`forward_ambient`]; dropping it ends the forwarding thread.
#[must_use]
pub struct AmbientForward {
    id: u64,
}

impl Drop for AmbientForward {
    fn drop(&mut self) {
        LISTENERS
            .lock_ignore_poison()
            .retain(|(id, _)| *id != self.id);
    }
}

#[derive(Debug)]
struct ScopeNode {
    flag: AtomicU8,
    parent: Option<std::sync::Arc<Self>>,
}

/// A handle into the cancel-scope tree.
///
/// Cheap to clone (one `Arc` bump), cheap to check (a chain of atomic loads).
/// Cancellation is one-way and monotone in the [`CancelCause`] order: a
/// cancelled scope stays cancelled and its recorded cause only rises.
#[derive(Debug, Clone)]
pub struct CancelScope(std::sync::Arc<ScopeNode>);

impl CancelScope {
    /// The one constructor every scope in the tree goes through.
    fn mint(parent: Option<std::sync::Arc<ScopeNode>>) -> Self {
        Self(std::sync::Arc::new(ScopeNode {
            flag: AtomicU8::new(0),
            parent,
        }))
    }

    /// A fresh top-level scope.  Spawned workers take a
    /// [`child`](Self::child) instead, so cancellation reaches them.
    pub(crate) fn root() -> Self {
        Self::mint(None)
    }

    /// A scope nested under `self`, cancelled by `self` or any ancestor.
    pub fn child(&self) -> Self {
        Self::mint(Some(self.0.clone()))
    }

    /// Raise this scope's flag to `cause`, never downgrading, then fire every
    /// [`CancelWatch`] whose cause is now in force.
    pub fn cancel(&self, cause: CancelCause) {
        self.0.flag.fetch_max(cause as u8, Ordering::Release);
        scan_cancels();
    }

    /// The join of every flag on this scope's chain — `0` when nothing is in
    /// force.  The single reader, so no observer sees a cancellation except as
    /// the whole join.
    fn fold(&self) -> u8 {
        let mut node: &std::sync::Arc<ScopeNode> = &self.0;
        let mut join = 0u8;
        loop {
            join = join.max(node.flag.load(Ordering::Acquire));
            match &node.parent {
                Some(p) => node = p,
                None => return join,
            }
        }
    }

    /// True if anything in this scope's join is in force.
    pub fn is_cancelled(&self) -> bool {
        self.fold() != 0
    }

    /// The strongest [`CancelCause`] in this scope's join, or `None`.
    pub fn cause(&self) -> Option<CancelCause> {
        CancelCause::from_u8(self.fold())
    }
}

impl Default for CancelScope {
    fn default() -> Self {
        Self::root()
    }
}

// ── Cancel watchers ─────────────────────────────────────────────────────────
//
// A registration table beside the scope tree, for a subscriber with no poll
// point of its own: the pipeline collector, `RunningChild::wait`, the engine
// enquiry park.  A cause lands only through `CancelScope::cancel`, which scans
// the table synchronously.

type OnCancel = Box<dyn FnOnce(CancelCause) + Send>;

struct WatchEntry {
    scope: CancelScope,
    on_cancel: OnCancel,
    armed: Arc<AtomicBool>,
}

static WATCHES: OnceLock<Mutex<Vec<WatchEntry>>> = OnceLock::new();

fn watches() -> &'static Mutex<Vec<WatchEntry>> {
    WATCHES.get_or_init(|| Mutex::new(Vec::new()))
}

/// Run `on_cancel` once, with the cause, as soon as `scope.cause()` is
/// `Some`.  Dropping the returned [`CancelWatch`] disarms it.
pub fn watch_cancel(
    scope: CancelScope,
    on_cancel: impl FnOnce(CancelCause) + Send + 'static,
) -> CancelWatch {
    let armed = Arc::new(AtomicBool::new(true));
    let mut table = watches().lock_ignore_poison();
    table.push(WatchEntry {
        scope,
        on_cancel: Box::new(on_cancel),
        armed: armed.clone(),
    });
    // Insert, then check this entry alone: a cause raised between a caller's
    // own check and this registration is not missed, since both this check
    // and `scan_cancels` take the same lock this insert just released.
    let idx = table.len() - 1;
    let fired = table[idx]
        .scope
        .cause()
        .map(|cause| (table.swap_remove(idx).on_cancel, cause));
    drop(table);
    if let Some((on_cancel, cause)) = fired {
        on_cancel(cause);
    }
    CancelWatch { armed }
}

/// Fire and remove every armed registration whose cause is now `Some` —
/// under the table lock, entries are only taken out; their closures run
/// after the lock is released, so a closure may itself cancel a scope
/// without deadlocking on this same table.
fn scan_cancels() {
    let mut fired: Vec<(OnCancel, CancelCause)> = Vec::new();
    {
        let mut table = watches().lock_ignore_poison();
        let mut i = 0;
        while i < table.len() {
            if !table[i].armed.load(Ordering::Acquire) {
                table.swap_remove(i);
                continue;
            }
            match table[i].scope.cause() {
                Some(cause) => fired.push((table.swap_remove(i).on_cancel, cause)),
                None => i += 1,
            }
        }
    }
    for (on_cancel, cause) in fired {
        on_cancel(cause);
    }
}

/// A registered [`watch_cancel`]; dropping it disarms the registration.
#[must_use]
pub struct CancelWatch {
    armed: Arc<AtomicBool>,
}

impl Drop for CancelWatch {
    fn drop(&mut self) {
        self.armed.store(false, Ordering::Release);
    }
}

// ── Typed root / foreground relation ───────────────────────────────────────
//
// The newtypes name one structural invariant: a `ForegroundScope` can only be
// minted from a `DurableRoot` or by nesting another, so a run's foreground is
// always a descendant of the session's durable root and no unrelated root can
// be installed as one by accident.

/// The session's durable cancel root.
///
/// Detached workers (`spawn`, `watch`, `par`) parent under it, so a foreground
/// cancel never reaches one: only a cancel on the root itself, or on the
/// worker's own scope, stops it.
#[derive(Debug, Clone)]
pub struct DurableRoot(CancelScope);

impl DurableRoot {
    /// Mint a fresh session root.  One per [`Shell`](crate::types::Shell).
    pub(crate) fn new() -> Self {
        Self(CancelScope::root())
    }

    /// Mint a scope under this root that is *not* a run's foreground — a
    /// detached worker's, and the anchor a session boots with.  A run's frame
    /// nests under the anchor, so by that shape cancelling it never reaches a
    /// worker, while cancelling the root reaches both.
    pub fn worker(&self) -> ForegroundScope {
        ForegroundScope(self.0.child())
    }

    /// Record `cause` on the root, reaching the foreground run and every
    /// detached worker at once.
    pub fn cancel(&self, cause: CancelCause) {
        self.0.cancel(cause);
    }

    /// Borrow the underlying scope for a host polling the session without a run
    /// in hand.
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
/// Minted only from a [`DurableRoot`] or by nesting another, so it always
/// descends from the session root.
#[derive(Debug, Clone)]
pub struct ForegroundScope(CancelScope);

impl ForegroundScope {
    /// Nest a child foreground scope: a deadline window over a run, or the
    /// shared scope a same-thread body inherits.
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
    /// `None`.
    pub fn cause(&self) -> Option<CancelCause> {
        self.0.cause()
    }

    /// Borrow the underlying [`CancelScope`] for a worker handle, a running
    /// pipeline, or a host parked outside the evaluator's poll points.
    pub fn as_scope(&self) -> &CancelScope {
        &self.0
    }
}

/// A serialization-only test lock.  Poisoning is shrugged off rather than
/// cascading one test's failure into every later taker's.
#[cfg(test)]
pub(crate) struct Serial(std::sync::Mutex<()>);

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test scaffolding")]
impl Serial {
    pub(crate) const fn new() -> Self {
        Self(std::sync::Mutex::new(()))
    }

    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Serializes every test in the ral-core binary that touches the ambient causes
/// — by raising one, or by listening for them: a raise beside a concurrent
/// listener would reach it.
#[cfg(test)]
pub(crate) static REQUEST_SERIAL: Serial = Serial::new();

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test scaffolding")]
mod tests {
    use super::*;

    /// The escalation order being total is what makes `fetch_max` on the flag
    /// byte mean "never downgrade".
    #[test]
    fn cause_encoding_roundtrips_and_orders() {
        let causes = [
            CancelCause::ReaderGone,
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

    /// Sharing, not shadowing — a Ctrl-C during a nested run unwinds the whole
    /// nest, as a POSIX shell's would.
    #[test]
    fn a_nested_frame_observes_its_parents_interrupt() {
        let root = DurableRoot::new();
        let outer = root.worker().child();
        outer.cancel(CancelCause::Interrupt);
        let inner = outer.child();
        assert_eq!(
            inner.cause(),
            Some(CancelCause::Interrupt),
            "the nested run observes the interrupt through the frame it nests in"
        );
        assert_eq!(
            outer.cause(),
            Some(CancelCause::Interrupt),
            "and so does the run it nests in — sharing, not shadowing"
        );
    }

    /// Nesting each entry under the frame it displaces is what makes a scope's
    /// ancestors the runs enclosing it.
    #[test]
    fn an_outer_frames_deadline_reaches_the_nest() {
        let outer = DurableRoot::new().worker().child();
        let inner = outer.child();
        outer.cancel(CancelCause::Deadline);
        assert_eq!(
            inner.cause(),
            Some(CancelCause::Deadline),
            "the enclosing run's deadline must unwind the run nested in it"
        );
    }

    /// Detached is detached by the shape of the chain: a worker hangs beside
    /// the anchor a run's frame nests under, so only the root reaches both.
    #[test]
    fn a_worker_is_spared_the_interrupt_and_hears_the_shutdown() {
        let root = DurableRoot::new();
        let frame = root.worker().child();
        let worker = root.worker();
        frame.cancel(CancelCause::Interrupt);
        assert_eq!(
            worker.cause(),
            None,
            "a run's interrupt must not reach a detached worker"
        );
        root.cancel(CancelCause::Terminate);
        assert_eq!(
            worker.cause(),
            Some(CancelCause::Terminate),
            "but a shutdown reaches it through its root"
        );
    }

    /// A cause raised before registration is not missed: `watch_cancel`
    /// checks its own entry once, right after inserting.
    #[test]
    fn a_watch_registered_after_the_cause_fires_at_once() {
        let scope = CancelScope::root();
        scope.cancel(CancelCause::Explicit);
        let seen = Arc::new(Mutex::new(None));
        let recorded = seen.clone();
        let _watch = watch_cancel(scope, move |cause| {
            *recorded.lock_ignore_poison() = Some(cause);
        });
        assert_eq!(*seen.lock_ignore_poison(), Some(CancelCause::Explicit));
    }

    /// A watch registered before the cause fires when `cancel` scans.
    #[test]
    fn a_watch_registered_before_fires_on_cancel() {
        let scope = CancelScope::root();
        let seen = Arc::new(Mutex::new(None));
        let recorded = seen.clone();
        let _watch = watch_cancel(scope.clone(), move |cause| {
            *recorded.lock_ignore_poison() = Some(cause);
        });
        assert_eq!(*seen.lock_ignore_poison(), None);
        scope.cancel(CancelCause::Deadline);
        assert_eq!(*seen.lock_ignore_poison(), Some(CancelCause::Deadline));
    }

    /// Dropping the guard disarms the registration: a later cancel never
    /// fires it.
    #[test]
    fn a_dropped_guard_never_fires() {
        let scope = CancelScope::root();
        let fired = Arc::new(AtomicBool::new(false));
        let flag = fired.clone();
        drop(watch_cancel(scope.clone(), move |_| {
            flag.store(true, Ordering::Release);
        }));
        scope.cancel(CancelCause::Interrupt);
        assert!(
            !fired.load(Ordering::Acquire),
            "a disarmed watch must not fire"
        );
    }

    /// Two watches on one scope both fire: the table holds independent
    /// entries, not one slot per scope.
    #[test]
    fn two_watches_on_one_scope_both_fire() {
        let scope = CancelScope::root();
        let fired_a = Arc::new(AtomicBool::new(false));
        let fired_b = Arc::new(AtomicBool::new(false));
        let flag_a = fired_a.clone();
        let flag_b = fired_b.clone();
        let _watch_a = watch_cancel(scope.clone(), move |_| {
            flag_a.store(true, Ordering::Release);
        });
        let _watch_b = watch_cancel(scope.clone(), move |_| {
            flag_b.store(true, Ordering::Release);
        });
        scope.cancel(CancelCause::Interrupt);
        assert!(fired_a.load(Ordering::Acquire), "the first watch must fire");
        assert!(
            fired_b.load(Ordering::Acquire),
            "the second watch must fire"
        );
    }

    /// A forwarder hears the interrupts raised after it and none before, and a
    /// root request whenever it stands, being absolute.
    #[test]
    fn a_forwarder_hears_what_it_is_owed() {
        use std::time::Duration;
        let _g = REQUEST_SERIAL.lock();
        let listen = || {
            let (tx, rx) = std::sync::mpsc::channel();
            let guard = forward_ambient(move |ambient| {
                let _ = tx.send(ambient);
            });
            (guard, rx)
        };
        let owed = Duration::from_secs(5);
        let unowed = Duration::from_millis(200);

        request_interrupt();
        let (_first, first) = listen();
        assert!(
            first.recv_timeout(unowed).is_err(),
            "an interrupt raised before the forwarder was aimed at other runs"
        );
        request_interrupt();
        assert_eq!(
            first.recv_timeout(owed),
            Ok(Ambient::Interrupt),
            "an interrupt raised after it is forwarded"
        );
        request_root_cancel(CancelCause::Terminate);
        assert_eq!(
            first.recv_timeout(owed),
            Ok(Ambient::Root(CancelCause::Terminate)),
            "a shutdown request is forwarded"
        );

        let (_second, second) = listen();
        let standing = second.recv_timeout(owed);
        clear_root_request();
        assert_eq!(
            standing,
            Ok(Ambient::Root(CancelCause::Terminate)),
            "a shutdown request already standing is forwarded at once"
        );
        assert!(
            second.recv_timeout(unowed).is_err(),
            "and nothing else: the older interrupts were not its to hear"
        );
    }
}
