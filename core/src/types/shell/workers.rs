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
//! ([`WorkerRegistry::advance_epoch`], driven by the host's ral-call
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
//! `session.root` and `session.builtins`: a `spawn` nested inside a
//! worker's body therefore registers into the *owning* shell's registry —
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
/// in this process. Minted from a process-global counter rather than a
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
        WorkerId(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// The lifetime a frame grants the workers its turns detach: an idle bound
/// on the observation clock under an absolute backstop. A lease, not a
/// death-clock — the worker is reaped when *unobserved* for `idle`, not
/// when `idle` old, and each eliminator naming the handle (`poll`, a
/// blocked `await`/`race` sweep) renews it. The two travel as one value so
/// no ceiling-without-backstop state exists: a frame either grants the
/// whole lease or (`None` on the turn axis — the interactive REPL) never
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
    /// — swept by [`WorkerRegistry::advance_epoch`], counted in ral calls
    /// since the sweep first observed the entry settled.
    Retention,
}

/// The compact record a reap leaves behind — the facts a transcript event
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
/// it. Storing the handle itself — rather than exposing a second by-id
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
    /// The host's ral-call epoch at which [`WorkerRegistry::advance_epoch`]
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

/// The two ledgers behind [`WorkerRegistry`]'s one lock: the live entries,
/// and the reap notices awaiting the host's next drain. One lock for both
/// so a reap is atomic — the entry leaves and its notice lands in the same
/// critical section, never one without the other.
#[derive(Default)]
struct RegistryInner {
    entries: Vec<WorkerEntry>,
    reap_notices: Vec<ReapNotice>,
}

/// Cheap-clonable, per-[`Shell`](super::Shell) directory of every worker
/// spawned from it, plus the reap notices the lease chain leaves behind.
///
/// A newtype over `Arc<Mutex<RegistryInner>>`: cloning shares the same
/// underlying store, which is how the flow rule above lets a nested
/// `spawn` register into its owning shell's registry rather than a private
/// copy — and how the lease chain, firing on the reaper daemon thread,
/// reaps into the same store the shell reads. Lock discipline is trivial by
/// construction: every operation locks, acts on the inner ledgers directly,
/// and unlocks — none ever calls out while holding the lock.
#[derive(Clone, Default)]
pub(crate) struct WorkerRegistry(Arc<Mutex<RegistryInner>>);

impl WorkerRegistry {
    /// File a freshly-spawned worker. `spawn_child` calls this exactly
    /// once per spawn, unconditionally — every spawn registers, and no
    /// policy attaches here.
    pub(crate) fn register(&self, entry: WorkerEntry) {
        self.0.lock().unwrap().entries.push(entry);
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

    /// Advance the registry to the host's ral-call `epoch`, sweeping
    /// settled entries against `retention` — the settled entry's own lease,
    /// where the lease chain governs running workers. Per entry, under one
    /// registry lock:
    ///
    /// - still `Running`: untouched — retention never applies to live work.
    /// - settled and unstamped: stamp `settled_epoch = Some(epoch)`. The
    ///   retention clock is ral-calls-since-first-observed-settled, so a
    ///   worker that settles mid-quiet-period starts its retention at the
    ///   next call, never retroactively.
    /// - stamped `Some(s)` with `epoch − s >= retention` (saturating — a
    ///   host-supplied epoch must never panic core): remove the entry and
    ///   push a [`ReapCause::Retention`] notice, atomic with the removal
    ///   like every reap here.
    ///
    /// The eliminators already remove an entry the moment its result is
    /// observed; this sweep only catches what nobody claimed.
    ///
    /// Lock order: the registry lock may take an entry's `state` lock (the
    /// brief read here and in [`Self::running_count`]), never the reverse —
    /// no code path acquires the registry lock while holding a `state`
    /// lock. Verified at the three state-lock sites outside this module:
    /// `lease_fire`'s if-condition temporary, `builtin_cancel`'s copied-out
    /// read, and the worker exit mark each drop their guard before any
    /// registry call.
    pub(crate) fn advance_epoch(&self, epoch: u64, retention: u64) {
        let mut inner = self.0.lock().unwrap();
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
    }

    /// Number of entries whose worker is still `Running` — the admission
    /// cap's measure. Settled entries lingering under retention never block
    /// a new birth, while a durable service counts like any other live
    /// work. Takes each entry's `state` lock briefly under the registry
    /// lock; see [`Self::advance_epoch`] for the lock order this relies on.
    pub(crate) fn running_count(&self) -> usize {
        self.0
            .lock()
            .unwrap()
            .entries
            .iter()
            .filter(|entry| *entry.handle.state.lock().unwrap() == HandleState::Running)
            .count()
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
        let taken = std::mem::take(&mut *self.0.lock().unwrap());
        for entry in &taken.entries {
            entry
                .handle
                .cancel
                .cancel(crate::process::CancelCause::Explicit);
        }
        taken.entries.len()
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
}
