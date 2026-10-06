//! The host-embedding accessor surface: the intent verbs the REPL and exarch
//! drive a session through, each a complete operation.  [`Shell`]'s fields are
//! all `pub(crate)`, so no host reaches past them to swap a stream or a
//! foreground scope behind a run guard's back.

use super::Mooring;
use super::Shell;
use super::TerminalAccess;
use super::bindings::BindingLease;
use super::detached::DetachPolicy;
use super::repl::ReplScratch;
use crate::io::Sink;
use crate::process::{DurableRoot, ForegroundScope, TerminalLease};
use crate::source::SourceDb;
use crate::terminal::TerminalState;
use crate::types::ExitHints;
use crate::types::{BuiltinEntry, Convention, ReapNotice, Set, WorkerEntry, WorkerId};
use std::sync::Arc;

impl Shell {
    /// The session's durable source registry, read after a run returns to
    /// render its errors against the right source text.
    pub fn sources(&self) -> &SourceDb {
        &self.session.sources
    }

    /// The session's durable cancel root, under which every run and detached
    /// worker hangs.
    pub(crate) fn durable_root(&self) -> &DurableRoot {
        &self.session.root
    }

    /// A clonable cancel handle: cancelling it unwinds the in-flight run at the
    /// evaluator's poll points and stops the session's detached workers.  No
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

    pub fn set_exit_hints(&mut self, hints: ExitHints) {
        self.session.exit_hints = hints;
    }

    /// Install the guest process jail — called only by an engine's boot when
    /// it sees `RAL_GUEST`, so every recipe stays unaware that jails exist.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn install_guest_jail(&mut self, jail: Arc<crate::process::jail::GuestJail>) {
        self.session.guest_jail = Some(jail);
    }

    /// The guest process jail installed on this session — `None` anywhere but
    /// a real Linux guest.
    pub(crate) fn guest_jail(&self) -> Option<Arc<crate::process::jail::GuestJail>> {
        self.session.guest_jail.clone()
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
        self.session.builtins.manifest().names()
    }

    /// The test-dressing door, with [`Self::install_captured_builtins`]; a
    /// production host's surface rides [`HostSurface::shell`](crate::HostSurface::shell).
    pub fn install_builtins(&mut self, entries: &'static [BuiltinEntry]) {
        self.install_set(&Set::Static(entries));
    }

    pub fn install_captured_builtins(&mut self, entries: &Arc<[BuiltinEntry]>) {
        self.install_set(&Set::Captured(Arc::clone(entries)));
    }

    /// Install `set`, then seed the base env scope and base handler frames
    /// from it, unless it was already here.
    pub(crate) fn install_set(&mut self, set: &Set) {
        if self.session.builtins.install(set.clone()) {
            seed_natives_and_base(self, set);
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
        let baseline = self
            .env
            .all_bindings(&self.sig)
            .into_iter()
            .map(|(name, _)| name);
        self.local.bindings.arm(lease, baseline);
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

/// Seed freshly installed `entries` into the two places the manifest's two
/// halves live: a value row becomes a native in the base env scope, an argv
/// row a base handler frame.  Each row says which it is
/// ([`Convention`](crate::types::Convention)); the one place either is
/// populated, for core and host installs alike.
fn seed_natives_and_base(shell: &mut Shell, entries: &[BuiltinEntry]) {
    let natives = entries
        .iter()
        .filter(|entry| entry.decl.convention == Convention::Value)
        .map(|entry| {
            (
                entry.decl.name.clone().into_owned(),
                crate::types::builtin::native_value(entry),
            )
        });
    let base: Vec<BuiltinEntry> = entries
        .iter()
        .filter(|entry| entry.decl.convention == Convention::Argv)
        .cloned()
        .collect();
    Arc::make_mut(&mut shell.sig).install_natives(natives);
    shell.context.handlers.install_base(&base);
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
        let mut shell = crate::test_helper::core_shell();
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
