//! The binding-lease ledger: a per-[`Shell`](super::Shell) policy that lets
//! an agent host expire an idle top-level scratch name, leaving core's
//! lexical semantics untouched everywhere else
//! (`decisions/260629_agent-binding-reaping`).
//!
//! **Single-writer verification.** This ledger is a plain owned struct with
//! no lock — unlike the [`WorkerRegistry`](super::workers::WorkerRegistry)
//! beside it on [`LocalState`](super::LocalState), which needs
//! `Arc<Mutex<…>>` because the reaper daemon thread and spawned worker
//! threads write it concurrently with the agent's own thread. The binding
//! ledger has exactly one writer, verified by walking every path that
//! reaches a `Shell` in exarch:
//!
//! - Every touch is `Agent::transport.shell_mut()`. Every `Agent` — the
//!   trunk and every forked sub-agent — is driven by exactly one dedicated
//!   OS thread running `Agent::drive`: the TUI's `worker` thread
//!   (`exarch/src/tui/tui_loop.rs`, `std::thread::Builder::spawn_scoped`),
//!   headless's single `pump` worker thread (`exarch/src/headless.rs`), and
//!   each `agent`-tool spawn's own dedicated thread
//!   (`exarch/src/tools/agent.rs`) for a fork. No two threads ever hold the
//!   same `Agent`, and an `Agent` (with it, its `Shell`) never migrates
//!   threads mid-life.
//! - `/clear` and `/resources` do not bypass this: both route as
//!   `InboxMsg::Command` through `Agent::drive`'s loop, handled by
//!   `Control::command` — called from *inside* `drive`, on the agent's own
//!   thread, never from the TUI's separate render/input thread (which reads
//!   only the bus and the fleet's shared, separately-locked registry).
//! - The reaper daemon thread that fires the worker lease
//!   (`decisions/260705_leases-and-budgets`) never touches
//!   `LocalState::bindings`: it only ever takes the worker registry's own
//!   lock.
//!
//! A lock here would document a race that cannot happen. If a future change
//! ever lets a second thread reach `&mut Shell` for a live agent, this
//! module's unlocked design is the first thing that must change — and this
//! doc comment is where to look.

use std::collections::{HashMap, HashSet};

/// Host-stated per-agent policy: a leased name expires once it has gone
/// `idle_calls` epochs without use. The epoch is the shell's committed-turn
/// clock ([`Shell::run_source_turn`](super::Shell::run_source_turn)'s tick),
/// never wall time.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BindingLease {
    pub idle_calls: u64,
}

/// The transcript facts of one pruned name.
#[derive(Clone, Debug)]
pub struct BindingPruneNotice {
    pub name: String,
    /// Epochs elapsed since last use at prune time (`>= lease.idle_calls`).
    pub idle_calls: u64,
    /// The pruned value's [`Value::type_name`](crate::types::Value::type_name),
    /// for the card.
    pub kind: &'static str,
}

/// The armed half of a [`BindingLedger`]: present only once a host has
/// called [`Shell::arm_binding_lease`](super::Shell::arm_binding_lease).
struct Armed {
    lease: BindingLease,
    /// The committed-turn clock. Ticks once per source-door turn.
    epoch: u64,
    /// Every name visible anywhere in the scope chain at arm time —
    /// prelude, agent library, rc bindings, host seed vars. A baseline name
    /// is never a candidate, forever (until the shell is rebuilt and
    /// re-armed). This also covers shadows: a model `let` that shadows a
    /// prelude name is itself baseline-named and therefore never pruned, so
    /// pruning can never un-shadow an older meaning.
    baseline: HashSet<String>,
    /// Leased candidates: name -> last-used epoch. An entry exists exactly
    /// for the non-baseline names installed at session scope since arming
    /// (enforced by the install chokepoint and self-healed by the prune
    /// verb's adoption sweep).
    last_used: HashMap<String, u64>,
}

/// Per-`Shell` binding-lease ledger. `Default` is the inert state: a host
/// that never arms it (REPL, batch, worker shells, pipeline children) pays
/// one branch per turn door and nothing else, and observes no expiry ever.
#[derive(Default)]
pub(crate) struct BindingLedger(Option<Armed>);

impl BindingLedger {
    /// Arm this ledger and seal `baseline` as permanently exempt. Idempotent
    /// by replacement: a re-arm discards any prior state and reseals.
    pub(crate) fn arm(&mut self, lease: BindingLease, baseline: impl IntoIterator<Item = String>) {
        self.0 = Some(Armed {
            lease,
            epoch: 0,
            baseline: baseline.into_iter().collect(),
            last_used: HashMap::new(),
        });
    }

    /// Whether a host has armed this ledger. A host that never calls
    /// [`Self::arm`] sees `false` forever.
    pub(crate) fn armed(&self) -> bool {
        self.0.is_some()
    }

    /// Advance the committed-turn clock by one. A no-op when unarmed.
    pub(crate) fn tick(&mut self) {
        if let Some(armed) = &mut self.0 {
            armed.epoch += 1;
        }
    }

    /// Install/rebind stamp. Baseline names are ignored; a candidate gets
    /// `last_used = epoch` — creation counts as use, and a rebind is a use,
    /// since writing a name is interest in it. A no-op when unarmed.
    pub(crate) fn note_install(&mut self, name: &str) {
        let Some(armed) = &mut self.0 else { return };
        if armed.baseline.contains(name) {
            return;
        }
        armed.last_used.insert(name.to_string(), armed.epoch);
    }

    /// Bump every already-tracked name in `names` to the current epoch.
    /// Names without an entry (baseline, builtins, undefined names) are
    /// ignored — renewal never *creates* a lease, so the harvest needs no
    /// filtering. A no-op when unarmed.
    pub(crate) fn renew<'a>(&mut self, names: impl IntoIterator<Item = &'a str>) {
        let Some(armed) = &mut self.0 else { return };
        let epoch = armed.epoch;
        for name in names {
            if let Some(last) = armed.last_used.get_mut(name) {
                *last = epoch;
            }
        }
    }

    /// [`Self::renew`]'s single-name sibling, for a dispatch-time touch
    /// (a runtime-resolved command head) rather than a batch harvest.
    pub(crate) fn renew_one(&mut self, name: &str) {
        let Some(armed) = &mut self.0 else { return };
        if let Some(last) = armed.last_used.get_mut(name) {
            *last = armed.epoch;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(idle_calls: u64) -> BindingLease {
        BindingLease { idle_calls }
    }

    #[test]
    fn unarmed_ledger_records_nothing() {
        let mut ledger = BindingLedger::default();
        assert!(!ledger.armed());
        ledger.tick();
        ledger.note_install("x");
        ledger.renew(["x"]);
        ledger.renew_one("x");
        assert!(!ledger.armed(), "an unarmed ledger stays unarmed");
    }

    #[test]
    fn baseline_names_are_never_candidates() {
        let mut ledger = BindingLedger::default();
        ledger.arm(lease(2), ["prelude_fn".to_string()]);
        // A rebind of a baseline name must never start a lease.
        ledger.note_install("prelude_fn");
        for _ in 0..10 {
            ledger.tick();
        }
        // If `prelude_fn` had somehow been tracked, it would now be
        // long-expired; the fact that pruning never reaches it is asserted
        // via the prune verb in parcel 4, but at this layer we can at least
        // check the entry never got created by re-arming with an empty
        // baseline and confirming a distinctly-named install *does* track.
        ledger.note_install("scratch");
        assert!(
            ledger.0.as_ref().unwrap().last_used.contains_key("scratch"),
            "a non-baseline install must be tracked"
        );
        assert!(
            !ledger
                .0
                .as_ref()
                .unwrap()
                .last_used
                .contains_key("prelude_fn"),
            "a baseline name must never gain a ledger entry"
        );
    }

    #[test]
    fn install_starts_lease_and_rebind_restamps() {
        let mut ledger = BindingLedger::default();
        ledger.arm(lease(4), []);
        ledger.note_install("x");
        assert_eq!(ledger.0.as_ref().unwrap().last_used["x"], 0);
        for _ in 0..3 {
            ledger.tick();
        }
        // A rebind at epoch 3 restamps to the current epoch.
        ledger.note_install("x");
        assert_eq!(ledger.0.as_ref().unwrap().last_used["x"], 3);
    }

    #[test]
    fn renew_bumps_only_existing() {
        let mut ledger = BindingLedger::default();
        ledger.arm(lease(4), []);
        ledger.note_install("tracked");
        for _ in 0..5 {
            ledger.tick();
        }
        ledger.renew(["tracked", "never_installed"]);
        assert_eq!(
            ledger.0.as_ref().unwrap().last_used["tracked"],
            5,
            "renew must bump a tracked name to the current epoch"
        );
        assert!(
            !ledger
                .0
                .as_ref()
                .unwrap()
                .last_used
                .contains_key("never_installed"),
            "renew must never create a lease for an untracked name"
        );
    }

    #[test]
    fn idle_arithmetic_expires_at_bound() {
        let mut ledger = BindingLedger::default();
        ledger.arm(lease(3), []);
        ledger.note_install("x");
        for _ in 0..2 {
            ledger.tick();
        }
        let armed = ledger.0.as_ref().unwrap();
        let idle = armed.epoch - armed.last_used["x"];
        assert_eq!(idle, 2, "two ticks since install is two idle calls");
        assert!(idle < armed.lease.idle_calls, "not yet at the bound");
        ledger.tick();
        let armed = ledger.0.as_ref().unwrap();
        let idle = armed.epoch - armed.last_used["x"];
        assert_eq!(idle, armed.lease.idle_calls, "exactly at the bound expires");
    }
}
