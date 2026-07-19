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
/// `idle_calls` epochs without use.
///
/// The epoch is the shell's committed-turn
/// clock ([`Shell::run_turn`](super::Shell::run_turn)'s source-arm tick),
/// never wall time. `large_binding_bytes` is a separate, orthogonal axis —
/// residency, not lifetime — read only by the install chokepoint's
/// large-binding check: an install whose
/// [`Value::shallow_size`](crate::types::Value::shallow_size) estimate
/// meets this threshold queues a [`LargeBindingNotice`], regardless of how
/// idle or fresh the name is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BindingLease {
    pub idle_calls: u64,
    pub large_binding_bytes: u64,
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

/// The transcript facts of one session-scope install whose shallow-size
/// estimate met [`BindingLease::large_binding_bytes`] at the moment it was
/// written
///
/// — a residency nudge, not a lease event: the binding itself is
/// completely untouched, and a rebind that still exceeds the threshold
/// queues another notice (`decisions/260629_agent-binding-reaping`).
#[derive(Clone, Debug)]
pub struct LargeBindingNotice {
    pub name: String,
    pub bytes: u64,
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
    /// Large-binding notices queued since the last drain — a plain `Vec`,
    /// not a registry: unlike the worker registry's `ReapNotice` ledger,
    /// there is no cross-thread producer here either, so the queue is just
    /// scratch, drained wholesale by the host's boundary reap.
    large_bindings: Vec<LargeBindingNotice>,
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
            large_bindings: Vec::new(),
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

    /// Names idle past the armed lease's bound, paired with the epochs
    /// elapsed since each was last used, in deterministic (sorted) name
    /// order — the prune verb's candidate list. Empty when unarmed.
    pub(crate) fn expired(&self) -> Vec<(String, u64)> {
        let Some(armed) = &self.0 else {
            return Vec::new();
        };
        let mut out: Vec<(String, u64)> = armed
            .last_used
            .iter()
            .filter_map(|(name, last)| {
                let idle = armed.epoch.saturating_sub(*last);
                (idle >= armed.lease.idle_calls).then(|| (name.clone(), idle))
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Drop `name`'s ledger entry without recording anything — the
    /// orphan-drop path (an install a panic rollback undid, leaving the
    /// entry behind on `LocalState`, untouched by the rollback) and the
    /// prune verb's own post-prune cleanup. A no-op when unarmed or when
    /// `name` carries no entry.
    pub(crate) fn drop_entry(&mut self, name: &str) {
        if let Some(armed) = &mut self.0 {
            armed.last_used.remove(name);
        }
    }

    /// Self-healing adoption: if `name` is neither baseline nor already
    /// tracked, start a fresh lease for it at the current epoch. Converts a
    /// name a future missed install path left untracked from "immortal
    /// zombie" into "leased from first sighting" — the conservative
    /// direction. A no-op when unarmed, when `name` is baseline, or when it
    /// already carries an entry (never resets an existing lease).
    pub(crate) fn adopt(&mut self, name: &str) {
        let Some(armed) = &mut self.0 else { return };
        if armed.baseline.contains(name) || armed.last_used.contains_key(name) {
            return;
        }
        armed.last_used.insert(name.to_string(), armed.epoch);
    }

    /// The armed lease, if any — read by the install chokepoint's
    /// large-binding check for `large_binding_bytes`. `None` when unarmed.
    pub(crate) fn lease(&self) -> Option<BindingLease> {
        self.0.as_ref().map(|armed| armed.lease)
    }

    /// Queue a large-binding notice. Unconditional (no de-duplication): a
    /// rebind that still meets the threshold queues another notice, since
    /// each offending install is its own fact. A no-op when unarmed.
    pub(crate) fn queue_large_binding_notice(&mut self, name: String, bytes: u64) {
        if let Some(armed) = &mut self.0 {
            armed
                .large_bindings
                .push(LargeBindingNotice { name, bytes });
        }
    }

    /// Drain every queued large-binding notice, leaving the queue empty.
    pub(crate) fn take_large_binding_notices(&mut self) -> Vec<LargeBindingNotice> {
        match &mut self.0 {
            Some(armed) => std::mem::take(&mut armed.large_bindings),
            None => Vec::new(),
        }
    }

    /// Number of names currently leased (tracked, non-baseline) — the
    /// `/resources` probe figure. Read-only: counting renews nothing, the
    /// same rule enumeration obeys everywhere else in this ledger. `0` when
    /// unarmed.
    pub(crate) fn leased_count(&self) -> usize {
        self.0.as_ref().map_or(0, |armed| armed.last_used.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(idle_calls: u64) -> BindingLease {
        BindingLease {
            idle_calls,
            large_binding_bytes: u64::MAX,
        }
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

/// Turn-level tests for the install chokepoint and the use-observation
/// harvest (`decisions/260629_agent-binding-reaping` parcels 2 and 3): every
/// persistent top-level install routes through
/// [`Shell::install_scope_binding`] and gets leased, while every
/// deeper-scope write is recorded nowhere; a committed turn's referenced
/// names renew already-leased entries at the three harvest seams
/// (`run_turn`'s own compiled program, `check_source`'s
/// runtime-compiled loads, `classify_command`'s `Resolution::Env` dispatch
/// touch). Driven through the public `run_turn` door, no exarch
/// involved — the same harness shape as `core/tests/top_level_vs_block.rs`.
#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod chokepoint_tests {
    use crate::driver::BakedPrelude;
    use crate::transport::{Program, Turn};
    use crate::types::{Capabilities, HandleState, Settled, Shell, Value};
    use crate::{RequestedTerminalAccess, TurnIo, TurnReport, TurnRequest, TurnStdin};
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    use super::BindingLease;

    /// The prelude baked once per test binary (no build-time blob inside
    /// core's own unit tests).
    fn prelude() -> &'static BakedPrelude {
        static P: OnceLock<BakedPrelude> = OnceLock::new();
        P.get_or_init(BakedPrelude::bake_runtime)
    }

    /// A shell booted with the real prelude and armed with `idle_calls` and
    /// an effectively-infinite large-binding threshold (never fires), the
    /// same seed-then-arm sequence exarch's `Agent::assemble` follows.
    fn armed_shell(idle_calls: u64) -> Shell {
        armed_shell_with(idle_calls, u64::MAX)
    }

    /// [`armed_shell`], with an explicit `large_binding_bytes` threshold for
    /// the large-binding tests below.
    fn armed_shell_with(idle_calls: u64, large_binding_bytes: u64) -> Shell {
        let mut shell = crate::driver::boot_shell(crate::io::TerminalState::default(), prelude(), Default::default());
        shell.arm_binding_lease(BindingLease {
            idle_calls,
            large_binding_bytes,
        });
        shell
    }

    /// Run one top-level turn through the public door. Every source below
    /// is expected to compile; a `Static` report is a test bug.
    fn top_level(shell: &mut Shell, source: &str) -> Settled<Value> {
        match shell.run_turn(TurnRequest {
            turn: Turn {
                program: Program::Source(source.into()),
                script_name: "<test>".into(),
                caps: Capabilities::root(),
                turn_limit: None,
                deferred_lease: None,
                worker_cap: None,
                io: TurnIo::Inherit,
                terminal: RequestedTerminalAccess::Leased,
                stdin: TurnStdin::Inherit,
            },
            surface: None,
            deferred: None,
            desk: None,
            nursery: None,
            lifecycle: Box::new(()),
        }) {
            TurnReport::Ran { result, .. } => result,
            TurnReport::Static { .. } => panic!("well-formed source must run: {source:?}"),
        }
    }

    /// Whether `name` carries a ledger entry — a leased candidate.
    fn is_leased(shell: &Shell, name: &str) -> bool {
        shell
            .local
            .bindings
            .0
            .as_ref()
            .is_some_and(|armed| armed.last_used.contains_key(name))
    }

    /// Whether `name` is sealed as a permanently-exempt baseline name.
    fn is_baseline(shell: &Shell, name: &str) -> bool {
        shell
            .local
            .bindings
            .0
            .as_ref()
            .is_some_and(|armed| armed.baseline.contains(name))
    }

    /// The ledger's committed-turn clock, for asserting the tick.
    fn epoch(shell: &Shell) -> u64 {
        shell.local.bindings.0.as_ref().expect("armed").epoch
    }

    /// `name`'s last-used epoch, for asserting renewal (or its absence).
    fn last_used_of(shell: &Shell, name: &str) -> u64 {
        shell.local.bindings.0.as_ref().expect("armed").last_used[name]
    }

    /// Idle out `name`'s lease relative to the current epoch by running
    /// `n` unrelated turns, so a later renewal is observable against a
    /// genuinely stale timestamp rather than one that happens to already
    /// equal the current epoch.
    fn idle_spin(shell: &mut Shell, n: u32) {
        for i in 0..n {
            top_level(shell, &format!("let _idle_spin_{i} = 0")).expect("idle spin");
        }
    }

    /// The `HandleState` of the handle bound to `name`. Panics if `name`
    /// isn't a `Value::Handle`.
    fn handle_state(shell: &Shell, name: &str) -> HandleState {
        match shell.scope_lookup(name) {
            Some(Value::Handle(h)) => *h.state.lock().unwrap(),
            other => panic!("{name} is not a handle: {other:?}"),
        }
    }

    #[test]
    fn top_level_let_is_leased() {
        let mut shell = armed_shell(64);
        top_level(&mut shell, "let top_scratch = 1").expect("top-level let");
        assert!(is_leased(&shell, "top_scratch"));
        assert!(!is_baseline(&shell, "top_scratch"));
    }

    #[test]
    fn destructured_components_are_leased() {
        let mut shell = armed_shell(64);
        top_level(
            &mut shell,
            "let [dpat_a, dpat_b, ...dpat_rest] = [1, 2, 3, 4]",
        )
        .expect("destructure");
        assert!(is_leased(&shell, "dpat_a"));
        assert!(is_leased(&shell, "dpat_b"));
        assert!(is_leased(&shell, "dpat_rest"));
    }

    #[test]
    fn block_local_let_is_not_leased() {
        let mut shell = armed_shell(64);
        top_level(&mut shell, "grant [exec: [:]] { let block_local = 1 }").expect("grant body");
        assert!(shell.scope_lookup("block_local").is_none());
        assert!(!is_leased(&shell, "block_local"));
        assert!(!is_baseline(&shell, "block_local"));
    }

    /// Two mutually-recursive top-level `let`s form one `LetRec` group
    /// (`syntax::group`'s SCC pre-pass); both names must be leased.
    #[test]
    fn letrec_group_is_leased() {
        let mut shell = armed_shell(64);
        top_level(
            &mut shell,
            concat!(
                "let even-lr = { |n acc| if $[$n > 10] { return $acc } else { odd-lr $[$n + 1] $[$acc + $n] } }\n",
                "let odd-lr  = { |n acc| if $[$n > 10] { return $acc } else { even-lr $[$n + 1] $acc } }\n",
            ),
        )
        .expect("mutually recursive definitions");
        assert!(is_leased(&shell, "even-lr"));
        assert!(is_leased(&shell, "odd-lr"));
    }

    /// `use`'s enclosing `let` is leased; the used file's own internal
    /// binding, installed at the pushed module scope, is recorded nowhere.
    #[test]
    fn use_module_internals_are_not_leased() {
        let mut shell = armed_shell(64);
        let path = std::env::temp_dir().join(format!(
            "ral_binding_lease_use_test_{}.ral",
            std::process::id()
        ));
        std::fs::write(&path, "let use_internal = 99\n").expect("write temp module");
        let p = path.to_string_lossy().into_owned();
        let result = top_level(&mut shell, &format!("let use_proj = use '{p}'"));
        std::fs::remove_file(&path).ok();
        result.expect("use");
        assert!(is_leased(&shell, "use_proj"));
        assert!(!is_leased(&shell, "use_internal"));
        assert!(!is_baseline(&shell, "use_internal"));
    }

    /// A prelude name and a host-seeded name (a host verb call preceding
    /// arming, mirroring `seed_session_dir`/rc `bindings:`) are both baseline
    /// — visible at arm time, so never lease candidates.
    #[test]
    fn prelude_and_host_seeds_are_baseline() {
        let mut shell = crate::driver::boot_shell(crate::io::TerminalState::default(), prelude(), Default::default());
        let (prelude_name, _) = shell
            .bindings()
            .into_iter()
            .next()
            .expect("the prelude seeds at least one binding");
        shell.set_var("host_seed".into(), Value::Int(1));
        shell.arm_binding_lease(BindingLease {
            idle_calls: 64,
            large_binding_bytes: u64::MAX,
        });
        assert!(is_baseline(&shell, &prelude_name));
        assert!(is_baseline(&shell, "host_seed"));
        assert!(!is_leased(&shell, &prelude_name));
        assert!(!is_leased(&shell, "host_seed"));
    }

    // ── use observation (parcel 3) ────────────────────────────────────────

    /// An ordinary expression referencing a stale name renews it to the
    /// turn's own epoch — the turn's-own-program harvest seam.
    #[test]
    fn turn_reference_renews_lease() {
        let mut shell = armed_shell(64);
        top_level(&mut shell, "let turn_ref_x = 1").expect("define");
        idle_spin(&mut shell, 3);
        let stale = last_used_of(&shell, "turn_ref_x");
        assert!(stale < epoch(&shell), "must be stale before the reference");
        top_level(&mut shell, "return $[$turn_ref_x + 1]").expect("reference");
        assert_eq!(
            last_used_of(&shell, "turn_ref_x"),
            epoch(&shell),
            "a referencing turn must renew to its own epoch"
        );
    }

    /// A turn that fails to typecheck ticks the clock (aging every leased
    /// name) but harvests nothing — `compile_turn` never returns a `Comp` to
    /// walk, so a stale name stays exactly as stale as it was.
    #[test]
    fn static_failure_ticks_but_renews_nothing() {
        let mut shell = armed_shell(64);
        top_level(&mut shell, "let static_x = 1").expect("define");
        idle_spin(&mut shell, 3);
        let epoch_before = epoch(&shell);
        let stale = last_used_of(&shell, "static_x");

        match shell.run_turn(TurnRequest {
            turn: Turn {
                program: Program::Source("$[1 + true]".into()),
                script_name: "<test>".into(),
                caps: Capabilities::root(),
                turn_limit: None,
                deferred_lease: None,
                worker_cap: None,
                io: TurnIo::Inherit,
                terminal: RequestedTerminalAccess::Leased,
                stdin: TurnStdin::Inherit,
            },
            surface: None,
            deferred: None,
            desk: None,
            nursery: None,
            lifecycle: Box::new(()),
        }) {
            TurnReport::Static { .. } => {}
            TurnReport::Ran { .. } => panic!("ill-typed source must not run"),
        }

        assert_eq!(
            epoch(&shell),
            epoch_before + 1,
            "a failed turn still ticks the clock"
        );
        assert_eq!(
            last_used_of(&shell, "static_x"),
            stale,
            "a failed turn must renew nothing"
        );
    }

    /// A registered hook run through `run_turn`'s `Program::Hook` is not a
    /// tool call: it ticks no epoch and renews nothing, even though its body
    /// reads a name the turn's own harvest would otherwise have caught.
    #[test]
    fn hook_door_neither_ticks_nor_renews() {
        let mut shell = armed_shell(64);
        top_level(&mut shell, "let hook_x = 1").expect("define");
        top_level(&mut shell, "let hook_thunk = { $hook_x }").expect("define the hook body");
        idle_spin(&mut shell, 3);
        let epoch_before = epoch(&shell);
        let stale = last_used_of(&shell, "hook_x");

        let thunk = shell
            .scope_lookup("hook_thunk")
            .cloned()
            .expect("hook_thunk must be bound");
        shell
            .register_hook(
                crate::types::HookName::session("test_prompt"),
                thunk,
                crate::types::HookSig::Prompt,
                crate::types::DefaultPolicy::denied(),
                crate::source::Span {
                    start: 0,
                    end: 0,
                    file: crate::source::FileId::DUMMY,
                },
            )
            .expect("register the hook");

        let report = shell.run_turn(TurnRequest {
            turn: Turn {
                program: Program::Hook {
                    name: crate::types::HookName::session("test_prompt"),
                    args: vec![],
                },
                script_name: "<test>".into(),
                caps: Capabilities::root(),
                turn_limit: None,
                deferred_lease: None,
                worker_cap: None,
                io: TurnIo::Inherit,
                terminal: RequestedTerminalAccess::Leased,
                stdin: TurnStdin::Inherit,
            },
            surface: None,
            deferred: None,
            desk: None,
            nursery: None,
            lifecycle: Box::new(()),
        });
        match report {
            TurnReport::Ran { result, .. } => {
                result.expect("the hook body must run");
            }
            TurnReport::Static { .. } => panic!("the registered hook must run"),
        }

        assert_eq!(
            epoch(&shell),
            epoch_before,
            "a hook must not tick the clock"
        );
        assert_eq!(
            last_used_of(&shell, "hook_x"),
            stale,
            "a hook must renew nothing, even a name its body reads"
        );
    }

    /// `remove_plugin_hooks` drops exactly the hooks in one plugin's
    /// namespace — the mechanism a failed plugin load rolls back through and
    /// an unload reverses through — leaving other plugins' and the session's
    /// hooks untouched.  `unregister_hook` drops a single named hook.
    #[test]
    fn plugin_hooks_removed_by_namespace() {
        use crate::types::{DefaultPolicy, HookName, HookSig};
        let span = crate::source::Span {
            start: 0,
            end: 0,
            file: crate::source::FileId::DUMMY,
        };
        let mut shell = armed_shell(64);
        top_level(&mut shell, "let body = { 1 }").expect("define a hook body");
        let thunk = shell
            .scope_lookup("body")
            .cloned()
            .expect("body must be bound");

        let reg = |shell: &mut Shell, name: HookName| {
            shell
                .register_hook(name, thunk.clone(), HookSig::Prompt, DefaultPolicy::denied(), span)
                .expect("register");
        };
        reg(&mut shell, HookName::plugin("p", "prompt"));
        reg(&mut shell, HookName::plugin("p", "factory"));
        reg(&mut shell, HookName::plugin("q", "prompt"));
        reg(&mut shell, HookName::session("startup"));

        // A single-name drop removes just that hook.
        assert!(shell.unregister_hook(&HookName::plugin("p", "factory")));
        assert!(!shell.has_hook(&HookName::plugin("p", "factory")));
        assert!(!shell.unregister_hook(&HookName::plugin("p", "factory")));

        // A namespace sweep removes the rest of plugin `p`, nothing else.
        assert_eq!(shell.remove_plugin_hooks("p"), 1);
        assert!(!shell.has_hook(&HookName::plugin("p", "prompt")));
        assert!(shell.has_hook(&HookName::plugin("q", "prompt")));
        assert!(shell.has_hook(&HookName::session("startup")));
    }

    /// A `source`d file referencing a name that exists only in the caller's
    /// scope — never mentioned anywhere in the outer turn's own compiled
    /// program — is still renewed: the `check_source` harvest seam, not the
    /// turn's-own-program seam, is what catches it here.
    #[test]
    fn sourced_module_reference_renews() {
        let mut shell = armed_shell(64);
        top_level(&mut shell, "let sourced_ref_x = 1").expect("define");
        idle_spin(&mut shell, 3);
        let stale = last_used_of(&shell, "sourced_ref_x");
        assert!(stale < epoch(&shell), "must be stale before the source");

        let path = std::env::temp_dir().join(format!(
            "ral_binding_lease_source_test_{}.ral",
            std::process::id()
        ));
        std::fs::write(&path, "let sourced_helper = $sourced_ref_x\n").expect("write temp module");
        let p = path.to_string_lossy().into_owned();
        let result = top_level(&mut shell, &format!("source '{p}'"));
        std::fs::remove_file(&path).ok();
        result.expect("source");

        assert_eq!(
            last_used_of(&shell, "sourced_ref_x"),
            epoch(&shell),
            "a sourced file's own reference must renew via check_source"
        );
    }

    /// A name installed by a runtime mechanism the elaborator could not
    /// see — here, `source` — compiles its later bare-word reference as an
    /// ordinary `Exec` rather than `App`; `classify_command`'s
    /// `Resolution::Env` arm still renews it at dispatch time.
    #[test]
    fn env_resolved_command_head_renews() {
        let mut shell = armed_shell(64);
        let path = std::env::temp_dir().join(format!(
            "ral_binding_lease_env_resolved_test_{}.ral",
            std::process::id()
        ));
        std::fs::write(&path, "let env_resolved_fn = { |x| $[$x + 1] }\n")
            .expect("write temp module");
        let p = path.to_string_lossy().into_owned();
        top_level(&mut shell, &format!("source '{p}'")).expect("source");
        idle_spin(&mut shell, 3);
        let stale = last_used_of(&shell, "env_resolved_fn");
        assert!(stale < epoch(&shell), "must be stale before the call");

        let result = top_level(&mut shell, "env_resolved_fn 41");
        std::fs::remove_file(&path).ok();
        result.expect("call the sourced function by bare command head");

        assert_eq!(
            last_used_of(&shell, "env_resolved_fn"),
            epoch(&shell),
            "the Resolution::Env dispatch touch must renew the resolved name"
        );
    }

    /// A name mentioned only inside a double-quoted interpolated string
    /// renews — exercising the walker's `CompKind::Interpolation` arm
    /// end-to-end through a real turn.
    #[test]
    fn interpolated_reference_renews() {
        let mut shell = armed_shell(64);
        top_level(&mut shell, "let interp_x = 1").expect("define");
        idle_spin(&mut shell, 3);
        let stale = last_used_of(&shell, "interp_x");
        assert!(stale < epoch(&shell), "must be stale before the reference");
        top_level(&mut shell, "return \"value is $interp_x\"").expect("interpolate");
        assert_eq!(
            last_used_of(&shell, "interp_x"),
            epoch(&shell),
            "an interpolated reference must renew"
        );
    }

    // ── the prune verb (parcel 4) ──────────────────────────────────────────

    /// A pruned name is gone from scope *and* from the next turn's type
    /// seed: `unset` drops the `Binding` (value and scheme together) in one
    /// act, so a later reference reads as an ordinary undefined variable —
    /// core's unmodified runtime diagnostic (`eval_val`'s `Val::Variable`
    /// arm), not a stale-scheme surprise. An unbound name is not a static
    /// type error in this language (the checker admits any reference,
    /// scheme or not; only evaluation resolves it), so the pruned-name
    /// symptom is this `Ran` error, not a `Static` diagnostic.
    #[test]
    fn prune_removes_name_and_type_seed() {
        let mut shell = armed_shell(2);
        top_level(&mut shell, "let prune_x = 1").expect("define");
        idle_spin(&mut shell, 2);

        let (notices, _snapshot) = shell
            .prune_idle_bindings()
            .expect("prune_x must be idle enough to prune");
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].name, "prune_x");
        assert_eq!(notices[0].kind, "Int");
        assert!(shell.scope_lookup("prune_x").is_none());

        match shell.run_turn(TurnRequest {
            turn: Turn {
                program: Program::Source("return $prune_x".into()),
                script_name: "<test>".into(),
                caps: Capabilities::root(),
                turn_limit: None,
                deferred_lease: None,
                worker_cap: None,
                io: TurnIo::Inherit,
                terminal: RequestedTerminalAccess::Leased,
                stdin: TurnStdin::Inherit,
            },
            surface: None,
            deferred: None,
            desk: None,
            nursery: None,
            lifecycle: Box::new(()),
        }) {
            TurnReport::Ran { result, .. } => {
                let err = result.expect_err("a pruned name must read as undefined");
                let msg = match err {
                    crate::types::Break::Error(e) => e.message,
                    other @ crate::types::Break::Escape(_) => {
                        panic!("expected an Error break, got {other:?}")
                    }
                };
                assert!(
                    msg.contains("undefined variable: $prune_x"),
                    "expected an undefined-variable diagnostic, got: {msg}"
                );
            }
            TurnReport::Static { .. } => {
                panic!("an unbound variable reference is a runtime error, not a static one")
            }
        }
    }

    /// A running worker pins its name: the first prune, while `sleep`
    /// still runs, yields no notice at all and leaves the entry untouched.
    /// Cancelling settles the handle; idling it out again, the second
    /// prune now removes it like any ordinary scratch value.
    #[test]
    fn running_handle_pins_name_then_settles_then_prunes() {
        let mut shell = armed_shell(2);
        top_level(&mut shell, "let h_pin = !{spawn { sleep 10 }}").expect("spawn");
        idle_spin(&mut shell, 2);
        assert_eq!(handle_state(&shell, "h_pin"), HandleState::Running);

        assert!(
            shell.prune_idle_bindings().is_none(),
            "a running handle must not be pruned"
        );
        assert!(shell.scope_lookup("h_pin").is_some());

        top_level(&mut shell, "cancel $h_pin").expect("cancel");
        let deadline = Instant::now() + Duration::from_secs(5);
        while handle_state(&shell, "h_pin") == HandleState::Running {
            assert!(
                Instant::now() < deadline,
                "the cancelled worker must settle within the budget"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // The `cancel $h_pin` turn itself referenced (and so renewed)
        // h_pin; idle it back out before pruning again.
        idle_spin(&mut shell, 2);

        let (notices, _snapshot) = shell
            .prune_idle_bindings()
            .expect("a settled handle must now prune");
        assert_eq!(notices[0].name, "h_pin");
    }

    /// A handle that settles on its own (never pinned at all) is ordinary
    /// scratch once idle — no special casing beyond the pin check.
    #[test]
    fn settled_handle_is_ordinary_scratch() {
        let mut shell = armed_shell(2);
        top_level(&mut shell, "let h_settled = !{spawn { return 1 }}").expect("spawn");
        let deadline = Instant::now() + Duration::from_secs(5);
        while handle_state(&shell, "h_settled") == HandleState::Running {
            assert!(
                Instant::now() < deadline,
                "the instant worker must settle within the budget"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        idle_spin(&mut shell, 2);

        let (notices, _snapshot) = shell
            .prune_idle_bindings()
            .expect("a settled handle prunes like any scratch");
        assert_eq!(notices[0].name, "h_settled");
        assert_eq!(notices[0].kind, "Handle");
    }

    /// A ledger entry whose install a panic rollback undid — simulated here
    /// with the same public checkpoint/restore pair exarch's panic recovery
    /// uses — is dropped silently at the next prune: no notice, no error.
    #[test]
    fn orphan_entry_dropped_silently() {
        let mut shell = armed_shell(2);
        let pre = shell.mobile_snapshot();
        top_level(&mut shell, "let orphan_x = 1").expect("define");
        assert!(is_leased(&shell, "orphan_x"));

        shell.restore_mobile(pre);
        assert!(
            shell.scope_lookup("orphan_x").is_none(),
            "the rollback must remove it from scope"
        );
        assert!(
            is_leased(&shell, "orphan_x"),
            "the ledger entry is not part of the rolled-back Mobile — it orphans"
        );

        idle_spin(&mut shell, 2);
        let result = shell.prune_idle_bindings();
        assert!(
            result.is_none(),
            "an orphan-only sweep drops the entry silently and yields no notice"
        );
        assert!(!is_leased(&shell, "orphan_x"), "the orphan must be gone");
    }

    /// A name a host verb wrote directly (bypassing the install chokepoint,
    /// as every host verb call does) is neither baseline nor tracked. The
    /// adoption sweep — which runs on every prune pass, even one that
    /// prunes nothing — leases it from this sighting forward rather than
    /// leaving it an immortal untracked name.
    #[test]
    fn stray_untracked_name_is_adopted_not_pruned() {
        let mut shell = armed_shell(2);
        shell.set_var("stray_y".into(), Value::Int(1));
        assert!(!is_leased(&shell, "stray_y"));
        assert!(!is_baseline(&shell, "stray_y"));

        idle_spin(&mut shell, 1);
        let epoch_at_sweep = epoch(&shell);
        let _ = shell.prune_idle_bindings();

        assert!(
            shell.scope_lookup("stray_y").is_some(),
            "adoption must never prune — only start tracking"
        );
        assert_eq!(
            last_used_of(&shell, "stray_y"),
            epoch_at_sweep,
            "adopted at the sweep's own epoch, a fresh lease starting now"
        );
    }

    /// A caller not at session scope — a mid-frame lifecycle-hook-shaped
    /// caller — is refused outright: `None`, not a partial or unsafe prune.
    #[test]
    fn prune_refused_mid_frame() {
        let mut shell = armed_shell(2);
        top_level(&mut shell, "let midframe_x = 1").expect("define");
        idle_spin(&mut shell, 2);

        shell.mobile.scope.push_scope();
        assert!(
            shell.prune_idle_bindings().is_none(),
            "a mid-frame caller must be refused, not partially served"
        );
        shell.mobile.scope.pop_scope();

        assert!(
            shell.prune_idle_bindings().is_some(),
            "back at session scope, the same idle name prunes normally"
        );
    }

    /// The snapshot the prune verb returns is post-prune: restoring it
    /// later — even after further turns run — must not resurrect the
    /// pruned name. This is the structural half of panic-recovery safety:
    /// a caller cannot obtain the notices without the checkpoint that
    /// makes them permanent.
    #[test]
    fn checkpoint_restores_post_prune_state() {
        let mut shell = armed_shell(2);
        top_level(&mut shell, "let checkpoint_x = 1").expect("define");
        idle_spin(&mut shell, 2);

        let (_notices, post_prune) = shell.prune_idle_bindings().expect("must prune");
        assert!(shell.scope_lookup("checkpoint_x").is_none());

        top_level(&mut shell, "let after_prune_y = 2").expect("further turns run");
        shell.restore_mobile(post_prune);
        assert!(
            shell.scope_lookup("checkpoint_x").is_none(),
            "the returned checkpoint is post-prune — restoring it cannot resurrect the name"
        );
    }

    /// Nothing idle enough yet: the verb is a clean no-op.
    #[test]
    fn nothing_expired_returns_none() {
        let mut shell = armed_shell(64);
        top_level(&mut shell, "let fresh_z = 1").expect("define");
        assert!(
            shell.prune_idle_bindings().is_none(),
            "nothing is idle enough to prune yet"
        );
    }

    // ── the large-binding warning (parcel 6) ────────────────────────────────

    /// A session-scope install meeting the threshold queues exactly one
    /// notice naming the binding and its estimate; draining leaves the
    /// queue empty until the next offending install. A rebind that still
    /// meets the threshold queues another notice — no de-duplication.
    #[test]
    fn large_binding_threshold_queues_one_notice() {
        let mut shell = armed_shell_with(64, 8);
        let text = "this string is definitely over eight bytes long";
        top_level(&mut shell, &format!("let large_x = '{text}'")).expect("define");

        let notices = shell.take_large_binding_notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].name, "large_x");
        assert_eq!(notices[0].bytes, text.len() as u64);

        assert!(
            shell.take_large_binding_notices().is_empty(),
            "the drain must emit exactly one notice per offending install"
        );

        let text2 = "a different string that also clears eight bytes";
        top_level(&mut shell, &format!("let large_x = '{text2}'")).expect("rebind");
        let notices2 = shell.take_large_binding_notices();
        assert_eq!(
            notices2.len(),
            1,
            "a rebind that still exceeds the threshold must warn again"
        );
        assert_eq!(notices2[0].name, "large_x");
    }

    /// An install whose estimate falls short of the threshold queues
    /// nothing at all.
    #[test]
    fn sub_threshold_install_queues_nothing() {
        let mut shell = armed_shell_with(64, 1_000_000);
        top_level(&mut shell, "let small_x = 1").expect("define");
        assert!(
            shell.take_large_binding_notices().is_empty(),
            "an install under the threshold must queue no notice"
        );
    }
}
