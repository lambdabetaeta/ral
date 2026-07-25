//! The worker registry: a per-[`Shell`](super::Shell) directory of every
//! detached worker (`spawn`, `watch`, `service`) spawned from it.
//!
//! `spawn_child` mints a [`WorkerEntry`] the instant a worker's
//! [`HandleInner`] is constructed and files it here — every spawn
//! registers, REPL included, and no policy attaches here: the registry is
//! pure bookkeeping, the directory the lease policies read rather than a
//! policy of its own. An entry
//! is removed the moment the worker is *observed* settled — `await`,
//! `race`'s winner and its cancelled losers, `poll`'s settled arm — or is
//! explicitly `cancel`led; a pending `poll` and plain listing never mutate
//! the registry. Removal always targets the *observing* shell's own
//! registry: if the handle was minted by (and registered in) a different
//! shell, the removal is a no-op and the entry lingers where it lives.
//!
//! Two policies also *write* here, per `decisions/260705_leases-and-budgets`.
//! The idle-observation lease (`builtins::concurrency`'s lease chain): under
//! a frame that supplies a
//! [`WorkerLease`], a still-running worker unobserved for `idle` — or older
//! than `backstop` regardless of observation — is reaped, and the reap is
//! recorded as a [`ReapNotice`] beside the entries. [`WorkerRegistry::reap`]
//! is one locked operation — remove the entry, and only if it was present,
//! push the notice — so the reap-vs-observation race is benign: an entry an
//! eliminator observed away first yields no notice. And the retention sweep
//! ([`WorkerRegistry::sweep_retention`], driven by the registry's own ral-call
//! epoch): a settled entry is where an unclaimed result waits, and one
//! nobody claims within the retention bound is removed with a
//! [`ReapCause::Retention`] notice. The host drains the
//! notices at its ready boundaries
//! ([`Shell::take_worker_reap_notices`](super::Shell::take_worker_reap_notices))
//! to emit transcript events, so the model's later "where did my job go?"
//! always has an answer in the log.
//!
//! **Flow rule.** The registry is `Arc`-shared into a spawned worker's own
//! `Shell` by [`Shell::spawn_thread`](super::Shell::spawn_thread), alongside
//! `session.root`, `session.builtins`, and `session.library_docs`: a
//! `spawn` nested inside a worker's body therefore registers into the
//! *owning* shell's registry —
//! the one a top-level `spawn` first registered into — rather than a fresh
//! one of its own. It does **not** flow through `fork_session` /
//! `child_from` / `child_of` / `inherit_from`: a sub-agent fork or a
//! pipeline stage starts with its own empty registry, one per agent,
//! matching the per-agent binding-lease ledger this module sits beside on
//! [`LocalState`](super::LocalState).

use crate::types::{HandleInner, HandleState, Resident};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// Stable identifier for a registered worker, unique across every `Shell`
/// in this process.
///
/// Minted from a process-global counter rather than a
/// per-registry one, so an id never collides even when compared across
/// shells — a fleet listing folds several agents' registries together.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WorkerId(pub u64);

impl WorkerId {
    /// Mint a fresh, process-wide-unique id. Core-only: a host names a
    /// worker through the [`WorkerEntry`] the registry hands back, never by
    /// constructing an id of its own.
    pub(crate) fn mint() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// The lifetime a frame grants the workers its runs detach: an idle bound
/// on the observation clock under an absolute backstop.
///
/// A lease, not a
/// death-clock — the worker is reaped when *unobserved* for `idle`, not
/// when `idle` old, and each eliminator naming the handle (`poll`, a
/// blocked `await`/`race` sweep) renews it. The two travel as one value so
/// no ceiling-without-backstop state exists: a frame either grants the
/// whole lease or (`None` on the run axis — the interactive REPL) never
/// reaps at all.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerLease {
    /// The idle bound: reap once the handle has gone this long without an
    /// eliminator naming it.
    pub idle: Duration,
    /// The absolute bound, measured from spawn: no amount of observation
    /// extends a worker past this age — ritual polling cannot manufacture
    /// immortality.
    pub backstop: Duration,
}

/// Which reaping policy governs a [`WorkerEntry`], declared at birth by
/// the spawning door (`decisions/260705_leases-and-budgets`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LeaseClass {
    /// An ordinary `spawn`/`watch` worker, governed by the frame's
    /// [`WorkerLease`] when one is supplied.
    Worker,
    /// A `service`-born worker: no idle lease, no backstop — legibility is
    /// the bound. It is listed like any entry and cancellable through its
    /// handle, and it dies with `/clear` or the process; the lease chain
    /// is simply never armed for it.
    Durable,
}

/// Why a worker's registry entry was removed by policy: the lease chain's
/// two bounds on a still-running worker, or the retention sweep expiring a
/// settled entry's unclaimed result.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ReapCause {
    /// Unobserved for the lease's `idle` bound.
    Idle,
    /// Older than the lease's `backstop`, observation notwithstanding.
    Backstop,
    /// A settled entry whose unclaimed result outlived the retention bound
    /// — swept by [`WorkerRegistry::sweep_retention`], counted in ral calls
    /// since the sweep first observed the entry settled.
    Retention,
}

/// The compact record a reap leaves behind
///
/// — the facts a transcript event
/// needs (which worker, spelled how, of what class, reaped why) without the
/// handle, which the reap deliberately does not keep alive. Recorded only
/// for an entry that was actually present at reap time: an entry an
/// eliminator observed away first was never reaped, so it leaves no notice.
#[derive(Clone, Debug)]
pub struct ReapNotice {
    pub id: WorkerId,
    pub cmd: String,
    pub class: LeaseClass,
    pub cause: ReapCause,
}

/// One registered worker: the [`WorkerRegistry`]'s record of a `spawn`/
/// `watch` call, paired with the handle a caller uses to observe or cancel
/// it.
///
/// Storing the handle itself — rather than exposing a second by-id
/// control plane — is the decisive design point: `poll`, `await`, `race`,
/// and `cancel` remain the only verbs that touch a worker, so "rediscover a
/// worker" is just list, then take the handle back and resume the ordinary
/// idiom.
#[derive(Clone, Debug)]
pub struct WorkerEntry {
    pub id: WorkerId,
    pub cmd: String,
    /// Wall-clock start, for rendering a listing. Lease math tracks idle
    /// time in its own cells, never through this field: it is
    /// display-only.
    pub started: SystemTime,
    pub class: LeaseClass,
    /// The registry's ral-call epoch at which
    /// [`WorkerRegistry::sweep_retention`]
    /// first observed this entry settled — `None` while it runs. The
    /// retention clock is ral-calls-since-first-observed-settled, so a
    /// worker that settles mid-quiet-period starts its retention at the
    /// next call, never retroactively.
    pub settled_epoch: Option<u64>,
    pub handle: HandleInner,
}

/// The worker chapter's resident signature (`decisions/260705_session-ledger`,
/// `types/resident.rs`): a `[wN]` designator distinct from a pgid job's, a
/// handle-typed capability, and a state word that already carries this
/// population's own `"(worker)"` qualifier — the REPL's `jobs` fold and its
/// exit-time survivor warning read every facet here rather than
/// hand-formatting a `WorkerEntry` themselves.
impl Resident for WorkerEntry {
    fn designator(&self) -> String {
        format!("w{}", self.id.0)
    }

    fn population(&self) -> &'static str {
        "worker"
    }

    fn capability_kind(&self) -> &'static str {
        "handle"
    }

    /// Core knows only the *class*, not a specific frame's `WorkerLease`
    /// bounds (those are a host policy, never stored on the entry) — so a
    /// `Worker` names the mechanism honestly rather than fabricating
    /// numbers it does not have; a host with the bounds in hand (exarch's
    /// `/resources` fold) renders its own sharper row from them instead of
    /// reading this one.
    fn lease_row(&self) -> String {
        match self.class {
            LeaseClass::Worker => {
                "idle-observation lease — idle bound under an absolute backstop, both host-configured".to_string()
            }
            LeaseClass::Durable => "none — durable; dies by cancel, /clear, or process exit".to_string(),
        }
    }

    fn state_label(&self) -> String {
        let running = *self.handle.state.lock().unwrap() == HandleState::Running;
        if running {
            "running (worker)".to_string()
        } else {
            "done (worker)".to_string()
        }
    }

    fn cancel(&self) {
        self.handle
            .cancel
            .cancel(crate::process::CancelCause::Explicit);
    }
}

/// The three ledgers behind [`WorkerRegistry`]'s one lock: the live
/// entries, the reap notices awaiting the host's next drain, and the count
/// of seats reserved for a birth still in flight. One lock for all three,
/// which is what makes a reap atomic (the entry leaves and its notice
/// lands in the same critical section, never one without the other) and
/// what makes admission honest (a reservation is counted the instant
/// [`WorkerRegistry::reserve`] grants it, so a sibling `reserve` racing the
/// same free seat never gets to read it as available).
#[derive(Default)]
struct RegistryInner {
    entries: Vec<WorkerEntry>,
    reap_notices: Vec<ReapNotice>,
    /// Seats held by a [`Reservation`] not yet fulfilled by
    /// [`WorkerRegistry::register`] or released by its drop. Counted
    /// alongside running entries in [`WorkerRegistry::reserve`]'s
    /// admission measure, so a birth-in-progress occupies its seat before
    /// it has anything registered to show for it.
    reserved: usize,
    /// The registry's own ral-call clock: one tick per source dispatch
    /// ([`Shell::run`](super::Shell::run)'s Source arm), the same
    /// cadence the binding-lease ledger keeps.
    epoch: u64,
    /// The armed settled-entry retention, in ral calls. `None` — no host
    /// ever armed it (the REPL) — retains settled entries indefinitely:
    /// [`WorkerRegistry::sweep_retention`] is then a no-op.
    retention: Option<u64>,
}

/// Cheap-clonable, per-[`Shell`](super::Shell) directory of every worker
/// spawned from it, plus the reap notices the lease chain leaves behind
/// and the seats reserved for a birth still in flight.
///
/// A newtype over `Arc<Mutex<RegistryInner>>`: cloning shares the same
/// underlying store, which is how the flow rule above lets a nested
/// `spawn` register into its owning shell's registry rather than a private
/// copy — and how the lease chain, firing on the reaper daemon thread,
/// reaps into the same store the shell reads. Lock discipline is trivial by
/// construction: every operation locks, acts on the inner ledgers directly,
/// and unlocks — none ever calls out while holding the lock. Admission and
/// registration are the one apparent exception: [`Self::reserve`] and
/// [`Self::register`] are still two separate locked steps, but the
/// [`Reservation`] bridging them holds the seat counted the instant
/// admission is granted, so nothing a sibling birth does in the gap
/// between the two calls can make the cap dishonest.
#[derive(Clone, Default)]
pub(crate) struct WorkerRegistry(Arc<Mutex<RegistryInner>>);

/// Refusal handed back by [`WorkerRegistry::reserve`] when `cap`
/// running-or-reserved workers already fill every seat the frame allows.
/// Carries the cap it was refused against, so the caller can compose the
/// user-facing remedy message without re-deriving the number that caused
/// the refusal.
pub(crate) struct CapReached(pub(crate) usize);

/// A seat held between [`WorkerRegistry::reserve`] granting admission and
/// [`WorkerRegistry::register`] filing the entry it was granted for — the
/// bridge that makes the two one atomic transaction even though a thread
/// spawn and a handle construction happen in between. Consuming a
/// `Reservation` is `register`'s only way to accept a [`WorkerEntry`], so
/// registering without ever having been admitted is not a thing the types
/// allow.
///
/// RAII covers every other exit: a `Reservation` dropped without reaching
/// `register` — an early return on the way to a handle, or any other error
/// before registration — releases its seat under the same lock. The
/// `armed` flag is the standard defusal: `register` clears it before the
/// consumed value's drop runs, so the seat is released exactly once either
/// way.
pub(crate) struct Reservation {
    registry: WorkerRegistry,
    armed: bool,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.armed {
            let mut inner = self.registry.0.lock().unwrap();
            inner.reserved = inner.reserved.saturating_sub(1);
        }
    }
}

impl WorkerRegistry {
    /// Measure admission and hold a seat in one locked step, so a birth
    /// that only registers later — after spawning its thread and building
    /// its handle — cannot be raced by a sibling `reserve` reading the same
    /// free seat in the gap before it does. The measure is running entries
    /// (`handle.state` still [`HandleState::Running`], each briefly locked
    /// under the registry lock — the order [`Self::sweep_retention`]
    /// documents) plus every seat already reserved and not yet filed or
    /// released. Under `cap = Some(c)` a reservation is refused once that
    /// count reaches `c`; under `cap = None` (an uncapped frame — the
    /// interactive REPL) it always succeeds. On success, `reserved` is
    /// incremented and a [`Reservation`] handed back for the caller to
    /// fulfil ([`Self::register`]) or abandon.
    pub(crate) fn reserve(&self, cap: Option<usize>) -> Result<Reservation, CapReached> {
        let mut inner = self.0.lock().unwrap();
        if let Some(cap) = cap {
            let running = inner
                .entries
                .iter()
                .filter(|entry| *entry.handle.state.lock().unwrap() == HandleState::Running)
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

    /// File a freshly-spawned worker, consuming the [`Reservation`]
    /// `reserve` minted for it. One locked operation: the seat leaves
    /// `reserved` and the entry joins `entries` together, so nothing ever
    /// observes an entry whose seat is still counted as reserved, or a
    /// reservation whose entry has already appeared. `spawn_child` calls
    /// this exactly once per admitted spawn — every admitted spawn
    /// registers, and no further policy attaches here.
    pub(crate) fn register(&self, mut reservation: Reservation, entry: WorkerEntry) {
        reservation.armed = false;
        let mut inner = self.0.lock().unwrap();
        inner.reserved = inner.reserved.saturating_sub(1);
        inner.entries.push(entry);
    }

    /// Remove the entry carrying `handle`, matched by [`HandleInner`]'s own
    /// [`PartialEq`] (`Arc::ptr_eq` on its result channel). A no-op if
    /// `handle` was registered in a different shell's registry: this
    /// method only ever touches the registry it's called on.
    pub(crate) fn remove(&self, handle: &HandleInner) {
        self.0
            .lock()
            .unwrap()
            .entries
            .retain(|entry| entry.handle != *handle);
    }

    /// Reap the entry carrying `id`: remove it and, only when it was
    /// actually present, record a [`ReapNotice`] built from its facts.
    /// One locked operation, which is what makes the reap-vs-observation
    /// race benign — an entry an eliminator observed away first is simply
    /// absent here, so the reap collapses to a silent no-op rather than a
    /// notice for a worker whose result was in fact claimed.
    pub(crate) fn reap(&self, id: WorkerId, cause: ReapCause) {
        let mut inner = self.0.lock().unwrap();
        let Some(at) = inner.entries.iter().position(|entry| entry.id == id) else {
            return;
        };
        let entry = inner.entries.remove(at);
        inner.reap_notices.push(ReapNotice {
            id: entry.id,
            cmd: entry.cmd,
            class: entry.class,
            cause,
        });
    }

    /// Arm the settled-entry retention bound, in ral calls. Idempotent by
    /// replacement, like the binding lease's arm door: a re-arm swaps the
    /// bound; already-stamped entries are measured against the new one at
    /// the next sweep.
    pub(crate) fn arm_retention(&self, retention: u64) {
        self.0.lock().unwrap().retention = Some(retention);
    }

    /// Advance the registry's own ral-call clock by one. Ticked at the run
    /// door's Source arm, beside the binding ledger's tick, so the two
    /// ledgers read one logical clock.
    pub(crate) fn tick_epoch(&self) {
        self.0.lock().unwrap().epoch += 1;
    }

    /// Sweep settled entries against the armed retention — the settled
    /// entry's own lease, where the lease chain governs running workers. A
    /// no-op unarmed. Per entry, under one registry lock:
    ///
    /// - still `Running`: untouched — retention never applies to live work.
    /// - settled and unstamped: stamp `settled_epoch = Some(epoch)`. The
    ///   retention clock is ral-calls-since-first-observed-settled, so a
    ///   worker that settles mid-quiet-period starts its retention at the
    ///   next call, never retroactively.
    /// - stamped `Some(s)` with `epoch − s >= retention` (saturating):
    ///   remove the entry and
    ///   push a [`ReapCause::Retention`] notice, atomic with the removal
    ///   like every reap here.
    ///
    /// The eliminators already remove an entry the moment its result is
    /// observed; this sweep only catches what nobody claimed.
    ///
    /// Lock order: the registry lock may take an entry's `state` lock (the
    /// brief read here and in [`Self::reserve`]), never the reverse — no
    /// code path acquires the registry lock while holding a `state` lock.
    /// Verified at the three state-lock sites outside this module:
    /// `lease_fire`'s if-condition temporary, `builtin_cancel`'s copied-out
    /// read, and the worker exit mark each drop their guard before any
    /// registry call.
    pub(crate) fn sweep_retention(&self) {
        let mut inner = self.0.lock().unwrap();
        let Some(retention) = inner.retention else {
            return;
        };
        let epoch = inner.epoch;
        let mut i = 0;
        while i < inner.entries.len() {
            let running = *inner.entries[i].handle.state.lock().unwrap() == HandleState::Running;
            match (running, inner.entries[i].settled_epoch) {
                (false, None) => {
                    inner.entries[i].settled_epoch = Some(epoch);
                    i += 1;
                }
                (false, Some(s)) if epoch.saturating_sub(s) >= retention => {
                    let entry = inner.entries.remove(i);
                    inner.reap_notices.push(ReapNotice {
                        id: entry.id,
                        cmd: entry.cmd,
                        class: entry.class,
                        cause: ReapCause::Retention,
                    });
                }
                _ => i += 1,
            }
        }
        drop(inner);
    }

    /// Drain every accumulated [`ReapNotice`], leaving the ledger empty.
    pub(crate) fn take_reap_notices(&self) -> Vec<ReapNotice> {
        std::mem::take(&mut self.0.lock().unwrap().reap_notices)
    }

    /// Clone out every entry for listing. Never mutates — enumeration is
    /// not observation, so it renews no lease.
    pub(crate) fn snapshot(&self) -> Vec<WorkerEntry> {
        self.0.lock().unwrap().entries.clone()
    }

    /// Clone out the one entry named by `id`, or `None` if it names
    /// nothing live.  A pure read like [`Self::snapshot`]: renews no lease.
    pub(crate) fn lookup(&self, id: WorkerId) -> Option<WorkerEntry> {
        self.0
            .lock()
            .unwrap()
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .cloned()
    }

    /// Number of registered entries.
    pub(crate) fn count(&self) -> usize {
        self.0.lock().unwrap().entries.len()
    }

    /// Cancel every registered entry's scope and reset the registry
    /// wholesale — entries and any pending reap notices both dropped, so a
    /// rebuilt context carries neither stale workers nor stale reap events.
    /// This is `/clear`'s arm: explicit destruction outranks every lease,
    /// the durable class included, so nothing here consults `LeaseClass`.
    /// Returns the number of entries cancelled.
    ///
    /// One locked operation takes the whole inner ledger, replacing it with
    /// an empty one; the cancels fire only after the guard drops, per the
    /// module's lock discipline — never calling out while holding the lock.
    pub(crate) fn cancel_all(&self) -> usize {
        let (entries, _notices) = {
            let mut inner = self.0.lock().unwrap();
            // Entries and pending notices reset together; the armed
            // retention policy and the epoch clock are configuration, not
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
                .cancel(crate::process::CancelCause::Explicit);
        }
        entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal registered-worker fixture — the same construction core's
    /// own concurrency tests use, every `HandleInner` field legitimately
    /// public this side of `decisions/260615_no-core-repr-leak-into-exarch`.
    fn fake_entry(id: u64, cmd: &str, class: LeaseClass, running: bool) -> WorkerEntry {
        let state = if running {
            HandleState::Running
        } else {
            HandleState::Completed
        };
        WorkerEntry {
            id: WorkerId(id),
            cmd: cmd.to_string(),
            started: SystemTime::now(),
            class,
            settled_epoch: None,
            handle: HandleInner {
                result: Arc::new(Mutex::new(None)),
                cached: Arc::new(Mutex::new(None)),
                state: Arc::new(Mutex::new(state)),
                stdout_buf: Arc::new(Mutex::new(Vec::new())),
                stderr_buf: Arc::new(Mutex::new(Vec::new())),
                surface_buf: Arc::new(Mutex::new(Vec::new())),
                joined: Arc::new(Mutex::new(false)),
                last_observed: Arc::new(Mutex::new(std::time::Instant::now())),
                cmd: cmd.to_string(),
                cancel: crate::process::CancelScope::default(),
            },
        }
    }

    /// Every facet a running `spawn`/`watch` worker answers through
    /// [`Resident`]: a `wN` designator (unbracketed — a fold brackets it
    /// uniformly), the `"worker"` population, a `"handle"` capability, and
    /// a state word that already carries this population's own qualifier.
    #[test]
    fn resident_facets_for_a_running_worker() {
        let entry = fake_entry(3, "spawn { x }", LeaseClass::Worker, true);
        assert_eq!(entry.designator(), "w3");
        assert_eq!(entry.population(), "worker");
        assert_eq!(entry.capability_kind(), "handle");
        assert_eq!(entry.state_label(), "running (worker)");
        assert!(entry.lease_row().contains("idle"));
    }

    /// A settled-but-unclaimed worker reads `done`, not `running` — the
    /// POSIX-`Done` analogue the REPL's `jobs` fold relies on.
    #[test]
    fn resident_state_label_for_a_settled_worker() {
        let entry = fake_entry(7, "watch { x }", LeaseClass::Worker, false);
        assert_eq!(entry.state_label(), "done (worker)");
    }

    /// A durable (`service`) worker's lease row names the degenerate case
    /// honestly: no idle bound, no backstop — it dies by cancel, `/clear`,
    /// or process exit, never by an unobserved timeout.
    #[test]
    fn resident_lease_row_names_the_durable_degenerate_case() {
        let entry = fake_entry(9, "service { x }", LeaseClass::Durable, true);
        let row = entry.lease_row();
        assert!(row.contains("durable"));
        assert!(row.contains("/clear"));
    }

    /// `cancel` fires the worker's own cooperative cancel scope — the same
    /// edge [`WorkerRegistry::cancel_all`] already fires per entry, reached
    /// here through the resident signature instead.
    #[test]
    fn resident_cancel_fires_the_handles_cancel_scope() {
        let entry = fake_entry(1, "spawn { x }", LeaseClass::Worker, true);
        assert!(!entry.handle.cancel.is_cancelled());
        entry.cancel();
        assert!(entry.handle.cancel.is_cancelled());
    }

    // ── reservation (the admission/registration TOCTOU close) ───────────

    /// Eight threads race `reserve` against `cap = Some(2)`, each holding
    /// whatever it was granted for a moment before reporting back — so a
    /// racy over-admission would have to show up as more than two
    /// reservations alive at once, not merely more than two successful
    /// calls that happened not to overlap. Exactly two of the eight ever
    /// succeed: the one locked measurement `reserve` performs is what a
    /// plain check-then-register could not guarantee.
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
                    std::thread::sleep(Duration::from_millis(20));
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

    /// A `Reservation` dropped without ever reaching `register` releases
    /// its seat immediately: under `cap = Some(1)`, a second `reserve` is
    /// refused while the first is held and admitted the instant the first
    /// drops. This is the RAII path `spawn_child`'s `clone_parent()?` early
    /// return (and any other error on the way to registration) relies on.
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
