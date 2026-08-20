//! The worker registry: a per-[`Shell`](super::Shell) directory of every
//! detached worker (`spawn`, `watch`, `service`) spawned from it. Pure
//! bookkeeping — the directory the lease policies read, never a policy itself.
//!
//! `spawn_child` in `builtins::concurrency` files an entry as it mints the
//! handle. The entry leaves when the worker is *observed* settled (`await`,
//! `race`'s winner and its cancelled losers, `poll`'s settled arm) or is
//! `cancel`led, and only from the observing shell's own registry — a handle
//! minted elsewhere lingers where it lives. Two policies also remove entries:
//! the lease chain in `builtins::concurrency`, against a [`WorkerLease`], and
//! [`WorkerRegistry::sweep_retention`]. Both leave a [`ReapNotice`] the host
//! drains at its ready boundaries, so a vanished job still has an answer in
//! the transcript.
//!
//! **Flow rule.** [`Shell::spawn_thread`](super::Shell::spawn_thread)
//! `Arc`-shares the registry into a worker's own `Shell`, so a nested `spawn`
//! registers into the owning shell's directory; `fork_session` / `child_from`
//! / `child_of` / `inherit_from` do not, and a sub-agent fork starts empty.

use crate::types::{HandleInner, HandleState, Resident};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// How long a teardown waits for cancelled workers to die. A child's wait loop
/// sees a cancel within 100ms and grants its group a 500ms SIGTERM grace before
/// SIGKILL (`runtime::command::child`), so anything that will die dies well
/// inside this; expiry means a wedged worker, and exiting anyway is the lesser
/// harm.
const TEARDOWN_GRACE: Duration = Duration::from_millis(1500);

/// Stable identifier for a registered worker, minted from a process-global
/// counter rather than a per-registry one, so ids never collide across
/// shells.
///
/// A fleet listing folds several agents' registries together.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WorkerId(pub u64);

impl WorkerId {
    pub(crate) fn mint() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// The lifetime a frame grants the workers its runs detach: an idle bound on
/// the observation clock under an absolute backstop.
///
/// A lease, not a death-clock: a worker is reaped when *unobserved* for
/// `idle`, and every eliminator naming its handle renews it. The bounds travel
/// as one value, so a frame grants the whole lease or — the interactive REPL —
/// none, never a ceiling without a backstop.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerLease {
    /// Reap once the handle has gone this long unnamed by an eliminator.
    pub idle: Duration,
    /// From spawn: no observation extends a worker past this age, so ritual
    /// polling cannot manufacture immortality.
    pub backstop: Duration,
}

/// Which reaping policy governs a [`WorkerEntry`], declared at birth by the
/// spawning door.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LeaseClass {
    /// Governed by the frame's [`WorkerLease`] when one is supplied.
    Worker,
    /// A `service`-born worker: the lease chain is never armed for it, so it
    /// dies only by cancel, `/clear`, or process exit.
    Durable,
}

/// Why policy removed a worker's entry.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ReapCause {
    /// Unobserved for the lease's `idle` bound.
    Idle,
    /// Older than the lease's `backstop`, observation notwithstanding.
    Backstop,
    /// A settled entry whose unclaimed result outlived the retention bound.
    Retention,
}

/// What a reap leaves for the transcript event, minus the handle it
/// deliberately does not keep alive.
///
/// Recorded only for an entry present at reap time, so a worker an
/// eliminator observed away first leaves none.
#[derive(Clone, Debug)]
pub struct ReapNotice {
    pub id: WorkerId,
    pub cmd: String,
    pub class: LeaseClass,
    pub cause: ReapCause,
}

/// One registered worker, paired with the handle a caller observes or cancels
/// it through.
///
/// Storing the handle rather than a second by-id control plane keeps `poll`,
/// `await`, `race`, and `cancel` the only verbs that touch a worker:
/// rediscovery is list, then take the handle back.
#[derive(Clone, Debug)]
pub struct WorkerEntry {
    pub id: WorkerId,
    pub cmd: String,
    /// Wall-clock start, display-only: lease math keeps its own clocks.
    pub started: SystemTime,
    pub class: LeaseClass,
    /// The ral-call epoch at which [`WorkerRegistry::sweep_retention`] first
    /// observed this entry settled — `None` while it runs, so retention starts
    /// at the next call rather than retroactively.
    pub settled_epoch: Option<u64>,
    pub handle: HandleInner,
}

/// The worker chapter's [`Resident`] facets, which the REPL's `jobs` fold and
/// its exit-time teardown notice read rather than hand-formatting a
/// `WorkerEntry` themselves.
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

    /// Core knows the *class*, never a frame's [`WorkerLease`] bounds — host
    /// policy, not stored on the entry — so it names the mechanism rather than
    /// fabricating numbers; a host holding the bounds renders its own row.
    fn lease_row(&self) -> String {
        match self.class {
            LeaseClass::Worker => {
                "idle-observation lease — idle bound under an absolute backstop, both host-configured".to_string()
            }
            LeaseClass::Durable => "none — durable; dies by cancel, /clear, or process exit, so it does not outlive this process — work that must still be running afterwards needs detach".to_string(),
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
/// shell's registry, and how the lease chain, on the reaper daemon thread,
/// reaps into the store the shell reads. Every operation locks, acts, and
/// unlocks — none ever calls out while holding the lock, [`Self::reserve`] and
/// [`Self::register`] included: the [`Reservation`] bridging those two locked
/// steps holds its seat from the instant admission is granted.
#[derive(Clone, Default)]
pub(crate) struct WorkerRegistry(Arc<Mutex<RegistryInner>>);

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
            let mut inner = self.registry.0.lock().unwrap();
            inner.reserved = inner.reserved.saturating_sub(1);
        }
    }
}

impl WorkerRegistry {
    /// Measure admission and hold a seat in one locked step, so a birth that
    /// registers only later — after a thread spawn and a handle construction —
    /// cannot be raced by a sibling reading the same free seat. The measure is
    /// running entries (each `state` briefly locked under the registry lock,
    /// the order [`Self::sweep_retention`] documents) plus seats reserved.
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

    /// File a freshly-spawned worker, consuming its [`Reservation`]. One
    /// locked step, so nothing ever observes an entry whose seat is still
    /// counted reserved, or a reservation whose entry has already appeared.
    pub(crate) fn register(&self, mut reservation: Reservation, entry: WorkerEntry) {
        reservation.armed = false;
        let mut inner = self.0.lock().unwrap();
        inner.reserved = inner.reserved.saturating_sub(1);
        inner.entries.push(entry);
    }

    /// Remove the entry carrying `handle`, matched by [`HandleInner`]'s own
    /// [`PartialEq`] (`Arc::ptr_eq` on its result channel). A no-op when the
    /// handle was registered in a different shell's registry.
    pub(crate) fn remove(&self, handle: &HandleInner) {
        self.0
            .lock()
            .unwrap()
            .entries
            .retain(|entry| entry.handle != *handle);
    }

    /// Remove the entry carrying `id` and, only if it was present, record a
    /// [`ReapNotice`]. One locked operation, so the reap-vs-observation race
    /// is benign: an entry an eliminator observed away first is simply absent,
    /// and the reap is silent rather than a notice for a claimed result.
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
    /// replacement: already-stamped entries are measured against the new one.
    pub(crate) fn arm_retention(&self, retention: u64) {
        self.0.lock().unwrap().retention = Some(retention);
    }

    /// Ticked at the run door's Source arm, beside the binding ledger's tick,
    /// so the two ledgers read one logical clock.
    pub(crate) fn tick_epoch(&self) {
        self.0.lock().unwrap().epoch += 1;
    }

    /// Expire settled entries against the armed retention; a no-op unarmed. An
    /// entry is stamped the first sweep that finds it settled and removed with
    /// a [`ReapCause::Retention`] notice `retention` calls later, so retention
    /// never runs retroactively over a quiet period. The eliminators already
    /// take an entry the moment its result is observed; this catches the rest.
    ///
    /// Lock order: the registry lock may take an entry's `state` lock (the
    /// brief read here and in [`Self::reserve`]), never the reverse. Every
    /// `state` lock outside this module drops its guard before any registry
    /// call.
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

    pub(crate) fn take_reap_notices(&self) -> Vec<ReapNotice> {
        std::mem::take(&mut self.0.lock().unwrap().reap_notices)
    }

    /// Clone out every entry for listing. Enumeration is not observation: it
    /// renews no lease.
    pub(crate) fn snapshot(&self) -> Vec<WorkerEntry> {
        self.0.lock().unwrap().entries.clone()
    }

    /// Clone out the entry named by `id`. A pure read like [`Self::snapshot`]:
    /// renews no lease.
    pub(crate) fn lookup(&self, id: WorkerId) -> Option<WorkerEntry> {
        self.0
            .lock()
            .unwrap()
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .cloned()
    }

    pub(crate) fn count(&self) -> usize {
        self.0.lock().unwrap().entries.len()
    }

    /// A live-thread ticket, held by the worker thread's own frame for as long
    /// as it runs. [`Shell::spawn_thread`](super::Shell::spawn_thread) takes one
    /// per worker, so no spawn site can forget to.
    pub(crate) fn live_ticket(&self) -> Arc<()> {
        self.0.lock().unwrap().live.clone()
    }

    /// Wait, up to [`TEARDOWN_GRACE`], for every live worker thread to end. A
    /// cancel lands at the worker's next observation point, and a host that
    /// exits in the same breath outruns it — orphaning the child under PID 1.
    fn drain(&self) {
        let deadline = std::time::Instant::now() + TEARDOWN_GRACE;
        while std::time::Instant::now() < deadline {
            if Arc::strong_count(&self.0.lock().unwrap().live) == 1 {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Destruction — `/clear`'s arm and the session teardown's: cancel every
    /// entry's scope, reset the roster, pending notices included, then wait for
    /// the cancels to land. Explicit destruction outranks every lease, the
    /// durable class included, so nothing here consults [`LeaseClass`]. The
    /// cancels fire only after the guard drops.
    ///
    /// An empty roster still drains: a worker `cancel`led moments ago left the
    /// roster when it was signalled, not when its child died.
    pub(crate) fn cancel_all(&self) -> usize {
        let (entries, _notices) = {
            let mut inner = self.0.lock().unwrap();
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
                .cancel(crate::process::CancelCause::Explicit);
        }
        self.drain();
        entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                stdout_buf: crate::io::ByteBuffer::default(),
                stderr_buf: crate::io::ByteBuffer::default(),
                surface_buf: Arc::new(Mutex::new(Vec::new())),
                joined: Arc::new(Mutex::new(false)),
                last_observed: Arc::new(Mutex::new(std::time::Instant::now())),
                cmd: cmd.to_string(),
                cancel: crate::process::CancelScope::default(),
            },
        }
    }

    /// Every facet a running worker answers through [`Resident`]; the
    /// designator is unbracketed, a fold bracketing it uniformly.
    #[test]
    fn resident_facets_for_a_running_worker() {
        let entry = fake_entry(3, "spawn { x }", LeaseClass::Worker, true);
        assert_eq!(entry.designator(), "w3");
        assert_eq!(entry.population(), "worker");
        assert_eq!(entry.capability_kind(), "handle");
        assert_eq!(entry.state_label(), "running (worker)");
        assert!(entry.lease_row().contains("idle"));
    }

    /// A settled-but-unclaimed worker reads `done` — the POSIX-`Done`
    /// analogue the REPL's `jobs` fold relies on.
    #[test]
    fn resident_state_label_for_a_settled_worker() {
        let entry = fake_entry(7, "watch { x }", LeaseClass::Worker, false);
        assert_eq!(entry.state_label(), "done (worker)");
    }

    /// A durable worker's lease row names the degenerate case honestly: no
    /// idle bound, no backstop, so it never dies by an unobserved timeout.
    #[test]
    fn resident_lease_row_names_the_durable_degenerate_case() {
        let entry = fake_entry(9, "service { x }", LeaseClass::Durable, true);
        let row = entry.lease_row();
        assert!(row.contains("durable"));
        assert!(row.contains("/clear"));
    }

    /// `cancel` fires the worker's own cooperative scope, the same edge
    /// [`WorkerRegistry::cancel_all`] fires per entry.
    #[test]
    fn resident_cancel_fires_the_handles_cancel_scope() {
        let entry = fake_entry(1, "spawn { x }", LeaseClass::Worker, true);
        assert!(!entry.handle.cancel.is_cancelled());
        entry.cancel();
        assert!(entry.handle.cancel.is_cancelled());
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
