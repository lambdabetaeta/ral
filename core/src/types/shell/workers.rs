//! The worker registry: a per-[`Shell`](super::Shell) directory of every
//! detached worker (`spawn`, `watch`) spawned from it.
//!
//! `spawn_child` mints a [`WorkerEntry`] the instant a worker's
//! [`HandleInner`] is constructed and files it here — every spawn
//! registers, REPL included, and no policy attaches here: the registry is
//! pure bookkeeping, the directory the lease policies of
//! `decisions/260705_leases-and-budgets` (reaping, retention, caps) read
//! rather than a policy of its own. An entry
//! is removed the moment the worker is *observed* settled — `await`,
//! `race`'s winner and its cancelled losers, `poll`'s settled arm — or is
//! explicitly `cancel`led; a pending `poll` and plain listing never mutate
//! the registry. Removal always targets the *observing* shell's own
//! registry: if the handle was minted by (and registered in) a different
//! shell, the removal is a no-op and the entry lingers where it lives.
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

use crate::types::HandleInner;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

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

/// Which reaping policy governs a [`WorkerEntry`]. Only the ordinary,
/// unreaped class exists today; `decisions/260705_leases-and-budgets` adds
/// a durable class (no idle lease — legibility is the only bound) together
/// with the lease policy that reads this field. Until then the class is
/// recorded but consulted by nothing — the registry is pure bookkeeping.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LeaseClass {
    /// An ordinary `spawn`/`watch` worker.
    Worker,
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
    pub handle: HandleInner,
}

/// Cheap-clonable, per-[`Shell`](super::Shell) directory of every worker
/// spawned from it.
///
/// A newtype over `Arc<Mutex<Vec<WorkerEntry>>>`: cloning shares the same
/// underlying vector, which is how the flow rule above lets a nested
/// `spawn` register into its owning shell's registry rather than a private
/// copy. Lock discipline is trivial by construction: every operation locks,
/// acts on the `Vec` directly, and unlocks — none ever calls out while
/// holding the lock.
#[derive(Clone, Default)]
pub(crate) struct WorkerRegistry(Arc<Mutex<Vec<WorkerEntry>>>);

impl WorkerRegistry {
    /// File a freshly-spawned worker. `spawn_child` calls this exactly
    /// once per spawn, unconditionally — every spawn registers, and no
    /// policy attaches here.
    pub(crate) fn register(&self, entry: WorkerEntry) {
        self.0.lock().unwrap().push(entry);
    }

    /// Remove the entry carrying `handle`, matched by [`HandleInner`]'s own
    /// [`PartialEq`] (`Arc::ptr_eq` on its result channel). A no-op if
    /// `handle` was registered in a different shell's registry: this
    /// method only ever touches the registry it's called on.
    pub(crate) fn remove(&self, handle: &HandleInner) {
        self.0
            .lock()
            .unwrap()
            .retain(|entry| entry.handle != *handle);
    }

    /// Clone out every entry for listing. Never mutates.
    pub(crate) fn snapshot(&self) -> Vec<WorkerEntry> {
        self.0.lock().unwrap().clone()
    }

    /// Number of registered entries.
    pub(crate) fn count(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}
