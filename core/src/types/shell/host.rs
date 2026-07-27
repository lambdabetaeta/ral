//! The host-embedding accessor surface.
//!
//! [`Shell::run`](super::Shell), `session`, and `local` are `pub(crate)`:
//! the fields that encode run safety are not a public API, so a host cannot
//! reach in and install an unrelated foreground scope, clear the source
//! registry mid-run, or swap a stream behind the run guard's back.  Hosts
//! drive a session through the narrow operations gathered here — the ones the
//! REPL and exarch actually need — while [`Shell::mobile`](super::Shell) stays
//! the public embedding seam.

use super::Mooring;
use super::Shell;
use super::TerminalAccess;
use super::bindings::{BindingLease, BindingPruneNotice, LargeBindingNotice};
use super::detached::DetachPolicy;
use super::repl::ReplScratch;
use super::workers::ReapCause;
use crate::exit_hints::ExitHints;
use crate::io::{Sink, TerminalState};
use crate::process::{DurableRoot, TerminalLease};
use crate::source::SourceDb;
use crate::types::{AuditFragment, BuiltinEntry, ReapNotice, Value, WorkerEntry, WorkerId};
use std::io::Write;
use std::sync::Arc;

impl Shell {
    /// The session's durable source registry.  Hosts read it after a run
    /// returns to render a runtime error against the right source text.
    pub fn sources(&self) -> &SourceDb {
        &self.session.sources
    }

    /// The session's durable cancel root.  A run door mints a run's
    /// foreground scope from it, under the session anchor or under the
    /// mooring of the run it nests in; the typed relation keeps that scope
    /// rooted here.  Crate-private: frame assembly lives behind the run
    /// doors.
    pub(crate) fn durable_root(&self) -> &DurableRoot {
        &self.session.root
    }

    /// A clonable cancel handle on this session's durable root.  A host
    /// that runs several sessions in one process (exarch's agent fleet)
    /// keeps one per session, so its own cancel cascade can reach the eval
    /// layer: cancelling the handle unwinds the session's in-flight run at
    /// the evaluator's poll points and stops its detached workers.  For a
    /// forked session — deaf to the ambient causes — this handle is the
    /// *only* way to stop a running eval.
    pub fn cancel_handle(&self) -> DurableRoot {
        self.session.root.clone()
    }

    /// Install the startup-loaded exit-code hint table.
    pub fn set_exit_hints(&mut self, hints: ExitHints) {
        self.session.exit_hints = hints;
    }

    /// Install the guest process jail onto a freshly-booted Shell — the
    /// one construction point [`crate::engine::run_engine`] uses when it
    /// detects `RAL_GUEST`.  Every other host bootstrap (exarch, a future
    /// synod) stays unaware that jails exist.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn install_guest_jail(&mut self, jail: Arc<crate::process::jail::GuestJail>) {
        self.session.guest_jail = Some(jail);
    }

    /// Cached terminal state probed at startup (isatty / ANSI / mode bits).
    /// `Copy`, so frontends read the bits they need without borrowing.
    pub fn terminal(&self) -> TerminalState {
        self.io.terminal
    }

    /// Whether the shell is running as an interactive REPL.
    pub fn is_interactive(&self) -> bool {
        self.io.interactive
    }

    /// Mark the shell interactive (or not).  The interactive REPL sets this
    /// at boot so external commands and prompts behave as a live session.
    pub fn set_interactive(&mut self, interactive: bool) {
        self.io.interactive = interactive;
    }

    /// Install the session stdout sink.  The interactive REPL installs its
    /// `ExternalPrinter` here so background output lands above the prompt.
    pub fn set_stdout(&mut self, stdout: Sink) {
        self.io.stdout = stdout;
    }

    /// The session stderr sink, for a host to write diagnostics into.
    pub fn stderr_mut(&mut self) -> &mut Sink {
        &mut self.io.stderr
    }

    /// Turn on top-level audit collection with byte capture (`ral --audit`).
    /// SPEC §10.3: every emitted command node carries stdout/stderr, so the
    /// trail is installed under `CapturePolicy::Bytes` — mirroring the
    /// `audit { … }` builtin, not the default `None` policy that would leave
    /// those fields empty.
    pub fn enable_audit(&mut self) {
        self.local
            .audit
            .install_active_policy(Some(crate::types::CapturePolicy::Bytes));
    }

    /// Drain the accumulated audit trail as a fragment (e.g. for `--audit`
    /// JSON output at end of run).
    pub fn take_audit_fragment(&mut self) -> AuditFragment {
        self.local.audit.take_fragment()
    }

    /// The names of every installed builtin command, for tab completion.
    pub fn builtin_names(&self) -> impl Iterator<Item = &str> {
        self.session.builtins.names()
    }

    /// Install process-static builtin commands into this shell — the
    /// test-dressing door; a production host's surface rides
    /// [`boot_shell`](crate::boot::boot_shell).
    pub fn install_builtins(&mut self, entries: &'static [BuiltinEntry]) {
        self.session.builtins.install_static(entries);
    }

    /// Install captured builtin commands into this shell — the
    /// test-dressing door; a production host's surface rides
    /// [`boot_shell`](crate::boot::boot_shell).
    pub fn install_captured_builtins(&mut self, entries: Arc<[BuiltinEntry]>) {
        self.session.builtins.install_arc(entries);
    }

    /// Look up a builtin command binding installed in this shell.
    pub fn lookup_builtin(&self, name: &str) -> Option<BuiltinEntry> {
        self.session.builtins.get(name)
    }

    /// Install extra `name -> doc` entries from an embedding host so that
    /// `help`/`explain` can list and look them up alongside the builtins and
    /// prelude — the door a host uses to document a sourced closure library
    /// its own builtin table never sees.
    pub fn install_library_docs(&mut self, entries: Vec<(String, String)>) {
        self.session.library_docs.extend(entries);
    }

    /// Read-only access to REPL/editor scratch (plugin context, TUI flag,
    /// queued chpwd notification).
    pub fn repl(&self) -> &ReplScratch {
        &self.local.repl
    }

    /// Mutable access to REPL/editor scratch.  This is the REPL host's own
    /// state; exposing it to the host that owns it is the seam, not a leak.
    pub fn repl_mut(&mut self) -> &mut ReplScratch {
        &mut self.local.repl
    }

    /// Every worker (`spawn`, `watch`, `service`) registered on this shell,
    /// settled or still running. There is no by-id control plane: a listing
    /// hands back the handle itself, so rediscovering a worker is list, take
    /// the handle back, and resume `poll`/`await`/`race`/`cancel` as usual.
    /// Never mutates the registry.
    pub fn workers(&self) -> Vec<WorkerEntry> {
        self.local.workers.snapshot()
    }

    /// Re-acquire one registered worker's entry by id, once a host has
    /// learned it some other way (a ledger row it maintains).  A pure read
    /// like [`Self::workers`]: renews no lease.  `None` names an id with no
    /// live entry.
    pub fn worker_by_id(&self, id: WorkerId) -> Option<WorkerEntry> {
        self.local.workers.lookup(id)
    }

    /// Arm this session's `detach` authority: the number of processes it may
    /// birth over its whole life.  A host calls this in the same act that
    /// installs the `detach` builtin — an unarmed shell simply lacks the
    /// verb. Re-arming replaces the budget and forgets the births spent
    /// against the old one.
    pub fn arm_detach(&mut self, budget: u64) {
        self.local.detach = Some(Arc::new(DetachPolicy {
            budget,
            births: std::sync::atomic::AtomicU64::new(0),
        }));
    }

    /// This session's detach authority, `None` when no host armed one.
    /// Survives the shell's teardown through any clone a birth kept.
    pub fn detach_policy(&self) -> Option<&DetachPolicy> {
        self.local.detach.as_deref()
    }

    /// Number of workers currently registered on this shell.
    pub fn worker_count(&self) -> usize {
        self.local.workers.count()
    }

    /// Drain the reap notices recorded since the last drain — one compact
    /// record per entry removed by policy (the lease chain's idle bound or
    /// backstop, the retention sweep's expiry), never one for an entry an
    /// eliminator observed away first. Crate-private:
    /// [`Self::emit_ready_boundary_notices`] is the one caller, pushing the
    /// fact as a `` `notice `` surface class from *inside* the run that
    /// produced it (`decisions/260706_enquiry-channel` §4.2).
    pub(crate) fn take_worker_reap_notices(&self) -> Vec<ReapNotice> {
        self.local.workers.take_reap_notices()
    }

    /// Arm this shell's settled-worker retention, in ral calls — the boot
    /// door beside [`Self::arm_binding_lease`]. The registry keeps its own
    /// clock (one tick per source dispatch) and sweeps at each run's ready
    /// boundary — stamping an entry first observed settled, expiring one
    /// whose unclaimed result has sat stamped a full `retention` of calls
    /// (a [`Retention`](crate::types::ReapCause::Retention) notice rides
    /// the same drain as the lease chain's). A host that never arms (the
    /// REPL) retains settled entries indefinitely.
    pub fn arm_worker_retention(&mut self, retention: u64) {
        self.local.workers.arm_retention(retention);
    }

    /// Number of distinct lexical bindings visible in scope, a name
    /// shadowed by an inner scope counted once — the `/resources` probe's
    /// binding figure. A count, never the values: it folds names without
    /// cloning, and counting renews nothing (as listing workers renews no
    /// lease).
    pub fn binding_count(&self) -> usize {
        self.mobile.scope.distinct_name_count()
    }

    /// Number of names currently leased (tracked, non-baseline) by the
    /// binding-lease ledger — the `/resources` probe's leased-count figure,
    /// a narrower read than [`Self::binding_count`] (which counts every
    /// visible binding, baseline included). `0` when unarmed. Read-only:
    /// counting renews nothing.
    pub fn leased_binding_count(&self) -> usize {
        self.local.bindings.leased_count()
    }

    /// Drain every large-binding notice queued since the last drain — one
    /// per session-scope install whose value's shallow-size estimate met
    /// the armed lease's threshold (`decisions/260629_agent-binding-reaping`).
    /// Crate-private for the same reason as [`Self::take_worker_reap_notices`]:
    /// [`Self::emit_ready_boundary_notices`] is now the one caller; the
    /// binding itself is never touched by this call.
    pub(crate) fn take_large_binding_notices(&mut self) -> Vec<LargeBindingNotice> {
        self.local.bindings.take_large_binding_notices()
    }

    /// Emit this shell's ready-boundary housekeeping. The large-binding
    /// warning rides the run's own stderr — it is model-facing feedback
    /// about the install the model just made, on the same channel exarch's
    /// timeout and command-exit tips use, so it reaches the model in its
    /// tool result and never becomes a frontend card. The worker-reap and
    /// idle-prune notices push as `` `notice `` surface events through the
    /// installed run's surface sink, in the order the ledgers accumulated
    /// them. Called once per settled run ([`crate::run::run_framed`]),
    /// *before* the run's frame tears down, so both ride this run's own
    /// streams, ordered before its Report
    /// (`decisions/260706_enquiry-channel` §4.2).
    ///
    /// The surface half is a no-op — those ledgers left untouched, not
    /// drained-and-dropped — when no surface sink is installed (a bare REPL,
    /// or a host-embedding test dispatching a raw `RunRequest`): with nobody
    /// to push to, the notices simply wait for a run that does install one.
    /// The large-binding write is ungated: the run's stderr is present
    /// whether or not a surface sink is, so the warning always reaches the
    /// install's own run.
    ///
    /// The binding-lease ledger's *idle-prune* runs here too — engine-side
    /// housekeeping at the engine's own ready boundary, its notice one more
    /// pushed class. Durability makes this safe structurally: the panic
    /// rollback checkpoint is taken at each run's entry
    /// ([`Shell::run`]), after any prior boundary's prune, so a pruned
    /// name can never be resurrected by a rollback.
    ///
    /// The pushed shapes — `` `notice [kind: `reap, cmd, cause] `` and
    /// `` `notice [kind: `prune, names, idle-calls] `` — are what the
    /// exarch host decodes back (`card::value_to_notice`); `cause` travels
    /// as the same lowercase tag exarch's transcript writer already used, so
    /// the wire word and the forensic-record word are one word.
    pub(crate) fn emit_ready_boundary_notices(&mut self, mooring: &Mooring) {
        // The retention sweep runs before the sink guard: expiry is a fact
        // regardless of anyone listening, and its notice waits in the
        // ledger for the next sinked run either way.
        self.local.workers.sweep_retention();
        // The large-binding warning is model-directed diagnostics about the
        // model's own install, so it rides the run's stderr rather than the
        // surface stream — drained ungated, above the sink guard, since the
        // capture stream is present regardless of any frontend sink.
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

    /// Arm this shell's binding-lease ledger and seal the boot baseline:
    /// every name currently visible anywhere in the scope chain — prelude,
    /// agent library, rc bindings, host seed vars — becomes permanently
    /// exempt from expiry. Idempotent by replacement: a re-arm discards any
    /// prior ledger state and reseals from the scope as it stands now. A
    /// host that never calls this observes no expiry
    /// (`decisions/260629_agent-binding-reaping`).
    pub fn arm_binding_lease(&mut self, lease: BindingLease) {
        let baseline = self
            .mobile
            .scope
            .all_bindings()
            .into_iter()
            .map(|(name, _)| name);
        self.local.bindings.arm(lease, baseline);
    }

    /// Prune every leased name idle past the armed lease's bound.
    ///
    /// Returns the empty vec when the ledger is unarmed, when the shell is
    /// not at session scope (a mid-frame caller — e.g. a lifecycle hook —
    /// is refused rather than allowed to unset from a transient frame), or
    /// when nothing was pruned. Engine-internal housekeeping: called at the
    /// ready boundary by [`Self::emit_ready_boundary_notices`], which
    /// pushes the notices as one `` `notice [kind: `prune] `` class. A
    /// pruned name cannot be resurrected by a panic rollback — the rollback
    /// checkpoint is taken at each run's own entry ([`Shell::run`]),
    /// after any prune.
    ///
    /// One pass, in deterministic (sorted) name order, over every expired
    /// candidate: a name absent from scope (its install was rolled back by
    /// a panic restore) drops its orphaned entry silently, with no notice;
    /// a name whose value still structurally reaches a running handle is
    /// skipped — pinned, not pruned, re-examined at the next boundary; every
    /// other name is unset (removing its scheme in the same act, since a
    /// `Binding` couples both), its entry dropped, and a notice recorded.
    /// Afterward, an adoption sweep gives any untracked, non-baseline name
    /// still in the session's top scope a fresh lease starting now — this
    /// runs even on a pass that prunes nothing, self-healing a missed
    /// install path into "leased late" rather than "immortal".
    pub(crate) fn prune_idle_bindings(&mut self) -> Vec<BindingPruneNotice> {
        if !self.local.bindings.armed() || !self.mobile.scope.at_session_scope() {
            return Vec::new();
        }
        let mut notices = Vec::new();
        for (name, idle_calls) in self.local.bindings.expired() {
            match self.mobile.scope.get(&name) {
                None => {
                    // Orphaned by a panic rollback: nothing to prune.
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

    /// The terminal-foreground handoff borrow: `Some(&TerminalLease)` iff
    /// `mooring`'s [`TerminalAccess`] permits it (`Leased` or
    /// `ExplicitLoan`) *and* the session actually owns a lease. The single
    /// gate every post-startup foreground handoff funnels through — the
    /// pipeline launch, the standalone foreground command, and `fg`-resume —
    /// so a run that was not handed authority (an exarch tool run installs
    /// `Denied`) cannot construct the handoff: it has no `&TerminalLease` to
    /// pass [`ForegroundGuard::try_acquire`](crate::process::ForegroundGuard::try_acquire).
    pub fn terminal_lease(&self, mooring: &Mooring) -> Option<&TerminalLease> {
        match mooring.terminal_access {
            TerminalAccess::Denied => None,
            TerminalAccess::Leased | TerminalAccess::ExplicitLoan => {
                self.session.terminal_lease.as_ref()
            }
        }
    }

    /// Exit status of the last command (`$?`).  A host reads it to set its
    /// own process exit code or to seed a prompt's status segment.
    pub fn last_status(&self) -> i32 {
        self.mobile.control.last_status
    }

    /// Set the last-command exit status (`$?`) to an explicit code.  The
    /// integer-valued sibling of
    /// [`set_status_from_bool`](Shell::set_status_from_bool).
    pub fn set_last_status(&mut self, status: i32) {
        self.mobile.control.last_status = status;
    }

    /// Run `f` with `last_status` saved across it and restored afterwards.
    /// The prompt cycle uses it: rendering `RAL_PROMPT` runs a value run
    /// whose own status must not clobber the previous command's exit code,
    /// which the next prompt segment still wants to read.
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

    /// Set the non-tail call-depth ceiling (rc `recursion_limit:` /
    /// `--recursion-limit`).
    pub fn set_recursion_limit(&mut self, n: usize) {
        self.mobile.control.recursion_limit = n;
    }

    /// Install the invocation positional args (`$ARGS`, `$1`, …) — the
    /// script arguments a CLI host passes after the program path.
    pub fn set_args(&mut self, args: Vec<String>) {
        self.mobile.context.args = args;
    }

    /// The acting principal (`$USER` from the dynamic env, empty if unset).
    /// Forwards to [`Context::principal`](super::Context::principal).
    pub fn principal(&self) -> String {
        self.mobile.context.principal()
    }

    /// Set a dynamic env-var override (`within [shell: …]`'s per-key door,
    /// also the seam a host uses to seed `NO_COLOR`, `EXARCH_SESSION_DIR`,
    /// and the like).  Forwards to
    /// [`Context::set_env_var`](super::Context::set_env_var).
    pub fn set_env_var(&mut self, k: impl Into<String>, v: impl Into<String>) {
        self.mobile.context.set_env_var(k, v);
    }

    /// Bulk-insert dynamic env-var overrides.  Forwards to
    /// [`Context::extend_env`](super::Context::extend_env) — the seam a host
    /// uses to seed a batch of vars at boot.
    pub fn extend_env<I, K, V>(&mut self, items: I)
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.mobile.context.extend_env(items);
    }

    /// Read an env var through the dynamic overlay, falling back to the host
    /// process environment — the `within [shell: K=…]` overlay-on-process
    /// rule. A host driving command completion reads `PATH` here.
    pub fn env_var(&self, name: &str) -> Option<String> {
        self.mobile.context.env_overrides().get_or_host(name)
    }

    /// Number of capability frames on the grant stack (the ambient root plus
    /// every live `grant` / `within` attenuation).  Hosts assert grant-stack
    /// balance across a run boundary with it — e.g. that a panicking tool
    /// call left no leaked frame behind.  The qualitative companion is
    /// [`Shell::has_active_capabilities`](Shell::has_active_capabilities).
    pub fn grant_depth(&self) -> usize {
        self.mobile.context.grants.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::process::TerminalLease;

    /// The handoff door: the lease borrow is reachable *only* when the run's
    /// access permits *and* the session owns a lease. A `Denied` run cannot
    /// reach the borrow even though the session owns the lease — the exarch
    /// tool-run case, where the foreground handoff becomes unconstructable.
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

        // Even with authority, no borrow when the session minted no lease
        // (a backgrounded / piped / tty-less launch).
        shell.session.terminal_lease = None;
        assert!(
            shell.terminal_lease(&leased).is_none(),
            "no session lease → no borrow, regardless of access"
        );
    }

    /// [`Mooring::lend_terminal`] raises `Leased` to `ExplicitLoan` and
    /// leaves the parent untouched — the within-run `_ed-tui` elevation.
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

    /// The loan only *raises* an authorised mooring; it never mints
    /// authority. A `Denied` mooring lending its terminal is left `Denied` —
    /// so even with a session lease the foreground borrow stays unreachable.
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
