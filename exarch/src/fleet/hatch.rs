//! The pending-hatch table: `agent-start`'s wire arm reserves a name and a
//! token here at phase 1; `agent-hatched`/`agent-abort` redeem or drop it at
//! phase 2. Fleet-shared and `Arc`-cloned like [`crate::fleet::registry::AgentRegistry`],
//! since the two enquiries that touch one entry may cross different `ral`
//! calls, and a racing sibling's `agent-start` must see a name this table
//! reserved from another call entirely.

use crate::agent::event::AgentLog;
use ral_core::types::Capabilities;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A pending hatch outlives its answer by at most this long — the clock that
/// frees a name whose builtin died before `agent-hatched` or `agent-abort`
/// ever reached this desk again. Twice [`crate::agent::hatchery::DIAL_PATIENCE`],
/// so a hatch genuinely still waiting on its own dial is never swept out
/// from under it.
pub(crate) const PENDING_HATCH_TTL: Duration = Duration::from_secs(10);

/// Everything `agent-hatched` needs to finish the spawn spine once the dial
/// lands — the same bits [`crate::fleet::desk::ExarchDesk::launch`]'s
/// identity arm computes and uses at once, stashed here instead across the
/// gap to phase 2.
pub(crate) struct PendingHatch {
    pub(crate) name: String,
    pub(crate) prompt: String,
    pub(crate) child_caps: Capabilities,
    pub(crate) child_log: AgentLog,
    pub(crate) system_prompt: String,
    pub(crate) search: bool,
    minted: Instant,
}

impl PendingHatch {
    pub(crate) fn new(
        name: String,
        prompt: String,
        child_caps: Capabilities,
        child_log: AgentLog,
        system_prompt: String,
        search: bool,
    ) -> Self {
        Self {
            name,
            prompt,
            child_caps,
            child_log,
            system_prompt,
            search,
            minted: Instant::now(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct PendingHatches(Arc<Mutex<HashMap<u64, PendingHatch>>>);

impl PendingHatches {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Mutex::new(HashMap::new())))
    }

    fn sweep(table: &mut HashMap<u64, PendingHatch>) {
        table.retain(|_, pending| pending.minted.elapsed() < PENDING_HATCH_TTL);
    }

    /// Whether `name` is reserved by a live pending hatch — `agent-start`'s
    /// name-collision guard checks this alongside
    /// [`crate::fleet::registry::AgentRegistry::name_live`], since a wire
    /// spawn's name is claimed here before any agent bearing it is ever
    /// registered.
    pub(crate) fn name_reserved(&self, name: &str) -> bool {
        let mut table = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::sweep(&mut table);
        table.values().any(|pending| pending.name == name)
    }

    pub(crate) fn reserve(&self, token: u64, pending: PendingHatch) {
        let mut table = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::sweep(&mut table);
        table.insert(token, pending);
    }

    /// Redeem (`agent-hatched`) or drop (`agent-abort`) a pending hatch —
    /// both remove it, so a token is single-use either way and a stray
    /// second enquiry finds nothing to act on.
    pub(crate) fn take(&self, token: u64) -> Option<PendingHatch> {
        let mut table = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::sweep(&mut table);
        table.remove(&token)
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs scaffolding"
)]
mod tests {
    use super::*;

    fn fresh_log() -> AgentLog {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "exarch-pending-hatch-test-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        AgentLog::root(&root, 0, "test", "test", 0).expect("session log")
    }

    fn pending(name: &str) -> PendingHatch {
        PendingHatch::new(
            name.to_string(),
            "go".to_string(),
            Capabilities::root(),
            fresh_log(),
            String::new(),
            true,
        )
    }

    #[test]
    fn reserve_then_take_is_single_use() {
        let table = PendingHatches::new();
        table.reserve(1, pending("helper"));
        assert!(table.name_reserved("helper"));
        assert!(table.take(1).is_some());
        assert!(table.take(1).is_none(), "a token is redeemed at most once");
        assert!(
            !table.name_reserved("helper"),
            "taking the entry frees its reserved name"
        );
    }

    #[test]
    fn a_stale_entry_is_swept_on_the_next_touch() {
        let table = PendingHatches::new();
        table.0.lock().unwrap().insert(
            99,
            PendingHatch {
                minted: Instant::now()
                    .checked_sub(PENDING_HATCH_TTL + Duration::from_secs(1))
                    .expect("the process has run for over ten seconds by the time tests execute"),
                ..pending("stale")
            },
        );
        assert!(
            !table.name_reserved("stale"),
            "an expired pending hatch must not go on reserving its name"
        );
        assert!(table.take(99).is_none());
    }
}
