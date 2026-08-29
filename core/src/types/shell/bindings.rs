//! The binding-lease ledger: a per-[`Shell`](super::Shell) policy that lets
//! an agent host expire an idle top-level scratch name, leaving core's
//! lexical semantics untouched everywhere else.
//!
//! Deliberately unlocked, unlike the
//! [`WorkerRegistry`](super::workers::WorkerRegistry) beside it on
//! [`LocalState`](super::LocalState): every touch arrives through
//! `&mut Shell` on the one thread driving that `Agent`'s attend loop — slash
//! commands included, since they reach it as posts handled inside that loop
//! rather than on exarch's render thread — and the daemon thread firing a
//! worker's idle lease takes only the worker registry's own lock. Let a
//! second thread ever reach `&mut Shell` for a live agent and this unlocked
//! design is the first thing that must change.

use std::collections::{HashMap, HashSet};

/// Host-stated per-agent policy.
///
/// A leased name expires after `idle_calls` epochs without use, an epoch
/// being the shell's committed-run clock ([`Shell::run`](super::Shell::run)'s
/// source-arm tick), never wall time.
///
/// `large_binding_bytes` is an orthogonal axis — residency, not lifetime:
/// an install whose
/// [`Value::shallow_size`](crate::types::Value::shallow_size) estimate meets
/// it queues a [`LargeBindingNotice`], however fresh the name.
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
    /// The pruned value's [`Value::type_name`](crate::types::Value::type_name).
    pub kind: &'static str,
}

/// The transcript facts of one session-scope install whose shallow-size
/// estimate met [`BindingLease::large_binding_bytes`] as it was written.
///
/// This is a residency nudge that leaves the binding wholly untouched,
/// re-queued by every rebind still over the threshold.
#[derive(Clone, Debug)]
pub struct LargeBindingNotice {
    pub name: String,
    pub bytes: u64,
}

/// The armed half of a [`BindingLedger`]: present only once a host has
/// called [`Shell::arm_binding_lease`](super::Shell::arm_binding_lease).
struct Armed {
    lease: BindingLease,
    /// The committed-run clock. Ticks once per source-door run.
    epoch: u64,
    /// Every name visible anywhere in the scope chain at arm time — prelude,
    /// agent library, rc bindings, host seed vars — never a candidate. This
    /// covers shadows too: a model `let` over a prelude name is itself
    /// baseline-named, so pruning can never un-shadow an older meaning.
    baseline: HashSet<String>,
    /// Leased candidates: name -> last-used epoch, one entry per non-baseline
    /// name installed at session scope since arming.
    last_used: HashMap<String, u64>,
    large_bindings: Vec<LargeBindingNotice>,
}

/// Per-`Shell` binding-lease ledger. `Default` is the inert state: a host
/// that never arms it (REPL, batch, worker shells, pipeline children) pays a
/// branch per run door and observes no expiry ever.
#[derive(Default)]
pub(crate) struct BindingLedger(Option<Armed>);

impl BindingLedger {
    /// Arm this ledger and seal `baseline` as permanently exempt. Idempotent
    /// by replacement: a re-arm discards all prior state and reseals.
    pub(crate) fn arm(&mut self, lease: BindingLease, baseline: impl IntoIterator<Item = String>) {
        self.0 = Some(Armed {
            lease,
            epoch: 0,
            baseline: baseline.into_iter().collect(),
            last_used: HashMap::new(),
            large_bindings: Vec::new(),
        });
    }

    pub(crate) fn armed(&self) -> bool {
        self.0.is_some()
    }

    /// Advance the committed-run clock by one.
    pub(crate) fn tick(&mut self) {
        if let Some(armed) = &mut self.0 {
            armed.epoch += 1;
        }
    }

    /// Stamp an install or rebind: baseline names are ignored, a candidate
    /// gets the current epoch — writing a name is itself interest in it.
    pub(crate) fn note_install(&mut self, name: &str) {
        let Some(armed) = &mut self.0 else { return };
        if armed.baseline.contains(name) {
            return;
        }
        armed.last_used.insert(name.to_string(), armed.epoch);
    }

    /// Bump every already-tracked name in `names` to the current epoch.
    /// Renewal never *creates* a lease, so a caller may hand over every name
    /// a run referenced unfiltered.
    pub(crate) fn renew<'a>(&mut self, names: impl IntoIterator<Item = &'a str>) {
        let Some(armed) = &mut self.0 else { return };
        let epoch = armed.epoch;
        for name in names {
            if let Some(last) = armed.last_used.get_mut(name) {
                *last = epoch;
            }
        }
    }

    /// [`Self::renew`]'s single-name sibling, for `classify_command`'s touch
    /// of a runtime-resolved command head rather than a batch harvest.
    pub(crate) fn renew_one(&mut self, name: &str) {
        let Some(armed) = &mut self.0 else { return };
        if let Some(last) = armed.last_used.get_mut(name) {
            *last = armed.epoch;
        }
    }

    /// Names idle past the lease's bound with the epochs each has been idle,
    /// in sorted name order so a prune pass is deterministic.
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

    /// Drop `name`'s entry with no notice: the orphan path (an install a
    /// panic rollback undid, leaving the entry behind on `LocalState`, which
    /// the rollback does not restore) and the prune verb's own cleanup.
    pub(crate) fn drop_entry(&mut self, name: &str) {
        if let Some(armed) = &mut self.0 {
            armed.last_used.remove(name);
        }
    }

    /// Self-healing: a name neither baseline nor tracked starts a fresh lease
    /// now, turning whatever a missed install path left behind from immortal
    /// into leased-from-first-sighting. Never resets an existing lease.
    pub(crate) fn adopt(&mut self, name: &str) {
        let Some(armed) = &mut self.0 else { return };
        if armed.baseline.contains(name) || armed.last_used.contains_key(name) {
            return;
        }
        armed.last_used.insert(name.to_string(), armed.epoch);
    }

    /// The armed lease — read by `Shell::install_scope_binding` for its
    /// large-binding check.
    pub(crate) fn lease(&self) -> Option<BindingLease> {
        self.0.as_ref().map(|armed| armed.lease)
    }

    /// Queue a notice. No de-duplication: each offending install is its own
    /// fact, so a rebind still over the threshold queues another.
    pub(crate) fn queue_large_binding_notice(&mut self, name: String, bytes: u64) {
        if let Some(armed) = &mut self.0 {
            armed
                .large_bindings
                .push(LargeBindingNotice { name, bytes });
        }
    }

    pub(crate) fn take_large_binding_notices(&mut self) -> Vec<LargeBindingNotice> {
        match &mut self.0 {
            Some(armed) => std::mem::take(&mut armed.large_bindings),
            None => Vec::new(),
        }
    }

    /// Names currently leased. Counting renews nothing, the rule every
    /// enumeration in this ledger obeys.
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
        // A rebind of a baseline name must never start a lease; ten ticks
        // would have long expired it, had it ever been tracked.
        ledger.note_install("prelude_fn");
        for _ in 0..10 {
            ledger.tick();
        }
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

/// Run-level tests for the install chokepoint and the use-observation
/// harvest: every persistent top-level install routes through
/// `Shell::install_scope_binding` and gets leased while deeper-scope writes
/// are recorded nowhere, and a committed run's referenced names renew at all
/// three harvest seams — `run`'s own compiled program, `check_source`'s
/// runtime-compiled loads, `classify_command`'s `Resolution::Env` dispatch
/// touch. Driven through the public `run` door, no exarch involved: the same
/// harness shape as `core/tests/top_level_vs_block.rs`.
#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod chokepoint_tests {
    use crate::boot::BakedPrelude;
    use crate::protocol::{Program, Run};
    use crate::types::{Capabilities, HandleState, Settled, Shell, Value};
    use crate::{RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin};
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    use super::BindingLease;

    /// The prelude baked once per test binary — core's own unit tests get no
    /// build-time blob.
    fn prelude() -> &'static BakedPrelude {
        static P: OnceLock<BakedPrelude> = OnceLock::new();
        P.get_or_init(BakedPrelude::bake_runtime)
    }

    /// A booted, armed shell whose large-binding threshold never fires.
    fn armed_shell(idle_calls: u64) -> Shell {
        armed_shell_with(idle_calls, u64::MAX)
    }

    /// [`armed_shell`] with an explicit threshold — the same seed-then-arm
    /// order exarch's `Agent::assemble` follows.
    fn armed_shell_with(idle_calls: u64, large_binding_bytes: u64) -> Shell {
        let mut shell = crate::boot::boot_shell(
            crate::io::TerminalState::default(),
            prelude(),
            &crate::boot::HostSurface::default(),
        );
        shell.arm_binding_lease(BindingLease {
            idle_calls,
            large_binding_bytes,
        });
        shell
    }

    /// One top-level run through the public door. Every source below must
    /// compile; a `Static` report is a test bug.
    fn top_level(shell: &mut Shell, source: &str) -> Settled<Value> {
        match shell.run(RunRequest {
            run: Run {
                program: Program::Source(source.into()),
                script_name: "<test>".into(),
                caps: Capabilities::root(),
                wall: None,
                deferred_lease: None,
                worker_cap: None,
                io: RunIo::Inherit,
                terminal: RequestedTerminalAccess::Leased,
                stdin: RunStdin::Inherit,
                trail: None,
            },
            surface: None,
            deferred: None,
            desk: None,
            fork: None,
            lifecycle: Box::new(()),
        }) {
            RunReport::Ran { ending, .. } => ending.into_result(),
            RunReport::Static { .. } => panic!("well-formed source must run: {source:?}"),
        }
    }

    /// One captured run's stderr — where the ready boundary writes the
    /// large-binding warning.
    fn top_level_stderr(shell: &mut Shell, source: &str) -> String {
        match shell.run(RunRequest {
            run: Run {
                program: Program::Source(source.into()),
                script_name: "<test>".into(),
                caps: Capabilities::root(),
                wall: None,
                deferred_lease: None,
                worker_cap: None,
                io: RunIo::Capture,
                terminal: RequestedTerminalAccess::Leased,
                stdin: RunStdin::Inherit,
                trail: None,
            },
            surface: None,
            deferred: None,
            desk: None,
            fork: None,
            lifecycle: Box::new(()),
        }) {
            RunReport::Ran { captured, .. } => {
                let captured = captured.expect("Capture must return buffers");
                String::from_utf8_lossy(&captured.stderr).into_owned()
            }
            RunReport::Static { .. } => panic!("well-formed source must run: {source:?}"),
        }
    }

    fn is_leased(shell: &Shell, name: &str) -> bool {
        shell
            .local
            .bindings
            .0
            .as_ref()
            .is_some_and(|armed| armed.last_used.contains_key(name))
    }

    fn is_baseline(shell: &Shell, name: &str) -> bool {
        shell
            .local
            .bindings
            .0
            .as_ref()
            .is_some_and(|armed| armed.baseline.contains(name))
    }

    fn epoch(shell: &Shell) -> u64 {
        shell.local.bindings.0.as_ref().expect("armed").epoch
    }

    fn last_used_of(shell: &Shell, name: &str) -> u64 {
        shell.local.bindings.0.as_ref().expect("armed").last_used[name]
    }

    /// Age every lease by `n` unrelated runs, so a later renewal is measured
    /// against a genuinely stale timestamp rather than the current epoch.
    fn idle_spin(shell: &mut Shell, n: u32) {
        for i in 0..n {
            top_level(shell, &format!("let _idle_spin_{i} = 0")).expect("idle spin");
        }
    }

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
    /// (`syntax::group`'s SCC pre-pass), and both names must still be leased.
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

    /// A prelude name and a host-seeded one (a host verb call preceding
    /// arming, as exarch's `seed_var` and rc `bindings:` make) are alike
    /// visible at arm time, so alike baseline.
    #[test]
    fn prelude_and_host_seeds_are_baseline() {
        let mut shell = crate::boot::boot_shell(
            crate::io::TerminalState::default(),
            prelude(),
            &crate::boot::HostSurface::default(),
        );
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

    // ── use observation ───────────────────────────────────────────────────

    /// An ordinary expression referencing a stale name renews it to the
    /// run's own epoch — the run's-own-program harvest seam.
    #[test]
    fn run_reference_renews_lease() {
        let mut shell = armed_shell(64);
        top_level(&mut shell, "let run_ref_x = 1").expect("define");
        idle_spin(&mut shell, 3);
        let stale = last_used_of(&shell, "run_ref_x");
        assert!(stale < epoch(&shell), "must be stale before the reference");
        top_level(&mut shell, "return $[$run_ref_x + 1]").expect("reference");
        assert_eq!(
            last_used_of(&shell, "run_ref_x"),
            epoch(&shell),
            "a referencing run must renew to its own epoch"
        );
    }

    /// A run that fails to typecheck still ages every lease but harvests
    /// nothing: `compile_run` returns no `Comp` to walk.
    #[test]
    fn static_failure_ticks_but_renews_nothing() {
        let mut shell = armed_shell(64);
        top_level(&mut shell, "let static_x = 1").expect("define");
        idle_spin(&mut shell, 3);
        let epoch_before = epoch(&shell);
        let stale = last_used_of(&shell, "static_x");

        match shell.run(RunRequest {
            run: Run {
                program: Program::Source("$[1 + true]".into()),
                script_name: "<test>".into(),
                caps: Capabilities::root(),
                wall: None,
                deferred_lease: None,
                worker_cap: None,
                io: RunIo::Inherit,
                terminal: RequestedTerminalAccess::Leased,
                stdin: RunStdin::Inherit,
                trail: None,
            },
            surface: None,
            deferred: None,
            desk: None,
            fork: None,
            lifecycle: Box::new(()),
        }) {
            RunReport::Static { .. } => {}
            RunReport::Ran { .. } => panic!("ill-typed source must not run"),
        }

        assert_eq!(
            epoch(&shell),
            epoch_before + 1,
            "a failed run still ticks the clock"
        );
        assert_eq!(
            last_used_of(&shell, "static_x"),
            stale,
            "a failed run must renew nothing"
        );
    }

    /// `run`'s `Program::Hook` arm is not a tool call: it ticks no epoch and
    /// renews nothing, even a name its body reads.
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

        let report = shell.run(RunRequest {
            run: Run {
                program: Program::Hook {
                    name: crate::types::HookName::session("test_prompt"),
                    args: vec![],
                },
                script_name: "<test>".into(),
                caps: Capabilities::root(),
                wall: None,
                deferred_lease: None,
                worker_cap: None,
                io: RunIo::Inherit,
                terminal: RequestedTerminalAccess::Leased,
                stdin: RunStdin::Inherit,
                trail: None,
            },
            surface: None,
            deferred: None,
            desk: None,
            fork: None,
            lifecycle: Box::new(()),
        });
        match report {
            RunReport::Ran { ending, .. } => {
                ending.into_result().expect("the hook body must run");
            }
            RunReport::Static { .. } => panic!("the registered hook must run"),
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

    /// `remove_plugin_hooks` drops exactly one plugin's namespace — what a
    /// failed load rolls back through and an unload reverses through —
    /// leaving other plugins' and the session's hooks alone.
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
                .register_hook(
                    name,
                    thunk.clone(),
                    HookSig::Prompt,
                    DefaultPolicy::denied(),
                    span,
                )
                .expect("register");
        };
        reg(&mut shell, HookName::plugin("p", "prompt"));
        reg(&mut shell, HookName::plugin("p", "factory"));
        reg(&mut shell, HookName::plugin("q", "prompt"));
        reg(&mut shell, HookName::session("startup"));

        assert!(shell.unregister_hook(&HookName::plugin("p", "factory")));
        assert!(!shell.has_hook(&HookName::plugin("p", "factory")));
        assert!(!shell.unregister_hook(&HookName::plugin("p", "factory")));

        assert_eq!(shell.remove_plugin_hooks("p"), 1);
        assert!(!shell.has_hook(&HookName::plugin("p", "prompt")));
        assert!(shell.has_hook(&HookName::plugin("q", "prompt")));
        assert!(shell.has_hook(&HookName::session("startup")));
    }

    /// A `source`d file's reference to a caller-scope name the outer run's
    /// own program never mentions still renews — the `check_source` seam,
    /// not the run's-own-program seam, catches this one.
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

    /// A name installed where the elaborator cannot see it — here by `source`
    /// — compiles its later bare-word reference as an `Exec`, not an `App`,
    /// so only `classify_command`'s `Resolution::Env` arm can renew it.
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

    /// A name mentioned only inside a double-quoted string renews — the
    /// walker's `CompKind::Interpolation` arm, through a real run.
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

    // ── the prune verb ─────────────────────────────────────────────────────

    /// A pruned name leaves scope *and* the next run's type seed together —
    /// `unset` drops the whole `Binding`, value and scheme in one act — so a
    /// later reference is an ordinary undefined variable, not a stale-scheme
    /// surprise. The checker admits any reference and only evaluation
    /// resolves it, so the symptom is a `Ran` error, not a `Static` one.
    #[test]
    fn prune_removes_name_and_type_seed() {
        let mut shell = armed_shell(2);
        top_level(&mut shell, "let prune_x = 1").expect("define");
        idle_spin(&mut shell, 2);

        let notices = shell.prune_idle_bindings();
        assert_eq!(notices.len(), 1, "prune_x must be idle enough to prune");
        assert_eq!(notices[0].name, "prune_x");
        assert_eq!(notices[0].kind, "Int");
        assert!(shell.scope_lookup("prune_x").is_none());

        match shell.run(RunRequest {
            run: Run {
                program: Program::Source("return $prune_x".into()),
                script_name: "<test>".into(),
                caps: Capabilities::root(),
                wall: None,
                deferred_lease: None,
                worker_cap: None,
                io: RunIo::Inherit,
                terminal: RequestedTerminalAccess::Leased,
                stdin: RunStdin::Inherit,
                trail: None,
            },
            surface: None,
            deferred: None,
            desk: None,
            fork: None,
            lifecycle: Box::new(()),
        }) {
            RunReport::Ran { ending, .. } => {
                let err = ending
                    .into_result()
                    .expect_err("a pruned name must read as undefined");
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
            RunReport::Static { .. } => {
                panic!("an unbound variable reference is a runtime error, not a static one")
            }
        }
    }

    /// A running worker pins its name: the first prune, with `sleep` still
    /// running, yields no notice and leaves the entry alone. Cancelling
    /// settles the handle, and the second prune takes it like any scratch.
    #[test]
    fn running_handle_pins_name_then_settles_then_prunes() {
        let mut shell = armed_shell(2);
        top_level(&mut shell, "let h_pin = !{spawn { sleep 10 }}").expect("spawn");
        idle_spin(&mut shell, 2);
        assert_eq!(handle_state(&shell, "h_pin"), HandleState::Running);

        assert!(
            shell.prune_idle_bindings().is_empty(),
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
        // The `cancel $h_pin` run referenced, and so renewed, h_pin.
        idle_spin(&mut shell, 2);

        let notices = shell.prune_idle_bindings();
        assert_eq!(notices[0].name, "h_pin", "a settled handle must now prune");
    }

    /// A handle that settles on its own is ordinary scratch once idle — no
    /// special casing beyond the pin check.
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

        let notices = shell.prune_idle_bindings();
        assert!(
            !notices.is_empty(),
            "a settled handle prunes like any scratch"
        );
        assert_eq!(notices[0].name, "h_settled");
        assert_eq!(notices[0].kind, "Handle");
    }

    /// A ledger entry whose install a panic rollback undid — simulated with
    /// the same env save/restore motion the run door performs — is
    /// dropped silently at the next prune: no notice, no error.
    #[test]
    fn orphan_entry_dropped_silently() {
        let mut shell = armed_shell(2);
        let pre_env = shell.env.clone();
        top_level(&mut shell, "let orphan_x = 1").expect("define");
        assert!(is_leased(&shell, "orphan_x"));

        shell.env = pre_env;
        assert!(
            shell.scope_lookup("orphan_x").is_none(),
            "the rollback must remove it from scope"
        );
        assert!(
            is_leased(&shell, "orphan_x"),
            "the ledger entry is not part of the rolled-back environment — it orphans"
        );

        idle_spin(&mut shell, 2);
        let result = shell.prune_idle_bindings();
        assert!(
            result.is_empty(),
            "an orphan-only sweep drops the entry silently and yields no notice"
        );
        assert!(!is_leased(&shell, "orphan_x"), "the orphan must be gone");
    }

    /// A name a host verb wrote directly, bypassing the install chokepoint
    /// as every host verb does, is neither baseline nor tracked. The adoption
    /// sweep runs on every prune pass, even one that prunes nothing, and
    /// leases such a name from this sighting rather than leave it immortal.
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

    #[test]
    fn nothing_expired_prunes_nothing() {
        let mut shell = armed_shell(64);
        top_level(&mut shell, "let fresh_z = 1").expect("define");
        assert!(
            shell.prune_idle_bindings().is_empty(),
            "nothing is idle enough to prune yet"
        );
    }

    // ── the large-binding warning ───────────────────────────────────────────

    /// A session-scope install meeting the threshold writes exactly one
    /// warning onto its run's stderr, and a rebind still over it warns
    /// again — no de-duplication.
    #[test]
    fn large_binding_threshold_warns_on_run_stderr() {
        let mut shell = armed_shell_with(64, 8);
        let text = "this string is definitely over eight bytes long";
        let stderr = top_level_stderr(&mut shell, &format!("let large_x = '{text}'"));

        assert_eq!(
            stderr.matches("large binding `large_x`").count(),
            1,
            "exactly one warning per offending install"
        );
        assert!(
            stderr.contains(&format!("~{} bytes", text.len())),
            "the warning names the byte estimate"
        );

        let text2 = "a different string that also clears eight bytes";
        let stderr2 = top_level_stderr(&mut shell, &format!("let large_x = '{text2}'"));
        assert_eq!(
            stderr2.matches("large binding `large_x`").count(),
            1,
            "a rebind that still exceeds the threshold must warn again"
        );
    }

    #[test]
    fn sub_threshold_install_does_not_warn() {
        let mut shell = armed_shell_with(64, 1_000_000);
        let stderr = top_level_stderr(&mut shell, "let small_x = 1");
        assert!(
            !stderr.contains("large binding"),
            "an install under the threshold must write no warning"
        );
    }
}
