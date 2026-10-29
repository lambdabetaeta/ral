//! Cooperative structured-concurrency cancellation.
//!
//! A scope's cancellation is the join of its own flag with every ancestor's —
//! walked, not flattened, so a subscope carries its own flag while still
//! observing its parents.  Computations read it with [`Scope::poll`] at every
//! step boundary, and every blocked wait re-reads it each [`POLL_SLICE`].
//!
//! A signal handler holds no scope, so it raises one of two ambient causes
//! instead, and no scope folds either: a host hears them through
//! [`forward_ambient`] and delivers each to its engine as `Control`.  A
//! shutdown request is absolute — once raised it holds for every listener
//! forever; an interrupt is aimed at whatever ran when the key was struck, so a
//! listener registered after it never hears it.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use strum::{IntoStaticStr, VariantArray};

use crate::sync::LockExt as _;

/// Why a [`Scope`] was cancelled.
///
/// The causes escalate `ReaderGone < Interrupted < Cancelled < TimedOut <
/// Terminated < Aborted`; a scope records the highest ever applied to it and
/// never downgrades.  The numeric values are the on-flag encoding, `0`
/// meaning uncancelled.  The variant names are the tags a `` `stopped ``
/// card and the wire's `Ending::Stopped` name a cause by.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    strum::FromRepr,
    IntoStaticStr,
    VariantArray,
    serde::Serialize,
    serde::Deserialize,
)]
#[strum(serialize_all = "kebab-case")]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum CancelCause {
    /// A pipeline stage's reader stage has already been observed; the
    /// mildest cause; its owner, the collector, forgives it.
    ReaderGone = 1,
    /// Ctrl-C / Esc.
    Interrupted = 2,
    /// A targeted teardown: `cancel <handle>`, or a `race` loser reaped.
    Cancelled = 3,
    /// A wall-clock or lifetime ceiling expired.
    TimedOut = 4,
    /// SIGTERM / SIGHUP.  Lands on the durable root, so it reaches
    /// workers and not just the foreground run.
    Terminated = 5,
    /// Ctrl-\, reaping the session root.
    Aborted = 6,
}

crate::label!(typed CancelCause);

/// How long ral's teardown waits between its cause signal and the kill that
/// ends the argument.  Short — a cancelled call is already over budget — but
/// enough for a test runner to print its summary and exit.
pub(crate) const TEARDOWN_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

impl CancelCause {
    /// The process exit status of a run this cause ended: a signal-born
    /// cause reports 128 plus its signal, a deadline `timeout(1)`'s 124.
    /// Async-signal-safe.
    pub const fn code(self) -> u8 {
        match self {
            Self::ReaderGone => 141,
            Self::Interrupted => 130,
            Self::Cancelled | Self::Terminated => 143,
            Self::TimedOut => 124,
            Self::Aborted => 131,
        }
    }

    /// The word every poll point and every host's parked wait reports this
    /// cause with, so the phrasing cannot drift between them.
    pub fn message(self) -> &'static str {
        match self {
            Self::ReaderGone => "its reader ended",
            Self::Interrupted => "interrupted",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed out",
            Self::Terminated => "terminated",
            Self::Aborted => "aborted",
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

/// Interrupts raised so far: a reader tells whether one landed since it looked.
pub fn interrupt_epoch() -> u64 {
    INTERRUPTS.load(Ordering::Acquire)
}

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
/// its own. Dropping the guard unregisters it, drains queued callbacks, and
/// waits for their captures to be released.
///
/// A root request already standing is handed on at once, being absolute; an
/// interrupt raised before now is not, having been aimed at a run that was in
/// flight then.
///
/// The callback must not destroy its own guard or wait on resources held by
/// the guard's dropping owner.
///
/// # Panics
/// If the forwarding thread cannot be spawned.
pub fn forward_ambient(forward: impl Fn(Ambient) + Send + 'static) -> AmbientForward {
    #[cfg(unix)]
    super::reaper::ensure_installed();
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = std::sync::mpsc::channel();
    let thread = std::thread::Builder::new()
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
    AmbientForward {
        id,
        thread: Some(thread),
    }
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
            if let Some(cause) = CancelCause::from_repr(root) {
                let _ = l.tx.send(Ambient::Root(cause));
            }
        }
    }
}

/// A registered [`forward_ambient`]; dropping it unregisters, drains queued
/// callbacks, and waits for their captures to be released.
///
/// A callback must not destroy this guard or wait on resources held by its
/// dropping owner.
#[must_use]
pub struct AmbientForward {
    id: u64,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for AmbientForward {
    fn drop(&mut self) {
        LISTENERS
            .lock_ignore_poison()
            .retain(|(id, _)| *id != self.id);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A stop an owner converted into an error at its boundary: the two a
/// worker's own switch can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerStop {
    Cancelled,
    TimedOut,
}

impl WorkerStop {
    pub fn of(cause: CancelCause) -> Option<Self> {
        match cause {
            CancelCause::Cancelled => Some(Self::Cancelled),
            CancelCause::TimedOut => Some(Self::TimedOut),
            _ => None,
        }
    }

    pub fn cause(self) -> CancelCause {
        match self {
            Self::Cancelled => CancelCause::Cancelled,
            Self::TimedOut => CancelCause::TimedOut,
        }
    }

    /// `worker was cancelled` (143), `worker timed out` (124).
    pub fn message(self) -> &'static str {
        match self {
            Self::Cancelled => "worker was cancelled",
            Self::TimedOut => "worker timed out",
        }
    }
}

/// A scope node's identity: the name a stop carries for its owner.
#[derive(Clone)]
pub struct ScopeId(Arc<Vertex>);

impl PartialEq for ScopeId {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for ScopeId {}

impl std::fmt::Debug for ScopeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ScopeId({:p})", Arc::as_ptr(&self.0))
    }
}

/// Cancellation in force on a scope, named for the node that owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cancellation {
    pub cause: CancelCause,
    pub(crate) owner: ScopeId,
}

/// Every sliced wait's period: a blocked wait re-reads its scope this often.
pub(crate) const POLL_SLICE: Duration = Duration::from_millis(10);

#[derive(Debug)]
struct Vertex {
    flag: AtomicU8,
    /// The instant the flag first rose, set in the act that raised it: a
    /// shield's budget runs from the earliest of these on its chain.
    cancelled_at: OnceLock<Instant>,
    parent: Option<Arc<Self>>,
    /// `Some(grace)` on a cleanup's node: masked for `grace` from the strike.
    shield: Option<Duration>,
}

/// What a chain folds to: the join, the outermost node attaining it, and the
/// earliest strike on the chain.
struct InForce<'a> {
    cause: u8,
    node: &'a Arc<Vertex>,
    since: Instant,
}

/// A shielded node admits from above only `Aborted`, and everything once
/// `since + grace` has passed.  `Instant::now()` is read only at a shielded
/// node with something above it.  Root-first over the chain, iteratively:
/// the chain is as deep as the nested cleanups, which only the recursion
/// limit bounds.
fn resolve(scope: &Arc<Vertex>) -> Option<InForce<'_>> {
    let chain: Vec<&Arc<Vertex>> =
        std::iter::successors(Some(scope), |n| n.parent.as_ref()).collect();
    chain.iter().rev().fold(None, |above, &node| {
        let above = above.filter(|j: &InForce<'_>| {
            node.shield.is_none_or(|grace| {
                j.cause >= CancelCause::Aborted as u8 || Instant::now() >= j.since + grace
            })
        });
        let own = node.flag.load(Ordering::Acquire);
        // `cancelled_at` is set before the flag's release store, so a nonzero flag
        // always has its instant.
        let at = (own != 0).then(|| {
            *node
                .cancelled_at
                .get()
                .expect("a struck node has its instant")
        });
        match (above, at) {
            (Some(above), Some(at)) if above.cause >= own => Some(InForce {
                since: above.since.min(at),
                ..above
            }),
            (Some(above), Some(at)) => Some(InForce {
                cause: own,
                node,
                since: above.since.min(at),
            }),
            (Some(above), None) => Some(above),
            (None, Some(at)) => Some(InForce {
                cause: own,
                node,
                since: at,
            }),
            (None, None) => None,
        }
    })
}

fn mint(parent: Option<&Arc<Vertex>>, shield: Option<Duration>) -> Arc<Vertex> {
    Arc::new(Vertex {
        flag: AtomicU8::new(0),
        cancelled_at: OnceLock::new(),
        parent: parent.cloned(),
        shield,
    })
}

/// A view of a scope node: its chain's join, and nothing to write it with.
///
/// Cheap to clone (one `Arc` bump), cheap to check (a chain of atomic loads).
/// Cancellation is one-way and monotone in the [`CancelCause`] order: a
/// cancelled scope stays cancelled and its recorded cause only rises.
#[derive(Debug, Clone)]
pub struct Scope(Arc<Vertex>);

impl Scope {
    /// A node nested under this scope; writing it touches only itself.
    pub fn child(&self) -> ScopeNode {
        ScopeNode(Self(mint(Some(&self.0), None)))
    }

    /// A cleanup's scope: masked against every ancestor cause below `Aborted`
    /// for `grace` from the chain's strike, or for the nearest enclosing
    /// shield's grace if that is shorter.  A view only, so nothing writes it.
    pub(crate) fn shielded(&self, grace: Duration) -> Self {
        let enclosing =
            std::iter::successors(Some(&self.0), |n| n.parent.as_ref()).find_map(|n| n.shield);
        Self(mint(
            Some(&self.0),
            Some(enclosing.map_or(grace, |e| e.min(grace))),
        ))
    }

    /// True if anything in this scope's join is in force.
    pub fn is_cancelled(&self) -> bool {
        self.in_force().is_some()
    }

    /// The strongest [`CancelCause`] in this scope's join, or `None`.
    pub fn cause(&self) -> Option<CancelCause> {
        self.in_force().map(|(cause, _)| cause)
    }

    /// Observe the cancellation in force, named for its owner.
    ///
    /// # Errors
    /// The strongest cause and the outermost node attaining it.
    pub fn poll(&self) -> Result<(), Cancellation> {
        self.in_force()
            .map_or(Ok(()), |(cause, owner)| Err(Cancellation { cause, owner }))
    }

    /// The strongest cause in force and the node that attained it.
    pub(crate) fn in_force(&self) -> Option<(CancelCause, ScopeId)> {
        resolve(&self.0).map(|j| {
            (
                CancelCause::from_repr(j.cause).expect("a nonzero flag is a cause"),
                ScopeId(Arc::clone(j.node)),
            )
        })
    }

    pub fn id(&self) -> ScopeId {
        ScopeId(Arc::clone(&self.0))
    }
}

/// A scope node with its write: what its one writer holds.
#[derive(Debug, Clone)]
pub struct ScopeNode(Scope);

impl std::ops::Deref for ScopeNode {
    type Target = Scope;
    fn deref(&self) -> &Scope {
        &self.0
    }
}

impl ScopeNode {
    /// The one write: the instant first, then the flag, so a reader that
    /// sees the flag sees the instant.
    pub fn cancel(&self, cause: CancelCause) {
        let v = &self.0.0;
        v.cancelled_at.get_or_init(Instant::now);
        v.flag.fetch_max(cause as u8, Ordering::Release);
    }
}

/// The session's durable cancel root, and the only free root, so every node
/// descends from one.  Its own node is never handed out, so it is never a
/// run's home.
///
/// A loose worker born in a run hangs under it, beside the anchor a run's
/// frame nests under: a foreground cancel never reaches one; a cancel on the
/// root, or on the worker's own node, does.
#[derive(Debug, Clone)]
pub struct DurableRoot(ScopeNode);

impl DurableRoot {
    /// Mint a fresh session root.
    pub(crate) fn new() -> Self {
        Self(ScopeNode(Scope(mint(None, None))))
    }

    /// A node under this root that is *not* a run's frame: a worker's, and
    /// the anchor a session boots with.
    pub fn worker(&self) -> ScopeNode {
        self.child()
    }

    /// Record `cause` on the root, reaching the foreground run and every
    /// worker at once.
    pub fn cancel(&self, cause: CancelCause) {
        self.0.cancel(cause);
    }
}

impl std::ops::Deref for DurableRoot {
    type Target = Scope;
    fn deref(&self) -> &Scope {
        &self.0
    }
}

impl Default for DurableRoot {
    fn default() -> Self {
        Self::new()
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
        crate::sync::LockExt::lock_ignore_poison(&self.0)
    }
}

/// Serializes every test in the ral-core binary that touches the ambient causes
/// — by raising one, or by listening for them: a raise beside a concurrent
/// listener would reach it.
#[cfg(test)]
pub(crate) static REQUEST_SERIAL: Serial = Serial::new();

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    /// The escalation order being total is what makes `fetch_max` on the flag
    /// byte mean "never downgrade".
    #[test]
    fn cause_encoding_roundtrips_and_orders() {
        let causes = [
            CancelCause::ReaderGone,
            CancelCause::Interrupted,
            CancelCause::Cancelled,
            CancelCause::TimedOut,
            CancelCause::Terminated,
            CancelCause::Aborted,
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
                CancelCause::from_repr(cause as u8),
                Some(cause),
                "{cause:?} must round-trip through its flag encoding"
            );
        }
        assert_eq!(CancelCause::from_repr(0), None, "0 means uncancelled");
    }

    #[test]
    fn parent_cancel_reaches_child() {
        let parent = DurableRoot::default();
        let child = parent.child();
        assert!(!child.is_cancelled(), "a fresh child starts uncancelled");
        parent.cancel(CancelCause::Interrupted);
        assert!(
            child.is_cancelled(),
            "cancelling the parent must cancel the child"
        );
    }

    #[test]
    fn child_cancel_isolates_from_parent_and_siblings() {
        let parent = DurableRoot::default();
        let one = parent.child();
        let two = parent.child();
        one.cancel(CancelCause::Interrupted);
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
        outer.cancel(CancelCause::Interrupted);
        let inner = outer.child();
        assert_eq!(
            inner.cause(),
            Some(CancelCause::Interrupted),
            "the nested run observes the interrupt through the frame it nests in"
        );
        assert_eq!(
            outer.cause(),
            Some(CancelCause::Interrupted),
            "and so does the run it nests in: sharing, not shadowing"
        );
    }

    /// Nesting each entry under the frame it displaces is what makes a scope's
    /// ancestors the runs enclosing it.
    #[test]
    fn an_outer_frames_deadline_reaches_the_nest() {
        let outer = DurableRoot::new().worker().child();
        let inner = outer.child();
        outer.cancel(CancelCause::TimedOut);
        assert_eq!(
            inner.cause(),
            Some(CancelCause::TimedOut),
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
        frame.cancel(CancelCause::Interrupted);
        assert_eq!(
            worker.cause(),
            None,
            "a run's interrupt must not reach a worker"
        );
        root.cancel(CancelCause::Terminated);
        assert_eq!(
            worker.cause(),
            Some(CancelCause::Terminated),
            "but a shutdown reaches it through its root"
        );
    }

    fn pair() -> (DurableRoot, ScopeNode) {
        let parent = DurableRoot::default();
        let node = parent.child();
        (parent, node)
    }

    #[test]
    fn a_fresh_scope_polls_clear() {
        assert_eq!(DurableRoot::default().poll(), Ok(()));
    }

    #[test]
    fn a_poll_names_the_outermost_node_attaining_the_join() {
        let (parent, child) = pair();
        parent.cancel(CancelCause::Interrupted);
        child.cancel(CancelCause::Interrupted);
        assert_eq!(
            child.poll(),
            Err(Cancellation {
                cause: CancelCause::Interrupted,
                owner: parent.id(),
            })
        );

        let (parent, child) = pair();
        parent.cancel(CancelCause::Cancelled);
        child.cancel(CancelCause::Interrupted);
        assert_eq!(
            child.poll(),
            Err(Cancellation {
                cause: CancelCause::Cancelled,
                owner: parent.id(),
            })
        );

        let (parent, child) = pair();
        parent.cancel(CancelCause::Interrupted);
        child.cancel(CancelCause::Cancelled);
        assert_eq!(
            child.poll(),
            Err(Cancellation {
                cause: CancelCause::Cancelled,
                owner: child.id(),
            })
        );
    }

    #[test]
    fn a_shield_masks_its_ancestors_within_the_grace() {
        let (parent, node) = pair();
        let s = node.shielded(Duration::from_secs(1));
        node.cancel(CancelCause::Cancelled);
        parent.cancel(CancelCause::Interrupted);
        assert_eq!(s.cause(), None);
        assert_eq!(node.cause(), Some(CancelCause::Cancelled));
    }

    #[test]
    fn a_shield_never_masks_a_descendants_own_flag() {
        let (parent, node) = pair();
        let s = node.shielded(Duration::from_secs(1));
        let inner = s.child();
        parent.cancel(CancelCause::Interrupted);
        inner.cancel(CancelCause::ReaderGone);
        assert_eq!(
            inner.in_force(),
            Some((CancelCause::ReaderGone, inner.id()))
        );
    }

    #[test]
    fn an_abort_passes_a_shield() {
        let (parent, node) = pair();
        let s = node.shielded(Duration::from_secs(1));
        parent.cancel(CancelCause::Aborted);
        assert_eq!(s.cause(), Some(CancelCause::Aborted));
    }

    #[test]
    fn a_spent_shield_masks_nothing() {
        let (parent, node) = pair();
        let s = node.shielded(Duration::from_millis(200));
        parent.cancel(CancelCause::Interrupted);
        assert_eq!(s.cause(), None);
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(s.cause(), Some(CancelCause::Interrupted));
    }

    #[test]
    fn a_grace_counts_from_the_strike_not_the_shield() {
        // Sleeps only overshoot, so only the unexpired check needs slack.
        let (parent, node) = pair();
        parent.cancel(CancelCause::Interrupted);
        std::thread::sleep(Duration::from_millis(100));
        let s = node.shielded(Duration::from_millis(60));
        assert_eq!(s.cause(), Some(CancelCause::Interrupted));
        let t = node.shielded(Duration::from_secs(2));
        assert_eq!(t.cause(), None);
        std::thread::sleep(Duration::from_secs(2));
        assert_eq!(t.cause(), Some(CancelCause::Interrupted));
    }

    #[test]
    fn successive_shields_share_the_strike() {
        let (parent, node) = pair();
        parent.cancel(CancelCause::Interrupted);
        let s1 = node.shielded(Duration::from_millis(400));
        std::thread::sleep(Duration::from_millis(300));
        drop(s1);
        let s2 = node.shielded(Duration::from_millis(400));
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(s2.cause(), Some(CancelCause::Interrupted));
    }

    #[test]
    fn a_nested_shield_ends_with_its_enclosing_one() {
        let (parent, node) = pair();
        let outer = node.shielded(Duration::from_millis(200));
        let inner = outer.shielded(Duration::from_secs(10));
        parent.cancel(CancelCause::Interrupted);
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(inner.cause(), Some(CancelCause::Interrupted));
    }

    #[test]
    fn a_stronger_cause_keeps_the_first_strike() {
        let (parent, node) = pair();
        parent.cancel(CancelCause::Interrupted);
        std::thread::sleep(Duration::from_millis(300));
        parent.cancel(CancelCause::Cancelled);
        let s = node.shielded(Duration::from_millis(400));
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(s.cause(), Some(CancelCause::Cancelled));
    }

    fn deliver_to_forwarder(guard: &AmbientForward, causes: &[Ambient]) {
        let listeners = LISTENERS.lock_ignore_poison();
        let (_, listener) = listeners
            .iter()
            .find(|(id, _)| *id == guard.id)
            .expect("the test forwarder is registered");
        for cause in causes {
            listener
                .tx
                .send(*cause)
                .expect("deliver to the test listener");
        }
        drop(listeners);
    }

    fn wait_for_forwarder_unregistration(id: u64) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if LISTENERS
                .try_lock()
                .is_ok_and(|listeners| listeners.iter().all(|(registered, _)| *registered != id))
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "drop must unregister and release LISTENERS before waiting for the callback"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn dropping_an_idle_ambient_forwarder_releases_its_capture() {
        let _serial = REQUEST_SERIAL.lock();
        let capture = Arc::new(());
        let captured = Arc::clone(&capture);
        let guard = forward_ambient(move |_| {
            assert_eq!(Arc::strong_count(&captured), 2);
        });

        drop(guard);

        assert!(
            Arc::into_inner(capture).is_some(),
            "guard destruction must release the callback's capture before returning"
        );
    }

    fn drop_blocked_ambient_forwarder(during_drop: impl Fn() + Send + 'static) {
        let _serial = REQUEST_SERIAL.lock();
        let timeout = Duration::from_secs(5);
        let capture = Arc::new(());
        let captured = Arc::clone(&capture);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (forwarded_tx, forwarded_rx) = std::sync::mpsc::channel();
        let guard = forward_ambient(move |ambient| {
            assert_eq!(Arc::strong_count(&captured), 2);
            if ambient == Ambient::Interrupt {
                entered_tx.send(()).expect("announce callback entry");
                release_rx
                    .recv_timeout(timeout)
                    .expect("the test releases the blocked callback");
                during_drop();
            }
            forwarded_tx.send(ambient).expect("record queued delivery");
        });
        let causes = [Ambient::Interrupt, Ambient::Root(CancelCause::Terminated)];
        deliver_to_forwarder(&guard, &causes);
        entered_rx.recv_timeout(timeout).expect("callback entered");

        let id = guard.id;
        let (dropped_tx, dropped_rx) = std::sync::mpsc::channel();
        let dropper = std::thread::spawn(move || {
            drop(guard);
            dropped_tx.send(()).expect("announce completed destruction");
        });
        wait_for_forwarder_unregistration(id);
        let pending = dropped_rx.recv_timeout(Duration::from_millis(100));
        release_tx.send(()).expect("release the callback");
        if pending == Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
            dropped_rx
                .recv_timeout(timeout)
                .expect("destruction completes after the callback is released");
        }
        dropper.join().expect("guard destruction must not panic");

        assert_eq!(
            pending,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout),
            "destruction must wait for the blocked callback"
        );
        assert!(
            Arc::into_inner(capture).is_some(),
            "destruction must release the callback's capture"
        );
        assert_eq!(
            forwarded_rx.try_iter().collect::<Vec<_>>(),
            causes,
            "destruction must drain queued causes in order"
        );
    }

    #[test]
    fn dropping_an_ambient_forwarder_waits_for_its_blocked_callback() {
        drop_blocked_ambient_forwarder(|| {});
    }

    #[test]
    fn dropping_an_ambient_forwarder_releases_the_registry_before_joining() {
        drop_blocked_ambient_forwarder(|| {
            let nested = forward_ambient(|_| {});
            drop(nested);
        });
    }

    #[test]
    fn dropping_a_panicked_ambient_forwarder_releases_its_capture_without_panicking() {
        let _serial = REQUEST_SERIAL.lock();
        let capture = Arc::new(());
        let captured = Arc::clone(&capture);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let guard = forward_ambient(move |_| {
            assert_eq!(Arc::strong_count(&captured), 2);
            entered_tx.send(()).expect("announce callback entry");
            panic!("test callback panic");
        });
        deliver_to_forwarder(&guard, &[Ambient::Interrupt]);
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the panicking callback entered");

        let dropped = std::thread::spawn(move || drop(guard)).join();

        assert!(dropped.is_ok(), "callback panic must not escape guard drop");
        assert!(
            Arc::into_inner(capture).is_some(),
            "guard destruction must release the panicked callback's capture"
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
        request_root_cancel(CancelCause::Terminated);
        assert_eq!(
            first.recv_timeout(owed),
            Ok(Ambient::Root(CancelCause::Terminated)),
            "a shutdown request is forwarded"
        );

        let (_second, second) = listen();
        let standing = second.recv_timeout(owed);
        clear_root_request();
        assert_eq!(
            standing,
            Ok(Ambient::Root(CancelCause::Terminated)),
            "a shutdown request already standing is forwarded at once"
        );
        assert!(
            second.recv_timeout(unowed).is_err(),
            "and nothing else: the older interrupts were not its to hear"
        );
    }
}
