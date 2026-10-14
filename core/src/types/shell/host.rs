//! The host-embedding accessor surface: the intent verbs the REPL and exarch
//! drive a session through, each a complete operation.  [`Shell`]'s fields are
//! all `pub(crate)`, so no host reaches past them to swap a stream or a
//! foreground scope behind a run guard's back.

use super::Mooring;
use super::Shell;
use super::TerminalAccess;
use super::bindings::BindingLease;
use super::detached::DetachPolicy;
use crate::io::Sink;
use crate::process::{DurableRoot, ForegroundScope, TerminalLease};
use crate::source::SourceDb;
use crate::terminal::TerminalState;
use crate::ty::Fixings;
use crate::types::ExitHints;
use crate::types::{BuiltinEntry, ReapNotice, WorkerEntry, WorkerId};
use std::sync::Arc;

impl Shell {
    /// The session's durable source registry, read after a run returns to
    /// render its errors against the right source text.
    pub fn sources(&self) -> &SourceDb {
        &self.session.sources
    }

    /// The session's durable cancel root, under which every run and
    /// worker hangs.
    pub(crate) fn durable_root(&self) -> &DurableRoot {
        &self.session.root
    }

    /// A clonable cancel handle: cancelling it unwinds the in-flight run at the
    /// evaluator's poll points and stops the session's workers.  No
    /// session folds the process's signals, so this — or a `Control` over it —
    /// is how a host stops a running eval.
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
    pub(crate) fn run_cancel_handle(&self) -> ForegroundScope {
        self.session.anchor.child()
    }

    /// What this session's `comparable` variables were fixed to.
    pub fn fixings(&self) -> &Fixings {
        &self.session.fixings
    }

    pub fn set_exit_hints(&mut self, hints: ExitHints) {
        self.session.exit_hints = Arc::new(hints);
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

    /// Every installed builtin's name, for tab completion.
    pub fn builtin_names(&self) -> impl Iterator<Item = &str> {
        self.session.runtime.builtins.manifest().names()
    }

    pub fn lookup_builtin(&self, name: &str) -> Option<BuiltinEntry> {
        self.session.runtime.builtins.get(name)
    }

    /// Every worker (`spawn`, `watch`, `service`) here, settled or running.
    /// There is no by-id control plane: the listing hands back the handle
    /// itself, so a rediscovered worker resumes `poll`/`await`/`race`/`cancel`
    /// as usual.  Enumeration is not observation, so it renews no lease.
    pub fn workers(&self) -> Vec<WorkerEntry> {
        self.local.workers.snapshot()
    }

    /// Ral calls until a settled worker's retention expires; `None` while it
    /// runs, or with no retention armed.
    pub fn worker_retention_left(&self, entry: &WorkerEntry) -> Option<u64> {
        self.local.workers.retention_left(entry)
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
        self.local.detach = Some(Arc::new(DetachPolicy::new(budget)));
    }

    pub fn detach_policy(&self) -> Option<&DetachPolicy> {
        self.local.detach.as_deref()
    }

    pub fn worker_count(&self) -> usize {
        self.local.workers.count()
    }

    /// One notice per entry removed by policy — idle bound, backstop, retention
    /// expiry — never one an eliminator observed away first.
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
    /// what `crate::carrier::Transport::binding_count` serves exarch's `/resources` fold.
    /// Names only, never the values, and renewing nothing.
    pub(crate) fn binding_count(&self) -> usize {
        self.env.distinct_name_count(&self.sig)
    }

    /// Names the binding-lease ledger tracks — non-baseline only, so a
    /// narrower read than [`Self::binding_count`], and `0` when unarmed.
    pub(crate) fn leased_binding_count(&self) -> usize {
        self.local.bindings.leased_count()
    }

    /// Arm the binding-lease ledger and seal the baseline: every name visible
    /// in the scope chain right now — prelude, agent library, rc bindings, host
    /// seed vars — becomes permanently exempt from expiry.  A re-arm discards
    /// the prior ledger and reseals; a host that never arms sees no expiry.
    pub fn arm_binding_lease(&mut self, lease: BindingLease) {
        let baseline = self.visible_names();
        self.local.bindings.arm(lease, baseline);
    }

    /// Every name visible in the scope chain right now.
    pub(super) fn visible_names(&self) -> Vec<String> {
        self.env
            .all_bindings(&self.sig)
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    /// The terminal-foreground handoff borrow: `Some` iff `mooring`'s
    /// [`TerminalAccess`] permits it *and* the session owns a lease.  Every
    /// post-startup handoff funnels through here, so a run denied authority
    /// (exarch's tool runs) cannot construct one at all — it has no
    /// `&TerminalLease` to hand
    /// [`TerminalLoan::try_acquire`](crate::process::TerminalLoan::try_acquire).
    pub(crate) fn terminal_lease(&self, mooring: &Mooring) -> Option<&TerminalLease> {
        match mooring.terminal_access {
            TerminalAccess::Denied => None,
            TerminalAccess::Leased | TerminalAccess::ExplicitLoan => {
                self.session.terminal_lease.as_ref()
            }
        }
    }

    /// The active stack cap: frames, not host stack frames.
    pub fn stack_limit(&self) -> usize {
        self.session.stack_limit
    }

    /// Set that ceiling — rc `recursion-limit:` and `--recursion-limit`.
    pub fn set_stack_limit(&mut self, n: usize) {
        self.session.stack_limit = n;
    }

    /// The invocation positionals (`args`) a CLI host passes after
    /// the program path.
    pub fn set_args(&mut self, args: Vec<String>) {
        self.context.args = args;
    }

    /// The acting principal: `USER` from the dynamic env, with no host-env
    /// fallback, so it names nobody until a front end seeds it.
    pub fn principal(&self) -> Option<String> {
        self.context.principal()
    }

    /// Set a dynamic env-var override for the rest of the session — how a host
    /// seeds `NO_COLOR`, `EXARCH_SESSION_DIR`, and the like.  `within [env: …]`
    /// wants the scoped [`Shell::with_env`] instead, which restores on exit.
    pub fn set_env_var(&mut self, k: impl Into<String>, v: impl Into<String>) {
        self.context.set_env_var(k, v);
    }

    /// Read an env var through the dynamic overlay, falling back to the host
    /// process environment — the overlay-on-process rule `within [env: …]`
    /// obeys.  A host driving command completion reads `PATH` here.
    pub fn env_var(&self, name: &str) -> Option<String> {
        self.context.env_overrides().get_or_host(name)
    }

    /// The dynamic overlay itself, for a host handing it to an overlay-aware
    /// helper (the `RAL_PATH` plugin search) rather than reading one key.
    pub fn env_overrides(&self) -> &crate::types::EnvVars {
        self.context.env_overrides()
    }

    /// Capability frames on the grant stack — the ambient root plus every live
    /// `grant` / `within` attenuation — with which a host asserts stack balance
    /// across a run boundary.  [`Shell::has_active_capabilities`] asks
    /// qualitatively.
    #[cfg(feature = "test-util")]
    pub(crate) fn grant_depth(&self) -> usize {
        self.context.grants.len()
    }
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
        let mut shell = crate::test_helper::core_shell();
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

    /// Only a held terminal is lent: `Leased` yields the raised loan and stays
    /// itself; `Denied` has nothing to lend, `ExplicitLoan` has already lent.
    #[test]
    fn lend_terminal_lends_only_a_held_terminal() {
        let with = |terminal_access| Mooring {
            terminal_access,
            ..Mooring::adrift()
        };
        let leased = with(TerminalAccess::Leased);
        let loan = leased.lend_terminal().expect("a Leased mooring lends");
        assert_eq!(loan.terminal_access, TerminalAccess::ExplicitLoan);
        assert_eq!(leased.terminal_access, TerminalAccess::Leased);
        assert!(with(TerminalAccess::Denied).lend_terminal().is_none());
        assert!(loan.lend_terminal().is_none());
    }
}
