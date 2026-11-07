//! Where a child is placed: the process group it lands in, and the terminal
//! lent to it when it takes the foreground.  A standalone external and a
//! pipeline's group are placed by the one rule.

use crate::io::Sink;
use crate::process::{PgidPolicy, PreparedTerminalLoan, TerminalLoan};
use crate::shell::runtime::{Mooring, Shell, TerminalAccess};

/// The group a child lands in, and the terminal it is lent.
///
/// A stage's child joins the stage's group, whose owner holds any loan.
/// Otherwise it takes the foreground, leading a group lent the terminal, iff
/// the run holds the session's terminal lease, its stdout is terminal-bound
/// or the run was lent the terminal explicitly, and it is not `enveloped`.
///
/// The lease, not `interactive`, says who owns the terminal: a script
/// launched at one foregrounds its children as the REPL does.  An explicit
/// loan's body (`_ed-tui`'s `fzf`) draws on `/dev/tty` whatever its stdout;
/// bwrap's `--new-session` puts a payload where `tcsetpgrp` cannot name it.
///
/// A headless child leads a session of its own, so a cancel kills its whole
/// subtree and nothing in it can signal the tty's owner; an interactive one
/// stays in ral's group, where the terminal's Ctrl-C reaches it.
pub(crate) fn placement(
    shell: &Shell,
    mooring: &Mooring,
    enveloped: bool,
) -> (PgidPolicy, Option<PreparedTerminalLoan>) {
    if let Some(stage) = shell.io.stage.as_ref() {
        return (PgidPolicy::Join(stage.group()), None);
    }
    let foreground = !enveloped
        && (matches!(shell.io.stdout, Sink::Terminal)
            || mooring.terminal_access == TerminalAccess::ExplicitLoan);
    match shell.terminal_lease(mooring).filter(|_| foreground) {
        Some(lease) => (
            PgidPolicy::NewLeader,
            TerminalLoan::prepare(lease, &mooring.home),
        ),
        None if shell.io.interactive => (PgidPolicy::Inherit, None),
        None => (PgidPolicy::NewSession, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{DurableRoot, Membership, Pgid, TerminalLease};

    /// A session owning a terminal lease plus a `Leased` mooring: the REPL, or
    /// a terminal-launched script.  Stdout defaults to `Sink::Terminal`.
    fn leased_shell() -> (Shell, Mooring) {
        let mut shell = crate::test_helper::core_shell();
        shell.io.interactive = true;
        shell.io.terminal.startup_stdin_tty = true;
        shell.io.terminal.startup_stdout_tty = true;
        shell.session.terminal_lease = TerminalLease::mint_at_startup(true);
        let mooring = Mooring {
            terminal_access: TerminalAccess::Leased,
            home: DurableRoot::default().worker(),
            cleanup_grace: std::time::Duration::ZERO,
            ..Mooring::adrift()
        };
        (shell, mooring)
    }

    fn captured(shell: &mut Shell) {
        shell.io.stdout = crate::io::new_buffer().0;
    }

    /// Never foreground: no loan, and the group `interactive` leaves it in.
    fn assert_background(placed: (PgidPolicy, Option<PreparedTerminalLoan>), interactive: bool) {
        let (policy, loan) = placed;
        assert!(loan.is_none(), "a background child is lent nothing");
        if interactive {
            assert!(matches!(policy, PgidPolicy::Inherit), "{policy:?}");
        } else {
            assert!(matches!(policy, PgidPolicy::NewSession), "{policy:?}");
        }
    }

    #[cfg(unix)]
    fn assert_foreground(placed: (PgidPolicy, Option<PreparedTerminalLoan>)) {
        let (policy, loan) = placed;
        assert!(matches!(policy, PgidPolicy::NewLeader), "{policy:?}");
        assert!(loan.is_some(), "a foreground child is lent the terminal");
    }

    /// A joined group is never lent: dropping a loan restores the terminal,
    /// out from under the owner.
    #[test]
    fn a_stages_child_joins_its_group_unlent() {
        let (mut shell, mooring) = leased_shell();
        let group = Pgid::from_raw(1).expect("1 is positive");
        shell.io.stage = Some(Membership::new(group, shell.scope.clone()));
        let (policy, loan) = placement(&shell, &mooring, false);
        assert!(
            matches!(policy, PgidPolicy::Join(g) if g == group),
            "{policy:?}"
        );
        assert!(loan.is_none());
    }

    #[test]
    #[cfg(unix)]
    fn a_leased_terminal_bound_child_foregrounds() {
        let (shell, mooring) = leased_shell();
        assert_foreground(placement(&shell, &mooring, false));
    }

    /// A capture's sink is a buffer, not the terminal, so it must not steal the
    /// foreground even under a `Leased` mooring.
    #[test]
    fn a_leased_captured_child_stays_in_the_background() {
        let (mut shell, mooring) = leased_shell();
        captured(&mut shell);
        assert_background(placement(&shell, &mooring, false), true);
    }

    /// The loan foregrounds despite the buffer sink, a lone command as a
    /// pipeline: otherwise `fzf`, drawing on `/dev/tty` from a background
    /// pgroup, raises SIGTTOU (the CTRL-R failure).
    #[test]
    #[cfg(unix)]
    fn an_explicit_loan_foregrounds_a_captured_child() {
        let (mut shell, mut mooring) = leased_shell();
        captured(&mut shell);
        mooring.terminal_access = TerminalAccess::ExplicitLoan;
        assert_foreground(placement(&shell, &mooring, false));
    }

    /// A terminal-launched script holds `Leased` exactly like the REPL: gating on
    /// interactivity would strand `claude` or `fzf` in the background on SIGTTOU.
    #[test]
    #[cfg(unix)]
    fn a_terminal_scripts_leased_child_foregrounds() {
        let (mut shell, mooring) = leased_shell();
        shell.io.interactive = false;
        assert_foreground(placement(&shell, &mooring, false));
    }

    /// A headless background child leads a session of its own.
    #[test]
    fn a_headless_captured_child_leads_a_session() {
        let (mut shell, mooring) = leased_shell();
        shell.io.interactive = false;
        captured(&mut shell);
        assert_background(placement(&shell, &mooring, false), false);
    }

    #[test]
    fn an_enveloped_child_never_foregrounds() {
        let (shell, mut mooring) = leased_shell();
        assert_background(placement(&shell, &mooring, true), true);
        mooring.terminal_access = TerminalAccess::ExplicitLoan;
        assert_background(placement(&shell, &mooring, true), true);
    }

    /// A `Denied` mooring never foregrounds even though the session owns a lease
    /// — exarch's tool runs: the borrow is unreachable, the handoff unbuildable.
    #[test]
    fn a_denied_run_never_foregrounds() {
        let (shell, mut mooring) = leased_shell();
        mooring.terminal_access = TerminalAccess::Denied;
        assert!(
            shell.session.terminal_lease.is_some(),
            "session owns a lease"
        );
        assert_background(placement(&shell, &mooring, false), true);
    }

    /// A launch that never owned the terminal foreground (backgrounded `ral … &`,
    /// a piped or tty-less eval) minted no lease, so there is nothing to borrow.
    #[test]
    fn no_lease_never_foregrounds() {
        let (mut shell, mooring) = leased_shell();
        shell.session.terminal_lease = None;
        assert_background(placement(&shell, &mooring, false), true);
    }
}
