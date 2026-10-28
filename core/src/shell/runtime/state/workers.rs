//! The worker registry: a per-[`Shell`] directory of every
//! worker (`spawn` and `watch`, a `gather`'s children included) spawned from
//! it. Pure bookkeeping, never a policy itself.
//!
//! `spawn_child` in `builtins::concurrency` files an entry as it mints the
//! handle. An entry remains in the directory while it is `running`, `stopping`,
//! or `ready`, and is pruned once it reaches `consumed` (its result has been
//! claimed and its execution has ended). Directory reads prune consumed entries
//! eagerly, while uncapped admission compacts them lazily on an amortized
//! threshold.
//! One policy also removes `ready` entries: [`WorkerRegistry::sweep_retention`].
//! It leaves a [`ReapNotice`] the host drains at its ready boundaries, so a
//! vanished job still has an answer in the transcript.
//!
//! **Flow rule.** [`Shell::spawn_thread`](super::Shell::spawn_thread)
//! `Arc`-shares the registry into a worker's own `Shell`, so a nested `spawn`
//! registers into the owning shell's directory; the other children of
//! [`Shell::child`](super::Shell::child) do not, and a sub-agent fork starts
//! empty.

use crate::data::FOValue;
use crate::data::datum::{Datum, tag, untag};
use crate::fact::WorkerId;
use crate::process::CancelCause;
use crate::shell::runtime::handle::compacting::Compacting;
use crate::shell::runtime::{Break, ErrorRecord, Escape, HandleInner, Sample, Shell, ThreadEnd};
use crate::sync::LockExt as _;
use crate::{label, record, variant};
use std::collections::HashSet;
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
/// `gather` it was born inside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Session,
    Worker(WorkerId),
    Gather,
}

variant!(typed Owner {
    Session: "session",
    Worker(WorkerId): "worker",
    Gather: "gather",
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
/// `is-done`, `await`, `race`, and `cancel` the only verbs that touch a worker;
/// listing an entry returns data projections, never a reconstructed handle.
#[derive(Clone, Debug)]
pub struct WorkerEntry {
    pub owner: Owner,
    /// The ral-call epoch at which [`WorkerRegistry::sweep_retention`] first
    /// observed this entry `ready` — `None` while it is live, so retention
    /// starts at the next call rather than retroactively.
    pub settled_epoch: Option<u64>,
    pub handle: HandleInner,
}

impl AsRef<HandleInner> for WorkerEntry {
    fn as_ref(&self) -> &HandleInner {
        &self.handle
    }
}

/// The three ledgers behind [`WorkerRegistry`]'s one lock: retained entries,
/// reap notices awaiting the host's drain, and seats held for a birth in
/// flight. One lock for all three is what makes a reap atomic — entry and
/// notice move in the same critical section — and admission honest.
struct RegistryInner {
    entries: Compacting<WorkerEntry>,
    reap_notices: Vec<ReapNotice>,
    /// Seats held by a [`Reservation`] not yet filed or released, counted
    /// alongside live entries in [`WorkerRegistry::reserve`]'s measure.
    reserved: usize,
    /// One tick per source dispatch, the cadence the binding-lease ledger
    /// keeps.
    epoch: u64,
    /// The armed ready-entry retention, in ral calls. `None` — no host armed
    /// it, as in the REPL — retains ready entries indefinitely.
    retention: Option<u64>,
    /// One clone per live worker thread, each held by the thread itself for its
    /// whole life; the strong count is therefore the session's live-thread
    /// census and the only thing [`WorkerRegistry::drain`] can honestly wait on.
    live: Arc<()>,
}

impl Default for RegistryInner {
    fn default() -> Self {
        Self {
            entries: Compacting::default(),
            reap_notices: Vec::new(),
            reserved: 0,
            epoch: 0,
            retention: None,
            live: Arc::new(()),
        }
    }
}

/// Cheap-clonable, per-[`Shell`] directory of every worker
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CapReached(pub(crate) usize);

impl std::fmt::Display for CapReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} workers are still executing or stopping; wait for one to end (`workers` lists them), or cancel work you no longer need",
            self.0
        )
    }
}

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
    /// live (`running` or `stopping`) entries (each handle briefly locked under
    /// the registry lock, the order [`HandleInner`] documents) plus seats
    /// reserved.
    pub(crate) fn reserve(&self, cap: Option<usize>) -> Result<Reservation, CapReached> {
        let mut inner = self.0.lock_ignore_poison();
        if let Some(cap) = cap {
            let mut live = 0;
            inner
                .entries
                .prune_observing(|sample| live += usize::from(sample.is_live()));
            if live + inner.reserved >= cap {
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
        if !matches!(entry.handle.sample(), Sample::Consumed) {
            inner.entries.push(entry);
        }
    }

    /// The registry locked, every consumed entry gone: a completed claim made
    /// anywhere removes the worker here, under this lock.
    fn retained(&self) -> MutexGuard<'_, RegistryInner> {
        let mut inner = self.0.lock_ignore_poison();
        inner.entries.prune();
        inner
    }

    pub(crate) fn retention(&self) -> Option<u64> {
        self.0.lock_ignore_poison().retention
    }

    /// Ral calls until a `ready` entry stamped `settled_epoch` (see
    /// [`WorkerEntry::settled_epoch`]) expires: the whole bound for one not
    /// yet stamped, `None` with no retention armed.
    pub(crate) fn retention_left(&self, settled_epoch: Option<u64>) -> Option<u64> {
        let (epoch, retention) = {
            let inner = self.0.lock_ignore_poison();
            (inner.epoch, inner.retention?)
        };
        Some(settled_epoch.map_or(retention, |stamped| {
            retention.saturating_sub(epoch.saturating_sub(stamped))
        }))
    }

    /// Arm the ready-entry retention bound, in ral calls. Idempotent by
    /// replacement: already-stamped entries are measured against the new one.
    pub(crate) fn arm_retention(&self, retention: u64) {
        self.0.lock_ignore_poison().retention = Some(retention);
    }

    /// Ticked at the run door's Source arm, beside the binding ledger's tick,
    /// so the two ledgers read one logical clock.
    pub(crate) fn tick_epoch(&self) {
        self.0.lock_ignore_poison().epoch += 1;
    }

    /// Expire `ready` entries against the armed retention; a no-op unarmed. An
    /// entry is stamped the first sweep that finds it `ready` and removed with
    /// a [`ReapCause::Retention`] notice `retention` calls later, so retention
    /// never runs retroactively over a quiet period. A consumed entry is gone
    /// under the same lock, so it is never reaped; live (`running` or
    /// `stopping`) entries are never retention candidates.
    /// A ready child of a live (`running` or `stopping`) worker, or of a
    /// gather, is left alone: its owner joins or drains it.
    pub(crate) fn sweep_retention(&self) {
        let mut inner = self.retained();
        let Some(retention) = inner.retention else {
            return;
        };
        let epoch = inner.epoch;
        // Each handle sampled once, so every entry's verdict reads one moment.
        let (mut live, mut ready) = (HashSet::new(), HashSet::new());
        for entry in inner.entries.iter() {
            match entry.handle.sample() {
                Sample::Ready(()) => ready.insert(entry.handle.id),
                sample if sample.is_live() => live.insert(entry.handle.id),
                _ => false,
            };
        }
        let reaped: Vec<WorkerEntry> = inner
            .entries
            .extract_if(|entry| {
                let held = !ready.contains(&entry.handle.id)
                    || entry.owner == Owner::Gather
                    || matches!(entry.owner, Owner::Worker(parent) if live.contains(&parent));
                match entry.settled_epoch {
                    _ if held => false,
                    None => {
                        entry.settled_epoch = Some(epoch);
                        false
                    }
                    Some(stamped) => epoch.saturating_sub(stamped) >= retention,
                }
            })
            .collect();
        inner
            .reap_notices
            .extend(reaped.iter().map(|entry| ReapNotice {
                id: entry.handle.id,
                title: entry.handle.title.clone(),
                cause: ReapCause::Retention,
            }));
        drop(inner);
        drop(reaped);
    }

    pub(crate) fn take_reap_notices(&self) -> Vec<ReapNotice> {
        std::mem::take(&mut self.0.lock_ignore_poison().reap_notices)
    }

    /// Clone out every retained entry for listing, sorted by [`WorkerId`]; a
    /// pure read.
    pub(crate) fn snapshot(&self) -> Vec<WorkerEntry> {
        let mut entries = self.retained().entries.to_vec();
        entries.sort_by_key(|entry| entry.handle.id.0);
        entries
    }

    pub(crate) fn count(&self) -> usize {
        self.retained().entries.len()
    }

    #[cfg(test)]
    pub(crate) fn raw_entry_count(&self) -> usize {
        self.0.lock_ignore_poison().entries.len()
    }

    #[cfg(test)]
    pub(crate) fn compaction_visits(&self) -> usize {
        self.0.lock_ignore_poison().entries.visits()
    }

    /// A live-thread ticket, held by the worker thread's own frame for as long
    /// as it runs. [`Shell::spawn_thread`](super::Shell::spawn_thread) takes one
    /// per worker, so no spawn site can forget to.
    pub(crate) fn live_ticket(&self) -> Arc<()> {
        self.0.lock_ignore_poison().live.clone()
    }

    pub(crate) fn is_drained(&self) -> bool {
        Arc::strong_count(&self.0.lock_ignore_poison().live) == 1
    }

    /// Wait, up to [`WORKER_DRAIN_GRACE`], for every live worker thread to end.
    /// A cancel lands at the worker's next observation point, and a host that
    /// exits in the same breath outruns it — orphaning the child under PID 1.
    fn drain(&self) {
        let deadline = std::time::Instant::now() + WORKER_DRAIN_GRACE;
        while std::time::Instant::now() < deadline {
            if self.is_drained() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Destruction — `/clear`'s arm and the session teardown's: claim and stop
    /// every entry, reset the roster, pending notices included, then wait for
    /// live worker threads to end. The cancels fire only after the guard drops.
    pub(crate) fn cancel_all(&self) -> usize {
        let entries = {
            let mut inner = self.0.lock_ignore_poison();
            // The armed retention and the epoch clock are configuration, not
            // roster, and survive the wipe.
            inner.entries.prune();
            inner.reap_notices.clear();
            std::mem::take(&mut inner.entries).into_vec()
        };
        for entry in &entries {
            entry.handle.claim_and_stop();
        }
        self.drain();
        entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::ByteBuffer;
    use crate::process::DurableRoot;
    use crate::shell::runtime::Value;
    use crate::shell::runtime::handle::compacting::MIN_SWEEP_AT;

    fn test_handle(id: u64, title: &str) -> (HandleInner, impl FnOnce()) {
        let (handle, permit) = HandleInner::new(
            WorkerId(id),
            title,
            DurableRoot::default().worker(),
            ByteBuffer::default(),
            ByteBuffer::default(),
        );
        (handle, move || {
            let _ = permit.finish(ThreadEnd::Settled(Ok(Value::Unit)));
        })
    }

    fn ready_handle(id: u64, title: &str) -> HandleInner {
        let (handle, finish) = test_handle(id, title);
        finish();
        handle
    }

    fn consumed_handle(id: u64, title: &str) -> HandleInner {
        let handle = ready_handle(id, title);
        let _ = handle.claim_ready();
        handle
    }

    fn entry_of(owner: Owner, handle: HandleInner) -> WorkerEntry {
        WorkerEntry {
            owner,
            settled_epoch: None,
            handle,
        }
    }

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

    /// N uncapped live births followed by registration perform amortized O(N)
    /// compaction visits, not two full scans per birth.
    #[test]
    fn uncapped_live_births_have_amortized_linear_compaction_visits() {
        let registry = WorkerRegistry::default();
        let n = 200usize;
        let mut permits = Vec::with_capacity(n);
        for i in 0..n {
            let res = registry.reserve(None).expect("uncapped reserve");
            let (handle, permit) = test_handle(i as u64, "live");
            registry.register(res, entry_of(Owner::Session, handle));
            permits.push(permit);
        }
        assert_eq!(registry.raw_entry_count(), n);
        let visits = registry.compaction_visits();
        assert!(
            visits <= 4 * n,
            "expected amortized O(N) compaction visits for {n} live births, got {visits}"
        );
        for permit in permits {
            permit();
        }
    }

    /// Repeated uncapped spawn/await in one invocation, without directory reads,
    /// leaves only a bounded batch of stale consumed entries.
    #[test]
    fn repeated_uncapped_births_and_claims_bound_stale_consumed_entries() {
        let registry = WorkerRegistry::default();
        for i in 0..200u64 {
            let res = registry.reserve(None).expect("uncapped reserve");
            let (handle, permit) = test_handle(i, "ephemeral");
            registry.register(res, entry_of(Owner::Session, handle.clone()));
            permit();
            assert!(matches!(handle.claim_ready(), Sample::Ready(_)));
        }
        assert!(
            registry.raw_entry_count() <= MIN_SWEEP_AT,
            "stale consumed entries must stay bounded by MIN_SWEEP_AT, got {}",
            registry.raw_entry_count()
        );
        assert_eq!(registry.count(), 0);
        assert_eq!(registry.raw_entry_count(), 0);
    }

    /// Capped reserve prunes consumed entries and counts live (`running` +
    /// `stopping`) entries in a single pass.
    #[test]
    fn capped_reserve_prunes_and_counts_in_one_pass_including_stopping() {
        let registry = WorkerRegistry::default();
        let (running_h, running_permit) = test_handle(1, "running");
        let (stopping_h, stopping_permit) = test_handle(2, "stopping");
        assert!(stopping_h.claim_and_stop());
        let ready_h = ready_handle(3, "ready");
        let consumed_1 = consumed_handle(4, "consumed-1");
        let consumed_2 = consumed_handle(5, "consumed-2");

        for h in [
            running_h,
            stopping_h.clone(),
            ready_h,
            consumed_1,
            consumed_2,
        ] {
            let res = registry.reserve(None).expect("uncapped seed");
            registry.register(res, entry_of(Owner::Session, h));
        }
        // Register two handles while ready, then claim them so they sit in `entries` as Consumed.
        let late_1 = ready_handle(6, "late-1");
        let late_2 = ready_handle(7, "late-2");
        for h in [late_1.clone(), late_2.clone()] {
            let res = registry.reserve(None).expect("uncapped seed");
            registry.register(res, entry_of(Owner::Session, h));
        }
        let _ = late_1.claim_ready();
        let _ = late_2.claim_ready();

        assert_eq!(
            registry.raw_entry_count(),
            5,
            "running, stopping, ready, and two post-registration consumed entries"
        );
        assert_eq!(registry.compaction_visits(), 0);

        // Cap 2 must be refused because `running` (1) + `stopping` (1) == 2 live workers.
        assert_eq!(registry.reserve(Some(2)).err(), Some(CapReached(2)));
        assert_eq!(
            registry.compaction_visits(),
            5,
            "capped reserve must prune and count in a single pass over the 5 entries"
        );
        assert_eq!(
            registry.raw_entry_count(),
            3,
            "consumed entries must be pruned during capped reserve"
        );

        // Once the stopping worker finishes execution, it becomes Consumed and frees its slot.
        stopping_permit();
        assert_eq!(stopping_h.sample(), Sample::Consumed);
        let admitted = registry
            .reserve(Some(2))
            .expect("finishing a stopping worker frees its capacity slot");
        drop(admitted);
        assert_eq!(registry.raw_entry_count(), 2);

        running_permit();
    }

    /// Retention never stamps or reaps `running` or `stopping`; only `ready`
    /// expires; children of `stopping` parents and gather children are exempt;
    /// consumed entries produce no reap notice.
    #[test]
    fn retention_only_stamps_and_expires_ready_and_exempts_stopping_parent_children() {
        let registry = WorkerRegistry::default();
        registry.arm_retention(2);

        let (running_h, running_permit) = test_handle(1, "running");
        let (stopping_parent_h, stopping_parent_permit) = test_handle(2, "stopping-parent");
        assert!(stopping_parent_h.claim_and_stop());
        let child_of_stopping = ready_handle(3, "child-of-stopping");
        let gather_child = ready_handle(4, "gather-child");
        let ready_session = ready_handle(5, "ready-session");
        let consumed_session = ready_handle(6, "consumed-session");

        for (owner, h) in [
            (Owner::Session, running_h),
            (Owner::Session, stopping_parent_h),
            (Owner::Worker(WorkerId(2)), child_of_stopping),
            (Owner::Gather, gather_child),
            (Owner::Session, ready_session),
            (Owner::Session, consumed_session.clone()),
        ] {
            let res = registry.reserve(None).unwrap();
            registry.register(res, entry_of(owner, h));
        }
        let _ = consumed_session.claim_ready();

        // First sweep at epoch 0: prunes consumed_session silently, stamps only ready_session.
        registry.sweep_retention();
        assert_eq!(registry.take_reap_notices().len(), 0);
        let snap = registry.snapshot();
        assert_eq!(snap.len(), 5);
        for entry in &snap {
            let expected = (entry.handle.id == WorkerId(5)).then_some(0);
            assert_eq!(
                entry.settled_epoch, expected,
                "unexpected settled_epoch for worker {:?}",
                entry.handle.id
            );
        }

        // Advance two epochs and sweep: only ready_session expires.
        registry.tick_epoch();
        registry.tick_epoch();
        registry.sweep_retention();
        assert_eq!(
            registry.take_reap_notices(),
            vec![ReapNotice {
                id: WorkerId(5),
                title: "ready-session".into(),
                cause: ReapCause::Retention,
            }]
        );

        // Once the stopping parent finishes, its ready child becomes eligible on the next sweep.
        stopping_parent_permit();
        registry.sweep_retention();
        let snap = registry.snapshot();
        let child_entry = snap
            .iter()
            .find(|e| e.handle.id == WorkerId(3))
            .expect("child-of-stopping still present");
        assert_eq!(child_entry.settled_epoch, Some(2));

        registry.tick_epoch();
        registry.tick_epoch();
        registry.sweep_retention();
        assert_eq!(
            registry.take_reap_notices(),
            vec![ReapNotice {
                id: WorkerId(3),
                title: "child-of-stopping".into(),
                cause: ReapCause::Retention,
            }]
        );

        running_permit();
    }

    /// Snapshot returns entries sorted by `WorkerId` even when registered out of order.
    #[test]
    fn snapshot_sorts_entries_by_worker_id() {
        let registry = WorkerRegistry::default();
        for id in [30, 10, 20] {
            let res = registry.reserve(None).unwrap();
            registry.register(res, entry_of(Owner::Session, ready_handle(id, "w")));
        }
        let ids: Vec<WorkerId> = registry
            .snapshot()
            .into_iter()
            .map(|e| e.handle.id)
            .collect();
        assert_eq!(ids, vec![WorkerId(10), WorkerId(20), WorkerId(30)]);
    }
}
