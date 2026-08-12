//! The host-embedding accessor surface: the intent verbs the REPL and exarch
//! drive a session through, each a complete operation.  [`Shell`]'s fields are
//! all `pub(crate)`, so no host reaches past them to swap a stream or a
//! foreground scope behind a run guard's back.

use super::Mooring;
use super::Shell;
use super::TerminalAccess;
use super::bindings::{BindingLease, BindingPruneNotice, LargeBindingNotice};
use super::detached::DetachPolicy;
use super::repl::ReplScratch;
use super::workers::ReapCause;
use crate::exit_hints::ExitHints;
use crate::io::{Sink, TerminalState};
use crate::process::{DurableRoot, ForegroundScope, TerminalLease};
use crate::source::SourceDb;
use crate::types::{AuditFragment, BuiltinEntry, ReapNotice, Value, WorkerEntry, WorkerId};
use std::io::Write;
use std::sync::Arc;

impl Shell {
    /// The session's durable source registry, read after a run returns to
    /// render its errors against the right source text.
    pub fn sources(&self) -> &SourceDb {
        &self.session.sources
    }

    /// The session's durable cancel root, under which every run's foreground
    /// scope is minted.  Crate-private: frame assembly stays behind the run
    /// doors in `run.rs`.
    pub(crate) fn durable_root(&self) -> &DurableRoot {
        &self.session.root
    }

    /// A clonable cancel handle: cancelling it unwinds the in-flight run at the
    /// evaluator's poll points and stops the session's detached workers.  A
    /// [`Shell::fork_session`] child is deaf to the ambient causes, so for that
    /// one this handle is the *only* way to stop a running eval.
    pub fn cancel_handle(&self) -> DurableRoot {
        self.session.root.clone()
    }

    /// A cancel handle for a run the host has not started yet, to be passed back
    /// as [`Shell::run_under`]'s `under`.
    ///
    /// Cancelling a scope is sticky and every poll re-walks the chain, so a host
    /// minting this *before* it dispatches the work can record a cancel that
    /// arrives before the run's own frame exists: the frame is born a descendant
    /// and reads the flag on its first poll.  Hung under the session anchor like
    /// every other top-level frame, so session teardown reaches it.
    pub fn run_cancel_handle(&self) -> ForegroundScope {
        self.session.anchor.child()
    }

    pub fn set_exit_hints(&mut self, hints: ExitHints) {
        self.session.exit_hints = hints;
    }

    /// Install the guest process jail — called only by
    /// [`crate::engine::run_engine`] when it sees `RAL_GUEST`, so every other
    /// host bootstrap stays unaware that jails exist.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn install_guest_jail(&mut self, jail: Arc<crate::process::jail::GuestJail>) {
        self.session.guest_jail = Some(jail);
    }

    /// Terminal state probed once at startup (isatty / ANSI / mode bits).
    pub fn terminal(&self) -> TerminalState {
        self.io.terminal
    }

    pub fn is_interactive(&self) -> bool {
        self.io.interactive
    }

    /// Mark the shell interactive — the REPL sets it at boot so external
    /// commands and prompts behave as a live session.
    pub fn set_interactive(&mut self, interactive: bool) {
        self.io.interactive = interactive;
    }

    /// Install the session stdout sink — the REPL puts its `ExternalPrinter`
    /// here so background output lands above the prompt.
    pub fn set_stdout(&mut self, stdout: Sink) {
        self.io.stdout = stdout;
    }

    pub fn stderr_mut(&mut self) -> &mut Sink {
        &mut self.io.stderr
    }

    /// Turn on top-level audit collection (`ral --audit`) under
    /// `CapturePolicy::Bytes`, as `audit { … }` does: every command
    /// observation must carry its stdout/stderr, which `Off` would leave empty.
    ///
    /// The session is its own extent — there is no later `close` to pair
    /// with this open, only [`Self::take_audit_fragment`] draining what
    /// accrued.  Unlike `try`/`audit`, which delimit a body inside one run,
    /// this trail stays open for the process's whole life.
    pub fn enable_audit(&mut self) {
        self.local
            .audit
            .install_active_policy(Some(crate::types::CapturePolicy::Bytes));
    }

    /// Drain what the session trail collected since the last drain, leaving
    /// it open for the next.
    pub fn take_audit_fragment(&mut self) -> AuditFragment {
        self.local.audit.take_fragment()
    }

    /// Every installed builtin's name, for tab completion.
    pub fn builtin_names(&self) -> impl Iterator<Item = &str> {
        self.session.builtins.names()
    }

    /// Every native's name — what a `$name` reference can reach, unlike
    /// [`Self::builtin_names`], which also lists the base-frame names.
    pub fn native_names(&self) -> impl Iterator<Item = &str> {
        self.mobile.scope.native_names()
    }

    /// The test-dressing door, with [`Self::install_captured_builtins`]; a
    /// production host's surface rides [`boot_shell`](crate::boot::boot_shell).
    /// Installs the manifest set, then seeds the base env scope and base
    /// handler frames from it.
    pub fn install_builtins(&mut self, entries: &'static [BuiltinEntry]) {
        if self.session.builtins.install_static(entries) {
            seed_natives_and_base(self, entries);
        }
    }

    pub fn install_captured_builtins(&mut self, entries: &Arc<[BuiltinEntry]>) {
        if self.session.builtins.install_arc(Arc::clone(entries)) {
            seed_natives_and_base(self, entries);
        }
    }

    pub fn lookup_builtin(&self, name: &str) -> Option<BuiltinEntry> {
        self.session.builtins.get(name)
    }

    /// Install extra `name -> doc` entries for `help`/`explain` — how a host
    /// documents a sourced closure library no builtin table ever sees.
    pub fn install_library_docs(&mut self, entries: Vec<(String, String)>) {
        self.session.library_docs.extend(entries);
    }

    pub fn repl(&self) -> &ReplScratch {
        &self.local.repl
    }

    /// Handing the REPL host its own state back is the seam, not a leak.
    pub fn repl_mut(&mut self) -> &mut ReplScratch {
        &mut self.local.repl
    }

    /// Every worker (`spawn`, `watch`, `service`) here, settled or running.
    /// There is no by-id control plane: the listing hands back the handle
    /// itself, so a rediscovered worker resumes `poll`/`await`/`race`/`cancel`
    /// as usual.  Enumeration is not observation, so it renews no lease.
    pub fn workers(&self) -> Vec<WorkerEntry> {
        self.local.workers.snapshot()
    }

    /// Re-acquire one entry by an id the host learned elsewhere — a pure read
    /// like [`Self::workers`], renewing no lease.
    pub fn worker_by_id(&self, id: WorkerId) -> Option<WorkerEntry> {
        self.local.workers.lookup(id)
    }

    /// Arm the `detach` authority: processes this session may birth over its
    /// whole life, armed in the same act that installs the `detach` builtin.
    /// Re-arming replaces the budget and forgets the births spent on the old.
    pub fn arm_detach(&mut self, budget: u64) {
        self.local.detach = Some(Arc::new(DetachPolicy {
            budget,
            births: std::sync::atomic::AtomicU64::new(0),
        }));
    }

    pub fn detach_policy(&self) -> Option<&DetachPolicy> {
        self.local.detach.as_deref()
    }

    pub fn worker_count(&self) -> usize {
        self.local.workers.count()
    }

    /// One notice per entry removed by policy — idle bound, backstop, retention
    /// expiry — never one an eliminator observed away first.
    /// [`Self::emit_ready_boundary_notices`] is the only caller.
    pub(crate) fn take_worker_reap_notices(&self) -> Vec<ReapNotice> {
        self.local.workers.take_reap_notices()
    }

    /// Arm settled-worker retention, in ral calls — the boot door beside
    /// [`Self::arm_binding_lease`].  An entry whose unclaimed result has sat
    /// settled a full `retention` of calls expires with a
    /// [`ReapCause::Retention`] notice; a host that never arms (the REPL) keeps
    /// settled entries forever.
    pub fn arm_worker_retention(&mut self, retention: u64) {
        self.local.workers.arm_retention(retention);
    }

    /// Distinct lexical names visible in scope, a shadowed one counted once —
    /// what `crate::transport::answer_probe` serves exarch's `/resources` fold.
    /// Names only, never the values, and renewing nothing.
    pub fn binding_count(&self) -> usize {
        self.mobile.scope.distinct_name_count()
    }

    /// Names the binding-lease ledger tracks — non-baseline only, so a
    /// narrower read than [`Self::binding_count`], and `0` when unarmed.
    pub fn leased_binding_count(&self) -> usize {
        self.local.bindings.leased_count()
    }

    /// One notice per session-scope install whose shallow-size estimate met the
    /// armed lease's threshold, the binding itself untouched.
    /// [`Self::emit_ready_boundary_notices`] is the only caller.
    pub(crate) fn take_large_binding_notices(&mut self) -> Vec<LargeBindingNotice> {
        self.local.bindings.take_large_binding_notices()
    }

    /// This run's ready-boundary housekeeping — [`crate::run::run_framed`] calls
    /// it once per settled run, *before* the frame tears down, so it rides the
    /// run's own streams and lands ahead of its report.
    ///
    /// The large-binding warning goes to stderr, reaching the model in its tool
    /// result rather than becoming a frontend card, and is ungated: stderr is
    /// there whether a surface sink is or not.  Reap and prune push as
    /// `` `notice [kind: `reap, cmd, cause] `` and
    /// `` `notice [kind: `prune, names, idle-calls] `` for exarch's
    /// `card::value_to_notice`; absent a sink that half leaves the ledgers
    /// *untouched* rather than drained and dropped, so their notices wait for a
    /// run that does install one.
    pub(crate) fn emit_ready_boundary_notices(&mut self, mooring: &Mooring) {
        // Above the sink guard: expiry is a fact regardless of anyone
        // listening, and its notice waits in the ledger either way.
        self.local.workers.sweep_retention();
        for notice in self.take_large_binding_notices() {
            let line = format!(
                "note: large binding `{}` (~{} bytes) held in session memory; consider \
                 writing it to a file and binding the path instead of the captured bytes\n",
                notice.name, notice.bytes,
            );
            let _ = self.io.stderr.write_all(line.as_bytes());
        }
        if mooring.surface.is_none() {
            return;
        }
        for notice in self.take_worker_reap_notices() {
            let cause = match notice.cause {
                ReapCause::Idle => "idle",
                ReapCause::Backstop => "backstop",
                ReapCause::Retention => "retention",
            };
            mooring.surface(&Value::Variant {
                label: "notice".into(),
                payload: Some(Box::new(Value::map(vec![
                    (
                        "kind".into(),
                        Value::Variant {
                            label: "reap".into(),
                            payload: None,
                        },
                    ),
                    ("cmd".into(), Value::String(notice.cmd)),
                    ("cause".into(), Value::String(cause.into())),
                ]))),
            });
        }
        let pruned = self.prune_idle_bindings();
        if !pruned.is_empty() {
            mooring.surface(&Value::Variant {
                label: "notice".into(),
                payload: Some(Box::new(Value::map(vec![
                    (
                        "kind".into(),
                        Value::Variant {
                            label: "prune".into(),
                            payload: None,
                        },
                    ),
                    (
                        "names".into(),
                        Value::list(
                            pruned
                                .iter()
                                .map(|n| Value::String(n.name.clone()))
                                .collect(),
                        ),
                    ),
                    (
                        "idle-calls".into(),
                        Value::list(
                            pruned
                                .iter()
                                .map(|n| {
                                    Value::Int({
                                        #[allow(
                                            clippy::cast_possible_wrap,
                                            reason = "an idle-call count is far below i64::MAX"
                                        )]
                                        {
                                            n.idle_calls as i64
                                        }
                                    })
                                })
                                .collect(),
                        ),
                    ),
                ]))),
            });
        }
    }

    /// Arm the binding-lease ledger and seal the baseline: every name visible
    /// in the scope chain right now — prelude, agent library, rc bindings, host
    /// seed vars — becomes permanently exempt from expiry.  A re-arm discards
    /// the prior ledger and reseals; a host that never arms sees no expiry.
    pub fn arm_binding_lease(&mut self, lease: BindingLease) {
        let baseline = self
            .mobile
            .scope
            .all_bindings()
            .into_iter()
            .map(|(name, _)| name);
        self.local.bindings.arm(lease, baseline);
    }

    /// Prune every leased name idle past the armed lease's bound, at the ready
    /// boundary [`Self::emit_ready_boundary_notices`] owns.  Nothing happens
    /// off session scope: a mid-frame caller such as a lifecycle hook is
    /// refused rather than allowed to unset from a transient frame.  A pruned
    /// name cannot come back through a panic rollback either, since
    /// [`Shell::run`] checkpoints at run entry, after any earlier prune.
    ///
    /// One pass in sorted order: a name absent from scope had its install
    /// rolled back, so the orphan drops silently; a value that structurally
    /// reaches a running handle is pinned and re-examined next boundary; the
    /// rest are unset, scheme and all, since a `Binding` couples both.  The
    /// adoption sweep afterwards runs even on a pass that prunes nothing, so a
    /// name a missed install path left untracked is leased late, not immortal.
    pub(crate) fn prune_idle_bindings(&mut self) -> Vec<BindingPruneNotice> {
        if !self.local.bindings.armed() || !self.mobile.scope.at_session_scope() {
            return Vec::new();
        }
        let mut notices = Vec::new();
        for (name, idle_calls) in self.local.bindings.expired() {
            match self.mobile.scope.get(&name) {
                None => {
                    // Orphaned by a rollback: nothing to prune.
                    self.local.bindings.drop_entry(&name);
                }
                Some(value) if crate::types::pins_running_work(value) => {
                    // Pinned: leave the entry exactly as it is.
                }
                Some(value) => {
                    let kind = value.type_name();
                    self.mobile.scope.unset(&name);
                    self.local.bindings.drop_entry(&name);
                    notices.push(BindingPruneNotice {
                        name,
                        idle_calls,
                        kind,
                    });
                }
            }
        }
        let top_scope_names: Vec<String> = self.mobile.scope.top_scope().keys().cloned().collect();
        for name in top_scope_names {
            self.local.bindings.adopt(&name);
        }
        notices
    }

    /// The terminal-foreground handoff borrow: `Some` iff `mooring`'s
    /// [`TerminalAccess`] permits it *and* the session owns a lease.  Every
    /// post-startup handoff funnels through here, so a run denied authority
    /// (exarch's tool runs) cannot construct one at all — it has no
    /// `&TerminalLease` to hand
    /// [`ForegroundGuard::try_acquire`](crate::process::ForegroundGuard::try_acquire).
    pub fn terminal_lease(&self, mooring: &Mooring) -> Option<&TerminalLease> {
        match mooring.terminal_access {
            TerminalAccess::Denied => None,
            TerminalAccess::Leased | TerminalAccess::ExplicitLoan => {
                self.session.terminal_lease.as_ref()
            }
        }
    }

    /// Exit status of the last command (`$?`) — a host's own exit code, or a
    /// prompt's status segment.
    pub fn last_status(&self) -> i32 {
        self.mobile.control.last_status
    }

    /// Plant `$?` explicitly.  `$?` is the run's own result, so core writes it
    /// directly and a host only reads: this exists for the integration tests,
    /// which prime a sentinel to prove what evaluation resets and cannot reach
    /// a `#[cfg(test)]` item to do it.
    pub fn set_last_status(&mut self, status: i32) {
        self.mobile.control.last_status = status;
    }

    /// Run `f` with `last_status` saved across it and restored after — the
    /// prompt cycle's need, since rendering `RAL_PROMPT` is itself a run and
    /// must not clobber the exit code a later prompt segment still reads.
    pub fn with_preserved_status<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let saved = self.mobile.control.last_status;
        let r = f(self);
        self.mobile.control.last_status = saved;
        r
    }

    /// The active non-tail call-depth ceiling.
    pub fn recursion_limit(&self) -> usize {
        self.mobile.control.recursion_limit
    }

    /// Set that ceiling — rc `recursion_limit:` and `--recursion-limit`.
    pub fn set_recursion_limit(&mut self, n: usize) {
        self.mobile.control.recursion_limit = n;
    }

    /// The invocation positionals (`$ARGS`, `$1`, …) a CLI host passes after
    /// the program path.
    pub fn set_args(&mut self, args: Vec<String>) {
        self.mobile.context.args = args;
    }

    /// The acting principal: `$USER` from the dynamic env, with no host-env
    /// fallback, so it stays empty until a front end seeds it.
    pub fn principal(&self) -> String {
        self.mobile.context.principal()
    }

    /// Set a dynamic env-var override for the rest of the session — how a host
    /// seeds `NO_COLOR`, `EXARCH_SESSION_DIR`, and the like.  `within [env: …]`
    /// wants the scoped [`Shell::with_env`] instead, which restores on exit.
    pub fn set_env_var(&mut self, k: impl Into<String>, v: impl Into<String>) {
        self.mobile.context.set_env_var(k, v);
    }

    /// [`Self::set_env_var`] in bulk, for a host seeding a batch at boot.
    pub fn extend_env<I, K, V>(&mut self, items: I)
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.mobile.context.extend_env(items);
    }

    /// Read an env var through the dynamic overlay, falling back to the host
    /// process environment — the overlay-on-process rule `within [env: …]`
    /// obeys.  A host driving command completion reads `PATH` here.
    pub fn env_var(&self, name: &str) -> Option<String> {
        self.mobile.context.env_overrides().get_or_host(name)
    }

    /// The dynamic overlay itself, for a host handing it to an overlay-aware
    /// helper (the `RAL_PATH` plugin search) rather than reading one key.
    pub fn env_overrides(&self) -> &crate::types::EnvVars {
        self.mobile.context.env_overrides()
    }

    /// Capability frames on the grant stack — the ambient root plus every live
    /// `grant` / `within` attenuation — with which a host asserts stack balance
    /// across a run boundary.  [`Shell::has_active_capabilities`] asks
    /// qualitatively.
    pub fn grant_depth(&self) -> usize {
        self.mobile.context.grants.len()
    }
}

/// Partition freshly installed `entries` by [`BuiltinEntry::fixed_arity`]:
/// fixed arity seeds the base env scope as a native value, an open argv
/// installs as a base handler frame.  The one place either is populated, for
/// core and host installs alike.
fn seed_natives_and_base(shell: &mut Shell, entries: &[BuiltinEntry]) {
    let mut natives = Vec::new();
    let mut base = Vec::new();
    for entry in entries {
        match crate::types::builtin::native_value(entry) {
            Some(value) => natives.push((entry.name.clone().into_owned(), value)),
            None => base.push(entry.clone()),
        }
    }
    shell.mobile.scope.install_natives(natives);
    shell.mobile.context.handlers.install_base(&base);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::process::TerminalLease;

    /// Both halves of the gate bear weight: `Denied` cannot borrow a lease the
    /// session owns, and no access reaches a lease never minted.
    #[test]
    #[cfg(unix)]
    fn terminal_lease_gated_by_access_and_session() {
        let mut shell = Shell::default();
        shell.session.terminal_lease = TerminalLease::mint_at_startup(true);
        assert!(
            shell.session.terminal_lease.is_some(),
            "session owns a lease"
        );

        let denied = Mooring::adrift();
        assert!(
            shell.terminal_lease(&denied).is_none(),
            "a Denied mooring cannot borrow the session lease"
        );

        let leased = Mooring {
            terminal_access: TerminalAccess::Leased,
            ..Mooring::adrift()
        };
        assert!(
            shell.terminal_lease(&leased).is_some(),
            "a Leased mooring borrows the session lease"
        );

        // A backgrounded / piped / tty-less launch mints no lease at all.
        shell.session.terminal_lease = None;
        assert!(
            shell.terminal_lease(&leased).is_none(),
            "no session lease → no borrow, regardless of access"
        );
    }

    /// The `_ed-tui` elevation: the loan derives a raised mooring rather than
    /// mutating the parent.
    #[test]
    fn lend_terminal_raises_leased_to_explicit_loan() {
        let leased = Mooring {
            terminal_access: TerminalAccess::Leased,
            ..Mooring::adrift()
        };
        assert!(!leased.in_terminal_loan());

        let loaned = leased.lend_terminal();
        assert!(
            loaned.in_terminal_loan(),
            "the derived mooring is raised to ExplicitLoan"
        );
        assert!(
            !leased.in_terminal_loan(),
            "the parent mooring is untouched by the derivation"
        );
    }

    /// The loan raises an authorised mooring but never mints authority, so
    /// lending from `Denied` leaves the borrow unreachable even with a lease.
    #[test]
    #[cfg(unix)]
    fn denied_mooring_lend_does_not_elevate() {
        let mut shell = Shell::default();
        shell.session.terminal_lease = TerminalLease::mint_at_startup(true);
        let denied = Mooring::adrift();

        let loaned = denied.lend_terminal();
        assert!(
            !loaned.in_terminal_loan(),
            "a Denied mooring is not raised to ExplicitLoan"
        );
        assert!(
            shell.terminal_lease(&loaned).is_none(),
            "no foreground borrow: the loan cannot mint authority from Denied"
        );
    }
}
