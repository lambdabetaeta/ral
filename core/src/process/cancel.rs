//! Cooperative structured-concurrency cancellation.
//!
//! A worker checks `is_cancelled` at well-defined poll points (the same
//! places [`check`](crate::process::check) is called); cancelling an outer
//! scope propagates to every inner scope, so a top-level Ctrl-\ root abort — or
//! a run deadline — unwinds every thread that inherited the scope at its next
//! poll point.
//!
//! Cancellation is a *join-semilattice*: [`CancelCause`] is totally ordered,
//! [`cancel`](CancelScope::cancel) is a `fetch_max`, and a scope's
//! cancellation is the join of its own element with every ancestor's — walked,
//! not flattened, so a subscope can carry its own flag while still observing
//! its parents.  A context holding no scope — a signal handler, a TUI input
//! thread — contributes an element to that join rather than reaching into the
//! tree: two process-lifetime *ambient causes*, folded by exactly the scopes
//! minted to face them.  Nothing is installed and nothing restored, so no
//! mutex, no allocation, and a handful of atomic loads per ancestor.
//!
//! The two ambient causes are different kinds of proposition, and their
//! shapes say so.  A shutdown request is **absolute**: once raised it holds
//! for every observer forever, so it is a plain lattice element
//! ([`REQUESTED_ROOT`]).  A user's interrupt is **temporal**: it is aimed at
//! whatever was running when the key was struck, so it is a monotone
//! watermark ([`STAMPED`]) read against a frame's *birth instant*.  A run born
//! after an interrupt is deaf to it by construction — which is why a Ctrl-C
//! for a settled command cannot unwind the next one, and why the prompt drawn
//! after it renders clean.  Nothing is ever handed back, so nobody needs the
//! authority to hand it back.
//!
//! [`CancelScope`] is the raw handle; [`DurableRoot`] and [`ForegroundScope`]
//! name the structural invariant the tree keeps — a run's foreground scope is
//! always a descendant of the session's durable root, and, since the run door
//! nests each entry under the frame it displaces, the tree is the runs'
//! dynamic extent.  An *aside* — a second [`Shell`](crate::types::Shell)
//! running plugin hooks beside the session — shares the session's
//! [`DurableRoot`] rather than minting one, so it is inside the session for
//! cancellation exactly as it is for everything else.
//!
//! The whole rule: **a scope observes every cause recorded on its chain, plus
//! every ambient interrupt younger than a frame on it.**

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

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

// ── The ambient causes ─────────────────────────────────────────────────────

/// The process-wide shutdown request (SIGTERM / SIGHUP, Ctrl-\), raised by
/// `fetch_max` — the exact store [`CancelScope::cancel`] performs,
/// async-signal-safe by construction.  Absolute and one-way, so a signal
/// delivered while the session sits idle latches and is read at the next
/// boundary.
static REQUESTED_ROOT: AtomicU8 = AtomicU8::new(0);

/// The instant counter the interrupt watermark is indexed by: ticked once per
/// raised interrupt, read once per foreground frame minted.  Only its own
/// modification order is load-bearing — it is what orders a frame's birth
/// against every interrupt — so the tick synchronises nothing else.
static CLOCK: AtomicU64 = AtomicU64::new(0);

/// The interrupt watermark: per [`CancelCause`], the instant it was last
/// raised at, `0` for never.  A frame born at `b` observes cause `c` exactly
/// when `STAMPED[c] > b`, so the causes aimed at runs that have settled
/// retire themselves as the clock passes them.
///
/// One stamp per cause rather than one packed `(instant, cause)` word,
/// because a frame reads a *suffix* of the escalation order and one word can
/// keep only one coordinate faithfully: ordered instant-major, a young weak
/// cause erases an old strong one; ordered cause-major, an old strong one
/// hides a young weak one from every frame born between them.
static STAMPED: [AtomicU64; CancelCause::RootAbort as usize] =
    [const { AtomicU64::new(0) }; CancelCause::RootAbort as usize];

/// Raise `cause` on the interrupt watermark, reaching the foreground of every
/// run already in flight and of none born after this instant.
///
/// Async-signal-safe: two atomic read-modify-writes, no allocation, no lock.
/// A detached worker's chain carries no birth instant at all, so by its shape
/// it cannot absorb a user's interrupt of the foreground.
pub fn request_foreground_cancel(cause: CancelCause) {
    let now = CLOCK.fetch_add(1, Ordering::Relaxed) + 1;
    STAMPED[cause as usize - 1].fetch_max(now, Ordering::Release);
}

/// Deliver `cause` to the signal-facing session's durable root, reaching its
/// foreground run and every detached worker parented under it.
pub fn request_root_cancel(cause: CancelCause) {
    REQUESTED_ROOT.fetch_max(cause as u8, Ordering::Release);
}

/// Hand [`REQUESTED_ROOT`] back, which no host ever does — a shutdown request
/// is one-way.  The ral-core test binary is one process hosting many
/// sessions, so a test that raises it clears it again under
/// [`REQUEST_SERIAL`] rather than terminating every session that follows.
/// The watermark needs no such courtesy: it clears itself.
#[cfg(test)]
pub(crate) fn clear_root_request() {
    REQUESTED_ROOT.store(0, Ordering::Release);
}

/// What a node folds from the ambient causes.  Fixed at mint: which of them
/// reaches a scope is a property of how it was constructed, never of
/// something installed later.
#[derive(Debug, Clone, Copy)]
enum Hears {
    /// Nothing of its own — a deaf session's scopes, a detached worker, and
    /// every nested scope, which hears its ancestors' by walking to them.
    Nothing,
    /// [`REQUESTED_ROOT`]: the signal-facing session's durable root.
    Shutdown,
    /// Every cause on [`STAMPED`] raised after this instant: a run's
    /// foreground frame, born under a root that faces signals.
    InterruptsSince(u64),
}

/// Internal node of the cancel-scope tree.
#[derive(Debug)]
struct ScopeNode {
    flag: AtomicU8,
    hears: Hears,
    parent: Option<std::sync::Arc<Self>>,
}

/// A handle into the cancel-scope tree.
///
/// Cheap to clone (one `Arc` bump), cheap to check (a chain of atomic loads).
/// Built by [`root`](Self::root) — a fresh top-level scope — or by
/// [`child`](Self::child), nested under `self` so that cancelling `self` or
/// any ancestor cancels it.
///
/// Cancellation is one-way and monotone in the [`CancelCause`]
/// escalation order: once cancelled a scope stays cancelled, and the
/// recorded cause only ever rises.
#[derive(Debug, Clone)]
pub struct CancelScope(std::sync::Arc<ScopeNode>);

impl CancelScope {
    /// The one constructor: a scope folding `hears`, nested under `parent`.
    fn mint(hears: Hears, parent: Option<std::sync::Arc<ScopeNode>>) -> Self {
        Self(std::sync::Arc::new(ScopeNode {
            flag: AtomicU8::new(0),
            hears,
            parent,
        }))
    }

    /// A fresh root scope, deaf to the ambient causes.  Constructed only at
    /// session init (and the `Default` impl below); spawned workers take a
    /// [`child`](Self::child) of their parent's scope so cancellation
    /// reaches them.
    pub(crate) fn root() -> Self {
        Self::mint(Hears::Nothing, None)
    }

    /// A new scope nested under `self`.  Cancelling any ancestor (or `self`)
    /// cancels the returned child, and it folds nothing of its own: what
    /// `self` hears from the ambient causes, the child hears by walking to
    /// the very nodes that fold them.
    pub fn child(&self) -> Self {
        Self::mint(Hears::Nothing, Some(self.0.clone()))
    }

    /// Record `cause` on this scope's flag, raising it to the maximum of
    /// the existing and new cause.  Visible to every share / child of
    /// this scope at the next [`is_cancelled`](Self::is_cancelled) /
    /// [`cause`](Self::cause) poll.
    pub fn cancel(&self, cause: CancelCause) {
        self.0.flag.fetch_max(cause as u8, Ordering::Release);
    }

    /// The join of every flag along this scope's chain to the root with the
    /// ambient causes those nodes fold — `0` when nothing is in force.
    ///
    /// The single reader of a flag and of the ambient causes.
    /// [`is_cancelled`](Self::is_cancelled) and [`cause`](Self::cause) are
    /// its only callers, so no observer can read a cancellation except as
    /// the whole join.
    fn fold(&self) -> u8 {
        let mut node: &std::sync::Arc<ScopeNode> = &self.0;
        let mut join = 0u8;
        loop {
            join = join.max(node.flag.load(Ordering::Acquire));
            join = join.max(match node.hears {
                Hears::Nothing => 0,
                Hears::Shutdown => REQUESTED_ROOT.load(Ordering::Acquire),
                // Walk the escalation order downwards: the strongest cause
                // stamped after this frame was born is the join of every
                // cause stamped after it.
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

    /// The strongest [`CancelCause`] in force on this scope, or `None` if
    /// nothing is.  An ancestor [`RootAbort`](CancelCause::RootAbort)
    /// dominates a child [`Interrupt`](CancelCause::Interrupt), and so does
    /// a requested one.
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
// Two newtypes name the one structural invariant the cancel tree must keep: a
// run's foreground scope is always a descendant of the session's durable
// root.  A [`ForegroundScope`] can only be minted from a [`DurableRoot`] (or
// by nesting another foreground), so an unrelated root can never be installed
// as a run's foreground by accident.  Both wrap a private [`CancelScope`];
// `as_scope` borrows it for the few places that still need the bare handle
// (a worker's returned cancel token, a parked host wait).

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
    /// Mint a fresh session root, deaf to the ambient causes.  One per
    /// [`Shell`](crate::types::Shell), constructed at session init; a host
    /// that owns the process's signals re-mints it with
    /// [`Shell::face_signals`](crate::types::Shell::face_signals).
    pub(crate) fn new() -> Self {
        Self(CancelScope::root())
    }

    /// Mint the signal-facing session's root: it folds [`REQUESTED_ROOT`], so a
    /// shutdown request reaches this session's foreground run and every
    /// detached worker parented under it — and latches while it is idle.
    pub(crate) fn signal_facing() -> Self {
        Self(CancelScope::mint(Hears::Shutdown, None))
    }

    /// Mint the foreground frame of a run entering under `displaced` — the
    /// frame the run door swaps out for this run's extent.
    ///
    /// Nested under that frame rather than beside it, so the tree *is* the
    /// runs' dynamic extent: a nested run observes the interrupt its outer
    /// run already carries, and an outer run's wall reaches into the nest.
    /// Stamped with its birth instant under a root that faces signals — so it
    /// observes exactly the interrupts raised after it began — and deaf under
    /// a root that does not.
    pub fn foreground(&self, displaced: &ForegroundScope) -> ForegroundScope {
        let hears = match self.0.0.hears {
            Hears::Shutdown => Hears::InterruptsSince(CLOCK.load(Ordering::Acquire)),
            _ => Hears::Nothing,
        };
        ForegroundScope(CancelScope::mint(hears, Some(displaced.0.0.clone())))
    }

    /// Mint a scope under this root that is *not* a run's foreground — a
    /// detached worker's scope, and the frame a session boots with.  It
    /// still folds [`REQUESTED_ROOT`] through this root, so a SIGTERM reaches
    /// a detached worker, while no node on its chain carries a birth instant,
    /// so by the shape of that chain it cannot absorb a foreground
    /// interrupt — whoever spawned it.
    pub fn worker(&self) -> ForegroundScope {
        ForegroundScope(CancelScope::mint(Hears::Nothing, Some(self.0.0.clone())))
    }

    /// Record `cause` on the root, reaching the foreground run and every
    /// detached worker at once.
    pub fn cancel(&self, cause: CancelCause) {
        self.0.cancel(cause);
    }

    /// Borrow the underlying scope for a host that polls the session's
    /// cancellation without a run in hand.
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
    /// shared scope a same-thread body inherits.  Still root-descended, and
    /// with the parent's reach.
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

    /// Borrow the underlying scope to hand a bare [`CancelScope`] to a
    /// worker handle, a running pipeline, or a host parked outside the
    /// evaluator's poll points.
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

/// Serializes every test in the ral-core binary that touches the ambient
/// causes — by raising one, or by minting a scope that folds one: a frame
/// born beside a concurrent raise may or may not fall on its older side.
#[cfg(test)]
pub(crate) static REQUEST_SERIAL: Serial = Serial::new();

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

    /// An interrupt reaches the frames already born and no frame born after
    /// it: the watermark retires itself as the clock passes it, which is what
    /// keeps a Ctrl-C for a settled command off the next one.
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

    /// The watermark is monotone in the escalation order: a frame born before
    /// both a weak and a strong cause joins them, strongest first, whichever
    /// order they were raised in.
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

    /// Why the watermark is one stamp per cause and not one `(instant,
    /// cause)` word: a frame born *between* an old strong cause and a young
    /// weak one must observe the young one alone, while a frame born before
    /// both observes the strong one.  No single word records both readings —
    /// instant-major loses the first assertion, cause-major the second.
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

    /// A frame's interrupt is shared with its nest, not shadowed by it: a run
    /// entering under a frame already carrying an interrupt observes it
    /// through that frame, so a Ctrl-C during a nested run unwinds the whole
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

    /// An outer run's own cancellation — its wall elapsing — reaches the run
    /// nested inside it, because the nest descends from it.  Cancellation is
    /// the join with every ancestor, and after the run door's nesting the
    /// ancestors are the runs actually enclosing it.
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

    /// A detached worker is spared the interrupt and hears the shutdown
    /// request: no node on a `worker` chain carries a birth instant, so the
    /// watermark is unreadable from it, while the facing root above it
    /// carries a root request down.  Detached is detached — the shape of the
    /// chain says so, whoever spawned the worker.
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

    /// The two ambient causes are independent, and a session minted by
    /// [`DurableRoot::new`] is deaf to both: an interrupt never touches the
    /// root, a root request reaches the root *and* the foreground descending
    /// from it, and neither reaches the deaf session.
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
