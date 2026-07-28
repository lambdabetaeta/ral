//! Cooperative structured-concurrency cancellation.
//!
//! A scope's cancellation is the join of its own flag with every ancestor's and
//! with the ambient causes those nodes fold — walked, not flattened, so a
//! subscope carries its own flag while still observing its parents.  Workers
//! read it wherever [`check`](crate::process::check) is called.
//!
//! The ambient causes let a signal handler or a TUI input thread, holding no
//! scope, contribute to that join.  A shutdown request is absolute — once
//! raised it holds for every observer forever — so it is a plain lattice
//! element; an interrupt is aimed at whatever ran when the key was struck, so
//! it is a monotone watermark read against a frame's birth instant, and a run
//! born after a Ctrl-C is deaf to it by construction.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

/// Why a [`CancelScope`] was cancelled.
///
/// The causes escalate `Interrupt < Explicit < Deadline < Terminate <
/// RootAbort`; a scope records the highest ever applied to it and never
/// downgrades.  The numeric values are the on-flag encoding, `0` meaning
/// uncancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CancelCause {
    /// Ctrl-C / Esc.
    Interrupt = 1,
    /// A targeted teardown: `cancel <handle>`, or a `race` loser reaped.
    Explicit = 2,
    /// A wall-clock or lifetime ceiling expired.
    Deadline = 3,
    /// SIGTERM / SIGHUP.  Lands on the durable root, so it reaches detached
    /// workers and not just the foreground run.
    Terminate = 4,
    /// Ctrl-\, reaping the session root.
    RootAbort = 5,
}

impl CancelCause {
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

    /// The word every poll point and every host's parked wait reports this
    /// cause with, so the phrasing cannot drift between them.
    pub fn message(self) -> &'static str {
        match self {
            Self::Interrupt => "interrupted",
            Self::Explicit => "cancelled",
            Self::Deadline => "timed out",
            Self::Terminate => "terminated",
            Self::RootAbort => "aborted",
        }
    }

    /// The status paired with [`message`](Self::message): 130 (`128 + SIGINT`)
    /// for every interactive-shaped cancellation, 143 (`128 + SIGTERM`) for a
    /// shutdown request — what a supervisor that SIGTERMed us reads back.
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Terminate => 143,
            _ => 130,
        }
    }
}

// ── The ambient causes ─────────────────────────────────────────────────────

/// The process-wide shutdown request (SIGTERM / SIGHUP, Ctrl-\).  Absolute and
/// one-way, so a signal delivered while the session sits idle latches and is
/// read at the next boundary.
static REQUESTED_ROOT: AtomicU8 = AtomicU8::new(0);

/// The instant counter the interrupt watermark is indexed by.  Only its own
/// modification order is load-bearing — it orders a frame's birth against every
/// interrupt and synchronises nothing else.
static CLOCK: AtomicU64 = AtomicU64::new(0);

/// The interrupt watermark: per [`CancelCause`], the instant it was last raised
/// at, `0` for never.  A frame born at `b` observes cause `c` exactly when
/// `STAMPED[c] > b`, so causes aimed at settled runs retire themselves as the
/// clock passes them.
///
/// One stamp per cause and not one packed `(instant, cause)` word, because a
/// frame reads a *suffix* of the escalation order and one word keeps only one
/// coordinate: instant-major, a young weak cause erases an old strong one;
/// cause-major, an old strong one hides a young weak one from every frame born
/// between them.
static STAMPED: [AtomicU64; CancelCause::RootAbort as usize] =
    [const { AtomicU64::new(0) }; CancelCause::RootAbort as usize];

/// Raise `cause` on the interrupt watermark, reaching the foreground of every
/// run already in flight and of none born after this instant.  Async-signal-
/// safe: two atomic read-modify-writes, no allocation, no lock.
pub fn request_foreground_cancel(cause: CancelCause) {
    let now = CLOCK.fetch_add(1, Ordering::Relaxed) + 1;
    STAMPED[cause as usize - 1].fetch_max(now, Ordering::Release);
}

/// Deliver `cause` to the signal-facing session's durable root, reaching its
/// foreground run and every detached worker parented under it.
pub fn request_root_cancel(cause: CancelCause) {
    REQUESTED_ROOT.fetch_max(cause as u8, Ordering::Release);
}

/// Hand the shutdown request back, which no host ever does.  The ral-core test
/// binary is one process hosting many sessions, so a test that raises it clears
/// it again rather than terminating every session that follows.
#[cfg(test)]
pub(crate) fn clear_root_request() {
    REQUESTED_ROOT.store(0, Ordering::Release);
}

/// What a node folds from the ambient causes — fixed at mint, never installed
/// later.
#[derive(Debug, Clone, Copy)]
enum Hears {
    /// Nothing of its own: a deaf session's scopes, a detached worker, and
    /// every nested scope, which hears its ancestors' by walking to them.
    Nothing,
    /// `REQUESTED_ROOT`: the signal-facing session's durable root.
    Shutdown,
    /// Every cause on `STAMPED` raised after this instant: a run's foreground
    /// frame, born under a root that faces signals.
    InterruptsSince(u64),
}

#[derive(Debug)]
struct ScopeNode {
    flag: AtomicU8,
    hears: Hears,
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
    fn mint(hears: Hears, parent: Option<std::sync::Arc<ScopeNode>>) -> Self {
        Self(std::sync::Arc::new(ScopeNode {
            flag: AtomicU8::new(0),
            hears,
            parent,
        }))
    }

    /// A fresh top-level scope, deaf to the ambient causes.  Spawned workers
    /// take a [`child`](Self::child) instead, so cancellation reaches them.
    pub(crate) fn root() -> Self {
        Self::mint(Hears::Nothing, None)
    }

    /// A scope nested under `self`, cancelled by `self` or any ancestor.  It
    /// folds nothing of its own: what `self` hears from the ambient causes, the
    /// child hears by walking to the very nodes that fold them.
    pub fn child(&self) -> Self {
        Self::mint(Hears::Nothing, Some(self.0.clone()))
    }

    /// Raise this scope's flag to `cause`, never downgrading.
    pub fn cancel(&self, cause: CancelCause) {
        self.0.flag.fetch_max(cause as u8, Ordering::Release);
    }

    /// The join of every flag on this scope's chain with the ambient causes
    /// those nodes fold — `0` when nothing is in force.  The single reader of
    /// either, so no observer sees a cancellation except as the whole join.
    fn fold(&self) -> u8 {
        let mut node: &std::sync::Arc<ScopeNode> = &self.0;
        let mut join = 0u8;
        loop {
            join = join.max(node.flag.load(Ordering::Acquire));
            join = join.max(match node.hears {
                Hears::Nothing => 0,
                Hears::Shutdown => REQUESTED_ROOT.load(Ordering::Acquire),
                // The strongest cause stamped after this frame was born is the
                // join of every cause stamped after it.
                Hears::InterruptsSince(birth) => (1..=CancelCause::RootAbort as u8)
                    .rev()
                    .find(|c| STAMPED[*c as usize - 1].load(Ordering::Acquire) > birth)
                    .unwrap_or(0),
            });
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
    /// Mint a fresh session root, deaf to the ambient causes.  One per
    /// [`Shell`](crate::types::Shell); a host that owns the process's signals
    /// re-mints it with [`face_signals`](crate::types::Shell::face_signals).
    pub(crate) fn new() -> Self {
        Self(CancelScope::root())
    }

    /// Mint the signal-facing session's root: it folds the ambient shutdown
    /// request, so a SIGTERM reaches this session's foreground run and every
    /// detached worker parented under it — and latches while it is idle.
    pub(crate) fn signal_facing() -> Self {
        Self(CancelScope::mint(Hears::Shutdown, None))
    }

    /// Mint the foreground frame of a run entering under `displaced`.
    ///
    /// Nested under that frame rather than beside it, so the tree *is* the
    /// runs' dynamic extent: a nested run observes the interrupt its outer run
    /// carries, and an outer run's wall reaches into the nest.  Stamped with
    /// its birth instant under a signal-facing root, deaf under any other.
    pub fn foreground(&self, displaced: &ForegroundScope) -> ForegroundScope {
        let hears = match self.0.0.hears {
            Hears::Shutdown => Hears::InterruptsSince(CLOCK.load(Ordering::Acquire)),
            _ => Hears::Nothing,
        };
        ForegroundScope(CancelScope::mint(hears, Some(displaced.0.0.clone())))
    }

    /// Mint a scope under this root that is *not* a run's foreground — a
    /// detached worker's, and the anchor a session boots with.  It folds the
    /// shutdown request through this root, while no node on its chain carries a
    /// birth instant, so by that shape it cannot absorb a foreground interrupt.
    pub fn worker(&self) -> ForegroundScope {
        ForegroundScope(CancelScope::mint(Hears::Nothing, Some(self.0.0.clone())))
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
/// — by raising one, or by minting a scope that folds one: a frame born beside
/// a concurrent raise may fall on either side of it.
#[cfg(test)]
pub(crate) static REQUEST_SERIAL: Serial = Serial::new();

#[cfg(test)]
mod tests {
    use super::*;

    /// The escalation order being total is what makes `fetch_max` on the flag
    /// byte mean "never downgrade".
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

    /// The watermark retires itself as the clock passes it, which is what keeps
    /// a Ctrl-C for a settled command off the next one.
    #[test]
    fn an_interrupt_reaches_the_frames_older_than_it_and_no_other() {
        let _g = REQUEST_SERIAL.lock();
        let root = DurableRoot::signal_facing();
        let boot = root.worker();
        let older = root.foreground(&boot);
        request_foreground_cancel(CancelCause::Interrupt);
        assert_eq!(
            older.cause(),
            Some(CancelCause::Interrupt),
            "an interrupt must reach the run that was in flight when it was raised"
        );
        assert_eq!(
            root.foreground(&boot).cause(),
            None,
            "and no frame born after it, with nothing handed back for it to miss"
        );
    }

    #[test]
    fn an_older_frame_joins_every_interrupt_raised_after_it() {
        let _g = REQUEST_SERIAL.lock();
        let root = DurableRoot::signal_facing();
        let boot = root.worker();
        let frame = root.foreground(&boot);
        request_foreground_cancel(CancelCause::Deadline);
        request_foreground_cancel(CancelCause::Interrupt);
        assert_eq!(
            frame.cause(),
            Some(CancelCause::Deadline),
            "the join is the strongest cause raised after the frame, not the latest"
        );
    }

    /// Why the watermark is one stamp per cause and not one `(instant, cause)`
    /// word: no single word records both readings — instant-major loses the
    /// first assertion, cause-major the second.
    #[test]
    fn each_cause_keeps_its_own_instant() {
        let _g = REQUEST_SERIAL.lock();
        let root = DurableRoot::signal_facing();
        let boot = root.worker();
        let before = root.foreground(&boot);
        request_foreground_cancel(CancelCause::Deadline);
        let between = root.foreground(&boot);
        request_foreground_cancel(CancelCause::Interrupt);
        assert_eq!(
            before.cause(),
            Some(CancelCause::Deadline),
            "the older frame keeps the stronger cause a younger weak one must not erase"
        );
        assert_eq!(
            between.cause(),
            Some(CancelCause::Interrupt),
            "the younger frame hears the weak cause the older strong one must not hide"
        );
    }

    /// Sharing, not shadowing — a Ctrl-C during a nested run unwinds the whole
    /// nest, as a POSIX shell's would.
    #[test]
    fn a_nested_frame_observes_its_parents_interrupt() {
        let _g = REQUEST_SERIAL.lock();
        let root = DurableRoot::signal_facing();
        let boot = root.worker();
        let outer = root.foreground(&boot);
        request_foreground_cancel(CancelCause::Interrupt);
        let inner = root.foreground(&outer);
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
        let _g = REQUEST_SERIAL.lock();
        let root = DurableRoot::signal_facing();
        let boot = root.worker();
        let outer = root.foreground(&boot);
        let inner = root.foreground(&outer);
        outer.cancel(CancelCause::Deadline);
        assert_eq!(
            inner.cause(),
            Some(CancelCause::Deadline),
            "the enclosing run's deadline must unwind the run nested in it"
        );
    }

    /// Detached is detached by the shape of the chain — no birth instant on it
    /// to read the watermark against — whoever spawned the worker.
    #[test]
    fn a_worker_is_spared_the_interrupt_and_hears_the_shutdown() {
        let _g = REQUEST_SERIAL.lock();
        let worker = DurableRoot::signal_facing().worker();
        request_foreground_cancel(CancelCause::Interrupt);
        assert_eq!(
            worker.cause(),
            None,
            "a foreground interrupt must not reach a detached worker"
        );
        request_root_cancel(CancelCause::Terminate);
        assert_eq!(
            worker.cause(),
            Some(CancelCause::Terminate),
            "but a shutdown request reaches it through its root"
        );
        clear_root_request();
    }

    #[test]
    fn root_request_is_independent_of_the_watermark() {
        let _g = REQUEST_SERIAL.lock();
        let root = DurableRoot::signal_facing();
        let boot = root.worker();
        let facing = root.foreground(&boot);
        let deaf_root = DurableRoot::new();
        let deaf = deaf_root.foreground(&deaf_root.worker());

        request_foreground_cancel(CancelCause::Interrupt);
        assert_eq!(
            facing.cause(),
            Some(CancelCause::Interrupt),
            "an interrupt reaches a facing run's frame"
        );
        assert_eq!(
            root.as_scope().cause(),
            None,
            "an interrupt must not touch the durable root"
        );

        request_root_cancel(CancelCause::RootAbort);
        assert_eq!(
            root.as_scope().cause(),
            Some(CancelCause::RootAbort),
            "a root request reaches the facing durable root"
        );
        assert_eq!(
            facing.cause(),
            Some(CancelCause::RootAbort),
            "and the foreground observes it through its root ancestry"
        );
        assert_eq!(
            deaf.cause(),
            None,
            "a session that faces no signals is deaf to both ambient causes"
        );
        clear_root_request();
    }
}
