//! The worker registry: a per-[`Shell`](super::Shell) directory of every
//! worker (`spawn`, `watch`, and `par`/`all` children) spawned from it. Pure
//! bookkeeping, never a policy itself.
//!
//! `spawn_child` in `builtins::concurrency` files an entry as it mints the
//! handle. The entry is gone once the worker's result is *claimed*, which
//! happens once: by `await`, by `race` (its winner and its cancelled losers),
//! by `cancel`, or by its owner's join or halt. Every locked read drops
//! claimed entries first, so a claim made from any shell removes the worker
//! here. `poll` claims nothing, on either arm.
//! One policy also removes entries: [`WorkerRegistry::sweep_retention`]. It
//! leaves a [`ReapNotice`] the host drains at its ready boundaries, so a
//! vanished job still has an answer in the transcript.
//!
//! **Flow rule.** [`Shell::spawn_thread`](super::Shell::spawn_thread)
//! `Arc`-shares the registry into a worker's own `Shell`, so a nested `spawn`
//! registers into the owning shell's directory; the other children of
//! [`Shell::child`](super::Shell::child) do not, and a sub-agent fork starts
//! empty.

use crate::fact::WorkerId;
use crate::first_order::FOValue;
use crate::first_order::datum::{Datum, tag, untag};
use crate::process::CancelCause;
use crate::sync::LockExt as _;
use crate::types::{Break, ErrorRecord, Escape, HandleInner, Shell, ThreadEnd};
use crate::{label, record, variant};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use strum::{IntoStaticStr, VariantArray};

/// How long a wipe waits for cancelled workers to end: the cleanup grace a
/// host grants by default (`Run::foreground`'s five seconds) plus the
/// teardown grace, so a shielded cleanup's child is not orphaned by the
/// session's end.  A host granting more accepts that a wipe may cut it.
const WORKER_DRAIN_GRACE: Duration = Duration::from_millis(5_500);

impl WorkerId {
    pub(crate) fn mint() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// Why policy removed a worker's entry: the one policy left.
#[derive(Copy, Clone, Debug, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum ReapCause {
    /// A settled entry whose unclaimed result outlived the retention bound.
    Retention,
}

label!(ReapCause);

/// Who settles a worker: the session, the worker it was born inside, or the
/// calling expression that birthed it (`par`, `all`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Session,
    Worker(WorkerId),
    Call,
}

variant!(typed Owner {
    Session: "session",
    Worker(WorkerId): "worker",
    Call: "call",
});

/// How a worker ended, as its `` `done `` event says: no return
/// value, which only `await` hands over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Done {
    Ok,
    Err(ErrorRecord),
    Exited(i32),
    Stopped(CancelCause),
    Panic(String),
}

variant!(typed Done {
    Ok: "ok",
    Err(ErrorRecord): "err",
    Exited(i32): "exited",
    Stopped(CancelCause): "stopped",
    Panic(String): "panic",
});

impl Done {
    pub(crate) fn of(end: &ThreadEnd, shell: &Shell) -> Self {
        match end {
            ThreadEnd::Settled(Ok(_)) => Self::Ok,
            ThreadEnd::Settled(Err(Break::Error(e))) => Self::Err(e.record(shell)),
            ThreadEnd::Settled(Err(Break::Escape(Escape::Exit(n)))) => Self::Exited(*n),
            ThreadEnd::Settled(Err(Break::Escape(Escape::Stopped { cause, .. }))) => {
                Self::Stopped(*cause)
            }
            ThreadEnd::Panicked(text) => Self::Panic(text.clone()),
        }
    }
}

/// The event a worker appends at completion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoneEvent {
    pub id: WorkerId,
    pub title: String,
    pub elapsed_ms: u64,
    pub outcome: Done,
}

record!(DoneEvent {
    id: "id",
    title: "title",
    elapsed_ms: "elapsed-ms",
    outcome: "outcome",
});

impl DoneEvent {
    /// The tag a completion carries on the surface channel.
    pub const SURFACE_TAG: &str = "done";

    pub fn to_surface(self) -> FOValue {
        tag(Self::SURFACE_TAG, Some(self.encode()))
    }

    /// Inverse of [`Self::to_surface`]; `None` for any other surface event.
    pub fn from_surface(v: &FOValue) -> Option<Self> {
        match untag(v)? {
            (Self::SURFACE_TAG, Some(body)) => Self::decode(body).ok(),
            _ => None,
        }
    }
}

/// One line of a watched stream, as it crosses the surface channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchEvent {
    pub label: String,
    pub line: String,
}

record!(WatchEvent {
    label: "label",
    line: "line",
});

impl WatchEvent {
    /// The tag a watched line carries on the surface channel.
    pub const SURFACE_TAG: &str = "watch";

    pub fn to_surface(self) -> FOValue {
        tag(Self::SURFACE_TAG, Some(self.encode()))
    }

    /// Inverse of [`Self::to_surface`]; `None` for any other surface event.
    pub fn from_surface(v: &FOValue) -> Option<Self> {
        match untag(v)? {
            (Self::SURFACE_TAG, Some(body)) => Self::decode(body).ok(),
            _ => None,
        }
    }
}

/// What a reap leaves for the transcript event, minus the handle it
/// deliberately does not keep alive.
///
/// Recorded only for an entry present at reap time, so a worker a claimer
/// removed first leaves none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReapNotice {
    pub id: WorkerId,
    pub title: String,
    pub cause: ReapCause,
}

record!(ReapNotice {
    id: "id",
    title: "title",
    cause: "cause",
});

/// One registered worker, paired with the handle a caller observes or cancels
/// it through.
///
/// Storing the handle rather than a second by-id control plane keeps `poll`,
/// `await`, `race`, and `cancel` the only verbs that touch a worker:
/// rediscovery is `worker <id>`, which hands this handle back.
#[derive(Clone, Debug)]
pub struct WorkerEntry {
    pub owner: Owner,
    /// The ral-call epoch at which [`WorkerRegistry::sweep_retention`] first
    /// observed this entry settled — `None` while it runs, so retention starts
    /// at the next call rather than retroactively.
    pub settled_epoch: Option<u64>,
    pub handle: HandleInner,
}

/// The three ledgers behind [`WorkerRegistry`]'s one lock: live entries, reap
/// notices awaiting the host's drain, and seats held for a birth in flight.
/// One lock for all three is what makes a reap atomic — entry and notice move
/// in the same critical section — and admission honest.
#[derive(Default)]
struct RegistryInner {
    entries: Vec<WorkerEntry>,
    reap_notices: Vec<ReapNotice>,
    /// Seats held by a [`Reservation`] not yet filed or released, counted
    /// alongside running entries in [`WorkerRegistry::reserve`]'s measure.
    reserved: usize,
    /// One tick per source dispatch, the cadence the binding-lease ledger
    /// keeps.
    epoch: u64,
    /// The armed settled-entry retention, in ral calls. `None` — no host armed
    /// it, as in the REPL — retains settled entries indefinitely.
    retention: Option<u64>,
    /// One clone per live worker thread, each held by the thread itself for its
    /// whole life; the strong count is therefore the session's live-thread
    /// census and the only thing [`WorkerRegistry::drain`] can honestly wait on.
    /// The roster cannot serve: `cancel` takes an entry out the instant it
    /// signals, while the thread it signalled is still tearing its child down.
    live: Arc<()>,
}

/// Cheap-clonable, per-[`Shell`](super::Shell) directory of every worker
/// spawned from it.
///
/// A newtype over `Arc<Mutex<RegistryInner>>`: cloning shares the store, which
/// is how the flow rule above lets a nested `spawn` register into its owning
/// shell's registry. Every operation locks, acts, and
/// unlocks — none ever calls out while holding the lock, [`Self::reserve`] and
/// [`Self::register`] included: the [`Reservation`] bridging those two locked
/// steps holds its seat from the instant admission is granted.
#[derive(Clone, Default)]
pub(crate) struct WorkerRegistry(Arc<Mutex<RegistryInner>>);

/// A shell's registry, and whether the shell answers for it: dropping an
/// `Owned` roster cancels every worker in it, so every teardown path (an
/// agent's end, a `/clear`'s replacement, a wire session's detach, a batch
/// script's last line) reaps its workers, external children and their
/// grandchildren included, without a host call site.  A worker's own shell
/// holds a `Shared` one: its dropping must not cancel its parent's roster.
pub(crate) enum Roster {
    Owned(WorkerRegistry),
    Shared(WorkerRegistry),
}

impl Roster {
    pub(crate) fn share(&self) -> Self {
        Self::Shared(WorkerRegistry::clone(self))
    }

    /// A registry of its own, carrying this one's retention.
    pub(crate) fn fresh(&self) -> Self {
        let registry = WorkerRegistry::default();
        if let Some(retention) = self.retention() {
            registry.arm_retention(retention);
        }
        Self::Owned(registry)
    }
}

impl Default for Roster {
    fn default() -> Self {
        Self::Owned(WorkerRegistry::default())
    }
}

impl Deref for Roster {
    type Target = WorkerRegistry;

    fn deref(&self) -> &WorkerRegistry {
        let (Self::Owned(registry) | Self::Shared(registry)) = self;
        registry
    }
}

impl Drop for Roster {
    fn drop(&mut self) {
        if let Self::Owned(registry) = self {
            registry.cancel_all();
        }
    }
}

/// Refusal from [`WorkerRegistry::reserve`], carrying the cap it was refused
/// against so the caller's remedy message need not re-derive the number.
pub(crate) struct CapReached(pub(crate) usize);

/// A seat held between [`WorkerRegistry::reserve`] granting admission and
/// [`WorkerRegistry::register`] filing the entry, across the thread spawn and
/// handle construction in between. Consuming one is `register`'s only way to
/// accept a [`WorkerEntry`]; dropping one unconsumed releases its seat, and
/// `armed` is the defusal that makes the release happen exactly once.
pub(crate) struct Reservation {
    registry: WorkerRegistry,
    armed: bool,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.armed {
            let mut inner = self.registry.0.lock_ignore_poison();
            inner.reserved = inner.reserved.saturating_sub(1);
        }
    }
}

impl WorkerRegistry {
    /// Measure admission and hold a seat in one locked step, so a birth that
    /// registers only later — after a thread spawn and a handle construction —
    /// cannot be raced by a sibling reading the same free seat. The measure is
    /// running entries (each handle briefly locked under the registry lock,
    /// the order [`HandleInner`] documents) plus seats reserved.
    pub(crate) fn reserve(&self, cap: Option<usize>) -> Result<Reservation, CapReached> {
        let mut inner = self.0.lock_ignore_poison();
        if let Some(cap) = cap {
            let running = inner
                .entries
                .iter()
                .filter(|entry| entry.handle.is_running())
                .count();
            if running + inner.reserved >= cap {
                return Err(CapReached(cap));
            }
        }
        inner.reserved += 1;
        drop(inner);
        Ok(Reservation {
            registry: self.clone(),
            armed: true,
        })
    }

    /// File a freshly-spawned worker, consuming its [`Reservation`]. One
    /// locked step, so nothing ever observes an entry whose seat is still
    /// counted reserved, or a reservation whose entry has already appeared.
    pub(crate) fn register(&self, mut reservation: Reservation, entry: WorkerEntry) {
        reservation.armed = false;
        let mut inner = self.0.lock_ignore_poison();
        inner.reserved = inner.reserved.saturating_sub(1);
        inner.entries.push(entry);
    }

    /// The registry locked, every claimed entry gone: a claim made anywhere
    /// removes the worker here, under this lock.
    fn unclaimed(&self) -> MutexGuard<'_, RegistryInner> {
        let mut inner = self.0.lock_ignore_poison();
        inner.entries.retain(|entry| !entry.handle.is_claimed());
        inner
    }

    pub(crate) fn retention(&self) -> Option<u64> {
        self.0.lock_ignore_poison().retention
    }

    /// Arm the settled-entry retention bound, in ral calls. Idempotent by
    /// replacement: already-stamped entries are measured against the new one.
    pub(crate) fn arm_retention(&self, retention: u64) {
        self.0.lock_ignore_poison().retention = Some(retention);
    }

    /// Ticked at the run door's Source arm, beside the binding ledger's tick,
    /// so the two ledgers read one logical clock.
    pub(crate) fn tick_epoch(&self) {
        self.0.lock_ignore_poison().epoch += 1;
    }

    /// Expire settled entries against the armed retention; a no-op unarmed. An
    /// entry is stamped the first sweep that finds it settled and removed with
    /// a [`ReapCause::Retention`] notice `retention` calls later, so retention
    /// never runs retroactively over a quiet period. A claimed entry is gone
    /// under the same lock, so it is never reaped; this catches the rest,
    /// polled or not.
    /// A settled child of a running worker, or of a call, is left alone: its
    /// owner joins it.
    pub(crate) fn sweep_retention(&self) {
        let mut inner = self.unclaimed();
        let Some(retention) = inner.retention else {
            return;
        };
        let epoch = inner.epoch;
        let running: Vec<WorkerId> = inner
            .entries
            .iter()
            .filter(|entry| entry.handle.is_running())
            .map(|entry| entry.handle.id)
            .collect();
        let mut i = 0;
        while i < inner.entries.len() {
            let entry = &inner.entries[i];
            let held = running.contains(&entry.handle.id)
                || entry.owner == Owner::Call
                || matches!(entry.owner, Owner::Worker(parent) if running.contains(&parent));
            match (held, entry.settled_epoch) {
                (false, None) => {
                    inner.entries[i].settled_epoch = Some(epoch);
                    i += 1;
                }
                (false, Some(s)) if epoch.saturating_sub(s) >= retention => {
                    let entry = inner.entries.remove(i);
                    inner.reap_notices.push(ReapNotice {
                        id: entry.handle.id,
                        title: entry.handle.title,
                        cause: ReapCause::Retention,
                    });
                }
                _ => i += 1,
            }
        }
        drop(inner);
    }

    /// Ral calls until a settled `entry`'s retention expires: the whole bound
    /// for one not yet stamped, `None` while it runs or with none armed.
    pub(crate) fn retention_left(&self, entry: &WorkerEntry) -> Option<u64> {
        if entry.handle.is_running() {
            return None;
        }
        let (epoch, retention) = {
            let inner = self.0.lock_ignore_poison();
            (inner.epoch, inner.retention?)
        };
        Some(entry.settled_epoch.map_or(retention, |stamped| {
            retention.saturating_sub(epoch.saturating_sub(stamped))
        }))
    }

    pub(crate) fn take_reap_notices(&self) -> Vec<ReapNotice> {
        std::mem::take(&mut self.0.lock_ignore_poison().reap_notices)
    }

    /// Clone out every entry for listing; a pure read.
    pub(crate) fn snapshot(&self) -> Vec<WorkerEntry> {
        self.unclaimed().entries.clone()
    }

    /// Clone out the entry named by `id`; a pure read like [`Self::snapshot`].
    pub(crate) fn lookup(&self, id: WorkerId) -> Option<WorkerEntry> {
        self.unclaimed()
            .entries
            .iter()
            .find(|entry| entry.handle.id == id)
            .cloned()
    }

    pub(crate) fn count(&self) -> usize {
        self.unclaimed().entries.len()
    }

    /// A live-thread ticket, held by the worker thread's own frame for as long
    /// as it runs. [`Shell::spawn_thread`](super::Shell::spawn_thread) takes one
    /// per worker, so no spawn site can forget to.
    pub(crate) fn live_ticket(&self) -> Arc<()> {
        self.0.lock_ignore_poison().live.clone()
    }

    /// Wait, up to [`WORKER_DRAIN_GRACE`], for every live worker thread to end.
    /// A cancel lands at the worker's next observation point, and a host that
    /// exits in the same breath outruns it — orphaning the child under PID 1.
    fn drain(&self) {
        let deadline = std::time::Instant::now() + WORKER_DRAIN_GRACE;
        while std::time::Instant::now() < deadline {
            if Arc::strong_count(&self.0.lock_ignore_poison().live) == 1 {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Destruction — `/clear`'s arm and the session teardown's: cancel every
    /// entry's scope, reset the roster, pending notices included, then wait for
    /// the cancels to land. The cancels fire only after the guard drops.
    ///
    /// An empty roster still drains: a worker `cancel`led moments ago left the
    /// roster when it was signalled, not when its child died.
    pub(crate) fn cancel_all(&self) -> usize {
        let (entries, _notices) = {
            let mut inner = self.unclaimed();
            // The armed retention and the epoch clock are configuration, not
            // roster, and survive the wipe.
            (
                std::mem::take(&mut inner.entries),
                std::mem::take(&mut inner.reap_notices),
            )
        };
        for entry in &entries {
            entry
                .handle
                .cancel
                .cancel(crate::process::CancelCause::Cancelled);
        }
        self.drain();
        entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── reservation (the admission/registration TOCTOU close) ───────────

    /// Eight threads race `reserve` against `cap = Some(2)`, each holding what
    /// it was granted for a moment, so over-admission shows up as overlapping
    /// reservations rather than successes that merely never overlapped.
    #[test]
    fn reserve_admits_at_most_cap_under_concurrent_racing() {
        let registry = WorkerRegistry::default();
        let barrier = Arc::new(std::sync::Barrier::new(8));

        #[allow(
            clippy::needless_collect,
            reason = "all 8 threads must be spawned before any is joined, to race"
        )]
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let registry = registry.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let reservation = registry.reserve(Some(2));
                    // No reservation drops before every thread has tried.
                    barrier.wait();
                    reservation.is_ok()
                })
            })
            .collect();

        let admitted = threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .filter(|&ok| ok)
            .count();
        assert_eq!(
            admitted, 2,
            "cap 2 must admit exactly 2 of the 8 racing reservations"
        );
    }

    /// The RAII path `spawn_child`'s early returns on the way to a handle rely
    /// on: a `Reservation` dropped before `register` frees its seat at once.
    #[test]
    fn dropping_an_unconsumed_reservation_frees_the_slot() {
        let registry = WorkerRegistry::default();
        let first = registry
            .reserve(Some(1))
            .unwrap_or_else(|_| panic!("the first reservation must be admitted"));
        assert!(
            registry.reserve(Some(1)).is_err(),
            "the seat is held: a second reservation must be refused"
        );
        drop(first);
        assert!(
            registry.reserve(Some(1)).is_ok(),
            "dropping the unconsumed reservation must free the slot"
        );
    }
}
