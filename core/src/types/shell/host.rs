//! The host-embedding accessor surface.
//!
//! [`Shell::turn`](super::Shell), `session`, and `local` are `pub(crate)`:
//! the fields that encode turn safety are not a public API, so a host cannot
//! reach in and install an unrelated foreground scope, clear the source
//! registry mid-turn, or swap a stream behind the turn guard's back.  Hosts
//! drive a session through the narrow operations gathered here — the ones the
//! REPL and exarch actually need — while [`Shell::mobile`](super::Shell) stays
//! the public embedding seam.

use super::Shell;
use super::TerminalAccess;
use super::repl::ReplScratch;
use crate::diagnostic::SourceDb;
use crate::exit_hints::ExitHints;
use crate::io::{Sink, TerminalState};
use crate::process::{DurableRoot, ForegroundScope, TerminalLease};
use crate::types::AuditFragment;

/// An in-flight terminal loan, returned by [`Shell::begin_terminal_loan`] and
/// surrendered to [`Shell::end_terminal_loan`].
///
/// Opaque to hosts: it carries the prior [`TerminalAccess`] to restore but
/// exposes no way to read or forge one. Obtaining it raises an already-`Leased`
/// turn to [`TerminalAccess::ExplicitLoan`] (a `Denied` turn is left untouched,
/// so the loan can only raise authority, never mint it); surrendering it
/// restores the prior access. This is the only path to an `ExplicitLoan` — a
/// `TurnRequest` cannot seed it — so the elevation is always a within-turn loan
/// held by the host that suspended its own terminal surface.
pub struct TerminalLoan(TerminalAccess);

impl Shell {
    /// The session's durable source registry.  Hosts read it after a turn
    /// returns to render a runtime error against the right source text.
    pub fn sources(&self) -> &SourceDb {
        &self.session.sources
    }

    /// The session's durable cancel root.  A turn door mints a turn's
    /// foreground scope from it (`self.durable_root().child()`); the typed
    /// relation keeps that scope rooted here.  No longer a host accessor —
    /// frame assembly moved behind the turn doors, so this is `pub(crate)`.
    pub(crate) fn durable_root(&self) -> &DurableRoot {
        &self.session.root
    }

    /// The current turn's foreground scope.  A host clones a deadline child
    /// of it, or cancels it to interrupt the foreground work.
    pub fn foreground(&self) -> &ForegroundScope {
        &self.turn.cancel
    }

    /// Install the startup-loaded exit-code hint table.
    pub fn set_exit_hints(&mut self, hints: ExitHints) {
        self.session.exit_hints = hints;
    }

    /// Cached terminal state probed at startup (isatty / ANSI / mode bits).
    /// `Copy`, so frontends read the bits they need without borrowing.
    pub fn terminal(&self) -> TerminalState {
        self.turn.io.terminal
    }

    /// Whether the shell is running as an interactive REPL.
    pub fn is_interactive(&self) -> bool {
        self.turn.io.interactive
    }

    /// Mark the shell interactive (or not).  The interactive REPL sets this
    /// at boot so external commands and prompts behave as a live session.
    pub fn set_interactive(&mut self, interactive: bool) {
        self.turn.io.interactive = interactive;
    }

    /// Install the session stdout sink.  The interactive REPL installs its
    /// `ExternalPrinter` here so background output lands above the prompt.
    pub fn set_stdout(&mut self, stdout: Sink) {
        self.turn.io.stdout = stdout;
    }

    /// The session stderr sink, for a host to write diagnostics into.
    pub fn stderr_mut(&mut self) -> &mut Sink {
        &mut self.turn.io.stderr
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

    /// The terminal-foreground handoff borrow: `Some(&TerminalLease)` iff the
    /// installed turn's [`TerminalAccess`] permits it (`Leased` or
    /// `ExplicitLoan`) *and* the session actually owns a lease. The single
    /// gate every post-startup foreground handoff funnels through — the
    /// pipeline launch, the standalone foreground command, and `fg`-resume —
    /// so a turn that was not handed authority (an exarch tool turn installs
    /// `Denied`) cannot construct the handoff: it has no `&TerminalLease` to
    /// pass [`ForegroundGuard::try_acquire`](crate::process::ForegroundGuard::try_acquire).
    pub fn terminal_lease(&self) -> Option<&TerminalLease> {
        match self.turn.terminal_access {
            TerminalAccess::Denied => None,
            TerminalAccess::Leased | TerminalAccess::ExplicitLoan => {
                self.session.terminal_lease.as_ref()
            }
        }
    }

    /// Begin an explicit terminal loan on the installed turn (the `_ed-tui`
    /// case): a foreground handoff may now fire even though stdout is captured,
    /// because the body draws on `/dev/tty` and must own the foreground pgid.
    /// The host must have suspended its own terminal reader/renderer first. The
    /// loan only *raises* an already-`Leased` turn to [`TerminalAccess::ExplicitLoan`];
    /// a `Denied` turn is left untouched, so the loan can only raise an
    /// authorised turn, never mint authority. The returned [`TerminalLoan`]
    /// restores the prior access when surrendered to [`Self::end_terminal_loan`].
    /// Mirrors the within-turn set/clear of the retired `tui_active` flag.
    pub fn begin_terminal_loan(&mut self) -> TerminalLoan {
        let prev = self.turn.terminal_access;
        // The loan may only *raise* an already-authorised turn; it never mints
        // authority. A `Denied` turn is left untouched, closing the
        // `Denied → ExplicitLoan` door the manual token previously left open.
        if matches!(prev, TerminalAccess::Leased) {
            self.turn.terminal_access = TerminalAccess::ExplicitLoan;
        }
        TerminalLoan(prev)
    }

    /// End an explicit terminal loan, restoring the access recorded when it
    /// began. Pair with [`Self::begin_terminal_loan`].
    pub fn end_terminal_loan(&mut self, loan: TerminalLoan) {
        self.turn.terminal_access = loan.0;
    }

    /// Whether the installed turn is currently under an explicit terminal loan.
    /// The re-entrancy guard for `_ed-tui`: a nested loan would be a logic
    /// error, so the editor builtin refuses when this is already true.
    pub fn in_terminal_loan(&self) -> bool {
        matches!(self.turn.terminal_access, TerminalAccess::ExplicitLoan)
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
    /// The prompt cycle uses it: rendering `RAL_PROMPT` runs a value turn
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

    /// Install the invocation positional args (`$args`, `$1`, …) — the
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
    /// balance across a turn boundary with it — e.g. that a panicking tool
    /// call left no leaked frame behind.  The qualitative companion is
    /// [`Shell::has_active_capabilities`](Shell::has_active_capabilities).
    pub fn grant_depth(&self) -> usize {
        self.mobile.context.grants.iter().count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::TerminalLease;

    /// The handoff door: the lease borrow is reachable *only* when the turn's
    /// access permits *and* the session owns a lease. A `Denied` turn cannot
    /// reach the borrow even though the session owns the lease — the exarch
    /// tool-turn case, where the foreground handoff becomes unconstructable.
    #[test]
    #[cfg(unix)]
    fn terminal_lease_gated_by_access_and_session() {
        let mut shell = Shell::default();
        shell.session.terminal_lease = TerminalLease::mint_at_startup(true);
        assert!(
            shell.session.terminal_lease.is_some(),
            "session owns a lease"
        );

        shell.turn.terminal_access = TerminalAccess::Denied;
        assert!(
            shell.terminal_lease().is_none(),
            "a Denied turn cannot borrow the session lease"
        );

        shell.turn.terminal_access = TerminalAccess::Leased;
        assert!(
            shell.terminal_lease().is_some(),
            "a Leased turn borrows the session lease"
        );

        // Even with authority, no borrow when the session minted no lease
        // (a backgrounded / piped / tty-less launch).
        shell.session.terminal_lease = None;
        assert!(
            shell.terminal_lease().is_none(),
            "no session lease → no borrow, regardless of access"
        );
    }

    /// The loan token raises the turn to `ExplicitLoan` and restores the prior
    /// access on surrender — the within-turn `_ed-tui` elevation, mirroring the
    /// retired `tui_active` set/clear.
    #[test]
    fn terminal_loan_round_trips_access() {
        let mut shell = Shell::default();
        shell.turn.terminal_access = TerminalAccess::Leased;
        assert!(!shell.in_terminal_loan());

        let loan = shell.begin_terminal_loan();
        assert!(
            shell.in_terminal_loan(),
            "loan raises the turn to ExplicitLoan"
        );

        shell.end_terminal_loan(loan);
        assert!(!shell.in_terminal_loan());
        assert_eq!(
            shell.turn.terminal_access,
            TerminalAccess::Leased,
            "ending the loan restores the pre-loan access"
        );
    }

    /// The loan only *raises* an authorised turn; it never mints authority.
    /// A `Denied` turn calling `begin_terminal_loan` is left `Denied` — the
    /// `Denied → ExplicitLoan` door is closed — so even with a session lease the
    /// foreground borrow stays unreachable.
    #[test]
    #[cfg(unix)]
    fn denied_turn_loan_does_not_elevate() {
        let mut shell = Shell::default();
        shell.session.terminal_lease = TerminalLease::mint_at_startup(true);
        shell.turn.terminal_access = TerminalAccess::Denied;

        let loan = shell.begin_terminal_loan();
        assert!(
            !shell.in_terminal_loan(),
            "a Denied turn is not raised to ExplicitLoan"
        );
        assert!(
            shell.terminal_lease().is_none(),
            "no foreground borrow: the loan cannot mint authority from Denied"
        );

        shell.end_terminal_loan(loan);
        assert_eq!(shell.turn.terminal_access, TerminalAccess::Denied);
    }
}
