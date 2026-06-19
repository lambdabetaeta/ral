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
use super::repl::ReplScratch;
use super::TerminalAccess;
use crate::diagnostic::SourceDb;
use crate::exit_hints::ExitHints;
use crate::io::{Sink, TerminalState};
use crate::process::{DurableRoot, ForegroundScope, TerminalLease};
use crate::types::AuditFragment;

/// An in-flight terminal loan, returned by [`Shell::begin_terminal_loan`] and
/// surrendered to [`Shell::end_terminal_loan`].
///
/// Opaque to hosts: it carries the prior [`TerminalAccess`] to restore but
/// exposes no way to read or forge one. Obtaining it raises the installed turn
/// to [`TerminalAccess::ExplicitLoan`]; surrendering it restores the prior
/// access. This is the only path to an `ExplicitLoan` — a `TurnRequest` cannot
/// seed it — so the elevation is always a within-turn loan held by the host
/// that suspended its own terminal surface.
pub struct TerminalLoan(TerminalAccess);

impl Shell {
    /// The session's durable source registry.  Hosts read it after a turn
    /// returns to render a runtime error against the right source text.
    pub fn sources(&self) -> &SourceDb {
        &self.session.sources
    }

    /// The session's durable cancel root.  `run_turn` mints a turn's
    /// foreground scope from it (`self.durable_root().child()`); the typed
    /// relation keeps that scope rooted here.  No longer a host accessor —
    /// frame assembly moved behind `run_turn`, so this is `pub(crate)`.
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
    /// returned [`TerminalLoan`] restores the prior access when surrendered to
    /// [`Self::end_terminal_loan`]. Mirrors the within-turn set/clear of the
    /// retired `tui_active` flag.
    pub fn begin_terminal_loan(&mut self) -> TerminalLoan {
        let prev = self.turn.terminal_access;
        self.turn.terminal_access = TerminalAccess::ExplicitLoan;
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
        assert!(shell.session.terminal_lease.is_some(), "session owns a lease");

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
        assert!(shell.in_terminal_loan(), "loan raises the turn to ExplicitLoan");

        shell.end_terminal_loan(loan);
        assert!(!shell.in_terminal_loan());
        assert_eq!(
            shell.turn.terminal_access,
            TerminalAccess::Leased,
            "ending the loan restores the pre-loan access"
        );
    }
}
